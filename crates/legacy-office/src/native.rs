use crate::NormalizedFormat;
use crate::authority::{RuntimeConfig, verify};
use crate::protocol::{self, ERROR_ENCRYPTED, ERROR_MALFORMED, ERROR_RESOURCE, ERROR_RUNTIME};
use crate::sandbox::{self, Policy};
#[cfg(not(target_os = "macos"))]
use libloading::Library;
#[cfg(not(target_os = "macos"))]
use std::ffi::{CStr, CString, c_char, c_int};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod macos_ipc;
#[cfg(all(test, target_os = "macos"))]
mod macos_tests;
#[cfg(target_os = "macos")]
use macos_ipc::OfficeIpcSocket;
#[cfg(target_os = "macos")]
pub(crate) use macos_ipc::office_ipc_path as macos_office_ipc_path;
#[cfg(target_os = "macos")]
pub(crate) use macos_ipc::remove_office_ipc as macos_remove_office_ipc;

pub(crate) fn run_from_args(arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<(), u8> {
    let policy = Policy::parse(arguments).map_err(|()| 70)?;
    let runtime = PreparedRuntime::new(&policy).map_err(|()| 72)?;
    sandbox::install_or_inherit(&policy).map_err(|()| 70)?;
    let runtime = runtime.load()?;
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
    let format = expected_output(metadata.source).ok_or(ERROR_MALFORMED)?;
    let source_path = request_root
        .path()
        .join(format!("normalized.{}", source_extension(metadata.source).ok_or(ERROR_MALFORMED)?));
    let mut source = create_private(&source_path).map_err(|_| ERROR_RUNTIME)?;
    protocol::copy_request_body(&mut input, &mut source, &metadata)
        .map_err(|()| ERROR_MALFORMED)?;
    protocol::require_eof(&mut input).map_err(|()| ERROR_MALFORMED)?;
    source.sync_all().map_err(|_| ERROR_RUNTIME)?;
    drop(source);
    let output_path = request_root.path().join(format!("normalized.{}", format.extension()));
    let profile = policy.temporary_root.join("profile");
    std::fs::create_dir(&profile).map_err(|_| ERROR_RUNTIME)?;
    runtime.convert(&source_path, &output_path, &profile, format)?;
    let bytes =
        read_bounded_output(&output_path, request_root.path(), metadata.maximum_output_bytes)?;
    let audit_context = into_markdown_core::ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits {
            max_memory_bytes: policy.address_limit,
            max_temporary_bytes: policy.file_limit,
            ..into_markdown_core::ResourceLimits::default()
        },
    );
    let _audit_memory = audit_context
        .reserve_memory(crate::NORMALIZED_PACKAGE_AUDIT_MEMORY_BYTES)
        .map_err(|_| ERROR_RESOURCE)?;
    crate::package::audit(&bytes, format, &audit_context).map_err(|error| package_error(&error))?;
    protocol::write_response(&mut std::io::stdout().lock(), format, &bytes)
        .map_err(|()| ERROR_RUNTIME)
}

fn package_error(error: &into_markdown_core::ConversionError) -> u8 {
    match error {
        into_markdown_core::ConversionError::Encrypted => ERROR_ENCRYPTED,
        into_markdown_core::ConversionError::ResourceLimit { .. } => ERROR_RESOURCE,
        _ => ERROR_MALFORMED,
    }
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

fn source_extension(source: into_markdown_core::InputFormat) -> Option<&'static str> {
    match source {
        into_markdown_core::InputFormat::Doc => Some("doc"),
        into_markdown_core::InputFormat::Ppt => Some("ppt"),
        into_markdown_core::InputFormat::Xls => Some("xls"),
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
    #[cfg(not(windows))]
    let canonical = path.canonicalize().map_err(|_| ERROR_RUNTIME)?;
    #[cfg(windows)]
    let canonical =
        crate::authority::authenticated_windows_path(path, false).map_err(|_| ERROR_RUNTIME)?;
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
    #[cfg(target_os = "macos")]
    authority: crate::authority::VerifiedBundle,
    #[cfg(not(target_os = "macos"))]
    _library: Library,
    #[cfg(windows)]
    _library_directory: WindowsDllDirectory,
    #[cfg(windows)]
    _dependency_libraries: Vec<Library>,
    #[cfg(not(target_os = "macos"))]
    _authority: crate::authority::VerifiedBundle,
    #[cfg(not(target_os = "macos"))]
    hook: unsafe extern "C" fn(*const c_char, *const c_char) -> *mut LibreOfficeKit,
    #[cfg(not(target_os = "macos"))]
    install_root: CString,
    #[cfg(target_os = "macos")]
    soffice: PathBuf,
    #[cfg(target_os = "macos")]
    inherited_process_sandbox: bool,
}

struct PreparedRuntime {
    authority: crate::authority::VerifiedBundle,
    #[cfg(not(target_os = "macos"))]
    install_root: CString,
    #[cfg(target_os = "macos")]
    inherited_process_sandbox: bool,
}

impl PreparedRuntime {
    fn new(policy: &Policy) -> Result<Self, ()> {
        let authority = reverify_authority(policy)?;
        #[cfg(target_os = "macos")]
        crate::authority::validate_mounted(
            &authority,
            &policy.runtime_root,
            &policy.kit_library,
            &policy.install_root,
            &into_markdown_core::ExecutionContext::new(
                into_markdown_core::ExecutionOptions::default(),
                into_markdown_core::ResourceLimits {
                    max_memory_bytes: policy.address_limit,
                    ..into_markdown_core::ResourceLimits::default()
                },
            ),
        )
        .map_err(|_| ())?;
        if authority.root != policy.runtime_root
            || authority.install_root != policy.install_root
            || authority.kit_library != policy.kit_library
        {
            return Err(());
        }
        Ok(Self {
            authority,
            #[cfg(not(target_os = "macos"))]
            install_root: path_c_string(&policy.install_root)?,
            #[cfg(target_os = "macos")]
            inherited_process_sandbox: policy.inherited_process_sandbox,
        })
    }

    fn load(self) -> Result<NativeRuntime, u8> {
        #[cfg(target_os = "macos")]
        {
            let contents = self.authority.install_root.parent().ok_or(72)?;
            let soffice = contents.join("MacOS/soffice");
            let metadata = std::fs::symlink_metadata(&soffice).map_err(|_| 72)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(72);
            }
            Ok(NativeRuntime {
                authority: self.authority,
                soffice,
                inherited_process_sandbox: self.inherited_process_sandbox,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let library_path = self.authority.kit_library.clone();
            // SAFETY: every package-owned dependency was copied from an
            // authority-hashed no-follow handle into this private immutable tree;
            // sandbox installation completed before the loader can run a constructor.
            #[cfg(not(windows))]
            let library = load_library(&library_path).map_err(|()| 72)?;
            #[cfg(windows)]
            let (library, library_directory, dependency_libraries) =
                load_library(&library_path, &self.authority.dependency_files)?;
            // SAFETY: authority ABI validation requires this exact C export.
            let hook = unsafe {
                *library
                    .get::<
                        unsafe extern "C" fn(*const c_char, *const c_char) -> *mut LibreOfficeKit,
                    >(b"libreofficekit_hook_2\0")
                    .map_err(|_| {
                        if cfg!(windows) { 75 } else { 72 }
                    })?
            };
            Ok(NativeRuntime {
                _library: library,
                #[cfg(windows)]
                _library_directory: library_directory,
                #[cfg(windows)]
                _dependency_libraries: dependency_libraries,
                _authority: self.authority,
                hook,
                install_root: self.install_root,
            })
        }
    }
}

impl NativeRuntime {
    fn convert(
        &self,
        source: &Path,
        output: &Path,
        profile: &Path,
        format: NormalizedFormat,
    ) -> Result<(), u8> {
        #[cfg(target_os = "macos")]
        {
            use std::process::{Command, Stdio};

            let work = source.parent().ok_or(ERROR_RUNTIME)?;
            let temporary = profile.parent().ok_or(ERROR_RUNTIME)?;
            let user_installation = format!("-env:UserInstallation={}", file_url(profile));
            let ipc = OfficeIpcSocket::new(profile)?;
            let seatbelt = (!self.inherited_process_sandbox)
                .then(|| {
                    macos_soffice_profile(
                        &self.authority.root,
                        temporary,
                        &self.soffice,
                        ipc.path(),
                    )
                })
                .transpose()?;
            let mut command = if let Some(seatbelt) = &seatbelt {
                let mut command = Command::new("/usr/bin/sandbox-exec");
                command.args(["-p", seatbelt]).arg(&self.soffice);
                command
            } else {
                Command::new(&self.soffice)
            };
            let status = command
                .args([
                    "--headless",
                    "--nologo",
                    "--nodefault",
                    "--nolockcheck",
                    "--norestore",
                    "--nofirststartwizard",
                    &user_installation,
                    "--convert-to",
                    format.extension(),
                    "--outdir",
                ])
                .arg(work)
                .arg(source)
                .env_clear()
                .env("HOME", work)
                .env("TMPDIR", work)
                .env("LANG", "en_US.UTF-8")
                .env("LC_ALL", "C")
                .env("CFFIXED_USER_HOME", work)
                .env("__CF_USER_TEXT_ENCODING", "0x1F5:0x0:0x0")
                .current_dir(work)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|_| crate::protocol::ERROR_SANDBOX)?;
            if output.is_file() {
                Ok(())
            } else if !status.success() {
                if status.code().is_some_and(|code| (64..=78).contains(&code)) {
                    return Err(crate::protocol::ERROR_SANDBOX);
                }
                Err(ERROR_MALFORMED)
            } else {
                Err(ERROR_RUNTIME)
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
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
}

#[cfg(target_os = "macos")]
fn macos_soffice_profile(
    runtime: &Path,
    temporary: &Path,
    soffice: &Path,
    ipc_socket: &Path,
) -> Result<String, u8> {
    use std::fmt::Write as _;
    let runtime = seatbelt_path(runtime)?;
    let temporary = seatbelt_path(temporary)?;
    let soffice = seatbelt_path(soffice)?;
    let ipc_network = seatbelt_path(&macos_ipc::office_ipc_network_path(ipc_socket)?)?;
    let ipc_socket = seatbelt_path(ipc_socket)?;
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(import \"system.sb\")\n\
         (deny network*)\n\
         (allow process-fork)\n(allow process-info*)\n(allow sysctl-read)\n\
         (allow user-preference-read)\n(allow signal (target self))\n\
         (allow mach-lookup\n\
           (global-name \"com.apple.FontObjectsServer\")\n\
           (global-name \"com.apple.fonts\")\n\
           (global-name \"com.apple.system.opendirectoryd.libinfo\")\n\
           (global-name \"com.apple.SystemConfiguration.configd\")\n\
           (global-name \"com.apple.CoreServices.coreservicesd\")\n\
           (global-name \"com.apple.DiskArbitration.diskarbitrationd\")\n\
           (global-name \"com.apple.pasteboard.1\")\n\
           (global-name \"com.apple.distributed_notifications@Uv3\")\n\
           (global-name \"com.apple.tccd.system\")\n\
           (global-name \"com.apple.windowserver.active\")\n\
           (global-name \"com.apple.coreservices.launchservicesd\")\n\
           (global-name \"com.apple.lsd.mapdb\")\n\
           (global-name \"com.apple.lsd.modifydb\")\n\
           (global-name \"com.apple.dock.server\")\n\
           (global-name \"com.apple.iohideventsystem\")\n\
           (global-name \"com.apple.windowmanager.server\")\n\
           (global-name \"com.apple.CARenderServer\")\n\
           (global-name \"com.apple.pbs.fetch_services\")\n\
           (global-name \"com.apple.appkit.restoration_storage\")\n\
           (global-name \"com.apple.coreservices.appleevents\")\n\
           (global-name \"com.apple.touchbarserver.mig\")\n\
           (global-name \"com.apple.window_proxies\"))\n\
         (allow iokit-open-user-client\n\
           (iokit-user-client-class \"IOHIDParamUserClient\")\n\
           (iokit-user-client-class \"IOSurfaceRootUserClient\"))\n",
    );
    writeln!(
        profile,
        "(allow network*\n  (local unix-socket (path-literal \"{ipc_network}\"))\n  (remote unix-socket (path-literal \"{ipc_socket}\")))"
    )
    .map_err(|_| ERROR_RUNTIME)?;
    writeln!(profile, "(allow file-read* file-write* (literal \"{ipc_socket}\"))")
        .map_err(|_| ERROR_RUNTIME)?;
    profile.push_str("(allow file-read-metadata file-write-data (literal \"/private/tmp\"))\n");
    writeln!(profile, "(allow file-read* (subpath \"{runtime}\") (subpath \"{temporary}\"))")
        .map_err(|_| ERROR_RUNTIME)?;
    let mut ancestors = std::collections::BTreeSet::new();
    for root in [runtime.as_str(), temporary.as_str()] {
        let mut ancestor = Path::new(root).parent();
        while let Some(path) = ancestor {
            if path != Path::new("/") {
                ancestors.insert(seatbelt_path(path)?);
            }
            ancestor = path.parent();
        }
    }
    for ancestor in ancestors {
        writeln!(profile, "(allow file-read* (literal \"{ancestor}\"))")
            .map_err(|_| ERROR_RUNTIME)?;
    }
    writeln!(profile, "(allow file-read* (literal \"/private/var/db/.AppleSetupDone\"))")
        .map_err(|_| ERROR_RUNTIME)?;
    writeln!(profile, "(allow file-write* (subpath \"{temporary}\"))")
        .map_err(|_| ERROR_RUNTIME)?;
    writeln!(profile, "(allow file-issue-extension (subpath \"{runtime}\"))")
        .map_err(|_| ERROR_RUNTIME)?;
    writeln!(profile, "(allow process-exec (literal \"{soffice}\"))").map_err(|_| ERROR_RUNTIME)?;
    Ok(profile)
}

#[cfg(target_os = "macos")]
fn seatbelt_path(path: &Path) -> Result<String, u8> {
    let value = path.to_str().ok_or(ERROR_RUNTIME)?;
    if value.bytes().any(|byte| byte == 0 || byte.is_ascii_control()) {
        return Err(ERROR_RUNTIME);
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn reverify_authority(policy: &Policy) -> Result<crate::authority::VerifiedBundle, ()> {
    use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};

    if running_executable_sha256()? != policy.worker_sha256 {
        return Err(());
    }
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
            policy.worker_original.clone(),
        ),
        &context,
    )
    .map_err(|_| ())?;
    if bundle.root != policy.runtime_root
        || bundle.install_root != policy.install_root
        || bundle.worker != policy.worker_original
        || bundle.worker_sha256 != policy.worker_sha256
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

fn running_executable_sha256() -> Result<String, ()> {
    #[cfg(target_os = "linux")]
    let path = Path::new("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let executable = std::env::current_exe().map_err(|_| ())?;
    #[cfg(not(target_os = "linux"))]
    let path = executable.as_path();
    file_sha256(path)
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn load_library(path: &Path) -> Result<Library, ()> {
    // SAFETY: the caller validated and hashed this absolute package-owned
    // library immediately before invoking the platform loader.
    unsafe { Library::new(path) }.map_err(|_| ())
}

#[cfg(windows)]
fn load_library(
    path: &Path,
    dependencies: &[crate::authority::VerifiedRuntimeFile],
) -> Result<(Library, WindowsDllDirectory, Vec<Library>), u8> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::System::LibraryLoader::AddDllDirectory;

    let runtime = path.parent().and_then(Path::parent).ok_or(74)?;
    let system64 = runtime.join("System64");
    crate::authority::authenticated_windows_path(&system64, true).map_err(|_| 73)?;
    let mut wide = system64.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    // SAFETY: the terminated path names the authority-validated package
    // directory and remains live for this synchronous call.
    let cookie = unsafe { AddDllDirectory(wide.as_ptr()) };
    if cookie.is_null() {
        return Err(73);
    }
    let directory = WindowsDllDirectory { cookie: cookie as usize };
    // DLL_LOAD_DIR confines package dependencies to the authority-validated
    // kit directory; USER_DIRS contains only the retained System64 cookie;
    // SYSTEM32 permits only operating-system dependencies. PATH, the current
    // directory, application directory, and default directories remain absent.
    let mut ordered = dependencies
        .iter()
        .filter(|dependency| {
            dependency.path != path
                && dependency
                    .path
                    .extension()
                    .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case("dll"))
        })
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        let left_local = left.path.parent() == Some(system64.as_path());
        let right_local = right.path.parent() == Some(system64.as_path());
        right_local.cmp(&left_local).then_with(|| left.relative.cmp(&right.relative))
    });
    let mut dependency_libraries = Vec::new();
    dependency_libraries.try_reserve(ordered.len()).map_err(|_| 74)?;
    for dependency in ordered {
        crate::authority::authenticated_windows_path(&dependency.path, false).map_err(|_| 76)?;
        dependency_libraries.push(load_windows_library_exact(&dependency.path)?);
    }
    let library = load_windows_library_exact(path)?;
    Ok((library, directory, dependency_libraries))
}

#[cfg(windows)]
fn load_windows_library_exact(path: &Path) -> Result<Library, u8> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryExW;

    let mut library_wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    library_wide.push(0);
    // SAFETY: the absolute, terminated library path and its complete dependency
    // closure were authenticated immediately above. The returned module handle
    // is transferred exactly once to libloading for RAII ownership.
    let handle = unsafe {
        LoadLibraryExW(library_wide.as_ptr(), std::ptr::null_mut(), WINDOWS_DLL_SEARCH_FLAGS)
    };
    if handle.is_null() {
        // SAFETY: GetLastError is read immediately after the failed loader call.
        let raw = unsafe { GetLastError() }.cast_signed();
        eprintln!("into-md-worker:loader-win32={raw}");
        return Err(windows_loader_exit_code(Some(raw)));
    }
    // SAFETY: handle is a unique successful LoadLibraryExW result and ownership
    // is transferred to this Library, which will call FreeLibrary on drop.
    let library = unsafe { libloading::os::windows::Library::from_raw(handle as isize) };
    Ok(library.into())
}

#[cfg(windows)]
fn windows_loader_exit_code(raw: Option<i32>) -> u8 {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_BAD_EXE_FORMAT, ERROR_DLL_INIT_FAILED, ERROR_INVALID_PARAMETER,
        ERROR_MOD_NOT_FOUND,
    };

    match raw.and_then(|value| u32::try_from(value).ok()) {
        Some(ERROR_ACCESS_DENIED) => 76,
        Some(ERROR_MOD_NOT_FOUND) => 77,
        Some(ERROR_BAD_EXE_FORMAT) => 78,
        Some(ERROR_DLL_INIT_FAILED) => 79,
        Some(ERROR_INVALID_PARAMETER) => 80,
        Some(_) | None => 74,
    }
}

#[cfg(windows)]
const WINDOWS_DLL_SEARCH_FLAGS: u32 = libloading::os::windows::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
    | libloading::os::windows::LOAD_LIBRARY_SEARCH_SYSTEM32
    | libloading::os::windows::LOAD_LIBRARY_SEARCH_USER_DIRS;

#[cfg(windows)]
struct WindowsDllDirectory {
    cookie: usize,
}

#[cfg(windows)]
impl Drop for WindowsDllDirectory {
    fn drop(&mut self) {
        use windows_sys::Win32::System::LibraryLoader::RemoveDllDirectory;
        // SAFETY: AddDllDirectory returned this cookie and it is removed once,
        // after the Library field has been dropped.
        unsafe {
            RemoveDllDirectory(self.cookie as *mut std::ffi::c_void);
        }
    }
}

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
#[cfg(not(target_os = "macos"))]
struct LibreOfficeKit {
    class: *mut LibreOfficeKitClass,
}

#[repr(C)]
#[cfg(not(target_os = "macos"))]
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
#[cfg(not(target_os = "macos"))]
struct LibreOfficeDocument {
    class: *mut LibreOfficeDocumentClass,
}

#[repr(C)]
#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
struct Office(*mut LibreOfficeKit);

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
struct Document(*mut LibreOfficeDocument);

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
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
    fn dll_search_is_limited_to_validated_package_and_system_directories() {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            LOAD_LIBRARY_SEARCH_USER_DIRS,
        };
        assert_eq!(
            WINDOWS_DLL_SEARCH_FLAGS,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
                | LOAD_LIBRARY_SEARCH_SYSTEM32
                | LOAD_LIBRARY_SEARCH_USER_DIRS
        );
        assert_eq!(WINDOWS_DLL_SEARCH_FLAGS & LOAD_LIBRARY_SEARCH_APPLICATION_DIR, 0);
        assert_eq!(WINDOWS_DLL_SEARCH_FLAGS & LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, 0);
        assert_ne!(WINDOWS_DLL_SEARCH_FLAGS & LOAD_LIBRARY_SEARCH_USER_DIRS, 0);
    }
}
