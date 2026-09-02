//! Asset admission, Markdown rendering, and compatibility-result assembly.

use super::{
    collect_provenance_preflighted, invoke_renderer_preflighted, preserve_utf8_markdown,
    provenance_inventory_bytes,
};
use into_markdown_core::{
    AiMode, AssetId, Block, BlockNode, ConversionError, ConversionOptions, ConversionResult,
    ConverterOutput, Diagnostic, DiagnosticSeverity, ErrorPolicy, ExecutionContext, Inline,
    InputFormat, MarkdownRenderer, ResolvedInput, SourceLocator, TextDecodingMode,
    estimate_retained_result,
};
use std::collections::BTreeMap;

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
    pub(crate) asset_spool: Option<crate::artifact_output::AssetSpool>,
}

pub(crate) async fn render(
    request: RenderRequest<'_>,
) -> Result<ConversionResult, ConversionError> {
    let format = request.format;
    let context = request.context;
    let RenderedArtifacts {
        output,
        markdown,
        provenance,
        markdown_memory,
        provenance_memory,
        asset_spool: _,
    } = render_artifacts(request).await?;
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
    let RenderRequest { renderer, mut output, source, format, options, context } = request;
    let mut effective_options = options.clone();
    validate_asset_limits(&mut output, &mut effective_options, context)?;
    context.report(
        into_markdown_core::ExecutionStage::Rendering,
        None,
        None,
        Some(renderer.id()),
    )?;
    let (markdown, markdown_memory) = if format == InputFormat::Markdown
        && effective_options.ai.markdown_postprocess == AiMode::Off
        && effective_options.text.charset.is_none()
        && effective_options.text.decoding_mode == TextDecodingMode::Strict
    {
        preserve_utf8_markdown(source, context)?
    } else {
        invoke_renderer_preflighted(
            renderer,
            &output.document,
            &output.assets,
            &effective_options,
            context,
        )
        .await?
    };
    let (provenance, provenance_memory) =
        collect_provenance_preflighted(&output.document.blocks, context)?;
    Ok(RenderedArtifacts {
        output,
        markdown,
        provenance,
        markdown_memory,
        provenance_memory,
        asset_spool: None,
    })
}

fn validate_asset_limits(
    output: &mut ConverterOutput,
    options: &mut ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut largest = 0_u64;
    let total = output.assets.iter().try_fold(0_u64, |total, asset| {
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset size cannot be represented as u64".into(),
            })?;
        largest = largest.max(size);
        total.checked_add(size).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_total_asset_bytes",
            detail: "asset byte count overflowed".into(),
        })
    })?;
    adapt_asset_limit(output, options, context, "max_asset_bytes", largest)?;
    adapt_asset_limit(output, options, context, "max_total_asset_bytes", total)?;
    let mut exceeds_single = false;
    for asset in &output.assets {
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset size cannot be represented as u64".into(),
            })?;
        if size > options.limits.max_asset_bytes {
            exceeds_single = true;
            if options.error_policy == ErrorPolicy::Strict {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_asset_bytes",
                    detail: format!(
                        "asset {}: {size} > {}",
                        asset.id.0, options.limits.max_asset_bytes
                    ),
                });
            }
        }
    }
    if total > options.limits.max_total_asset_bytes {
        if options.error_policy == ErrorPolicy::Strict {
            return Err(ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: format!("{total} > {}", options.limits.max_total_asset_bytes),
            });
        }
    }
    if exceeds_single || total > options.limits.max_total_asset_bytes {
        degrade_asset_payloads(output, options, total, context)?;
    }
    Ok(())
}

fn degrade_asset_payloads(
    output: &mut ConverterOutput,
    options: &ConversionOptions,
    observed_total: u64,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut retained_total = 0_u64;
    let mut placeholders = BTreeMap::<AssetId, String>::new();
    let mut total_omitted = 0_u64;
    let mut first_total_locator = None;
    for asset in &mut output.assets {
        context.checkpoint()?;
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset size cannot be represented as u64".into(),
            })?;
        if size == 0 {
            continue;
        }
        let single = size > options.limits.max_asset_bytes;
        let cumulative =
            retained_total.checked_add(size).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: "asset byte count overflowed".into(),
            })?;
        let total = !single && cumulative > options.limits.max_total_asset_bytes;
        if !single && !total {
            retained_total = cumulative;
            continue;
        }
        let locator = locate_asset(&output.document.blocks, &asset.id).or_else(|| {
            Some(SourceLocator {
                part: Some(asset.filename.clone().unwrap_or_else(|| asset.id.0.clone())),
                ..SourceLocator::default()
            })
        });
        if single {
            output.diagnostics.push(Diagnostic {
                code: "resource.max_asset_bytes.unitOmitted".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "resource limit max_asset_bytes: configured={}, observed={size}, action=omitted 1 asset payload; reference or placeholder retained",
                    options.limits.max_asset_bytes
                ),
                locator: locator.clone(),
            });
        } else {
            total_omitted = total_omitted.saturating_add(1);
            first_total_locator.get_or_insert(locator.clone());
        }
        asset.bytes = Vec::new();
        if asset.external_uri.is_none() {
            placeholders
                .insert(asset.id.clone(), "asset payload omitted by resource policy".into());
        }
    }
    if total_omitted > 0 {
        output.diagnostics.push(Diagnostic {
            code: "resource.max_total_asset_bytes.sequenceTruncated".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "resource limit max_total_asset_bytes: configured={}, observed={observed_total}, action=kept payloads totaling {retained_total} bytes and omitted {total_omitted} subsequent asset payloads",
                options.limits.max_total_asset_bytes
            ),
            locator: first_total_locator.flatten(),
        });
    }
    replace_omitted_images(&mut output.document.blocks, &placeholders);
    output.assets.retain(|asset| !placeholders.contains_key(&asset.id));
    let reconciled = std::mem::take(output).reconcile_retained_output(context)?;
    *output = reconciled;
    Ok(())
}

fn replace_omitted_images(nodes: &mut [BlockNode], omitted: &BTreeMap<AssetId, String>) {
    for node in nodes {
        match &mut node.block {
            Block::Image { asset, alt } if omitted.contains_key(asset) => {
                let label = alt
                    .take()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "Image".into());
                let reason = omitted.get(asset).map_or("asset omitted", String::as_str);
                node.block = Block::Paragraph(vec![Inline::Text {
                    value: format!("[{label}: {reason}]"),
                    marks: Vec::new(),
                }]);
            }
            Block::List { items, .. } => {
                for item in items {
                    replace_omitted_images(&mut item.blocks, omitted);
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &mut row.cells {
                        replace_omitted_images(&mut cell.blocks, omitted);
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => replace_omitted_images(blocks, omitted),
            _ => {}
        }
    }
}

fn locate_asset(nodes: &[BlockNode], id: &AssetId) -> Option<SourceLocator> {
    for node in nodes {
        match &node.block {
            Block::Image { asset, .. } if asset == id => {
                return Some(node.provenance.locator.clone());
            }
            Block::List { items, .. } => {
                for item in items {
                    if let Some(locator) = locate_asset(&item.blocks, id) {
                        return Some(locator);
                    }
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        if let Some(locator) = locate_asset(&cell.blocks, id) {
                            return Some(locator);
                        }
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                if let Some(locator) = locate_asset(blocks, id) {
                    return Some(locator);
                }
            }
            _ => {}
        }
    }
    None
}

fn adapt_asset_limit(
    output: &mut ConverterOutput,
    options: &mut ConversionOptions,
    context: &ExecutionContext,
    limit: &'static str,
    required: u64,
) -> Result<(), ConversionError> {
    let configured = match limit {
        "max_asset_bytes" => options.limits.max_asset_bytes,
        "max_total_asset_bytes" => options.limits.max_total_asset_bytes,
        _ => return Ok(()),
    };
    let effective = context.effective_soft_limit(limit, configured);
    let raised = if required > effective {
        context.try_raise_soft_limit(limit, effective, required, 0)?
    } else {
        None
    };
    let final_limit = raised.unwrap_or(effective);
    match limit {
        "max_asset_bytes" => options.limits.max_asset_bytes = final_limit,
        "max_total_asset_bytes" => options.limits.max_total_asset_bytes = final_limit,
        _ => {}
    }
    if let Some(new_limit) = raised {
        output.diagnostics.push(into_markdown_core::Diagnostic {
            code: format!("resource.{limit}.limitRaised"),
            severity: into_markdown_core::DiagnosticSeverity::Info,
            message: format!(
                "resource limit {limit}: configured={configured}, observed={required}, action=raised to {new_limit}"
            ),
            locator: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        AdaptiveResourceLimits, Asset, AssetId, Block, BlockNode, Document, ExecutionOptions,
        NodeId, Provenance, ProvenanceKind, ResourceLimits,
    };

    fn output(sizes: &[usize]) -> ConverterOutput {
        let blocks = sizes
            .iter()
            .enumerate()
            .map(|(index, _)| BlockNode {
                id: NodeId(format!("image-{index}")),
                block: Block::Image {
                    asset: AssetId(format!("asset-{index}")),
                    alt: Some(format!("image {index}")),
                },
                provenance: Provenance {
                    kind: ProvenanceKind::NativeParser,
                    provider: "fixture".into(),
                    locator: SourceLocator {
                        page: u32::try_from(index + 1).ok(),
                        ..Default::default()
                    },
                    confidence: Some(1.0),
                },
            })
            .collect();
        ConverterOutput::new(
            Document { blocks, ..Document::default() },
            sizes
                .iter()
                .enumerate()
                .map(|(index, size)| Asset {
                    id: AssetId(format!("asset-{index}")),
                    filename: None,
                    media_type: "application/octet-stream".into(),
                    bytes: vec![0; *size],
                    external_uri: None,
                })
                .collect(),
            vec![],
        )
    }

    #[test]
    fn local_implicit_asset_limits_raise_once_after_exact_preflight() {
        let limits = ResourceLimits {
            max_memory_bytes: 1024 * 1024,
            max_asset_bytes: 4,
            max_total_asset_bytes: 8,
            ..ResourceLimits::default()
        };
        let mut ceilings = limits.clone();
        ceilings.max_asset_bytes = 32;
        ceilings.max_total_asset_bytes = 48;
        let context = ExecutionContext::new(
            ExecutionOptions {
                resource_adaptation: AdaptiveResourceLimits::local(
                    ["max_asset_bytes", "max_total_asset_bytes"],
                    ceilings,
                ),
                ..ExecutionOptions::default()
            },
            limits.clone(),
        );
        let mut options = ConversionOptions { limits, ..ConversionOptions::default() };
        let mut first = output(&[6, 5]);
        validate_asset_limits(&mut first, &mut options, &context).unwrap();
        assert_eq!(options.limits.max_asset_bytes, 6);
        assert_eq!(options.limits.max_total_asset_bytes, 11);
        assert_eq!(first.diagnostics.len(), 2);

        let mut later = output(&[7]);
        validate_asset_limits(&mut later, &mut options, &context).unwrap();
        assert!(later.assets.is_empty());
        assert!(
            later
                .diagnostics
                .iter()
                .any(|item| item.code == "resource.max_asset_bytes.unitOmitted")
        );
        assert!(!later.diagnostics.iter().any(|item| item.code.ends_with(".limitRaised")));
    }

    #[test]
    fn fixed_api_and_explicit_limits_do_not_raise() {
        let limits = ResourceLimits {
            max_memory_bytes: 1024 * 1024,
            max_asset_bytes: 4,
            max_total_asset_bytes: 8,
            ..ResourceLimits::default()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        let mut options = ConversionOptions {
            error_policy: ErrorPolicy::Strict,
            limits,
            ..ConversionOptions::default()
        };
        let mut output = output(&[5]);
        let error = validate_asset_limits(&mut output, &mut options, &context).unwrap_err();
        assert_eq!(error.limit().unwrap().0, "max_asset_bytes");
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn best_effort_omits_oversized_and_subsequent_asset_payloads_with_placeholders() {
        let limits = ResourceLimits {
            max_memory_bytes: 1024 * 1024,
            max_asset_bytes: 5,
            max_total_asset_bytes: 7,
            ..ResourceLimits::default()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        let mut options = ConversionOptions { limits, ..ConversionOptions::default() };
        let mut output = output(&[6, 4, 4]);
        validate_asset_limits(&mut output, &mut options, &context).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].id.0, "asset-1");
        assert_eq!(output.diagnostics[0].code, "resource.max_asset_bytes.unitOmitted");
        assert_eq!(output.diagnostics[0].locator.as_ref().and_then(|value| value.page), Some(1));
        assert_eq!(output.diagnostics[1].code, "resource.max_total_asset_bytes.sequenceTruncated");
        assert_eq!(output.diagnostics[1].locator.as_ref().and_then(|value| value.page), Some(3));
        assert!(matches!(output.document.blocks[0].block, Block::Paragraph(_)));
        assert!(matches!(output.document.blocks[1].block, Block::Image { .. }));
        assert!(matches!(output.document.blocks[2].block, Block::Paragraph(_)));
        assert_eq!(context.reserved_memory_bytes(), output.leased_memory_for(&context));
    }
}
