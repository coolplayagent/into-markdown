//! Bounded PP-OCRv6 text-detection preprocessing and DB postprocessing.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use clipper2_rust::{EndType, JoinType, PointD, inflate_paths_d};
use image::GrayImage;
use imageproc::contours::find_contours;
use imageproc::geometry::min_area_rect;
use imageproc::point::Point;
use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, ResourceReservation, Tensor, TensorRuntime,
};
use serde::Deserialize;
use std::sync::Arc;

const PROVIDER: &str = "builtin.ocr.ppocrv6-detector";
const MODEL_ID: &str = "pp-ocrv6-tiny-zh-en";
const STRIDE: usize = 32;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];

/// Byte layout supplied by a future audited image decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Gray8,
    Rgb8,
    Bgr8,
    Rgba8,
    Bgra8,
}

impl PixelFormat {
    const fn channels(self) -> usize {
        match self {
            Self::Gray8 => 1,
            Self::Rgb8 | Self::Bgr8 => 3,
            Self::Rgba8 | Self::Bgra8 => 4,
        }
    }
}

/// EXIF-compatible source orientation. Mirroring is applied before rotation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageOrientation {
    #[default]
    Normal,
    MirrorHorizontal,
    Rotate180,
    MirrorVertical,
    MirrorHorizontalRotate270,
    Rotate90,
    MirrorHorizontalRotate90,
    Rotate270,
}

/// Borrowed decoded pixels; this type deliberately does not decode image bytes.
#[derive(Debug, Clone, Copy)]
pub struct PixelView<'a> {
    pub width: usize,
    pub height: usize,
    pub row_stride: usize,
    pub format: PixelFormat,
    pub orientation: ImageOrientation,
    pub bytes: &'a [u8],
}

/// Geometry needed by recognition to construct a perspective crop later.
#[derive(Debug, Clone, PartialEq)]
pub struct CropDescriptor {
    pub polygon: [(f32, f32); 4],
    pub width: u32,
    pub height: u32,
}

/// One detector-only result in original source coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedTextRegion {
    pub polygon: [(f32, f32); 4],
    pub angle_degrees: f32,
    pub confidence: f32,
    pub crop: CropDescriptor,
}

/// Stable detector output; recognition text is intentionally absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DetectionResult {
    pub regions: Vec<DetectedTextRegion>,
    pub provider: String,
}

/// Audited PP-OCRv6 tiny detection parameters and local safety bounds.
#[derive(Debug, Clone)]
pub struct DetectionConfig {
    pub min_side_len: usize,
    pub max_side_len: usize,
    pub bitmap_threshold: f32,
    pub box_threshold: f32,
    pub unclip_ratio: f32,
    pub max_candidates: usize,
    pub max_source_pixels: usize,
    pub max_model_pixels: usize,
    pub max_contour_points: usize,
    pub max_offset_points: usize,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            min_side_len: 736,
            max_side_len: 4000,
            bitmap_threshold: 0.2,
            box_threshold: 0.4,
            unclip_ratio: 1.4,
            max_candidates: 3000,
            max_source_pixels: 100_000_000,
            max_model_pixels: 16_000_000,
            max_contour_points: 16_000_000,
            max_offset_points: 4096,
        }
    }
}

/// Offline detector over the runtime seam introduced by the ONNX integration.
pub struct PpOcrTextDetector {
    runtime: Arc<dyn TensorRuntime>,
    config: DetectionConfig,
}

impl PpOcrTextDetector {
    pub fn new(
        runtime: Arc<dyn TensorRuntime>,
        config: DetectionConfig,
    ) -> Result<Self, ConversionError> {
        validate_authority()?;
        validate_config(&config)?;
        Ok(Self { runtime, config })
    }

    #[must_use]
    pub fn detect<'a>(
        &'a self,
        image: PixelView<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<DetectionResult, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let Prepared { tensor, transform, _reservation } =
                preprocess(image, &self.config, context)?;
            let outputs = self.runtime.run(MODEL_ID, &[tensor], context).await?;
            context.checkpoint()?;
            postprocess(&outputs, image, transform, &self.config, context)
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DetectorAuthority {
    schema_version: u32,
    model_id: String,
    upstream_repository: String,
    upstream_commit: String,
    upstream_config: String,
    paper: String,
    input_color: String,
    resize_stride: usize,
    resize_rounding: String,
    resize_interpolation: String,
    minimum_side: usize,
    maximum_side: usize,
    scale: f64,
    mean: [f32; 3],
    standard_deviation: [f32; 3],
    bitmap_threshold: f32,
    box_threshold: f32,
    maximum_candidates: usize,
    unclip_ratio: f32,
    contour_retrieval: String,
    contour_approximation: String,
    offset_join: String,
    offset_end: String,
}

fn validate_authority() -> Result<(), ConversionError> {
    let value: DetectorAuthority =
        serde_json::from_str(include_str!("../../../models/ppocrv6-tiny-detector-authority.json"))
            .map_err(|_| ocr("invalidDetectorAuthority"))?;
    if value.schema_version != 1
        || value.model_id != MODEL_ID
        || value.upstream_repository != "https://github.com/PaddlePaddle/PaddleOCR"
        || value.upstream_commit != "2661c7c0ef5c613e8f93c6e93b2e052399f0f854"
        || value.upstream_config != "configs/det/PP-OCRv6/PP-OCRv6_tiny_det.yml"
        || value.paper != "https://arxiv.org/abs/1911.08947"
        || value.input_color != "BGR"
        || value.resize_stride != STRIDE
        || value.resize_rounding != "ties-to-even"
        || value.resize_interpolation != "bilinear"
        || value.minimum_side != 736
        || value.maximum_side != 4000
        || (value.scale - 1.0 / 255.0).abs() > f64::EPSILON
        || value.mean.map(f32::to_bits) != MEAN.map(f32::to_bits)
        || value.standard_deviation.map(f32::to_bits) != STD.map(f32::to_bits)
        || value.bitmap_threshold.to_bits() != 0.2_f32.to_bits()
        || value.box_threshold.to_bits() != 0.4_f32.to_bits()
        || value.maximum_candidates != 3000
        || value.unclip_ratio.to_bits() != 1.4_f32.to_bits()
        || value.contour_retrieval != "list"
        || value.contour_approximation != "simple"
        || value.offset_join != "round"
        || value.offset_end != "closed-polygon"
    {
        return Err(ocr("detectorAuthorityDrift"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Transform {
    oriented_w: usize,
    oriented_h: usize,
    model_w: usize,
    model_h: usize,
}
struct Prepared {
    tensor: Tensor,
    transform: Transform,
    _reservation: ResourceReservation,
}

fn validate_config(c: &DetectionConfig) -> Result<(), ConversionError> {
    if c.min_side_len == 0
        || c.min_side_len > c.max_side_len
        || c.max_side_len < STRIDE
        || c.max_side_len > 4000
        || c.max_candidates == 0
        || c.max_source_pixels == 0
        || c.max_model_pixels == 0
        || c.max_contour_points == 0
        || c.max_offset_points < 3
        || !c.bitmap_threshold.is_finite()
        || !(0.0..=1.0).contains(&c.bitmap_threshold)
        || !c.box_threshold.is_finite()
        || !(0.0..=1.0).contains(&c.box_threshold)
        || !c.unclip_ratio.is_finite()
        || c.unclip_ratio <= 0.0
        || c.unclip_ratio > 4.0
    {
        return Err(ocr("invalidDetectionConfig"));
    }
    Ok(())
}

fn validate_pixels(image: PixelView<'_>, c: &DetectionConfig) -> Result<(), ConversionError> {
    let pixels = image.width.checked_mul(image.height).ok_or_else(|| limit("sourcePixels"))?;
    if image.width == 0 || image.height == 0 || pixels > c.max_source_pixels {
        return Err(limit("sourcePixels"));
    }
    let row =
        image.width.checked_mul(image.format.channels()).ok_or_else(|| limit("pixelStride"))?;
    if image.row_stride < row {
        return Err(ocr("invalidPixelStride"));
    }
    let needed = image
        .height
        .checked_sub(1)
        .and_then(|h| h.checked_mul(image.row_stride))
        .and_then(|n| n.checked_add(row))
        .ok_or_else(|| limit("pixelStride"))?;
    if needed > image.bytes.len() {
        return Err(ocr("truncatedPixelBuffer"));
    }
    Ok(())
}

fn oriented_size(image: PixelView<'_>) -> (usize, usize) {
    match image.orientation {
        ImageOrientation::Rotate90
        | ImageOrientation::Rotate270
        | ImageOrientation::MirrorHorizontalRotate90
        | ImageOrientation::MirrorHorizontalRotate270 => (image.height, image.width),
        _ => (image.width, image.height),
    }
}

fn round_stride(value: f64) -> Result<usize, ConversionError> {
    if !value.is_finite() || value < 1.0 {
        return Err(limit("modelPixels"));
    }
    let stride_units = (value / STRIDE as f64).round_ties_even().max(1.0);
    if stride_units > usize::MAX as f64 {
        return Err(limit("modelPixels"));
    }
    (stride_units as usize).checked_mul(STRIDE).ok_or_else(|| limit("modelPixels"))
}

fn preprocess(
    image: PixelView<'_>,
    c: &DetectionConfig,
    context: &ExecutionContext,
) -> Result<Prepared, ConversionError> {
    validate_pixels(image, c)?;
    let (ow, oh) = oriented_size(image);
    let short = ow.min(oh) as f64;
    let long = ow.max(oh) as f64;
    let mut ratio = if short < c.min_side_len as f64 { c.min_side_len as f64 / short } else { 1.0 };
    if long * ratio > c.max_side_len as f64 {
        ratio = c.max_side_len as f64 / long;
    }
    let mw = round_stride(ow as f64 * ratio)?;
    let mh = round_stride(oh as f64 * ratio)?;
    let count = mw.checked_mul(mh).ok_or_else(|| limit("modelPixels"))?;
    if count > c.max_model_pixels {
        return Err(limit("modelPixels"));
    }
    let values = count.checked_mul(3).ok_or_else(|| limit("tensorMemory"))?;
    let bytes =
        values.checked_mul(std::mem::size_of::<f32>()).ok_or_else(|| limit("tensorMemory"))?;
    let reservation =
        context.reserve_memory(u64::try_from(bytes).map_err(|_| limit("tensorMemory"))?)?;
    let mut output = Vec::new();
    output.try_reserve_exact(values).map_err(|_| limit("tensorMemory"))?;
    output.resize(values, 0.0);
    for y in 0..mh {
        if y % 32 == 0 {
            context.checkpoint()?;
        }
        let sy = ((y as f64 + 0.5) * oh as f64 / mh as f64 - 0.5).clamp(0.0, (oh - 1) as f64);
        for x in 0..mw {
            let sx = ((x as f64 + 0.5) * ow as f64 / mw as f64 - 0.5).clamp(0.0, (ow - 1) as f64);
            let bgr = bilinear(image, sx, sy)?;
            for channel in 0..3 {
                output[channel * count + y * mw + x] =
                    (bgr[channel] / 255.0 - MEAN[channel]) / STD[channel];
            }
        }
    }
    Ok(Prepared {
        tensor: Tensor { shape: vec![1, 3, mh, mw], values: output },
        transform: Transform { oriented_w: ow, oriented_h: oh, model_w: mw, model_h: mh },
        _reservation: reservation,
    })
}

fn source_xy(image: PixelView<'_>, x: usize, y: usize) -> (usize, usize) {
    let (w, h) = (image.width, image.height);
    match image.orientation {
        ImageOrientation::Normal => (x, y),
        ImageOrientation::MirrorHorizontal => (w - 1 - x, y),
        ImageOrientation::Rotate180 => (w - 1 - x, h - 1 - y),
        ImageOrientation::MirrorVertical => (x, h - 1 - y),
        ImageOrientation::MirrorHorizontalRotate270 => (y, x),
        ImageOrientation::Rotate90 => (y, h - 1 - x),
        ImageOrientation::MirrorHorizontalRotate90 => (w - 1 - y, h - 1 - x),
        ImageOrientation::Rotate270 => (w - 1 - y, x),
    }
}

fn pixel_bgr(image: PixelView<'_>, x: usize, y: usize) -> Result<[f32; 3], ConversionError> {
    let (x, y) = source_xy(image, x, y);
    let base = y
        .checked_mul(image.row_stride)
        .and_then(|n| n.checked_add(x * image.format.channels()))
        .ok_or_else(|| limit("pixelStride"))?;
    let p = &image.bytes[base..base + image.format.channels()];
    Ok(match image.format {
        PixelFormat::Gray8 => [f32::from(p[0]); 3],
        PixelFormat::Rgb8 | PixelFormat::Rgba8 => {
            [f32::from(p[2]), f32::from(p[1]), f32::from(p[0])]
        }
        PixelFormat::Bgr8 | PixelFormat::Bgra8 => {
            [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])]
        }
    })
}

fn bilinear(
    image: PixelView<'_>,
    sample_x: f64,
    sample_y: f64,
) -> Result<[f32; 3], ConversionError> {
    let (ow, oh) = oriented_size(image);
    let x0 = sample_x.floor() as usize;
    let y0 = sample_y.floor() as usize;
    let x1 = (x0 + 1).min(ow - 1);
    let y1 = (y0 + 1).min(oh - 1);
    let fx = (sample_x - x0 as f64) as f32;
    let fy = (sample_y - y0 as f64) as f32;
    let top_left = pixel_bgr(image, x0, y0)?;
    let top_right = pixel_bgr(image, x1, y0)?;
    let bottom_left = pixel_bgr(image, x0, y1)?;
    let bottom_right = pixel_bgr(image, x1, y1)?;
    let mut out = [0.0; 3];
    for c in 0..3 {
        out[c] = (top_left[c] * (1.0 - fx) + top_right[c] * fx) * (1.0 - fy)
            + (bottom_left[c] * (1.0 - fx) + bottom_right[c] * fx) * fy;
    }
    Ok(out)
}

fn postprocess(
    outputs: &[Tensor],
    image: PixelView<'_>,
    transform: Transform,
    config: &DetectionConfig,
    context: &ExecutionContext,
) -> Result<DetectionResult, ConversionError> {
    let [output] = outputs else {
        return Err(ocr("detectionOutputCountMismatch"));
    };
    if output.shape != [1, 1, transform.model_h, transform.model_w] {
        return Err(ocr("detectionOutputShapeMismatch"));
    }
    let pixels =
        transform.model_w.checked_mul(transform.model_h).ok_or_else(|| limit("modelPixels"))?;
    if output.values.len() != pixels {
        return Err(ocr("detectionOutputElementCountMismatch"));
    }
    if output.values.iter().any(|value| !value.is_finite() || !(0.0..=1.0).contains(value)) {
        return Err(ocr("invalidDetectionProbability"));
    }
    // imageproc's Suzuki-Abe implementation retains one i32 label and at most one
    // point per source pixel in addition to this u8 bitmap. This cooperative
    // logical reservation is not a measurement of allocator metadata or RSS.
    let logical = pixels
        .checked_mul(1 + std::mem::size_of::<i32>() + std::mem::size_of::<Point<i32>>())
        .and_then(|bytes| bytes.checked_add(config.max_candidates.checked_mul(256)?))
        .and_then(|bytes| {
            bytes.checked_add(
                config
                    .max_offset_points
                    .checked_mul(std::mem::size_of::<PointD>())?
                    .checked_mul(4)?,
            )
        })
        .ok_or_else(|| limit("contourMemory"))?;
    let _geometry =
        context.reserve_memory(u64::try_from(logical).map_err(|_| limit("contourMemory"))?)?;
    let mut bitmap = Vec::new();
    bitmap.try_reserve_exact(pixels).map_err(|_| limit("contourMemory"))?;
    bitmap.extend(output.values.iter().map(|value| u8::from(*value > config.bitmap_threshold)));
    let width = u32::try_from(transform.model_w).map_err(|_| limit("modelPixels"))?;
    let height = u32::try_from(transform.model_h).map_err(|_| limit("modelPixels"))?;
    let mask =
        GrayImage::from_raw(width, height, bitmap).ok_or_else(|| ocr("invalidDetectionBitmap"))?;
    context.checkpoint()?;
    let contours = find_contours::<i32>(&mask);
    let contour_points = contours.iter().try_fold(0_usize, |total, contour| {
        total.checked_add(contour.points.len()).ok_or_else(|| limit("contourPoints"))
    })?;
    if contour_points > config.max_contour_points {
        return Err(limit("contourPoints"));
    }
    let mut regions = Vec::new();
    regions
        .try_reserve(config.max_candidates.min(contours.len()))
        .map_err(|_| limit("contourMemory"))?;
    for (index, contour) in contours.iter().take(config.max_candidates).enumerate() {
        if index % 32 == 0 {
            context.checkpoint()?;
        }
        if contour.points.len() < 3 || contour.points.len() > pixels {
            continue;
        }
        let first = min_area_rect(&contour.points);
        let first_quad = first.map(|p| [f64::from(p.x), f64::from(p.y)]);
        let short = quad_sides(first_quad).0;
        if short < 3.0 {
            continue;
        }
        let confidence =
            polygon_score(&output.values, transform.model_w, transform.model_h, first_quad)?;
        if confidence < config.box_threshold {
            continue;
        }
        let expanded =
            unclip(first_quad, f64::from(config.unclip_ratio), config.max_offset_points)?;
        let Some(expanded) = expanded else { continue };
        let final_quad = minimum_rect_f64(&expanded)?;
        let (short, _) = quad_sides(final_quad);
        if short < 5.0 {
            continue;
        }
        let source =
            canonical_quad(final_quad.map(|point| model_to_source(point, image, transform)));
        if source.iter().any(|point| !point.0.is_finite() || !point.1.is_finite()) {
            return Err(ocr("invalidDetectionGeometry"));
        }
        let source_sides = quad_sides(source.map(|p| [f64::from(p.0), f64::from(p.1)]));
        let crop_width = source_sides.1.ceil().clamp(1.0, f64::from(u32::MAX)) as u32;
        let crop_height = source_sides.0.ceil().clamp(1.0, f64::from(u32::MAX)) as u32;
        let dx = source[1].0 - source[0].0;
        let dy = source[1].1 - source[0].1;
        let angle = dy.atan2(dx).to_degrees();
        regions.push(DetectedTextRegion {
            polygon: source,
            angle_degrees: angle,
            confidence,
            crop: CropDescriptor { polygon: source, width: crop_width, height: crop_height },
        });
    }
    sort_reading_order(&mut regions);
    Ok(DetectionResult { regions, provider: format!("{PROVIDER}/{MODEL_ID}") })
}

fn polygon_score(
    values: &[f32],
    width: usize,
    height: usize,
    polygon: [[f64; 2]; 4],
) -> Result<f32, ConversionError> {
    let min_x = polygon
        .iter()
        .map(|p| p[0])
        .fold(f64::INFINITY, f64::min)
        .floor()
        .clamp(0.0, (width - 1) as f64) as usize;
    let max_x = polygon
        .iter()
        .map(|p| p[0])
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .clamp(0.0, (width - 1) as f64) as usize;
    let min_y = polygon
        .iter()
        .map(|p| p[1])
        .fold(f64::INFINITY, f64::min)
        .floor()
        .clamp(0.0, (height - 1) as f64) as usize;
    let max_y = polygon
        .iter()
        .map(|p| p[1])
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .clamp(0.0, (height - 1) as f64) as usize;
    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            if point_in_polygon([x as f64, y as f64], &polygon) {
                let value = *values
                    .get(
                        y.checked_mul(width)
                            .and_then(|n| n.checked_add(x))
                            .ok_or_else(|| limit("probabilityIndex"))?,
                    )
                    .ok_or_else(|| ocr("invalidProbabilityMap"))?;
                sum += f64::from(value);
                count = count.checked_add(1).ok_or_else(|| limit("scorePixels"))?;
            }
        }
    }
    if count == 0 {
        return Ok(0.0);
    }
    Ok((sum / count as f64) as f32)
}

fn point_in_polygon(point: [f64; 2], polygon: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let mut j = polygon.len() - 1;
    for i in 0..polygon.len() {
        let (a, b) = (polygon[i], polygon[j]);
        let cross = (point[0] - a[0]) * (b[1] - a[1]) - (point[1] - a[1]) * (b[0] - a[0]);
        if cross.abs() <= 1e-9
            && point[0] >= a[0].min(b[0])
            && point[0] <= a[0].max(b[0])
            && point[1] >= a[1].min(b[1])
            && point[1] <= a[1].max(b[1])
        {
            return true;
        }
        if ((a[1] > point[1]) != (b[1] > point[1]))
            && point[0] < (b[0] - a[0]) * (point[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn polygon_area_perimeter(polygon: &[[f64; 2]]) -> (f64, f64) {
    let mut area = 0.0;
    let mut perimeter = 0.0;
    for index in 0..polygon.len() {
        let a = polygon[index];
        let b = polygon[(index + 1) % polygon.len()];
        area += a[0] * b[1] - b[0] * a[1];
        perimeter += (b[0] - a[0]).hypot(b[1] - a[1]);
    }
    (area.abs() * 0.5, perimeter)
}

fn unclip(
    polygon: [[f64; 2]; 4],
    ratio: f64,
    max_offset_points: usize,
) -> Result<Option<Vec<[f64; 2]>>, ConversionError> {
    let (area, perimeter) = polygon_area_perimeter(&polygon);
    if area <= 0.0 || perimeter <= 0.0 {
        return Ok(None);
    }
    let distance = area * ratio / perimeter;
    if !distance.is_finite() {
        return Err(ocr("invalidDetectionGeometry"));
    }
    let path: Vec<PointD> = polygon.iter().map(|p| PointD::new(p[0], p[1])).collect();
    let paths =
        inflate_paths_d(&vec![path], distance, JoinType::Round, EndType::Polygon, 2.0, 3, 0.0);
    if paths.len() != 1 || paths[0].len() < 3 {
        return Ok(None);
    }
    if paths[0].len() > max_offset_points {
        return Err(limit("offsetPoints"));
    }
    let result = paths[0].iter().map(|p| [p.x, p.y]).collect::<Vec<_>>();
    Ok(result.iter().flatten().all(|v| v.is_finite()).then_some(result))
}

fn minimum_rect_f64(points: &[[f64; 2]]) -> Result<[[f64; 2]; 4], ConversionError> {
    // PaddleOCR passes its polygon through pyclipper's integer Path before the
    // second minAreaRect. Round here as well; extra decimal scaling can overflow
    // imageproc's documented i32 orientation arithmetic.
    let min_x = points.iter().map(|point| point[0]).fold(f64::INFINITY, f64::min);
    let max_x = points.iter().map(|point| point[0]).fold(f64::NEG_INFINITY, f64::max);
    let min_y = points.iter().map(|point| point[1]).fold(f64::INFINITY, f64::min);
    let max_y = points.iter().map(|point| point[1]).fold(f64::NEG_INFINITY, f64::max);
    // imageproc converts the generic points to i32 and evaluates two products
    // of coordinate differences. This span bound keeps their difference well
    // below i32::MAX even if an upstream geometry dependency changes output.
    if !min_x.is_finite()
        || !max_x.is_finite()
        || !min_y.is_finite()
        || !max_y.is_finite()
        || max_x - min_x > 16_000.0
        || max_y - min_y > 16_000.0
        || min_x < -16_000.0
        || max_x > 16_000.0
        || min_y < -16_000.0
        || max_y > 16_000.0
    {
        return Err(ocr("invalidDetectionGeometry"));
    }
    let integer = points
        .iter()
        .map(|p| {
            if !p[0].is_finite()
                || !p[1].is_finite()
                || p[0] < f64::from(i32::MIN)
                || p[0] > f64::from(i32::MAX)
                || p[1] < f64::from(i32::MIN)
                || p[1] > f64::from(i32::MAX)
            {
                return Err(ocr("invalidDetectionGeometry"));
            }
            Ok(Point::new(p[0].round() as i32, p[1].round() as i32))
        })
        .collect::<Result<Vec<_>, ConversionError>>()?;
    if integer.len() < 3 {
        return Err(ocr("invalidDetectionGeometry"));
    }
    Ok(min_area_rect(&integer).map(|p| [f64::from(p.x), f64::from(p.y)]))
}

fn quad_sides(quad: [[f64; 2]; 4]) -> (f64, f64) {
    let a = (quad[1][0] - quad[0][0]).hypot(quad[1][1] - quad[0][1]);
    let b = (quad[2][0] - quad[1][0]).hypot(quad[2][1] - quad[1][1]);
    (a.min(b), a.max(b))
}

fn model_to_source(point: [f64; 2], image: PixelView<'_>, transform: Transform) -> (f32, f32) {
    let oriented_x = (point[0] * transform.oriented_w as f64 / transform.model_w as f64)
        .clamp(0.0, (transform.oriented_w - 1) as f64);
    let oriented_y = (point[1] * transform.oriented_h as f64 / transform.model_h as f64)
        .clamp(0.0, (transform.oriented_h - 1) as f64);
    let (source_width, source_height) = (image.width as f64, image.height as f64);
    let source_point = match image.orientation {
        ImageOrientation::Normal => (oriented_x, oriented_y),
        ImageOrientation::MirrorHorizontal => (source_width - 1.0 - oriented_x, oriented_y),
        ImageOrientation::Rotate180 => {
            (source_width - 1.0 - oriented_x, source_height - 1.0 - oriented_y)
        }
        ImageOrientation::MirrorVertical => (oriented_x, source_height - 1.0 - oriented_y),
        ImageOrientation::MirrorHorizontalRotate270 => (oriented_y, oriented_x),
        ImageOrientation::Rotate90 => (oriented_y, source_height - 1.0 - oriented_x),
        ImageOrientation::MirrorHorizontalRotate90 => {
            (source_width - 1.0 - oriented_y, source_height - 1.0 - oriented_x)
        }
        ImageOrientation::Rotate270 => (source_width - 1.0 - oriented_y, oriented_x),
    };
    (
        source_point.0.clamp(0.0, source_width - 1.0) as f32,
        source_point.1.clamp(0.0, source_height - 1.0) as f32,
    )
}

fn canonical_quad(mut q: [(f32, f32); 4]) -> [(f32, f32); 4] {
    // PaddleOCR's get_mini_boxes splits by x, then sorts each side by y.
    q.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
    let mut left = [q[0], q[1]];
    let mut right = [q[2], q[3]];
    left.sort_by(|a, b| a.1.total_cmp(&b.1));
    right.sort_by(|a, b| a.1.total_cmp(&b.1));
    [left[0], right[0], right[1], left[1]]
}

fn sort_reading_order(regions: &mut [DetectedTextRegion]) {
    let center_y =
        |region: &DetectedTextRegion| region.polygon.iter().map(|point| point.1).sum::<f32>() / 4.0;
    regions.sort_by(|a, b| {
        center_y(a).total_cmp(&center_y(b)).then_with(|| a.polygon[0].0.total_cmp(&b.polygon[0].0))
    });
    // PaddleOCR applies adjacent same-line swaps after the primary y ordering.
    // The relative-height threshold retains that behavior across source scales.
    for _ in 0..regions.len() {
        let mut changed = false;
        for index in 0..regions.len().saturating_sub(1) {
            let a = &regions[index];
            let b = &regions[index + 1];
            let ah = ((a.polygon[3].1 - a.polygon[0].1).abs()
                + (a.polygon[2].1 - a.polygon[1].1).abs())
                * 0.5;
            let bh = ((b.polygon[3].1 - b.polygon[0].1).abs()
                + (b.polygon[2].1 - b.polygon[1].1).abs())
                * 0.5;
            if (center_y(a) - center_y(b)).abs() <= 0.5 * ah.max(bh)
                && a.polygon[0].0 > b.polygon[0].0
            {
                regions.swap(index, index + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}
fn ocr(detail: &str) -> ConversionError {
    ConversionError::Ocr { provider: PROVIDER.into(), detail: detail.into() }
}
fn limit(name: &'static str) -> ConversionError {
    ConversionError::ResourceLimit {
        limit: name,
        detail: "PP-OCRv6 detection safety bound exceeded".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use into_markdown_core::{CancellationToken, ExecutionOptions, ResourceLimits};
    use std::sync::Mutex;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn small_config() -> DetectionConfig {
        DetectionConfig {
            min_side_len: 32,
            max_side_len: 64,
            max_source_pixels: 4096,
            max_model_pixels: 4096,
            max_contour_points: 4096,
            max_offset_points: 512,
            ..DetectionConfig::default()
        }
    }

    fn view(bytes: &[u8], width: usize, height: usize) -> PixelView<'_> {
        PixelView {
            width,
            height,
            row_stride: width,
            format: PixelFormat::Gray8,
            orientation: ImageOrientation::Normal,
            bytes,
        }
    }

    #[test]
    fn embedded_authority_is_exact_and_config_is_bounded() {
        validate_authority().unwrap();
        assert!(
            validate_config(&DetectionConfig {
                bitmap_threshold: f32::NAN,
                ..DetectionConfig::default()
            })
            .is_err()
        );
        assert!(
            validate_config(&DetectionConfig {
                max_offset_points: 2,
                ..DetectionConfig::default()
            })
            .is_err()
        );
    }

    #[test]
    fn stride_rounding_matches_paddle_ties_to_even() {
        assert_eq!(round_stride(47.5).unwrap(), 32);
        assert_eq!(round_stride(48.0).unwrap(), 64);
        assert_eq!(round_stride(80.0).unwrap(), 64);
        assert_eq!(round_stride(80.1).unwrap(), 96);
    }

    #[test]
    fn controlled_pixels_reject_stride_truncation_and_empty_input() {
        let c = small_config();
        assert!(validate_pixels(view(&[], 0, 1), &c).is_err());
        assert!(validate_pixels(view(&[0; 3], 2, 2), &c).is_err());
        let rgb = PixelView {
            width: 2,
            height: 1,
            row_stride: 5,
            format: PixelFormat::Rgb8,
            orientation: ImageOrientation::Normal,
            bytes: &[0; 6],
        };
        assert!(validate_pixels(rgb, &c).is_err());
    }

    #[test]
    fn preprocessing_is_bgr_nchw_and_holds_its_logical_reservation() {
        let bytes = [255, 0, 0];
        let input = PixelView {
            width: 1,
            height: 1,
            row_stride: 3,
            format: PixelFormat::Rgb8,
            orientation: ImageOrientation::Normal,
            bytes: &bytes,
        };
        let prepared = preprocess(input, &small_config(), &context()).unwrap();
        assert_eq!(prepared.tensor.shape, [1, 3, 32, 32]);
        let plane = 32 * 32;
        assert!((prepared.tensor.values[0] - (0.0 - MEAN[0]) / STD[0]).abs() < 1e-6);
        assert!((prepared.tensor.values[plane] - (0.0 - MEAN[1]) / STD[1]).abs() < 1e-6);
        assert!((prepared.tensor.values[plane * 2] - (1.0 - MEAN[2]) / STD[2]).abs() < 1e-6);
    }

    #[test]
    fn all_orientation_mappings_stay_exact_for_a_rectangular_source() {
        let bytes = [0_u8; 6];
        let expected = [(0, 0), (2, 0), (2, 1), (0, 1), (0, 0), (0, 1), (2, 1), (2, 0)];
        let variants = [
            ImageOrientation::Normal,
            ImageOrientation::MirrorHorizontal,
            ImageOrientation::Rotate180,
            ImageOrientation::MirrorVertical,
            ImageOrientation::MirrorHorizontalRotate270,
            ImageOrientation::Rotate90,
            ImageOrientation::MirrorHorizontalRotate90,
            ImageOrientation::Rotate270,
        ];
        for (orientation, expected) in variants.into_iter().zip(expected) {
            let image = PixelView {
                width: 3,
                height: 2,
                row_stride: 3,
                format: PixelFormat::Gray8,
                orientation,
                bytes: &bytes,
            };
            assert_eq!(source_xy(image, 0, 0), expected);
        }
    }

    #[test]
    fn inverse_resize_orientation_maps_model_centers_back_to_source_centers() {
        let bytes = [0_u8; 6];
        let variants = [
            (ImageOrientation::Normal, (0.0, 0.0)),
            (ImageOrientation::MirrorHorizontal, (2.0, 0.0)),
            (ImageOrientation::Rotate180, (2.0, 1.0)),
            (ImageOrientation::MirrorVertical, (0.0, 1.0)),
            (ImageOrientation::MirrorHorizontalRotate270, (0.0, 0.0)),
            (ImageOrientation::Rotate90, (0.0, 1.0)),
            (ImageOrientation::MirrorHorizontalRotate90, (2.0, 1.0)),
            (ImageOrientation::Rotate270, (2.0, 0.0)),
        ];
        for (orientation, expected) in variants {
            let image = PixelView {
                width: 3,
                height: 2,
                row_stride: 3,
                format: PixelFormat::Gray8,
                orientation,
                bytes: &bytes,
            };
            let (ow, oh) = oriented_size(image);
            let actual = model_to_source(
                [0.0, 0.0],
                image,
                Transform { oriented_w: ow, oriented_h: oh, model_w: ow * 4, model_h: oh * 4 },
            );
            assert_eq!(actual, expected);
            let far_oriented = [
                (ow - 1) as f64 * (ow * 4) as f64 / ow as f64,
                (oh - 1) as f64 * (oh * 4) as f64 / oh as f64,
            ];
            let far = model_to_source(
                far_oriented,
                image,
                Transform { oriented_w: ow, oriented_h: oh, model_w: ow * 4, model_h: oh * 4 },
            );
            let expected_far = source_xy(image, ow - 1, oh - 1);
            assert_eq!(far, (expected_far.0 as f32, expected_far.1 as f32));
        }
    }

    #[test]
    fn transparent_alpha_is_ignored_like_the_official_bgr_conversion() {
        let bytes = [10, 20, 30, 0];
        let image = PixelView {
            width: 1,
            height: 1,
            row_stride: 4,
            format: PixelFormat::Rgba8,
            orientation: ImageOrientation::Normal,
            bytes: &bytes,
        };
        assert_eq!(
            pixel_bgr(image, 0, 0).unwrap().map(f32::to_bits),
            [30.0_f32, 20.0, 10.0].map(f32::to_bits)
        );
    }

    #[test]
    fn official_shaped_db_golden_matches_rectangle_score_offset_and_box() {
        let (width, height) = (96, 64);
        let mut values = vec![0.0; width * height];
        for y in 20..=39 {
            for x in 20..=59 {
                values[y * width + x] = 0.9;
            }
        }
        let bytes = vec![0; width * height];
        let result = postprocess(
            &[Tensor { shape: vec![1, 1, height, width], values }],
            view(&bytes, width, height),
            Transform { oriented_w: width, oriented_h: height, model_w: width, model_h: height },
            &DetectionConfig {
                max_source_pixels: width * height,
                max_model_pixels: width * height,
                max_contour_points: width * height,
                max_offset_points: 512,
                ..DetectionConfig::default()
            },
            &context(),
        )
        .unwrap();
        assert_eq!(result.regions.len(), 1);
        let region = &result.regions[0];
        assert!((region.confidence - 0.9).abs() <= 1e-5);
        let xs = region.polygon.map(|point| point.0);
        let ys = region.polygon.map(|point| point.1);
        let min_x = xs.into_iter().fold(f32::INFINITY, f32::min);
        let max_x = xs.into_iter().fold(f32::NEG_INFINITY, f32::max);
        let min_y = ys.into_iter().fold(f32::INFINITY, f32::min);
        let max_y = ys.into_iter().fold(f32::NEG_INFINITY, f32::max);
        // OpenCV RETR_LIST/CHAIN_APPROX_SIMPLE + minAreaRect and pyclipper's
        // JT_ROUND/ET_CLOSEDPOLYGON reference is [11,11]-[68,48].
        assert!((min_x - 11.0).abs() <= 1.0, "min_x={min_x}");
        assert!((max_x - 68.0).abs() <= 1.0, "max_x={max_x}");
        assert!((min_y - 11.0).abs() <= 1.0, "min_y={min_y}");
        assert!((max_y - 48.0).abs() <= 1.0, "max_y={max_y}");
    }

    #[test]
    fn rotated_box_is_clockwise_and_starts_at_top_left() {
        let (width, height) = (112, 96);
        let mut values = vec![0.0; width * height];
        let polygon = [[18.0, 35.0], [34.0, 14.0], [88.0, 54.0], [72.0, 75.0]];
        for y in 0..height {
            for x in 0..width {
                if point_in_polygon([x as f64, y as f64], &polygon) {
                    values[y * width + x] = 0.95;
                }
            }
        }
        let bytes = vec![0; width * height];
        let result = postprocess(
            &[Tensor { shape: vec![1, 1, height, width], values }],
            view(&bytes, width, height),
            Transform { oriented_w: width, oriented_h: height, model_w: width, model_h: height },
            &DetectionConfig {
                max_source_pixels: width * height,
                max_model_pixels: width * height,
                max_contour_points: width * height,
                max_offset_points: 512,
                ..DetectionConfig::default()
            },
            &context(),
        )
        .unwrap();
        assert_eq!(result.regions.len(), 1);
        let region = &result.regions[0];
        let q = region.polygon;
        assert!(q[0].1 <= q[3].1 && q[1].1 <= q[2].1, "{q:?}");
        let cross = (q[1].0 - q[0].0) * (q[2].1 - q[1].1) - (q[1].1 - q[0].1) * (q[2].0 - q[1].0);
        assert!(cross > 0.0, "{q:?}");
        // Reference after source-bound clipping and Paddle point ordering.
        let reference = [(30.7121, 0.0), (107.0248, 50.1665), (74.2879, 94.3614), (0.0, 37.8335)];
        for (actual, expected) in q.into_iter().zip(reference) {
            assert!((actual.0 - expected.0).abs() <= 3.0, "actual={q:?}");
            assert!((actual.1 - expected.1).abs() <= 3.0, "actual={q:?}");
        }
        assert!((region.confidence - 0.893_830_3).abs() <= 0.03);
    }

    #[test]
    fn retr_list_hole_is_scored_but_does_not_duplicate_a_filled_ring() {
        let (width, height) = (96, 96);
        let mut values = vec![0.0; width * height];
        for y in 15..=80 {
            for x in 15..=80 {
                if !(35..=60).contains(&x) || !(35..=60).contains(&y) {
                    values[y * width + x] = 0.9;
                }
            }
        }
        let mask = GrayImage::from_raw(
            width as u32,
            height as u32,
            values.iter().map(|value| u8::from(*value > 0.2)).collect(),
        )
        .unwrap();
        assert!(find_contours::<i32>(&mask).len() >= 2, "fixture must exercise outer and hole");
        let bytes = vec![0; width * height];
        let result = postprocess(
            &[Tensor { shape: vec![1, 1, height, width], values }],
            view(&bytes, width, height),
            Transform { oriented_w: width, oriented_h: height, model_w: width, model_h: height },
            &DetectionConfig {
                max_source_pixels: width * height,
                max_model_pixels: width * height,
                max_contour_points: width * height,
                max_offset_points: 512,
                ..DetectionConfig::default()
            },
            &context(),
        )
        .unwrap();
        assert_eq!(result.regions.len(), 1);
        // The inner RETR_LIST contour is traversed but its polygon mean is below
        // box_threshold. OpenCV/pyclipper reference emits only the outer box.
        assert!((result.regions[0].confidence - 0.760_330_56).abs() <= 0.03);
    }

    #[test]
    fn multiple_probability_islands_are_sorted_by_line_then_left_to_right() {
        let (width, height) = (128, 96);
        let mut values = vec![0.0; width * height];
        for (left, top, right, bottom) in [(70, 15, 110, 28), (10, 16, 50, 29), (20, 55, 80, 70)] {
            for y in top..=bottom {
                for x in left..=right {
                    values[y * width + x] = 0.9;
                }
            }
        }
        let bytes = vec![0; width * height];
        let result = postprocess(
            &[Tensor { shape: vec![1, 1, height, width], values }],
            view(&bytes, width, height),
            Transform { oriented_w: width, oriented_h: height, model_w: width, model_h: height },
            &DetectionConfig {
                max_source_pixels: width * height,
                max_model_pixels: width * height,
                max_contour_points: width * height,
                max_offset_points: 512,
                ..DetectionConfig::default()
            },
            &context(),
        )
        .unwrap();
        assert_eq!(result.regions.len(), 3);
        let centers = result
            .regions
            .iter()
            .map(|region| {
                (
                    region.polygon.iter().map(|point| point.0).sum::<f32>() / 4.0,
                    region.polygon.iter().map(|point| point.1).sum::<f32>() / 4.0,
                )
            })
            .collect::<Vec<_>>();
        assert!(centers[0].0 < centers[1].0, "{centers:?}");
        assert!(centers[0].1 < centers[2].1 && centers[1].1 < centers[2].1, "{centers:?}");
    }

    #[test]
    fn output_contract_and_probability_values_fail_closed() {
        let bytes = [0_u8; 32 * 32];
        let image = view(&bytes, 32, 32);
        let transform = Transform { oriented_w: 32, oriented_h: 32, model_w: 32, model_h: 32 };
        assert!(postprocess(&[], image, transform, &small_config(), &context()).is_err());
        let mut values = vec![0.0; 32 * 32];
        values[0] = f32::INFINITY;
        assert!(
            postprocess(
                &[Tensor { shape: vec![1, 1, 32, 32], values }],
                image,
                transform,
                &small_config(),
                &context(),
            )
            .is_err()
        );
    }

    #[test]
    fn empty_and_single_pixel_noise_maps_have_no_regions() {
        let bytes = [0_u8; 32 * 32];
        for hot in [None, Some(0)] {
            let mut values = vec![0.0; 32 * 32];
            if let Some(index) = hot {
                values[index] = 1.0;
            }
            let result = postprocess(
                &[Tensor { shape: vec![1, 1, 32, 32], values }],
                view(&bytes, 32, 32),
                Transform { oriented_w: 32, oriented_h: 32, model_w: 32, model_h: 32 },
                &small_config(),
                &context(),
            )
            .unwrap();
            assert!(result.regions.is_empty());
        }
    }

    #[test]
    fn cancellation_is_observed_before_pixel_work() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        assert!(preprocess(view(&[0], 1, 1), &small_config(), &context).is_err());
    }

    #[test]
    fn stable_reading_order_groups_lines_then_sorts_left_to_right() {
        let region = |x: f32, y: f32| DetectedTextRegion {
            polygon: [(x, y), (x + 10.0, y), (x + 10.0, y + 5.0), (x, y + 5.0)],
            angle_degrees: 0.0,
            confidence: 1.0,
            crop: CropDescriptor {
                polygon: [(x, y), (x + 10.0, y), (x + 10.0, y + 5.0), (x, y + 5.0)],
                width: 10,
                height: 5,
            },
        };
        let mut regions = vec![region(30.0, 0.5), region(0.0, 12.0), region(0.0, 0.0)];
        sort_reading_order(&mut regions);
        assert_eq!(
            regions.iter().map(|r| r.polygon[0]).collect::<Vec<_>>(),
            vec![(0.0, 0.0), (30.0, 0.5), (0.0, 12.0)]
        );
    }

    #[test]
    fn model_unavailable_error_is_propagated_unchanged() {
        #[derive(Debug)]
        struct Unavailable;
        impl TensorRuntime for Unavailable {
            fn id(&self) -> &'static str {
                "test.unavailable"
            }
            fn run<'a>(
                &'a self,
                _: &'a str,
                _: &'a [Tensor],
                _: &'a ExecutionContext,
            ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
                Box::pin(async {
                    Err(ConversionError::ComponentUnavailable {
                        component: "onnx-model".into(),
                        detail: "ModelUnavailable".into(),
                    })
                })
            }
        }
        let detector = PpOcrTextDetector::new(Arc::new(Unavailable), small_config()).unwrap();
        let error = block_on(detector.detect(view(&[0], 1, 1), &context())).unwrap_err();
        assert!(matches!(
            error,
            ConversionError::ComponentUnavailable { ref detail, .. } if detail == "ModelUnavailable"
        ));
    }

    #[test]
    fn detector_calls_the_expected_model_with_nchw_and_accepts_a_fake_map() {
        #[derive(Debug, Default)]
        struct Fake {
            call: Mutex<Option<(String, Vec<usize>, f32)>>,
        }
        impl TensorRuntime for Fake {
            fn id(&self) -> &'static str {
                "test.fake"
            }
            fn run<'a>(
                &'a self,
                model_id: &'a str,
                inputs: &'a [Tensor],
                _: &'a ExecutionContext,
            ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
                Box::pin(async move {
                    let input = inputs.first().unwrap();
                    *self.call.lock().unwrap() =
                        Some((model_id.to_owned(), input.shape.clone(), input.values[0]));
                    let height = input.shape[2];
                    let width = input.shape[3];
                    Ok(vec![Tensor {
                        shape: vec![1, 1, height, width],
                        values: vec![0.0; height * width],
                    }])
                })
            }
        }
        let fake = Arc::new(Fake::default());
        let detector = PpOcrTextDetector::new(fake.clone(), small_config()).unwrap();
        let result = block_on(detector.detect(view(&[255], 1, 1), &context())).unwrap();
        assert!(result.regions.is_empty());
        let call = fake.call.lock().unwrap().clone().unwrap();
        assert_eq!(call.0, MODEL_ID);
        assert_eq!(call.1, [1, 3, 32, 32]);
        assert!((call.2 - (1.0 - MEAN[0]) / STD[0]).abs() < 1e-6);
    }
}
