//! Safe raster-image conversion with bounded local OCR and optional image description.

mod ai;
pub(crate) mod decode;
pub(crate) mod encode;
pub(crate) mod envelope;
pub(crate) mod format;
mod metadata;
pub(crate) mod ocr;

use decode::DecodedFrame;
use format::RasterFormat;
use into_markdown_core::{
    AiMode, Asset, AssetId, Block, BlockNode, BoxFuture, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ExecutionContext,
    FormatCandidate, InputFormat, NodeId, ProbeOutcome, Provenance, ProvenanceKind, ResolvedInput,
    Services, SourceLocator,
};

const PROVIDER_ID: &str = "builtin.converter.image";
const FORMATS: &[InputFormat] = &[InputFormat::Image];

/// Strict PNG, JPEG, TIFF, WebP, and BMP converter.
#[derive(Debug, Default)]
pub struct ImageConverter;

impl Converter for ImageConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        260
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
            if candidate.format != InputFormat::Image {
                return Ok(ProbeOutcome::NotApplicable);
            }
            Ok(if format::detect(&input.bytes, context)?.is_some() {
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
        Ok(context.available_memory_bytes())
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { convert_image(input, options, services, context).await })
    }
}

#[allow(clippy::too_many_lines)]
async fn convert_image(
    input: &ResolvedInput,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    if input.bytes.len() as u64 > options.limits.max_input_bytes {
        return Err(resource(
            "max_input_bytes",
            format!("image source {} exceeds max_input_bytes", input.bytes.len()),
        ));
    }
    let format =
        format::detect(&input.bytes, context)?.ok_or_else(|| ConversionError::Unsupported {
            detail: "input is not an audited PNG, JPEG, TIFF, WebP, or BMP envelope".into(),
        })?;
    let summary = envelope::validate(format, &input.bytes, &options.limits, context)?;
    let density = metadata::density(format, &input.bytes, context)?;
    let decoded = decode::decode(format, &input.bytes, summary, &options.limits, context)?;

    let original_bytes = input.bytes.len() as u64;
    if original_bytes > options.limits.max_asset_bytes {
        return Err(resource(
            "max_asset_bytes",
            format!("original image {original_bytes} exceeds max_asset_bytes"),
        ));
    }
    if original_bytes > options.limits.max_total_asset_bytes {
        return Err(resource(
            "max_total_asset_bytes",
            format!("original image {original_bytes} exceeds max_total_asset_bytes"),
        ));
    }
    let mut total_asset_bytes = original_bytes;
    let original_memory = context.reserve_memory(original_bytes)?;
    let original_id = AssetId("image-original".into());
    let mut assets = vec![Asset {
        id: original_id.clone(),
        filename: Some(format!("source.{}", format.extension())),
        media_type: format.media_type().into(),
        bytes: input.bytes.to_vec(),
        external_uri: None,
    }];
    let mut leases = vec![original_memory];
    let mut diagnostics = Vec::new();
    let mut document = document_metadata(format, summary, &decoded, density);

    for (index, frame) in decoded.frames.iter().enumerate() {
        context.checkpoint()?;
        let page = u32::try_from(index + 1).map_err(|_| resource("max_pages", "page overflow"))?;
        let needs_normalized_asset = summary.frames > 1 || decoded.orientation != 1;
        let ocr_enabled = options.ocr.policy != into_markdown_core::OcrPolicy::Off
            || options.ai.vision_ocr != AiMode::Off;
        let ai_enabled = options.ai.image_description != AiMode::Off;
        let needs_normalized =
            needs_normalized_asset || ai_enabled || (ocr_enabled && !frame.has_alpha);
        let normalized = needs_normalized
            .then(|| encode::png(&frame.pixels, false, &options.limits, context))
            .transpose()?;
        let ocr_input = if ocr_enabled && frame.has_alpha {
            Some(encode::png(&frame.pixels, true, &options.limits, context)?)
        } else {
            None
        };
        let inference_bytes = ocr_input.as_ref().map_or_else(
            || normalized.as_ref().map_or(&[][..], |value| value.bytes.as_slice()),
            |value| value.bytes.as_slice(),
        );

        let mut ai_nodes = Vec::new();
        if matches!(options.ai.image_description, AiMode::Prefer | AiMode::Only) {
            let ai_image = normalized.as_ref().ok_or_else(|| ConversionError::Internal {
                detail: "AI image-description input was not materialized".into(),
            })?;
            match ai::describe(&ai_image.bytes, page, options, services, context).await {
                Ok(contribution) => {
                    ai_nodes = contribution.nodes;
                    diagnostics.extend(contribution.diagnostics);
                    leases.push(contribution.memory);
                }
                Err(error) if options.ai.image_description == AiMode::Only => return Err(error),
                Err(error) => fallback_diagnostic(&mut diagnostics, page, error)?,
            }
        }

        let mut ocr = ocr::recognize(
            inference_bytes,
            page,
            frame.pixels.width(),
            frame.pixels.height(),
            options,
            services,
            context,
        )
        .await?;
        if let Some(memory) = ocr.memory.take() {
            leases.push(memory);
        }
        if options.ai.image_description == AiMode::Fallback && !ocr.accepted_text {
            let ai_image = normalized.as_ref().ok_or_else(|| ConversionError::Internal {
                detail: "AI fallback image input was not materialized".into(),
            })?;
            match ai::describe(&ai_image.bytes, page, options, services, context).await {
                Ok(contribution) => {
                    ai_nodes = contribution.nodes;
                    diagnostics.extend(contribution.diagnostics);
                    leases.push(contribution.memory);
                }
                Err(error) => fallback_diagnostic(&mut diagnostics, page, error)?,
            }
        }

        let image_id = if needs_normalized_asset {
            let normalized = normalized.ok_or_else(|| ConversionError::Internal {
                detail: "normalized image asset was not materialized".into(),
            })?;
            let (bytes, memory) = normalized.into_parts();
            total_asset_bytes = total_asset_bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| resource("max_total_asset_bytes", "asset byte total overflow"))?;
            if total_asset_bytes > options.limits.max_total_asset_bytes {
                return Err(resource(
                    "max_total_asset_bytes",
                    format!("image assets {total_asset_bytes} exceed max_total_asset_bytes"),
                ));
            }
            let id = AssetId(format!("image-page-{page}"));
            assets.push(Asset {
                id: id.clone(),
                filename: Some(format!("page-{page}.png")),
                media_type: "image/png".into(),
                bytes,
                external_uri: None,
            });
            leases.push(memory);
            id
        } else {
            original_id.clone()
        };
        let mut blocks = vec![image_block(image_id, page, frame)];
        blocks.append(&mut ai_nodes);
        blocks.append(&mut ocr.nodes);
        diagnostics.append(&mut ocr.diagnostics);
        document.blocks.push(BlockNode {
            id: NodeId(format!("image-page-{page}")),
            block: Block::Page { number: page, blocks },
            provenance: native_provenance(page, frame),
        });
    }
    drop(decoded);
    document.validate().map_err(|error| ConversionError::Internal {
        detail: format!("image converter emitted invalid IR at {}: {}", error.path, error.detail),
    })?;
    let output =
        ConverterOutput::new_with_memory_reservations(document, assets, diagnostics, leases);
    output.account_retained(context)
}

fn image_block(asset: AssetId, page: u32, frame: &DecodedFrame) -> BlockNode {
    BlockNode {
        id: NodeId(format!("image-page-{page}-visual")),
        block: Block::Image { asset, alt: None },
        provenance: native_provenance(page, frame),
    }
}

fn native_provenance(page: u32, frame: &DecodedFrame) -> Provenance {
    Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: PROVIDER_ID.into(),
        locator: SourceLocator {
            page: Some(page),
            page_width: Some(f32::from(u16::try_from(frame.pixels.width()).unwrap_or(u16::MAX))),
            page_height: Some(f32::from(u16::try_from(frame.pixels.height()).unwrap_or(u16::MAX))),
            ..SourceLocator::default()
        },
        confidence: Some(1.0),
    }
}

fn document_metadata(
    format: RasterFormat,
    summary: envelope::Summary,
    decoded: &decode::DecodedSet,
    density: metadata::Density,
) -> Document {
    let mut document = Document::default();
    document.metadata.properties.insert("image.format".into(), format.extension().into());
    document.metadata.properties.insert("image.frames".into(), summary.frames.to_string());
    document.metadata.properties.insert("image.animated".into(), summary.animated.to_string());
    document
        .metadata
        .properties
        .insert("image.orientationApplied".into(), decoded.orientation.to_string());
    document.metadata.properties.insert("image.color".into(), decoded.color.clone());
    if let Some(frame) = decoded.frames.first() {
        document.metadata.properties.insert("image.width".into(), frame.pixels.width().to_string());
        document
            .metadata
            .properties
            .insert("image.height".into(), frame.pixels.height().to_string());
        document.metadata.properties.insert("image.alpha".into(), frame.has_alpha.to_string());
    }
    if let Some(x) = density.x_dpi {
        document.metadata.properties.insert("image.dpiX".into(), format!("{x:.4}"));
    }
    if let Some(y) = density.y_dpi {
        document.metadata.properties.insert("image.dpiY".into(), format!("{y:.4}"));
    }
    document
}

fn fallback_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    page: u32,
    error: ConversionError,
) -> Result<(), ConversionError> {
    if matches!(error, ConversionError::Cancelled | ConversionError::Timeout) {
        return Err(error);
    }
    diagnostics.push(Diagnostic {
        code: "image.aiDescriptionFallback".into(),
        severity: DiagnosticSeverity::Warning,
        message: format!(
            "AI image description was unavailable; local image output was retained ({error})"
        ),
        locator: Some(SourceLocator { page: Some(page), ..SourceLocator::default() }),
    });
    Ok(())
}

fn resource(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}

#[cfg(test)]
mod tests;
