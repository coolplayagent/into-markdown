//! Converter and source-resolution catalog.
//!
//! Built-in source resolvers, format detectors, and converters.

mod core_catalog;
mod core_catalog_authority;
mod delimited;
mod docx;
mod drawio;
mod embedded_visual_ocr;
mod epub;
mod feed;
mod html;
mod image_converter;
mod legacy_office;
mod markdown;
mod media;
mod media_type;
use media_type::format_from_media_type;
mod msg;
mod notebook;
mod odf;
mod pdf;
mod pdf_ocr;
mod presentation;
mod remote;
mod rtf;
mod structured;
mod text;
mod workbook;
#[path = "zip/mod.rs"]
mod zip_converter;

#[cfg(test)]
mod fixture_corpus_tests;

pub use core_catalog::{
    CapabilityAvailability, CapabilityDescriptor, CapabilityKind, CapabilitySource,
    CatalogFormatDescriptor, CoreCatalogAuthority, CoreCatalogAuthorityEntry,
    CoreRuntimeAuthorityEntry, FormatDescriptor, FormatStatus, RuntimeRequirement,
    core_capabilities, core_catalog_authority, core_format_catalog, core_formats,
    register_core_components, validate_core_capabilities, verify_packaged_legacy_office_runtime,
};
pub use delimited::DelimitedTextConverter;
pub use docx::DocxConverter;
pub use drawio::DrawioConverter;
pub use embedded_visual_ocr::EmbeddedVisualOcrEnricher;
pub use epub::EpubConverter;
pub use feed::FeedConverter;
pub use html::HtmlConverter;
pub use image_converter::ImageConverter;
pub use into_markdown_pdf_layout::{
    LayoutConfig as PdfLayoutConfig, LayoutLimits as PdfLayoutLimits,
    reconstruct_document as reconstruct_pdf_layout,
};
pub use legacy_office::LegacyOfficeConverter;
pub use markdown::MarkdownConverter;
pub use media::MediaConverter;
pub use msg::MsgConverter;
pub use notebook::NotebookConverter;
pub use odf::OdfConverter;
pub use pdf::{
    PdfConverter, default_pdfium_runtime_path, install_pdfium_runtime_resolver,
    verify_pdfium_runtime,
};
pub use pdf_ocr::merge_pdf_ocr;
pub use presentation::PresentationConverter;
pub use remote::{
    HttpSourceResolver, MediaWikiConverter, MediaWikiFormatDetector, MediaWikiSourceResolver,
};
pub use rtf::RtfConverter;
pub use structured::StructuredDataConverter;
pub use text::TextConverter;
pub use workbook::WorkbookConverter;
pub use zip_converter::ZipConverter;

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, ExecutionContext, FormatCandidate,
    FormatDetector, FormatHint, InputFormat, InputRef, ResolvedInput, ResolvedSource,
    SourceMetadata, SourceResolver,
};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use std::collections::BTreeMap;
use std::fs::{File, Metadata, OpenOptions};
use std::future::Future;
use std::io::{Cursor, Read};
use std::path::Path;
use std::pin::Pin;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll, Waker};

/// Backward-compatible access to the installed core format catalog.
///
/// The result intentionally contains no planned, media, site-specific, or
/// plugin capabilities.
#[must_use]
pub fn planned_formats() -> &'static [FormatDescriptor] {
    core_formats()
}

const PATH_WORKER_COUNT: usize = 4;
const PATH_QUEUE_CAPACITY: usize = 32;
const STDIN_QUEUE_CAPACITY: usize = 1;

type BlockingJob = Box<dyn FnOnce() + Send + 'static>;

struct BlockingPool {
    sender: SyncSender<BlockingJob>,
    queue_limit: &'static str,
}

impl BlockingPool {
    fn new(
        worker_name: &'static str,
        worker_count: usize,
        queue_capacity: usize,
        queue_limit: &'static str,
    ) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel::<BlockingJob>(queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("{worker_name}-{index}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let receiver = lock_unpoisoned(&receiver);
                            receiver.recv()
                        };
                        let Ok(job) = job else { break };
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                })
                .map_err(|error| format!("start {worker_name} worker: {error}"))?;
        }
        Ok(Self { sender, queue_limit })
    }

    fn submit<T, F>(&self, operation: F) -> Result<BlockingFuture<T>, ConversionError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, ConversionError> + Send + 'static,
    {
        let state = Arc::new(BlockingState::default());
        let worker_state = Arc::clone(&state);
        let job: BlockingJob = Box::new(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
                .unwrap_or_else(|_| {
                    Err(ConversionError::Internal {
                        detail: "blocking source worker panicked".into(),
                    })
                });
            worker_state.complete(outcome);
        });
        match self.sender.try_send(job) {
            Ok(()) => Ok(BlockingFuture { state }),
            Err(TrySendError::Full(_)) => Err(ConversionError::ResourceLimit {
                limit: self.queue_limit,
                detail: "blocking source worker queue is full".into(),
            }),
            Err(TrySendError::Disconnected(_)) => Err(ConversionError::ComponentUnavailable {
                component: "blocking-source-workers".into(),
                detail: "blocking source worker queue is unavailable".into(),
            }),
        }
    }
}

struct BlockingState<T> {
    inner: Mutex<BlockingStateInner<T>>,
}

struct BlockingStateInner<T> {
    outcome: Option<Result<T, ConversionError>>,
    waker: Option<Waker>,
    abandoned: bool,
}

impl<T> Default for BlockingState<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(BlockingStateInner { outcome: None, waker: None, abandoned: false }),
        }
    }
}

impl<T> BlockingState<T> {
    fn complete(&self, outcome: Result<T, ConversionError>) {
        let mut inner = lock_unpoisoned(&self.inner);
        if inner.abandoned {
            return;
        }
        inner.outcome = Some(outcome);
        let waker = inner.waker.take();
        drop(inner);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct BlockingFuture<T> {
    state: Arc<BlockingState<T>>,
}

impl<T> Future for BlockingFuture<T> {
    type Output = Result<T, ConversionError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = lock_unpoisoned(&self.state.inner);
        if let Some(outcome) = inner.outcome.take() {
            return Poll::Ready(outcome);
        }
        inner.waker = Some(context.waker().clone());
        Poll::Pending
    }
}

impl<T> Drop for BlockingFuture<T> {
    fn drop(&mut self) {
        let mut inner = lock_unpoisoned(&self.state.inner);
        inner.abandoned = true;
        inner.waker = None;
        inner.outcome.take();
    }
}

fn path_pool() -> Result<&'static BlockingPool, ConversionError> {
    static POOL: OnceLock<Result<BlockingPool, String>> = OnceLock::new();
    POOL.get_or_init(|| {
        BlockingPool::new(
            "into-md-path-io",
            PATH_WORKER_COUNT,
            PATH_QUEUE_CAPACITY,
            "blocking_path_queue",
        )
    })
    .as_ref()
    .map_err(|detail| ConversionError::ComponentUnavailable {
        component: "blocking-path-workers".into(),
        detail: detail.clone(),
    })
}

fn stdin_pool() -> Result<&'static BlockingPool, ConversionError> {
    static POOL: OnceLock<Result<BlockingPool, String>> = OnceLock::new();
    POOL.get_or_init(|| {
        BlockingPool::new("into-md-stdin-io", 1, STDIN_QUEUE_CAPACITY, "blocking_stdin_queue")
    })
    .as_ref()
    .map_err(|detail| ConversionError::ComponentUnavailable {
        component: "blocking-stdin-worker".into(),
        detail: detail.clone(),
    })
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Resolver for in-memory inputs.
#[derive(Debug, Default)]
pub struct MemorySourceResolver;

impl SourceResolver for MemorySourceResolver {
    fn id(&self) -> &'static str {
        "builtin.source.memory"
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Bytes { .. })
    }

    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        let future = self.resolve_accounted(input, options, context);
        Box::pin(async move { future.await.map(ResolvedSource::into_input) })
    }

    fn resolve_accounted<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedSource, ConversionError>> {
        Box::pin(async move {
            let InputRef::Bytes { data, name } = input else {
                return Err(ConversionError::Unsupported {
                    detail: "expected memory input".into(),
                });
            };
            context.checkpoint()?;
            enforce_input_limit(data.len() as u64, options.limits.max_input_bytes)?;
            let size = u64::try_from(data.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "memory input size cannot be represented as u64".into(),
            })?;
            let memory = context.reserve_memory(size)?;
            Ok(ResolvedSource::with_memory_reservation(
                ResolvedInput {
                    bytes: Arc::clone(data),
                    metadata: SourceMetadata {
                        name: name.clone(),
                        size,
                        ..SourceMetadata::default()
                    },
                },
                memory,
            ))
        })
    }
}

/// Resolver for local paths.
#[derive(Debug, Default)]
pub struct LocalFileSourceResolver;

impl SourceResolver for LocalFileSourceResolver {
    fn id(&self) -> &'static str {
        "builtin.source.local-file"
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Path(_))
    }

    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        let future = self.resolve_accounted(input, options, context);
        Box::pin(async move { future.await.map(ResolvedSource::into_input) })
    }

    fn resolve_accounted<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedSource, ConversionError>> {
        let InputRef::Path(path) = input else {
            return Box::pin(async {
                Err(ConversionError::Unsupported { detail: "expected local path".into() })
            });
        };
        let path = path.clone();
        let limit = options.limits.max_input_bytes;
        let context = context.clone();
        let submitted = path_pool().and_then(|pool| {
            pool.submit(move || {
                context.checkpoint()?;
                let (mut file, metadata) = securely_open_local_file(&path)?;
                enforce_input_limit(metadata.len(), limit)?;
                let (bytes, memory) = read_bounded(&mut file, limit, &context)?;
                let size =
                    u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                        limit: "max_input_bytes",
                        detail: "resolved file size cannot be represented as u64".into(),
                    })?;
                let (bytes, memory) = into_shared_source(bytes, memory)?;
                Ok(ResolvedSource::with_memory_reservation(
                    ResolvedInput {
                        bytes,
                        metadata: SourceMetadata {
                            name: path
                                .file_name()
                                .and_then(|value| value.to_str())
                                .map(str::to_owned),
                            size,
                            ..SourceMetadata::default()
                        },
                    },
                    memory,
                ))
            })
        });
        Box::pin(async move {
            let future = submitted?;
            future.await
        })
    }
}

/// Resolver for standard input.
#[derive(Debug, Default)]
pub struct StdinSourceResolver;

impl SourceResolver for StdinSourceResolver {
    fn id(&self) -> &'static str {
        "builtin.source.stdin"
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Stdin)
    }

    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        let future = self.resolve_accounted(input, options, context);
        Box::pin(async move { future.await.map(ResolvedSource::into_input) })
    }

    fn resolve_accounted<'a>(
        &'a self,
        _: &'a InputRef,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedSource, ConversionError>> {
        let limit = options.limits.max_input_bytes;
        let context = context.clone();
        let submitted = stdin_pool().and_then(|pool| {
            pool.submit(move || {
                context.checkpoint()?;
                let mut stdin = std::io::stdin().lock();
                let (bytes, memory) = read_bounded(&mut stdin, limit, &context)?;
                let size =
                    u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                        limit: "max_input_bytes",
                        detail: "resolved stdin size cannot be represented as u64".into(),
                    })?;
                let (bytes, memory) = into_shared_source(bytes, memory)?;
                Ok(ResolvedSource::with_memory_reservation(
                    ResolvedInput {
                        bytes,
                        metadata: SourceMetadata {
                            name: Some("stdin".into()),
                            size,
                            ..SourceMetadata::default()
                        },
                    },
                    memory,
                ))
            })
        });
        Box::pin(async move {
            let future = submitted?;
            future.await
        })
    }
}

fn read_bounded(
    reader: &mut dyn Read,
    limit: u64,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, into_markdown_core::ResourceReservation), ConversionError> {
    let mut bytes = Vec::new();
    let mut memory = context.reserve_memory(0)?;
    let mut total = 0_u64;
    let scratch_bytes = limit.saturating_add(1).min(64 * 1024);
    let scratch = context.reserve_memory(scratch_bytes)?;
    let scratch_len =
        usize::try_from(scratch_bytes).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "source scratch buffer size cannot be represented as usize".into(),
        })?;
    // The charge must exist before this allocation. It remains held until the
    // reader and its scratch buffer are no longer used.
    let mut chunk = vec![0_u8; scratch_len].into_boxed_slice();
    loop {
        context.checkpoint()?;
        let remaining_plus_one = limit.saturating_sub(total).saturating_add(1);
        let read_limit = usize::try_from(remaining_plus_one).unwrap_or(usize::MAX).min(chunk.len());
        let read = reader.read(&mut chunk[..read_limit])?;
        if read == 0 {
            break;
        }
        let read_u64 = u64::try_from(read).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: "source read size cannot be represented as u64".into(),
        })?;
        total = total.checked_add(read_u64).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: "source byte count overflowed".into(),
        })?;
        if total > limit {
            return Err(ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: format!("{total} > {limit}"),
            });
        }
        // Charge initialized payload bytes before requesting backing storage.
        // `try_reserve_exact` avoids implementation-requested growth slack;
        // allocator size-class rounding and bookkeeping are outside this
        // cooperative logical budget and are not represented as process RSS.
        memory.grow(read_u64)?;
        bytes.try_reserve_exact(read).map_err(|error| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!("could not reserve source buffer: {error}"),
        })?;
        bytes.extend_from_slice(&chunk[..read]);
    }
    // Release the allocation before its accounting guard; keeping the reverse
    // order would create an uncharged lifetime window during the handoff.
    drop(chunk);
    drop(scratch);
    Ok((bytes, memory))
}

fn into_shared_source(
    bytes: Vec<u8>,
    mut memory: into_markdown_core::ResourceReservation,
) -> Result<(Arc<[u8]>, into_markdown_core::ResourceReservation), ConversionError> {
    let size = u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "resolved source size cannot be represented as u64".into(),
    })?;
    // `Arc<[u8]>::from(Vec<u8>)` may copy. Account the worst-case overlap
    // before allocating the shared buffer, then release only the consumed Vec
    // half after the conversion has completed.
    memory.grow(size)?;
    let shared = Arc::from(bytes);
    memory.shrink(size)?;
    Ok((shared, memory))
}

#[cfg(unix)]
fn securely_open_local_file(path: &Path) -> Result<(File, Metadata), ConversionError> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let expected = std::fs::symlink_metadata(path)?;
    if expected.file_type().is_symlink() || !expected.is_file() {
        return Err(ConversionError::Io {
            detail: format!("local input is not a regular non-symlink file: {}", path.display()),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || expected.dev() != opened.dev() || expected.ino() != opened.ino() {
        return Err(ConversionError::Io {
            detail: format!("local input changed while it was opened: {}", path.display()),
        });
    }
    Ok((file, opened))
}

#[cfg(windows)]
fn securely_open_local_file(path: &Path) -> Result<(File, Metadata), ConversionError> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    validate_windows_local_path(path)?;
    // This no-follow handle is the authoritative source object. There is no
    // path metadata snapshot to race with a later open, and the same handle is
    // retained for every subsequent read.
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let file_type = winapi_util::file::typ(&file)?;
    let opened = file.metadata()?;
    validate_windows_opened_file(
        path,
        file_type.is_disk(),
        opened.file_attributes(),
        opened.is_file(),
    )?;
    Ok((file, opened))
}

#[cfg(windows)]
fn validate_windows_opened_file(
    path: &Path,
    is_disk: bool,
    attributes: u32,
    is_regular: bool,
) -> Result<(), ConversionError> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    if !is_disk || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !is_regular {
        return Err(ConversionError::Io {
            detail: format!(
                "local input is not a regular non-reparse disk file: {}",
                path.display()
            ),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_local_path(path: &Path) -> Result<(), ConversionError> {
    use std::os::windows::ffi::OsStrExt as _;

    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if is_windows_device_namespace(&wide) || has_windows_reserved_device_component(&wide) {
        return Err(ConversionError::Io {
            detail: format!("local input uses a denied Windows device path: {}", path.display()),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn is_windows_device_namespace(path: &[u16]) -> bool {
    // ASCII code units are written directly because `From<u8>` is not const on the pinned Rust.
    const BACKSLASH: u16 = 0x005c;
    const QUESTION: u16 = 0x003f;
    const DOT: u16 = 0x002e;

    let is_separator = |value| value == BACKSLASH || value == u16::from(b'/');
    if path.len() >= 4
        && is_separator(path[0])
        && is_separator(path[1])
        && path[2] == DOT
        && is_separator(path[3])
    {
        return true;
    }
    if path.len() >= 4
        && is_separator(path[0])
        && path[1] == QUESTION
        && path[2] == QUESTION
        && is_separator(path[3])
    {
        return true;
    }
    if path.len() >= 5
        && is_separator(path[0])
        && is_separator(path[1])
        && path[2] == QUESTION
        && path[3] == QUESTION
        && is_separator(path[4])
    {
        return true;
    }
    if path.len() < 4
        || !is_separator(path[0])
        || !is_separator(path[1])
        || path[2] != QUESTION
        || !is_separator(path[3])
    {
        return false;
    }

    let extended = &path[4..];
    let drive_path = extended.len() >= 3
        && is_ascii_letter(extended[0])
        && extended[1] == u16::from(b':')
        && is_separator(extended[2]);
    let unc_path = starts_with_ascii_case_insensitive(extended, "UNC\\")
        || starts_with_ascii_case_insensitive(extended, "UNC/");
    !drive_path && !unc_path
}

#[cfg(windows)]
fn has_windows_reserved_device_component(path: &[u16]) -> bool {
    path.split(|value| *value == u16::from(b'\\') || *value == u16::from(b'/'))
        .filter(|component| !component.is_empty())
        .any(|component| {
            let end = component
                .iter()
                .rposition(|value| *value != u16::from(b' ') && *value != u16::from(b'.'))
                .map_or(0, |index| index + 1);
            let stem_end = component[..end]
                .iter()
                .position(|value| *value == u16::from(b'.') || *value == u16::from(b':'))
                .unwrap_or(end);
            let trimmed_stem_end = component[..stem_end]
                .iter()
                .rposition(|value| *value != u16::from(b' ') && *value != u16::from(b'.'))
                .map_or(0, |index| index + 1);
            let stem = &component[..trimmed_stem_end];
            ["CON", "PRN", "AUX", "NUL", "CLOCK$", "CONIN$", "CONOUT$"]
                .iter()
                .any(|name| equals_ascii_case_insensitive(stem, name))
                || is_numbered_windows_device(stem, "COM")
                || is_numbered_windows_device(stem, "LPT")
        })
}

#[cfg(windows)]
fn is_numbered_windows_device(value: &[u16], prefix: &str) -> bool {
    value.len() == prefix.len() + 1
        && starts_with_ascii_case_insensitive(value, prefix)
        && ((u16::from(b'1')..=u16::from(b'9')).contains(&value[prefix.len()])
            || [0x00B9, 0x00B2, 0x00B3].contains(&value[prefix.len()]))
}

#[cfg(windows)]
fn starts_with_ascii_case_insensitive(value: &[u16], prefix: &str) -> bool {
    value.len() >= prefix.len()
        && value
            .iter()
            .zip(prefix.bytes())
            .all(|(left, right)| ascii_uppercase(*left) == u16::from(right.to_ascii_uppercase()))
}

#[cfg(windows)]
fn equals_ascii_case_insensitive(value: &[u16], expected: &str) -> bool {
    value.len() == expected.len() && starts_with_ascii_case_insensitive(value, expected)
}

#[cfg(windows)]
fn is_ascii_letter(value: u16) -> bool {
    (u16::from(b'A')..=u16::from(b'Z')).contains(&ascii_uppercase(value))
}

#[cfg(windows)]
fn ascii_uppercase(value: u16) -> u16 {
    if (u16::from(b'a')..=u16::from(b'z')).contains(&value) {
        value - u16::from(b'a' - b'A')
    } else {
        value
    }
}

#[cfg(not(any(unix, windows)))]
fn securely_open_local_file(_: &Path) -> Result<(File, Metadata), ConversionError> {
    Err(ConversionError::ComponentUnavailable {
        component: "secure-local-file-open".into(),
        detail: "this platform has no audited no-follow local-file open policy".into(),
    })
}

/// Backward-compatible name for the audited HTTP(S) resolver.
pub type UriSourceResolver = HttpSourceResolver;

fn enforce_input_limit(size: u64, limit: u64) -> Result<(), ConversionError> {
    if size > limit {
        return Err(ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: format!("{size} > {limit}"),
        });
    }
    Ok(())
}

/// Detector for caller/source hints. It reports every usable hint so conflicts
/// remain visible instead of silently accepting the first value.
#[derive(Debug, Default)]
pub struct HintFormatDetector;

impl FormatDetector for HintFormatDetector {
    fn id(&self) -> &'static str {
        "builtin.detector.hints"
    }

    fn priority(&self) -> i32 {
        100
    }

    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        hint: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let mut evidence: BTreeMap<InputFormat, Vec<&str>> = BTreeMap::new();
            let extension = hint
                .extension
                .as_deref()
                .or_else(|| hint.filename.as_deref().and_then(extension_of))
                .or_else(|| input.metadata.name.as_deref().and_then(extension_of));
            if let Some(format) = extension.and_then(InputFormat::from_extension) {
                evidence.entry(format).or_default().push("filename extension");
            }
            let media_type = hint.media_type.as_deref().or(input.metadata.media_type.as_deref());
            if let Some(format) = media_type.and_then(format_from_media_type) {
                evidence.entry(format).or_default().push("media type");
            }
            let delimited_hint =
                evidence.keys().any(|format| matches!(format, InputFormat::Csv | InputFormat::Tsv));
            if hint.charset.is_some() && !delimited_hint {
                evidence.entry(InputFormat::Text).or_default().push("character encoding hint");
            }
            let conflict = evidence.len() > 1;
            Ok(evidence
                .into_iter()
                .map(|(format, reasons)| {
                    let strong_package_hint = !conflict
                        && reasons.contains(&"filename extension")
                        && matches!(
                            format,
                            InputFormat::Docx
                                | InputFormat::Pptx
                                | InputFormat::Xlsx
                                | InputFormat::Odt
                                | InputFormat::Ods
                                | InputFormat::Odp
                                | InputFormat::Epub
                        );
                    let confidence = if format == InputFormat::Drawio {
                        1.0
                    } else if strong_package_hint
                        || matches!(
                            format,
                            InputFormat::Markdown
                                | InputFormat::Csv
                                | InputFormat::Tsv
                                | InputFormat::Wikipedia
                        )
                    {
                        if strong_package_hint { 0.91 } else { 0.99 }
                    } else if reasons.len() > 1 {
                        0.68
                    } else if reasons[0] == "media type" {
                        0.60
                    } else {
                        0.55
                    };
                    let candidate = FormatCandidate::new(format, confidence, reasons.join(" + "));
                    if conflict {
                        candidate.with_diagnostic("filename extension and media type disagree")
                    } else {
                        candidate
                    }
                })
                .collect())
        })
    }
}

/// Detector for file signatures and bounded inspection of ZIP/OLE containers.
#[derive(Debug, Default)]
pub struct ContentFormatDetector;

impl FormatDetector for ContentFormatDetector {
    fn id(&self) -> &'static str {
        "builtin.detector.content"
    }

    fn priority(&self) -> i32 {
        200
    }

    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            detect_content(&input.bytes, context)
        })
    }
}

const ZIP_INSPECTION_ENTRY_LIMIT: usize = 4096;
const ZIP_MIMETYPE_READ_LIMIT: u64 = 128;
const ZIP_NAME_READ_LIMIT: usize = 1024 * 1024;
const ZIP_CENTRAL_DIRECTORY_LIMIT: usize = 8 * 1024 * 1024;
const ZIP_METADATA_READ_LIMIT: u64 = 256 * 1024;
const ZIP_XML_EVENT_LIMIT: usize = 32 * 1024;
const OLE_INSPECTION_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const TEXT_INSPECTION_BYTE_LIMIT: usize = 1024 * 1024;
const JSON_SCAN_DEPTH_LIMIT: usize = 4096;
const JSON_SCAN_CHECKPOINT_BYTES: usize = 4096;

fn detect_content(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Vec<FormatCandidate>, ConversionError> {
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        return Ok(detect_zip(bytes));
    }
    if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
        return Ok(detect_ole(bytes));
    }
    if let Some(candidate) = magic_candidate(bytes) {
        return Ok(vec![candidate]);
    }
    if drawio::evidence(bytes, context)? {
        return Ok(vec![FormatCandidate::new(InputFormat::Drawio, 0.99, "Drawio graph root")]);
    }
    if let Some(candidate) = structured_text_candidate(bytes, context)? {
        return Ok(vec![candidate]);
    }
    Ok(text::sniff_unstructured_text(bytes, context)?
        .map(|confidence| {
            FormatCandidate::new(
                InputFormat::Text,
                confidence,
                "plain-text safety and encoding thresholds",
            )
        })
        .into_iter()
        .collect())
}

fn magic_candidate(bytes: &[u8]) -> Option<FormatCandidate> {
    let (format, confidence, evidence) = if pdf::has_pdf_header(bytes) {
        (InputFormat::Pdf, 0.99, "PDF magic bytes")
    } else if rtf::strict_header(bytes).is_some() {
        (InputFormat::Rtf, 0.99, "RTF signature")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"II*\0")
        || bytes.starts_with(b"MM\0*")
        || bytes.starts_with(b"II+\0")
        || bytes.starts_with(b"MM\0+")
        || bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
    {
        (InputFormat::Image, 0.98, "image magic bytes")
    } else if valid_bmp_header(bytes) {
        (InputFormat::Image, 0.98, "validated BMP file and DIB headers")
    } else if bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"ID3")
        || bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
    {
        (InputFormat::Audio, 0.96, "audio magic bytes")
    } else if valid_mpeg_audio_frame_header(bytes) {
        (InputFormat::Audio, 0.92, "MPEG audio frame header")
    } else if valid_adts_aac_header(bytes) {
        (InputFormat::Audio, 0.96, "ADTS AAC frame headers")
    } else if bytes.starts_with(b"OggS") {
        return Some(detect_ogg(bytes));
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some(detect_ebml(bytes));
    } else if bytes.get(4..8) == Some(b"ftyp") {
        return detect_iso_media(bytes);
    } else {
        return None;
    };
    Some(FormatCandidate::new(format, confidence, evidence))
}

fn valid_adts_aac_header(bytes: &[u8]) -> bool {
    let Some(first_bytes) = adts_aac_frame_bytes(bytes, 0) else {
        return false;
    };
    adts_aac_frame_bytes(bytes, first_bytes).is_some()
}

fn adts_aac_frame_bytes(bytes: &[u8], offset: usize) -> Option<usize> {
    let header = bytes.get(offset..offset.checked_add(7)?)?;
    if header[0] != 0xff || header[1] & 0xf6 != 0xf0 {
        return None;
    }
    let sample_rate_index = (header[2] >> 2) & 0x0f;
    let channel_configuration = ((header[2] & 1) << 2) | (header[3] >> 6);
    if sample_rate_index == 0x0f || channel_configuration == 0 {
        return None;
    }
    let frame_bytes = usize::from(header[3] & 0x03) << 11
        | usize::from(header[4]) << 3
        | usize::from(header[5] >> 5);
    let header_bytes = if header[1] & 1 == 1 { 7 } else { 9 };
    if frame_bytes < header_bytes || offset.checked_add(frame_bytes)? > bytes.len() {
        return None;
    }
    Some(frame_bytes)
}

fn valid_mpeg_audio_frame_header(bytes: &[u8]) -> bool {
    let Some(first) = mpeg_audio_header(bytes, 0) else {
        return false;
    };
    let Some(second) = mpeg_audio_header(bytes, first.frame_bytes) else {
        return false;
    };
    first.version == second.version
        && first.layer == second.layer
        && first.sample_rate_hz == second.sample_rate_hz
}

#[derive(Clone, Copy)]
struct MpegAudioHeader {
    version: u8,
    layer: u8,
    sample_rate_hz: usize,
    frame_bytes: usize,
}

fn mpeg_audio_header(bytes: &[u8], offset: usize) -> Option<MpegAudioHeader> {
    let header = bytes.get(offset..offset.checked_add(4)?)?;
    if header[0] != 0xff || header[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (header[1] >> 3) & 0b11;
    let layer = (header[1] >> 1) & 0b11;
    let bitrate_index = usize::from(header[2] >> 4);
    let sample_rate_index = usize::from((header[2] >> 2) & 0b11);
    if version == 0b01
        || layer == 0
        || matches!(bitrate_index, 0 | 0x0f)
        || sample_rate_index == 0b11
    {
        return None;
    }
    let bitrate_kbps = match (version, layer) {
        (0b11, 0b11) => [32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
        (0b11, 0b10) => [32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        (0b11, 0b01) => [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
        (_, 0b11) => [32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
        (_, 0b10 | 0b01) => [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        _ => return None,
    }[bitrate_index - 1];
    let sample_rate_hz = match version {
        0b11 => [44_100, 48_000, 32_000],
        0b10 => [22_050, 24_000, 16_000],
        0b00 => [11_025, 12_000, 8_000],
        _ => return None,
    }[sample_rate_index];
    let padding = usize::from((header[2] >> 1) & 1);
    let bitrate = bitrate_kbps * 1_000;
    let frame_bytes = if layer == 0b11 {
        (12 * bitrate / sample_rate_hz + padding) * 4
    } else if layer == 0b01 && version != 0b11 {
        72 * bitrate / sample_rate_hz + padding
    } else {
        144 * bitrate / sample_rate_hz + padding
    };
    offset.checked_add(frame_bytes)?;
    Some(MpegAudioHeader { version, layer, sample_rate_hz, frame_bytes })
}

fn valid_bmp_header(bytes: &[u8]) -> bool {
    if bytes.get(..2) != Some(b"BM") {
        return false;
    }
    let Some(file_size) = little_u32(bytes, 2).and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(pixel_offset) = little_u32(bytes, 10).and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    let Some(dib_size) = little_u32(bytes, 14).and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(headers_end) = 14_usize.checked_add(dib_size) else {
        return false;
    };
    if !matches!(dib_size, 12 | 40 | 52 | 56 | 64 | 108 | 124)
        || headers_end > bytes.len()
        || pixel_offset < headers_end
        || file_size <= pixel_offset
        || file_size > bytes.len()
    {
        return false;
    }
    let (dimensions_valid, planes, bits_per_pixel) = if dib_size == 12 {
        let dimensions_valid = little_u16(bytes, 18).is_some_and(|value| value != 0)
            && little_u16(bytes, 20).is_some_and(|value| value != 0);
        (dimensions_valid, little_u16(bytes, 22), little_u16(bytes, 24))
    } else {
        let dimensions_valid = little_i32(bytes, 18).is_some_and(|value| value != 0)
            && little_i32(bytes, 22).is_some_and(|value| value != 0);
        (dimensions_valid, little_u16(bytes, 26), little_u16(bytes, 28))
    };
    dimensions_valid
        && planes == Some(1)
        && bits_per_pixel.is_some_and(|value| matches!(value, 1 | 4 | 8 | 16 | 24 | 32))
}

fn little_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn little_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn little_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    little_u32(bytes, offset).map(|value| i32::from_le_bytes(value.to_le_bytes()))
}

fn detect_ogg(bytes: &[u8]) -> FormatCandidate {
    let packet = bytes.get(26).and_then(|segments| {
        let table_end = 27_usize.checked_add(usize::from(*segments))?;
        let payload = bytes.get(table_end..)?;
        Some(payload)
    });
    if packet
        .is_some_and(|value| value.starts_with(b"OpusHead") || value.starts_with(b"\x01vorbis"))
    {
        FormatCandidate::new(InputFormat::Audio, 0.98, "Ogg audio codec signature")
    } else if packet.is_some_and(|value| value.starts_with(b"\x80theora")) {
        FormatCandidate::new(InputFormat::Video, 0.98, "Ogg Theora codec signature")
    } else {
        FormatCandidate::new(InputFormat::Audio, 0.40, "Ogg container signature")
            .with_diagnostic("Ogg codec could not be identified; container may hold audio or video")
    }
}

fn detect_ebml(bytes: &[u8]) -> FormatCandidate {
    let prefix = &bytes[..bytes.len().min(4096)];
    let document_type = if contains_ascii_case_insensitive(prefix, b"webm") {
        "WebM"
    } else if contains_ascii_case_insensitive(prefix, b"matroska") {
        "Matroska"
    } else {
        "unknown EBML"
    };
    FormatCandidate::new(InputFormat::Video, 0.60, format!("{document_type} container signature"))
        .with_diagnostic("container type does not prove that a video track is present")
}

fn detect_iso_media(bytes: &[u8]) -> Option<FormatCandidate> {
    let box_size = usize::try_from(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    if box_size < 16 || box_size > bytes.len() || (box_size - 16) % 4 != 0 {
        return None;
    }
    let brand = bytes.get(8..12)?;
    if matches!(brand, b"M4A " | b"M4B " | b"F4A ") {
        Some(FormatCandidate::new(InputFormat::Audio, 0.96, "audio ISO base media brand"))
    } else if matches!(
        brand,
        b"avc1" | b"iso2" | b"isom" | b"mp41" | b"mp42" | b"qt  " | b"M4V " | b"F4V "
    ) {
        Some(
            FormatCandidate::new(InputFormat::Video, 0.70, "video-capable ISO base media brand")
                .with_diagnostic("container brand does not prove that a video track is present"),
        )
    } else {
        Some(
            FormatCandidate::new(InputFormat::Video, 0.40, "unknown ISO base media brand")
                .with_diagnostic("ISO base media brand is not recognized as audio or video"),
        )
    }
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

#[allow(clippy::too_many_lines)]
fn structured_text_candidate(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<FormatCandidate>, ConversionError> {
    if let Some(json) = json_payload(bytes) {
        let summary = scan_json(json, context)?;
        if summary.status == JsonScanStatus::Complete {
            return Ok(Some(if summary.notebook {
                FormatCandidate::new(InputFormat::Ipynb, 0.99, "Jupyter notebook JSON structure")
            } else {
                FormatCandidate::new(InputFormat::Json, 0.96, "valid JSON content")
            }));
        }
        if strong_json_prefix(json) {
            return Ok(Some(
                FormatCandidate::new(
                    InputFormat::Json,
                    0.50,
                    "JSON structural prefix with invalid or incomplete syntax",
                )
                .with_diagnostic("incomplete JSON evidence does not override a filename extension"),
            ));
        }
    }

    if feed::strong_feed_evidence(bytes, context)? {
        return Ok(Some(FormatCandidate::new(
            InputFormat::Feed,
            0.99,
            "namespace-qualified RSS 2.0 or Atom 1.0 root",
        )));
    }

    if let Some(decoded) = structured::decode_xml_for_detection(bytes, context)? {
        match decoded {
            structured::XmlDetectionText::Decoded(decoded) => {
                let text = decoded.trim_start();
                if let Some(root) = xml_root_name(text) {
                    return Ok(Some(if structured::xml_complete_for_detection(bytes, context)? {
                        FormatCandidate::new(
                            InputFormat::Xml,
                            0.92,
                            format!("complete UTF-16 XML {root} root element"),
                        )
                    } else {
                        FormatCandidate::new(
                            InputFormat::Xml,
                            0.50,
                            format!("incomplete UTF-16 XML {root} root element"),
                        )
                        .with_diagnostic(
                            "incomplete XML evidence does not override a filename extension",
                        )
                    }));
                }
                if strong_xml_prefix(text) {
                    return Ok(Some(FormatCandidate::new(
                        InputFormat::Xml,
                        0.90,
                        "UTF-16 XML declaration or paired markup",
                    )));
                }
            }
            structured::XmlDetectionText::InvalidUtf16 => {
                return Ok(Some(FormatCandidate::new(
                    InputFormat::Xml,
                    0.50,
                    "UTF-16 XML signature with invalid encoded content",
                )));
            }
        }
    }

    let Some((prefix, _)) = bounded_utf8_prefix(bytes, TEXT_INSPECTION_BYTE_LIMIT) else {
        return Ok(None);
    };
    let text = prefix.trim_start_matches('\u{feff}');
    if markdown_indented_code_prefix(text, context)? {
        return Ok(Some(FormatCandidate::new(
            InputFormat::Markdown,
            0.91,
            "Markdown indented code containing markup",
        )));
    }
    let text = text.trim_start();
    if html_prelude_identifies_html(text) {
        return Ok(Some(FormatCandidate::new(InputFormat::Html, 0.96, "HTML root markup")));
    }
    let root = xml_root_name(text);
    if root.is_some_and(|root| root.eq_ignore_ascii_case("html")) {
        return Ok(Some(FormatCandidate::new(InputFormat::Html, 0.96, "HTML/XHTML root element")));
    }
    if text.starts_with("<?xml") {
        return Ok(Some(if structured::xml_complete_for_detection(bytes, context)? {
            FormatCandidate::new(InputFormat::Xml, 0.92, "complete XML declaration")
        } else {
            FormatCandidate::new(InputFormat::Xml, 0.50, "incomplete XML declaration")
                .with_diagnostic("incomplete XML evidence does not override a filename extension")
        }));
    }
    let markdown = markdown::strong_markdown_evidence(text, context)?;
    let html = html_document_evidence(text, context)?;
    if html && (markdown || markdown_prefix_evidence(text, context)?) {
        return Ok(Some(FormatCandidate::new(
            InputFormat::Markdown,
            0.91,
            "Markdown structure containing raw HTML",
        )));
    }
    if html {
        return Ok(Some(FormatCandidate::new(
            InputFormat::Html,
            0.94,
            "complete HTML semantic structure",
        )));
    }
    if let Some(root) = root {
        return Ok(Some(if structured::xml_complete_for_detection(bytes, context)? {
            FormatCandidate::new(
                InputFormat::Xml,
                0.92,
                format!("complete XML {root} root element"),
            )
        } else {
            FormatCandidate::new(
                InputFormat::Xml,
                0.50,
                format!("incomplete XML {root} root element"),
            )
            .with_diagnostic("incomplete XML evidence does not override a filename extension")
        }));
    }
    if strong_xml_prefix(text) {
        return Ok(Some(
            FormatCandidate::new(
                InputFormat::Xml,
                0.50,
                "XML declaration or paired markup with invalid structure",
            )
            .with_diagnostic("incomplete XML evidence does not override a filename extension"),
        ));
    }
    if markdown {
        return Ok(Some(FormatCandidate::new(
            InputFormat::Markdown,
            0.91,
            "multiple unambiguous Markdown/GFM structures",
        )));
    }
    let delimited_bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let Ok(delimited_text) = std::str::from_utf8(delimited_bytes) else {
        return Ok(None);
    };
    delimited::detected_candidate(delimited_text, context)
}

fn strong_json_prefix(json: &[u8]) -> bool {
    let Some(first) = json.first() else { return false };
    let next = json[1..].iter().copied().find(|byte| !byte.is_ascii_whitespace());
    matches!(
        (*first, next),
        (b'{', Some(b'"'))
            | (b'[', Some(b'{' | b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n'))
    )
}

fn strong_xml_prefix(text: &str) -> bool {
    text.starts_with("<?xml")
        || (text.starts_with('<') && (text.contains("</") || text.contains("/>")))
}

pub(crate) fn html_document_evidence(
    text: &str,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    if html_prelude_identifies_html(text.trim_start()) {
        return Ok(true);
    }
    let bytes = text.as_bytes();
    let mut starts = 0_u16;
    let mut ends = 0_u16;
    let mut paragraph = (false, false);
    let mut offset = 0_usize;
    let mut fence = None;
    let mut line_start = true;
    let mut indented_code = false;
    while offset < bytes.len() {
        if offset.is_multiple_of(4096) {
            context.checkpoint()?;
        }
        if line_start {
            let mut marker = offset;
            while bytes.get(marker) == Some(&b' ') {
                marker += 1;
            }
            let spaces = marker.saturating_sub(offset);
            indented_code = spaces >= 4 || bytes.get(marker) == Some(&b'\t');
            if !indented_code && spaces <= 3 {
                let found = if bytes.get(marker..marker.saturating_add(3)) == Some(b"```") {
                    Some(b'`')
                } else if bytes.get(marker..marker.saturating_add(3)) == Some(b"~~~") {
                    Some(b'~')
                } else {
                    None
                };
                if let Some(found) = found {
                    fence = if fence == Some(found) {
                        None
                    } else if fence.is_none() {
                        Some(found)
                    } else {
                        fence
                    };
                }
            }
        }
        if bytes[offset] == b'\n' || bytes[offset] == b'\r' {
            line_start = true;
            indented_code = false;
            offset += 1;
            continue;
        }
        if fence.is_some() || indented_code {
            line_start = false;
            offset += 1;
            continue;
        }
        line_start = false;
        if bytes.get(offset..offset.saturating_add(4)) == Some(b"<!--") {
            offset = find_bounded_ascii(bytes, offset.saturating_add(4), b"-->", context)?
                .map_or(bytes.len(), |end| end.saturating_add(3));
            continue;
        }
        if bytes[offset] != b'<' {
            offset += 1;
            continue;
        }
        let closing = bytes.get(offset.saturating_add(1)) == Some(&b'/');
        let name_start = offset.saturating_add(if closing { 2 } else { 1 });
        let mut name_end = name_start;
        while bytes.get(name_end).is_some_and(u8::is_ascii_alphabetic) {
            name_end += 1;
        }
        let Some(name) = bytes.get(name_start..name_end).filter(|name| !name.is_empty()) else {
            offset += 1;
            continue;
        };
        if !bytes
            .get(name_end)
            .is_some_and(|byte| matches!(byte, b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n' | 0x0c))
        {
            offset += 1;
            continue;
        }
        if name.eq_ignore_ascii_case(b"p") {
            if closing { paragraph.1 = true } else { paragraph.0 = true }
        } else if let Some(bit) = html_evidence_bit(name) {
            if closing { ends |= bit } else { starts |= bit }
        }
        offset = find_bounded_ascii(bytes, name_end, b">", context)?
            .map_or(bytes.len(), |end| end.saturating_add(1));
    }
    let pairs = (starts & ends).count_ones();
    Ok(pairs >= 2 || (pairs >= 1 && paragraph.0 && paragraph.1))
}

fn html_evidence_bit(name: &[u8]) -> Option<u16> {
    [b"main".as_slice(), b"article", b"section", b"nav", b"table", b"ul", b"ol", b"pre", b"title"]
        .iter()
        .position(|candidate| name.eq_ignore_ascii_case(candidate))
        .map(|index| 1_u16 << index)
}

fn find_bounded_ascii(
    bytes: &[u8],
    mut offset: usize,
    needle: &[u8],
    context: &ExecutionContext,
) -> Result<Option<usize>, ConversionError> {
    while offset.saturating_add(needle.len()) <= bytes.len() {
        if offset.is_multiple_of(4096) {
            context.checkpoint()?;
        }
        if bytes
            .get(offset..offset.saturating_add(needle.len()))
            .is_some_and(|value| value.eq_ignore_ascii_case(needle))
        {
            return Ok(Some(offset));
        }
        offset += 1;
    }
    Ok(None)
}

fn markdown_prefix_evidence(
    text: &str,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    for (index, line) in text.lines().take(4096).enumerate() {
        if index.is_multiple_of(128) {
            context.checkpoint()?;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(trimmed.starts_with("# ")
            || trimmed.starts_with("## ")
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("> ")
            || trimmed.starts_with("```")
            || trimmed.starts_with("~~~"));
    }
    Ok(false)
}

fn markdown_indented_code_prefix(
    text: &str,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    for (index, line) in text.lines().take(4096).enumerate() {
        if index.is_multiple_of(128) {
            context.checkpoint()?;
        }
        if line.trim().is_empty() {
            continue;
        }
        let spaces = line.as_bytes().iter().take_while(|byte| **byte == b' ').count();
        let indented = spaces >= 4 || line.as_bytes().get(spaces) == Some(&b'\t');
        return Ok(indented && line.trim_start().starts_with('<'));
    }
    Ok(false)
}

fn json_payload(mut bytes: &[u8]) -> Option<&[u8]> {
    if let Some(rest) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        bytes = rest;
    }
    let start = bytes.iter().position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))?;
    Some(&bytes[start..])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonScanStatus {
    Complete,
    Open,
    Invalid,
}

#[derive(Debug, Clone, Copy)]
struct JsonScanSummary {
    status: JsonScanStatus,
    notebook: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonRootState {
    Value,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonArrayState {
    FirstValueOrEnd,
    Value,
    CommaOrEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonObjectState {
    FirstKeyOrEnd,
    Key,
    Colon,
    Value,
    CommaOrEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonTopKey {
    Nbformat,
    Cells,
    Metadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonValueKind {
    Object,
    Array,
    String,
    Number,
    Other,
}

#[derive(Debug)]
enum JsonFrame {
    Array(JsonArrayState),
    Object { state: JsonObjectState, pending_key: Option<JsonTopKey> },
}

struct JsonParser {
    root: JsonRootState,
    frames: Vec<JsonFrame>,
    nbformat_number: bool,
    cells_array: bool,
    metadata_object: bool,
    memory: text::LogicalMemory,
}

impl JsonParser {
    fn new(context: &ExecutionContext) -> Result<Self, ConversionError> {
        Ok(Self {
            root: JsonRootState::Value,
            frames: Vec::new(),
            nbformat_number: false,
            cells_array: false,
            metadata_object: false,
            memory: text::LogicalMemory::new(context)?,
        })
    }

    fn consume_value(&mut self, kind: JsonValueKind) -> bool {
        let top_level = self.frames.len() == 1;
        let pending_key = if let Some(frame) = self.frames.last_mut() {
            match frame {
                JsonFrame::Array(state)
                    if matches!(state, JsonArrayState::FirstValueOrEnd | JsonArrayState::Value) =>
                {
                    *state = JsonArrayState::CommaOrEnd;
                    None
                }
                JsonFrame::Object { state, pending_key } if *state == JsonObjectState::Value => {
                    *state = JsonObjectState::CommaOrEnd;
                    pending_key.take()
                }
                _ => return false,
            }
        } else if self.root == JsonRootState::Value {
            self.root = JsonRootState::Complete;
            None
        } else {
            return false;
        };
        if top_level {
            match (pending_key, kind) {
                (Some(JsonTopKey::Nbformat), value) => {
                    self.nbformat_number = value == JsonValueKind::Number;
                }
                (Some(JsonTopKey::Cells), value) => {
                    self.cells_array = value == JsonValueKind::Array;
                }
                (Some(JsonTopKey::Metadata), value) => {
                    self.metadata_object = value == JsonValueKind::Object;
                }
                _ => {}
            }
        }
        true
    }

    fn push_container(&mut self, kind: JsonValueKind) -> Result<bool, ConversionError> {
        if !self.consume_value(kind) {
            return Ok(false);
        }
        let depth =
            self.frames.len().checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "json_scan_depth",
                detail: "JSON detector nesting depth overflowed".into(),
            })?;
        if depth > JSON_SCAN_DEPTH_LIMIT {
            return Err(ConversionError::ResourceLimit {
                limit: "json_scan_depth",
                detail: format!("JSON detector exceeds {JSON_SCAN_DEPTH_LIMIT} nested containers"),
            });
        }
        self.memory.reserve_vec(&mut self.frames, 1)?;
        self.frames.push(match kind {
            JsonValueKind::Object => {
                JsonFrame::Object { state: JsonObjectState::FirstKeyOrEnd, pending_key: None }
            }
            JsonValueKind::Array => JsonFrame::Array(JsonArrayState::FirstValueOrEnd),
            _ => return Ok(false),
        });
        Ok(true)
    }

    fn consume_string(&mut self, key: Option<JsonTopKey>) -> bool {
        let top_level = self.frames.len() == 1;
        if let Some(JsonFrame::Object { state, pending_key }) = self.frames.last_mut()
            && matches!(*state, JsonObjectState::FirstKeyOrEnd | JsonObjectState::Key)
        {
            *state = JsonObjectState::Colon;
            *pending_key = top_level.then_some(key).flatten();
            return true;
        }
        self.consume_value(JsonValueKind::String)
    }

    fn consume_colon(&mut self) -> bool {
        if let Some(JsonFrame::Object { state, .. }) = self.frames.last_mut()
            && *state == JsonObjectState::Colon
        {
            *state = JsonObjectState::Value;
            return true;
        }
        false
    }

    fn consume_comma(&mut self) -> bool {
        match self.frames.last_mut() {
            Some(JsonFrame::Array(state)) if *state == JsonArrayState::CommaOrEnd => {
                *state = JsonArrayState::Value;
                true
            }
            Some(JsonFrame::Object { state, .. }) if *state == JsonObjectState::CommaOrEnd => {
                *state = JsonObjectState::Key;
                true
            }
            _ => false,
        }
    }

    fn close_array(&mut self) -> bool {
        if !matches!(
            self.frames.last(),
            Some(JsonFrame::Array(JsonArrayState::FirstValueOrEnd | JsonArrayState::CommaOrEnd))
        ) {
            return false;
        }
        self.frames.pop();
        true
    }

    fn close_object(&mut self) -> bool {
        if !matches!(
            self.frames.last(),
            Some(JsonFrame::Object {
                state: JsonObjectState::FirstKeyOrEnd | JsonObjectState::CommaOrEnd,
                ..
            })
        ) {
            return false;
        }
        self.frames.pop();
        true
    }

    fn summary(&self, status: JsonScanStatus) -> JsonScanSummary {
        JsonScanSummary {
            status,
            notebook: status == JsonScanStatus::Complete
                && self.nbformat_number
                && self.cells_array
                && self.metadata_object,
        }
    }

    fn is_complete(&self) -> bool {
        self.root == JsonRootState::Complete && self.frames.is_empty()
    }
}

struct JsonScanBudget<'a> {
    context: &'a ExecutionContext,
    next_checkpoint: usize,
}

impl JsonScanBudget<'_> {
    fn checkpoint(&mut self, offset: usize) -> Result<(), ConversionError> {
        if offset >= self.next_checkpoint {
            self.context.checkpoint()?;
            self.next_checkpoint = offset.saturating_add(JSON_SCAN_CHECKPOINT_BYTES);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonLexeme {
    Complete(usize),
    Open,
    Invalid,
}

fn scan_json(bytes: &[u8], context: &ExecutionContext) -> Result<JsonScanSummary, ConversionError> {
    let mut parser = JsonParser::new(context)?;
    let mut budget = JsonScanBudget { context, next_checkpoint: 0 };
    let mut offset = 0_usize;
    while offset < bytes.len() {
        budget.checkpoint(offset)?;
        if matches!(bytes[offset], b' ' | b'\t' | b'\n' | b'\r') {
            offset += 1;
            continue;
        }
        let accepted = match bytes[offset] {
            b'{' => {
                offset += 1;
                parser.push_container(JsonValueKind::Object)?
            }
            b'[' => {
                offset += 1;
                parser.push_container(JsonValueKind::Array)?
            }
            b'}' => {
                offset += 1;
                parser.close_object()
            }
            b']' => {
                offset += 1;
                parser.close_array()
            }
            b':' => {
                offset += 1;
                parser.consume_colon()
            }
            b',' => {
                offset += 1;
                parser.consume_comma()
            }
            b'"' => match scan_json_string(bytes, offset, &mut budget)? {
                JsonLexeme::Complete(next) => {
                    let key = json_top_key(&bytes[offset + 1..next - 1]);
                    offset = next;
                    parser.consume_string(key)
                }
                JsonLexeme::Open => return Ok(parser.summary(JsonScanStatus::Open)),
                JsonLexeme::Invalid => return Ok(parser.summary(JsonScanStatus::Invalid)),
            },
            b'-' | b'0'..=b'9' => match scan_json_number(bytes, offset, &mut budget)? {
                JsonLexeme::Complete(next) => {
                    offset = next;
                    parser.consume_value(JsonValueKind::Number)
                }
                JsonLexeme::Open => return Ok(parser.summary(JsonScanStatus::Open)),
                JsonLexeme::Invalid => return Ok(parser.summary(JsonScanStatus::Invalid)),
            },
            b't' => match scan_json_literal(bytes, offset, b"true") {
                JsonLexeme::Complete(next) => {
                    offset = next;
                    parser.consume_value(JsonValueKind::Other)
                }
                JsonLexeme::Open => return Ok(parser.summary(JsonScanStatus::Open)),
                JsonLexeme::Invalid => return Ok(parser.summary(JsonScanStatus::Invalid)),
            },
            b'f' => match scan_json_literal(bytes, offset, b"false") {
                JsonLexeme::Complete(next) => {
                    offset = next;
                    parser.consume_value(JsonValueKind::Other)
                }
                JsonLexeme::Open => return Ok(parser.summary(JsonScanStatus::Open)),
                JsonLexeme::Invalid => return Ok(parser.summary(JsonScanStatus::Invalid)),
            },
            b'n' => match scan_json_literal(bytes, offset, b"null") {
                JsonLexeme::Complete(next) => {
                    offset = next;
                    parser.consume_value(JsonValueKind::Other)
                }
                JsonLexeme::Open => return Ok(parser.summary(JsonScanStatus::Open)),
                JsonLexeme::Invalid => return Ok(parser.summary(JsonScanStatus::Invalid)),
            },
            _ => false,
        };
        if !accepted {
            return Ok(parser.summary(JsonScanStatus::Invalid));
        }
    }
    Ok(parser.summary(if parser.is_complete() {
        JsonScanStatus::Complete
    } else {
        JsonScanStatus::Open
    }))
}

fn scan_json_string(
    bytes: &[u8],
    start: usize,
    budget: &mut JsonScanBudget<'_>,
) -> Result<JsonLexeme, ConversionError> {
    let mut offset = start + 1;
    while offset < bytes.len() {
        budget.checkpoint(offset)?;
        match bytes[offset] {
            b'"' => {
                return Ok(JsonLexeme::Complete(offset + 1));
            }
            b'\\' => {
                offset += 1;
                let Some(&escape) = bytes.get(offset) else {
                    return Ok(JsonLexeme::Open);
                };
                if escape == b'u' {
                    offset = match scan_json_unicode_escape(bytes, offset) {
                        Ok(next) => next,
                        Err(status) => return Ok(status),
                    };
                    continue;
                }
                if !matches!(escape, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') {
                    return Ok(JsonLexeme::Invalid);
                }
            }
            0x00..=0x1f => return Ok(JsonLexeme::Invalid),
            0x80..=0xff => {
                let width = match bytes[offset] {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Ok(JsonLexeme::Invalid),
                };
                let Some(encoded) = bytes.get(offset..offset + width) else {
                    return Ok(JsonLexeme::Open);
                };
                if std::str::from_utf8(encoded).is_err() {
                    return Ok(JsonLexeme::Invalid);
                }
                offset += width;
                continue;
            }
            _ => {}
        }
        offset += 1;
    }
    Ok(JsonLexeme::Open)
}

fn scan_json_unicode_escape(bytes: &[u8], u_offset: usize) -> Result<usize, JsonLexeme> {
    let first = parse_json_hex_quad(bytes, u_offset + 1)?;
    let next = u_offset + 5;
    if (0xdc00..=0xdfff).contains(&first) {
        return Err(JsonLexeme::Invalid);
    }
    if !(0xd800..=0xdbff).contains(&first) {
        return Ok(next);
    }
    if bytes.get(next..next + 2) != Some(b"\\u") {
        return Err(JsonLexeme::Invalid);
    }
    let low = parse_json_hex_quad(bytes, next + 2).map_err(|_| JsonLexeme::Invalid)?;
    if !(0xdc00..=0xdfff).contains(&low) {
        return Err(JsonLexeme::Invalid);
    }
    Ok(next + 6)
}

fn parse_json_hex_quad(bytes: &[u8], start: usize) -> Result<u16, JsonLexeme> {
    let Some(hex) = bytes.get(start..start + 4) else {
        return Err(
            if bytes.get(start..).is_some_and(|tail| tail.iter().all(u8::is_ascii_hexdigit)) {
                JsonLexeme::Open
            } else {
                JsonLexeme::Invalid
            },
        );
    };
    let mut value = 0_u16;
    for &digit in hex {
        let Some(nibble) = char::from(digit).to_digit(16) else {
            return Err(JsonLexeme::Invalid);
        };
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(u16::try_from(nibble).ok()?))
            .ok_or(JsonLexeme::Invalid)?;
    }
    Ok(value)
}

fn scan_json_number(
    bytes: &[u8],
    start: usize,
    budget: &mut JsonScanBudget<'_>,
) -> Result<JsonLexeme, ConversionError> {
    let mut offset = start;
    if bytes[offset] == b'-' {
        offset += 1;
        if offset == bytes.len() {
            return Ok(JsonLexeme::Open);
        }
    }
    match bytes[offset] {
        b'0' => {
            offset += 1;
            if bytes.get(offset).is_some_and(u8::is_ascii_digit) {
                return Ok(JsonLexeme::Invalid);
            }
        }
        b'1'..=b'9' => {
            offset += 1;
            while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
                budget.checkpoint(offset)?;
                offset += 1;
            }
        }
        _ => return Ok(JsonLexeme::Invalid),
    }
    if bytes.get(offset) == Some(&b'.') {
        offset += 1;
        if offset == bytes.len() {
            return Ok(JsonLexeme::Open);
        }
        if !bytes[offset].is_ascii_digit() {
            return Ok(JsonLexeme::Invalid);
        }
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            budget.checkpoint(offset)?;
            offset += 1;
        }
    }
    if bytes.get(offset).is_some_and(|byte| matches!(byte, b'e' | b'E')) {
        offset += 1;
        if bytes.get(offset).is_some_and(|byte| matches!(byte, b'+' | b'-')) {
            offset += 1;
        }
        if offset == bytes.len() {
            return Ok(JsonLexeme::Open);
        }
        if !bytes[offset].is_ascii_digit() {
            return Ok(JsonLexeme::Invalid);
        }
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            budget.checkpoint(offset)?;
            offset += 1;
        }
    }
    Ok(JsonLexeme::Complete(offset))
}

fn scan_json_literal(bytes: &[u8], start: usize, literal: &[u8]) -> JsonLexeme {
    let remaining = &bytes[start..];
    if remaining.len() < literal.len() {
        return if literal.starts_with(remaining) { JsonLexeme::Open } else { JsonLexeme::Invalid };
    }
    if remaining.starts_with(literal) {
        JsonLexeme::Complete(start + literal.len())
    } else {
        JsonLexeme::Invalid
    }
}

fn json_top_key(raw: &[u8]) -> Option<JsonTopKey> {
    match raw {
        b"nbformat" => Some(JsonTopKey::Nbformat),
        b"cells" => Some(JsonTopKey::Cells),
        b"metadata" => Some(JsonTopKey::Metadata),
        _ => None,
    }
}

fn bounded_utf8_prefix(bytes: &[u8], limit: usize) -> Option<(&str, bool)> {
    let bounded_len = bytes.len().min(limit);
    let bounded = bytes.get(..bounded_len)?;
    match std::str::from_utf8(bounded) {
        Ok(text) => Some((text, bytes.len() > bounded_len)),
        Err(error) if bytes.len() > bounded_len && error.error_len().is_none() => {
            let valid = bounded.get(..error.valid_up_to())?;
            std::str::from_utf8(valid).ok().map(|text| (text, true))
        }
        Err(_) => None,
    }
}

fn html_prelude_identifies_html(mut text: &str) -> bool {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix("<?") {
            let Some((_, after)) = rest.split_once("?>") else {
                return false;
            };
            text = after;
        } else if let Some(rest) = text.strip_prefix("<!--") {
            let Some((_, after)) = rest.split_once("-->") else {
                return false;
            };
            text = after;
        } else if let Some(rest) = strip_ascii_case_prefix(text, "<!doctype") {
            let Some((declaration, after)) = rest.split_once('>') else {
                return false;
            };
            if declaration
                .trim_start()
                .split(|character: char| character.is_ascii_whitespace() || character == '[')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("html"))
            {
                return true;
            }
            text = after;
        } else {
            return starts_with_tag(text, "html");
        }
    }
}

fn strip_ascii_case_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        .then(|| &value[prefix.len()..])
}

fn starts_with_tag(text: &str, name: &str) -> bool {
    text.starts_with('<')
        && text.get(1..1 + name.len()).is_some_and(|value| value.eq_ignore_ascii_case(name))
        && text
            .as_bytes()
            .get(1 + name.len())
            .is_some_and(|byte| matches!(byte, b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n'))
}

fn xml_root_name(mut text: &str) -> Option<&str> {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix("<?") {
            text = rest.split_once("?>")?.1;
        } else if let Some(rest) = text.strip_prefix("<!--") {
            text = rest.split_once("-->")?.1;
        } else if let Some(rest) = strip_ascii_case_prefix(text, "<!doctype") {
            text = rest.split_once('>')?.1;
        } else {
            break;
        }
    }
    let rest = text.strip_prefix('<')?;
    if rest.starts_with(['!', '?', '/']) {
        return None;
    }
    let end = rest.find(|character: char| {
        character.is_ascii_whitespace() || matches!(character, '>' | '/')
    })?;
    let qualified = &rest[..end];
    let local = qualified.rsplit(':').next()?;
    (!local.is_empty()
        && local.chars().next().is_some_and(|value| value.is_ascii_alphabetic())
        && local
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '_' | '-' | '.')))
    .then_some(local)
}

fn detect_zip(bytes: &[u8]) -> Vec<FormatCandidate> {
    let mut candidates = vec![FormatCandidate::new(InputFormat::Zip, 0.90, "ZIP magic bytes")];
    let entry_count = match zip_preflight(bytes) {
        Ok(entry_count) if entry_count <= ZIP_INSPECTION_ENTRY_LIMIT => entry_count,
        Ok(entry_count) => {
            candidates[0].diagnostics.push(format!(
                "ZIP inspection stopped before archive construction: {entry_count} entries exceed the {ZIP_INSPECTION_ENTRY_LIMIT} entry limit"
            ));
            return candidates;
        }
        Err(diagnostic) => {
            candidates[0].diagnostics.push(diagnostic);
            return candidates;
        }
    };
    let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(error) => {
            candidates[0]
                .diagnostics
                .push(format!("ZIP directory could not be inspected: {error}"));
            return candidates;
        }
    };
    if archive.len() != entry_count {
        candidates[0].diagnostics.push(format!(
            "ZIP entry count changed after validated EOCD preflight: {entry_count} != {}",
            archive.len()
        ));
        return candidates;
    }

    let mut names = Vec::with_capacity(archive.len());
    let mut name_bytes = 0_usize;
    for index in 0..archive.len() {
        match archive.by_index(index) {
            Ok(entry) => {
                name_bytes = name_bytes.saturating_add(entry.name().len());
                if name_bytes > ZIP_NAME_READ_LIMIT {
                    candidates[0].diagnostics.push(format!(
                        "ZIP inspection stopped: entry names exceed the {ZIP_NAME_READ_LIMIT} byte limit"
                    ));
                    return candidates;
                }
                names.push(entry.name().replace('\\', "/"));
            }
            Err(error) => candidates[0]
                .diagnostics
                .push(format!("ZIP entry {index} could not be inspected: {error}")),
        }
    }
    let specialized = inspect_zip_package(&mut archive, &names, &mut candidates[0].diagnostics);
    if specialized.len() == 1 {
        let (format, evidence) = specialized[0];
        candidates.push(FormatCandidate::new(format, 0.99, evidence));
    } else if specialized.len() > 1 {
        candidates[0].diagnostics.push(format!(
            "conflicting package structures detected: {}",
            specialized.iter().map(|(format, _)| format.as_str()).collect::<Vec<_>>().join(",")
        ));
    }
    candidates
}

fn inspect_zip_package(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    names: &[String],
    diagnostics: &mut Vec<String>,
) -> Vec<(InputFormat, &'static str)> {
    let mimetype = read_zip_text(archive, "mimetype", ZIP_MIMETYPE_READ_LIMIT, diagnostics);
    let mut matches = inspect_ooxml_package(archive, names, diagnostics);
    if mimetype.as_deref() == Some("application/epub+zip")
        && inspect_epub_package(archive, names, diagnostics)
    {
        matches.push((InputFormat::Epub, "validated EPUB mimetype, container, and rootfile"));
    }
    if let Some(candidate) = inspect_odf_package(archive, mimetype.as_deref(), diagnostics) {
        matches.push(candidate);
    }
    matches
}

fn inspect_ooxml_package(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    names: &[String],
    diagnostics: &mut Vec<String>,
) -> Vec<(InputFormat, &'static str)> {
    let content_types =
        read_zip_text(archive, "[Content_Types].xml", ZIP_METADATA_READ_LIMIT, diagnostics);
    let mut matches = Vec::new();
    let ooxml_content_types =
        content_types.as_deref().and_then(|document| match parse_ooxml_content_types(document) {
            Ok(content_types) => Some(content_types),
            Err(error) => {
                diagnostics.push(format!("ZIP [Content_Types].xml was rejected: {error}"));
                None
            }
        });
    if let Some(content_types) = ooxml_content_types.as_ref() {
        let ooxml = [
            (
                InputFormat::Docx,
                "word/document.xml",
                &[
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
                    "application/vnd.ms-word.document.macroEnabled.main+xml",
                ][..],
                "validated OOXML Word content type and package part",
            ),
            (
                InputFormat::Pptx,
                "ppt/presentation.xml",
                &[
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
                    "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml",
                    "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml",
                    "application/vnd.ms-powerpoint.slideshow.macroEnabled.main+xml",
                    "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml",
                    "application/vnd.ms-powerpoint.template.macroEnabled.main+xml",
                ][..],
                "validated OOXML presentation content type and package part",
            ),
            (
                InputFormat::Xlsx,
                "xl/workbook.xml",
                &[
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
                    "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
                ][..],
                "validated OOXML workbook content type and package part",
            ),
            (
                InputFormat::Xlsx,
                "xl/workbook.bin",
                &["application/vnd.ms-excel.sheet.binary.macroEnabled.main"][..],
                "validated OOXML binary workbook content type and package part",
            ),
        ];
        for (format, part, content_types_allowed, evidence) in ooxml {
            if names.iter().any(|name| name == part)
                && zip_entry_nonempty(archive, part, diagnostics)
                && content_types.for_part(part).is_some_and(|content_type| {
                    content_types_allowed.contains(&content_type.as_str())
                })
                && !matches.iter().any(|(existing, _)| *existing == format)
            {
                matches.push((format, evidence));
            }
        }
    }
    matches
}

fn inspect_epub_package(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    names: &[String],
    diagnostics: &mut Vec<String>,
) -> bool {
    let rootfile =
        read_zip_text(archive, "META-INF/container.xml", ZIP_METADATA_READ_LIMIT, diagnostics)
            .and_then(|document| match parse_epub_container(&document) {
                Ok(rootfile) => Some(rootfile),
                Err(error) => {
                    diagnostics.push(format!("ZIP META-INF/container.xml was rejected: {error}"));
                    None
                }
            });
    rootfile.is_some_and(|rootfile| {
        is_safe_archive_name(&rootfile)
            && names.iter().any(|name| name == &rootfile)
            && zip_entry_nonempty(archive, &rootfile, diagnostics)
    })
}

fn inspect_odf_package(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    mimetype: Option<&str>,
    diagnostics: &mut Vec<String>,
) -> Option<(InputFormat, &'static str)> {
    let odf = [
        (
            "application/vnd.oasis.opendocument.text",
            InputFormat::Odt,
            "validated OpenDocument text package",
        ),
        (
            "application/vnd.oasis.opendocument.spreadsheet",
            InputFormat::Ods,
            "validated OpenDocument spreadsheet package",
        ),
        (
            "application/vnd.oasis.opendocument.presentation",
            InputFormat::Odp,
            "validated OpenDocument presentation package",
        ),
    ];
    if let Some((expected_mimetype, format, evidence)) =
        odf.into_iter().find(|(expected, _, _)| mimetype == Some(*expected))
        && zip_entry_nonempty(archive, "content.xml", diagnostics)
        && let Some(manifest) =
            read_zip_text(archive, "META-INF/manifest.xml", ZIP_METADATA_READ_LIMIT, diagnostics)
    {
        match parse_odf_manifest(&manifest) {
            Ok(Some(media_type)) if media_type == expected_mimetype => {
                return Some((format, evidence));
            }
            Ok(_) => {}
            Err(error) => {
                diagnostics.push(format!("ZIP META-INF/manifest.xml was rejected: {error}"));
            }
        }
    }
    None
}

fn read_zip_text(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    limit: u64,
    diagnostics: &mut Vec<String>,
) -> Option<String> {
    let mut entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return None,
        Err(error) => {
            diagnostics.push(format!("ZIP {name} could not be opened: {error}"));
            return None;
        }
    };
    let mut raw = Vec::new();
    if let Err(error) = entry.by_ref().take(limit + 1).read_to_end(&mut raw) {
        diagnostics.push(format!("ZIP {name} could not be inspected: {error}"));
        return None;
    }
    if raw.len() as u64 > limit {
        diagnostics.push(format!("ZIP {name} exceeds the {limit} byte read limit"));
        return None;
    }
    if let Ok(value) = String::from_utf8(raw) {
        Some(value.trim().to_owned())
    } else {
        diagnostics.push(format!("ZIP {name} is not UTF-8"));
        None
    }
}

fn zip_entry_nonempty(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    diagnostics: &mut Vec<String>,
) -> bool {
    match archive.by_name(name) {
        Ok(mut entry) => {
            let mut byte = [0_u8; 1];
            entry.read(&mut byte).is_ok_and(|read| read == 1)
        }
        Err(zip::result::ZipError::FileNotFound) => false,
        Err(error) => {
            diagnostics.push(format!("ZIP {name} could not be opened: {error}"));
            false
        }
    }
}

const OOXML_CONTENT_TYPES_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/content-types";
const EPUB_CONTAINER_NAMESPACE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:container";
const ODF_MANIFEST_NAMESPACE: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";

#[derive(Debug, Clone, PartialEq, Eq)]
struct XmlName {
    namespace: Option<Vec<u8>>,
    local: Vec<u8>,
}

impl XmlName {
    fn matches(&self, namespace: &[u8], local: &[u8]) -> bool {
        self.namespace.as_deref() == Some(namespace) && self.local == local
    }
}

#[derive(Debug)]
struct XmlAttribute {
    namespace: Option<Vec<u8>>,
    local: Vec<u8>,
    value: String,
}

#[derive(Debug, Default)]
struct OoxmlContentTypes {
    defaults: BTreeMap<String, String>,
    overrides: BTreeMap<String, String>,
}

impl OoxmlContentTypes {
    fn for_part(&self, part: &str) -> Option<&String> {
        self.overrides.get(&format!("/{part}")).or_else(|| {
            let extension = part.rsplit_once('.')?.1.to_ascii_lowercase();
            self.defaults.get(&extension)
        })
    }
}

fn parse_ooxml_content_types(document: &str) -> Result<OoxmlContentTypes, String> {
    let mut root_valid = false;
    let mut content_types = OoxmlContentTypes::default();
    parse_package_xml(document, |depth, name, parent, attributes| {
        if depth == 1 {
            if !name.matches(OOXML_CONTENT_TYPES_NAMESPACE, b"Types") {
                return Err("expected the OOXML Types root and namespace".into());
            }
            root_valid = true;
        } else if name.matches(OOXML_CONTENT_TYPES_NAMESPACE, b"Override") {
            if depth != 2
                || !parent
                    .is_some_and(|parent| parent.matches(OOXML_CONTENT_TYPES_NAMESPACE, b"Types"))
            {
                return Err("OOXML Override is not a direct child of Types".into());
            }
            let part = required_xml_attribute(attributes, None, b"PartName")?.to_owned();
            let content_type = required_xml_attribute(attributes, None, b"ContentType")?.to_owned();
            if content_types.overrides.insert(part, content_type).is_some() {
                return Err("duplicate OOXML Override PartName".into());
            }
        } else if name.matches(OOXML_CONTENT_TYPES_NAMESPACE, b"Default") {
            if depth != 2
                || !parent
                    .is_some_and(|parent| parent.matches(OOXML_CONTENT_TYPES_NAMESPACE, b"Types"))
            {
                return Err("OOXML Default is not a direct child of Types".into());
            }
            let extension =
                required_xml_attribute(attributes, None, b"Extension")?.to_ascii_lowercase();
            let content_type = required_xml_attribute(attributes, None, b"ContentType")?.to_owned();
            if extension.is_empty()
                || extension.starts_with('.')
                || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
                || content_types.defaults.insert(extension, content_type).is_some()
            {
                return Err("duplicate or invalid OOXML Default Extension".into());
            }
        }
        Ok(())
    })?;
    if !root_valid {
        return Err("OOXML Types root is missing".into());
    }
    Ok(content_types)
}

fn parse_epub_container(document: &str) -> Result<String, String> {
    let mut root_valid = false;
    let mut rootfiles_seen = false;
    let mut rootfile = None;
    parse_package_xml(document, |depth, name, parent, attributes| {
        if depth == 1 {
            if !name.matches(EPUB_CONTAINER_NAMESPACE, b"container") {
                return Err("expected the EPUB container root and namespace".into());
            }
            root_valid = true;
        } else if name.matches(EPUB_CONTAINER_NAMESPACE, b"rootfiles") {
            if depth != 2
                || !parent
                    .is_some_and(|parent| parent.matches(EPUB_CONTAINER_NAMESPACE, b"container"))
            {
                return Err("EPUB rootfiles is not a direct child of container".into());
            }
            if rootfiles_seen {
                return Err("duplicate EPUB rootfiles element".into());
            }
            rootfiles_seen = true;
        } else if name.matches(EPUB_CONTAINER_NAMESPACE, b"rootfile") {
            if depth != 3
                || !parent
                    .is_some_and(|parent| parent.matches(EPUB_CONTAINER_NAMESPACE, b"rootfiles"))
            {
                return Err("EPUB rootfile is not a direct child of rootfiles".into());
            }
            let path = required_xml_attribute(attributes, None, b"full-path")?.to_owned();
            if rootfile.replace(path).is_some() {
                return Err("duplicate EPUB rootfile element".into());
            }
        }
        Ok(())
    })?;
    if !root_valid || !rootfiles_seen {
        return Err("EPUB container structure is incomplete".into());
    }
    rootfile.ok_or_else(|| "EPUB rootfile is missing".into())
}

fn parse_odf_manifest(document: &str) -> Result<Option<String>, String> {
    let mut root_valid = false;
    let mut package_media_type = None;
    parse_package_xml(document, |depth, name, parent, attributes| {
        if depth == 1 {
            if !name.matches(ODF_MANIFEST_NAMESPACE, b"manifest") {
                return Err("expected the ODF manifest root and namespace".into());
            }
            root_valid = true;
        } else if name.matches(ODF_MANIFEST_NAMESPACE, b"file-entry") {
            if depth != 2
                || !parent.is_some_and(|parent| parent.matches(ODF_MANIFEST_NAMESPACE, b"manifest"))
            {
                return Err("ODF file-entry is not a direct child of manifest".into());
            }
            let path =
                required_xml_attribute(attributes, Some(ODF_MANIFEST_NAMESPACE), b"full-path")?;
            if path == "/" {
                let media_type = required_xml_attribute(
                    attributes,
                    Some(ODF_MANIFEST_NAMESPACE),
                    b"media-type",
                )?
                .to_owned();
                if package_media_type.replace(media_type).is_some() {
                    return Err("duplicate ODF package file-entry".into());
                }
            }
        }
        Ok(())
    })?;
    if !root_valid {
        return Err("ODF manifest root is missing".into());
    }
    Ok(package_media_type)
}

fn parse_package_xml(
    document: &str,
    mut inspect: impl FnMut(usize, &XmlName, Option<&XmlName>, &[XmlAttribute]) -> Result<(), String>,
) -> Result<(), String> {
    let mut reader = NsReader::from_str(document);
    let config = reader.config_mut();
    config.allow_dangling_amp = false;
    config.allow_unmatched_ends = false;
    config.check_end_names = true;
    config.check_comments = true;
    let mut ancestors = Vec::new();
    let mut root_seen = false;
    let mut events = 0_usize;
    loop {
        events += 1;
        if events > ZIP_XML_EVENT_LIMIT {
            return Err(format!("XML exceeds the {ZIP_XML_EVENT_LIMIT} event limit"));
        }
        let event = reader.read_event().map_err(|error| format!("invalid XML: {error}"))?;
        match event {
            Event::Start(element) => {
                if ancestors.is_empty() && root_seen {
                    return Err("XML contains multiple root elements".into());
                }
                let name = xml_name(&reader, &element)?;
                let attributes = checked_xml_attributes(&reader, &element)?;
                inspect(ancestors.len() + 1, &name, ancestors.last(), &attributes)?;
                if ancestors.is_empty() {
                    root_seen = true;
                }
                ancestors.push(name);
            }
            Event::Empty(element) => {
                if ancestors.is_empty() && root_seen {
                    return Err("XML contains multiple root elements".into());
                }
                let name = xml_name(&reader, &element)?;
                let attributes = checked_xml_attributes(&reader, &element)?;
                inspect(ancestors.len() + 1, &name, ancestors.last(), &attributes)?;
                if ancestors.is_empty() {
                    root_seen = true;
                }
            }
            Event::End(element) => {
                let expected = ancestors.pop().ok_or("XML end tag has no open element")?;
                let (namespace, local) = reader.resolve_element(element.name());
                let actual = XmlName {
                    namespace: owned_xml_namespace(namespace)?,
                    local: local.as_ref().to_vec(),
                };
                if actual != expected {
                    return Err("XML end tag namespace does not match its start tag".into());
                }
            }
            Event::Text(text)
                if ancestors.is_empty() && !text.iter().all(u8::is_ascii_whitespace) =>
            {
                return Err("XML has character data outside its root".into());
            }
            Event::CData(text) if ancestors.is_empty() && !text.is_empty() => {
                return Err("XML has CDATA outside its root".into());
            }
            Event::DocType(_) | Event::GeneralRef(_) => {
                return Err("DTD and entity references are not allowed".into());
            }
            Event::Eof => break,
            Event::Text(_)
            | Event::CData(_)
            | Event::Comment(_)
            | Event::Decl(_)
            | Event::PI(_) => {}
        }
    }
    if !root_seen || !ancestors.is_empty() {
        return Err("XML root is missing or incomplete".into());
    }
    Ok(())
}

fn xml_name(reader: &NsReader<&[u8]>, element: &BytesStart<'_>) -> Result<XmlName, String> {
    let (namespace, local) = reader.resolve_element(element.name());
    Ok(XmlName { namespace: owned_xml_namespace(namespace)?, local: local.as_ref().to_vec() })
}

fn checked_xml_attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<Vec<XmlAttribute>, String> {
    let mut attributes = Vec::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("invalid XML attribute: {error}"))?;
        if attribute.value.contains(&b'&') {
            return Err("entity references in XML attributes are not allowed".into());
        }
        let raw_name = attribute.key.as_ref();
        if raw_name == b"xmlns" || raw_name.starts_with(b"xmlns:") {
            continue;
        }
        let (namespace, local) = reader.resolve_attribute(attribute.key);
        let value = std::str::from_utf8(attribute.value.as_ref())
            .map_err(|_| "XML attribute is not UTF-8")?
            .to_owned();
        attributes.push(XmlAttribute {
            namespace: owned_xml_namespace(namespace)?,
            local: local.as_ref().to_vec(),
            value,
        });
    }
    Ok(attributes)
}

fn owned_xml_namespace(namespace: ResolveResult<'_>) -> Result<Option<Vec<u8>>, String> {
    match namespace {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(namespace) => Ok(Some(namespace.as_ref().to_vec())),
        ResolveResult::Unknown(prefix) => Err(format!(
            "XML namespace prefix is not declared: {}",
            String::from_utf8_lossy(&prefix)
        )),
    }
}

fn required_xml_attribute<'a>(
    attributes: &'a [XmlAttribute],
    namespace: Option<&[u8]>,
    local: &[u8],
) -> Result<&'a str, String> {
    let mut values = attributes.iter().filter(|attribute| {
        attribute.namespace.as_deref() == namespace && attribute.local == local
    });
    let value = values.next().ok_or_else(|| {
        format!("required XML attribute {} is missing", String::from_utf8_lossy(local))
    })?;
    if values.next().is_some() {
        return Err(format!("duplicate XML attribute {}", String::from_utf8_lossy(local)));
    }
    Ok(&value.value)
}

fn is_safe_archive_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && name.split('/').all(|component| !matches!(component, "" | "." | ".."))
}

fn zip_preflight(bytes: &[u8]) -> Result<usize, String> {
    const EOCD_MIN_SIZE: usize = 22;
    const MAX_COMMENT_SIZE: usize = u16::MAX as usize;
    let search_start = bytes.len().saturating_sub(EOCD_MIN_SIZE + MAX_COMMENT_SIZE);
    let mut last_error = "ZIP EOCD record is missing or invalid".to_owned();
    for relative in (0..=bytes.len().saturating_sub(search_start + 4)).rev() {
        let eocd = search_start + relative;
        if bytes.get(eocd..eocd + 4) != Some(b"PK\x05\x06") {
            continue;
        }
        let Some(record) = bytes.get(eocd..eocd + EOCD_MIN_SIZE) else {
            continue;
        };
        let comment_size = usize::from(u16::from_le_bytes([record[20], record[21]]));
        if eocd.checked_add(EOCD_MIN_SIZE + comment_size) != Some(bytes.len()) {
            continue;
        }
        match validate_classic_eocd(bytes, eocd, record) {
            Ok(entry_count) => return Ok(entry_count),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn validate_classic_eocd(bytes: &[u8], eocd: usize, record: &[u8]) -> Result<usize, String> {
    let disk = u16::from_le_bytes([record[4], record[5]]);
    let central_disk = u16::from_le_bytes([record[6], record[7]]);
    let disk_entries = u16::from_le_bytes([record[8], record[9]]);
    let total_entries = u16::from_le_bytes([record[10], record[11]]);
    let central_size = u32::from_le_bytes(record[12..16].try_into().map_err(|_| "invalid EOCD")?);
    let central_offset = u32::from_le_bytes(record[16..20].try_into().map_err(|_| "invalid EOCD")?);
    if disk != 0 || central_disk != 0 || disk_entries != total_entries {
        return Err("multi-disk ZIP structure inspection is unsupported".into());
    }
    if total_entries == u16::MAX
        || central_size == u32::MAX
        || central_offset == u32::MAX
        || eocd >= 20 && bytes.get(eocd - 20..eocd - 16) == Some(b"PK\x06\x07")
    {
        return Err("ZIP64 structure inspection is unsupported; using ZIP-only evidence".into());
    }
    let entry_count = usize::from(total_entries);
    let central_size =
        usize::try_from(central_size).map_err(|_| "ZIP central size is too large")?;
    let central_offset =
        usize::try_from(central_offset).map_err(|_| "ZIP central offset is too large")?;
    if central_offset.checked_add(central_size) != Some(eocd) {
        return Err("ZIP central directory does not end at the validated EOCD".into());
    }
    if entry_count > ZIP_INSPECTION_ENTRY_LIMIT {
        return Ok(entry_count);
    }
    if central_size > ZIP_CENTRAL_DIRECTORY_LIMIT {
        return Err(format!(
            "ZIP central directory exceeds the {ZIP_CENTRAL_DIRECTORY_LIMIT} byte limit before archive construction"
        ));
    }
    let mut cursor = central_offset;
    let mut name_bytes = 0_usize;
    for _ in 0..entry_count {
        let header =
            bytes.get(cursor..cursor + 46).ok_or("ZIP central directory header is truncated")?;
        if &header[..4] != b"PK\x01\x02" {
            return Err("ZIP central directory entry signature is invalid".into());
        }
        let name_size = usize::from(u16::from_le_bytes([header[28], header[29]]));
        name_bytes = name_bytes
            .checked_add(name_size)
            .filter(|total| *total <= ZIP_NAME_READ_LIMIT)
            .ok_or_else(|| {
                format!(
                    "ZIP entry names exceed the {ZIP_NAME_READ_LIMIT} byte limit before archive construction"
                )
            })?;
        let variable_size = name_size
            + usize::from(u16::from_le_bytes([header[30], header[31]]))
            + usize::from(u16::from_le_bytes([header[32], header[33]]));
        cursor = cursor
            .checked_add(46 + variable_size)
            .filter(|end| *end <= eocd)
            .ok_or("ZIP central directory entry exceeds the EOCD boundary")?;
    }
    if cursor != eocd {
        return Err("ZIP central directory count or size is inconsistent".into());
    }
    Ok(entry_count)
}

fn detect_ole(bytes: &[u8]) -> Vec<FormatCandidate> {
    let streams = [
        (InputFormat::Doc, "WordDocument"),
        (InputFormat::Xls, "Workbook"),
        (InputFormat::Xls, "Book"),
        (InputFormat::Ppt, "PowerPoint Document"),
        (InputFormat::OutlookMsg, "__properties_version1.0"),
    ];
    let directory_names = match cfb_directory_stream_names(bytes) {
        Ok(names) => names,
        Err(diagnostic) => {
            return [InputFormat::Doc, InputFormat::Xls, InputFormat::Ppt, InputFormat::OutlookMsg]
                .into_iter()
                .map(|format| {
                    FormatCandidate::new(format, 0.20, "OLE compound file signature")
                        .with_diagnostic(diagnostic.clone())
                })
                .collect();
        }
    };
    let mut candidates = Vec::new();
    for (format, stream) in streams {
        if directory_names.iter().any(|name| name == stream) {
            candidates.push(FormatCandidate::new(
                format,
                0.98,
                format!("OLE compound file stream {stream}"),
            ));
        }
    }
    candidates.sort_by_key(|candidate| candidate.format);
    candidates.dedup_by_key(|candidate| candidate.format);
    candidates
}

const CFB_FREE_SECTOR: u32 = 0xffff_ffff;
const CFB_END_OF_CHAIN: u32 = 0xffff_fffe;

#[allow(clippy::too_many_lines)]
fn cfb_directory_stream_names(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.len() < 512 {
        return Err("CFB header is truncated".into());
    }
    if bytes[..8] != [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1] {
        return Err("CFB signature is invalid".into());
    }
    if read_u16(bytes, 28)? != 0xfffe {
        return Err("CFB byte order is invalid".into());
    }
    let major = read_u16(bytes, 26)?;
    let sector_shift = read_u16(bytes, 30)?;
    if !matches!((major, sector_shift), (3, 9) | (4, 12)) {
        return Err("CFB major version or sector size is invalid".into());
    }
    if read_u16(bytes, 32)? != 6 {
        return Err("CFB mini-sector size is invalid".into());
    }
    let sector_size = 1_usize << sector_shift;
    let inspected_len = bytes.len().min(OLE_INSPECTION_BYTE_LIMIT);
    let sector_count = inspected_len.saturating_sub(sector_size) / sector_size;
    if sector_count == 0 {
        return Err("CFB contains no complete sectors within the inspection limit".into());
    }
    let fat_sector_count =
        usize::try_from(read_u32(bytes, 44)?).map_err(|_| "CFB FAT count is too large")?;
    let max_fat_sectors = sector_count.min(OLE_INSPECTION_BYTE_LIMIT / sector_size);
    if fat_sector_count == 0 || fat_sector_count > max_fat_sectors {
        return Err("CFB FAT sector count exceeds the inspection limit".into());
    }

    let mut fat_sector_ids = Vec::with_capacity(fat_sector_count);
    for offset in (76..512).step_by(4) {
        let sector = read_u32(bytes, offset)?;
        if sector != CFB_FREE_SECTOR {
            fat_sector_ids.push(sector);
            if fat_sector_ids.len() == fat_sector_count {
                break;
            }
        }
    }
    let difat_sector_count =
        usize::try_from(read_u32(bytes, 72)?).map_err(|_| "CFB DIFAT count is too large")?;
    if difat_sector_count > sector_count {
        return Err("CFB DIFAT sector count exceeds the inspection limit".into());
    }
    let mut difat_sector = read_u32(bytes, 68)?;
    let mut seen_difat = std::collections::BTreeSet::new();
    for _ in 0..difat_sector_count {
        if difat_sector == CFB_END_OF_CHAIN || !seen_difat.insert(difat_sector) {
            return Err("CFB DIFAT chain is truncated or cyclic".into());
        }
        let sector = cfb_sector(bytes, difat_sector, sector_size, inspected_len)?;
        for offset in (0..sector_size - 4).step_by(4) {
            let fat_sector = read_u32(sector, offset)?;
            if fat_sector != CFB_FREE_SECTOR {
                fat_sector_ids.push(fat_sector);
                if fat_sector_ids.len() == fat_sector_count {
                    break;
                }
            }
        }
        difat_sector = read_u32(sector, sector_size - 4)?;
    }
    if fat_sector_ids.len() != fat_sector_count {
        return Err("CFB DIFAT does not reference the declared FAT sectors".into());
    }

    let mut fat = Vec::with_capacity(fat_sector_count * sector_size / 4);
    for sector_id in fat_sector_ids {
        let sector = cfb_sector(bytes, sector_id, sector_size, inspected_len)?;
        fat.extend(
            sector
                .chunks_exact(4)
                .map(|value| u32::from_le_bytes([value[0], value[1], value[2], value[3]])),
        );
    }
    let first_directory_sector = read_u32(bytes, 48)?;
    let mut directory_sector = first_directory_sector;
    let mut seen_directory = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    while directory_sector != CFB_END_OF_CHAIN {
        if !seen_directory.insert(directory_sector) || seen_directory.len() > sector_count {
            return Err("CFB directory chain is cyclic or exceeds the inspection limit".into());
        }
        let sector = cfb_sector(bytes, directory_sector, sector_size, inspected_len)?;
        entries.extend(sector.chunks_exact(128));
        directory_sector = *fat
            .get(
                usize::try_from(directory_sector)
                    .map_err(|_| "CFB directory sector is too large")?,
            )
            .ok_or("CFB directory sector has no FAT entry")?;
    }
    cfb_root_stream_names(&entries)
}

fn cfb_root_stream_names(entries: &[&[u8]]) -> Result<Vec<String>, String> {
    let root = entries.first().ok_or("CFB directory has no root storage")?;
    if root[66] != 5 {
        return Err("CFB directory does not begin with root storage".into());
    }
    let mut pending = vec![read_u32(root, 76)?];
    let mut visited = std::collections::BTreeSet::new();
    let mut names = Vec::new();
    while let Some(index) = pending.pop() {
        if index == CFB_FREE_SECTOR {
            continue;
        }
        if !visited.insert(index) {
            return Err("CFB root directory tree is cyclic or aliased".into());
        }
        let entry = entries.get(index as usize).ok_or("CFB root child is out of range")?;
        if !matches!(entry[66], 1 | 2) {
            return Err("CFB root child has an invalid entry type".into());
        }
        pending.push(read_u32(entry, 68)?);
        pending.push(read_u32(entry, 72)?);
        // Follow siblings, never a storage's child tree: embedded streams cannot
        // supply the enclosing document's format authority.
        if entry[66] == 2 {
            let name_bytes = usize::from(read_u16(entry, 64)?);
            if !(2..=64).contains(&name_bytes) || name_bytes % 2 != 0 {
                return Err("CFB directory stream contains an invalid stream name".into());
            }
            let name = entry[..name_bytes - 2]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
            names.push(
                char::decode_utf16(name)
                    .collect::<Result<String, _>>()
                    .map_err(|_| "CFB directory stream contains invalid UTF-16")?,
            );
        }
    }
    Ok(names)
}

fn cfb_sector(
    bytes: &[u8],
    sector_id: u32,
    sector_size: usize,
    inspected_len: usize,
) -> Result<&[u8], String> {
    let sector_id = usize::try_from(sector_id).map_err(|_| "CFB sector ID is too large")?;
    let start = sector_id
        .checked_add(1)
        .and_then(|value| value.checked_mul(sector_size))
        .ok_or("CFB sector offset overflows")?;
    let end = start.checked_add(sector_size).ok_or("CFB sector end overflows")?;
    if end > inspected_len {
        return Err(format!(
            "CFB sector falls outside the {OLE_INSPECTION_BYTE_LIMIT} byte inspection limit or input"
        ));
    }
    bytes.get(start..end).ok_or_else(|| "CFB sector is truncated".into())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let value = bytes.get(offset..offset + 2).ok_or("CFB structure is truncated")?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes.get(offset..offset + 4).ok_or("CFB structure is truncated")?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn extension_of(name: &str) -> Option<&str> {
    Path::new(name).extension().and_then(|value| value.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn execution_context() -> ExecutionContext {
        ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        )
    }

    fn detect(bytes: &[u8]) -> Vec<FormatCandidate> {
        detect_content(bytes, &execution_context()).unwrap()
    }

    fn structured(bytes: &[u8]) -> Option<FormatCandidate> {
        structured_text_candidate(bytes, &execution_context()).unwrap()
    }

    #[test]
    fn blocking_worker_wait_is_deadline_interruptible() {
        let pool = BlockingPool::new("test-blocking", 1, 1, "test_blocking_queue").unwrap();
        let (started_sender, started_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let future = pool
            .submit(move || {
                let _ = started_sender.send(());
                let _ = release_receiver.recv();
                Ok(())
            })
            .unwrap();
        started_receiver.recv().unwrap();
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions {
                timeout: Some(Duration::from_millis(20)),
                ..into_markdown_core::ExecutionOptions::default()
            },
            into_markdown_core::ResourceLimits::default(),
        );
        let start = Instant::now();
        let error = block_on(context.run(future)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Timeout);
        assert!(start.elapsed() < Duration::from_millis(500));
        release_sender.send(()).unwrap();
    }

    #[test]
    fn blocking_worker_overload_has_a_stable_resource_error() {
        let pool = BlockingPool::new("test-overload", 1, 1, "test_blocking_queue").unwrap();
        let (started_sender, started_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let running = pool
            .submit(move || {
                let _ = started_sender.send(());
                let _ = release_receiver.recv();
                Ok(())
            })
            .unwrap();
        started_receiver.recv().unwrap();
        let queued = pool.submit(|| Ok(())).unwrap();
        let error = pool.submit(|| Ok(())).err().unwrap();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
        assert!(error.to_string().contains("test_blocking_queue"));
        drop(running);
        drop(queued);
        release_sender.send(()).unwrap();
    }

    #[test]
    fn blocking_worker_panic_is_a_stable_internal_error() {
        let pool = BlockingPool::new("test-panic", 1, 1, "test_blocking_queue").unwrap();
        let future = pool.submit::<(), _>(|| panic!("untrusted worker panic")).unwrap();
        let error = block_on(future).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Internal);
    }

    #[test]
    fn abandoned_blocking_result_is_dropped_after_worker_returns() {
        struct DropSignal(Arc<AtomicUsize>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }

        let pool = BlockingPool::new("test-abandon", 1, 1, "test_blocking_queue").unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let worker_dropped = Arc::clone(&dropped);
        let (started_sender, started_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let future = pool
            .submit(move || {
                let _ = started_sender.send(());
                let _ = release_receiver.recv();
                Ok(DropSignal(worker_dropped))
            })
            .unwrap();
        started_receiver.recv().unwrap();
        drop(future);
        release_sender.send(()).unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while dropped.load(Ordering::Acquire) == 0 && Instant::now() < limit {
            std::thread::yield_now();
        }
        assert_eq!(dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn abandoned_blocking_source_releases_its_memory_after_worker_returns() {
        let pool = BlockingPool::new("test-abandon-memory", 1, 1, "test_queue").unwrap();
        let resource_context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 8,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let worker_context = resource_context.clone();
        let (started_sender, started_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let future = pool
            .submit(move || {
                let reservation = worker_context.reserve_memory(8)?;
                let _ = started_sender.send(());
                let _ = release_receiver.recv();
                Ok(reservation)
            })
            .unwrap();
        started_receiver.recv().unwrap();
        let wait_context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions {
                timeout: Some(Duration::from_millis(20)),
                ..into_markdown_core::ExecutionOptions::default()
            },
            into_markdown_core::ResourceLimits::default(),
        );
        assert_eq!(
            block_on(wait_context.run(future)).unwrap_err().code(),
            into_markdown_core::ErrorCode::Timeout
        );
        assert_eq!(
            resource_context.reserve_memory(1).unwrap_err().code(),
            into_markdown_core::ErrorCode::ResourceLimit
        );
        release_sender.send(()).unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(reservation) = resource_context.reserve_memory(8) {
                drop(reservation);
                break;
            }
            assert!(Instant::now() < limit, "worker did not release abandoned source memory");
            std::thread::yield_now();
        }
    }

    struct EndlessReader {
        read: Arc<AtomicUsize>,
    }

    impl Read for EndlessReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            buffer.fill(b'x');
            self.read.fetch_add(buffer.len(), Ordering::AcqRel);
            Ok(buffer.len())
        }
    }

    #[test]
    fn growing_source_reads_at_most_input_limit_plus_one() {
        let read = Arc::new(AtomicUsize::new(0));
        let mut reader = EndlessReader { read: Arc::clone(&read) };
        let context = execution_context();
        let error = read_bounded(&mut reader, 65_537, &context).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
        assert_eq!(read.load(Ordering::Acquire), 65_538);
    }

    struct CountReads(AtomicUsize);

    impl Read for CountReads {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(0)
        }
    }

    #[test]
    fn source_scratch_is_reserved_before_reader_or_large_buffer_use() {
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 1,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let mut reader = CountReads(AtomicUsize::new(0));
        let error = read_bounded(&mut reader, 128 * 1024, &context).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
        assert_eq!(reader.0.load(Ordering::Acquire), 0);
        assert!(context.reserve_memory(1).is_ok());
    }

    #[test]
    fn exact_two_payload_budget_covers_arc_peak_after_scratch_refund() {
        const SIZE: usize = 100_000;
        const READ_PEAK: u64 = SIZE as u64 + 64 * 1024;
        let too_small = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: READ_PEAK,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let (bytes, memory) =
            read_bounded(&mut Cursor::new(vec![b'x'; SIZE]), SIZE as u64, &too_small).unwrap();
        let error = into_shared_source(bytes, memory).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
        assert!(too_small.reserve_memory(READ_PEAK).is_ok());

        let enough = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: (SIZE as u64) * 2,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let (bytes, memory) =
            read_bounded(&mut Cursor::new(vec![b'x'; SIZE]), SIZE as u64, &enough).unwrap();
        let (shared, memory) = into_shared_source(bytes, memory).unwrap();
        assert_eq!(shared.len(), SIZE);
        let remainder = enough.reserve_memory(SIZE as u64).unwrap();
        drop(remainder);
        assert_eq!(
            enough.reserve_memory(SIZE as u64 + 1).unwrap_err().code(),
            into_markdown_core::ErrorCode::ResourceLimit
        );
        drop(memory);
        drop(shared);
        assert!(enough.reserve_memory((SIZE as u64) * 2).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn secure_open_refuses_a_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "into-md-secure-open-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let planned = root.join("planned.txt");
        let target = root.join("target.txt");
        std::fs::write(&planned, b"planned").unwrap();
        std::fs::write(&target, b"target").unwrap();
        std::fs::remove_file(&planned).unwrap();
        symlink(&target, &planned).unwrap();
        let error = securely_open_local_file(&planned).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Io);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_authoritative_handle_is_stable_across_regular_path_replacement() {
        let root = std::env::temp_dir().join(format!(
            "into-md-windows-identity-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let expected_path = root.join("planned.txt");
        let replacement_path = root.join("replacement.txt");
        std::fs::write(&expected_path, b"expected").unwrap();
        std::fs::write(&replacement_path, b"replacement").unwrap();
        let (mut authoritative, _) = securely_open_local_file(&expected_path).unwrap();
        std::fs::remove_file(&expected_path).unwrap();
        std::fs::rename(&replacement_path, &expected_path).unwrap();
        let mut contents = Vec::new();
        authoritative.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"expected");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_policy_allows_long_disk_paths_and_rejects_devices() {
        for allowed in
            [r"C:\safe\input.txt", r"\\?\C:\safe\input.txt", r"\\?\UNC\server\share\input.txt"]
        {
            validate_windows_local_path(Path::new(allowed)).unwrap();
        }
        for denied in [
            r"\\.\NUL",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\input.txt",
            r"\??\C:\input.txt",
            r"C:\safe\NUL.txt",
            r"C:\safe\NUL .txt",
            r"C:\safe\COM1.md",
            "C:\\safe\\lpt¹.txt",
        ] {
            let error = validate_windows_local_path(Path::new(denied)).unwrap_err();
            assert_eq!(error.code(), into_markdown_core::ErrorCode::Io, "{denied}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_reserved_device_is_rejected_before_open() {
        let error = securely_open_local_file(Path::new("NUL")).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Io);
        assert!(error.to_string().contains("denied Windows device path"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_non_disk_handle_is_rejected_even_if_metadata_looks_regular() {
        let error =
            validate_windows_opened_file(Path::new("input.txt"), false, 0, true).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Io);
    }

    #[test]
    fn memory_resolver_enforces_input_budget() {
        let resolver = MemorySourceResolver;
        let input = InputRef::bytes(b"large".as_slice(), Some("x.txt"));
        let mut options = ConversionOptions::default();
        options.limits.max_input_bytes = 2;
        let context = execution_context();
        let error = block_on(resolver.resolve(&input, &options, &context)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
    }

    #[test]
    fn memory_resolver_charges_shared_arc_without_copying_it() {
        let resolver = MemorySourceResolver;
        let data = Arc::<[u8]>::from(b"large".as_slice());
        let input = InputRef::Bytes { data: Arc::clone(&data), name: Some("x.txt".into()) };
        let options = ConversionOptions::default();
        let too_small = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 4,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let error = block_on(resolver.resolve(&input, &options, &too_small)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);

        let exact = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 5,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let output = block_on(resolver.resolve_accounted(&input, &options, &exact)).unwrap();
        assert!(Arc::ptr_eq(&data, &output.input().bytes));
        assert_eq!(
            exact.reserve_memory(1).unwrap_err().code(),
            into_markdown_core::ErrorCode::ResourceLimit
        );
        drop(output);
        assert!(exact.reserve_memory(5).is_ok());
    }

    #[test]
    fn remote_resolution_is_disabled_by_default() {
        let resolver = UriSourceResolver::default();
        let input = InputRef::Uri("https://example.com/a.pdf".into());
        let options = ConversionOptions::default();
        let context = execution_context();
        let error = block_on(resolver.resolve(&input, &options, &context)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Network);
    }

    fn resolved(bytes: Vec<u8>, name: &str) -> ResolvedInput {
        let size = bytes.len() as u64;
        ResolvedInput {
            bytes: Arc::from(bytes),
            metadata: SourceMetadata { name: Some(name.into()), size, ..SourceMetadata::default() },
        }
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in entries {
            archive.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            archive.write_all(contents).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }

    fn ooxml_content_type(part: &str, content_type: &str) -> Vec<u8> {
        format!(
            r#"<ct:Types xmlns:ct="http://schemas.openxmlformats.org/package/2006/content-types"><ct:Override PartName="/{part}" ContentType="{content_type}"/></ct:Types>"#
        )
        .into_bytes()
    }

    fn central_directory_with_name_lengths(name_lengths: &[usize]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, name_length) in name_lengths.iter().copied().enumerate() {
            let mut header = [0_u8; 46];
            header[..4].copy_from_slice(b"PK\x01\x02");
            header[28..30].copy_from_slice(&u16::try_from(name_length).unwrap().to_le_bytes());
            bytes.extend_from_slice(&header);
            bytes
                .extend(std::iter::repeat_n(b'a' + u8::try_from(index % 26).unwrap(), name_length));
        }
        let central_size = u32::try_from(bytes.len()).unwrap();
        let entries = u16::try_from(name_lengths.len()).unwrap();
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&central_size.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes
    }

    fn cfb_with_stream(name: &str) -> Vec<u8> {
        let mut bytes = vec![0_u8; 3 * 512];
        bytes[..8].copy_from_slice(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]);
        bytes[26..28].copy_from_slice(&3_u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&0xfffe_u16.to_le_bytes());
        bytes[30..32].copy_from_slice(&9_u16.to_le_bytes());
        bytes[32..34].copy_from_slice(&6_u16.to_le_bytes());
        bytes[44..48].copy_from_slice(&1_u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&1_u32.to_le_bytes());
        bytes[56..60].copy_from_slice(&4096_u32.to_le_bytes());
        bytes[60..64].copy_from_slice(&CFB_END_OF_CHAIN.to_le_bytes());
        bytes[68..72].copy_from_slice(&CFB_END_OF_CHAIN.to_le_bytes());
        for offset in (76..512).step_by(4) {
            bytes[offset..offset + 4].copy_from_slice(&CFB_FREE_SECTOR.to_le_bytes());
        }
        bytes[76..80].copy_from_slice(&0_u32.to_le_bytes());
        for offset in (512..1024).step_by(4) {
            bytes[offset..offset + 4].copy_from_slice(&CFB_FREE_SECTOR.to_le_bytes());
        }
        bytes[512..516].copy_from_slice(&0xffff_fffd_u32.to_le_bytes());
        bytes[516..520].copy_from_slice(&CFB_END_OF_CHAIN.to_le_bytes());
        let encoded =
            format!("{name}\0").encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        bytes[1090] = 5;
        bytes[1100..1104].copy_from_slice(&1_u32.to_le_bytes());
        bytes[1152..1152 + encoded.len()].copy_from_slice(&encoded);
        bytes[1216..1218].copy_from_slice(&u16::try_from(encoded.len()).unwrap().to_le_bytes());
        bytes[1218] = 2;
        bytes[1220..1232].fill(0xff);
        bytes
    }

    #[test]
    fn hints_preserve_conflicting_extension_and_media_type() {
        let detector = HintFormatDetector;
        let input = resolved(b"ignored".to_vec(), "report.docx");
        let hint =
            FormatHint { media_type: Some("application/pdf".into()), ..FormatHint::default() };
        let candidates = block_on(detector.detect(&input, &hint, &execution_context())).unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| !candidate.diagnostics.is_empty()));
    }

    #[test]
    fn package_extension_is_stronger_than_generic_zip_content() {
        let input = resolved([b"PK\x05\x06".as_slice(), &[0_u8; 18]].concat(), "invalid.xlsx");
        let hints = block_on(HintFormatDetector.detect(
            &input,
            &FormatHint::default(),
            &execution_context(),
        ))
        .unwrap();
        let content = block_on(ContentFormatDetector.detect(
            &input,
            &FormatHint::default(),
            &execution_context(),
        ))
        .unwrap();
        let xlsx = hints.iter().find(|candidate| candidate.format == InputFormat::Xlsx).unwrap();
        let zip = content.iter().find(|candidate| candidate.format == InputFormat::Zip).unwrap();
        assert!(xlsx.confidence > zip.confidence);
    }

    #[test]
    fn magic_identification_does_not_trust_a_misleading_name() {
        let detector = ContentFormatDetector;
        let input = resolved(b"%PDF-1.7\n".to_vec(), "report.docx");
        let candidates =
            block_on(detector.detect(&input, &FormatHint::default(), &execution_context()))
                .unwrap();
        assert_eq!(candidates[0].format, InputFormat::Pdf);
        assert!((candidates[0].confidence - 0.99).abs() < f32::EPSILON);
    }

    #[test]
    fn rtf_magic_requires_version_and_delimiter() {
        assert_eq!(magic_candidate(b"{\\rtf1\\ansi ok}").unwrap().format, InputFormat::Rtf);
        assert!(magic_candidate(b"{\\rtf\\ansi missing-version}").is_none());
        assert!(magic_candidate(b"{\\rtf1x plain-prefix}").is_none());
        assert!(magic_candidate(b"prefix {\\rtf1 no}").is_none());
    }

    #[test]
    fn mpeg_audio_frame_header_detects_mp3_without_id3() {
        let mut mp3 = vec![0_u8; 421];
        mp3[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        mp3[417..421].copy_from_slice(&[0xff, 0xfb, 0xa0, 0x64]);
        let candidate = magic_candidate(&mp3).unwrap();
        assert_eq!(candidate.format, InputFormat::Audio);
        assert_eq!(candidate.evidence, "MPEG audio frame header");
        assert!(magic_candidate(&[0xff, 0xff, 0xff, 0xff]).is_none());
        assert!(magic_candidate(&[0x12, 0xff, 0xfb, 0x90, 0x64]).is_none());
        assert!(magic_candidate(&[0xff, 0xfb, 0xf0, 0x64]).is_none());
        assert!(magic_candidate(&[0xff, 0xfe, b'A', 0]).is_none());
        mp3[417..421].copy_from_slice(&[0xff, 0xf3, 0xa0, 0x64]);
        assert!(magic_candidate(&mp3).is_none());
    }

    #[test]
    fn bmp_requires_consistent_file_and_dib_headers() {
        let mut bmp = vec![0_u8; 58];
        bmp[..2].copy_from_slice(b"BM");
        bmp[2..6].copy_from_slice(&58_u32.to_le_bytes());
        bmp[10..14].copy_from_slice(&54_u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40_u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&1_i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&1_i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1_u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24_u16.to_le_bytes());
        assert_eq!(magic_candidate(&bmp).unwrap().format, InputFormat::Image);
        assert!(magic_candidate(b"BM").is_none());
        assert!(magic_candidate(&[b'B', b'M', 0, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0]).is_none());
        bmp[10..14].copy_from_slice(&200_u32.to_le_bytes());
        assert!(magic_candidate(&bmp).is_none());
    }

    #[test]
    fn structured_text_detection_orders_specific_formats_first() {
        let fixtures = [
            (b"<!doctype html><html></html>".as_slice(), InputFormat::Html),
            (
                b"<?xml version='1.0'?><rss version='2.0'><channel/></rss>".as_slice(),
                InputFormat::Feed,
            ),
            (b"<feed xmlns='http://www.w3.org/2005/Atom'></feed>".as_slice(), InputFormat::Feed),
            (b"<document/>".as_slice(), InputFormat::Xml),
            (br#"{"nbformat":4,"metadata":{},"cells":[]}"#.as_slice(), InputFormat::Ipynb),
            (br#"{"ordinary":true}"#.as_slice(), InputFormat::Json),
        ];
        for (bytes, expected) in fixtures {
            let input = resolved(bytes.to_vec(), "misleading.txt");
            let candidates = block_on(ContentFormatDetector.detect(
                &input,
                &FormatHint::default(),
                &execution_context(),
            ))
            .unwrap();
            assert_eq!(candidates[0].format, expected);
        }
    }

    #[test]
    fn bounded_structured_detection_protects_large_json_csv_and_tsv_from_text() {
        let mut large_json = "[".repeat(200);
        large_json.push('"');
        large_json.extend(std::iter::repeat_n('x', TEXT_INSPECTION_BYTE_LIMIT + 50_000));
        large_json.push('"');
        large_json.push_str(&"]".repeat(200));
        let candidate = structured(large_json.as_bytes()).unwrap();
        assert_eq!(candidate.format, InputFormat::Json);
        assert!(candidate.evidence.contains("valid JSON"));

        let fixtures = [
            (b"name,age\nAlice,42\nBob,30\n".as_slice(), InputFormat::Csv),
            (b"name\tage\nAlice\t42\nBob\t30\n".as_slice(), InputFormat::Tsv),
        ];
        for (bytes, expected) in fixtures {
            let candidates = detect(bytes);
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].format, expected);
        }
        assert_eq!(
            detect(b"ordinary prose, with one comma\nand a second plain line")[0].format,
            InputFormat::Text
        );
        assert_eq!(
            detect(b"Today, we walked home\nTomorrow, we will rest")[0].format,
            InputFormat::Text
        );

        let mut malformed_after_bound = String::from("name,age\nAlice,42\nBob,30\n");
        malformed_after_bound.extend(std::iter::repeat_n(' ', TEXT_INSPECTION_BYTE_LIMIT));
        malformed_after_bound.push_str("\n\"unterminated");
        assert!(structured(malformed_after_bound.as_bytes()).is_none());
    }

    #[test]
    fn json_scanner_reads_past_complete_sample_boundaries_and_limits_depth() {
        let prefix = "{\"value\":\"";
        let suffix = "\"}";
        let mut complete_at_boundary = String::with_capacity(TEXT_INSPECTION_BYTE_LIMIT + 16);
        complete_at_boundary.push_str(prefix);
        complete_at_boundary.extend(std::iter::repeat_n(
            'x',
            TEXT_INSPECTION_BYTE_LIMIT - prefix.len() - suffix.len(),
        ));
        complete_at_boundary.push_str(suffix);
        assert_eq!(complete_at_boundary.len(), TEXT_INSPECTION_BYTE_LIMIT);
        complete_at_boundary.push_str(" trailing prose");
        assert_eq!(structured(complete_at_boundary.as_bytes()).unwrap().format, InputFormat::Json);
        assert_eq!(detect(complete_at_boundary.as_bytes())[0].format, InputFormat::Json);

        assert_eq!(
            scan_json(b"{\"open\":[1,", &execution_context()).unwrap().status,
            JsonScanStatus::Open
        );
        let too_deep = "[".repeat(JSON_SCAN_DEPTH_LIMIT + 1);
        let error = scan_json(too_deep.as_bytes(), &execution_context()).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
        assert!(error.to_string().contains("json_scan_depth"));
    }

    #[test]
    fn json_scanner_validates_strings_numbers_literals_and_structure_without_recursion() {
        let valid = r#"{
            "escaped":"quote: \" slash: \\ unicode: \u4e2d 😀",
            "numbers":[0,-1,2.5,6.02e23,-4E-2],
            "literals":[true,false,null],
            "nested":{"empty":[],"object":{}}
        }"#
        .as_bytes();
        assert_eq!(
            scan_json(valid, &execution_context()).unwrap().status,
            JsonScanStatus::Complete
        );

        for invalid in [
            br#"{"bad":"\x"}"#.as_slice(),
            br#"{"bad":"\u12xz"}"#.as_slice(),
            br#"{"bad":01}"#.as_slice(),
            br#"{"bad":1.}"#.as_slice(),
            br#"{"bad":truX}"#.as_slice(),
            br#"{"bad":[1,]}"#.as_slice(),
            br#"{"bad":{]}}"#.as_slice(),
            b"{} trailing".as_slice(),
        ] {
            assert_eq!(
                scan_json(invalid, &execution_context()).unwrap().status,
                JsonScanStatus::Invalid,
                "{}",
                String::from_utf8_lossy(invalid)
            );
        }
    }

    #[test]
    fn json_scanner_requires_well_formed_utf16_surrogate_escapes() {
        for valid in [
            br#"{"emoji":"\uD83D\uDE00"}"#.as_slice(),
            br#"{"pairs":"\uD83D\uDE00\uD834\uDD1E"}"#.as_slice(),
            br#"{"bmp":"\u4e2d\u0000"}"#.as_slice(),
            br#"{"text":"\\uD800 is not a Unicode escape"}"#.as_slice(),
        ] {
            assert_eq!(
                scan_json(valid, &execution_context()).unwrap().status,
                JsonScanStatus::Complete,
                "{}",
                String::from_utf8_lossy(valid)
            );
        }

        for invalid in [
            br#"{"bad":"\uD800"}"#.as_slice(),
            br#"{"bad":"\uD800x"}"#.as_slice(),
            br#"{"bad":"\uD800\u0041"}"#.as_slice(),
            br#"{"bad":"\uDC00"}"#.as_slice(),
            br#"{"bad":"\uD800\uD800"}"#.as_slice(),
            br#"{"bad":"\uD800\u"}"#.as_slice(),
            br#"{"bad":"\uD800"#.as_slice(),
            br#"{"bad":"\uD800\uD"#.as_slice(),
        ] {
            assert_eq!(
                scan_json(invalid, &execution_context()).unwrap().status,
                JsonScanStatus::Invalid,
                "{}",
                String::from_utf8_lossy(invalid)
            );
            assert_eq!(structured(invalid).unwrap().format, InputFormat::Json);
            assert_eq!(detect(invalid)[0].format, InputFormat::Json);
        }
    }

    #[test]
    fn html_detection_handles_bounded_preludes_case_and_xhtml() {
        let fixtures = [
            "\u{feff}  <?xml version='1.0'?><!--lead--><HtMl xmlns='http://www.w3.org/1999/xhtml'>",
            " \n<!--lead--><!DoCtYpE HtMl><HTML>",
            "<?xml version='1.0'?><html xmlns='http://www.w3.org/1999/xhtml'/>",
            "<?xml version='1.0'?><xhtml:html xmlns:xhtml='http://www.w3.org/1999/xhtml'/>",
        ];
        for fixture in fixtures {
            let candidate = structured(fixture.as_bytes()).unwrap();
            assert_eq!(candidate.format, InputFormat::Html);
        }
        assert_eq!(
            structured(b"<?xml version='1.0'?><document/>").unwrap().format,
            InputFormat::Xml
        );
        assert!(!starts_with_tag("xhtml>", "html"));
        assert!(!starts_with_tag("zhtml>", "html"));
        assert_eq!(structured(b"<xhtml>").unwrap().format, InputFormat::Xml);
    }

    #[test]
    fn ambiguous_media_containers_do_not_receive_high_confidence() {
        let ogg = detect(b"OggS\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        let ebml = detect(b"\x1a\x45\xdf\xa3unknown");
        let iso = detect(b"\0\0\0\x10ftypzzzz\0\0\0\0");
        for candidate in [&ogg[0], &ebml[0], &iso[0]] {
            assert!(candidate.confidence <= 0.60);
            assert!(!candidate.diagnostics.is_empty());
        }
        let mut theora = vec![0_u8; 28];
        theora[..4].copy_from_slice(b"OggS");
        theora[26] = 1;
        theora[27] = 7;
        theora.extend_from_slice(b"\x80theora");
        assert_eq!(detect(&theora)[0].format, InputFormat::Video);
    }

    #[test]
    fn iso_media_requires_a_complete_bounded_ftyp_box() {
        let audio = detect(b"\0\0\0\x10ftypM4A \0\0\0\0");
        assert_eq!(audio[0].format, InputFormat::Audio);
        assert!((audio[0].confidence - 0.96).abs() < f32::EPSILON);
        assert!(detect(b"\0\0\0\x10ftypM4A ").is_empty());
        assert!(detect(b"\0\0\0\x0cftypM4A ").is_empty());
        assert!(detect(b"\0\0\0\x20ftypM4A \0\0\0\0").is_empty());
    }

    #[test]
    fn adts_aac_requires_two_complete_consistent_frame_boundaries() {
        let frame = [0xff, 0xf1, 0x50, 0x80, 0x00, 0xff, 0xfc];
        let mut audio = frame.to_vec();
        audio.extend_from_slice(&frame);
        let candidate = detect(&audio);
        assert_eq!(candidate[0].format, InputFormat::Audio);
        assert!((candidate[0].confidence - 0.96).abs() < f32::EPSILON);
        assert!(detect(&frame).is_empty());
        let mut invalid_sample_rate = audio;
        invalid_sample_rate[2] |= 0x3c;
        assert!(detect(&invalid_sample_rate).is_empty());
    }

    #[test]
    fn zip_parts_distinguish_ooxml_epub_and_odf() {
        let detector = ContentFormatDetector;
        let word_types = ooxml_content_type(
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        let fixtures = [
            (
                zip_with(&[
                    ("[Content_Types].xml", word_types.as_slice()),
                    ("word/document.xml", b"<w:document/>"),
                ]),
                InputFormat::Docx,
            ),
            (
                zip_with(&[
                    ("mimetype", b"application/epub+zip"),
                    (
                        "META-INF/container.xml",
                        b"<ocf:container xmlns:ocf='urn:oasis:names:tc:opendocument:xmlns:container'><ocf:rootfiles><ocf:rootfile full-path='OPS/content.opf'/></ocf:rootfiles></ocf:container>",
                    ),
                    ("OPS/content.opf", b"<package/>"),
                ]),
                InputFormat::Epub,
            ),
            (
                zip_with(&[
                    ("mimetype", b"application/vnd.oasis.opendocument.text"),
                    ("content.xml", b"<office:document-content/>"),
                    (
                        "META-INF/manifest.xml",
                        b"<manifest:manifest xmlns:manifest='urn:oasis:names:tc:opendocument:xmlns:manifest:1.0'><manifest:file-entry manifest:full-path='/' manifest:media-type='application/vnd.oasis.opendocument.text'/></manifest:manifest>",
                    ),
                ]),
                InputFormat::Odt,
            ),
        ];
        for (bytes, expected) in fixtures {
            let input = resolved(bytes, "misleading.zip");
            let candidates =
                block_on(detector.detect(&input, &FormatHint::default(), &execution_context()))
                    .unwrap();
            assert_eq!(candidates[1].format, expected);
            assert_eq!(candidates[0].format, InputFormat::Zip);
        }
    }

    #[test]
    fn zip_parts_detect_xlsb_main_content_type_from_default_extension() {
        let types = br#"<ct:Types xmlns:ct="http://schemas.openxmlformats.org/package/2006/content-types"><ct:Default Extension="bin" ContentType="application/vnd.ms-excel.sheet.binary.macroEnabled.main"/></ct:Types>"#;
        let bytes = zip_with(&[("[Content_Types].xml", types), ("xl/workbook.bin", b"workbook")]);
        let candidates = detect_zip(&bytes);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].format, InputFormat::Zip);
        assert_eq!(candidates[1].format, InputFormat::Xlsx);
    }

    #[test]
    fn zip_mimetype_read_is_bounded_and_explained() {
        let bytes = zip_with(&[("mimetype", &[b'x'; 129])]);
        let input = resolved(bytes, "oversized.odt");
        let candidates = block_on(ContentFormatDetector.detect(
            &input,
            &FormatHint::default(),
            &execution_context(),
        ))
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].format, InputFormat::Zip);
        assert!(candidates[0].diagnostics[0].contains("128 byte read limit"));
    }

    #[test]
    fn zip_entry_limit_is_checked_before_archive_construction() {
        let mut bytes = b"PK\x05\x06".to_vec();
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&5000_u16.to_le_bytes());
        bytes.extend_from_slice(&5000_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&22_u16.to_le_bytes());
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&22_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        assert_eq!(zip_preflight(&bytes).unwrap(), 5000);
        let candidates = detect_zip(&bytes);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].diagnostics[0].contains("5000 entries"));
        assert!(candidates[0].diagnostics[0].contains("before archive construction"));
    }

    #[test]
    fn zip_name_budget_is_checked_before_archive_construction() {
        let bytes = central_directory_with_name_lengths(&[u16::MAX as usize; 17]);
        let error = zip_preflight(&bytes).unwrap_err();
        assert!(error.contains("entry names exceed"));
        assert!(error.contains("before archive construction"));
        let candidates = detect_zip(&bytes);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].diagnostics[0].contains("entry names exceed"));
    }

    #[test]
    fn zip64_safely_skips_structure_inspection() {
        let mut bytes = b"PK\x05\x06".to_vec();
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        let candidates = detect_zip(&bytes);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].diagnostics[0].contains("ZIP64"));
    }

    #[test]
    fn package_detection_rejects_empty_plain_and_conflicting_archives() {
        let word_type = ooxml_content_type(
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        let combined_types = br#"<ct:Types xmlns:ct="http://schemas.openxmlformats.org/package/2006/content-types"><ct:Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><ct:Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></ct:Types>"#;
        let ordinary = zip_with(&[("ordinary.txt", b"hello")]);
        let empty_part =
            zip_with(&[("[Content_Types].xml", word_type.as_slice()), ("word/document.xml", b"")]);
        for (index, bytes) in [ordinary, empty_part].into_iter().enumerate() {
            let candidates = detect_zip(&bytes);
            assert_eq!(candidates.len(), 1, "fixture {index}");
            assert_eq!(candidates[0].format, InputFormat::Zip);
        }
        let conflict_bytes = zip_with(&[
            ("[Content_Types].xml", combined_types.as_slice()),
            ("word/document.xml", b"<w:document/>"),
            ("ppt/presentation.xml", b"<p:presentation/>"),
        ]);
        let conflict = detect_zip(&conflict_bytes);
        assert_eq!(conflict.len(), 1);
        assert!(conflict[0].diagnostics.iter().any(|value| value.contains("conflicting")));
    }

    #[test]
    fn package_xml_comments_cannot_spoof_structure() {
        let word_content_type =
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
        let ooxml = format!(
            r#"<ct:Types xmlns:ct="http://schemas.openxmlformats.org/package/2006/content-types"><!-- <ct:Override PartName="/word/document.xml" ContentType="{word_content_type}"/> --></ct:Types>"#
        );
        let epub = b"<ocf:container xmlns:ocf='urn:oasis:names:tc:opendocument:xmlns:container'><ocf:rootfiles><!-- <ocf:rootfile full-path='OPS/content.opf'/> --></ocf:rootfiles></ocf:container>";
        let odf = b"<manifest:manifest xmlns:manifest='urn:oasis:names:tc:opendocument:xmlns:manifest:1.0'><!-- <manifest:file-entry manifest:full-path='/' manifest:media-type='application/vnd.oasis.opendocument.text'/> --></manifest:manifest>";
        let fixtures = [
            zip_with(&[
                ("[Content_Types].xml", ooxml.as_bytes()),
                ("word/document.xml", b"<w:document/>"),
            ]),
            zip_with(&[
                ("mimetype", b"application/epub+zip"),
                ("META-INF/container.xml", epub),
                ("OPS/content.opf", b"<package/>"),
            ]),
            zip_with(&[
                ("mimetype", b"application/vnd.oasis.opendocument.text"),
                ("content.xml", b"<office:document-content/>"),
                ("META-INF/manifest.xml", odf),
            ]),
        ];
        for (index, bytes) in fixtures.into_iter().enumerate() {
            let candidates = detect_zip(&bytes);
            assert_eq!(candidates.len(), 1, "fixture {index}");
            assert_eq!(candidates[0].format, InputFormat::Zip, "fixture {index}");
        }
    }

    #[test]
    fn invalid_or_ambiguous_package_xml_is_rejected() {
        let content_type =
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
        let wrong_root = format!(
            r#"<ct:NotTypes xmlns:ct="http://schemas.openxmlformats.org/package/2006/content-types"><ct:Override PartName="/word/document.xml" ContentType="{content_type}"/></ct:NotTypes>"#
        );
        let entity = format!(
            r#"<!DOCTYPE ct:Types [<!ENTITY target "/word/document.xml">]><ct:Types xmlns:ct="http://schemas.openxmlformats.org/package/2006/content-types"><ct:Override PartName="&target;" ContentType="{content_type}"/></ct:Types>"#
        );
        let duplicate = format!(
            r#"<ct:Types xmlns:ct="http://schemas.openxmlformats.org/package/2006/content-types"><ct:Override PartName="/word/document.xml" ContentType="{content_type}"/><ct:Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></ct:Types>"#
        );
        for (index, metadata) in [wrong_root, entity, duplicate].into_iter().enumerate() {
            let bytes = zip_with(&[
                ("[Content_Types].xml", metadata.as_bytes()),
                ("word/document.xml", b"<w:document/>"),
            ]);
            let candidates = detect_zip(&bytes);
            assert_eq!(candidates.len(), 1, "fixture {index}");
            assert_eq!(candidates[0].format, InputFormat::Zip, "fixture {index}");
            assert!(
                candidates[0].diagnostics.iter().any(|value| value.contains("rejected")),
                "fixture {index}"
            );
        }
    }

    #[test]
    fn cfb_directory_chain_distinguishes_legacy_office() {
        let input = resolved(cfb_with_stream("PowerPoint Document"), "slides.doc");
        let candidates = block_on(ContentFormatDetector.detect(
            &input,
            &FormatHint::default(),
            &execution_context(),
        ))
        .unwrap();
        assert_eq!(candidates[0].format, InputFormat::Ppt);
        assert!((candidates[0].confidence - 0.98).abs() < f32::EPSILON);
    }

    #[test]
    fn embedded_ole_streams_do_not_supply_root_format_authority() {
        let mut bytes = cfb_with_stream("Workbook");
        bytes[1224..1228].copy_from_slice(&2_u32.to_le_bytes());
        bytes[1346] = 1; // Root sibling storage, with its own child tree.
        bytes[1348..1356].fill(0xff);
        bytes[1356..1360].copy_from_slice(&3_u32.to_le_bytes());
        let encoded =
            "WordDocument\0".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        bytes[1408..1408 + encoded.len()].copy_from_slice(&encoded);
        bytes[1472..1474].copy_from_slice(&u16::try_from(encoded.len()).unwrap().to_le_bytes());
        bytes[1474] = 2;
        bytes[1476..1488].fill(0xff);
        let candidates = detect_ole(&bytes);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].format, InputFormat::Xls);

        // Moving the same stream into the root sibling tree restores real ambiguity.
        bytes[1352..1356].copy_from_slice(&3_u32.to_le_bytes());
        let candidates = detect_ole(&bytes);
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|candidate| candidate.format == InputFormat::Doc));
        assert!(candidates.iter().any(|candidate| candidate.format == InputFormat::Xls));
    }

    #[test]
    fn root_directory_cycles_and_bad_indices_never_supply_authority() {
        for invalid in [0, 1, 4, u32::MAX - 1] {
            let mut bytes = cfb_with_stream("Workbook");
            bytes[1224..1228].copy_from_slice(&invalid.to_le_bytes());
            let candidates = detect_ole(&bytes);
            assert!(candidates.iter().all(|candidate| candidate.confidence < 0.5));
        }
    }

    #[test]
    fn aligned_bytes_outside_cfb_directory_are_not_stream_entries() {
        let mut bytes = cfb_with_stream("unrelated");
        let forged = "WordDocument\0".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        bytes[640..640 + forged.len()].copy_from_slice(&forged);
        bytes[704..706].copy_from_slice(&u16::try_from(forged.len()).unwrap().to_le_bytes());
        bytes[706] = 2;
        let candidates = detect_ole(&bytes);
        assert!(candidates.iter().all(|candidate| candidate.format != InputFormat::Doc));
    }

    #[test]
    fn malformed_cfb_header_degrades_to_diagnostic_low_confidence() {
        let mut bytes = cfb_with_stream("WordDocument");
        bytes[28..30].copy_from_slice(&0_u16.to_le_bytes());
        let candidates = detect_ole(&bytes);
        assert_eq!(candidates.len(), 4);
        assert!(
            candidates.iter().all(|candidate| (candidate.confidence - 0.20).abs() < f32::EPSILON)
        );
        assert!(
            candidates.iter().all(|candidate| { candidate.diagnostics[0].contains("byte order") })
        );
    }
}
