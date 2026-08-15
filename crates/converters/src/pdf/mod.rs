//! Faithful page-level PDF extraction followed by bounded semantic layout reconstruction.

mod assets;
mod budget;
mod count;
mod coverage;
mod error;
mod geometry;
mod ir;
mod runtime;

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
use geometry::{normalize_rect, page_locator, render_dimensions, safe_link_target};
use ir::{allocation_capacity_bound, character_working_set_bytes, provenance, text_block};
use runtime::lock_pdf_conversion;

#[cfg(test)]
use geometry::{displayed_dimensions, normalize_point};
#[cfg(test)]
use ir::character_ir_allocation_bytes;

use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter,
    ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ExecutionContext, FormatCandidate,
    Inline, InputFormat, NodeId, OcrPolicy, ProbeOutcome, Provenance, ProvenanceKind, Rect,
    ResolvedInput, ResourceReservation, Services, SourceLocator,
};
use into_markdown_pdfium::{
    Bitmap, Character, Error as PdfiumError, ImageBitmap, Limits, LinkTarget, PageInfo, PdfRect,
    Pdfium, PixelFormat,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::Duration;

const PROVIDER_ID: &str = "builtin.converter.pdfium";
const FORMATS: &[InputFormat] = &[InputFormat::Pdf];
const MIN_NATIVE_TEXT_CHARS: usize = 8;
const MIN_SCAN_IMAGE_COVERAGE: f64 = 0.50;
const MAX_RENDER_DIMENSION: u32 = 4096;
static PDF_CONVERSION_GATE: OnceLock<Mutex<()>> = OnceLock::new();

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
        self.runtime_path
            .clone()
            .or_else(|| std::env::var_os("PDFIUM_LIBRARY").map(PathBuf::from))
            .or_else(default_pdfium_runtime_path)
            .ok_or_else(|| ConversionError::ComponentUnavailable {
                component: "pdfium".into(),
                detail: crate::core_catalog::PDFIUM.install_hint.into(),
            })
    }
}

/// Resolve the explicit environment override or the pinned runtime shipped
/// beside the canonical installed executable. No PATH lookup occurs.
#[must_use]
pub fn default_pdfium_runtime_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PDFIUM_LIBRARY") {
        return Some(PathBuf::from(path));
    }
    let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
    packaged_pdfium_path(&executable)
}

fn packaged_pdfium_path(executable: &Path) -> Option<PathBuf> {
    let root = executable.parent()?.parent()?;
    #[cfg(target_os = "macos")]
    let relative = Path::new("lib/pdfium/libpdfium.dylib");
    #[cfg(target_os = "linux")]
    let relative = Path::new("lib/pdfium/libpdfium.so");
    #[cfg(target_os = "windows")]
    let relative = Path::new("lib/pdfium/pdfium.dll");
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return None;
    let path = root.join(relative);
    path.is_file().then_some(path)
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

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Ok(if candidate.format == InputFormat::Pdf && input.bytes.starts_with(b"%PDF-") {
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
            convert_pdf(&path, input, options, context)
        })
    }
}

#[allow(clippy::too_many_lines)]
fn convert_pdf(
    runtime_path: &Path,
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let _conversion_guard = lock_pdf_conversion(context)?;
    let bitmap_limit =
        options.limits.max_asset_bytes.min(options.limits.max_memory_bytes).min(400_000_000);
    let limits = Limits {
        max_document_bytes: options.limits.max_input_bytes.min(1024 * 1024 * 1024),
        max_pages: options.limits.max_pages,
        max_text_units_per_page: u32::try_from(into_markdown_core::MAX_DOCUMENT_INLINES)
            .unwrap_or(u32::MAX),
        max_render_dimension: MAX_RENDER_DIMENSION,
        max_render_pixels: bitmap_limit / 4,
        max_images_per_page: 10_000,
        max_page_objects: u32::try_from(into_markdown_core::MAX_DOCUMENT_NODES).unwrap_or(u32::MAX),
        max_password_bytes: 1024,
        max_bitmap_bytes: bitmap_limit,
        max_links_per_page: 10_000,
        max_link_bytes: 8 * 1024,
        max_font_name_bytes: 4 * 1024,
    };
    let runtime = Pdfium::load_pinned(runtime_path, limits).map_err(map_pdfium_error)?;
    let open_plan = runtime.plan_open(input.bytes.clone(), None).map_err(map_pdfium_error)?;
    let open_memory = context.reserve_memory(open_plan.allocation_bytes())?;
    let pdf = open_plan.materialize().map_err(map_pdfium_error)?;
    // The plan covers only the exact temporary password copy. PDFium has
    // consumed it before `materialize` returns; the input Arc remains owned by
    // the Engine input lease and the native parser allocation is documented
    // residual risk.
    drop(open_memory);
    let mut document = Document::default();
    let mut assets = Vec::new();
    let mut asset_ids = HashSet::new();
    let mut diagnostics = Vec::new();
    let mut total_asset_bytes = 0_u64;
    let mut total_nodes = 0_usize;
    let mut total_inlines = 0_usize;
    let mut total_page_objects = 0_usize;
    let mut retained_memory = Vec::new();
    let path_page_capacity =
        allocation_capacity_bound(usize::try_from(pdf.page_count()).unwrap_or(usize::MAX))?;
    let path_page_bytes = path_page_capacity
        .checked_mul(std::mem::size_of::<into_markdown_pdf_layout::PagePathEvidence>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| resource("max_memory_bytes", "PDF path page inventory overflow"))?;
    let path_page_memory = context.reserve_memory(path_page_bytes)?;
    let mut path_evidence = Vec::new();
    path_evidence
        .try_reserve_exact(path_page_capacity)
        .map_err(|_| resource("max_memory_bytes", "PDF path page inventory allocation failed"))?;
    if path_evidence.capacity() > path_page_capacity {
        return Err(resource(
            "max_memory_bytes",
            "PDF path page inventory capacity exceeded its plan",
        ));
    }
    let mut path_memory = Vec::new();
    retain_existing_reservation(context, &mut path_memory, path_page_memory)?;

    for page_index in 0..pdf.page_count() {
        context.checkpoint()?;
        let page_number = page_index + 1;
        let page = pdf.page(page_index).map_err(map_pdfium_error)?;
        total_page_objects = checked_count(
            total_page_objects,
            usize::try_from(page.object_count().map_err(map_pdfium_error)?).unwrap_or(usize::MAX),
            into_markdown_core::MAX_DOCUMENT_NODES,
            "pdfPageObjects",
        )?;
        let info = page.info().map_err(map_pdfium_error)?;
        let path_plan = request_path_scan(context, |checkpoint| {
            page.plan_path_bounds_with_checkpoint(checkpoint)
        })?;
        let path_allocation_bytes = path_plan.allocation_bytes();
        let (raw_path_bounds, raw_path_memory) =
            materialize_after_reserve(context, path_allocation_bytes, || {
                request_path_scan(context, |checkpoint| {
                    path_plan.materialize_with_checkpoint(checkpoint)
                })
            })?;
        let path_capacity = allocation_capacity_bound(raw_path_bounds.len())?;
        let normalized_path_bytes = path_capacity
            .checked_mul(std::mem::size_of::<Rect>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| resource("max_memory_bytes", "PDF path bounds allocation overflow"))?;
        let normalized_path_memory = context.reserve_memory(normalized_path_bytes)?;
        let mut normalized_paths = Vec::new();
        normalized_paths
            .try_reserve_exact(path_capacity)
            .map_err(|_| resource("max_memory_bytes", "PDF path bounds allocation failed"))?;
        if normalized_paths.capacity() > path_capacity {
            return Err(resource("max_memory_bytes", "PDF path bounds capacity exceeded its plan"));
        }
        for bounds in raw_path_bounds {
            context.checkpoint()?;
            normalized_paths.push(normalize_rect(bounds, &info)?);
        }
        drop(raw_path_memory);
        if normalized_paths.is_empty() {
            drop(normalized_path_memory);
        } else {
            retain_existing_reservation(context, &mut path_memory, normalized_path_memory)?;
            path_evidence.push(into_markdown_pdf_layout::PagePathEvidence {
                page: page_number,
                bounds: normalized_paths,
            });
        }
        let text_page = page.text_page().map_err(map_pdfium_error)?;
        let character_plan = text_page.plan_characters().map_err(map_pdfium_error)?;
        let character_count = character_plan.count();
        let character_budget = character_working_set_bytes(
            character_plan.allocation_bytes(),
            character_plan.retained_font_bytes(),
            character_count,
        )?;
        let (characters, character_memory) =
            materialize_after_reserve(context, character_budget, || {
                character_plan.materialize().map_err(map_pdfium_error)
            })?;
        retain_existing_reservation(context, &mut retained_memory, character_memory)?;
        total_inlines = checked_count(
            total_inlines,
            characters.len(),
            into_markdown_core::MAX_DOCUMENT_INLINES,
            "documentInlines",
        )?;
        let mut blocks = Vec::new();
        if !characters.is_empty() {
            total_nodes = checked_count(
                total_nodes,
                1,
                into_markdown_core::MAX_DOCUMENT_NODES,
                "documentNodes",
            )?;
            blocks
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "PDF block allocation failed"))?;
            blocks.push(text_block(page_number, &info, &characters)?);
        }
        let link_plan = text_page.plan_links().map_err(map_pdfium_error)?;
        let link_plan_bytes = link_plan.allocation_bytes();
        let (links, link_memory) = materialize_after_reserve(context, link_plan_bytes, || {
            link_plan.materialize().map_err(map_pdfium_error)
        })?;
        for (link_index, link) in links.into_iter().enumerate() {
            total_nodes = checked_count(
                total_nodes,
                1,
                into_markdown_core::MAX_DOCUMENT_NODES,
                "documentNodes",
            )?;
            total_inlines = checked_count(
                total_inlines,
                2,
                into_markdown_core::MAX_DOCUMENT_INLINES,
                "documentInlines",
            )?;
            let target_length = match &link.target {
                LinkTarget::ExternalUri(value) => value.len(),
                LinkTarget::InternalPage { .. } => 32,
            };
            let link_ir_bytes = u64::try_from(target_length)
                .unwrap_or(u64::MAX)
                .checked_mul(2)
                .and_then(|bytes| bytes.checked_add(output_block_overhead(2).ok()?))
                .ok_or_else(|| resource("max_memory_bytes", "link IR memory overflow"))?;
            retain_output_bytes(context, &mut retained_memory, link_ir_bytes)?;
            let target = safe_link_target(link.target, pdf.page_count())?;
            blocks
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "PDF block allocation failed"))?;
            blocks.push(BlockNode {
                id: NodeId(format!("pdf-page-{page_number}-link-{link_index}")),
                block: Block::Paragraph(vec![Inline::Link {
                    target: target.clone(),
                    content: vec![Inline::Text { value: target, marks: Vec::new() }],
                }]),
                provenance: provenance(
                    page_number,
                    Some(normalize_rect(link.bounds, &info)?),
                    None,
                    &info,
                )?,
            });
        }
        drop(link_memory);

        let mut coverage = PageCoverage::default();
        let image_plan = page.plan_images().map_err(map_pdfium_error)?;
        let image_plan_bytes = image_plan.allocation_bytes();
        let (images, image_metadata_memory) =
            materialize_after_reserve(context, image_plan_bytes, || {
                image_plan.materialize().map_err(map_pdfium_error)
            })?;
        for image in images {
            context.checkpoint()?;
            let bounds = normalize_rect(image.bounds(), &info)?;
            coverage.add(bounds, &info);
            let bitmap_plan = image.plan_bitmap().map_err(map_pdfium_error)?;
            let bitmap_plan_bytes = bitmap_plan.allocation_bytes();
            let (bitmap, bitmap_memory) =
                materialize_after_reserve(context, bitmap_plan_bytes, || {
                    bitmap_plan.materialize().map_err(map_pdfium_error)
                })?;
            let (encoded, encoded_memory) = image_bitmap_to_bmp(&bitmap, options, context)?;
            drop(bitmap);
            drop(bitmap_memory);
            account_asset(&encoded, &mut total_asset_bytes, options)?;
            retain_output_bytes(context, &mut retained_memory, asset_record_overhead()?)?;
            let id = content_asset_id("pdf-image", &encoded)?;
            asset_ids
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "asset ID allocation failed"))?;
            if asset_ids.insert(id.clone()) {
                retain_existing_reservation(context, &mut retained_memory, encoded_memory)?;
                assets.try_reserve(1).map_err(|_| {
                    resource("max_memory_bytes", "asset inventory allocation failed")
                })?;
                assets.push(Asset {
                    id: AssetId(id.clone()),
                    filename: Some(format!("{id}.bmp")),
                    media_type: "image/bmp".into(),
                    bytes: encoded,
                    external_uri: None,
                });
            }
            total_nodes = checked_count(
                total_nodes,
                1,
                into_markdown_core::MAX_DOCUMENT_NODES,
                "documentNodes",
            )?;
            retain_output_bytes(context, &mut retained_memory, output_block_overhead(0)?)?;
            blocks
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "PDF block allocation failed"))?;
            blocks.push(BlockNode {
                id: NodeId(format!("pdf-page-{page_number}-image-{}", image.index())),
                block: Block::Image { asset: AssetId(id), alt: None },
                provenance: provenance(page_number, Some(bounds), None, &info)?,
            });
        }
        drop(image_metadata_memory);

        let printable = characters
            .iter()
            .filter(|character| !character.value.is_control() && !character.value.is_whitespace())
            .count();
        let scanned =
            printable < MIN_NATIVE_TEXT_CHARS && coverage.ratio() >= MIN_SCAN_IMAGE_COVERAGE;
        let render_requested = options.ocr.policy == OcrPolicy::Always
            || (options.ocr.policy == OcrPolicy::Auto && scanned);
        if render_requested {
            let (width, height) = render_dimensions(&info)?;
            let render_bytes = u64::from(width)
                .checked_mul(u64::from(height))
                .and_then(|pixels| pixels.checked_mul(4))
                .and_then(|bytes| bytes.checked_mul(2))
                .ok_or_else(|| resource("max_memory_bytes", "render working set overflow"))?;
            let bitmap_memory = context.reserve_memory(render_bytes)?;
            let bitmap = page.render_bgra(width, height).map_err(map_pdfium_error)?;
            let (encoded, encoded_memory) = rendered_bitmap_to_bmp(&bitmap, options, context)?;
            drop(bitmap);
            drop(bitmap_memory);
            account_asset(&encoded, &mut total_asset_bytes, options)?;
            retain_output_bytes(context, &mut retained_memory, asset_record_overhead()?)?;
            let id = content_asset_id("pdf-page-render", &encoded)?;
            asset_ids
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "asset ID allocation failed"))?;
            if asset_ids.insert(id.clone()) {
                retain_existing_reservation(context, &mut retained_memory, encoded_memory)?;
                assets.try_reserve(1).map_err(|_| {
                    resource("max_memory_bytes", "asset inventory allocation failed")
                })?;
                assets.push(Asset {
                    id: AssetId(id.clone()),
                    filename: Some(format!("{id}.bmp")),
                    media_type: "image/bmp".into(),
                    bytes: encoded,
                    external_uri: None,
                });
            }
            total_nodes = checked_count(
                total_nodes,
                1,
                into_markdown_core::MAX_DOCUMENT_NODES,
                "documentNodes",
            )?;
            retain_output_bytes(context, &mut retained_memory, output_block_overhead(0)?)?;
            blocks
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "PDF block allocation failed"))?;
            blocks.push(BlockNode {
                id: NodeId(format!("pdf-page-{page_number}-ocr-render")),
                block: Block::Image { asset: AssetId(id), alt: Some("page render for OCR".into()) },
                provenance: provenance(page_number, None, None, &info)?,
            });
        }
        if scanned {
            retain_output_bytes(context, &mut retained_memory, diagnostic_overhead()?)?;
            diagnostics
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "diagnostic allocation failed"))?;
            diagnostics.push(Diagnostic {
                code: "pdf.scannedPage".into(),
                severity: DiagnosticSeverity::Info,
                message: format!(
                    "page {page_number} has fewer than {MIN_NATIVE_TEXT_CHARS} printable native characters and image coverage of at least 50%"
                ),
                locator: Some(page_locator(page_number, &info)),
            });
        }
        total_nodes =
            checked_count(total_nodes, 1, into_markdown_core::MAX_DOCUMENT_NODES, "documentNodes")?;
        retain_output_bytes(context, &mut retained_memory, output_block_overhead(0)?)?;
        document
            .blocks
            .try_reserve(1)
            .map_err(|_| resource("max_memory_bytes", "page allocation failed"))?;
        document.blocks.push(BlockNode {
            id: NodeId(format!("pdf-page-{page_number}")),
            block: Block::Page { number: page_number, blocks },
            provenance: provenance(page_number, None, None, &info)?,
        });
    }
    let layout_config = into_markdown_pdf_layout::LayoutConfig {
        limits: into_markdown_pdf_layout::LayoutLimits {
            max_atoms: into_markdown_core::MAX_DOCUMENT_INLINES,
            max_lines: into_markdown_core::MAX_DOCUMENT_NODES,
            max_comparisons: 12_000_000,
            max_table_columns: usize::try_from(options.limits.max_table_columns)
                .unwrap_or(usize::MAX)
                .min(into_markdown_core::MAX_TABLE_COLUMNS),
            max_table_cells: usize::try_from(options.limits.max_table_cells)
                .unwrap_or(usize::MAX)
                .min(into_markdown_core::MAX_DOCUMENT_NODES),
        },
    };
    let layout = into_markdown_pdf_layout::reconstruct_document_with_path_evidence(
        document,
        &layout_config,
        &path_evidence,
        context,
    )?;
    let (rebuilt_document, layout_reservation) = layout.into_parts();
    drop(path_evidence);
    drop(path_memory);
    document = rebuilt_document;
    if let Some(reservation) = layout_reservation {
        retain_existing_reservation(context, &mut retained_memory, reservation)?;
    }
    let validation_bytes = total_nodes
        .checked_add(total_inlines)
        .and_then(|value| value.checked_mul(256))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| resource("max_memory_bytes", "document validation memory overflow"))?;
    let validation_memory = context.reserve_memory(validation_bytes)?;
    document.validate().map_err(|error| ConversionError::Internal {
        detail: format!("PDF converter emitted invalid IR at {}: {}", error.path, error.detail),
    })?;
    drop(validation_memory);
    let output = ConverterOutput::new_with_memory_reservations(
        document,
        assets,
        diagnostics,
        retained_memory,
    );
    output.account_retained(context)
}
