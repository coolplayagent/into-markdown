//! Preserve one coordinate space for embedded OCR evidence and its locator.

use into_markdown_core::{
    Block, BlockNode, ConversionError, ExecutionContext, Inline, NodeId, Provenance, SourceLocator,
};

#[cfg(test)]
#[path = "geometry_tests.rs"]
mod tests;

pub(super) fn remap_ocr_node(
    mut node: BlockNode,
    fresh_id: NodeId,
    source: &Provenance,
    context: &ExecutionContext,
) -> Result<BlockNode, ConversionError> {
    node.id = fresh_id;
    let ocr_locator = node.provenance.locator.clone();
    node.provenance.locator = remapped_ocr_locator(&source.locator, &ocr_locator);
    if let Block::Paragraph(inlines) = &mut node.block {
        for (inline_index, inline) in inlines.iter_mut().enumerate() {
            if inline_index % 256 == 0 {
                context.checkpoint()?;
            }
            if let Inline::OcrText { provenance, evidence, .. } = inline {
                let inline_locator = provenance.locator.clone();
                provenance.locator = remapped_ocr_locator(&source.locator, &inline_locator);
                evidence.page = source.locator.page.or(source.locator.slide).unwrap_or(1);
                if let Some((source_bounds, image_width, image_height)) =
                    coordinate_frame(&source.locator, &inline_locator)
                {
                    for (region_index, region) in evidence.regions.iter_mut().enumerate() {
                        if region_index % 256 == 0 {
                            context.checkpoint()?;
                        }
                        for (point_index, point) in region.polygon.iter_mut().enumerate() {
                            if point_index % 256 == 0 {
                                context.checkpoint()?;
                            }
                            point.x = source_bounds.x + point.x * source_bounds.width / image_width;
                            point.y =
                                source_bounds.y + point.y * source_bounds.height / image_height;
                        }
                    }
                }
                provenance.locator.bounds = evidence_bounds(&evidence.regions);
            }
        }
    }
    Ok(node)
}

pub(super) fn evidence_bounds(
    regions: &[into_markdown_core::OcrSourceRegion],
) -> Option<into_markdown_core::Rect> {
    (!regions.is_empty()).then(|| {
        let minimum_x = regions
            .iter()
            .flat_map(|region| region.polygon.iter())
            .map(|point| point.x)
            .fold(f32::INFINITY, f32::min);
        let minimum_y = regions
            .iter()
            .flat_map(|region| region.polygon.iter())
            .map(|point| point.y)
            .fold(f32::INFINITY, f32::min);
        let maximum_x = regions
            .iter()
            .flat_map(|region| region.polygon.iter())
            .map(|point| point.x)
            .fold(f32::NEG_INFINITY, f32::max);
        let maximum_y = regions
            .iter()
            .flat_map(|region| region.polygon.iter())
            .map(|point| point.y)
            .fold(f32::NEG_INFINITY, f32::max);
        into_markdown_core::Rect {
            x: minimum_x,
            y: minimum_y,
            width: maximum_x - minimum_x,
            height: maximum_y - minimum_y,
        }
    })
}

fn coordinate_frame(
    source: &SourceLocator,
    ocr: &SourceLocator,
) -> Option<(into_markdown_core::Rect, f32, f32)> {
    let bounds = source.bounds?;
    let page_width = source.page_width?;
    let page_height = source.page_height?;
    let width = ocr.page_width?;
    let height = ocr.page_height?;
    // A container bounding box alone is not an image-to-page transform. In
    // particular, do not mix ODF points or DrawingML EMUs with image pixels,
    // or infer a rotated/cropped placement from its axis-aligned envelope.
    (bounds.x.is_finite()
        && bounds.y.is_finite()
        && bounds.width.is_finite()
        && bounds.height.is_finite()
        && bounds.width > 0.0
        && bounds.height > 0.0
        && bounds.x >= 0.0
        && bounds.y >= 0.0
        && page_width.is_finite()
        && page_height.is_finite()
        && page_width > 0.0
        && page_height > 0.0
        && f64::from(bounds.x) + f64::from(bounds.width) <= f64::from(page_width)
        && f64::from(bounds.y) + f64::from(bounds.height) <= f64::from(page_height)
        && source.rotation_degrees.is_none_or(|rotation| rotation == 0.0)
        && width.is_finite()
        && height.is_finite()
        && width > 0.0
        && height > 0.0)
        .then_some((bounds, width, height))
}

fn remapped_ocr_locator(source: &SourceLocator, ocr: &SourceLocator) -> SourceLocator {
    let mut locator = remapped_locator(source);
    locator.page = source.page.or(source.slide).or(Some(1));
    if let (Some((frame, width, height)), Some(bounds)) =
        (coordinate_frame(source, ocr), ocr.bounds)
    {
        locator.page_width = source.page_width;
        locator.page_height = source.page_height;
        locator.bounds = Some(into_markdown_core::Rect {
            x: frame.x + bounds.x * frame.width / width,
            y: frame.y + bounds.y * frame.height / height,
            width: bounds.width * frame.width / width,
            height: bounds.height * frame.height / height,
        });
    } else {
        locator.page_width = ocr.page_width;
        locator.page_height = ocr.page_height;
        locator.bounds = ocr.bounds;
        locator.rotation_degrees = ocr.rotation_degrees;
    }
    locator
}

pub(super) fn remapped_locator(source: &SourceLocator) -> SourceLocator {
    let mut locator = source.clone();
    locator.byte_start = None;
    locator.byte_end = None;
    locator.character_index = None;
    locator
}
