//! Faithful page-level PDF extraction followed by bounded semantic layout reconstruction.

mod assets;
mod budget;
mod count;
mod coverage;
mod error;
mod geometry;
mod ir;
mod pages;
mod runtime;
mod stream;

#[cfg(test)]
mod tests;

use assets::{account_asset, content_asset_id, image_bitmap_to_bmp, rendered_bitmap_to_bmp};
use budget::{
    asset_record_overhead, diagnostic_overhead, materialize_after_reserve, output_block_overhead,
    retain_existing_reservation, retain_output_bytes,
};
use count::checked_count;
use coverage::PageCoverage;
use error::{malformed, map_pdfium_error, resource};
use geometry::{
    displayed_dimensions, normalize_rect, page_locator, render_dimensions, safe_link_target,
};
use ir::{allocation_capacity_bound, character_working_set_bytes, provenance, text_block};
#[cfg(test)]
use runtime::lock_pdf_conversion;

#[cfg(test)]
use geometry::normalize_point;
#[cfg(test)]
use ir::character_ir_allocation_bytes;

use into_markdown_core::{
    AiMode, Asset, AssetId, AssetMode, Block, BlockNode, BoxFuture, ConversionError,
    ConversionOptions, Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document,
    ErrorPolicy, ExecutionContext, FormatCandidate, Inline, InputFormat, NodeId, OcrPolicy,
    ProbeOutcome, Provenance, ProvenanceKind, Rect, ResolvedInput, ResourceReservation, Services,
    SourceLocator,
};
use into_markdown_pdfium::{
    Bitmap, Character, Error as PdfiumError, ImageBitmap, Limits, LinkTarget, PageInfo, PdfRect,
    Pdfium, PixelFormat,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
#[cfg(target_os = "windows")]
use std::os::windows::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const PROVIDER_ID: &str = "builtin.converter.pdfium";
const FORMATS: &[InputFormat] = &[InputFormat::Pdf];
const MIN_NATIVE_TEXT_CHARS: usize = 8;
const MIN_SCAN_IMAGE_COVERAGE: f64 = 0.50;
const MAX_RENDER_DIMENSION: u32 = 16_384;
const MAX_PAGE_RENDER_DIMENSION: u32 = 4096;
type PdfiumRuntimeResolver = fn() -> Result<PathBuf, ConversionError>;
static PDFIUM_RUNTIME_RESOLVER: OnceLock<PdfiumRuntimeResolver> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static IMAGE_BITMAP_MATERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn has_pdf_header(bytes: &[u8]) -> bool {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes).starts_with(b"%PDF-")
}

fn image_pixels_required(options: &ConversionOptions) -> bool {
    options.output.asset_mode != AssetMode::Omit
        || options.ocr.policy != OcrPolicy::Off
        || [
            options.ai.vision_ocr,
            options.ai.image_description,
            options.ai.layout_repair,
            options.ai.table_repair,
            options.ai.formula_repair,
        ]
        .iter()
        .any(|mode| *mode != AiMode::Off)
}

fn request_path_scan<T>(
    context: &ExecutionContext,
    scan: impl FnOnce(&mut dyn FnMut() -> bool) -> Result<T, PdfiumError>,
) -> Result<T, ConversionError> {
    let mut checkpoint_error = None;
    let result = {
        let mut checkpoint = || {
            if checkpoint_error.is_some() {
                return false;
            }
            match context.checkpoint() {
                Ok(()) => true,
                Err(error) => {
                    checkpoint_error = Some(error);
                    false
                }
            }
        };
        scan(&mut checkpoint)
    };
    if let Some(error) = checkpoint_error {
        return Err(error);
    }
    result.map_err(map_pdfium_error)
}

/// PDF converter backed only by an explicitly configured, pinned `PDFium` runtime.
#[derive(Debug, Clone, Default)]
pub struct PdfConverter {
    runtime_path: Option<PathBuf>,
}

impl PdfConverter {
    /// Configure the exact pinned runtime file. No PATH lookup or system fallback occurs.
    #[must_use]
    pub fn with_runtime_path(path: impl Into<PathBuf>) -> Self {
        Self { runtime_path: Some(path.into()) }
    }

    fn runtime_path(&self) -> Result<PathBuf, ConversionError> {
        if let Some(path) = self
            .runtime_path
            .clone()
            .or_else(|| std::env::var_os("PDFIUM_LIBRARY").map(PathBuf::from))
            .or_else(packaged_pdfium_runtime_path)
        {
            return Ok(path);
        }
        if let Some(resolver) = PDFIUM_RUNTIME_RESOLVER.get() {
            return resolver();
        }
        Err(ConversionError::ComponentUnavailable {
            component: "pdfium".into(),
            detail: crate::core_catalog::PDFIUM.install_hint.into(),
        })
    }
}

/// Install a process-local lazy resolver for a `PDFium` runtime embedded by the
/// final application binary. Registering performs no filesystem work; the
/// resolver is called only when a PDF conversion actually begins.
#[must_use]
pub fn install_pdfium_runtime_resolver(resolver: PdfiumRuntimeResolver) -> bool {
    PDFIUM_RUNTIME_RESOLVER.set(resolver).is_ok()
}

/// Resolve the explicit environment override or the pinned runtime shipped
/// beside the canonical installed executable. No PATH lookup occurs.
#[must_use]
pub fn default_pdfium_runtime_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PDFIUM_LIBRARY") {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = packaged_pdfium_runtime_path() {
        return Some(path);
    }
    PDFIUM_RUNTIME_RESOLVER.get().and_then(|resolver| resolver().ok())
}

fn packaged_pdfium_runtime_path() -> Option<PathBuf> {
    packaged_pdfium_path(&std::env::current_exe().ok()?)
}

#[cfg(not(target_os = "windows"))]
fn packaged_pdfium_path(_executable: &Path) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "windows")]
fn packaged_pdfium_path(executable: &Path) -> Option<PathBuf> {
    if !path_is_physical(executable) {
        return None;
    }
    let executable = executable.canonicalize().ok()?;
    let executable_metadata = std::fs::symlink_metadata(&executable).ok()?;
    if !executable_metadata.is_file() || is_reparse_or_link(&executable_metadata) {
        return None;
    }
    let executable_directory = executable.parent()?;
    let relative = Path::new("lib/pdfium/pdfium.dll");
    let portable = executable_directory.join(relative);
    match packaged_runtime_state(executable_directory, &portable) {
        PackagedRuntimeState::Physical => return Some(portable),
        PackagedRuntimeState::Unsafe => return None,
        PackagedRuntimeState::Missing => {}
    }
    if !executable_directory.file_name().is_some_and(|name| name.eq_ignore_ascii_case("bin")) {
        return None;
    }
    let installed_root = executable_directory.parent()?;
    let installed = installed_root.join(relative);
    matches!(packaged_runtime_state(installed_root, &installed), PackagedRuntimeState::Physical)
        .then_some(installed)
}

#[cfg(target_os = "windows")]
fn path_is_physical(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            return false;
        };
        if is_reparse_or_link(&metadata) {
            return false;
        }
    }
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(target_os = "windows")]
enum PackagedRuntimeState {
    Missing,
    Physical,
    Unsafe,
}

#[cfg(target_os = "windows")]
fn packaged_runtime_state(root: &Path, runtime: &Path) -> PackagedRuntimeState {
    let Ok(relative) = runtime.strip_prefix(root) else {
        return PackagedRuntimeState::Unsafe;
    };
    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty() {
        return PackagedRuntimeState::Unsafe;
    }
    let mut current = root.to_owned();
    for (index, component) in components.iter().enumerate() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return PackagedRuntimeState::Unsafe;
        }
        current.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return PackagedRuntimeState::Missing;
            }
            Err(_) => return PackagedRuntimeState::Unsafe,
        };
        if is_reparse_or_link(&metadata)
            || if index + 1 == components.len() { !metadata.is_file() } else { !metadata.is_dir() }
        {
            return PackagedRuntimeState::Unsafe;
        }
    }
    if std::fs::canonicalize(runtime).is_ok_and(|canonical| canonical == runtime) {
        PackagedRuntimeState::Physical
    } else {
        PackagedRuntimeState::Unsafe
    }
}

#[cfg(target_os = "windows")]
fn is_reparse_or_link(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    metadata.file_attributes() & 0x400 != 0
}

/// Verify an exact pinned `PDFium` file without opening a document.
///
/// # Errors
///
/// Returns a stable component error if the runtime is missing, corrupt, or
/// incompatible with the embedded `PDFium` authority.
pub fn verify_pdfium_runtime(path: &Path) -> Result<(), ConversionError> {
    Pdfium::load_pinned(path, Limits::default()).map(drop).map_err(map_pdfium_error)
}

impl Converter for PdfConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        200
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
    }

    fn stream_support(&self) -> Option<&dyn into_markdown_core::ConverterStream> {
        Some(self)
    }

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Ok(if candidate.format == InputFormat::Pdf && has_pdf_header(&input.bytes) {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
            })
        })
    }

    fn planned_output_bytes(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        // The outer credit covers the complete native/materialized/output peak;
        // precise incremental charges inside the converter fail before each
        // bounded allocation and become authenticated retained leases.
        Ok(context.available_memory_bytes())
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            let path = self.runtime_path()?;
            let _permit = runtime::acquire_pdf_conversion(context).await?;
            convert_pdf_admitted(&path, input, options, context)
        })
    }
}

#[cfg(test)]
fn convert_pdf(
    runtime_path: &Path,
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let _conversion_guard = lock_pdf_conversion(context)?;
    convert_pdf_admitted(runtime_path, input, options, context)
}

fn convert_pdf_admitted(
    runtime_path: &Path,
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let runtime = pages::load_runtime(runtime_path, options)?;
    let pdf = open_document(&runtime, input, context)?;
    let mut output = pages::PdfOutput::new(pdf.page_count(), context)?;
    let mut counts = pages::Counts::default();
    for page_index in 0..pdf.page_count() {
        output = output.extract_page(&pdf, page_index, options, context, &mut counts, false)?;
    }
    output.finish(options, context)
}

fn open_document<'a>(
    runtime: &'a Pdfium,
    input: &ResolvedInput,
    context: &ExecutionContext,
) -> Result<into_markdown_pdfium::Document<'a>, ConversionError> {
    let plan = runtime.plan_open(input.bytes.clone(), None).map_err(map_pdfium_error)?;
    let _memory = context.reserve_memory(plan.allocation_bytes())?;
    plan.materialize().map_err(map_pdfium_error)
}
