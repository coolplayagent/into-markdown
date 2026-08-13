use super::{MergeConfig, OcrPageInput, limit, ocr};
use into_markdown_core::{
    Block, ConversionError, Diagnostic, Document, ExecutionContext, Inline, ResourceReservation,
    estimate_retained_output, estimate_validation_working_set,
};

pub(crate) struct MergeBudget<'a> {
    reservation: Option<ResourceReservation>,
    context: &'a ExecutionContext,
    remaining_work: u64,
    checkpoint_work: u64,
    planned_diagnostics: usize,
}

impl<'a> MergeBudget<'a> {
    pub(crate) fn preflight(
        document: &Document,
        pages: &[OcrPageInput],
        config: &MergeConfig,
        context: &'a ExecutionContext,
    ) -> Result<Self, ConversionError> {
        context.checkpoint()?;
        let empty_assets = Vec::new();
        let empty_diagnostics = Vec::<Diagnostic>::new();
        let validation_bytes =
            estimate_validation_working_set(document, &empty_assets, &empty_diagnostics)?;
        let mut reservation = context.reserve_memory(validation_bytes)?;
        document.validate().map_err(|error| {
            ocr(format!("invalidInputDocument:{}:{}", error.code.as_str(), error.path))
        })?;
        if pages.len() > config.limits.max_pages {
            return Err(limit("ocrMergePages", pages.len(), config.limits.max_pages));
        }
        let inventory = document_inventory(document, context)?;
        let mut regions = 0_usize;
        let mut text_bytes = 0_usize;
        let mut identity_bytes = 0_usize;
        for page in pages {
            page.recognition.validate_identity(&page.detection.identity)?;
            page.recognition.validate_payload(context)?;
            let page_blocks = super::existing_page_blocks(document, page.page())?;
            if config.policy == into_markdown_core::OcrPolicy::Auto
                && super::policy::has_sufficient_native_text(
                    page_blocks.blocks,
                    page.page(),
                    page_blocks.explicitly_scoped,
                    config.auto_min_native_characters,
                    context,
                )?
            {
                continue;
            }
            regions = regions
                .checked_add(page.detected().regions.len())
                .ok_or_else(|| limit("ocrMergeRegions", usize::MAX, config.limits.max_regions))?;
            text_bytes = page.recognition.result().regions.iter().try_fold(
                text_bytes,
                |total, region| {
                    total.checked_add(region.text.len()).ok_or_else(|| {
                        limit("ocrMergeTextBytes", usize::MAX, config.limits.max_text_bytes)
                    })
                },
            )?;
            identity_bytes = [
                page.detected().provider.len(),
                page.recognition.result().provider.len(),
                page.detection.identity.detector_model.len(),
                page.recognition.recognizer_model().len(),
            ]
            .into_iter()
            .try_fold(identity_bytes, |total, bytes| {
                total.checked_add(bytes).ok_or_else(|| {
                    limit("ocrMergeIdentityBytes", usize::MAX, config.limits.max_identity_bytes)
                })
            })?;
        }
        if regions > config.limits.max_regions {
            return Err(limit("ocrMergeRegions", regions, config.limits.max_regions));
        }
        if text_bytes > config.limits.max_text_bytes {
            return Err(limit("ocrMergeTextBytes", text_bytes, config.limits.max_text_bytes));
        }
        if identity_bytes > config.limits.max_identity_bytes {
            return Err(limit(
                "ocrMergeIdentityBytes",
                identity_bytes,
                config.limits.max_identity_bytes,
            ));
        }
        let nodes_after = inventory
            .nodes
            .checked_add(regions)
            .and_then(|value| value.checked_add(pages.len()))
            .ok_or_else(|| {
                limit("documentNodes", usize::MAX, into_markdown_core::MAX_DOCUMENT_NODES)
            })?;
        if nodes_after > into_markdown_core::MAX_DOCUMENT_NODES {
            return Err(limit(
                "documentNodes",
                nodes_after,
                into_markdown_core::MAX_DOCUMENT_NODES,
            ));
        }
        let inlines_after = inventory
            .inlines
            .checked_add(regions.checked_mul(6).ok_or_else(|| {
                limit("documentInlines", usize::MAX, into_markdown_core::MAX_DOCUMENT_INLINES)
            })?)
            .ok_or_else(|| {
                limit("documentInlines", usize::MAX, into_markdown_core::MAX_DOCUMENT_INLINES)
            })?;
        if inlines_after > into_markdown_core::MAX_DOCUMENT_INLINES {
            return Err(limit(
                "documentInlines",
                inlines_after,
                into_markdown_core::MAX_DOCUMENT_INLINES,
            ));
        }

        // Dedup and line clustering each have a bounded worst-case candidate
        // comparison count. Preflight the complete bound before indexing.
        let maximum_work = usize::try_from(config.limits.max_comparisons).unwrap_or(usize::MAX);
        let pairs = u64::try_from(regions)
            .ok()
            .and_then(|count| count.checked_mul(count.saturating_sub(1)))
            .map(|value| value / 2)
            .ok_or_else(|| limit("ocrMergeWork", usize::MAX, maximum_work))?;
        let native_work = u64::try_from(regions)
            .ok()
            .and_then(|regions| regions.checked_mul(u64::try_from(inventory.nodes).ok()?))
            .ok_or_else(|| limit("ocrMergeWork", usize::MAX, maximum_work))?;
        let work = pairs
            .checked_mul(2)
            .and_then(|value| value.checked_add(native_work))
            .and_then(|value| value.checked_add(u64::try_from(regions).ok()?))
            .ok_or_else(|| limit("ocrMergeWork", usize::MAX, maximum_work))?;
        if work > config.limits.max_comparisons {
            return Err(limit(
                "ocrMergeWork",
                usize::try_from(work).unwrap_or(usize::MAX),
                usize::try_from(config.limits.max_comparisons).unwrap_or(usize::MAX),
            ));
        }

        let retained = estimate_retained_output(document, &empty_assets, &empty_diagnostics)?;
        let region_bytes = u64::try_from(regions)
            .ok()
            .and_then(|count| count.checked_mul(8_192))
            .ok_or_else(|| limit("max_memory_bytes", usize::MAX, usize::MAX))?;
        let string_bytes = u64::try_from(text_bytes)
            .ok()
            .and_then(|bytes| bytes.checked_mul(8))
            .ok_or_else(|| limit("max_memory_bytes", usize::MAX, usize::MAX))?;
        let identity_working = u64::try_from(identity_bytes)
            .ok()
            .and_then(|bytes| bytes.checked_mul(u64::try_from(regions.max(1)).ok()?))
            .ok_or_else(|| limit("max_memory_bytes", usize::MAX, usize::MAX))?;
        let native_index_bytes = u64::try_from(inventory.inlines)
            .ok()
            .and_then(|count| count.checked_mul(64))
            .ok_or_else(|| limit("max_memory_bytes", usize::MAX, usize::MAX))?;
        let node_index_bytes = u64::try_from(inventory.nodes)
            .ok()
            .and_then(|count| count.checked_mul(128))
            .and_then(|bytes| bytes.checked_add(u64::try_from(inventory.node_id_bytes).ok()?))
            .ok_or_else(|| limit("max_memory_bytes", usize::MAX, usize::MAX))?;
        let native_canonical_bytes = u64::try_from(inventory.native_text_bytes)
            .ok()
            .and_then(|bytes| bytes.checked_mul(3))
            .ok_or_else(|| limit("max_memory_bytes", usize::MAX, usize::MAX))?;
        let page_index_bytes = u64::try_from(pages.len())
            .ok()
            .and_then(|count| count.checked_mul(16))
            .ok_or_else(|| limit("max_memory_bytes", usize::MAX, usize::MAX))?;
        let planned = retained
            .checked_add(region_bytes)
            .and_then(|value| value.checked_add(string_bytes))
            .and_then(|value| value.checked_add(identity_working))
            .and_then(|value| value.checked_add(native_index_bytes))
            .and_then(|value| value.checked_add(node_index_bytes))
            .and_then(|value| value.checked_add(native_canonical_bytes))
            .and_then(|value| value.checked_add(page_index_bytes))
            .and_then(|value| value.checked_add(64 * 1024))
            .ok_or_else(|| limit("max_memory_bytes", usize::MAX, usize::MAX))?;
        if planned > validation_bytes {
            reservation.grow(planned - validation_bytes)?;
        }
        Ok(Self {
            reservation: Some(reservation),
            context,
            remaining_work: work,
            checkpoint_work: 0,
            planned_diagnostics: regions,
        })
    }

    pub(crate) const fn planned_diagnostics(&self) -> usize {
        self.planned_diagnostics
    }

    pub(crate) fn consume(&mut self, units: u64) -> Result<(), ConversionError> {
        self.remaining_work =
            self.remaining_work.checked_sub(units).ok_or_else(|| ocr("mergeWorkPlanExceeded"))?;
        self.checkpoint_work = self.checkpoint_work.saturating_add(units);
        if self.checkpoint_work >= 256 {
            self.context.checkpoint()?;
            self.checkpoint_work = 0;
        }
        Ok(())
    }

    pub(crate) fn checkpoint(&self) -> Result<(), ConversionError> {
        self.context.checkpoint()
    }

    pub(crate) const fn context(&self) -> &ExecutionContext {
        self.context
    }

    pub(crate) fn finish(mut self) -> Result<ResourceReservation, ConversionError> {
        self.context.checkpoint()?;
        self.reservation.take().ok_or_else(|| ocr("mergeReservationMissing"))
    }
}

#[derive(Default)]
struct Inventory {
    nodes: usize,
    inlines: usize,
    native_text_bytes: usize,
    node_id_bytes: usize,
}

fn document_inventory(
    document: &Document,
    context: &ExecutionContext,
) -> Result<Inventory, ConversionError> {
    let mut inventory = Inventory::default();
    let mut block_stack = Vec::new();
    block_stack.try_reserve_exact(1).map_err(|_| super::memory())?;
    block_stack.push(document.blocks.as_slice());
    let mut inline_stack = Vec::new();
    let mut visited = 0_usize;
    while let Some(values) = block_stack.pop() {
        for node in values {
            visited += 1;
            super::traversal_checkpoint(context, visited)?;
            inventory.nodes = inventory.nodes.checked_add(1).ok_or_else(|| {
                limit("documentNodes", usize::MAX, into_markdown_core::MAX_DOCUMENT_NODES)
            })?;
            inventory.node_id_bytes =
                inventory.node_id_bytes.checked_add(node.id.0.len()).ok_or_else(super::memory)?;
            match &node.block {
                Block::Paragraph(values)
                | Block::Heading { content: values, .. }
                | Block::TimedSegment { content: values, .. } => {
                    inline_stack.try_reserve(1).map_err(|_| super::memory())?;
                    inline_stack.push(values.as_slice());
                }
                Block::List { items, .. } => {
                    for item in items {
                        inventory.nodes = inventory.nodes.checked_add(1).ok_or_else(|| {
                            limit(
                                "documentNodes",
                                usize::MAX,
                                into_markdown_core::MAX_DOCUMENT_NODES,
                            )
                        })?;
                        block_stack.try_reserve(1).map_err(|_| super::memory())?;
                        block_stack.push(item.blocks.as_slice());
                    }
                }
                Block::Table { rows, .. } => {
                    for row in rows {
                        inventory.nodes = inventory.nodes.checked_add(1).ok_or_else(|| {
                            limit(
                                "documentNodes",
                                usize::MAX,
                                into_markdown_core::MAX_DOCUMENT_NODES,
                            )
                        })?;
                        for cell in &row.cells {
                            inventory.nodes = inventory.nodes.checked_add(1).ok_or_else(|| {
                                limit(
                                    "documentNodes",
                                    usize::MAX,
                                    into_markdown_core::MAX_DOCUMENT_NODES,
                                )
                            })?;
                            block_stack.try_reserve(1).map_err(|_| super::memory())?;
                            block_stack.push(cell.blocks.as_slice());
                        }
                    }
                }
                Block::Footnote { blocks: values, .. }
                | Block::Page { blocks: values, .. }
                | Block::Slide { blocks: values, .. }
                | Block::Sheet { blocks: values, .. } => {
                    block_stack.try_reserve(1).map_err(|_| super::memory())?;
                    block_stack.push(values.as_slice());
                }
                _ => {}
            }
        }
    }
    while let Some(values) = inline_stack.pop() {
        for value in values {
            visited += 1;
            super::traversal_checkpoint(context, visited)?;
            inventory.inlines = inventory.inlines.checked_add(1).ok_or_else(|| {
                limit("documentInlines", usize::MAX, into_markdown_core::MAX_DOCUMENT_INLINES)
            })?;
            match value {
                Inline::Text { value, .. }
                | Inline::SourceText { value, .. }
                | Inline::Code(value)
                | Inline::Formula(value) => {
                    inventory.native_text_bytes = inventory
                        .native_text_bytes
                        .checked_add(value.len())
                        .ok_or_else(super::memory)?;
                }
                Inline::Link { content, .. } => {
                    inline_stack.try_reserve(1).map_err(|_| super::memory())?;
                    inline_stack.push(content.as_slice());
                }
                Inline::OcrText { value, evidence, .. } => {
                    inventory.native_text_bytes = inventory
                        .native_text_bytes
                        .checked_add(value.len())
                        .ok_or_else(super::memory)?;
                    inventory.inlines = inventory
                        .inlines
                        .checked_add(evidence.regions.len())
                        .and_then(|count| count.checked_add(evidence.chain.len()))
                        .ok_or_else(|| {
                            limit(
                                "documentInlines",
                                usize::MAX,
                                into_markdown_core::MAX_DOCUMENT_INLINES,
                            )
                        })?;
                }
                _ => {}
            }
        }
    }
    Ok(inventory)
}
