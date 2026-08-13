//! Faithful page-level PDF extraction. Reading-order reconstruction is intentionally deferred.

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
            .ok_or_else(|| ConversionError::ComponentUnavailable {
                component: "pdfium".into(),
                detail: "set PDFIUM_LIBRARY to the exact pinned runtime file".into(),
            })
    }
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

fn materialize_after_reserve<T>(
    context: &ExecutionContext,
    allocation_bytes: u64,
    materialize: impl FnOnce() -> Result<T, ConversionError>,
) -> Result<(T, ResourceReservation), ConversionError> {
    let reservation = context.reserve_memory(allocation_bytes)?;
    let value = materialize()?;
    Ok((value, reservation))
}

fn retain_existing_reservation(
    context: &ExecutionContext,
    retained: &mut Vec<ResourceReservation>,
    reservation: ResourceReservation,
) -> Result<(), ConversionError> {
    ensure_reservation_inventory(context, retained, 1)?;
    retained.push(reservation);
    Ok(())
}

fn retain_output_bytes(
    context: &ExecutionContext,
    retained: &mut Vec<ResourceReservation>,
    bytes: u64,
) -> Result<(), ConversionError> {
    ensure_reservation_inventory(context, retained, 1)?;
    let reservation = context.reserve_memory(bytes)?;
    retained.push(reservation);
    Ok(())
}

fn ensure_reservation_inventory(
    context: &ExecutionContext,
    retained: &mut Vec<ResourceReservation>,
    additional: usize,
) -> Result<(), ConversionError> {
    let required = retained
        .len()
        .checked_add(additional)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| resource("max_memory_bytes", "reservation inventory overflow"))?;
    if required <= retained.capacity() {
        return Ok(());
    }
    let planned = allocation_capacity_bound(required)?;
    let additional_capacity = planned.saturating_sub(retained.capacity());
    let bytes = additional_capacity
        .checked_mul(std::mem::size_of::<ResourceReservation>())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| resource("max_memory_bytes", "reservation inventory memory overflow"))?;
    let inventory_reservation = context.reserve_memory(bytes)?;
    retained
        .try_reserve_exact(additional_capacity)
        .map_err(|_| resource("max_memory_bytes", "reservation inventory allocation failed"))?;
    if retained.capacity() > planned {
        return Err(resource(
            "max_memory_bytes",
            "reservation inventory capacity exceeded its plan",
        ));
    }
    retained.push(inventory_reservation);
    Ok(())
}

fn output_block_overhead(nested_inlines: usize) -> Result<u64, ConversionError> {
    let block_slots = 2_usize
        .checked_mul(std::mem::size_of::<BlockNode>())
        .ok_or_else(|| resource("max_memory_bytes", "block slot memory overflow"))?;
    let inline_slots = allocation_capacity_bound(nested_inlines)?
        .checked_mul(std::mem::size_of::<Inline>())
        .ok_or_else(|| resource("max_memory_bytes", "inline slot memory overflow"))?;
    // Covers bounded node ID, provider, link/image-owned short strings, Box
    // backing, allocator metadata, and the per-page container header.
    let bytes = block_slots
        .checked_add(inline_slots)
        .and_then(|value| value.checked_add(std::mem::size_of::<Provenance>()))
        .and_then(|value| value.checked_add(1_024))
        .ok_or_else(|| resource("max_memory_bytes", "block output memory overflow"))?;
    u64::try_from(bytes).map_err(|_| resource("max_memory_bytes", "block memory does not fit u64"))
}

fn asset_record_overhead() -> Result<u64, ConversionError> {
    let bytes = 2_usize
        .checked_mul(std::mem::size_of::<Asset>())
        .and_then(|value| value.checked_add(2 * std::mem::size_of::<String>()))
        // HashSet bucket/control storage, two deterministic IDs, filename,
        // media type, and allocator alignment.
        .and_then(|value| value.checked_add(2_048))
        .ok_or_else(|| resource("max_memory_bytes", "asset record memory overflow"))?;
    u64::try_from(bytes)
        .map_err(|_| resource("max_memory_bytes", "asset record memory does not fit u64"))
}

fn diagnostic_overhead() -> Result<u64, ConversionError> {
    let bytes = 2_usize
        .checked_mul(std::mem::size_of::<Diagnostic>())
        .and_then(|value| value.checked_add(1_024))
        .ok_or_else(|| resource("max_memory_bytes", "diagnostic memory overflow"))?;
    u64::try_from(bytes)
        .map_err(|_| resource("max_memory_bytes", "diagnostic memory does not fit u64"))
}

fn lock_pdf_conversion(
    context: &ExecutionContext,
) -> Result<MutexGuard<'static, ()>, ConversionError> {
    let gate = PDF_CONVERSION_GATE.get_or_init(|| Mutex::new(()));
    loop {
        context.checkpoint()?;
        match gate.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
            Err(TryLockError::Poisoned(_)) => {
                return Err(ConversionError::Internal {
                    detail: "PDF conversion gate is poisoned".into(),
                });
            }
        }
    }
}

fn text_block(
    page: u32,
    info: &PageInfo,
    characters: &[Character],
) -> Result<BlockNode, ConversionError> {
    let mut inlines = Vec::new();
    let inline_capacity = allocation_capacity_bound(characters.len())?;
    inlines
        .try_reserve_exact(inline_capacity)
        .map_err(|_| resource("max_memory_bytes", "source text allocation failed"))?;
    if inlines.capacity() > inline_capacity {
        return Err(resource("max_memory_bytes", "source text capacity exceeded its plan"));
    }
    for character in characters {
        let mut value = String::new();
        value
            .try_reserve_exact(4)
            .map_err(|_| resource("max_memory_bytes", "source character allocation failed"))?;
        if value.capacity() > allocation_capacity_bound(4)? {
            return Err(resource(
                "max_memory_bytes",
                "source character capacity exceeded its plan",
            ));
        }
        value.push(character.value);
        inlines.push(Inline::SourceText {
            value,
            marks: Vec::new(),
            provenance: Box::new(provenance(
                page,
                Some(normalize_rect(character.bounds, info)?),
                Some(character),
                info,
            )?),
        });
    }
    Ok(BlockNode {
        id: NodeId(format!("pdf-page-{page}-native-text")),
        block: Block::Paragraph(inlines),
        provenance: provenance(page, None, None, info)?,
    })
}

fn provenance(
    page: u32,
    bounds: Option<Rect>,
    character: Option<&Character>,
    info: &PageInfo,
) -> Result<Provenance, ConversionError> {
    let mut locator = page_locator(page, info);
    locator.bounds = bounds;
    if let Some(character) = character {
        locator.character_index = Some(character.index);
        locator.font_name = character
            .font_name
            .as_ref()
            .map(|name| try_clone_bounded(name, "font name clone"))
            .transpose()?;
        locator.font_size = Some(character.font_size);
        locator.rotation_degrees = Some(character.angle_degrees);
    }
    Ok(Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: try_clone_bounded(PROVIDER_ID, "provenance provider")?,
        locator,
        confidence: None,
    })
}

fn allocation_capacity_bound(length: usize) -> Result<usize, ConversionError> {
    if length == 0 {
        return Ok(0);
    }
    length
        .checked_mul(2)
        .map(|value| value.max(4))
        .ok_or_else(|| resource("max_memory_bytes", "allocation capacity bound overflow"))
}

fn try_clone_bounded(value: &str, detail: &'static str) -> Result<String, ConversionError> {
    into_markdown_pdfium::fixed_string(value).map_err(|_| resource("max_memory_bytes", detail))
}

fn character_ir_allocation_bytes(count: u32) -> Result<u64, ConversionError> {
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    let inline_capacity = allocation_capacity_bound(count)?;
    let per_character = std::mem::size_of::<Provenance>()
        .checked_add(allocation_capacity_bound(4)?)
        .and_then(|value| value.checked_add(allocation_capacity_bound(PROVIDER_ID.len()).ok()?))
        .ok_or_else(|| resource("max_memory_bytes", "character IR memory overflow"))?;
    let node_id_length = "pdf-page--native-text"
        .len()
        .checked_add(10)
        .ok_or_else(|| resource("max_memory_bytes", "character node ID overflow"))?;
    let bytes = inline_capacity
        .checked_mul(std::mem::size_of::<Inline>())
        .and_then(|value| value.checked_add(count.checked_mul(per_character)?))
        .and_then(|value| value.checked_add(allocation_capacity_bound(node_id_length).ok()?))
        .and_then(|value| value.checked_add(allocation_capacity_bound(PROVIDER_ID.len()).ok()?))
        .and_then(|value| value.checked_add(2 * std::mem::size_of::<BlockNode>()))
        .ok_or_else(|| resource("max_memory_bytes", "character IR memory overflow"))?;
    u64::try_from(bytes)
        .map_err(|_| resource("max_memory_bytes", "character IR memory does not fit u64"))
}

fn character_working_set_bytes(
    materialization_bytes: u64,
    retained_font_bytes: u64,
    count: u32,
) -> Result<u64, ConversionError> {
    materialization_bytes
        .checked_add(retained_font_bytes)
        .and_then(|bytes| bytes.checked_add(character_ir_allocation_bytes(count).ok()?))
        .ok_or_else(|| resource("max_memory_bytes", "character working set overflow"))
}

fn page_locator(page: u32, info: &PageInfo) -> SourceLocator {
    let (width, height) = displayed_dimensions(info);
    SourceLocator {
        page: Some(page),
        page_width: Some(width),
        page_height: Some(height),
        rotation_degrees: Some(f32::from(info.rotation_degrees)),
        ..SourceLocator::default()
    }
}

fn displayed_dimensions(info: &PageInfo) -> (f32, f32) {
    (info.width_points, info.height_points)
}

#[allow(clippy::cast_possible_truncation)]
fn normalize_rect(rect: PdfRect, info: &PageInfo) -> Result<Rect, ConversionError> {
    let points = [
        normalize_point(rect.left, rect.bottom, info)?,
        normalize_point(rect.left, rect.top, info)?,
        normalize_point(rect.right, rect.bottom, info)?,
        normalize_point(rect.right, rect.top, info)?,
    ];
    let min_x = points.iter().map(|point| point.0).fold(f64::INFINITY, f64::min);
    let max_x = points.iter().map(|point| point.0).fold(f64::NEG_INFINITY, f64::max);
    let min_y = points.iter().map(|point| point.1).fold(f64::INFINITY, f64::min);
    let max_y = points.iter().map(|point| point.1).fold(f64::NEG_INFINITY, f64::max);
    let values = [min_x, min_y, max_x, max_y, max_x - min_x, max_y - min_y];
    if values.iter().any(|value| !value.is_finite() || value.abs() > f64::from(f32::MAX)) {
        return Err(malformed("geometry", "normalized rectangle is not representable"));
    }
    Ok(Rect {
        x: min_x as f32,
        y: min_y as f32,
        width: (max_x - min_x) as f32,
        height: (max_y - min_y) as f32,
    })
}

fn normalize_point(x: f32, y: f32, info: &PageInfo) -> Result<(f64, f64), ConversionError> {
    let (raw_width, raw_height) = if matches!(info.rotation_degrees, 90 | 270) {
        (f64::from(info.height_points), f64::from(info.width_points))
    } else {
        (f64::from(info.width_points), f64::from(info.height_points))
    };
    let (x, y) = (f64::from(x), f64::from(y));
    let point = match info.rotation_degrees {
        0 => (x, raw_height - y),
        90 => (y, x),
        180 => (raw_width - x, y),
        270 => (raw_height - y, raw_width - x),
        _ => unreachable!("PDFium boundary validates page rotation"),
    };
    if !point.0.is_finite() || !point.1.is_finite() {
        return Err(malformed("geometry", "normalized point is not finite"));
    }
    Ok(point)
}

fn safe_link_target(target: LinkTarget, page_count: u32) -> Result<String, ConversionError> {
    match target {
        LinkTarget::InternalPage { page_index } => {
            if page_index >= page_count {
                return Err(malformed("link", "internal destination is outside the document"));
            }
            let page = page_index
                .checked_add(1)
                .ok_or_else(|| malformed("link", "internal destination overflow"))?;
            Ok(format!("#pdf-page-{page}"))
        }
        LinkTarget::ExternalUri(value) => {
            if value.contains('\0') || value.chars().any(char::is_control) {
                return Err(malformed("link", "URI contains a NUL or control character"));
            }
            let parsed = url::Url::parse(&value)
                .map_err(|_| malformed("link", "external URI is not absolute"))?;
            if !matches!(parsed.scheme(), "http" | "https" | "mailto") {
                return Err(malformed("link", "external URI scheme is not permitted"));
            }
            Ok(value)
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn render_dimensions(info: &PageInfo) -> Result<(u32, u32), ConversionError> {
    let (width, height) = displayed_dimensions(info);
    let scale = (f64::from(MAX_RENDER_DIMENSION) / f64::from(width.max(height))).min(2.0);
    let width = (f64::from(width) * scale).ceil();
    let height = (f64::from(height) * scale).ceil();
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
        return Err(malformed("page", "invalid render dimensions"));
    }
    Ok((width as u32, height as u32))
}

fn image_bitmap_to_bmp(
    bitmap: &ImageBitmap,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), ConversionError> {
    encode_bmp(
        bitmap.width,
        bitmap.height,
        bitmap.stride,
        bitmap.format,
        &bitmap.bytes,
        options,
        context,
    )
}

fn rendered_bitmap_to_bmp(
    bitmap: &Bitmap,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), ConversionError> {
    encode_bmp(
        bitmap.width,
        bitmap.height,
        bitmap.stride,
        PixelFormat::Bgra,
        &bitmap.bytes,
        options,
        context,
    )
}

#[allow(clippy::too_many_lines)]
fn encode_bmp(
    width: u32,
    height: u32,
    source_stride: u32,
    format: PixelFormat,
    source: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), ConversionError> {
    context.checkpoint()?;
    let source_size = u64::from(source_stride)
        .checked_mul(u64::from(height))
        .ok_or_else(|| resource("max_asset_bytes", "source bitmap size overflow"))?;
    if source_size != u64::try_from(source.len()).unwrap_or(u64::MAX) {
        return Err(malformed("image", "bitmap byte length does not match stride and height"));
    }
    let stride =
        width.checked_mul(4).ok_or_else(|| resource("max_asset_bytes", "BMP stride overflow"))?;
    let pixel_bytes = u64::from(stride)
        .checked_mul(u64::from(height))
        .ok_or_else(|| resource("max_asset_bytes", "BMP pixel size overflow"))?;
    let size = 54_u64
        .checked_add(pixel_bytes)
        .ok_or_else(|| resource("max_asset_bytes", "BMP size overflow"))?;
    if size > options.limits.max_asset_bytes {
        return Err(resource(
            "max_asset_bytes",
            format!("{size} > {}", options.limits.max_asset_bytes),
        ));
    }
    let reservation = context.reserve_memory(size)?;
    let capacity = usize::try_from(size)
        .map_err(|_| resource("max_asset_bytes", "BMP size does not fit usize"))?;
    let mut output =
        into_markdown_pdfium::fixed_zeroed_bytes(capacity).map_err(map_pdfium_error)?;
    let mut cursor = 0_usize;
    {
        let mut write = |bytes: &[u8]| -> Result<(), ConversionError> {
            let end = cursor
                .checked_add(bytes.len())
                .ok_or_else(|| malformed("image", "BMP write offset overflow"))?;
            output
                .get_mut(cursor..end)
                .ok_or_else(|| malformed("image", "BMP write exceeds planned buffer"))?
                .copy_from_slice(bytes);
            cursor = end;
            Ok(())
        };
        write(b"BM")?;
        write(
            &u32::try_from(size)
                .map_err(|_| resource("max_asset_bytes", "BMP exceeds 32-bit format"))?
                .to_le_bytes(),
        )?;
        write(&[0; 4])?;
        write(&54_u32.to_le_bytes())?;
        write(&40_u32.to_le_bytes())?;
        write(
            &i32::try_from(width)
                .map_err(|_| resource("max_asset_bytes", "BMP width exceeds format"))?
                .to_le_bytes(),
        )?;
        write(
            &i32::try_from(height)
                .map_err(|_| resource("max_asset_bytes", "BMP height exceeds format"))?
                .checked_neg()
                .ok_or_else(|| resource("max_asset_bytes", "BMP height cannot be negated"))?
                .to_le_bytes(),
        )?;
        write(&1_u16.to_le_bytes())?;
        write(&32_u16.to_le_bytes())?;
        write(&0_u32.to_le_bytes())?;
        write(
            &u32::try_from(pixel_bytes)
                .map_err(|_| resource("max_asset_bytes", "BMP pixels exceed 32-bit format"))?
                .to_le_bytes(),
        )?;
        write(&[0; 16])?;
        let bytes_per_pixel = match format {
            PixelFormat::Gray => 1,
            PixelFormat::Bgr => 3,
            PixelFormat::Bgrx | PixelFormat::Bgra => 4,
        };
        let minimum_source_stride = width
            .checked_mul(u32::try_from(bytes_per_pixel).unwrap_or(u32::MAX))
            .ok_or_else(|| malformed("image", "minimum source stride overflow"))?;
        if source_stride < minimum_source_stride {
            return Err(malformed("image", "bitmap stride is shorter than one pixel row"));
        }
        for row in 0..height {
            let row_start = usize::try_from(u64::from(row) * u64::from(source_stride))
                .map_err(|_| malformed("image", "row offset overflow"))?;
            for column in 0..width {
                let offset = row_start
                    .checked_add(
                        usize::try_from(column)
                            .unwrap_or(usize::MAX)
                            .checked_mul(bytes_per_pixel)
                            .ok_or_else(|| malformed("image", "pixel offset overflow"))?,
                    )
                    .ok_or_else(|| malformed("image", "pixel offset overflow"))?;
                let end = offset
                    .checked_add(bytes_per_pixel)
                    .ok_or_else(|| malformed("image", "pixel end overflow"))?;
                let pixel = source
                    .get(offset..end)
                    .ok_or_else(|| malformed("image", "pixel lies outside bitmap bytes"))?;
                match format {
                    PixelFormat::Gray => {
                        let gray = pixel[0];
                        write(&[gray, gray, gray, 255])?;
                    }
                    PixelFormat::Bgr | PixelFormat::Bgrx => {
                        write(&[pixel[0], pixel[1], pixel[2], 255])?;
                    }
                    PixelFormat::Bgra => write(pixel)?,
                }
            }
        }
    }
    if cursor != capacity {
        return Err(malformed("image", "BMP encoder length mismatch"));
    }
    Ok((output, reservation))
}

fn content_asset_id(prefix: &str, bytes: &[u8]) -> Result<String, ConversionError> {
    use std::fmt::Write as _;

    let mut id = String::new();
    id.try_reserve_exact(prefix.len().saturating_add(65))
        .map_err(|_| resource("max_memory_bytes", "asset ID allocation failed"))?;
    id.push_str(prefix);
    id.push('-');
    for byte in Sha256::digest(bytes) {
        write!(&mut id, "{byte:02x}")
            .map_err(|_| resource("max_memory_bytes", "asset ID formatting failed"))?;
    }
    Ok(id)
}

fn account_asset(
    bytes: &[u8],
    total: &mut u64,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let size = u64::try_from(bytes.len())
        .map_err(|_| resource("max_asset_bytes", "asset size does not fit u64"))?;
    *total = total
        .checked_add(size)
        .ok_or_else(|| resource("max_total_asset_bytes", "asset total overflow"))?;
    if *total > options.limits.max_total_asset_bytes {
        return Err(resource(
            "max_total_asset_bytes",
            format!("{} > {}", *total, options.limits.max_total_asset_bytes),
        ));
    }
    Ok(())
}

fn checked_count(
    current: usize,
    added: usize,
    maximum: usize,
    limit: &'static str,
) -> Result<usize, ConversionError> {
    let total = current.checked_add(added).ok_or_else(|| resource(limit, "count overflow"))?;
    if total > maximum {
        return Err(resource(limit, format!("{total} > {maximum}")));
    }
    Ok(total)
}

const COVERAGE_GRID: usize = 64;

#[derive(Clone)]
struct PageCoverage {
    occupied: [bool; COVERAGE_GRID * COVERAGE_GRID],
}

impl Default for PageCoverage {
    fn default() -> Self {
        Self { occupied: [false; COVERAGE_GRID * COVERAGE_GRID] }
    }
}

impl PageCoverage {
    #[allow(clippy::cast_precision_loss)]
    fn add(&mut self, bounds: Rect, info: &PageInfo) {
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return;
        }
        let cell_width = info.width_points / COVERAGE_GRID as f32;
        let cell_height = info.height_points / COVERAGE_GRID as f32;
        for row in 0..COVERAGE_GRID {
            let top = row as f32 * cell_height;
            let bottom = top + cell_height;
            for column in 0..COVERAGE_GRID {
                let left = column as f32 * cell_width;
                let right = left + cell_width;
                if left >= bounds.x
                    && right <= bounds.x + bounds.width
                    && top >= bounds.y
                    && bottom <= bounds.y + bounds.height
                {
                    self.occupied[row * COVERAGE_GRID + column] = true;
                }
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn ratio(&self) -> f64 {
        self.occupied.iter().filter(|occupied| **occupied).count() as f64
            / self.occupied.len() as f64
    }
}

fn map_pdfium_error(error: PdfiumError) -> ConversionError {
    match error {
        PdfiumError::Native { operation: "load_document", code: 4 | 5 } => {
            ConversionError::Encrypted
        }
        PdfiumError::Native { operation: "load_document", code } => {
            malformed("document", format!("PDFium rejected the PDF (native error {code})"))
        }
        PdfiumError::ResourceLimit { limit, actual, maximum } => {
            resource(limit, format!("{actual} > {maximum}"))
        }
        PdfiumError::InvalidPath(_)
        | PdfiumError::DigestMismatch { .. }
        | PdfiumError::BinaryValidation(_)
        | PdfiumError::Load(_)
        | PdfiumError::UnsupportedPlatform { .. } => ConversionError::ComponentUnavailable {
            component: "pdfium".into(),
            detail: error.to_string(),
        },
        PdfiumError::InvalidResult { operation, detail } => malformed(operation, detail),
        PdfiumError::Allocation { operation, bytes } => {
            resource("max_memory_bytes", format!("{operation} could not allocate {bytes} bytes"))
        }
        PdfiumError::Poisoned => ConversionError::Internal { detail: error.to_string() },
        PdfiumError::Native { operation, code } => {
            malformed(operation, format!("PDFium native error {code}"))
        }
    }
}

fn malformed(part: impl Into<String>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(part.into()), detail: detail.into() }
}

fn resource(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::SourceMetadata;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn backend_materialization_only_runs_after_exact_memory_permit() {
        let calls = AtomicUsize::new(0);
        let low = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 31,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        assert!(matches!(
            materialize_after_reserve(&low, 32, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let exact = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 32,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let ((), permit) = materialize_after_reserve(&exact, 32, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(permit);
        assert_eq!(exact.reserved_memory_bytes(), 0);
    }

    #[test]
    fn character_native_and_ir_peak_has_exact_permit_boundary_before_construction() {
        let count = 64_u32;
        let long_font_capacity = 4_096_usize;
        let font_bytes = long_font_capacity * usize::try_from(count).unwrap();
        let character_slots = usize::try_from(count).unwrap();
        let materialization = character_slots * std::mem::size_of::<Character>() + font_bytes;
        let required = character_working_set_bytes(
            u64::try_from(materialization).unwrap(),
            u64::try_from(font_bytes).unwrap(),
            count,
        )
        .unwrap();
        let info = PageInfo { width_points: 100.0, height_points: 200.0, rotation_degrees: 0 };
        let native_calls = AtomicUsize::new(0);
        let ir_calls = AtomicUsize::new(0);
        let materialize = || {
            native_calls.fetch_add(1, Ordering::SeqCst);
            let long_font = "F".repeat(4_096);
            Ok::<Vec<Character>, ConversionError>(
                (0..count)
                    .map(|index| Character {
                        index,
                        value: 'x',
                        bounds: PdfRect { left: 1.0, bottom: 1.0, right: 2.0, top: 2.0 },
                        font_name: Some(long_font.clone()),
                        font_size: 12.0,
                        angle_degrees: 0.0,
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let low = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: required - 1,
                ..Default::default()
            },
        );
        assert!(
            materialize_after_reserve(&low, required, || {
                let characters = materialize()?;
                ir_calls.fetch_add(1, Ordering::SeqCst);
                text_block(1, &info, &characters)
            })
            .is_err()
        );
        assert_eq!(native_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ir_calls.load(Ordering::SeqCst), 0);

        let exact = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits { max_memory_bytes: required, ..Default::default() },
        );
        let (block, permit) = materialize_after_reserve(&exact, required, || {
            let characters = materialize()?;
            ir_calls.fetch_add(1, Ordering::SeqCst);
            text_block(1, &info, &characters)
        })
        .unwrap();
        assert_eq!(native_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ir_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(block.block, Block::Paragraph(ref values) if values.len() == 64));
        drop(permit);
        assert_eq!(exact.reserved_memory_bytes(), 0);

        assert!(
            character_ir_allocation_bytes(0).unwrap() < character_ir_allocation_bytes(1).unwrap()
        );
        assert!(
            character_ir_allocation_bytes(4).unwrap() < character_ir_allocation_bytes(5).unwrap()
        );
    }

    #[test]
    fn incremental_output_inventory_has_combined_exact_boundary_and_precedes_allocation() {
        fn account_fixture(
            context: &ExecutionContext,
            calls: &AtomicUsize,
        ) -> Result<Vec<ResourceReservation>, ConversionError> {
            let mut retained = Vec::new();
            for bytes in [
                output_block_overhead(2)?,
                output_block_overhead(0)?,
                asset_record_overhead()?,
                asset_record_overhead()?, // duplicate processing is still charged
                diagnostic_overhead()?,
                output_block_overhead(0)?,
            ] {
                retain_output_bytes(context, &mut retained, bytes)?;
                calls.fetch_add(1, Ordering::SeqCst);
            }
            Ok(retained)
        }

        let measuring = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let calls = AtomicUsize::new(0);
        let retained = account_fixture(&measuring, &calls).unwrap();
        let required = measuring.reserved_memory_bytes();
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        drop(retained);

        let exact = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits { max_memory_bytes: required, ..Default::default() },
        );
        calls.store(0, Ordering::SeqCst);
        let retained = account_fixture(&exact, &calls).unwrap();
        assert_eq!(exact.reserved_memory_bytes(), required);
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        let output = ConverterOutput::new_with_memory_reservations(
            Document::default(),
            Vec::new(),
            Vec::new(),
            retained,
        )
        .account_retained(&exact)
        .unwrap();
        assert_eq!(exact.reserved_memory_bytes(), required);
        drop(output);
        assert_eq!(exact.reserved_memory_bytes(), 0);

        let low = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: required - 1,
                ..Default::default()
            },
        );
        calls.store(0, Ordering::SeqCst);
        assert!(matches!(
            account_fixture(&low, &calls),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert_eq!(low.reserved_memory_bytes(), 0);
    }

    #[test]
    fn coordinates_are_top_left_and_rotation_aware() {
        let rect = PdfRect { left: 10.0, bottom: 20.0, right: 30.0, top: 40.0 };
        let plain = PageInfo { width_points: 100.0, height_points: 200.0, rotation_degrees: 0 };
        assert_eq!(
            normalize_rect(rect, &plain).unwrap(),
            Rect { x: 10.0, y: 160.0, width: 20.0, height: 20.0 }
        );
        let rotated = PageInfo { width_points: 200.0, height_points: 100.0, rotation_degrees: 90 };
        assert_eq!(
            normalize_rect(rect, &rotated).unwrap(),
            Rect { x: 20.0, y: 10.0, width: 20.0, height: 20.0 }
        );
        let upside_down = PageInfo { rotation_degrees: 180, ..plain };
        assert_eq!(normalize_point(10.0, 20.0, &upside_down).unwrap(), (90.0, 20.0));
        let counter_clockwise =
            PageInfo { width_points: 200.0, height_points: 100.0, rotation_degrees: 270 };
        assert_eq!(normalize_point(10.0, 20.0, &counter_clockwise).unwrap(), (180.0, 90.0));
        assert_eq!(displayed_dimensions(&rotated), (200.0, 100.0));
        let extreme =
            PageInfo { width_points: f32::MAX, height_points: f32::MAX, rotation_degrees: 180 };
        assert!(
            normalize_rect(
                PdfRect { left: -f32::MAX, bottom: 0.0, right: 0.0, top: 1.0 },
                &extreme
            )
            .is_err()
        );
    }

    #[test]
    fn links_reject_dangerous_and_unrepresentable_targets() {
        assert!(
            safe_link_target(LinkTarget::ExternalUri("javascript:alert(1)".into()), 3).is_err()
        );
        assert!(
            safe_link_target(LinkTarget::ExternalUri("https://example.test/a".into()), 3).is_ok()
        );
        assert_eq!(
            safe_link_target(LinkTarget::InternalPage { page_index: 2 }, 3).unwrap(),
            "#pdf-page-3"
        );
        assert!(safe_link_target(LinkTarget::InternalPage { page_index: 3 }, 3).is_err());
    }

    #[test]
    fn bitmap_validation_is_fail_closed_before_indexing() {
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let bitmap = ImageBitmap {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgra,
            bytes: vec![0; 15],
        };
        assert!(image_bitmap_to_bmp(&bitmap, &ConversionOptions::default(), &context).is_err());

        let short_stride = ImageBitmap {
            width: 2,
            height: 2,
            stride: 4,
            format: PixelFormat::Bgr,
            bytes: vec![0; 8],
        };
        assert!(
            image_bitmap_to_bmp(&short_stride, &ConversionOptions::default(), &context).is_err()
        );
    }

    #[test]
    fn bmp_output_memory_has_an_exact_boundary() {
        let bitmap = ImageBitmap {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgra,
            bytes: vec![0; 16],
        };
        for (limit, succeeds) in [(70, true), (69, false)] {
            let context = ExecutionContext::new(
                into_markdown_core::ExecutionOptions::default(),
                into_markdown_core::ResourceLimits {
                    max_memory_bytes: limit,
                    ..into_markdown_core::ResourceLimits::default()
                },
            );
            assert_eq!(
                image_bitmap_to_bmp(&bitmap, &ConversionOptions::default(), &context).is_ok(),
                succeeds
            );
        }
    }

    #[test]
    fn scan_coverage_is_union_based_and_blank_or_overlapping_small_images_do_not_qualify() {
        let info = PageInfo { width_points: 100.0, height_points: 100.0, rotation_degrees: 0 };
        let mut blank = PageCoverage::default();
        assert!(blank.ratio().abs() < f64::EPSILON);
        let small = Rect { x: 0.0, y: 0.0, width: 30.0, height: 30.0 };
        for _ in 0..100 {
            blank.add(small, &info);
        }
        assert!(blank.ratio() < MIN_SCAN_IMAGE_COVERAGE);
        blank.add(Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 }, &info);
        assert!((blank.ratio() - 1.0).abs() < f64::EPSILON);

        let mut below = PageCoverage::default();
        below.add(Rect { x: 0.0, y: 0.0, width: 49.9, height: 100.0 }, &info);
        assert!(below.ratio() < 0.5);
        let mut boundary = PageCoverage::default();
        boundary.add(Rect { x: 0.0, y: 0.0, width: 50.0, height: 100.0 }, &info);
        assert!((boundary.ratio() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn conversion_gate_wait_honors_cancellation() {
        let held = PDF_CONVERSION_GATE.get_or_init(|| Mutex::new(())).lock().unwrap();
        let cancellation = into_markdown_core::CancellationToken::new();
        cancellation.cancel();
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions {
                cancellation,
                ..into_markdown_core::ExecutionOptions::default()
            },
            into_markdown_core::ResourceLimits::default(),
        );
        assert!(matches!(lock_pdf_conversion(&context), Err(ConversionError::Cancelled)));
        drop(held);
    }

    #[test]
    fn total_asset_budget_counts_bytes_before_deduplication() {
        let mut options = ConversionOptions::default();
        options.limits.max_total_asset_bytes = 7;
        let mut total = 0;
        account_asset(b"same", &mut total, &options).unwrap();
        assert!(matches!(
            account_asset(b"same", &mut total, &options),
            Err(ConversionError::ResourceLimit { limit: "max_total_asset_bytes", .. })
        ));
    }

    #[test]
    #[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
    #[allow(clippy::too_many_lines)]
    fn native_production_converter_is_serialized_and_emits_unified_ir() {
        let path = PathBuf::from(std::env::var_os("PDFIUM_LIBRARY").expect("PDFIUM_LIBRARY"));
        let bytes: Arc<[u8]> = Arc::from(rotated_pdf());
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..2 {
                let path = path.clone();
                let bytes = Arc::clone(&bytes);
                handles.push(scope.spawn(move || {
                    let input = ResolvedInput { bytes, metadata: SourceMetadata::default() };
                    let context = ExecutionContext::new(
                        into_markdown_core::ExecutionOptions::default(),
                        into_markdown_core::ResourceLimits::default(),
                    );
                    let output =
                        convert_pdf(&path, &input, &ConversionOptions::default(), &context)
                            .expect("both serialized conversions succeed");
                    assert_eq!(output.document.blocks.len(), 4);
                    assert!(!output.assets.is_empty());
                    assert!(output.document.to_json().is_ok());
                    let dimensions = output
                        .document
                        .blocks
                        .iter()
                        .map(|page| {
                            (
                                page.provenance.locator.page,
                                page.provenance.locator.page_width,
                                page.provenance.locator.page_height,
                                page.provenance.locator.rotation_degrees,
                            )
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        dimensions,
                        vec![
                            (Some(1), Some(100.0), Some(200.0), Some(0.0)),
                            (Some(2), Some(200.0), Some(100.0), Some(90.0)),
                            (Some(3), Some(100.0), Some(200.0), Some(180.0)),
                            (Some(4), Some(200.0), Some(100.0), Some(270.0)),
                        ]
                    );
                    let image_bounds = [
                        Rect { x: 10.0, y: 150.0, width: 20.0, height: 30.0 },
                        Rect { x: 20.0, y: 10.0, width: 30.0, height: 20.0 },
                        Rect { x: 70.0, y: 20.0, width: 20.0, height: 30.0 },
                        Rect { x: 150.0, y: 70.0, width: 30.0, height: 20.0 },
                    ];
                    let external_bounds = [
                        Rect { x: 10.0, y: 30.0, width: 30.0, height: 20.0 },
                        Rect { x: 150.0, y: 10.0, width: 20.0, height: 30.0 },
                        Rect { x: 60.0, y: 150.0, width: 30.0, height: 20.0 },
                        Rect { x: 30.0, y: 60.0, width: 20.0, height: 30.0 },
                    ];
                    let internal_bounds = [
                        Rect { x: 50.0, y: 80.0, width: 40.0, height: 20.0 },
                        Rect { x: 100.0, y: 50.0, width: 20.0, height: 40.0 },
                        Rect { x: 10.0, y: 100.0, width: 40.0, height: 20.0 },
                        Rect { x: 80.0, y: 10.0, width: 20.0, height: 40.0 },
                    ];
                    let Block::Page { blocks: first_blocks, .. } = &output.document.blocks[0].block
                    else {
                        panic!("page")
                    };
                    let Block::Paragraph(first_inlines) = &first_blocks[0].block else {
                        panic!("text")
                    };
                    let Inline::SourceText { provenance, .. } = &first_inlines[0] else {
                        panic!("character")
                    };
                    let first_character_bounds = provenance.locator.bounds.unwrap();
                    let raw_character = PdfRect {
                        left: first_character_bounds.x,
                        right: first_character_bounds.x + first_character_bounds.width,
                        top: 200.0 - first_character_bounds.y,
                        bottom: 200.0 - first_character_bounds.y - first_character_bounds.height,
                    };
                    for (index, page) in output.document.blocks.iter().enumerate() {
                        let Block::Page { blocks, .. } = &page.block else { panic!("page") };
                        let image = blocks
                            .iter()
                            .find(|block| matches!(block.block, Block::Image { .. }))
                            .unwrap();
                        assert_eq!(image.provenance.locator.bounds, Some(image_bounds[index]));
                        let Block::Paragraph(inlines) = &blocks[0].block else { panic!("text") };
                        let Inline::SourceText { provenance, .. } = &inlines[0] else {
                            panic!("character")
                        };
                        let character = provenance.locator.bounds.unwrap();
                        let info = PageInfo {
                            width_points: page.provenance.locator.page_width.unwrap(),
                            height_points: page.provenance.locator.page_height.unwrap(),
                            rotation_degrees: u16::try_from(index * 90).unwrap(),
                        };
                        assert_eq!(character, normalize_rect(raw_character, &info).unwrap());
                        let mut found_external = false;
                        let mut found_internal = false;
                        for block in blocks {
                            let Block::Paragraph(inlines) = &block.block else { continue };
                            let Some(Inline::Link { target, .. }) = inlines.first() else {
                                continue;
                            };
                            if target == "https://example.test/rotated" {
                                assert_eq!(
                                    block.provenance.locator.bounds,
                                    Some(external_bounds[index])
                                );
                                found_external = true;
                            } else if target == "#pdf-page-2" {
                                assert_eq!(
                                    block.provenance.locator.bounds,
                                    Some(internal_bounds[index])
                                );
                                found_internal = true;
                            }
                        }
                        assert!(found_external && found_internal);
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });

        let input =
            ResolvedInput { bytes: Arc::from(rotated_pdf()), metadata: SourceMetadata::default() };
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let mut always = ConversionOptions::default();
        always.ocr.policy = OcrPolicy::Always;
        let output = convert_pdf(&path, &input, &always, &context).unwrap();
        assert!(output.assets.iter().any(|asset| asset.id.0.starts_with("pdf-page-render-")));

        let mut off = ConversionOptions::default();
        off.ocr.policy = OcrPolicy::Off;
        let output = convert_pdf(&path, &input, &off, &context).unwrap();
        assert!(!output.assets.iter().any(|asset| asset.id.0.starts_with("pdf-page-render-")));
        let markdown =
            into_markdown_render_markdown::render(&output.document, &output.assets, &off).unwrap();
        assert!(markdown.contains("https://example.test/rotated"));
        assert!(markdown.contains("#pdf-page-2"));
        assert!(markdown.contains("<a id=\"pdf-page-2\"></a>"));

        let auto = ConversionOptions::default();
        for (fixture, rendered) in
            [(text_only_pdf(), false), (mixed_pdf(), false), (scanned_pdf(), true)]
        {
            let input =
                ResolvedInput { bytes: Arc::from(fixture), metadata: SourceMetadata::default() };
            let output = convert_pdf(&path, &input, &auto, &context).unwrap();
            assert_eq!(
                output.assets.iter().any(|asset| asset.id.0.starts_with("pdf-page-render-")),
                rendered
            );
        }

        let modest = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 64 * 1024 * 1024,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        assert!(convert_pdf(&path, &input, &auto, &modest).is_ok());

        let mut page_limited = ConversionOptions::default();
        page_limited.limits.max_pages = 3;
        assert!(matches!(
            convert_pdf(&path, &input, &page_limited, &context),
            Err(ConversionError::ResourceLimit { limit: "max_pages", .. })
        ));

        let low = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 1,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        assert!(matches!(
            convert_pdf(&path, &input, &ConversionOptions::default(), &low),
            Err(ConversionError::ResourceLimit { .. })
        ));
        let damaged = ResolvedInput {
            bytes: Arc::from(b"%PDF-1.4\nbroken".as_slice()),
            metadata: SourceMetadata::default(),
        };
        assert!(matches!(
            convert_pdf(&path, &damaged, &ConversionOptions::default(), &context),
            Err(ConversionError::Malformed { .. })
        ));
        let encrypted = ResolvedInput {
            bytes: Arc::from(encrypted_pdf()),
            metadata: SourceMetadata::default(),
        };
        assert!(matches!(
            convert_pdf(&path, &encrypted, &ConversionOptions::default(), &context),
            Err(ConversionError::Encrypted)
        ));
    }

    fn rotated_pdf() -> Vec<u8> {
        let content = b"BT /F1 12 Tf 10 160 Td (Rotated) Tj ET\nq 20 0 0 30 10 20 cm /Im1 Do Q\n";
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R] /Count 4 >>".to_vec(),
        ];
        for rotation in [0, 90, 180, 270] {
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Rotate {rotation} /Resources << /Font << /F1 8 0 R >> /XObject << /Im1 9 0 R >> >> /Contents 7 0 R /Annots [10 0 R 11 0 R] >>").into_bytes());
        }
        objects.extend([
            stream_object("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            stream_object("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[255, 0, 0]),
            b"<< /Type /Annot /Subtype /Link /Rect [10 150 40 170] /A << /S /URI /URI (https://example.test/rotated) >> >>".to_vec(),
            b"<< /Type /Annot /Subtype /Link /Rect [50 100 90 120] /Dest [4 0 R /Fit] >>".to_vec(),
        ]);
        assemble_pdf(&objects)
    }

    fn text_only_pdf() -> Vec<u8> {
        one_page_fixture(b"BT /F1 12 Tf 10 160 Td (Text only page) Tj ET\n", false)
    }

    fn mixed_pdf() -> Vec<u8> {
        one_page_fixture(
            b"BT /F1 12 Tf 10 160 Td (Mixed page text) Tj ET\nq 20 0 0 30 10 20 cm /Im1 Do Q\n",
            true,
        )
    }

    fn scanned_pdf() -> Vec<u8> {
        one_page_fixture(b"q 100 0 0 200 0 0 cm /Im1 Do Q\n", true)
    }

    fn encrypted_pdf() -> Vec<u8> {
        let content = b"BT /F1 12 Tf 10 160 Td (Encrypted) Tj ET\n";
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            stream_object("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            b"<< /Filter /Standard /V 1 /R 2 /O <0000000000000000000000000000000000000000000000000000000000000000> /U <0000000000000000000000000000000000000000000000000000000000000000> /P -4 >>".to_vec(),
        ];
        assemble_pdf_with_trailer(
            &objects,
            "/Encrypt 6 0 R /ID [<00112233445566778899aabbccddeeff><00112233445566778899aabbccddeeff>]",
        )
    }

    fn one_page_fixture(content: &[u8], image: bool) -> Vec<u8> {
        let resources = if image {
            "<< /Font << /F1 5 0 R >> /XObject << /Im1 6 0 R >> >>"
        } else {
            "<< /Font << /F1 5 0 R >> >>"
        };
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Resources {resources} /Contents 4 0 R >>").into_bytes(),
            stream_object("", content),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        if image {
            objects.push(stream_object("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[255, 0, 0]));
        }
        assemble_pdf(&objects)
    }

    fn stream_object(dictionary: &str, bytes: &[u8]) -> Vec<u8> {
        let mut object =
            format!("<< {dictionary} /Length {} >>\nstream\n", bytes.len()).into_bytes();
        object.extend_from_slice(bytes);
        object.extend_from_slice(b"\nendstream");
        object
    }

    fn assemble_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
        assemble_pdf_with_trailer(objects, "")
    }

    fn assemble_pdf_with_trailer(objects: &[Vec<u8>], extra: &str) -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n%\x80\x80\x80\x80\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R {extra} >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1,
            )
            .as_bytes(),
        );
        pdf
    }
}
