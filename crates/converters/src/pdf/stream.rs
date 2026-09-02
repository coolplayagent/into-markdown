//! Page-bounded PDF OCR working pixels; only publishable source assets survive.

use super::{
    PdfConverter, convert_pdf_admitted, open_document, pages, runtime::acquire_pdf_conversion,
};
use into_markdown_core::{
    Asset, AssetId, AssetMode, Block, BlockNode, ConversionError, ConversionOptions,
    ConverterEventSink, ConverterOutput, ConverterStream, ConverterStreamCompletion,
    ConverterStreamMode, Diagnostic, ExecutionContext, FormatCandidate, LocalBoxFuture, OcrPolicy,
    ResolvedInput, Services, StreamConsumerKind, stream_converter_output,
};
use std::collections::HashSet;

fn page_ocr_requested(options: &ConversionOptions) -> bool {
    crate::embedded_visual_ocr::effective_ocr_policy(options) != OcrPolicy::Off
}

impl ConverterStream for PdfConverter {
    fn stream_mode_for(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        options: &ConversionOptions,
        _: StreamConsumerKind,
    ) -> ConverterStreamMode {
        if page_ocr_requested(options) {
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
            if !page_ocr_requested(options) {
                return stream_converter_output(
                    convert_pdf_admitted(&path, input, options, context)?,
                    sink,
                );
            }
            if !sink.supports_page_enrichment() {
                let output = convert_pdf_admitted(&path, input, options, context)?;
                let output = super::working_visual::discard(output, context)?;
                return stream_converter_output(output, sink);
            }
            let runtime = pages::load_runtime(&path, options)?;
            let pdf = open_document(&runtime, input, context)?;
            let observed_pages = pdf.page_count();
            let selected_pages = observed_pages.min(options.limits.max_pages);
            let mut counts = pages::Counts::default();
            let mut output = ConverterOutput::default();
            let mut published_asset_ids = HashSet::new();
            if observed_pages > selected_pages {
                output
                    .diagnostics
                    .push(super::page_truncation_diagnostic(observed_pages, selected_pages));
            }
            for page_index in 0..selected_pages {
                context.checkpoint()?;
                sink.checkpoint()?;
                let page = pages::PdfOutput::new(1, context)?
                    .extract_page(
                        &pdf,
                        page_index,
                        options,
                        context,
                        &mut counts,
                        options.output.asset_mode == AssetMode::Omit,
                    )?
                    .finish(options, context)?;
                let page = sink.enrich_page(page).await?;
                append_page(
                    &mut output,
                    page,
                    options.output.asset_mode,
                    &mut published_asset_ids,
                    context,
                )?;
                // Every page must carry the payloads needed by its own OCR
                // transaction. The final collector performs document-wide
                // content-ID deduplication after those working bytes are gone.
                counts.asset_ids.clear();
                if options.output.asset_mode == AssetMode::Omit {
                    counts.asset_bytes = 0;
                }
            }
            // Keep document-wide native/OCR layout and running-matter policy,
            // now over semantic text only, without retaining page pixels.
            let output = crate::pdf_ocr::reconstruct_enriched_pdf(output, options, context)?;
            stream_converter_output(output.account_retained(context)?, sink)
        })
    }
}

fn append_page(
    output: &mut ConverterOutput,
    page: ConverterOutput,
    asset_mode: AssetMode,
    published_asset_ids: &mut HashSet<AssetId>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    let mut page = super::working_visual::discard(page, context)?;
    if asset_mode == AssetMode::Omit {
        remove_images(&mut page.document.blocks, context)?;
        page.assets.clear();
    } else {
        page.assets.retain(|asset| published_asset_ids.insert(asset.id.clone()));
    }
    let mut page = page.reconcile_retained_output(context)?;
    let growth = page
        .document
        .blocks
        .len()
        .checked_mul(size_of::<BlockNode>())
        .and_then(|bytes| {
            bytes.checked_add(page.diagnostics.len().checked_mul(size_of::<Diagnostic>())?)
        })
        .and_then(|bytes| bytes.checked_add(page.assets.len().checked_mul(size_of::<Asset>())?))
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
    output.assets.try_reserve(page.assets.len()).map_err(|_| {
        super::resource("max_memory_bytes", "PDF asset collector allocation failed")
    })?;
    output.document.blocks.append(&mut page.document.blocks);
    output.diagnostics.append(&mut page.diagnostics);
    output.assets.append(&mut page.assets);
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
    fn every_pdf_ocr_mode_uses_page_consumption() {
        let mut options = ConversionOptions::default();
        options.output.asset_mode = AssetMode::Omit;
        assert!(page_ocr_requested(&options));
        options.ocr.policy = into_markdown_core::OcrPolicy::Off;
        assert!(!page_ocr_requested(&options));
        options.ai.vision_ocr = into_markdown_core::AiMode::Only;
        assert!(page_ocr_requested(&options));
        options.output.asset_mode = AssetMode::Extract;
        assert!(page_ocr_requested(&options));
        options.output.asset_mode = AssetMode::Embed;
        assert!(page_ocr_requested(&options));
    }
}
