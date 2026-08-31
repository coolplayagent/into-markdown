//! Shared page extraction and layout for aggregate and OCR-only PDF execution.

#[cfg(test)]
use super::IMAGE_BITMAP_MATERIALIZATIONS;
use super::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput,
    Diagnostic, DiagnosticSeverity, Document, ExecutionContext, HashSet, Limits,
    MAX_RENDER_DIMENSION, MIN_NATIVE_TEXT_CHARS, MIN_SCAN_IMAGE_COVERAGE, NodeId, OcrPolicy,
    PageCoverage, Path, Pdfium, Rect, ResourceReservation, account_asset,
    allocation_capacity_bound, asset_record_overhead, character_working_set_bytes, checked_count,
    content_asset_id, diagnostic_overhead, displayed_dimensions, image_bitmap_to_bmp,
    image_pixels_required, map_pdfium_error, materialize_after_reserve, normalize_rect,
    output_block_overhead, page_locator, provenance, render_dimensions, rendered_bitmap_to_bmp,
    request_path_scan, resource, retain_existing_reservation, retain_output_bytes, text_block,
};

#[derive(Default)]
pub(super) struct Counts {
    pub(super) asset_bytes: u64,
    pub(super) asset_ids: HashSet<String>,
    nodes: usize,
    inlines: usize,
    page_objects: u64,
}

impl Counts {
    pub(super) fn account_link(&mut self) -> Result<(), ConversionError> {
        self.nodes =
            checked_count(self.nodes, 1, into_markdown_core::MAX_DOCUMENT_NODES, "documentNodes")?;
        self.inlines = checked_count(
            self.inlines,
            2,
            into_markdown_core::MAX_DOCUMENT_INLINES,
            "documentInlines",
        )?;
        Ok(())
    }
    fn account_page_objects(
        &mut self,
        count: u32,
        page: u32,
        options: &ConversionOptions,
    ) -> Result<(), ConversionError> {
        if count > options.limits.max_pdf_page_objects {
            return Err(resource(
                "max_pdf_page_objects",
                format!("page {page}: {count} > {}", options.limits.max_pdf_page_objects),
            ));
        }
        let total = self.page_objects.checked_add(u64::from(count)).ok_or_else(|| {
            resource("max_pdf_total_objects", format!("page {page}: object count overflow"))
        })?;
        if total > options.limits.max_pdf_total_objects {
            return Err(resource(
                "max_pdf_total_objects",
                format!("page {page}: {total} > {}", options.limits.max_pdf_total_objects),
            ));
        }
        self.page_objects = total;
        Ok(())
    }
}

pub(super) struct PdfOutput {
    document: Document,
    assets: Vec<Asset>,
    diagnostics: Vec<Diagnostic>,
    retained_memory: Vec<ResourceReservation>,
    path_evidence: Vec<into_markdown_pdf_layout::PagePathEvidence>,
    path_memory: Vec<ResourceReservation>,
}

pub(super) fn load_runtime(
    runtime_path: &Path,
    options: &ConversionOptions,
) -> Result<Pdfium, ConversionError> {
    options.limits.validate_pdf()?;
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
        max_page_objects: options.limits.max_pdf_page_objects,
        max_password_bytes: 1024,
        max_bitmap_bytes: bitmap_limit,
        max_links_per_page: 10_000,
        max_link_bytes: 8 * 1024,
        max_font_name_bytes: 4 * 1024,
    };
    Pdfium::load_pinned(runtime_path, limits).map_err(map_pdfium_error)
}

impl PdfOutput {
    pub(super) fn new(pages: u32, context: &ExecutionContext) -> Result<Self, ConversionError> {
        let path_page_capacity =
            allocation_capacity_bound(usize::try_from(pages).unwrap_or(usize::MAX))?;
        let path_page_bytes = path_page_capacity
            .checked_mul(std::mem::size_of::<into_markdown_pdf_layout::PagePathEvidence>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| resource("max_memory_bytes", "PDF path page inventory overflow"))?;
        let path_page_memory = context.reserve_memory(path_page_bytes)?;
        let mut path_evidence = Vec::new();
        path_evidence.try_reserve_exact(path_page_capacity).map_err(|_| {
            resource("max_memory_bytes", "PDF path page inventory allocation failed")
        })?;
        if path_evidence.capacity() > path_page_capacity {
            return Err(resource(
                "max_memory_bytes",
                "PDF path page inventory capacity exceeded its plan",
            ));
        }
        let mut path_memory = Vec::new();
        retain_existing_reservation(context, &mut path_memory, path_page_memory)?;
        Ok(Self {
            document: Document::default(),
            assets: Vec::new(),
            diagnostics: Vec::new(),
            retained_memory: Vec::new(),
            path_evidence,
            path_memory,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn extract_page(
        self,
        pdf: &into_markdown_pdfium::Document<'_>,
        page_index: u32,
        options: &ConversionOptions,
        context: &ExecutionContext,
        counts: &mut Counts,
        ocr_only: bool,
    ) -> Result<Self, ConversionError> {
        let Self {
            mut document,
            mut assets,
            mut diagnostics,
            mut retained_memory,
            mut path_evidence,
            mut path_memory,
        } = self;
        let retain_image_pixels = image_pixels_required(options);
        context.checkpoint()?;
        let page_number = page_index + 1;
        let page = pdf.page(page_index).map_err(map_pdfium_error)?;
        let page_objects = page.object_count().map_err(map_pdfium_error)?;
        counts.account_page_objects(page_objects, page_number, options)?;
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
        counts.inlines = checked_count(
            counts.inlines,
            characters.len(),
            into_markdown_core::MAX_DOCUMENT_INLINES,
            "documentInlines",
        )?;
        let mut blocks = Vec::new();
        if !characters.is_empty() {
            counts.nodes = checked_count(
                counts.nodes,
                1,
                into_markdown_core::MAX_DOCUMENT_NODES,
                "documentNodes",
            )?;
            blocks
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "PDF block allocation failed"))?;
            blocks.push(text_block(page_number, &info, &characters)?);
        }
        super::links::PageLinks {
            number: page_number,
            info: &info,
            options,
            context,
            counts,
            blocks: &mut blocks,
            diagnostics: &mut diagnostics,
            retained: &mut retained_memory,
        }
        .extract(&text_page, pdf.page_count())?;

        let mut coverage = PageCoverage::default();
        let image_plan = page.plan_images().map_err(map_pdfium_error)?;
        let image_plan_bytes = image_plan.allocation_bytes();
        let (images, image_metadata_memory) =
            materialize_after_reserve(context, image_plan_bytes, || {
                image_plan.materialize().map_err(map_pdfium_error)
            })?;
        for image in &images {
            context.checkpoint()?;
            coverage.add(normalize_rect(image.bounds(), &info)?, &info);
        }
        let printable = characters
            .iter()
            .filter(|character| !character.value.is_control() && !character.value.is_whitespace())
            .count();
        let scanned =
            printable < MIN_NATIVE_TEXT_CHARS && coverage.ratio() >= MIN_SCAN_IMAGE_COVERAGE;
        let render_requested = options.ocr.policy == OcrPolicy::Always
            || (options.ocr.policy == OcrPolicy::Auto && scanned);
        for image in images {
            context.checkpoint()?;
            // A full displayed-page render already includes every embedded
            // image. Do not decode or recognize that same page a second time
            // when no permanent image output was requested.
            if ocr_only && render_requested {
                continue;
            }
            let bounds = normalize_rect(image.bounds(), &info)?;
            let id = if retain_image_pixels {
                #[cfg(test)]
                IMAGE_BITMAP_MATERIALIZATIONS.set(IMAGE_BITMAP_MATERIALIZATIONS.get() + 1);
                let bitmap_plan = image.plan_bitmap().map_err(map_pdfium_error)?;
                let bitmap_plan_bytes = bitmap_plan.allocation_bytes();
                let (bitmap, bitmap_memory) =
                    materialize_after_reserve(context, bitmap_plan_bytes, || {
                        bitmap_plan.materialize().map_err(map_pdfium_error)
                    })?;
                let (encoded, encoded_memory) = image_bitmap_to_bmp(&bitmap, options, context)?;
                drop(bitmap);
                drop(bitmap_memory);
                account_asset(&encoded, &mut counts.asset_bytes, options)?;
                retain_output_bytes(context, &mut retained_memory, asset_record_overhead()?)?;
                let id = content_asset_id("pdf-image", &encoded)?;
                counts
                    .asset_ids
                    .try_reserve(1)
                    .map_err(|_| resource("max_memory_bytes", "asset ID allocation failed"))?;
                if counts.asset_ids.insert(id.clone()) {
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
                id
            } else {
                // Preserve image geometry through reading-order reconstruction.
                // This transient reference is removed before publishing the IR.
                "pdf-omitted-image".into()
            };
            counts.nodes = checked_count(
                counts.nodes,
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

        if render_requested {
            let (width, height) = render_dimensions(&info)?;
            let (page_width, page_height) = displayed_dimensions(&info);
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
            account_asset(&encoded, &mut counts.asset_bytes, options)?;
            retain_output_bytes(context, &mut retained_memory, asset_record_overhead()?)?;
            let id = content_asset_id("pdf-page-render", &encoded)?;
            counts
                .asset_ids
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "asset ID allocation failed"))?;
            if counts.asset_ids.insert(id.clone()) {
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
            counts.nodes = checked_count(
                counts.nodes,
                1,
                into_markdown_core::MAX_DOCUMENT_NODES,
                "documentNodes",
            )?;
            retain_output_bytes(context, &mut retained_memory, output_block_overhead(0)?)?;
            blocks
                .try_reserve(1)
                .map_err(|_| resource("max_memory_bytes", "PDF block allocation failed"))?;
            let mut rendered_provenance = provenance(
                page_number,
                Some(Rect { x: 0.0, y: 0.0, width: page_width, height: page_height }),
                None,
                &info,
            )?;
            // PDFium already applied page rotation to this displayed-page
            // bitmap. The OCR frame must not rotate its polygons a second time.
            rendered_provenance.locator.rotation_degrees = Some(0.0);
            blocks.push(BlockNode {
                id: NodeId(format!("pdf-page-{page_number}-ocr-render")),
                block: Block::Image { asset: AssetId(id), alt: Some("page render for OCR".into()) },
                provenance: rendered_provenance,
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
        counts.nodes = checked_count(
            counts.nodes,
            1,
            into_markdown_core::MAX_DOCUMENT_NODES,
            "documentNodes",
        )?;
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
        Ok(Self { document, assets, diagnostics, retained_memory, path_evidence, path_memory })
    }

    pub(super) fn finish(
        self,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<ConverterOutput, ConversionError> {
        let Self {
            mut document,
            assets,
            diagnostics,
            mut retained_memory,
            path_evidence,
            path_memory,
        } = self;
        let retain_image_pixels = image_pixels_required(options);
        let layout_config = into_markdown_pdf_layout::LayoutConfig {
            limits: into_markdown_pdf_layout::LayoutLimits {
                max_atoms: into_markdown_core::MAX_DOCUMENT_INLINES,
                max_lines: into_markdown_core::MAX_DOCUMENT_NODES,
                max_comparisons: options.limits.max_pdf_layout_comparisons,
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
        if !retain_image_pixels {
            for page in &mut document.blocks {
                if let Block::Page { blocks, .. } = &mut page.block {
                    blocks.retain(|node| !matches!(node.block, Block::Image { .. }));
                }
            }
        }
        if let Some(reservation) = layout_reservation {
            retain_existing_reservation(context, &mut retained_memory, reservation)?;
        }
        let validation_bytes =
            into_markdown_core::estimate_validation_working_set(&document, &assets, &diagnostics)?;
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
}

#[cfg(test)]
mod object_budget_tests {
    use super::*;
    #[test]
    fn cumulative_object_overflow_and_ir_nodes_fail_independently() {
        let mut counts = Counts { page_objects: u64::MAX, ..Counts::default() };
        let mut options = ConversionOptions::default();
        options.limits.max_pdf_total_objects = u64::MAX;
        assert!(
            matches!(counts.account_page_objects(1, 7, &options), Err(ConversionError::ResourceLimit { limit: "max_pdf_total_objects", detail }) if detail.contains("page 7") && detail.contains("overflow"))
        );
        let mut counts =
            Counts { nodes: into_markdown_core::MAX_DOCUMENT_NODES, ..Counts::default() };
        assert!(matches!(
            counts.account_link(),
            Err(ConversionError::ResourceLimit { limit: "documentNodes", .. })
        ));
    }
}
