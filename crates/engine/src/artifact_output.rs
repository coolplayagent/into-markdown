//! Ordered emission of rendered converter artifacts to a caller-owned sink.

use crate::rendering::RenderedArtifacts;
use into_markdown_core::{
    ArtifactSink, ArtifactSinkCapabilities, AssetStreamInfo, ConversionError, ConversionSummary,
    DocumentStreamEvent, ExecutionContext, InputFormat,
};
use sha2::{Digest as _, Sha256};

const EMIT_CHUNK_BYTES: usize = 64 * 1024;

pub(crate) fn emit(
    rendered: RenderedArtifacts,
    format: InputFormat,
    capabilities: ArtifactSinkCapabilities,
    sink: &mut dyn ArtifactSink,
    context: &ExecutionContext,
) -> Result<ConversionSummary, ConversionError> {
    let RenderedArtifacts { output, markdown, provenance, markdown_memory, provenance_memory } =
        rendered;
    let result_content = into_markdown_core::classify_result(
        &output.document,
        &markdown,
        &output.assets,
        &output.diagnostics,
    )?;
    if capabilities.semantic_events {
        emit_document(&output, &provenance, sink, context)?;
    }
    if capabilities.markdown {
        for chunk in markdown.as_bytes().chunks(EMIT_CHUNK_BYTES) {
            context.checkpoint()?;
            sink.write_markdown(chunk)?;
        }
    }
    if capabilities.assets {
        emit_assets(&output.assets, sink, context)?;
    }
    let markdown_bytes =
        u64::try_from(markdown.len()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_output_bytes",
            detail: "rendered Markdown byte count cannot be represented".into(),
        })?;
    drop(provenance_memory);
    drop(markdown_memory);
    drop(markdown);
    Ok(output.into_conversion_summary(format, markdown_bytes, result_content))
}

fn emit_document(
    output: &into_markdown_core::ConverterOutput,
    provenance: &[into_markdown_core::Provenance],
    sink: &mut dyn ArtifactSink,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    sink.write_document_event(&DocumentStreamEvent::Metadata(&output.document.metadata))?;
    for block in &output.document.blocks {
        context.checkpoint()?;
        sink.write_document_event(&DocumentStreamEvent::RootBlock(block))?;
    }
    context.checkpoint()?;
    sink.finish_document(&output.diagnostics, provenance)
}

fn emit_assets(
    assets: &[into_markdown_core::Asset],
    sink: &mut dyn ArtifactSink,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for asset in assets {
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset byte length cannot be represented".into(),
            })?;
        context.checkpoint()?;
        sink.begin_asset(&AssetStreamInfo {
            id: asset.id.clone(),
            filename: asset.filename.clone(),
            media_type: asset.media_type.clone(),
            size,
            external_uri: asset.external_uri.clone(),
            content_sha256: (!asset.bytes.is_empty()).then(|| Sha256::digest(&asset.bytes).into()),
        })?;
        for chunk in asset.bytes.chunks(EMIT_CHUNK_BYTES) {
            context.checkpoint()?;
            sink.write_asset(chunk)?;
        }
        context.checkpoint()?;
        sink.end_asset()?;
    }
    Ok(())
}
