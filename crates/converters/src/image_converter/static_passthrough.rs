use super::{
    asset_lifecycle, decode, envelope, format::RasterFormat, image_block_dimensions, metadata,
    native_provenance_dimensions,
};
use into_markdown_core::{
    AiMode, Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput, Document,
    ExecutionContext, NodeId, OcrPolicy, ResolvedInput,
};

pub(super) fn try_convert(
    input: &ResolvedInput,
    format: RasterFormat,
    summary: envelope::Summary,
    density: metadata::Density,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Option<ConverterOutput>, ConversionError> {
    let ocr = options.ocr.policy != OcrPolicy::Off || options.ai.vision_ocr != AiMode::Off;
    if ocr
        || options.ai.image_description != AiMode::Off
        || summary.frames != 1
        || format == RasterFormat::Tiff
    {
        return Ok(None);
    }
    let header = decode::inspect_static(format, &input.bytes, &options.limits, context)?;
    // Rotation still needs pixel normalization to preserve published
    // dimensions. Alpha channels are decoded to preserve actual-alpha metadata.
    if header.orientation != 1 || header.has_alpha_channel {
        return Ok(None);
    }
    convert(input, format, summary, density, &header, options, context).map(Some)
}

fn convert(
    input: &ResolvedInput,
    format: RasterFormat,
    summary: envelope::Summary,
    density: metadata::Density,
    header: &decode::StaticHeader,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let inventory = asset_lifecycle::original(
        input,
        format,
        options.output.asset_mode != into_markdown_core::AssetMode::Omit,
        options,
        context,
    )?;
    let asset_id = inventory.original_id.clone();
    let mut document = document_metadata(format, summary, header, density);
    document.blocks.push(BlockNode {
        id: NodeId("image-page-1".into()),
        block: Block::Page {
            number: 1,
            blocks: vec![image_block_dimensions(asset_id, 1, header.width, header.height)],
        },
        provenance: native_provenance_dimensions(1, header.width, header.height),
    });
    document.validate().map_err(|error| ConversionError::Internal {
        detail: format!("image converter emitted invalid IR at {}: {}", error.path, error.detail),
    })?;
    ConverterOutput::new_with_memory_reservations(
        document,
        inventory.assets,
        Vec::new(),
        inventory.leases,
    )
    .account_retained(context)
}

fn document_metadata(
    format: RasterFormat,
    summary: envelope::Summary,
    header: &decode::StaticHeader,
    density: metadata::Density,
) -> Document {
    let mut document = Document::default();
    document.metadata.properties.insert("image.format".into(), format.extension().into());
    document.metadata.properties.insert("image.frames".into(), summary.frames.to_string());
    document.metadata.properties.insert("image.animated".into(), summary.animated.to_string());
    document
        .metadata
        .properties
        .insert("image.orientationApplied".into(), header.orientation.to_string());
    document.metadata.properties.insert("image.color".into(), header.color.clone());
    document.metadata.properties.insert("image.width".into(), header.width.to_string());
    document.metadata.properties.insert("image.height".into(), header.height.to_string());
    document.metadata.properties.insert("image.alpha".into(), "false".into());
    if let Some(x) = density.x_dpi {
        document.metadata.properties.insert("image.dpiX".into(), format!("{x:.4}"));
    }
    if let Some(y) = density.y_dpi {
        document.metadata.properties.insert("image.dpiY".into(), format!("{y:.4}"));
    }
    document
}
