//! Asset admission, Markdown rendering, and compatibility-result assembly.

use super::{
    collect_provenance_preflighted, invoke_renderer_preflighted, preserve_utf8_markdown,
    provenance_inventory_bytes,
};
use into_markdown_core::{
    AiMode, ConversionError, ConversionOptions, ConversionResult, ConverterOutput,
    ExecutionContext, InputFormat, MarkdownRenderer, ResolvedInput, TextDecodingMode,
    estimate_retained_result,
};

pub(crate) struct RenderRequest<'a> {
    pub(crate) renderer: &'a dyn MarkdownRenderer,
    pub(crate) output: ConverterOutput,
    pub(crate) source: &'a ResolvedInput,
    pub(crate) format: InputFormat,
    pub(crate) options: &'a ConversionOptions,
    pub(crate) context: &'a ExecutionContext,
}

pub(crate) struct RenderedArtifacts {
    pub(crate) output: ConverterOutput,
    pub(crate) markdown: String,
    pub(crate) provenance: Vec<into_markdown_core::Provenance>,
    pub(crate) markdown_memory: into_markdown_core::ResourceReservation,
    pub(crate) provenance_memory: into_markdown_core::ResourceReservation,
}

pub(crate) async fn render(
    request: RenderRequest<'_>,
) -> Result<ConversionResult, ConversionError> {
    let format = request.format;
    let context = request.context;
    let RenderedArtifacts { output, markdown, provenance, markdown_memory, provenance_memory } =
        render_artifacts(request).await?;
    let markdown_bytes =
        u64::try_from(markdown.capacity()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "rendered Markdown capacity cannot be represented as u64".into(),
        })?;
    let final_required = estimate_retained_result(
        &output.document,
        &markdown,
        &output.assets,
        &output.diagnostics,
        &provenance,
    )?;
    let final_memory = context.reserve_memory(
        final_required.saturating_sub(
            output
                .leased_memory_for(context)
                .saturating_add(markdown_bytes)
                .saturating_add(provenance_inventory_bytes(&provenance)?),
        ),
    )?;
    let mut result = output.into_conversion_result(
        markdown,
        provenance,
        [Some(markdown_memory), Some(provenance_memory), Some(final_memory)],
    )?;
    result.set_detected_format(format);
    result.content()?;
    Ok(result)
}

pub(crate) async fn render_artifacts(
    request: RenderRequest<'_>,
) -> Result<RenderedArtifacts, ConversionError> {
    let RenderRequest { renderer, output, source, format, options, context } = request;
    validate_asset_limits(&output, options)?;
    context.report(
        into_markdown_core::ExecutionStage::Rendering,
        None,
        None,
        Some(renderer.id()),
    )?;
    let (markdown, markdown_memory) = if format == InputFormat::Markdown
        && options.ai.markdown_postprocess == AiMode::Off
        && options.text.charset.is_none()
        && options.text.decoding_mode == TextDecodingMode::Strict
    {
        preserve_utf8_markdown(source, context)?
    } else {
        invoke_renderer_preflighted(renderer, &output.document, &output.assets, options, context)
            .await?
    };
    let (provenance, provenance_memory) =
        collect_provenance_preflighted(&output.document.blocks, context)?;
    Ok(RenderedArtifacts { output, markdown, provenance, markdown_memory, provenance_memory })
}

fn validate_asset_limits(
    output: &ConverterOutput,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let total = output.assets.iter().try_fold(0_u64, |total, asset| {
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset size cannot be represented as u64".into(),
            })?;
        if size > options.limits.max_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: format!(
                    "asset {}: {size} > {}",
                    asset.id.0, options.limits.max_asset_bytes
                ),
            });
        }
        total.checked_add(size).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_asset_bytes",
            detail: "asset byte count overflowed".into(),
        })
    })?;
    if total > options.limits.max_total_asset_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_total_asset_bytes",
            detail: format!("{total} > {}", options.limits.max_total_asset_bytes),
        });
    }
    Ok(())
}
