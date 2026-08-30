//! Lazily materialized native runtimes embedded in the final `into-md` executable.

use fd_lock::RwLock;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const COMPLETE_MARKER: &str = ".complete";
const MAX_CATALOG_BYTES: u64 = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static MATERIALIZE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TEMPORARY_RUNTIME_ROOTS: std::sync::Mutex<BTreeSet<PathBuf>> =
    std::sync::Mutex::new(BTreeSet::new());

#[derive(Debug)]
pub(super) struct EmbeddedFile {
    path: &'static str,
    bytes: u64,
    sha256: &'static str,
    executable: bool,
}

#[cfg(feature = "embedded-runtime")]
include!(concat!(env!("OUT_DIR"), "/embedded_runtime_payloads.rs"));

#[cfg(not(feature = "embedded-runtime"))]
const EMBEDDED_RUNTIME_ENABLED: bool = false;
#[cfg(not(feature = "embedded-runtime"))]
static PDFIUM_ARCHIVE: &[u8] = &[];
#[cfg(not(feature = "embedded-runtime"))]
const PDFIUM_ARCHIVE_SHA256: &str = "";
#[cfg(not(feature = "embedded-runtime"))]
static PDFIUM_FILES: &[EmbeddedFile] = &[];
#[cfg(not(feature = "embedded-runtime"))]
static OCR_ARCHIVE: &[u8] = &[];
#[cfg(not(feature = "embedded-runtime"))]
const OCR_ARCHIVE_SHA256: &str = "";
#[cfg(not(feature = "embedded-runtime"))]
static OCR_FILES: &[EmbeddedFile] = &[];

pub(super) const fn enabled() -> bool {
    EMBEDDED_RUNTIME_ENABLED
}

#[derive(Clone, Copy)]
struct Payload {
    label: &'static str,
    archive: &'static [u8],
    archive_sha256: &'static str,
    files: &'static [EmbeddedFile],
}

const fn pdfium_payload() -> Payload {
    Payload {
        label: "pdfium",
        archive: PDFIUM_ARCHIVE,
        archive_sha256: PDFIUM_ARCHIVE_SHA256,
        files: PDFIUM_FILES,
    }
}

const fn ocr_payload() -> Payload {
    Payload {
        label: "ocr",
        archive: OCR_ARCHIVE,
        archive_sha256: OCR_ARCHIVE_SHA256,
        files: OCR_FILES,
    }
}

pub(super) fn register_pdfium_resolver() {
    if should_register_pdfium_resolver() {
        let _ = into_markdown::install_pdfium_runtime_resolver(resolve_pdfium_library);
    }
}

const fn should_register_pdfium_resolver() -> bool {
    EMBEDDED_RUNTIME_ENABLED && !cfg!(windows)
}

/// Remove process-private runtime fallbacks after all conversions and Web tasks have stopped.
///
/// The normal hash-addressed user cache is durable and is deliberately not registered here.
/// Only roots created because that cache was unavailable or unsafe are removed.
pub(super) fn release_temporary_runtimes() {
    let roots = match TEMPORARY_RUNTIME_ROOTS.lock() {
        Ok(mut roots) => std::mem::take(&mut *roots),
        Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
    };
    for root in roots {
        let _ = remove_verified_physical_tree(&root);
    }
}

pub(super) fn resolve_pdfium_library() -> Result<PathBuf, into_markdown::ConversionError> {
    let root = materialize(pdfium_payload()).map_err(runtime_error)?;
    #[cfg(target_os = "windows")]
    let relative = "lib/pdfium/pdfium.dll";
    #[cfg(target_os = "linux")]
    let relative = "lib/pdfium/libpdfium.so";
    #[cfg(target_os = "macos")]
    let relative = "lib/pdfium/libpdfium.dylib";
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    return Err(runtime_error("PDFium is unsupported on this platform".into()));
    Ok(root.join(relative))
}

pub(super) fn embedded_ocr_engine(
    options: &into_markdown::ConversionOptions,
) -> Result<std::sync::Arc<dyn into_markdown::OcrEngine>, into_markdown::ConversionError> {
    if !EMBEDDED_RUNTIME_ENABLED {
        return Err(runtime_error("embedded OCR is unavailable in this development build".into()));
    }
    Ok(std::sync::Arc::new(LazyEmbeddedOcr {
        options: options.clone(),
        engine: std::sync::Mutex::new(None),
    }))
}

/// Materialize and authenticate the complete embedded OCR process runtime.
///
/// Unlike [`embedded_ocr_engine`], this is intentionally eager: explicit
/// setup/verify/repair commands must not report success for a corrupt cache or
/// an unavailable platform sandbox merely because the conversion engine is
/// lazily initialized.
pub(super) fn verify_embedded_ocr_runtime(
    options: &into_markdown::ConversionOptions,
) -> Result<(), into_markdown::ConversionError> {
    build_embedded_ocr_engine(options).map(drop)
}

struct LazyEmbeddedOcr {
    options: into_markdown::ConversionOptions,
    engine: std::sync::Mutex<Option<std::sync::Arc<dyn into_markdown::OcrEngine>>>,
}

impl LazyEmbeddedOcr {
    fn get(
        &self,
    ) -> Result<std::sync::Arc<dyn into_markdown::OcrEngine>, into_markdown::ConversionError> {
        let mut slot = self.engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(engine) = slot.as_ref() {
            return Ok(std::sync::Arc::clone(engine));
        }
        let engine = build_embedded_ocr_engine(&self.options)?;
        *slot = Some(std::sync::Arc::clone(&engine));
        Ok(engine)
    }
}

impl into_markdown::OcrEngine for LazyEmbeddedOcr {
    fn id(&self) -> &'static str {
        "builtin.ocr.ppocrv6-image"
    }

    fn planned_bound_output(
        &self,
        request: into_markdown::OcrRequest<'_>,
        options: &into_markdown::ConversionOptions,
        context: &into_markdown::ExecutionContext,
    ) -> Result<into_markdown::OcrOutputPlan, into_markdown::ConversionError> {
        self.get()?.planned_bound_output(request, options, context)
    }

    fn planned_normalized_png_output(
        &self,
        width: u32,
        height: u32,
        options: &into_markdown::ConversionOptions,
        context: &into_markdown::ExecutionContext,
    ) -> Result<into_markdown::OcrOutputPlan, into_markdown::ConversionError> {
        self.get()?.planned_normalized_png_output(width, height, options, context)
    }

    fn recognize<'a>(
        &'a self,
        request: into_markdown::OcrRequest<'a>,
        context: &'a into_markdown::ExecutionContext,
    ) -> into_markdown::BoxFuture<
        'a,
        Result<into_markdown::OcrResult, into_markdown::ConversionError>,
    > {
        Box::pin(async move { self.get()?.recognize(request, context).await })
    }

    fn recognize_bound<'a>(
        &'a self,
        request: into_markdown::OcrRequest<'a>,
        context: &'a into_markdown::ExecutionContext,
    ) -> into_markdown::BoxFuture<
        'a,
        Result<into_markdown::OcrRecognition, into_markdown::ConversionError>,
    > {
        Box::pin(async move { self.get()?.recognize_bound(request, context).await })
    }
}

fn build_embedded_ocr_engine(
    options: &into_markdown::ConversionOptions,
) -> Result<std::sync::Arc<dyn into_markdown::OcrEngine>, into_markdown::ConversionError> {
    let root = materialize(ocr_payload()).map_err(runtime_error)?;
    let descriptor = read_verified_file(&root.join("provider.json"), MAX_CATALOG_BYTES * 512)
        .map_err(runtime_error)?;
    let manifest: into_markdown_provider_plugin::PluginManifest =
        serde_json::from_slice(&descriptor)
            .map_err(|_| runtime_error("embedded OCR provider descriptor is invalid".into()))?;
    manifest.validate().map_err(|detail| {
        runtime_error(format!("embedded OCR provider descriptor is invalid: {detail}"))
    })?;
    if manifest.id != "official.ocr.ppocrv6" {
        return Err(runtime_error("embedded OCR provider identity is invalid".into()));
    }
    let target = manifest
        .target(into_markdown_provider_plugin::current_target())
        .ok_or_else(|| runtime_error("embedded OCR has no runtime for this platform".into()))?;
    let entry = target
        .files
        .iter()
        .find(|file| file.path == target.entrypoint)
        .ok_or_else(|| runtime_error("embedded OCR entrypoint is not authenticated".into()))?;
    let capability = manifest
        .capabilities
        .iter()
        .find(|capability| {
            capability.id == "ocr"
                && capability.kind == into_markdown_provider_plugin::CapabilityKind::Ocr
        })
        .ok_or_else(|| runtime_error("embedded OCR capability is absent".into()))?;
    let binding = into_markdown_provider_plugin::ProviderBinding {
        plugin_id: manifest.id.clone(),
        plugin_version: manifest.version.clone(),
        manifest_sha256: format!("{:x}", Sha256::digest(&descriptor)),
        capability_id: capability.id.clone(),
        provider_id: capability.provider_id.clone(),
        install_root: root.clone(),
    };
    #[allow(unused_mut)]
    let mut policy =
        into_markdown_provider_plugin::ProcessCapability::runtime_policy(&manifest, &binding)?;
    #[cfg(windows)]
    {
        let authority = into_markdown_process_plugin::provision_windows_sandbox(
            "into-markdown:embedded:official.ocr.ppocrv6",
        )
        .map_err(|error| runtime_error(format!("prepare embedded OCR AppContainer: {error}")))?;
        into_markdown_process_plugin::authorize_windows_sandbox_path(&authority, &root)
            .and_then(|()| {
                into_markdown_process_plugin::verify_windows_sandbox_path(&authority, &root)
            })
            .map_err(|error| runtime_error(format!("authorize embedded OCR runtime: {error}")))?;
        policy.windows = authority;
    }
    let process =
        into_markdown_process_plugin::ProcessPlugin::from_authenticated_read_only_runtime(
            into_markdown_process_plugin::PluginManifest {
                plugin_id: manifest.id.clone(),
                executable: root.join(&target.entrypoint),
                runtime_root: root,
                executable_sha256: entry.sha256.clone(),
                protocol_versions: vec![1],
            },
            policy,
        )
        .map_err(|error| {
            runtime_error(format!("embedded OCR process authority failed: {error}"))
        })?;
    into_markdown_provider_plugin::ProcessCapability::new_embedded(process, &manifest, binding)?
        .ocr(options.clone())
        .map(|engine| std::sync::Arc::new(engine) as std::sync::Arc<dyn into_markdown::OcrEngine>)
}

/// Return the embedded official publisher catalog without materializing OCR.
pub(super) fn official_publisher_catalog() -> Result<Option<Vec<u8>>, String> {
    if !EMBEDDED_RUNTIME_ENABLED {
        return Ok(None);
    }
    read_archive_member(ocr_payload(), "official-publisher.json", MAX_CATALOG_BYTES).map(Some)
}

fn runtime_error(detail: String) -> into_markdown::ConversionError {
    into_markdown::ConversionError::ComponentUnavailable {
        component: "embedded-runtime".into(),
        detail,
    }
}

fn materialize(payload: Payload) -> Result<PathBuf, String> {
    if !EMBEDDED_RUNTIME_ENABLED || payload.archive.is_empty() {
        return Err(format!("embedded {} payload is unavailable", payload.label));
    }
    if let Ok(base) = cache_runtime_root()
        && let Ok(path) = materialize_at(&base, payload)
    {
        return Ok(path);
    }
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let fallback = std::env::temp_dir()
        .join(format!("into-markdown-runtime-{}-{sequence}", std::process::id()));
    create_private_directory(&fallback)?;
    TEMPORARY_RUNTIME_ROOTS
        .lock()
        .map_err(|_| format!("track temporary {} runtime", payload.label))?
        .insert(fallback.clone());
    materialize_at(&fallback, payload)
}

fn cache_runtime_root() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let cache = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA is unavailable".to_owned())?;
    #[cfg(target_os = "macos")]
    let cache = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is unavailable".to_owned())?
        .join("Library/Caches");
    #[cfg(all(unix, not(target_os = "macos")))]
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| "user cache directory is unavailable".to_owned())?;
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    return Err("user cache directory is unsupported".into());
    Ok(cache.join("into-markdown/runtime"))
}

fn materialize_at(base: &Path, payload: Payload) -> Result<PathBuf, String> {
    // Some Unix file-lock implementations are process-scoped, so two threads in
    // this process must not contend on separate descriptors for the same lock.
    // The on-disk lock below remains the authority between different processes.
    let _process_guard = MATERIALIZE_LOCK
        .lock()
        .map_err(|_| format!("lock {} runtime materialization in this process", payload.label))?;
    reject_existing_ancestor_links(base)?;
    create_private_directory(base)?;
    reject_link(base)?;
    let lock_path = base.join(format!(".{}.lock", payload.archive_sha256));
    reject_link_if_present(&lock_path)?;
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("open {} runtime lock: {error}", payload.label))?;
    private_file_permissions(&lock_path, false)?;
    let mut lock = RwLock::new(lock_file);
    let _guard =
        lock.write().map_err(|error| format!("lock {} runtime: {error}", payload.label))?;
    let destination = base.join(payload.archive_sha256);
    match verify_tree(&destination, payload) {
        Ok(()) => return Ok(destination),
        Err(TreeState::Unsafe(detail)) => return Err(detail),
        Err(TreeState::MissingOrCorrupt) => {}
    }
    if fs::symlink_metadata(&destination).is_ok() {
        remove_verified_physical_tree(&destination)?;
    }
    remove_stale_staging_directories(base, payload.archive_sha256)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let staging =
        base.join(format!(".{}.tmp-{}-{sequence}", payload.archive_sha256, std::process::id()));
    if fs::symlink_metadata(&staging).is_ok() {
        remove_verified_physical_tree(&staging)?;
    }
    create_private_directory(&staging)?;
    if let Err(error) = extract_payload(&staging, payload) {
        let _ = remove_verified_physical_tree(&staging);
        return Err(error);
    }
    let marker_path = staging.join(COMPLETE_MARKER);
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_path)
        .map_err(|error| format!("create {} completion marker: {error}", payload.label))?;
    marker
        .write_all(payload.archive_sha256.as_bytes())
        .and_then(|()| marker.sync_all())
        .map_err(|error| format!("write {} completion marker: {error}", payload.label))?;
    drop(marker);
    private_file_permissions(&marker_path, false)?;
    sync_tree(&staging, payload)?;
    match fs::rename(&staging, &destination) {
        Ok(()) => {}
        Err(_) if verify_tree(&destination, payload).is_ok() => {
            let _ = remove_verified_physical_tree(&staging);
        }
        Err(error) => {
            let _ = remove_verified_physical_tree(&staging);
            return Err(format!("publish {} runtime atomically: {error}", payload.label));
        }
    }
    verify_tree(&destination, payload).map_err(|state| match state {
        TreeState::MissingOrCorrupt => {
            format!("published {} runtime failed verification", payload.label)
        }
        TreeState::Unsafe(detail) => detail,
    })?;
    Ok(destination)
}

fn remove_stale_staging_directories(base: &Path, archive_sha256: &str) -> Result<(), String> {
    let prefix = format!(".{archive_sha256}.tmp-");
    for entry in fs::read_dir(base).map_err(|error| format!("enumerate runtime cache: {error}"))? {
        let path = entry.map_err(|error| format!("enumerate runtime cache entry: {error}"))?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with(&prefix) {
            remove_verified_physical_tree(&path)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
enum TreeState {
    MissingOrCorrupt,
    Unsafe(String),
}

fn verify_tree(root: &Path, payload: Payload) -> Result<(), TreeState> {
    let root_metadata = fs::symlink_metadata(root).map_err(|_| TreeState::MissingOrCorrupt)?;
    if !root_metadata.is_dir() || is_reparse_or_link(&root_metadata) {
        return Err(TreeState::Unsafe(format!(
            "{} runtime cache is a link or non-directory",
            payload.label
        )));
    }
    require_private_metadata(&root_metadata, 0o700, "runtime root")?;
    let marker_path = root.join(COMPLETE_MARKER);
    let marker_metadata =
        fs::symlink_metadata(&marker_path).map_err(|_| TreeState::MissingOrCorrupt)?;
    if !marker_metadata.is_file() || is_reparse_or_link(&marker_metadata) {
        return Err(TreeState::Unsafe(format!(
            "{} runtime completion marker is unsafe",
            payload.label
        )));
    }
    require_private_metadata(&marker_metadata, 0o600, "runtime completion marker")?;
    let marker = read_verified_file(&marker_path, 64).map_err(TreeState::Unsafe)?;
    if marker != payload.archive_sha256.as_bytes() {
        return Err(TreeState::MissingOrCorrupt);
    }
    let expected = payload.files.iter().map(|file| file.path.to_owned()).collect::<BTreeSet<_>>();
    let actual = list_physical_files(root).map_err(TreeState::Unsafe)?;
    if actual != expected {
        return Err(TreeState::MissingOrCorrupt);
    }
    for expected in payload.files {
        let path = root.join(expected.path);
        let metadata = fs::symlink_metadata(&path).map_err(|_| TreeState::MissingOrCorrupt)?;
        if !metadata.is_file() || is_reparse_or_link(&metadata) {
            return Err(TreeState::Unsafe(format!(
                "{} runtime contains an unsafe file",
                payload.label
            )));
        }
        require_private_metadata(
            &metadata,
            if expected.executable { 0o700 } else { 0o600 },
            "runtime file",
        )?;
        if metadata.len() != expected.bytes {
            return Err(TreeState::MissingOrCorrupt);
        }
        let bytes =
            read_verified_file(&path, expected.bytes).map_err(|_| TreeState::MissingOrCorrupt)?;
        if format!("{:x}", Sha256::digest(bytes)) != expected.sha256 {
            return Err(TreeState::MissingOrCorrupt);
        }
    }
    Ok(())
}

fn require_private_metadata(
    metadata: &fs::Metadata,
    expected_mode: u32,
    label: &str,
) -> Result<(), TreeState> {
    if private_metadata_is_valid(metadata, expected_mode) {
        return Ok(());
    }
    Err(TreeState::Unsafe(format!("{label} owner or permissions are unsafe")))
}

#[cfg(unix)]
fn private_metadata_is_valid(metadata: &fs::Metadata, expected_mode: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & 0o7777 == expected_mode
}

#[cfg(not(unix))]
fn private_metadata_is_valid(_metadata: &fs::Metadata, _expected_mode: u32) -> bool {
    true
}

fn list_physical_files(root: &Path) -> Result<BTreeSet<String>, String> {
    fn walk(root: &Path, directory: &Path, result: &mut BTreeSet<String>) -> Result<(), String> {
        for entry in
            fs::read_dir(directory).map_err(|error| format!("enumerate runtime cache: {error}"))?
        {
            let path = entry.map_err(|error| format!("enumerate runtime entry: {error}"))?.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("inspect runtime entry: {error}"))?;
            if is_reparse_or_link(&metadata) {
                return Err("runtime cache contains a link or reparse point".into());
            }
            if metadata.is_dir() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;
                    if metadata.uid() != rustix::process::geteuid().as_raw()
                        || metadata.mode() & 0o7777 != 0o700
                    {
                        return Err(
                            "runtime cache directory owner or permissions are unsafe".into()
                        );
                    }
                }
                walk(root, &path, result)?;
            } else if metadata.is_file() {
                let relative =
                    path.strip_prefix(root).map_err(|_| "runtime path escaped cache".to_owned())?;
                let name = relative.to_string_lossy().replace('\\', "/");
                if name != COMPLETE_MARKER {
                    result.insert(name);
                }
            } else {
                return Err("runtime cache contains a non-file entry".into());
            }
        }
        Ok(())
    }
    let mut owned = BTreeSet::new();
    walk(root, root, &mut owned)?;
    Ok(owned)
}

fn extract_payload(root: &Path, payload: Payload) -> Result<(), String> {
    if format!("{:x}", Sha256::digest(payload.archive)) != payload.archive_sha256 {
        return Err(format!("embedded {} archive digest mismatch", payload.label));
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(payload.archive))
        .map_err(|error| format!("open embedded {} archive: {error}", payload.label))?;
    if archive.len() != payload.files.len() {
        return Err(format!("embedded {} archive inventory differs", payload.label));
    }
    for (index, expected) in payload.files.iter().enumerate() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("read embedded {} member: {error}", payload.label))?;
        if entry.name() != expected.path || !entry.is_file() || entry.size() != expected.bytes {
            return Err(format!("embedded {} member authority differs", payload.label));
        }
        let relative = safe_relative(expected.path)?;
        let destination = root.join(&relative);
        if let Some(parent) = destination.parent() {
            create_private_directory(parent)?;
        }
        reject_link_if_present(&destination)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|error| format!("create embedded {} member: {error}", payload.label))?;
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        loop {
            let read = entry.read(&mut buffer).map_err(|error| {
                format!("decompress embedded {} member: {error}", payload.label)
            })?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .ok_or_else(|| "runtime member length overflow".to_owned())?;
            if total > expected.bytes {
                return Err(format!("embedded {} member exceeds authority", payload.label));
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("write embedded {} member: {error}", payload.label))?;
            hasher.update(&buffer[..read]);
        }
        if total != expected.bytes || format!("{:x}", hasher.finalize()) != expected.sha256 {
            return Err(format!("embedded {} member digest mismatch", payload.label));
        }
        output
            .sync_all()
            .map_err(|error| format!("sync embedded {} member: {error}", payload.label))?;
        drop(output);
        private_file_permissions(&destination, expected.executable)?;
    }
    Ok(())
}

fn read_archive_member(payload: Payload, name: &str, maximum: u64) -> Result<Vec<u8>, String> {
    if format!("{:x}", Sha256::digest(payload.archive)) != payload.archive_sha256 {
        return Err(format!("embedded {} archive digest mismatch", payload.label));
    }
    let expected = payload
        .files
        .iter()
        .find(|file| file.path == name)
        .ok_or_else(|| format!("embedded {} member is absent", payload.label))?;
    if expected.bytes > maximum {
        return Err(format!("embedded {} member exceeds its bound", payload.label));
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(payload.archive))
        .map_err(|error| format!("open embedded {} archive: {error}", payload.label))?;
    let mut entry = archive
        .by_name(name)
        .map_err(|error| format!("read embedded {} member: {error}", payload.label))?;
    if !entry.is_file() || entry.size() != expected.bytes {
        return Err(format!("embedded {} member authority differs", payload.label));
    }
    let mut bytes = Vec::new();
    let capacity = usize::try_from(expected.bytes)
        .map_err(|_| "embedded member size does not fit this platform".to_owned())?;
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| "embedded member allocation failed".to_owned())?;
    entry
        .read_to_end(&mut bytes)
        .map_err(|error| format!("decompress embedded member: {error}"))?;
    if bytes.len() as u64 != expected.bytes
        || format!("{:x}", Sha256::digest(&bytes)) != expected.sha256
    {
        return Err(format!("embedded {} member digest mismatch", payload.label));
    }
    Ok(bytes)
}

fn safe_relative(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.contains('\\')
        || path.is_absolute()
        || !path.components().all(|part| matches!(part, Component::Normal(_)))
    {
        return Err("embedded runtime path is unsafe".into());
    }
    Ok(path.to_owned())
}

fn read_verified_file(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    reject_link(path)?;
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("inspect runtime file: {error}"))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err("runtime file is not a bounded regular file".into());
    }
    let mut file = File::open(path).map_err(|error| format!("open runtime file: {error}"))?;
    let mut bytes = Vec::new();
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| "runtime file size does not fit this platform".to_owned())?;
    bytes.try_reserve_exact(capacity).map_err(|_| "runtime file allocation failed".to_owned())?;
    file.read_to_end(&mut bytes).map_err(|error| format!("read runtime file: {error}"))?;
    if bytes.len() as u64 != metadata.len() {
        return Err("runtime file changed while reading".into());
    }
    Ok(bytes)
}

#[cfg(windows)]
fn create_private_directory(path: &Path) -> Result<(), String> {
    reject_existing_ancestor_links(path)?;
    if fs::symlink_metadata(path).is_err() {
        let mut missing = Vec::new();
        let mut cursor = path;
        while fs::symlink_metadata(cursor).is_err() {
            missing.push(cursor.to_owned());
            cursor = cursor
                .parent()
                .ok_or_else(|| "private runtime directory has no trusted parent".to_owned())?;
        }
        for directory in missing.into_iter().rev() {
            if let Err(error) =
                into_markdown_process_plugin::create_windows_plugin_store_directory(&directory)
            {
                let concurrently_created = fs::symlink_metadata(&directory)
                    .is_ok_and(|metadata| metadata.is_dir() && !is_reparse_or_link(&metadata));
                if !concurrently_created {
                    return Err(format!("create private runtime directory: {error}"));
                }
            }
        }
    }
    into_markdown_process_plugin::verify_windows_plugin_store_path(path)
        .map_err(|error| format!("verify private runtime directory: {error}"))
}

#[cfg(not(windows))]
fn create_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    reject_existing_ancestor_links(path)?;
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if !metadata.is_dir() || is_reparse_or_link(&metadata) {
                    return Err("private runtime path is a link or non-directory".into());
                }
                if cursor == path
                    && (metadata.uid() != rustix::process::geteuid().as_raw()
                        || metadata.mode() & 0o7777 != 0o700)
                {
                    return Err("private runtime directory owner or permissions are unsafe".into());
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(cursor.to_owned());
                cursor = cursor
                    .parent()
                    .ok_or_else(|| "private runtime directory has no trusted parent".to_owned())?;
            }
            Err(error) => return Err(format!("inspect private runtime directory: {error}")),
        }
    }
    for directory in missing.into_iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!("create private runtime directory: {error}"));
            }
        }
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|error| format!("inspect created runtime directory: {error}"))?;
        if !metadata.is_dir()
            || is_reparse_or_link(&metadata)
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err("created runtime directory owner or permissions are unsafe".into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn private_file_permissions(path: &Path, executable: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if executable { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("protect runtime file: {error}"))
}

#[cfg(windows)]
fn private_file_permissions(path: &Path, executable: bool) -> Result<(), String> {
    let _ = executable;
    into_markdown_process_plugin::verify_windows_plugin_store_child(path)
        .map_err(|error| format!("verify private runtime file: {error}"))
}

fn reject_link_if_present(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(_) => reject_link(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect runtime path: {error}")),
    }
}

fn reject_existing_ancestor_links(path: &Path) -> Result<(), String> {
    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        match fs::symlink_metadata(candidate) {
            Ok(metadata)
                if is_reparse_or_link(&metadata) && !is_trusted_macos_system_alias(candidate) =>
            {
                return Err("runtime path contains a link or reparse point".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect runtime path ancestor: {error}")),
        }
        cursor = candidate.parent();
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_trusted_macos_system_alias(path: &Path) -> bool {
    // macOS exposes /var as a root-owned compatibility symlink to /private/var.
    // Temporary directories returned by the OS therefore necessarily traverse it.
    path == Path::new("/var")
        && fs::canonicalize(path).is_ok_and(|target| target == Path::new("/private/var"))
}

#[cfg(not(target_os = "macos"))]
fn is_trusted_macos_system_alias(_path: &Path) -> bool {
    false
}

fn reject_link(path: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("inspect runtime path: {error}"))?;
    if is_reparse_or_link(&metadata) {
        return Err("runtime path contains a link or reparse point".into());
    }
    Ok(())
}

fn is_reparse_or_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    false
}

fn remove_verified_physical_tree(path: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("inspect stale runtime: {error}"))?;
    if !metadata.is_dir() || is_reparse_or_link(&metadata) {
        return Err("refusing to remove unsafe runtime cache path".into());
    }
    check_physical_tree(path)?;
    fs::remove_dir_all(path).map_err(|error| format!("remove stale runtime: {error}"))
}

fn check_physical_tree(directory: &Path) -> Result<(), String> {
    for entry in
        fs::read_dir(directory).map_err(|error| format!("enumerate stale runtime: {error}"))?
    {
        let path = entry.map_err(|error| format!("enumerate stale entry: {error}"))?.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| format!("inspect stale entry: {error}"))?;
        if is_reparse_or_link(&metadata) {
            return Err("refusing to remove runtime tree containing a link".into());
        }
        if metadata.is_dir() {
            check_physical_tree(&path)?;
        } else if !metadata.is_file() {
            return Err("refusing to remove non-file runtime entry".into());
        }
    }
    Ok(())
}

fn sync_tree(root: &Path, payload: Payload) -> Result<(), String> {
    #[cfg(unix)]
    File::open(root)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| format!("sync {} runtime directory: {error}", payload.label))?;
    let _ = (root, payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_archive(files: &[(&str, &[u8], bool)]) -> (Vec<u8>, Vec<EmbeddedFile>) {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut archive = zip::ZipWriter::new(&mut cursor);
            for (name, bytes, executable) in files {
                archive.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                archive.write_all(bytes).unwrap();
                let _ = executable;
            }
            archive.finish().unwrap();
        }
        let manifest = files
            .iter()
            .map(|(name, bytes, executable)| EmbeddedFile {
                path: Box::leak((*name).to_owned().into_boxed_str()),
                bytes: bytes.len() as u64,
                sha256: Box::leak(format!("{:x}", Sha256::digest(bytes)).into_boxed_str()),
                executable: *executable,
            })
            .collect();
        (cursor.into_inner(), manifest)
    }

    #[test]
    fn materialization_is_atomic_reusable_and_repairs_corruption() {
        let temporary = tempfile::tempdir().unwrap();
        let (archive, files) =
            test_archive(&[("bin/tool", b"tool", true), ("models/model", b"model", false)]);
        let archive = Box::leak(archive.into_boxed_slice());
        let files = Box::leak(files.into_boxed_slice());
        let archive_sha256 = Box::leak(format!("{:x}", Sha256::digest(&*archive)).into_boxed_str());
        let payload = Payload { label: "test", archive, archive_sha256, files };
        let base = temporary.path().join("runtime");
        let first = materialize_at(&base, payload).unwrap();
        let second = materialize_at(&base, payload).unwrap();
        assert_eq!(first, second);
        fs::write(first.join("models/model"), b"bad").unwrap();
        let repaired = materialize_at(&base, payload).unwrap();
        assert_eq!(fs::read(repaired.join("models/model")).unwrap(), b"model");
    }

    #[test]
    fn archive_member_read_does_not_materialize_payload() {
        let (archive, files) = test_archive(&[("official-publisher.json", b"{}", false)]);
        let archive = Box::leak(archive.into_boxed_slice());
        let files = Box::leak(files.into_boxed_slice());
        let archive_sha256 = Box::leak(format!("{:x}", Sha256::digest(&*archive)).into_boxed_str());
        let payload = Payload { label: "test", archive, archive_sha256, files };
        assert_eq!(read_archive_member(payload, "official-publisher.json", 16).unwrap(), b"{}");
    }

    #[test]
    fn concurrent_first_use_publishes_one_complete_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let (archive, files) =
            test_archive(&[("bin/tool", b"tool", true), ("models/model", b"model", false)]);
        let archive = Box::leak(archive.into_boxed_slice());
        let files = Box::leak(files.into_boxed_slice());
        let archive_sha256 = Box::leak(format!("{:x}", Sha256::digest(&*archive)).into_boxed_str());
        let payload = Payload { label: "test", archive, archive_sha256, files };
        let base = temporary.path().join("runtime");
        let paths = (0..8)
            .map(|_| {
                let base = base.clone();
                std::thread::spawn(move || materialize_at(&base, payload).unwrap())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert!(paths.iter().all(|path| path == &paths[0]));
        verify_tree(&paths[0], payload).unwrap();
    }

    #[test]
    fn interrupted_staging_directory_is_removed_before_publish() {
        let temporary = tempfile::tempdir().unwrap();
        let (archive, files) = test_archive(&[("bin/tool", b"tool", true)]);
        let archive = Box::leak(archive.into_boxed_slice());
        let files = Box::leak(files.into_boxed_slice());
        let archive_sha256 = Box::leak(format!("{:x}", Sha256::digest(&*archive)).into_boxed_str());
        let payload = Payload { label: "test", archive, archive_sha256, files };
        let base = temporary.path().join("runtime");
        create_private_directory(&base).unwrap();
        let stale = base.join(format!(".{archive_sha256}.tmp-crashed"));
        create_private_directory(&stale).unwrap();
        fs::write(stale.join("partial"), b"partial").unwrap();
        private_file_permissions(&stale.join("partial"), false).unwrap();
        let published = materialize_at(&base, payload).unwrap();
        assert!(!stale.exists());
        verify_tree(&published, payload).unwrap();
    }

    #[test]
    fn process_temporary_runtime_roots_are_removed_explicitly() {
        release_temporary_runtimes();
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("into-markdown-runtime-test");
        create_private_directory(&root).unwrap();
        fs::write(root.join("payload"), b"payload").unwrap();
        private_file_permissions(&root.join("payload"), false).unwrap();
        TEMPORARY_RUNTIME_ROOTS.lock().unwrap().insert(root.clone());

        release_temporary_runtimes();

        assert!(!root.exists());
        assert!(TEMPORARY_RUNTIME_ROOTS.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn first_ocr_materialization_under_default_umask_makes_every_directory_private() {
        use std::os::unix::fs::MetadataExt as _;

        const CHILD: &str = "INTO_MD_TEST_UMASK_022_MATERIALIZATION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let current_test = std::thread::current().name().unwrap().to_owned();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(current_test)
                .arg("--nocapture")
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "umask regression subprocess failed");
            return;
        }

        let previous = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
        let temporary = tempfile::tempdir().unwrap();
        let (archive, files) = test_archive(&[
            ("bin/helpers/tool", b"tool", true),
            ("models/detection/model", b"model", false),
        ]);
        let archive = Box::leak(archive.into_boxed_slice());
        let files = Box::leak(files.into_boxed_slice());
        let archive_sha256 = Box::leak(format!("{:x}", Sha256::digest(&*archive)).into_boxed_str());
        let payload = Payload { label: "ocr", archive, archive_sha256, files };
        let base = temporary.path().join("cache/into-markdown/runtime");
        let published = materialize_at(&base, payload).unwrap();
        for directory in [
            temporary.path().join("cache"),
            temporary.path().join("cache/into-markdown"),
            base,
            published.clone(),
            published.join("bin"),
            published.join("bin/helpers"),
            published.join("models"),
            published.join("models/detection"),
        ] {
            let metadata = fs::symlink_metadata(&directory).unwrap();
            assert_eq!(metadata.mode() & 0o7777, 0o700, "{}", directory.display());
        }
        verify_tree(&published, payload).unwrap();
        let _ = rustix::process::umask(previous);
    }

    #[cfg(unix)]
    #[test]
    fn existing_non_private_managed_directory_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let managed = temporary.path().join("runtime");
        fs::create_dir(&managed).unwrap();
        fs::set_permissions(&managed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            create_private_directory(&managed).unwrap_err(),
            "private runtime directory owner or permissions are unsafe"
        );
        assert_eq!(fs::symlink_metadata(&managed).unwrap().permissions().mode() & 0o7777, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn cache_reuse_rejects_files_with_non_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let (archive, files) = test_archive(&[("models/model", b"model", false)]);
        let archive = Box::leak(archive.into_boxed_slice());
        let files = Box::leak(files.into_boxed_slice());
        let archive_sha256 = Box::leak(format!("{:x}", Sha256::digest(&*archive)).into_boxed_str());
        let payload = Payload { label: "test", archive, archive_sha256, files };
        let published = materialize_at(&temporary.path().join("runtime"), payload).unwrap();

        fs::set_permissions(published.join("models/model"), fs::Permissions::from_mode(0o644))
            .unwrap();

        assert!(matches!(verify_tree(&published, payload), Err(TreeState::Unsafe(_))));
    }

    #[cfg(unix)]
    #[test]
    fn cache_reuse_rejects_linked_completion_marker_without_following_it() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let (archive, files) = test_archive(&[("bin/tool", b"tool", true)]);
        let archive = Box::leak(archive.into_boxed_slice());
        let files = Box::leak(files.into_boxed_slice());
        let archive_sha256 = Box::leak(format!("{:x}", Sha256::digest(&*archive)).into_boxed_str());
        let payload = Payload { label: "test", archive, archive_sha256, files };
        let published = materialize_at(&temporary.path().join("runtime"), payload).unwrap();
        let marker = published.join(COMPLETE_MARKER);
        fs::remove_file(&marker).unwrap();
        symlink(temporary.path().join("outside"), marker).unwrap();

        assert!(matches!(verify_tree(&published, payload), Err(TreeState::Unsafe(_))));
    }

    #[cfg(all(windows, feature = "embedded-runtime"))]
    #[test]
    fn windows_embeds_ocr_but_not_packaged_pdfium() {
        assert!(!should_register_pdfium_resolver());
        assert_eq!(PDFIUM_ARCHIVE.len(), 0);
        assert!(PDFIUM_ARCHIVE_SHA256.is_empty());
        assert!(PDFIUM_FILES.is_empty());
        assert!(!OCR_ARCHIVE.is_empty());
        assert!(!OCR_FILES.is_empty());
    }
}
