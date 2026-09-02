//! Page-bounded OCR-only pixel lifetime; final semantic ownership moves once.

use super::{
    PdfConverter, convert_pdf_admitted, image_pixels_required, open_document, pages,
    runtime::acquire_pdf_conversion,
};
use into_markdown_core::{
    AiMode, AssetMode, Block, BlockNode, ConversionError, ConversionOptions, ConverterEventSink,
    ConverterOutput, ConverterStream, ConverterStreamCompletion, ConverterStreamMode, Diagnostic,
    ExecutionContext, FormatCandidate, LocalBoxFuture, ResolvedInput, Services, StreamConsumerKind,
    estimate_retained_output, stream_converter_output,
};

fn ocr_only_pixels(options: &ConversionOptions) -> bool {
    options.output.asset_mode == AssetMode::Omit
        && image_pixels_required(options)
        && [
            options.ai.image_description,
            options.ai.layout_repair,
            options.ai.table_repair,
            options.ai.formula_repair,
        ]
        .iter()
        .all(|mode| *mode == AiMode::Off)
}

impl ConverterStream for PdfConverter {
    fn stream_mode_for(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        options: &ConversionOptions,
        _: StreamConsumerKind,
    ) -> ConverterStreamMode {
        if ocr_only_pixels(options) {
            ConverterStreamMode::Native
        } else {
            ConverterStreamMode::AggregateAdapter
        }
    }

    fn convert_stream<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
        sink: &'a mut dyn ConverterEventSink,
    ) -> LocalBoxFuture<'a, Result<ConverterStreamCompletion, ConversionError>> {
        Box::pin(async move {
            let path = self.runtime_path()?;
            let _permit = acquire_pdf_conversion(context).await?;
            if !ocr_only_pixels(options) || !sink.supports_page_enrichment() {
                return stream_converter_output(
                    convert_pdf_admitted(&path, input, options, context)?,
                    sink,
                );
            }
            let runtime = pages::load_runtime(&path, options)?;
            let pdf = open_document(&runtime, input, context)?;
            let observed_pages = pdf.page_count();
            let selected_pages = observed_pages.min(options.limits.max_pages);
            let mut counts = pages::Counts::default();
            let mut output = ConverterOutput::default();
            if observed_pages > selected_pages {
                output
                    .diagnostics
                    .push(super::page_truncation_diagnostic(observed_pages, selected_pages));
            }
            for page_index in 0..selected_pages {
                context.checkpoint()?;
                sink.checkpoint()?;
                let page = pages::PdfOutput::new(1, context)?
                    .extract_page(&pdf, page_index, options, context, &mut counts, true)?
                    .finish(options, context)?;
                let page = sink.enrich_page(page).await?;
                append_consumed_page(&mut output, page, context)?;
                // Reset only after the page's actual bitmap allocations AND
                // their leases were destroyed. Permanent asset modes never use
                // this branch and keep document-wide byte/dedup accounting.
                counts.asset_bytes = 0;
                counts.asset_ids.clear();
            }
            // Keep document-wide native/OCR layout and running-matter policy,
            // now over semantic text only, without retaining page pixels.
            let output = crate::pdf_ocr::reconstruct_enriched_pdf(output, options, context)?;
            stream_converter_output(output.account_retained(context)?, sink)
        })
    }
}

fn append_consumed_page(
    output: &mut ConverterOutput,
    mut page: ConverterOutput,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    remove_images(&mut page.document.blocks, context)?;
    page.assets = Vec::new();
    let retained = estimate_retained_output(&page.document, &page.assets, &page.diagnostics)?;
    let compact = context.reserve_memory(retained)?;
    // Replace the now-freed pixel/codec/layout peak leases with only the live
    // semantic output. Ownership is covered throughout the transition.
    let mut page = page.certify_preflight_reservation(context, compact)?;
    let growth = page
        .document
        .blocks
        .len()
        .checked_mul(size_of::<BlockNode>())
        .and_then(|bytes| {
            bytes.checked_add(page.diagnostics.len().checked_mul(size_of::<Diagnostic>())?)
        })
        .and_then(|bytes| bytes.checked_mul(2))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| super::resource("max_memory_bytes", "PDF page collector growth overflow"))?;
    let growth = context.reserve_memory(growth)?;
    output
        .document
        .blocks
        .try_reserve(page.document.blocks.len())
        .map_err(|_| super::resource("max_memory_bytes", "PDF page collector allocation failed"))?;
    output.diagnostics.try_reserve(page.diagnostics.len()).map_err(|_| {
        super::resource("max_memory_bytes", "PDF diagnostic collector allocation failed")
    })?;
    output.document.blocks.append(&mut page.document.blocks);
    output.diagnostics.append(&mut page.diagnostics);
    output.absorb_memory_lease(&mut page, context)?;
    output.attach_memory_reservation(context, growth)
}

fn remove_images(
    nodes: &mut Vec<BlockNode>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for node in nodes.iter_mut() {
        context.checkpoint()?;
        match &mut node.block {
            Block::Page { blocks, .. }
            | Block::Footnote { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => remove_images(blocks, context)?,
            Block::List { items, .. } => {
                for item in items {
                    remove_images(&mut item.blocks, context)?;
                }
            }
            Block::Table { rows, .. } => {
                for cell in rows.iter_mut().flat_map(|row| &mut row.cells) {
                    remove_images(&mut cell.blocks, context)?;
                }
            }
            _ => {}
        }
    }
    nodes.retain(|node| !matches!(node.block, Block::Image { .. }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ocr_only_omitted_pixels_use_page_consumption() {
        let mut options = ConversionOptions::default();
        options.output.asset_mode = AssetMode::Omit;
        assert!(ocr_only_pixels(&options));
        options.ocr.policy = into_markdown_core::OcrPolicy::Off;
        assert!(!ocr_only_pixels(&options));
        options.ai.vision_ocr = AiMode::Only;
        assert!(ocr_only_pixels(&options));
        options.ai.image_description = AiMode::Prefer;
        assert!(!ocr_only_pixels(&options));
        options.ai.image_description = AiMode::Off;
        options.output.asset_mode = AssetMode::Extract;
        assert!(!ocr_only_pixels(&options));
    }
}
