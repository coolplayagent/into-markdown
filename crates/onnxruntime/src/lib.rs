//! Audited native ONNX Runtime loading boundary.
//!
//! The crate is intentionally separate because dynamic symbol lookup and the C
//! ABI require `unsafe`. All consumers see only the safe [`RuntimeLibrary`].
#![deny(unsafe_op_in_unsafe_fn)]

use into_markdown_core::{ConversionError, ExecutionContext, Tensor};
use into_markdown_ocr::{
    Dimension, ModelMetadata, ResolvedModel, SessionAdapter, SessionFactory, SessionOptions,
    TensorElementType as ContractElementType, TensorSpec,
};
use libloading::Library;
use ort::sys::OrtApiBase;
use ort::{AsPointer, session::RunOptions, value::ValueType};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use thiserror::Error;

const MAX_VERSION_BYTES: usize = 64;

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
    /// Another crate initialized ORT before the audited telemetry/runtime policy.
    #[error("ONNX Runtime process-global state is incompatible")]
    GlobalStateMismatch,
    /// A different audited runtime was already fixed for this process.
    #[error("a different ONNX Runtime is already fixed for this process")]
    RuntimeConflict,
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
    load_identity: String,
    library_sha256: String,
    system_dependencies: Vec<String>,
}

/// Loaded exact CPU runtime. The private verified copy and dynamic handle live
/// for at least as long as this value.
pub struct RuntimeLibrary {
    version: String,
    api_version: u32,
    private_path: PathBuf,
    identity: String,
    _source: File,
    library: Option<Library>,
    private_dir: Option<TempDir>,
    api: ort::sys::OrtApi,
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
    /// Verify and load one explicit library below `trusted_root`.
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
        let private_dir =
            tempfile::Builder::new().prefix("into-md-ort-").tempdir().map_err(|_| LoadError::Io)?;
        let file_name = Path::new(&target.library).file_name().ok_or(LoadError::UnsafePath)?;
        let private_path = private_dir.path().join(file_name);
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&private_path)
            .map_err(|_| LoadError::Io)?;
        let actual_hash = copy_and_hash(&mut source, &mut destination)?;
        if actual_hash != target.library_sha256 {
            return Err(LoadError::HashMismatch);
        }
        destination.sync_all().map_err(|_| LoadError::Io)?;
        drop(destination);
        ensure_same_open_file(&source, &canonical)?;
        let private_path = private_path.canonicalize().map_err(|_| LoadError::UnsafePath)?;

        ensure_loader_environment_clean()?;
        audit_loaded_modules(file_name, &target.load_identity, &target.system_dependencies, None)?;
        let library = load_verified_library(&private_path)?;
        audit_loaded_modules(
            file_name,
            &target.load_identity,
            &target.system_dependencies,
            Some(&private_path),
        )?;
        let (version, api) = probe(&library, authority.api_version)?;
        if version != authority.version {
            return Err(LoadError::VersionMismatch);
        }
        Ok(Self {
            version,
            api_version: authority.api_version,
            private_path,
            identity: format!(
                "{target_name}:{}:{}:{}:{:x}",
                authority.version,
                authority.api_version,
                target.library_sha256,
                Sha256::digest(include_str!("../../../third_party/onnxruntime/manifest.json"))
            ),
            _source: source,
            library: Some(library),
            private_dir: Some(private_dir),
            api,
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

impl Drop for RuntimeLibrary {
    fn drop(&mut self) {
        // On Windows this guarantees FreeLibrary precedes removal of the
        // private directory. Process-installed runtimes are retained by a
        // static and reach neither operation during normal shutdown.
        drop(self.library.take());
        drop(self.private_dir.take());
    }
}

enum ProcessRuntime {
    Ready(Arc<RuntimeLibrary>),
    RejectedWithLibrary(Arc<RuntimeLibrary>),
    Rejected,
}

static PROCESS_RUNTIME: OnceLock<ProcessRuntime> = OnceLock::new();
static PROCESS_RUNTIME_INIT: Mutex<()> = Mutex::new(());

/// Real CPU session factory backed by the verified runtime library.
#[derive(Debug)]
pub struct OrtSessionFactory {
    library: Arc<RuntimeLibrary>,
}

impl OrtSessionFactory {
    /// Commit the process-global ORT environment from an explicit verified path.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::GlobalStateMismatch`] if another caller configured
    /// ORT first, or [`LoadError::RuntimeConflict`] if this process already
    /// fixed a different authority identity.
    pub fn new(library: Arc<RuntimeLibrary>) -> Result<Self, LoadError> {
        let _guard = lock(&PROCESS_RUNTIME_INIT);
        if let Some(state) = PROCESS_RUNTIME.get() {
            return match state {
                ProcessRuntime::Ready(installed) if installed.identity == library.identity => {
                    Ok(Self { library: Arc::clone(installed) })
                }
                ProcessRuntime::Ready(_) => Err(LoadError::RuntimeConflict),
                ProcessRuntime::RejectedWithLibrary(retained) => {
                    let _ = retained.version();
                    Err(LoadError::GlobalStateMismatch)
                }
                ProcessRuntime::Rejected => Err(LoadError::GlobalStateMismatch),
            };
        }
        if !ort::set_api(library.api.clone()) {
            let _ = PROCESS_RUNTIME.set(ProcessRuntime::Rejected);
            return Err(LoadError::GlobalStateMismatch);
        }
        if !ort::init().with_telemetry(false).commit() {
            // `set_api` retained function pointers from this library. Keep its
            // handle for process lifetime even though environment policy lost.
            let _ = PROCESS_RUNTIME.set(ProcessRuntime::RejectedWithLibrary(library));
            return Err(LoadError::GlobalStateMismatch);
        }
        let installed = Arc::clone(&library);
        let _ = PROCESS_RUNTIME.set(ProcessRuntime::Ready(installed));
        Ok(Self { library })
    }
}

#[cfg(test)]
fn commit_environment_policy(commit: impl FnOnce(bool) -> bool) -> bool {
    commit(false)
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
        let mut builder =
            ort::session::Session::builder().map_err(|_| ort_error("sessionCreate"))?;
        builder = builder
            .with_intra_threads(usize::from(options.intra_op_threads))
            .map_err(|_| ort_error("invalidThreadOptions"))?
            .with_inter_threads(usize::from(options.inter_op_threads))
            .map_err(|_| ort_error("invalidThreadOptions"))?
            .with_parallel_execution(false)
            .map_err(|_| ort_error("invalidThreadOptions"))?
            .with_memory_pattern(false)
            .map_err(|_| ort_error("invalidMemoryOptions"))?;
        configure_cpu_arena(&mut builder, options.cpu_arena)?;
        context.checkpoint()?;
        let session =
            builder.commit_from_memory(&model.bytes).map_err(|_| ort_error("sessionLoad"))?;
        let metadata = validate_outlets(&session, model)?;
        context.checkpoint()?;
        let estimate = self.estimate_bytes(model, options)?;
        Ok(Arc::new(OrtSession {
            session: Mutex::new(session),
            metadata,
            estimated_bytes: estimate,
        }))
    }
}

struct OrtSession {
    session: Mutex<ort::session::Session>,
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
        context.checkpoint()?;
        let values = self
            .metadata
            .inputs
            .iter()
            .zip(inputs)
            .map(|(spec, tensor)| {
                ort::value::Tensor::from_array((tensor.shape.clone(), tensor.values.clone()))
                    .map(|value| (spec.name.clone(), value))
                    .map_err(|_| ort_error("inputTensor"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let run_options = Arc::new(RunOptions::new().map_err(|_| ort_error("runOptions"))?);
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let monitor = {
            let run_options = Arc::clone(&run_options);
            let stopped = Arc::clone(&stopped);
            let context = context.clone();
            std::thread::spawn(move || {
                while !stopped.load(std::sync::atomic::Ordering::Acquire) {
                    if context.checkpoint().is_err() {
                        let _ = run_options.terminate();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })
        };
        let result = lock(&self.session)
            .run_with_options(values, &run_options)
            .map_err(|_| ort_error("inference"))
            .and_then(|outputs| {
                outputs
                    .iter()
                    .map(|(_, value)| {
                        let (shape, values) = value
                            .try_extract_tensor::<f32>()
                            .map_err(|_| ort_error("outputTensor"))?;
                        let shape = shape
                            .iter()
                            .map(|dimension| {
                                usize::try_from(*dimension).map_err(|_| ort_error("outputShape"))
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok(Tensor { shape, values: values.to_vec() })
                    })
                    .collect::<Result<Vec<_>, ConversionError>>()
            });
        stopped.store(true, std::sync::atomic::Ordering::Release);
        let _ = monitor.join();
        context.checkpoint()?;
        result
    }
}

fn configure_cpu_arena(
    builder: &mut ort::session::builder::SessionBuilder,
    enabled: bool,
) -> Result<(), ConversionError> {
    let operation =
        if enabled { ort::api().EnableCpuMemArena } else { ort::api().DisableCpuMemArena };
    // SAFETY: `builder.ptr_mut()` is a live, uniquely borrowed ORT session
    // options pointer. `operation` comes from the API table validated by ORT.
    let status = unsafe { operation(builder.ptr_mut()) };
    // SAFETY: the status pointer is returned by the matching ORT API and is
    // consumed exactly once by ort's status conversion routine.
    unsafe { ort::Error::result_from_status(status) }.map_err(|_| ort_error("invalidMemoryOptions"))
}

fn validate_outlets(
    session: &ort::session::Session,
    model: &ResolvedModel,
) -> Result<ModelMetadata, ConversionError> {
    if session.inputs().len() != model.contract.inputs.len()
        || session.outputs().len() != model.contract.outputs.len()
    {
        return Err(ort_error("modelIoMismatch"));
    }
    for (outlet, expected) in session.inputs().iter().zip(&model.contract.inputs) {
        validate_outlet(outlet, expected)?;
    }
    for (outlet, expected) in session.outputs().iter().zip(&model.contract.outputs) {
        validate_outlet(outlet, expected)?;
    }
    Ok(ModelMetadata {
        ir_version: model.contract.ir_version,
        opsets: model.contract.opsets.clone(),
        inputs: model.contract.inputs.clone(),
        outputs: model.contract.outputs.clone(),
    })
}

fn validate_outlet(
    outlet: &ort::value::Outlet,
    expected: &TensorSpec,
) -> Result<(), ConversionError> {
    if outlet.name() != expected.name || outlet.name().as_bytes().contains(&0) {
        return Err(ort_error("modelIoMismatch"));
    }
    let ValueType::Tensor { ty, shape, .. } = outlet.dtype() else {
        return Err(ort_error("modelDtypeMismatch"));
    };
    if expected.element_type != ContractElementType::Float32
        || *ty != ort::value::TensorElementType::Float32
        || shape.len() != expected.dimensions.len()
    {
        return Err(ort_error("modelDtypeMismatch"));
    }
    for (actual, expected) in shape.iter().zip(&expected.dimensions) {
        let compatible = match expected {
            Dimension::Exact(value) => i64::try_from(*value) == Ok(*actual),
            Dimension::Dynamic { min, max } => {
                *actual == -1
                    || usize::try_from(*actual).is_ok_and(|value| value >= *min && value <= *max)
            }
        };
        if !compatible {
            return Err(ort_error("modelShapeMismatch"));
        }
    }
    Ok(())
}

fn ort_error(detail: &'static str) -> ConversionError {
    ConversionError::Ocr { provider: "onnxruntime-cpu".into(), detail: detail.into() }
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
        || !is_safe_file_name(&target.load_identity)
        || target.system_dependencies.is_empty()
        || target.system_dependencies.iter().any(String::is_empty)
        || !dependencies_are_system_only(target_name, &target.system_dependencies)
        || !is_safe_relative(Path::new(&target.library))
    {
        return Err(LoadError::VersionMismatch);
    }
    Ok(())
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

fn dependencies_are_system_only(target_name: &str, dependencies: &[String]) -> bool {
    let mut unique = std::collections::BTreeSet::new();
    dependencies.iter().all(|dependency| {
        let normalized = if target_name == "x86_64-pc-windows-msvc" {
            dependency.to_ascii_lowercase()
        } else {
            dependency.clone()
        };
        unique.insert(normalized)
            && if target_name == "aarch64-apple-darwin" {
                let path = Path::new(dependency);
                path.is_absolute()
                    && (path.starts_with("/System/Library") || path.starts_with("/usr/lib"))
                    && path.components().all(|component| {
                        matches!(component, Component::RootDir | Component::Normal(_))
                    })
            } else {
                is_safe_file_name(dependency)
            }
    })
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

fn audit_loaded_modules(
    main_file: &std::ffi::OsStr,
    load_identity: &str,
    dependencies: &[String],
    trusted_main: Option<&Path>,
) -> Result<(), LoadError> {
    #[cfg(target_os = "linux")]
    return audit_loaded_linux(main_file, load_identity, dependencies, trusted_main);
    #[cfg(target_os = "macos")]
    return audit_loaded_macos(main_file, load_identity, dependencies, trusted_main);
    #[cfg(windows)]
    return audit_loaded_windows(main_file, load_identity, dependencies, trusted_main);
    #[allow(unreachable_code)]
    Err(LoadError::UnsupportedTarget)
}

#[cfg(target_os = "linux")]
fn audit_loaded_linux(
    main_file: &std::ffi::OsStr,
    load_identity: &str,
    dependencies: &[String],
    trusted_main: Option<&Path>,
) -> Result<(), LoadError> {
    struct Audit<'a> {
        main_file: &'a std::ffi::OsStr,
        load_identity: &'a str,
        dependencies: &'a [String],
        trusted_main: Option<&'a Path>,
        rejected: bool,
    }
    unsafe extern "C" fn inspect(
        information: *mut libc::dl_phdr_info,
        _size: usize,
        opaque: *mut std::ffi::c_void,
    ) -> std::ffi::c_int {
        // SAFETY: `dl_iterate_phdr` calls synchronously with the exact context
        // pointer and a live `dl_phdr_info` for the duration of this callback.
        let (information, audit) = unsafe { (&*information, &mut *opaque.cast::<Audit<'_>>()) };
        if information.dlpi_name.is_null() {
            return 0;
        }
        // SAFETY: the platform loader supplies a NUL-terminated image name.
        let path = unsafe { CStr::from_ptr(information.dlpi_name) };
        let path = Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
        let Some(name) = path.file_name() else {
            return 0;
        };
        if name == audit.main_file || name == std::ffi::OsStr::new(audit.load_identity) {
            if audit.trusted_main != Some(path) {
                audit.rejected = true;
                return 1;
            }
            return 0;
        }
        let is_dependency = audit
            .dependencies
            .iter()
            .any(|dependency| Path::new(dependency).file_name() == Some(name));
        if is_dependency
            && (!path.is_absolute()
                || !(path.starts_with("/lib")
                    || path.starts_with("/lib64")
                    || path.starts_with("/usr/lib")
                    || path.starts_with("/usr/lib64")))
        {
            audit.rejected = true;
            return 1;
        }
        0
    }
    use std::os::unix::ffi::OsStrExt;
    let mut audit = Audit { main_file, load_identity, dependencies, trusted_main, rejected: false };
    // SAFETY: the callback and opaque context remain valid for this synchronous
    // enumeration and do not escape it.
    unsafe { libc::dl_iterate_phdr(Some(inspect), (&raw mut audit).cast()) };
    if audit.rejected { Err(LoadError::DependencyMismatch) } else { Ok(()) }
}

#[cfg(target_os = "macos")]
fn audit_loaded_macos(
    main_file: &std::ffi::OsStr,
    load_identity: &str,
    dependencies: &[String],
    trusted_main: Option<&Path>,
) -> Result<(), LoadError> {
    unsafe extern "C" {
        fn _dyld_image_count() -> u32;
        fn _dyld_get_image_name(index: u32) -> *const std::ffi::c_char;
    }
    // SAFETY: dyld image enumeration returns process-lifetime NUL-terminated
    // names for indices below the captured image count.
    let count = unsafe { _dyld_image_count() };
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
        let Some(name) = path.file_name() else {
            continue;
        };
        if name == main_file || name == std::ffi::OsStr::new(load_identity) {
            if trusted_main != Some(path) {
                return Err(LoadError::DependencyMismatch);
            }
            continue;
        }
        for dependency in dependencies {
            let expected = Path::new(dependency);
            if expected.file_name() == Some(name) && path != expected {
                return Err(LoadError::DependencyMismatch);
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn audit_loaded_windows(
    main_file: &std::ffi::OsStr,
    load_identity: &str,
    dependencies: &[String],
    trusted_main: Option<&Path>,
) -> Result<(), LoadError> {
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
    let main = main_file.to_string_lossy();
    let names = std::iter::once(main.as_ref())
        .chain(std::iter::once(load_identity))
        .chain(dependencies.iter().map(String::as_str));
    for name in names {
        let wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL terminated and retained through the call.
        let module = unsafe { GetModuleHandleW(wide.as_ptr()) };
        if module.is_null() {
            continue;
        }
        let is_main =
            name.eq_ignore_ascii_case(main.as_ref()) || name.eq_ignore_ascii_case(load_identity);
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

fn copy_and_hash(source: &mut File, destination: &mut File) -> Result<String, LoadError> {
    source.seek(SeekFrom::Start(0)).map_err(|_| LoadError::Io)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(|_| LoadError::Io)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(|_| LoadError::Io)?;
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
    use into_markdown_ocr::{ModelContract, ModelIdentity};

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
                outputs: vec![tensor("y")],
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
            &["/usr/lib/libSystem.B.dylib".into()]
        ));
        assert!(!dependencies_are_system_only(
            "aarch64-apple-darwin",
            &["@rpath/libc++.dylib".into()]
        ));
        assert!(dependencies_are_system_only("x86_64-unknown-linux-gnu", &["libc.so.6".into()]));
        assert!(!dependencies_are_system_only(
            "x86_64-unknown-linux-gnu",
            &["../libc.so.6".into()]
        ));
        assert!(!dependencies_are_system_only(
            "x86_64-pc-windows-msvc",
            &["KERNEL32.dll".into(), "kernel32.DLL".into()]
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
    fn environment_policy_explicitly_disables_telemetry() {
        let observed = std::cell::Cell::new(true);
        assert!(commit_environment_policy(|telemetry| {
            observed.set(telemetry);
            true
        }));
        assert!(!observed.get());
    }

    #[test]
    fn explicit_native_runtime_matches_hash_version_and_api() {
        let Some(repository) = option_env!("ORT_TEST_REPOSITORY") else {
            return;
        };
        let Some(library) = option_env!("ORT_TEST_LIBRARY") else {
            panic!("ORT_TEST_LIBRARY must accompany ORT_TEST_REPOSITORY");
        };
        assert_ne!(repository, "unsupported");
        if std::env::var_os("ORT_NATIVE_CHILD").is_none() {
            let executable = std::env::current_exe().unwrap();
            for mode in ["normal", "preinitialized"] {
                let mut child = std::process::Command::new(&executable);
                child
                    .arg("--exact")
                    .arg("tests::explicit_native_runtime_matches_hash_version_and_api")
                    .arg("--nocapture")
                    .env("ORT_NATIVE_CHILD", mode);
                for variable in [
                    "LD_PRELOAD",
                    "LD_LIBRARY_PATH",
                    "LD_AUDIT",
                    "DYLD_LIBRARY_PATH",
                    "DYLD_FRAMEWORK_PATH",
                    "DYLD_FALLBACK_LIBRARY_PATH",
                    "DYLD_INSERT_LIBRARIES",
                ] {
                    child.env_remove(variable);
                }
                assert!(child.status().unwrap().success());
            }
            return;
        }
        let runfiles = PathBuf::from(std::env::var_os("TEST_SRCDIR").unwrap());
        let runfile = runfiles.join(repository).join(library);
        let canonical_library = runfile.canonicalize().unwrap();
        let component_count = Path::new(library).components().count();
        let trusted_root =
            canonical_library.ancestors().nth(component_count).unwrap().to_path_buf();
        let loaded = Arc::new(RuntimeLibrary::load(&trusted_root, &canonical_library).unwrap());
        let authority = authority().unwrap();
        assert_eq!(loaded.version(), authority.version);
        assert_eq!(loaded.api_version(), authority.api_version);
        if std::env::var_os("ORT_NATIVE_CHILD").as_deref()
            == Some(std::ffi::OsStr::new("preinitialized"))
        {
            assert!(ort::init().with_telemetry(true).commit());
            assert_eq!(OrtSessionFactory::new(loaded).unwrap_err(), LoadError::GlobalStateMismatch);
            return;
        }
        let factory = OrtSessionFactory::new(Arc::clone(&loaded)).unwrap();
        let environment = ort::environment::Environment::current().unwrap();
        let builder = ort::session::Session::builder().unwrap();
        drop(builder);
        drop(factory);
        drop(environment);
        let second = OrtSessionFactory::new(loaded).unwrap();
        let second_environment = ort::environment::Environment::current().unwrap();
        let model = tiny_identity_model();
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let session = second.create(&model, &SessionOptions::default(), &context).unwrap();
        let outputs =
            session.run(&[Tensor { shape: vec![1], values: vec![3.5] }], &context).unwrap();
        assert_eq!(outputs, [Tensor { shape: vec![1], values: vec![3.5] }]);
        drop(session);
        drop(second_environment);
        drop(second);
    }
}
