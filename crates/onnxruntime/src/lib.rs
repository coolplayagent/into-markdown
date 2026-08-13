//! Audited native ONNX Runtime loading boundary.
//!
//! The crate is intentionally separate because dynamic symbol lookup and the C
//! ABI require `unsafe`. All consumers see only the safe [`RuntimeLibrary`].
#![deny(unsafe_op_in_unsafe_fn)]

mod native;
mod protocol;
mod worker;

use into_markdown_core::{ConversionError, ExecutionContext, Tensor};
use into_markdown_ocr::{
    ModelMetadata, ResolvedModel, SessionAdapter, SessionFactory, SessionOptions,
};
use libloading::Library;
use object::{Object, read::elf::FileHeader as ElfFileHeader, read::macho::MachHeader};
use ort::sys::OrtApiBase;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use thiserror::Error;

const MAX_VERSION_BYTES: usize = 64;
const MAX_BINARY_DEPENDENCIES: usize = 128;
const MAX_BINARY_PATH_BYTES: usize = 1024;
const MAX_RUNTIME_LIBRARY_BYTES: u64 = 512 * 1024 * 1024;

/// Stable failures produced before any session is created.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LoadError {
    /// The build target is outside the four audited CPU distributions.
    #[error("unsupported ONNX Runtime target")]
    UnsupportedTarget,
    /// The configured library path or one of its components is unsafe.
    #[error("unsafe ONNX Runtime library path")]
    UnsafePath,
    /// The library bytes differ from the embedded authority.
    #[error("ONNX Runtime library hash mismatch")]
    HashMismatch,
    /// The library cannot be opened or copied.
    #[error("ONNX Runtime library I/O failed")]
    Io,
    /// The dynamic library is missing the required C entry point.
    #[error("ONNX Runtime ABI entry point is unavailable")]
    MissingEntryPoint,
    /// The official library did not expose the authoritative API level.
    #[error("ONNX Runtime API version mismatch")]
    ApiMismatch,
    /// The runtime version string is absent, unterminated, non-UTF-8, or unexpected.
    #[error("ONNX Runtime version mismatch")]
    VersionMismatch,
    /// The audited dependency closure or current process loader state is unsafe.
    #[error("ONNX Runtime dependency audit mismatch")]
    DependencyMismatch,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    version: String,
    api_version: u32,
    source: String,
    license: String,
    targets: std::collections::BTreeMap<String, Target>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    asset: String,
    sha256: String,
    library: String,
    library_bytes: u64,
    worker_address_space_overhead_bytes: u64,
    binary_format: String,
    binary_architecture: String,
    load_identity: String,
    library_sha256: String,
    rpaths: Vec<String>,
    system_dependencies: Vec<SystemDependency>,
    companion_dependencies: Vec<CompanionDependency>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemDependency {
    load_name: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompanionDependency {
    load_name: String,
    path: String,
    sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
struct BinaryMetadata {
    format: &'static str,
    architecture: &'static str,
    load_identity: String,
    dependencies: Vec<String>,
    rpaths: Vec<String>,
}

/// Verified exact CPU runtime bytes retained without loading native code in the
/// parent process.
pub struct RuntimeLibrary {
    version: String,
    api_version: u32,
    private_path: PathBuf,
    _source: File,
    _private_dir: TempDir,
}

impl std::fmt::Debug for RuntimeLibrary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeLibrary")
            .field("version", &self.version)
            .field("api_version", &self.api_version)
            .field("private_path", &self.private_path)
            .finish_non_exhaustive()
    }
}

impl RuntimeLibrary {
    /// Verify one explicit library below `trusted_root` and retain a private
    /// create-new copy for an isolated worker.
    ///
    /// No current-directory, `PATH`, loader variable, or process environment
    /// lookup is performed.
    ///
    /// # Errors
    ///
    /// Returns a stable [`LoadError`] if the target, path, hash, ABI, API level,
    /// version string, or local I/O does not satisfy the embedded authority.
    pub fn load(trusted_root: &Path, library_path: &Path) -> Result<Self, LoadError> {
        let target_name = current_target().ok_or(LoadError::UnsupportedTarget)?;
        let authority = authority()?;
        let target = authority.targets.get(target_name).ok_or(LoadError::UnsupportedTarget)?;
        validate_authority(&authority, target_name, target)?;
        let (mut source, canonical) = open_explicit_no_follow(trusted_root, library_path)?;
        if source.metadata().map_err(|_| LoadError::Io)?.len() != target.library_bytes {
            return Err(LoadError::HashMismatch);
        }
        let private_dir =
            tempfile::Builder::new().prefix("into-md-ort-").tempdir().map_err(|_| LoadError::Io)?;
        let file_name = Path::new(&target.library).file_name().ok_or(LoadError::UnsafePath)?;
        let private_path = private_dir.path().join(file_name);
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&private_path)
            .map_err(|_| LoadError::Io)?;
        let actual_hash = copy_and_hash(&mut source, &mut destination, target.library_bytes)?;
        if actual_hash != target.library_sha256 {
            return Err(LoadError::HashMismatch);
        }
        destination.sync_all().map_err(|_| LoadError::Io)?;
        drop(destination);
        ensure_same_open_file(&source, &canonical)?;
        let private_path = private_path.canonicalize().map_err(|_| LoadError::UnsafePath)?;
        let binary = read_binary_metadata(&private_path, target.library_bytes)?;
        validate_binary_metadata(target, &binary)?;

        Ok(Self {
            version: authority.version,
            api_version: authority.api_version,
            private_path,
            _source: source,
            _private_dir: private_dir,
        })
    }

    /// Exact `GetVersionString` value verified against the authority.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Exact API level accepted by `OrtApiBase::GetApi`.
    #[must_use]
    pub const fn api_version(&self) -> u32 {
        self.api_version
    }

    /// Private verified path retained by the explicit loader.
    #[must_use]
    pub fn private_path(&self) -> &Path {
        &self.private_path
    }
}

/// Real CPU session factory backed by the verified runtime library.
#[derive(Debug)]
pub struct OrtSessionFactory {
    library: Arc<RuntimeLibrary>,
    worker_executable: PathBuf,
}

impl OrtSessionFactory {
    /// Bind an explicit worker executable to the verified runtime.
    ///
    /// # Errors
    ///
    /// Returns a stable path error before any process is started.
    pub fn new(
        library: Arc<RuntimeLibrary>,
        worker_executable: impl Into<PathBuf>,
    ) -> Result<Self, LoadError> {
        let worker_executable = worker_executable.into();
        if !worker_executable.is_absolute() {
            return Err(LoadError::UnsafePath);
        }
        Ok(Self { library, worker_executable })
    }
}

impl SessionFactory for OrtSessionFactory {
    fn estimate_bytes(
        &self,
        model: &ResolvedModel,
        options: &SessionOptions,
    ) -> Result<u64, ConversionError> {
        let estimate = model.contract.session_memory_bytes;
        if estimate == 0 || estimate > options.max_session_bytes {
            return Err(ort_error("sessionMemory"));
        }
        Ok(estimate)
    }

    fn create(
        &self,
        model: &ResolvedModel,
        options: &SessionOptions,
        context: &ExecutionContext,
    ) -> Result<Arc<dyn SessionAdapter>, ConversionError> {
        context.checkpoint()?;
        if self.library.version().is_empty() || model.bytes.is_empty() {
            return Err(ort_error("ModelUnavailable"));
        }
        let (worker, metadata) = worker::WorkerClient::start(
            &self.library,
            &self.worker_executable,
            &model.bytes,
            &model.contract,
            options,
            context,
        )?;
        context.checkpoint()?;
        let estimate = self.estimate_bytes(model, options)?;
        Ok(Arc::new(OrtSession { worker: Mutex::new(worker), metadata, estimated_bytes: estimate }))
    }
}

struct OrtSession {
    worker: Mutex<worker::WorkerClient>,
    metadata: ModelMetadata,
    estimated_bytes: u64,
}

impl SessionAdapter for OrtSession {
    fn metadata(&self) -> Result<ModelMetadata, ConversionError> {
        Ok(self.metadata.clone())
    }

    fn estimated_bytes(&self) -> u64 {
        self.estimated_bytes
    }

    fn run(
        &self,
        inputs: &[Tensor],
        context: &ExecutionContext,
    ) -> Result<Vec<Tensor>, ConversionError> {
        lock(&self.worker).run(inputs, &self.metadata.outputs, context)
    }
}

fn ort_error(detail: &'static str) -> ConversionError {
    ConversionError::Ocr { provider: "onnxruntime-cpu".into(), detail: detail.into() }
}

/// Run the isolated ONNX Runtime worker protocol on standard input/output.
///
/// This entry point is intended only for the repository's audited worker
/// binary; callers create sessions through [`OrtSessionFactory`].
#[doc(hidden)]
#[must_use]
pub fn worker_main() -> std::process::ExitCode {
    worker::worker_entry()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn authority() -> Result<Authority, LoadError> {
    serde_json::from_str(include_str!("../../../third_party/onnxruntime/manifest.json"))
        .map_err(|_| LoadError::VersionMismatch)
}

fn validate_authority(
    authority: &Authority,
    target_name: &str,
    target: &Target,
) -> Result<(), LoadError> {
    let expected_targets = [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
    ];
    if authority.version.is_empty()
        || authority.api_version == 0
        || authority.source
            != format!(
                "https://github.com/microsoft/onnxruntime/releases/tag/v{}",
                authority.version
            )
        || authority.license != "MIT"
        || authority.targets.len() != 4
        || !expected_targets.iter().all(|name| authority.targets.contains_key(*name))
        || !is_safe_file_name(&target.asset)
        || !is_sha256(&target.sha256)
        || !is_sha256(&target.library_sha256)
        || target.library_bytes == 0
        || target.library_bytes > MAX_RUNTIME_LIBRARY_BYTES
        || !(256 * 1024 * 1024..=2 * 1024 * 1024 * 1024 * 1024)
            .contains(&target.worker_address_space_overhead_bytes)
        || !matches!(target.binary_format.as_str(), "elf" | "mach-o" | "pe")
        || !load_identity_is_safe(&target.binary_format, &target.load_identity)
        || target.rpaths.len() > MAX_BINARY_DEPENDENCIES
        || target.system_dependencies.len() > MAX_BINARY_DEPENDENCIES
        || target.companion_dependencies.len() > MAX_BINARY_DEPENDENCIES
        || !target.companion_dependencies.is_empty()
        || target.system_dependencies.is_empty()
        || !dependencies_are_system_only(target_name, &target.system_dependencies)
        || !companions_are_safe(&target.companion_dependencies)
        || !is_safe_relative(Path::new(&target.library))
    {
        return Err(LoadError::VersionMismatch);
    }
    Ok(())
}

fn load_identity_is_safe(format: &str, identity: &str) -> bool {
    match format {
        "elf" | "pe" => is_safe_file_name(identity),
        "mach-o" => identity.strip_prefix("@rpath/").is_some_and(is_safe_file_name),
        _ => false,
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_safe_file_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && Path::new(value).file_name().is_some_and(|name| name == value)
        && !value.contains('/')
        && !value.contains('\\')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

fn dependencies_are_system_only(target_name: &str, dependencies: &[SystemDependency]) -> bool {
    let mut unique = std::collections::BTreeSet::new();
    dependencies.iter().all(|dependency| {
        if dependency.load_name.is_empty() || dependency.load_name.len() > MAX_BINARY_PATH_BYTES {
            return false;
        }
        let normalized = if target_name == "x86_64-pc-windows-msvc" {
            dependency.load_name.to_ascii_lowercase()
        } else {
            dependency.load_name.clone()
        };
        unique.insert(normalized)
            && if target_name == "aarch64-apple-darwin" {
                let Some(expected_path) = dependency.path.as_deref() else {
                    return false;
                };
                if expected_path != dependency.load_name {
                    return false;
                }
                let path = Path::new(expected_path);
                path.is_absolute()
                    && (path.starts_with("/System/Library") || path.starts_with("/usr/lib"))
                    && path.components().all(|component| {
                        matches!(component, Component::RootDir | Component::Normal(_))
                    })
            } else {
                dependency.path.is_none() && is_safe_file_name(&dependency.load_name)
            }
    })
}

fn companions_are_safe(dependencies: &[CompanionDependency]) -> bool {
    let mut names = std::collections::BTreeSet::new();
    dependencies.iter().all(|dependency| {
        !dependency.load_name.is_empty()
            && dependency.load_name.len() <= MAX_BINARY_PATH_BYTES
            && names.insert(dependency.load_name.as_str())
            && is_safe_relative(Path::new(&dependency.path))
            && is_sha256(&dependency.sha256)
    })
}

fn read_binary_metadata(path: &Path, expected_bytes: u64) -> Result<BinaryMetadata, LoadError> {
    let file = File::open(path).map_err(|_| LoadError::Io)?;
    if file.metadata().map_err(|_| LoadError::Io)?.len() != expected_bytes {
        return Err(LoadError::DependencyMismatch);
    }
    let capacity = usize::try_from(expected_bytes).map_err(|_| LoadError::DependencyMismatch)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| LoadError::Io)?;
    file.take(expected_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| LoadError::Io)?;
    if bytes.len() != capacity {
        return Err(LoadError::DependencyMismatch);
    }
    parse_binary_metadata(&bytes, path.file_name().ok_or(LoadError::DependencyMismatch)?)
}

fn parse_binary_metadata(
    bytes: &[u8],
    file_name: &std::ffi::OsStr,
) -> Result<BinaryMetadata, LoadError> {
    let file = object::File::parse(bytes).map_err(|_| LoadError::DependencyMismatch)?;
    let architecture = match file.architecture() {
        object::Architecture::Aarch64 => "aarch64",
        object::Architecture::X86_64 => "x86_64",
        _ => return Err(LoadError::DependencyMismatch),
    };
    let mut dependencies = file
        .import_libraries()
        .map_err(|_| LoadError::DependencyMismatch)?
        .map(|library| {
            let library = library.map_err(|_| LoadError::DependencyMismatch)?;
            checked_binary_string(library.name())
        })
        .collect::<Result<Vec<_>, LoadError>>()?;
    canonicalize_binary_names(&mut dependencies)?;
    let (format, load_identity, mut rpaths) = match &file {
        object::File::Elf32(file) => elf_identity_and_rpaths(file)?,
        object::File::Elf64(file) => elf_identity_and_rpaths(file)?,
        object::File::MachO32(file) => macho_identity_and_rpaths(file)?,
        object::File::MachO64(file) => macho_identity_and_rpaths(file)?,
        object::File::Pe32(_) | object::File::Pe64(_) => {
            ("pe", file_name.to_str().ok_or(LoadError::DependencyMismatch)?.to_owned(), Vec::new())
        }
        _ => return Err(LoadError::DependencyMismatch),
    };
    canonicalize_binary_names(&mut rpaths)?;
    Ok(BinaryMetadata { format, architecture, load_identity, dependencies, rpaths })
}

fn checked_binary_string(bytes: &[u8]) -> Result<String, LoadError> {
    if bytes.is_empty() || bytes.len() > MAX_BINARY_PATH_BYTES || bytes.contains(&0) {
        return Err(LoadError::DependencyMismatch);
    }
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| LoadError::DependencyMismatch)
}

fn canonicalize_binary_names(values: &mut [String]) -> Result<(), LoadError> {
    if values.len() > MAX_BINARY_DEPENDENCIES {
        return Err(LoadError::DependencyMismatch);
    }
    values.sort();
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(LoadError::DependencyMismatch);
    }
    Ok(())
}

fn elf_identity_and_rpaths<Elf>(
    file: &object::read::elf::ElfFile<'_, Elf>,
) -> Result<(&'static str, String, Vec<String>), LoadError>
where
    Elf: ElfFileHeader,
{
    let (identity, rpaths) = elf_optional_identity_and_rpaths(file)?;
    Ok(("elf", identity.ok_or(LoadError::DependencyMismatch)?, rpaths))
}

fn elf_optional_identity_and_rpaths<Elf>(
    file: &object::read::elf::ElfFile<'_, Elf>,
) -> Result<(Option<String>, Vec<String>), LoadError>
where
    Elf: ElfFileHeader,
{
    let dynamic = file.elf_dynamic_table().map_err(|_| LoadError::DependencyMismatch)?;
    let mut identity = None;
    let mut rpaths = Vec::new();
    for entry in &dynamic {
        if entry.tag == object::elf::DT_SONAME {
            if identity.is_some() {
                return Err(LoadError::DependencyMismatch);
            }
            identity = Some(checked_binary_string(
                dynamic.string(entry).map_err(|_| LoadError::DependencyMismatch)?,
            )?);
        } else if entry.tag == object::elf::DT_RPATH || entry.tag == object::elf::DT_RUNPATH {
            let joined = checked_binary_string(
                dynamic.string(entry).map_err(|_| LoadError::DependencyMismatch)?,
            )?;
            for path in joined.split(':') {
                rpaths.push(checked_binary_string(path.as_bytes())?);
            }
        }
    }
    Ok((identity, rpaths))
}

fn macho_identity_and_rpaths<Mach>(
    file: &object::read::macho::MachOFile<'_, Mach>,
) -> Result<(&'static str, String, Vec<String>), LoadError>
where
    Mach: MachHeader,
{
    let (identity, rpaths) = macho_optional_identity_and_rpaths(file)?;
    Ok(("mach-o", identity.ok_or(LoadError::DependencyMismatch)?, rpaths))
}

fn macho_optional_identity_and_rpaths<Mach>(
    file: &object::read::macho::MachOFile<'_, Mach>,
) -> Result<(Option<String>, Vec<String>), LoadError>
where
    Mach: MachHeader,
{
    let endian = file.endian();
    let mut commands = file.macho_load_commands().map_err(|_| LoadError::DependencyMismatch)?;
    let mut identity = None;
    let mut rpaths = Vec::new();
    while let Some(command) = commands.next().map_err(|_| LoadError::DependencyMismatch)? {
        match command.variant().map_err(|_| LoadError::DependencyMismatch)? {
            object::read::macho::LoadCommandVariant::IdDylib(dylib) => {
                if identity.is_some() {
                    return Err(LoadError::DependencyMismatch);
                }
                identity = Some(checked_binary_string(
                    command
                        .string(endian, dylib.dylib.name)
                        .map_err(|_| LoadError::DependencyMismatch)?,
                )?);
            }
            object::read::macho::LoadCommandVariant::Rpath(rpath) => {
                rpaths.push(checked_binary_string(
                    command
                        .string(endian, rpath.path)
                        .map_err(|_| LoadError::DependencyMismatch)?,
                )?);
            }
            _ => {}
        }
    }
    Ok((identity, rpaths))
}

fn validate_binary_metadata(target: &Target, actual: &BinaryMetadata) -> Result<(), LoadError> {
    let mut expected_dependencies = target
        .system_dependencies
        .iter()
        .map(|dependency| dependency.load_name.clone())
        .chain(target.companion_dependencies.iter().map(|dependency| dependency.load_name.clone()))
        .collect::<Vec<_>>();
    canonicalize_binary_names(&mut expected_dependencies)?;
    let mut expected_rpaths = target.rpaths.clone();
    canonicalize_binary_names(&mut expected_rpaths)?;
    if actual.format != target.binary_format
        || actual.architecture != target.binary_architecture
        || actual.load_identity != target.load_identity
        || actual.dependencies != expected_dependencies
        || actual.rpaths != expected_rpaths
        || !rpaths_are_safe(&target.binary_format, &actual.rpaths)
    {
        return Err(LoadError::DependencyMismatch);
    }
    Ok(())
}

fn rpaths_are_safe(format: &str, rpaths: &[String]) -> bool {
    match format {
        // These exact paths refer only to the create-new private directory.
        "elf" => rpaths == ["$ORIGIN"],
        "mach-o" => rpaths == ["@loader_path"],
        "pe" => rpaths.is_empty(),
        _ => false,
    }
}

fn ensure_loader_environment_clean() -> Result<(), LoadError> {
    #[cfg(target_os = "linux")]
    const VARIABLES: &[&str] = &["LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT"];
    #[cfg(target_os = "macos")]
    const VARIABLES: &[&str] = &[
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
    ];
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const VARIABLES: &[&str] = &[];
    if VARIABLES.iter().any(|name| std::env::var_os(name).is_some()) {
        return Err(LoadError::DependencyMismatch);
    }
    Ok(())
}

fn load_verified_library(path: &Path) -> Result<Library, LoadError> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            Library as WindowsLibrary,
        };
        // SAFETY: `path` is an absolute create-new private file verified by
        // SHA-256. The flags constrain dependency resolution to that private
        // directory and System32, excluding CWD, PATH, and user directories.
        return unsafe {
            WindowsLibrary::load_with_flags(
                path,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
        .map(Into::into)
        .map_err(|_| LoadError::MissingEntryPoint);
    }
    #[cfg(unix)]
    {
        use libloading::os::unix::{Library as UnixLibrary, RTLD_LOCAL, RTLD_NOW};
        // SAFETY: `path` is the hash-verified private file; the environment and
        // current loaded-module set were audited immediately before this call.
        return unsafe { UnixLibrary::open(Some(path), RTLD_NOW | RTLD_LOCAL) }
            .map(Into::into)
            .map_err(|_| LoadError::MissingEntryPoint);
    }
    #[allow(unreachable_code)]
    Err(LoadError::UnsupportedTarget)
}

fn audit_loaded_modules(target: &Target, trusted_main: Option<&Path>) -> Result<(), LoadError> {
    #[cfg(target_os = "linux")]
    return audit_loaded_linux(target, trusted_main);
    #[cfg(target_os = "macos")]
    return audit_loaded_macos(target, trusted_main);
    #[cfg(windows)]
    return audit_loaded_windows(target, trusted_main);
    #[allow(unreachable_code)]
    Err(LoadError::UnsupportedTarget)
}

#[cfg(target_os = "linux")]
fn audit_loaded_linux(target: &Target, trusted_main: Option<&Path>) -> Result<(), LoadError> {
    struct Audit {
        paths: Vec<PathBuf>,
        rejected: bool,
    }
    unsafe extern "C" fn inspect(
        information: *mut libc::dl_phdr_info,
        _size: usize,
        opaque: *mut std::ffi::c_void,
    ) -> std::ffi::c_int {
        // SAFETY: `dl_iterate_phdr` calls synchronously with the exact context
        // pointer and a live `dl_phdr_info` for the duration of this callback.
        let (information, audit) = unsafe { (&*information, &mut *opaque.cast::<Audit>()) };
        if information.dlpi_name.is_null() {
            return 0;
        }
        // SAFETY: the platform loader supplies a NUL-terminated image name.
        let path = unsafe { CStr::from_ptr(information.dlpi_name) };
        let path = Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
        if path.is_absolute() {
            audit.paths.push(path.to_owned());
        } else if path != Path::new("linux-vdso.so.1") {
            audit.rejected = true;
            return 1;
        }
        0
    }
    use std::os::unix::ffi::OsStrExt;
    let mut audit = Audit { paths: Vec::new(), rejected: false };
    // SAFETY: the callback and opaque context remain valid for this synchronous
    // enumeration and do not escape it.
    unsafe { libc::dl_iterate_phdr(Some(inspect), (&raw mut audit).cast()) };
    if audit.rejected {
        Err(LoadError::DependencyMismatch)
    } else {
        audit_loaded_paths(target, trusted_main, &audit.paths)
    }
}

#[cfg(target_os = "macos")]
fn audit_loaded_macos(target: &Target, trusted_main: Option<&Path>) -> Result<(), LoadError> {
    unsafe extern "C" {
        fn _dyld_image_count() -> u32;
        fn _dyld_get_image_name(index: u32) -> *const std::ffi::c_char;
    }
    // SAFETY: dyld image enumeration returns process-lifetime NUL-terminated
    // names for indices below the captured image count.
    let count = unsafe { _dyld_image_count() };
    let mut paths = Vec::new();
    for index in 0..count {
        // SAFETY: `index` is below the count captured immediately above.
        let pointer = unsafe { _dyld_get_image_name(index) };
        if pointer.is_null() {
            continue;
        }
        // SAFETY: dyld documents this as a NUL-terminated image path.
        let path = Path::new(
            unsafe { CStr::from_ptr(pointer) }
                .to_str()
                .map_err(|_| LoadError::DependencyMismatch)?,
        );
        if path.is_absolute() {
            paths.push(path.to_owned());
        }
    }
    audit_loaded_paths(target, trusted_main, &paths)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn audit_loaded_paths(
    target: &Target,
    trusted_main: Option<&Path>,
    paths: &[PathBuf],
) -> Result<(), LoadError> {
    for path in paths {
        let Some(identity) = loaded_binary_identity(path)? else {
            continue;
        };
        if identity == target.load_identity {
            if trusted_main != Some(path.as_path()) || hash_file(path)? != target.library_sha256 {
                return Err(LoadError::DependencyMismatch);
            }
            continue;
        }
        if let Some(companion) =
            target.companion_dependencies.iter().find(|dependency| dependency.load_name == identity)
        {
            let Some(root) = trusted_main.and_then(Path::parent) else {
                return Err(LoadError::DependencyMismatch);
            };
            let expected = root.join(&companion.path);
            if path != &expected || hash_file(path)? != companion.sha256 {
                return Err(LoadError::DependencyMismatch);
            }
            continue;
        }
        if let Some(system) =
            target.system_dependencies.iter().find(|dependency| dependency.load_name == identity)
        {
            let safe = if let Some(expected) = system.path.as_deref() {
                path == Path::new(expected)
            } else {
                path.starts_with("/lib")
                    || path.starts_with("/lib64")
                    || path.starts_with("/usr/lib")
                    || path.starts_with("/usr/lib64")
            };
            if !safe {
                return Err(LoadError::DependencyMismatch);
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn loaded_binary_identity(path: &Path) -> Result<Option<String>, LoadError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        // System images in dyld's shared cache need not exist as standalone
        // files. Their absolute authority path is still checked by dyld.
        Err(_) if path.starts_with("/System/Library") || path.starts_with("/usr/lib") => {
            return Ok(path.to_str().map(str::to_owned));
        }
        Err(_) => return Err(LoadError::DependencyMismatch),
    };
    if metadata.len() == 0 || metadata.len() > MAX_RUNTIME_LIBRARY_BYTES {
        return Err(LoadError::DependencyMismatch);
    }
    let bytes = fs::read(path).map_err(|_| LoadError::DependencyMismatch)?;
    let Ok(file) = object::File::parse(bytes.as_slice()) else {
        return Ok(None);
    };
    match file {
        object::File::Elf32(file) => {
            elf_optional_identity_and_rpaths(&file).map(|(identity, _)| identity)
        }
        object::File::Elf64(file) => {
            elf_optional_identity_and_rpaths(&file).map(|(identity, _)| identity)
        }
        object::File::MachO32(file) => {
            macho_optional_identity_and_rpaths(&file).map(|(identity, _)| identity)
        }
        object::File::MachO64(file) => {
            macho_optional_identity_and_rpaths(&file).map(|(identity, _)| identity)
        }
        _ => Ok(None),
    }
}

fn hash_file(path: &Path) -> Result<String, LoadError> {
    let mut file = File::open(path).map_err(|_| LoadError::DependencyMismatch)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|_| LoadError::DependencyMismatch)?;
        if count == 0 {
            return Ok(format!("{:x}", digest.finalize()));
        }
        digest.update(&buffer[..count]);
    }
}

#[cfg(windows)]
fn audit_loaded_windows(target: &Target, trusted_main: Option<&Path>) -> Result<(), LoadError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut system = vec![0_u16; 32_768];
    // SAFETY: `system` is writable for the supplied capacity.
    let count = unsafe {
        GetSystemDirectoryW(system.as_mut_ptr(), u32::try_from(system.len()).unwrap_or(u32::MAX))
    };
    if count == 0 || usize::try_from(count).map_or(true, |count| count >= system.len()) {
        return Err(LoadError::DependencyMismatch);
    }
    let system = normalize_windows_path(&String::from_utf16_lossy(
        &system[..usize::try_from(count).unwrap()],
    ));
    let names = std::iter::once(target.load_identity.as_str())
        .chain(target.system_dependencies.iter().map(|dependency| dependency.load_name.as_str()))
        .chain(
            target.companion_dependencies.iter().map(|dependency| dependency.load_name.as_str()),
        );
    for name in names {
        let wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL terminated and retained through the call.
        let module = unsafe { GetModuleHandleW(wide.as_ptr()) };
        if module.is_null() {
            continue;
        }
        let is_main = name.eq_ignore_ascii_case(&target.load_identity);
        let mut path = vec![0_u16; 32_768];
        // SAFETY: `module` is a live loaded module and `path` is writable for
        // the supplied capacity.
        let length = unsafe {
            GetModuleFileNameW(
                module,
                path.as_mut_ptr(),
                u32::try_from(path.len()).unwrap_or(u32::MAX),
            )
        };
        if length == 0 || usize::try_from(length).map_or(true, |length| length >= path.len()) {
            return Err(LoadError::DependencyMismatch);
        }
        let loaded = normalize_windows_path(&String::from_utf16_lossy(
            &path[..usize::try_from(length).unwrap()],
        ));
        if is_main {
            let trusted = trusted_main.map(|path| normalize_windows_path(&path.to_string_lossy()));
            if trusted.as_deref() != Some(loaded.as_str()) {
                return Err(LoadError::DependencyMismatch);
            }
            if hash_file(Path::new(&loaded))? != target.library_sha256 {
                return Err(LoadError::DependencyMismatch);
            }
            continue;
        }
        if loaded != system && !loaded.starts_with(&(system.clone() + "/")) {
            return Err(LoadError::DependencyMismatch);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn normalize_windows_path(value: &str) -> String {
    let normalized = value.replace('\\', "/").to_ascii_lowercase();
    if let Some(path) = normalized.strip_prefix("//?/unc/") {
        format!("//{path}")
    } else {
        normalized.strip_prefix("//?/").unwrap_or(&normalized).to_owned()
    }
}

fn current_target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

fn is_safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|part| matches!(part, Component::Normal(_)))
}

fn open_explicit_no_follow(root: &Path, path: &Path) -> Result<(File, PathBuf), LoadError> {
    if !root.is_absolute() || !path.is_absolute() {
        return Err(LoadError::UnsafePath);
    }
    let canonical_root = root.canonicalize().map_err(|_| LoadError::UnsafePath)?;
    let relative = path.strip_prefix(root).map_err(|_| LoadError::UnsafePath)?;
    if !is_safe_relative(relative) {
        return Err(LoadError::UnsafePath);
    }
    let candidate = canonical_root.join(relative);
    let mut cursor = canonical_root.clone();
    for part in relative.components() {
        cursor.push(part.as_os_str());
        let metadata = fs::symlink_metadata(&cursor).map_err(|_| LoadError::Io)?;
        if metadata.file_type().is_symlink() || is_windows_reparse_point(&metadata) {
            return Err(LoadError::UnsafePath);
        }
    }
    let file = open_no_follow(&candidate)?;
    let canonical = candidate.canonicalize().map_err(|_| LoadError::UnsafePath)?;
    if !canonical.starts_with(&canonical_root)
        || !file.metadata().map_err(|_| LoadError::Io)?.is_file()
    {
        return Err(LoadError::UnsafePath);
    }
    ensure_same_open_file(&file, &canonical)?;
    Ok((file, canonical))
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> Result<File, LoadError> {
    use rustix::fs::{Mode, OFlags};
    let fd =
        rustix::fs::open(path, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
            .map_err(|_| LoadError::UnsafePath)?;
    Ok(File::from(fd))
}

#[cfg(windows)]
fn open_no_follow(path: &Path) -> Result<File, LoadError> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| LoadError::UnsafePath)
}

#[cfg(not(any(unix, windows)))]
fn open_no_follow(_path: &Path) -> Result<File, LoadError> {
    Err(LoadError::UnsupportedTarget)
}

fn ensure_same_open_file(file: &File, path: &Path) -> Result<(), LoadError> {
    let opened = file.metadata().map_err(|_| LoadError::Io)?;
    let named = fs::metadata(path).map_err(|_| LoadError::Io)?;
    if opened.len() != named.len() || opened.modified().ok() != named.modified().ok() {
        return Err(LoadError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != named.dev() || opened.ino() != named.ino() {
            return Err(LoadError::UnsafePath);
        }
    }
    #[cfg(windows)]
    {
        let named_file = open_no_follow(path)?;
        if windows_file_identity(file)? != windows_file_identity(&named_file)? {
            return Err(LoadError::UnsafePath);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> Result<(u32, u64), LoadError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` owns a valid live Windows file handle, and `information`
    // points to writable storage of the exact API structure size.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } != 0;
    if !succeeded {
        return Err(LoadError::Io);
    }
    // SAFETY: successful `GetFileInformationByHandle` initializes the complete
    // output structure.
    let information = unsafe { information.assume_init() };
    Ok((
        information.dwVolumeSerialNumber,
        u64::from(information.nFileIndexHigh) << 32 | u64::from(information.nFileIndexLow),
    ))
}

#[cfg(not(windows))]
const fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn copy_and_hash(
    source: &mut File,
    destination: &mut File,
    expected_bytes: u64,
) -> Result<String, LoadError> {
    source.seek(SeekFrom::Start(0)).map_err(|_| LoadError::Io)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    loop {
        let count = source.read(&mut buffer).map_err(|_| LoadError::Io)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(count).map_err(|_| LoadError::Io)?)
            .filter(|total| *total <= expected_bytes)
            .ok_or(LoadError::HashMismatch)?;
        digest.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(|_| LoadError::Io)?;
    }
    if total != expected_bytes {
        return Err(LoadError::HashMismatch);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn probe(library: &Library, api_version: u32) -> Result<(String, ort::sys::OrtApi), LoadError> {
    type GetApiBase = unsafe extern "system" fn() -> *const OrtApiBase;
    // SAFETY: symbol lookup is limited to the hash-verified official library,
    // and the symbol type exactly matches the official C header.
    let getter = unsafe { library.get::<GetApiBase>(b"OrtGetApiBase\0") }
        .map_err(|_| LoadError::MissingEntryPoint)?;
    // SAFETY: same invariant as above; the official function has no arguments.
    let base = unsafe { getter() };
    // SAFETY: the non-null pointer is returned by the retained official
    // `OrtGetApiBase` function and points to static library storage.
    let base = unsafe { base.as_ref() }.ok_or(LoadError::MissingEntryPoint)?;
    let version = probe_base(base, api_version)?;
    // SAFETY: the API-28 table is the exact layout generated by the selected
    // ort-sys binding. API-29 support was independently probed above.
    let api = unsafe { (base.GetApi)(ort::sys::ORT_API_VERSION) };
    // SAFETY: a non-null API pointer returned by the official base points to a
    // static table whose API-28 prefix exactly matches the binding.
    let api = unsafe { api.as_ref() }.ok_or(LoadError::ApiMismatch)?.clone();
    Ok((version, api))
}

fn probe_base(base: &OrtApiBase, api_version: u32) -> Result<String, LoadError> {
    // SAFETY: `GetApi` is obtained from the verified `OrtApiBase`; a null
    // result is handled and no field beyond the opaque pointer is accessed.
    if unsafe { (base.GetApi)(api_version) }.is_null() {
        return Err(LoadError::ApiMismatch);
    }
    // SAFETY: `GetVersionString` belongs to the retained official library.
    // The returned storage is documented as static and NUL terminated.
    let pointer = unsafe { (base.GetVersionString)() };
    parse_version_pointer(pointer)
}

fn parse_version_pointer(pointer: *const std::ffi::c_char) -> Result<String, LoadError> {
    if pointer.is_null() {
        return Err(LoadError::VersionMismatch);
    }
    // SAFETY: callers only pass the official static version buffer. Scanning is
    // bounded so malformed non-NUL test buffers cannot trigger an unbounded read.
    let length = unsafe {
        (0..MAX_VERSION_BYTES)
            .find(|offset| pointer.add(*offset).read() == 0)
            .ok_or(LoadError::VersionMismatch)?
    };
    // SAFETY: the bounded scan established a NUL within the same official
    // static buffer.
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    if bytes.len() != length {
        return Err(LoadError::VersionMismatch);
    }
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| LoadError::VersionMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_ocr::{
        Dimension, ModelContract, ModelIdentity, TensorElementType as ContractElementType,
        TensorSpec,
    };

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = u8::try_from(value & 0x7f).unwrap();
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn field_bytes(field: u64, value: &[u8]) -> Vec<u8> {
        let mut bytes = encode_varint((field << 3) | 2);
        bytes.extend(encode_varint(u64::try_from(value.len()).unwrap()));
        bytes.extend(value);
        bytes
    }

    fn value_info(name: &str) -> Vec<u8> {
        let shape = field_bytes(1, &[0x08, 0x01]);
        let mut tensor_type = vec![0x08, 0x01]; // FLOAT
        tensor_type.extend(field_bytes(2, &shape));
        let type_proto = field_bytes(1, &tensor_type);
        let mut info = field_bytes(1, name.as_bytes());
        info.extend(field_bytes(2, &type_proto));
        info
    }

    fn tiny_identity_model() -> ResolvedModel {
        let mut node = field_bytes(1, b"x");
        node.extend(field_bytes(2, b"y"));
        node.extend(field_bytes(4, b"Identity"));
        let mut graph = field_bytes(1, &node);
        graph.extend(field_bytes(2, b"identity"));
        graph.extend(field_bytes(11, &value_info("x")));
        graph.extend(field_bytes(12, &value_info("y")));
        let mut model = vec![0x08, 0x09];
        model.extend(field_bytes(2, b"into-markdown-test"));
        model.extend(field_bytes(7, &graph));
        model.extend(field_bytes(8, &[0x10, 0x12]));
        let bytes: Arc<[u8]> = Arc::from(model);
        let tensor = |name: &str| TensorSpec {
            name: name.into(),
            element_type: ContractElementType::Float32,
            dimensions: vec![Dimension::Exact(1)],
        };
        ResolvedModel {
            identity: ModelIdentity {
                canonical_path: PathBuf::from("/audited-test/identity.onnx"),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                bytes: u64::try_from(bytes.len()).unwrap(),
                file_identity: "native-test-fixture".into(),
            },
            contract: ModelContract {
                ir_version: 9,
                opsets: std::collections::BTreeMap::from([(String::new(), 18)]),
                inputs: vec![tensor("x")],
                overridable_inputs: Vec::new(),
                outputs: vec![tensor("y")],
                session_memory_bytes: 64 * 1024 * 1024,
                run_memory_bytes: 1024 * 1024,
            },
            bytes,
        }
    }

    fn expanding_model() -> ResolvedModel {
        const ELEMENTS: u64 = 2_000_000_000_000;
        let mut node = field_bytes(1, b"x");
        node.extend(field_bytes(1, b"shape"));
        node.extend(field_bytes(2, b"y"));
        node.extend(field_bytes(4, b"Expand"));

        let mut initializer = vec![0x08, 0x01, 0x10, 0x07];
        initializer.extend(field_bytes(8, b"shape"));
        initializer.extend(field_bytes(9, &i64::try_from(ELEMENTS).unwrap().to_le_bytes()));

        let mut graph = field_bytes(1, &node);
        graph.extend(field_bytes(2, b"expanding"));
        graph.extend(field_bytes(5, &initializer));
        graph.extend(field_bytes(11, &value_info("x")));
        let symbolic_dimension = field_bytes(2, b"expanded");
        let shape = field_bytes(1, &symbolic_dimension);
        let mut tensor_type = vec![0x08, 0x01];
        tensor_type.extend(field_bytes(2, &shape));
        let type_proto = field_bytes(1, &tensor_type);
        let mut output = field_bytes(1, b"y");
        output.extend(field_bytes(2, &type_proto));
        graph.extend(field_bytes(12, &output));
        let mut model = vec![0x08, 0x09];
        model.extend(field_bytes(7, &graph));
        model.extend(field_bytes(8, &[0x10, 0x12]));
        let bytes: Arc<[u8]> = Arc::from(model);
        ResolvedModel {
            identity: ModelIdentity {
                canonical_path: PathBuf::from("/audited-test/expanding.onnx"),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                bytes: u64::try_from(bytes.len()).unwrap(),
                file_identity: "native-expanding-fixture".into(),
            },
            contract: ModelContract {
                ir_version: 9,
                opsets: std::collections::BTreeMap::from([(String::new(), 18)]),
                inputs: vec![TensorSpec {
                    name: "x".into(),
                    element_type: ContractElementType::Float32,
                    dimensions: vec![Dimension::Exact(1)],
                }],
                overridable_inputs: Vec::new(),
                outputs: vec![TensorSpec {
                    name: "y".into(),
                    element_type: ContractElementType::Float32,
                    dimensions: vec![Dimension::Dynamic {
                        min: 1,
                        max: usize::try_from(ELEMENTS).unwrap(),
                    }],
                }],
                session_memory_bytes: 64 * 1024 * 1024,
                run_memory_bytes: 1024 * 1024,
            },
            bytes,
        }
    }

    #[test]
    fn authority_has_exactly_four_targets_and_no_macos_x64() {
        let authority = authority().unwrap();
        assert_eq!(authority.targets.len(), 4);
        assert!(!authority.targets.contains_key("x86_64-apple-darwin"));
    }

    #[test]
    fn implicit_and_symlink_paths_are_rejected() {
        assert_eq!(
            open_explicit_no_follow(Path::new("relative"), Path::new("libonnxruntime.so"))
                .unwrap_err(),
            LoadError::UnsafePath
        );
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("runtime");
        fs::write(&library, b"not a library").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&library, directory.path().join("link")).unwrap();
            assert_eq!(
                open_explicit_no_follow(directory.path(), &directory.path().join("link"))
                    .unwrap_err(),
                LoadError::UnsafePath
            );
        }
    }

    #[test]
    fn dependency_authority_accepts_only_platform_system_names() {
        assert!(dependencies_are_system_only(
            "aarch64-apple-darwin",
            &[SystemDependency {
                load_name: "/usr/lib/libSystem.B.dylib".into(),
                path: Some("/usr/lib/libSystem.B.dylib".into()),
            }]
        ));
        assert!(!dependencies_are_system_only(
            "aarch64-apple-darwin",
            &[SystemDependency { load_name: "@rpath/libc++.dylib".into(), path: None }]
        ));
        assert!(dependencies_are_system_only(
            "x86_64-unknown-linux-gnu",
            &[SystemDependency { load_name: "libc.so.6".into(), path: None }]
        ));
        assert!(!dependencies_are_system_only(
            "x86_64-unknown-linux-gnu",
            &[SystemDependency { load_name: "../libc.so.6".into(), path: None }]
        ));
        assert!(!dependencies_are_system_only(
            "x86_64-pc-windows-msvc",
            &[
                SystemDependency { load_name: "KERNEL32.dll".into(), path: None },
                SystemDependency { load_name: "kernel32.DLL".into(), path: None },
            ]
        ));
    }

    #[test]
    fn invalid_c_strings_fail_with_a_stable_error() {
        let missing_nul = [std::ffi::c_char::try_from(b'1').unwrap(); MAX_VERSION_BYTES];
        assert_eq!(
            parse_version_pointer(missing_nul.as_ptr()).unwrap_err(),
            LoadError::VersionMismatch
        );
        let invalid_utf8 = [std::ffi::c_char::MIN, 0];
        assert_eq!(
            parse_version_pointer(invalid_utf8.as_ptr()).unwrap_err(),
            LoadError::VersionMismatch
        );
    }

    #[test]
    fn api_and_version_mismatch_are_rejected() {
        unsafe extern "system" fn missing_api(_version: u32) -> *const ort::sys::OrtApi {
            std::ptr::null()
        }
        unsafe extern "system" fn present_api(_version: u32) -> *const ort::sys::OrtApi {
            std::ptr::NonNull::<ort::sys::OrtApi>::dangling().as_ptr()
        }
        unsafe extern "system" fn version() -> *const std::ffi::c_char {
            c"wrong".as_ptr()
        }
        let missing = OrtApiBase { GetApi: missing_api, GetVersionString: version };
        assert_eq!(probe_base(&missing, 29).unwrap_err(), LoadError::ApiMismatch);
        let wrong = OrtApiBase { GetApi: present_api, GetVersionString: version };
        assert_eq!(probe_base(&wrong, 29).unwrap(), "wrong");
    }

    #[test]
    fn explicit_archives_match_preload_binary_authority() {
        if option_env!("ORT_AUDIT_ALL_ARCHIVES").is_none() {
            return;
        }
        let root = PathBuf::from(std::env::var_os("TEST_SRCDIR").unwrap());
        let locations = [
            ("aarch64-apple-darwin", "+downloads+onnxruntime_macos_arm64/lib/libonnxruntime.dylib"),
            (
                "aarch64-unknown-linux-gnu",
                "+downloads+onnxruntime_linux_arm64/lib/libonnxruntime.so.1.29.0",
            ),
            (
                "x86_64-unknown-linux-gnu",
                "+downloads+onnxruntime_linux_x86_64/lib/libonnxruntime.so.1.29.0",
            ),
            ("x86_64-pc-windows-msvc", "+downloads+onnxruntime_windows_x86_64/lib/onnxruntime.dll"),
        ];
        let authority = authority().unwrap();
        for (name, location) in locations {
            let target = authority.targets.get(name).unwrap();
            let metadata =
                read_binary_metadata(&root.join(location), target.library_bytes).unwrap();
            validate_binary_metadata(target, &metadata)
                .unwrap_or_else(|error| panic!("{name}: {error:?}: {metadata:?}"));
        }
    }

    #[test]
    fn explicit_native_runtime_matches_hash_version_and_api() {
        let Some(repository) = option_env!("ORT_TEST_REPOSITORY") else {
            return;
        };
        let Some(library) = option_env!("ORT_TEST_LIBRARY") else {
            panic!("ORT_TEST_LIBRARY must accompany ORT_TEST_REPOSITORY");
        };
        let Some(worker) = option_env!("ORT_TEST_WORKER") else {
            panic!("ORT_TEST_WORKER must accompany native validation");
        };
        assert_ne!(repository, "unsupported");
        let runfiles = PathBuf::from(std::env::var_os("TEST_SRCDIR").unwrap());
        let runfile = runfiles.join(repository).join(library);
        let worker = runfiles.join(worker).canonicalize().unwrap();
        let canonical_library = runfile.canonicalize().unwrap();
        let component_count = Path::new(library).components().count();
        let trusted_root =
            canonical_library.ancestors().nth(component_count).unwrap().to_path_buf();
        let loaded = Arc::new(RuntimeLibrary::load(&trusted_root, &canonical_library).unwrap());
        let authority = authority().unwrap();
        assert_eq!(loaded.version(), authority.version);
        assert_eq!(loaded.api_version(), authority.api_version);
        let model = tiny_identity_model();
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let factory = OrtSessionFactory::new(Arc::clone(&loaded), &worker).unwrap();
        let session = factory.create(&model, &SessionOptions::default(), &context).unwrap();
        drop(factory);
        let outputs =
            session.run(&[Tensor { shape: vec![1], values: vec![3.5] }], &context).unwrap();
        assert_eq!(outputs, [Tensor { shape: vec![1], values: vec![3.5] }]);
        drop(session);

        let expanding = expanding_model();
        let limited = OrtSessionFactory::new(Arc::clone(&loaded), &worker).unwrap();
        let session = limited.create(&expanding, &SessionOptions::default(), &context).unwrap();
        let error =
            session.run(&[Tensor { shape: vec![1], values: vec![1.0] }], &context).unwrap_err();
        assert_eq!(error.code().as_str(), "resourceLimit");
        drop(session);
        drop(limited);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let fake = tempfile::tempdir().unwrap();
            let fake_worker = fake.path().join("bounded-stall-worker");
            fs::write(&fake_worker, b"#!/bin/sh\nwhile :; do :; done\n").unwrap();
            fs::set_permissions(&fake_worker, fs::Permissions::from_mode(0o700)).unwrap();
            let fake_worker = fake_worker.canonicalize().unwrap();
            let cancellation = into_markdown_core::CancellationToken::new();
            let cancelled_context = ExecutionContext::new(
                into_markdown_core::ExecutionOptions {
                    cancellation: cancellation.clone(),
                    ..into_markdown_core::ExecutionOptions::default()
                },
                into_markdown_core::ResourceLimits::default(),
            );
            let cancel = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                cancellation.cancel();
            });
            let error = worker::WorkerClient::start(
                &loaded,
                &fake_worker,
                &model.bytes,
                &model.contract,
                &SessionOptions::default(),
                &cancelled_context,
            )
            .err()
            .unwrap();
            cancel.join().unwrap();
            assert_eq!(error.code().as_str(), "cancelled");
        }

        let second = OrtSessionFactory::new(loaded, worker).unwrap();
        let session = second.create(&model, &SessionOptions::default(), &context).unwrap();
        let outputs =
            session.run(&[Tensor { shape: vec![1], values: vec![7.0] }], &context).unwrap();
        assert_eq!(outputs, [Tensor { shape: vec![1], values: vec![7.0] }]);
        drop(session);
        drop(second);
    }
}
