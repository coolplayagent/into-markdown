use crate::NormalizedFormat;
use crate::authority::{RuntimeConfig, verify};
use crate::protocol::{self, ERROR_ENCRYPTED, ERROR_MALFORMED, ERROR_RESOURCE, ERROR_RUNTIME};
use crate::sandbox::{self, Policy};
use libloading::Library;
use std::ffi::{CStr, CString, c_char, c_int};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;

pub(crate) fn run_from_args(arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<(), u8> {
    let policy = Policy::parse(arguments).map_err(|()| 70)?;
    let runtime = NativeRuntime::load(&policy).map_err(|()| 72)?;
    sandbox::install(&policy).map_err(|()| 70)?;
    match run_request(&policy, &runtime) {
        Ok(()) => Ok(()),
        Err(code) => {
            protocol::write_error(&mut std::io::stdout().lock(), code).map_err(|()| 71)?;
            Ok(())
        }
    }
}

fn run_request(policy: &Policy, runtime: &NativeRuntime) -> Result<(), u8> {
    let mut input = std::io::stdin().lock();
    let metadata = protocol::read_request_meta(&mut input).map_err(|()| ERROR_MALFORMED)?;
    if metadata.input_bytes == 0
        || metadata.input_bytes > policy.file_limit
        || metadata.maximum_output_bytes == 0
        || metadata.maximum_output_bytes > policy.file_limit
        || metadata.maximum_output_bytes > crate::MAX_NORMALIZED_PACKAGE_BYTES
    {
        return Err(ERROR_RESOURCE);
    }
    let request_root = tempfile::Builder::new()
        .prefix("request-")
        .tempdir_in(&policy.temporary_root)
        .map_err(|_| ERROR_RUNTIME)?;
    let source_path = request_root.path().join("source.bin");
    let mut source = create_private(&source_path).map_err(|_| ERROR_RUNTIME)?;
    protocol::copy_request_body(&mut input, &mut source, &metadata)
        .map_err(|()| ERROR_MALFORMED)?;
    protocol::require_eof(&mut input).map_err(|()| ERROR_MALFORMED)?;
    source.sync_all().map_err(|_| ERROR_RUNTIME)?;
    drop(source);
    let format = expected_output(metadata.source).ok_or(ERROR_MALFORMED)?;
    let output_path = request_root.path().join(format!("normalized.{}", format.extension()));
    let profile = request_root.path().join("profile");
    std::fs::create_dir(&profile).map_err(|_| ERROR_RUNTIME)?;
    runtime.convert(&source_path, &output_path, &profile, format)?;
    let bytes =
        read_bounded_output(&output_path, request_root.path(), metadata.maximum_output_bytes)?;
    protocol::write_response(&mut std::io::stdout().lock(), format, &bytes)
        .map_err(|()| ERROR_RUNTIME)
}

fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn expected_output(source: into_markdown_core::InputFormat) -> Option<NormalizedFormat> {
    match source {
        into_markdown_core::InputFormat::Doc => Some(NormalizedFormat::Docx),
        into_markdown_core::InputFormat::Ppt => Some(NormalizedFormat::Pptx),
        into_markdown_core::InputFormat::Xls => Some(NormalizedFormat::Xlsx),
        _ => None,
    }
}

fn read_bounded_output(path: &Path, root: &Path, maximum: u64) -> Result<Vec<u8>, u8> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ERROR_RUNTIME)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > maximum
    {
        return Err(ERROR_RESOURCE);
    }
    let canonical = path.canonicalize().map_err(|_| ERROR_RUNTIME)?;
    if !canonical.starts_with(root) {
        return Err(ERROR_RUNTIME);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|_| ERROR_RUNTIME)?;
    let opened = file.metadata().map_err(|_| ERROR_RUNTIME)?;
    if opened.len() != metadata.len() || !same_file(&metadata, &opened) {
        return Err(ERROR_RUNTIME);
    }
    let length = usize::try_from(metadata.len()).map_err(|_| ERROR_RESOURCE)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| ERROR_RESOURCE)?;
    let mut limited = (&mut file).take(maximum.saturating_add(1));
    limited.read_to_end(&mut bytes).map_err(|_| ERROR_RUNTIME)?;
    if bytes.len() != length || u64::try_from(bytes.len()).map_err(|_| ERROR_RESOURCE)? > maximum {
        return Err(ERROR_RESOURCE);
    }
    let after = file.metadata().map_err(|_| ERROR_RUNTIME)?;
    if !same_file(&opened, &after) {
        return Err(ERROR_RUNTIME);
    }
    Ok(bytes)
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino() && left.len() == right.len()
}

#[cfg(not(unix))]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

struct NativeRuntime {
    _library: Library,
    hook: unsafe extern "C" fn(*const c_char, *const c_char) -> *mut LibreOfficeKit,
    install_root: CString,
}

impl NativeRuntime {
    fn load(policy: &Policy) -> Result<Self, ()> {
        let authority = reverify_authority(policy)?;
        if file_sha256(&policy.kit_library)? != policy.kit_sha256 {
            return Err(());
        }
        // SAFETY: the absolute, canonical, authority-hashed library is loaded
        // before the sandbox closes the dynamic loader's dependency paths.
        let library = load_library(&authority.kit_library)?;
        // SAFETY: authority ABI validation requires this exact C export.
        let hook = unsafe {
            *library
                .get::<unsafe extern "C" fn(*const c_char, *const c_char) -> *mut LibreOfficeKit>(
                    b"libreofficekit_hook_2\0",
                )
                .map_err(|_| ())?
        };
        Ok(Self { _library: library, hook, install_root: path_c_string(&policy.install_root)? })
    }

    fn convert(
        &self,
        source: &Path,
        output: &Path,
        profile: &Path,
        format: NormalizedFormat,
    ) -> Result<(), u8> {
        let profile_url = CString::new(file_url(profile)).map_err(|_| ERROR_RUNTIME)?;
        // SAFETY: hook and both strings follow the audited LOK C ABI. The
        // returned object is immediately wrapped and destroyed on every path.
        let office = unsafe { (self.hook)(self.install_root.as_ptr(), profile_url.as_ptr()) };
        let office = Office::new(office).ok_or(ERROR_RUNTIME)?;
        let source_url = CString::new(file_url(source)).map_err(|_| ERROR_RUNTIME)?;
        let load_options = c"Language=en-US,MacroSecurityLevel=3,ReadOnly=true";
        let document = office.load(&source_url, load_options)?;
        let output_url = CString::new(file_url(output)).map_err(|_| ERROR_RUNTIME)?;
        let format = CString::new(format.extension()).map_err(|_| ERROR_RUNTIME)?;
        document.save(&output_url, &format)
    }
}

fn reverify_authority(policy: &Policy) -> Result<crate::authority::VerifiedBundle, ()> {
    use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};

    let worker = std::env::current_exe().map_err(|_| ())?.canonicalize().map_err(|_| ())?;
    let context = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits {
            max_memory_bytes: policy.address_limit,
            max_temporary_bytes: policy.file_limit,
            ..ResourceLimits::default()
        },
    );
    let bundle = verify(
        &RuntimeConfig::new(
            policy.runtime_root.join("authority.json"),
            policy.runtime_root.clone(),
            worker,
        ),
        &context,
    )
    .map_err(|_| ())?;
    if bundle.root != policy.runtime_root
        || bundle.install_root != policy.install_root
        || bundle.kit_library != policy.kit_library
        || bundle.kit_sha256 != policy.kit_sha256
        || bundle.authority_sha256 != policy.authority_sha256
        || bundle.file_size_limit != policy.file_limit
        || bundle.open_file_limit != policy.open_file_limit
        || bundle.system_read_paths != policy.system_read_paths
    {
        return Err(());
    }
    #[cfg(windows)]
    if bundle.app_container.sid != policy.app_container_sid {
        return Err(());
    }
    Ok(bundle)
}

#[cfg(not(windows))]
fn load_library(path: &Path) -> Result<Library, ()> {
    // SAFETY: the caller validated and hashed this absolute package-owned
    // library immediately before invoking the platform loader.
    unsafe { Library::new(path) }.map_err(|_| ())
}

#[cfg(windows)]
fn load_library(path: &Path) -> Result<Library, ()> {
    // DLL_LOAD_DIR confines package dependencies to the authority-validated
    // kit directory; SYSTEM32 permits only operating-system dependencies. PATH,
    // the current directory, application directory, and user DLL directories
    // are deliberately absent.
    let library = unsafe {
        libloading::os::windows::Library::load_with_flags(path, WINDOWS_DLL_SEARCH_FLAGS)
    }
    .map_err(|_| ())?;
    Ok(library.into())
}

#[cfg(windows)]
const WINDOWS_DLL_SEARCH_FLAGS: u32 = libloading::os::windows::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
    | libloading::os::windows::LOAD_LIBRARY_SEARCH_SYSTEM32;

fn file_sha256(path: &Path) -> Result<String, ()> {
    use sha2::Digest as _;
    let mut file = File::open(path).map_err(|_| ())?;
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|_| ())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[repr(C)]
struct LibreOfficeKit {
    class: *mut LibreOfficeKitClass,
}

#[repr(C)]
struct LibreOfficeKitClass {
    size: usize,
    destroy: Option<unsafe extern "C" fn(*mut LibreOfficeKit)>,
    document_load: Option<
        unsafe extern "C" fn(*mut LibreOfficeKit, *const c_char) -> *mut LibreOfficeDocument,
    >,
    get_error: Option<unsafe extern "C" fn(*mut LibreOfficeKit) -> *mut c_char>,
    document_load_with_options: Option<
        unsafe extern "C" fn(
            *mut LibreOfficeKit,
            *const c_char,
            *const c_char,
        ) -> *mut LibreOfficeDocument,
    >,
    free_error: Option<unsafe extern "C" fn(*mut c_char)>,
}

#[repr(C)]
struct LibreOfficeDocument {
    class: *mut LibreOfficeDocumentClass,
}

#[repr(C)]
struct LibreOfficeDocumentClass {
    size: usize,
    destroy: Option<unsafe extern "C" fn(*mut LibreOfficeDocument)>,
    save_as: Option<
        unsafe extern "C" fn(
            *mut LibreOfficeDocument,
            *const c_char,
            *const c_char,
            *const c_char,
        ) -> c_int,
    >,
}

struct Office(*mut LibreOfficeKit);

impl Office {
    fn new(pointer: *mut LibreOfficeKit) -> Option<Self> {
        if pointer.is_null() { None } else { Some(Self(pointer)) }
    }

    fn load(&self, url: &CStr, options: &CStr) -> Result<Document, u8> {
        // SAFETY: self owns a non-null object returned by LOK for the lifetime
        // of this call. Class size and function pointers are checked first.
        let class = unsafe { self.0.as_ref() }
            .and_then(|office| unsafe { office.class.as_ref() })
            .ok_or(ERROR_RUNTIME)?;
        if class.size < std::mem::size_of::<LibreOfficeKitClass>() {
            return Err(ERROR_RUNTIME);
        }
        let load = class.document_load_with_options.ok_or(ERROR_RUNTIME)?;
        // SAFETY: arguments are live C strings and `load` belongs to this object.
        let document = unsafe { load(self.0, url.as_ptr(), options.as_ptr()) };
        if document.is_null() {
            return Err(self.classify_error(class));
        }
        Ok(Document(document))
    }

    fn classify_error(&self, class: &LibreOfficeKitClass) -> u8 {
        let Some(get_error) = class.get_error else { return ERROR_MALFORMED };
        // SAFETY: callback belongs to self and returns either null or a C string
        // whose ownership is described by the adjacent free_error callback.
        let error = unsafe { get_error(self.0) };
        if error.is_null() {
            return ERROR_MALFORMED;
        }
        // SAFETY: non-null LOK error is a terminated C string.
        let encrypted = unsafe { CStr::from_ptr(error) }
            .to_bytes()
            .windows(8)
            .any(|window| window.eq_ignore_ascii_case(b"password"))
            || unsafe { CStr::from_ptr(error) }
                .to_bytes()
                .windows(9)
                .any(|window| window.eq_ignore_ascii_case(b"encrypted"));
        if let Some(free) = class.free_error {
            // SAFETY: the pointer came from this LOK instance's get_error.
            unsafe { free(error) };
        }
        if encrypted { ERROR_ENCRYPTED } else { ERROR_MALFORMED }
    }
}

impl Drop for Office {
    fn drop(&mut self) {
        // SAFETY: the class pointer is part of the live LOK object and destroy
        // consumes that object exactly once.
        unsafe {
            if let Some(class) = self.0.as_ref().and_then(|office| office.class.as_ref())
                && let Some(destroy) = class.destroy
            {
                destroy(self.0);
            }
        }
    }
}

struct Document(*mut LibreOfficeDocument);

impl Document {
    fn save(&self, url: &CStr, format: &CStr) -> Result<(), u8> {
        // SAFETY: document/class pointers originate from the live Office.
        let class = unsafe { self.0.as_ref() }
            .and_then(|document| unsafe { document.class.as_ref() })
            .ok_or(ERROR_RUNTIME)?;
        if class.size < std::mem::size_of::<LibreOfficeDocumentClass>() {
            return Err(ERROR_RUNTIME);
        }
        let save = class.save_as.ok_or(ERROR_RUNTIME)?;
        // SAFETY: strings and document remain live during the exact LOK call.
        let result = unsafe { save(self.0, url.as_ptr(), format.as_ptr(), c"".as_ptr()) };
        if result == 0 { Err(ERROR_RUNTIME) } else { Ok(()) }
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        // SAFETY: destroy is invoked at most once for this live document.
        unsafe {
            if let Some(class) = self.0.as_ref().and_then(|document| document.class.as_ref())
                && let Some(destroy) = class.destroy
            {
                destroy(self.0);
            }
        }
    }
}

fn path_c_string(path: &Path) -> Result<CString, ()> {
    CString::new(path.to_str().ok_or(())?).map_err(|_| ())
}

fn file_url(path: &Path) -> String {
    let mut output = String::from("file://");
    let value = path.to_string_lossy().replace('\\', "/");
    if !value.starts_with('/') {
        output.push('/');
    }
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'.' | b'_' | b'-') {
            output.push(char::from(byte));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            output.push('%');
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_encodes_delimiters_and_unicode() {
        let url = file_url(Path::new("/private/a b/#中.doc"));
        assert_eq!(url, "file:///private/a%20b/%23%E4%B8%AD.doc");
    }

    #[cfg(windows)]
    #[test]
    fn dll_search_excludes_path_cwd_application_and_user_directories() {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            LOAD_LIBRARY_SEARCH_USER_DIRS,
        };
        assert_eq!(
            WINDOWS_DLL_SEARCH_FLAGS,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32
        );
        assert_eq!(WINDOWS_DLL_SEARCH_FLAGS & LOAD_LIBRARY_SEARCH_APPLICATION_DIR, 0);
        assert_eq!(WINDOWS_DLL_SEARCH_FLAGS & LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, 0);
        assert_eq!(WINDOWS_DLL_SEARCH_FLAGS & LOAD_LIBRARY_SEARCH_USER_DIRS, 0);
    }
}
