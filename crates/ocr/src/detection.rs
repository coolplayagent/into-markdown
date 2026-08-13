//! Bounded PP-OCRv6 text-detection preprocessing and DB postprocessing.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use clipper2_rust::{EndType, JoinType, Point64, inflate_paths_64};
use imageproc::geometry::{convex_hull, min_area_rect};
use imageproc::point::Point;
use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, ResourceReservation, Tensor, TensorRuntime,
};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::Arc;

const PROVIDER: &str = "builtin.ocr.ppocrv6-detector";
const MODEL_ID: &str = "pp-ocrv6-tiny-zh-en";
const STRIDE: usize = 32;
const SCALE: f32 = 1.0 / 255.0;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];
const MIN_SIDE_LEN: usize = 736;
const MAX_SIDE_LEN: usize = 4000;
const BITMAP_THRESHOLD: f32 = 0.2;
const BOX_THRESHOLD: f32 = 0.4;
const UNCLIP_RATIO: f64 = 1.4;
const MAX_CANDIDATES: usize = 3000;
// A rotated minimum rectangle over a 4000x4000 map can extend outside the map;
// twice the maximum side is a conservative bound for each integer edge span.
const SCORE_MAX_EDGE_STEPS: usize = MAX_SIDE_LEN * 2 + 1;
const SCORE_WORK_PER_PIXEL_UPPER_BOUND: usize = 1 + 4 * SCORE_MAX_EDGE_STEPS + 4;
// clipper2-rust 1.1.0 uses ARC_CONST=0.002 when arc_tolerance is zero.
// Its maximum circle step count is <50, so each of four round joins emits at
// most ceil(25)+1 points. The finishing union cannot add vertices to the one
// convex input path. Keep the bound deliberately rounded up to 104.
const OFFSET_MAX_GENERATED_POINTS: usize = 104;
const OFFSET_MAX_PATH_HEADERS: usize = OFFSET_MAX_GENERATED_POINTS + 4;
const OFFSET_MAX_WORK_UNITS: usize = 11_000;
const OFFSET_BYTES_PER_POINT: usize = 1024;
const OFFSET_BYTES_PER_HEADER: usize = 256;
const OFFSET_BYTES_PER_WORK_UNIT: usize = 256;

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

/// Local safety bounds. Detection algorithm parameters come only from the
/// embedded, commit-pinned authority and cannot be changed by callers.
#[derive(Debug, Clone)]
pub struct DetectionConfig {
    pub max_source_pixels: usize,
    pub max_model_pixels: usize,
    pub max_contour_events: usize,
    pub max_contour_points: usize,
    pub max_score_pixels: usize,
    pub max_score_work: usize,
    pub max_offset_points: usize,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            max_source_pixels: 100_000_000,
            max_model_pixels: 16_000_000,
            max_contour_events: 16_000_000,
            max_contour_points: 16_000_000,
            max_score_pixels: 32_000_000,
            max_score_work: 32_000_000 * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
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
    opencv_reference_version: String,
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
    contour_result_order: String,
    score_mode: String,
    box_type: String,
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
        || value.opencv_reference_version != "4.13.0"
        || value.resize_interpolation != "INTER_LINEAR-to-uint8"
        || value.minimum_side != 736
        || value.maximum_side != 4000
        || (value.scale as f32).to_bits() != SCALE.to_bits()
        || value.mean.map(f32::to_bits) != MEAN.map(f32::to_bits)
        || value.standard_deviation.map(f32::to_bits) != STD.map(f32::to_bits)
        || value.bitmap_threshold.to_bits() != 0.2_f32.to_bits()
        || value.box_threshold.to_bits() != 0.4_f32.to_bits()
        || value.maximum_candidates != 3000
        || value.unclip_ratio.to_bits() != 1.4_f32.to_bits()
        || value.contour_retrieval != "list"
        || value.contour_approximation != "simple"
        || value.contour_result_order != "reverse-scan"
        || value.score_mode != "fast"
        || value.box_type != "quad"
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
    if c.max_source_pixels == 0
        || c.max_model_pixels == 0
        || c.max_contour_events == 0
        || c.max_contour_points == 0
        || c.max_score_pixels == 0
        || c.max_score_work == 0
        || c.max_offset_points < 3
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

fn round_stride(value: usize) -> Result<usize, ConversionError> {
    if value == 0 {
        return Err(limit("modelPixels"));
    }
    let stride_units = (value as f64 / STRIDE as f64).round_ties_even().max(1.0);
    if stride_units > usize::MAX as f64 {
        return Err(limit("modelPixels"));
    }
    (stride_units as usize).checked_mul(STRIDE).ok_or_else(|| limit("modelPixels"))
}

fn official_resize_dimensions(
    width: usize,
    height: usize,
) -> Result<(usize, usize), ConversionError> {
    let short = width.min(height) as f64;
    let ratio = if short < MIN_SIDE_LEN as f64 { MIN_SIDE_LEN as f64 / short } else { 1.0 };
    // Paddle first converts each scaled dimension with Python int(), which
    // truncates a positive value toward zero.
    let mut resized_w = (width as f64 * ratio) as usize;
    let mut resized_h = (height as f64 * ratio) as usize;
    let resized_long = resized_w.max(resized_h);
    if resized_long > MAX_SIDE_LEN {
        let limit_ratio = MAX_SIDE_LEN as f64 / resized_long as f64;
        resized_w = (resized_w as f64 * limit_ratio) as usize;
        resized_h = (resized_h as f64 * limit_ratio) as usize;
    }
    Ok((round_stride(resized_w)?, round_stride(resized_h)?))
}

fn preprocess(
    image: PixelView<'_>,
    c: &DetectionConfig,
    context: &ExecutionContext,
) -> Result<Prepared, ConversionError> {
    validate_pixels(image, c)?;
    let (ow, oh) = oriented_size(image);
    // DetResizeForTest pads only this tiny-image case before it computes the
    // resize ratio. The recorded destination shape remains the unpadded source.
    let (padded_w, padded_h) = if ow.checked_add(oh).ok_or_else(|| limit("sourcePixels"))? < 64 {
        (ow.max(STRIDE), oh.max(STRIDE))
    } else {
        (ow, oh)
    };
    let (mw, mh) = official_resize_dimensions(padded_w, padded_h)?;
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
        let sy = ((y as f64 + 0.5) * padded_h as f64 / mh as f64 - 0.5)
            .clamp(0.0, (padded_h - 1) as f64);
        for x in 0..mw {
            let sx = ((x as f64 + 0.5) * padded_w as f64 / mw as f64 - 0.5)
                .clamp(0.0, (padded_w - 1) as f64);
            let bgr = bilinear_u8(image, ow, oh, padded_w, padded_h, sx, sy)?;
            for channel in 0..3 {
                output[channel * count + y * mw + x] = normalize(bgr[channel], channel);
            }
        }
    }
    Ok(Prepared {
        tensor: Tensor { shape: vec![1, 3, mh, mw], values: output },
        transform: Transform { oriented_w: ow, oriented_h: oh, model_w: mw, model_h: mh },
        _reservation: reservation,
    })
}

fn normalize(pixel: u8, channel: usize) -> f32 {
    // Pinned Paddle NormalizeImage evaluates the parsed scale as float32 and
    // multiplies the float32 image before subtracting mean and dividing by std.
    (f32::from(pixel) * SCALE - MEAN[channel]) / STD[channel]
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

fn pixel_bgr(image: PixelView<'_>, x: usize, y: usize) -> Result<[u8; 3], ConversionError> {
    let (x, y) = source_xy(image, x, y);
    let base = y
        .checked_mul(image.row_stride)
        .and_then(|n| n.checked_add(x * image.format.channels()))
        .ok_or_else(|| limit("pixelStride"))?;
    let p = &image.bytes[base..base + image.format.channels()];
    Ok(match image.format {
        PixelFormat::Gray8 => [p[0]; 3],
        PixelFormat::Rgb8 | PixelFormat::Rgba8 => [p[2], p[1], p[0]],
        PixelFormat::Bgr8 | PixelFormat::Bgra8 => [p[0], p[1], p[2]],
    })
}

fn padded_pixel_bgr(
    image: PixelView<'_>,
    oriented_w: usize,
    oriented_h: usize,
    x: usize,
    y: usize,
) -> Result<[u8; 3], ConversionError> {
    if x >= oriented_w || y >= oriented_h { Ok([0; 3]) } else { pixel_bgr(image, x, y) }
}

fn bilinear_u8(
    image: PixelView<'_>,
    oriented_w: usize,
    oriented_h: usize,
    padded_w: usize,
    padded_h: usize,
    sample_x: f64,
    sample_y: f64,
) -> Result<[u8; 3], ConversionError> {
    let x0 = sample_x.floor() as usize;
    let y0 = sample_y.floor() as usize;
    let x1 = (x0 + 1).min(padded_w - 1);
    let y1 = (y0 + 1).min(padded_h - 1);
    let fx = sample_x - x0 as f64;
    let fy = sample_y - y0 as f64;
    let top_left = padded_pixel_bgr(image, oriented_w, oriented_h, x0, y0)?;
    let top_right = padded_pixel_bgr(image, oriented_w, oriented_h, x1, y0)?;
    let bottom_left = padded_pixel_bgr(image, oriented_w, oriented_h, x0, y1)?;
    let bottom_right = padded_pixel_bgr(image, oriented_w, oriented_h, x1, y1)?;
    let mut out = [0; 3];
    for c in 0..3 {
        let value = (f64::from(top_left[c]) * (1.0 - fx) + f64::from(top_right[c]) * fx)
            * (1.0 - fy)
            + (f64::from(bottom_left[c]) * (1.0 - fx) + f64::from(bottom_right[c]) * fx) * fy;
        out[c] = value.round_ties_even().clamp(0.0, 255.0) as u8;
    }
    Ok(out)
}

// Border following below is a request-accounted adaptation of the Suzuki-Abe
// scanner in imageproc 0.25.0 (`src/contours.rs`), copyright 2015
// PistonDevelopers, MIT licensed. Unlike the dependency API it emits one
// contour at a time, polls cancellation while scanning/following, and never
// materializes an attacker-controlled Vec of every contour. RETR_LIST does not
// require hierarchy, so parent bookkeeping is deliberately omitted.
fn scan_contours(
    bitmap: &[u8],
    width: usize,
    height: usize,
    config: &DetectionConfig,
    context: &ExecutionContext,
    reservation: &mut ResourceReservation,
) -> Result<Vec<Vec<Point<i32>>>, ConversionError> {
    let pixels = width.checked_mul(height).ok_or_else(|| limit("modelPixels"))?;
    let mut labels = Vec::new();
    labels.try_reserve_exact(pixels).map_err(|_| limit("contourMemory"))?;
    labels.extend(bitmap.iter().map(|value| i32::from(*value)));
    let at = |x: usize, y: usize| x + width * y;
    let mut diffs = VecDeque::from([
        Point::new(-1, 0),
        Point::new(-1, -1),
        Point::new(0, -1),
        Point::new(1, -1),
        Point::new(1, 0),
        Point::new(1, 1),
        Point::new(0, 1),
        Point::new(-1, 1),
    ]);
    let mut selected = VecDeque::<Vec<Point<i32>>>::new();
    let mut border_number = 1_i32;
    let mut events = 0_usize;
    let mut total_points = 0_usize;

    for y in 0..height {
        if y % 16 == 0 {
            context.checkpoint()?;
        }
        for x in 0..width {
            let point_x = i32::try_from(x).map_err(|_| limit("modelPixels"))?;
            let point_y = i32::try_from(y).map_err(|_| limit("modelPixels"))?;
            if labels[at(x, y)] == 0 {
                continue;
            }
            let adjacent = if labels[at(x, y)] == 1 && (x == 0 || labels[at(x - 1, y)] == 0) {
                Some(Point::new(point_x - 1, point_y))
            } else if labels[at(x, y)] > 0 && (x + 1 == width || labels[at(x + 1, y)] == 0) {
                Some(Point::new(point_x + 1, point_y))
            } else {
                None
            };
            let Some(adjacent) = adjacent else { continue };
            events = events.checked_add(1).ok_or_else(|| limit("contourEvents"))?;
            if events > config.max_contour_events {
                return Err(limit("contourEvents"));
            }
            border_number = border_number.checked_add(1).ok_or_else(|| limit("contourEvents"))?;
            let start = Point::new(point_x, point_y);
            rotate_diffs(&mut diffs, adjacent - start)?;
            let neighbor = diffs.iter().find_map(|difference| {
                foreground_neighbor(&labels, width, height, start + *difference)
            });
            let mut points = Vec::<Point<i32>>::new();
            if let Some(first) = neighbor {
                let mut previous = first;
                let mut current = start;
                loop {
                    if points.len() == points.capacity() {
                        let extra = 64_usize;
                        let bytes = extra
                            .checked_mul(std::mem::size_of::<Point<i32>>())
                            .ok_or_else(|| limit("contourMemory"))?;
                        reservation
                            .grow(u64::try_from(bytes).map_err(|_| limit("contourMemory"))?)?;
                        if points.try_reserve_exact(extra).is_err() {
                            reservation.shrink(
                                u64::try_from(bytes).map_err(|_| limit("contourMemory"))?,
                            )?;
                            return Err(limit("contourMemory"));
                        }
                    }
                    points.push(current);
                    total_points =
                        total_points.checked_add(1).ok_or_else(|| limit("contourPoints"))?;
                    if total_points > config.max_contour_points {
                        return Err(limit("contourPoints"));
                    }
                    if total_points.is_multiple_of(4096) {
                        context.checkpoint()?;
                    }
                    rotate_diffs(&mut diffs, previous - current)?;
                    let next = diffs
                        .iter()
                        .rev()
                        .find_map(|difference| {
                            foreground_neighbor(&labels, width, height, current + *difference)
                        })
                        .ok_or_else(|| ocr("invalidContourTopology"))?;
                    let mut right_edge = false;
                    for difference in diffs.iter().rev() {
                        if *difference == next - current {
                            break;
                        }
                        if *difference == Point::new(1, 0) {
                            right_edge = true;
                            break;
                        }
                    }
                    let index = at(current.x as usize, current.y as usize);
                    if current.x as usize + 1 == width || right_edge {
                        labels[index] = -border_number;
                    } else if labels[index] == 1 {
                        labels[index] = border_number;
                    }
                    if next == start && current == first {
                        break;
                    }
                    previous = current;
                    current = next;
                }
            } else {
                reservation.grow(std::mem::size_of::<Point<i32>>() as u64)?;
                points.try_reserve_exact(1).map_err(|_| limit("contourMemory"))?;
                points.push(start);
                labels[at(x, y)] = -border_number;
                total_points = total_points.checked_add(1).ok_or_else(|| limit("contourPoints"))?;
            }
            chain_approx_simple(&mut points);
            // OpenCV's current RETR_LIST implementation exposes completed
            // contours in reverse scanner order. Keep only the suffix needed by
            // maxCandidates, then reverse once scanning and event accounting end.
            selected.push_back(points);
            if selected.len() > MAX_CANDIDATES
                && let Some(discarded) = selected.pop_front()
            {
                let bytes = discarded
                    .capacity()
                    .checked_mul(std::mem::size_of::<Point<i32>>())
                    .ok_or_else(|| limit("contourMemory"))?;
                drop(discarded);
                reservation.shrink(u64::try_from(bytes).map_err(|_| limit("contourMemory"))?)?;
            }
        }
    }
    let mut result = selected.into_iter().collect::<Vec<_>>();
    result.reverse();
    Ok(result)
}

fn foreground_neighbor(
    labels: &[i32],
    width: usize,
    height: usize,
    point: Point<i32>,
) -> Option<Point<i32>> {
    (point.x >= 0
        && point.y >= 0
        && (point.x as usize) < width
        && (point.y as usize) < height
        && labels[point.y as usize * width + point.x as usize] != 0)
        .then_some(point)
}

fn rotate_diffs(
    diffs: &mut VecDeque<Point<i32>>,
    value: Point<i32>,
) -> Result<(), ConversionError> {
    let index = diffs
        .iter()
        .position(|difference| *difference == value)
        .ok_or_else(|| ocr("invalidContourDirection"))?;
    diffs.rotate_left(index);
    Ok(())
}

fn chain_approx_simple(points: &mut Vec<Point<i32>>) {
    if points.len() < 3 {
        return;
    }
    let original_len = points.len();
    let direction = |a: Point<i32>, b: Point<i32>| ((b.x - a.x).signum(), (b.y - a.y).signum());
    let mut write = 0;
    for index in 0..original_len {
        let previous = points[(index + original_len - 1) % original_len];
        let current = points[index];
        let next = points[(index + 1) % original_len];
        if direction(previous, current) != direction(current, next) {
            points[write] = current;
            write += 1;
        }
    }
    points.truncate(write);
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
    // Reserve all fixed scanner/output structures before allocation. Individual
    // contour capacities grow this guard before their Vec grows.
    let logical = pixels
        .checked_mul(1 + std::mem::size_of::<i32>())
        .and_then(|bytes| bytes.checked_add(MAX_CANDIDATES.checked_mul(256)?))
        .ok_or_else(|| limit("contourMemory"))?;
    let mut geometry =
        context.reserve_memory(u64::try_from(logical).map_err(|_| limit("contourMemory"))?)?;
    let mut bitmap = Vec::new();
    bitmap.try_reserve_exact(pixels).map_err(|_| limit("contourMemory"))?;
    for (index, value) in output.values.iter().enumerate() {
        if index.is_multiple_of(4096) {
            context.checkpoint()?;
        }
        if !value.is_finite() || !(0.0..=1.0).contains(value) {
            return Err(ocr("invalidDetectionProbability"));
        }
        bitmap.push(u8::from(*value > BITMAP_THRESHOLD));
    }
    context.checkpoint()?;
    let contours = scan_contours(
        &bitmap,
        transform.model_w,
        transform.model_h,
        config,
        context,
        &mut geometry,
    )?;
    let mut regions = Vec::new();
    regions.try_reserve(MAX_CANDIDATES.min(contours.len())).map_err(|_| limit("contourMemory"))?;
    let mut geometry_events = 0_usize;
    let mut score_budget = ScoreBudget {
        pixels: 0,
        max_pixels: config.max_score_pixels,
        work: 0,
        max_work: config.max_score_work,
    };
    for (index, contour) in contours.iter().enumerate() {
        if index % 32 == 0 {
            context.checkpoint()?;
        }
        if contour.len() < 3 || contour.len() > pixels {
            continue;
        }
        let first_quad = minimum_rect_contour(
            contour,
            context,
            &mut geometry,
            &mut geometry_events,
            config.max_contour_events,
        )?;
        if !is_convex_quad(first_quad) {
            return Err(ocr("invalidDetectionGeometry"));
        }
        let short = quad_sides(first_quad).0;
        if short < 3.0 {
            continue;
        }
        let confidence = polygon_score(
            &output.values,
            transform.model_w,
            transform.model_h,
            first_quad,
            context,
            &mut score_budget,
        )?;
        if confidence < BOX_THRESHOLD {
            continue;
        }
        let expanded = unclip(first_quad, UNCLIP_RATIO, config.max_offset_points, context)?;
        let Some(expanded) = expanded else { continue };
        let final_quad = minimum_rect_f64(&expanded.points)?;
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

struct ScoreBudget {
    pixels: usize,
    max_pixels: usize,
    work: usize,
    max_work: usize,
}

fn polygon_score(
    values: &[f32],
    width: usize,
    height: usize,
    polygon: [[f64; 2]; 4],
    context: &ExecutionContext,
    budget: &mut ScoreBudget,
) -> Result<f32, ConversionError> {
    polygon_score_with_checkpoint(values, width, height, polygon, budget, || context.checkpoint())
}

fn polygon_score_with_checkpoint<F>(
    values: &[f32],
    width: usize,
    height: usize,
    polygon: [[f64; 2]; 4],
    budget: &mut ScoreBudget,
    mut checkpoint: F,
) -> Result<f32, ConversionError>
where
    F: FnMut() -> Result<(), ConversionError>,
{
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
    let scan_pixels = max_x
        .checked_sub(min_x)
        .and_then(|span| span.checked_add(1))
        .and_then(|columns| {
            max_y
                .checked_sub(min_y)
                .and_then(|span| span.checked_add(1))
                .and_then(|rows| columns.checked_mul(rows))
        })
        .ok_or_else(|| limit("scorePixels"))?;
    budget.pixels = budget.pixels.checked_add(scan_pixels).ok_or_else(|| limit("scorePixels"))?;
    if budget.pixels > budget.max_pixels {
        return Err(limit("scorePixels"));
    }
    let conservative_work = scan_pixels
        .checked_mul(SCORE_WORK_PER_PIXEL_UPPER_BOUND)
        .ok_or_else(|| limit("scoreWork"))?;
    budget.work = budget.work.checked_add(conservative_work).ok_or_else(|| limit("scoreWork"))?;
    if budget.work > budget.max_work {
        return Err(limit("scoreWork"));
    }
    checkpoint()?;
    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    let mut scan_steps = 0_usize;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            if opencv_fill_poly_contains(
                i32::try_from(x).map_err(|_| limit("modelPixels"))?,
                i32::try_from(y).map_err(|_| limit("modelPixels"))?,
                &polygon,
                &mut scan_steps,
                &mut checkpoint,
            )? {
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

fn opencv_fill_poly_contains<F>(
    x: i32,
    y: i32,
    polygon: &[[f64; 2]; 4],
    scan_steps: &mut usize,
    checkpoint: &mut F,
) -> Result<bool, ConversionError>
where
    F: FnMut() -> Result<(), ConversionError>,
{
    score_scan_step(scan_steps, checkpoint)?;
    let integer = polygon.map(|point| Point::new(point[0] as i32, point[1] as i32));
    for (index, end) in integer.iter().enumerate() {
        if opencv_line_contains(
            integer[(index + integer.len() - 1) % integer.len()],
            *end,
            x,
            y,
            scan_steps,
            checkpoint,
        )? {
            return Ok(true);
        }
    }
    let converted = integer.map(|point| [f64::from(point.x), f64::from(point.y)]);
    Ok(point_in_polygon([f64::from(x), f64::from(y)], &converted))
}

fn score_scan_step<F>(scan_steps: &mut usize, checkpoint: &mut F) -> Result<(), ConversionError>
where
    F: FnMut() -> Result<(), ConversionError>,
{
    *scan_steps = scan_steps.checked_add(1).ok_or_else(|| limit("scoreWork"))?;
    if (*scan_steps).is_multiple_of(4096) {
        checkpoint()?;
    }
    Ok(())
}

// OpenCV 4.13 LineIterator connectivity=8, leftToRight=true. fillPoly draws
// these integer outlines before its even-odd scan conversion.
fn opencv_line_contains<F>(
    mut start: Point<i32>,
    mut end: Point<i32>,
    x: i32,
    y: i32,
    scan_steps: &mut usize,
    checkpoint: &mut F,
) -> Result<bool, ConversionError>
where
    F: FnMut() -> Result<(), ConversionError>,
{
    let mut delta_x = 1;
    let mut delta_y = 1;
    let mut dx = end.x - start.x;
    let mut dy = end.y - start.y;
    if dx < 0 {
        dx = -dx;
        dy = -dy;
        std::mem::swap(&mut start, &mut end);
    }
    if dy < 0 {
        dy = -dy;
        delta_y = -1;
    }
    let vertical = dy > dx;
    if vertical {
        std::mem::swap(&mut dx, &mut dy);
        std::mem::swap(&mut delta_x, &mut delta_y);
    }
    let mut error = dx - 2 * dy;
    let plus_delta = 2 * dx;
    let minus_delta = -2 * dy;
    let mut minus_shift = delta_x;
    let mut plus_shift = 0;
    let mut minus_step = 0;
    let mut plus_step = delta_y;
    if vertical {
        std::mem::swap(&mut plus_step, &mut plus_shift);
        std::mem::swap(&mut minus_step, &mut minus_shift);
    }
    let mut point = start;
    for _ in 0..=dx {
        score_scan_step(scan_steps, checkpoint)?;
        if point.x == x && point.y == y {
            return Ok(true);
        }
        let mask = if error < 0 { -1 } else { 0 };
        error += minus_delta + (plus_delta & mask);
        point.x += minus_shift + (plus_shift & mask);
        point.y += minus_step + (plus_step & mask);
    }
    Ok(false)
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

struct OffsetPath {
    points: Vec<[f64; 2]>,
    _reservation: ResourceReservation,
}

fn unclip(
    polygon: [[f64; 2]; 4],
    ratio: f64,
    max_offset_points: usize,
    context: &ExecutionContext,
) -> Result<Option<OffsetPath>, ConversionError> {
    unclip_with_inflater(polygon, ratio, max_offset_points, context, |paths, distance| {
        inflate_paths_64(paths, distance, JoinType::Round, EndType::Polygon, 2.0, 0.0)
    })
}

fn unclip_with_inflater<F>(
    polygon: [[f64; 2]; 4],
    ratio: f64,
    max_offset_points: usize,
    context: &ExecutionContext,
    inflater: F,
) -> Result<Option<OffsetPath>, ConversionError>
where
    F: FnOnce(&Vec<Vec<Point64>>, f64) -> Vec<Vec<Point64>>,
{
    let (area, perimeter) = polygon_area_perimeter(&polygon);
    if area <= 0.0 || perimeter <= 0.0 {
        return Ok(None);
    }
    let distance = area * ratio / perimeter;
    if !distance.is_finite() {
        return Err(ocr("invalidDetectionGeometry"));
    }
    // clipper2-rust 1.1.0's zero-tolerance round join has a pinned maximum of
    // 104 generated vertices for the one convex four-point input. Its finishing
    // union has at most one input edge per generated point, so the audited
    // sweep bound is quadratic and the path-header bound is linear. Check the
    // caller cap and reserve a deliberately conservative logical heap envelope
    // before constructing Clipper-owned Vecs or entering its non-polling call.
    let reserved_bytes = offset_reservation_bytes(max_offset_points)?;
    let reservation = context.reserve_memory(reserved_bytes)?;
    context.checkpoint()?;
    // pyclipper's Path conversion consumes integer coordinates. Python/NumPy
    // values reaching this point are integral for the quad path; reject rather
    // than silently accepting geometry outside the audited i64 range.
    let mut path = Vec::new();
    path.try_reserve_exact(polygon.len()).map_err(|_| limit("offsetMemory"))?;
    for point in polygon {
        if !point[0].is_finite()
            || !point[1].is_finite()
            || point[0] < i64::MIN as f64
            || point[0] > i64::MAX as f64
            || point[1] < i64::MIN as f64
            || point[1] > i64::MAX as f64
        {
            return Err(ocr("invalidDetectionGeometry"));
        }
        path.push(Point64::new(point[0] as i64, point[1] as i64));
    }
    let mut input = Vec::new();
    input.try_reserve_exact(1).map_err(|_| limit("offsetMemory"))?;
    input.push(path);
    let paths = inflater(&input, distance);
    context.checkpoint()?;
    if paths.len() != 1 || paths[0].len() < 3 {
        return Ok(None);
    }
    if paths[0].len() > OFFSET_MAX_GENERATED_POINTS {
        return Err(ocr("offsetDependencyBoundDrift"));
    }
    let mut result = Vec::new();
    result.try_reserve_exact(paths[0].len()).map_err(|_| limit("offsetMemory"))?;
    result.extend(paths[0].iter().map(|point| [point.x as f64, point.y as f64]));
    drop(paths);
    drop(input);
    Ok(Some(OffsetPath { points: result, _reservation: reservation }))
}

fn offset_reservation_bytes(max_offset_points: usize) -> Result<u64, ConversionError> {
    let work = OFFSET_MAX_GENERATED_POINTS
        .checked_mul(OFFSET_MAX_GENERATED_POINTS)
        .ok_or_else(|| limit("offsetWork"))?;
    if work > OFFSET_MAX_WORK_UNITS {
        return Err(limit("offsetWork"));
    }
    if OFFSET_MAX_GENERATED_POINTS > max_offset_points {
        return Err(limit("offsetPoints"));
    }
    let reserved_bytes = OFFSET_MAX_GENERATED_POINTS
        .checked_mul(OFFSET_BYTES_PER_POINT)
        .and_then(|bytes| {
            OFFSET_MAX_PATH_HEADERS
                .checked_mul(OFFSET_BYTES_PER_HEADER)
                .and_then(|headers| bytes.checked_add(headers))
        })
        .and_then(|bytes| {
            work.checked_mul(OFFSET_BYTES_PER_WORK_UNIT)
                .and_then(|scratch| bytes.checked_add(scratch))
        })
        .ok_or_else(|| limit("offsetMemory"))?;
    u64::try_from(reserved_bytes).map_err(|_| limit("offsetMemory"))
}

fn minimum_rect_contour(
    points: &[Point<i32>],
    context: &ExecutionContext,
    reservation: &mut ResourceReservation,
    events: &mut usize,
    max_events: usize,
) -> Result<[[f64; 2]; 4], ConversionError> {
    let scratch = points
        .len()
        .checked_mul(std::mem::size_of::<Point<i32>>())
        .and_then(|bytes| bytes.checked_mul(4))
        .ok_or_else(|| limit("contourMemory"))?;
    reservation.grow(u64::try_from(scratch).map_err(|_| limit("contourMemory"))?)?;
    let hull = convex_hull(points.to_vec());
    let result = minimum_rect_hull(&hull, context, events, max_events);
    drop(hull);
    reservation.shrink(u64::try_from(scratch).map_err(|_| limit("contourMemory"))?)?;
    result
}

fn minimum_rect_hull(
    hull: &[Point<i32>],
    context: &ExecutionContext,
    events: &mut usize,
    max_events: usize,
) -> Result<[[f64; 2]; 4], ConversionError> {
    if hull.len() < 3 {
        return Err(ocr("invalidDetectionGeometry"));
    }
    let mut best_area = f64::INFINITY;
    let mut best = [[0.0; 2]; 4];
    for edge_index in 0..hull.len() {
        let start = hull[edge_index];
        let end = hull[(edge_index + 1) % hull.len()];
        let dx = f64::from(end.x - start.x);
        let dy = f64::from(end.y - start.y);
        let length = dx.hypot(dy);
        if length == 0.0 {
            continue;
        }
        let (ux, uy) = (dx / length, dy / length);
        let (vx, vy) = (-uy, ux);
        let (mut min_u, mut max_u, mut min_v, mut max_v) =
            (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
        for point in hull {
            *events = events.checked_add(1).ok_or_else(|| limit("contourEvents"))?;
            if *events > max_events {
                return Err(limit("contourEvents"));
            }
            if (*events).is_multiple_of(4096) {
                context.checkpoint()?;
            }
            let projection_u = f64::from(point.x) * ux + f64::from(point.y) * uy;
            let projection_v = f64::from(point.x) * vx + f64::from(point.y) * vy;
            min_u = min_u.min(projection_u);
            max_u = max_u.max(projection_u);
            min_v = min_v.min(projection_v);
            max_v = max_v.max(projection_v);
        }
        let area = (max_u - min_u) * (max_v - min_v);
        if area < best_area {
            best_area = area;
            let to_xy = |u: f64, v: f64| [u * ux + v * vx, u * uy + v * vy];
            best = [
                to_xy(min_u, min_v),
                to_xy(max_u, min_v),
                to_xy(max_u, max_v),
                to_xy(min_u, max_v),
            ];
        }
    }
    best.iter()
        .flatten()
        .all(|value| value.is_finite())
        .then_some(best)
        .ok_or_else(|| ocr("invalidDetectionGeometry"))
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

fn is_convex_quad(quad: [[f64; 2]; 4]) -> bool {
    let mut sign = 0_i8;
    for index in 0..4 {
        let a = quad[index];
        let b = quad[(index + 1) % 4];
        let c = quad[(index + 2) % 4];
        let cross = (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0]);
        if !cross.is_finite() || cross.abs() <= f64::EPSILON {
            return false;
        }
        let current = if cross > 0.0 { 1 } else { -1 };
        if sign != 0 && sign != current {
            return false;
        }
        sign = current;
    }
    true
}

fn model_to_source(point: [f64; 2], image: PixelView<'_>, transform: Transform) -> (f32, f32) {
    let oriented_x = (point[0] * transform.oriented_w as f64 / transform.model_w as f64)
        .round_ties_even()
        .clamp(0.0, (transform.oriented_w - 1) as f64);
    let oriented_y = (point[1] * transform.oriented_h as f64 / transform.model_h as f64)
        .round_ties_even()
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
    regions.sort_by(|a, b| {
        a.polygon[0]
            .1
            .total_cmp(&b.polygon[0].1)
            .then_with(|| a.polygon[0].0.total_cmp(&b.polygon[0].0))
    });
    // Exact insertion-style adjacent swaps from PaddleOCR predict_system.py.
    for index in 0..regions.len().saturating_sub(1) {
        for previous in (0..=index).rev() {
            if (regions[previous + 1].polygon[0].1 - regions[previous].polygon[0].1).abs() < 10.0
                && regions[previous + 1].polygon[0].0 < regions[previous].polygon[0].0
            {
                regions.swap(previous, previous + 1);
            } else {
                break;
            }
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
    use sha2::{Digest, Sha256};
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::time::Duration;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn small_config() -> DetectionConfig {
        DetectionConfig {
            max_source_pixels: 4096,
            max_model_pixels: 736 * 736,
            max_contour_events: 4096,
            max_contour_points: 4096,
            max_score_pixels: 16_384,
            max_score_work: 16_384 * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
            max_offset_points: 512,
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
                max_offset_points: 2,
                ..DetectionConfig::default()
            })
            .is_err()
        );
    }

    #[test]
    fn stride_rounding_matches_paddle_ties_to_even() {
        assert_eq!(round_stride(47).unwrap(), 32);
        assert_eq!(round_stride(48).unwrap(), 64);
        assert_eq!(round_stride(80).unwrap(), 64);
        assert_eq!(round_stride(81).unwrap(), 96);
        assert_eq!(official_resize_dimensions(99, 32).unwrap(), (2272, 736));
        assert_eq!(official_resize_dimensions(99, 2).unwrap(), (4000, 64));
    }

    #[test]
    fn normalize_matches_pinned_paddle_float32_bits_for_every_u8_channel() {
        // SHA-256 over the big-endian f32 bits for pixels 0..=255. These
        // references were generated by the pinned operators.py expression with
        // np.float32 scale/mean/std, independently of this Rust implementation.
        let expected = [
            "6a6724773d325a67638dc6c69d66c5f0cb9af3b6d01721d99b93d35cf3f7d8e8",
            "35a14e12fe826b55cdf4f6615b43350123788600351c2cfbc88ed7d3a89ec394",
            "93a92e5c298e2f3ea78ef9a308d478da376e5c6055e33cc790f724d34b1c1d3f",
        ];
        for (channel, expected) in expected.into_iter().enumerate() {
            let mut digest = Sha256::new();
            for pixel in u8::MIN..=u8::MAX {
                digest.update(normalize(pixel, channel).to_bits().to_be_bytes());
            }
            assert_eq!(format!("{:x}", digest.finalize()), expected, "channel={channel}");
        }
        assert_eq!(normalize(48, 0).to_bits(), 0xbfa5_e091);
        assert_eq!(normalize(48, 1).to_bits(), 0xbf99_0226);
        assert_eq!(normalize(48, 2).to_bits(), 0xbf77_c490);
    }

    #[test]
    fn bilinear_uint8_matches_opencv_4_13_reference_within_one_lsb() {
        let bytes = (0_u8..18).map(|value| value * 10).collect::<Vec<_>>();
        let image = PixelView {
            width: 3,
            height: 2,
            row_stride: 9,
            format: PixelFormat::Bgr8,
            orientation: ImageOrientation::Normal,
            bytes: &bytes,
        };
        let reference = [
            0, 10, 20, 4, 14, 24, 17, 27, 37, 30, 40, 50, 43, 53, 63, 56, 66, 76, 60, 70, 80, 9,
            19, 29, 13, 23, 33, 26, 36, 46, 39, 49, 59, 52, 62, 72, 65, 75, 85, 69, 79, 89, 45, 55,
            65, 49, 59, 69, 62, 72, 82, 75, 85, 95, 88, 98, 108, 101, 111, 121, 105, 115, 125, 81,
            91, 101, 85, 95, 105, 98, 108, 118, 111, 121, 131, 124, 134, 144, 137, 147, 157, 141,
            151, 161, 90, 100, 110, 94, 104, 114, 107, 117, 127, 120, 130, 140, 133, 143, 153, 146,
            156, 166, 150, 160, 170,
        ];
        let mut actual = Vec::new();
        for y in 0..5 {
            for x in 0..7 {
                let sx = ((f64::from(x) + 0.5) * 3.0 / 7.0 - 0.5).clamp(0.0, 2.0);
                let sy = ((f64::from(y) + 0.5) * 2.0 / 5.0 - 0.5).clamp(0.0, 1.0);
                actual.extend(bilinear_u8(image, 3, 2, 3, 2, sx, sy).unwrap());
            }
        }
        assert!(
            actual.iter().zip(reference).all(|(actual, expected)| actual.abs_diff(expected) <= 1),
            "actual={actual:?}"
        );

        let downsample_reference = [53, 63, 73, 98, 108, 118];
        let mut downsample = Vec::new();
        for x in 0..2 {
            let sx = (f64::from(x) + 0.5) * 3.0 / 2.0 - 0.5;
            let sy = 0.5;
            downsample.extend(bilinear_u8(image, 3, 2, 3, 2, sx, sy).unwrap());
        }
        assert!(
            downsample
                .iter()
                .zip(downsample_reference)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1),
            "downsample={downsample:?}"
        );

        let padded_bytes = [10, 20, 30, 40, 50, 60];
        let padded_image = PixelView {
            width: 2,
            height: 1,
            row_stride: 6,
            format: PixelFormat::Bgr8,
            orientation: ImageOrientation::Normal,
            bytes: &padded_bytes,
        };
        let padded_reference = [
            10, 20, 30, 17, 27, 37, 32, 42, 52, 30, 37, 45, 10, 12, 15, 0, 0, 0, 8, 15, 23, 13, 21,
            28, 24, 32, 39, 23, 28, 34, 8, 9, 11, 0, 0, 0, 3, 5, 8, 4, 7, 9, 8, 11, 13, 8, 9, 11,
            3, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut padded_actual = Vec::new();
        for y in 0..4 {
            for x in 0..6 {
                let sx = (f64::from(x) + 0.5) * 0.5 - 0.5;
                let sy = (f64::from(y) + 0.5) * 0.5 - 0.5;
                padded_actual.extend(
                    bilinear_u8(padded_image, 2, 1, 32, 32, sx.max(0.0), sy.max(0.0)).unwrap(),
                );
            }
        }
        assert!(
            padded_actual
                .iter()
                .zip(padded_reference)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1),
            "padded_actual={padded_actual:?}"
        );

        let random_bytes = [
            127, 211, 151, 39, 151, 73, 162, 102, 0, 144, 40, 24, 175, 3, 39, 136, 220, 246, 81,
            210, 47, 117, 171, 125, 176, 69, 179, 5, 45, 82, 46, 184, 126, 23, 156, 161,
        ];
        let random_image = PixelView {
            width: 4,
            height: 3,
            row_stride: 12,
            format: PixelFormat::Bgr8,
            orientation: ImageOrientation::Normal,
            bytes: &random_bytes,
        };
        let random_reference = [
            127, 211, 151, 65, 169, 96, 100, 126, 36, 156, 83, 7, 144, 40, 24, 134, 181, 135, 77,
            167, 109, 102, 139, 52, 147, 100, 16, 140, 59, 38, 154, 92, 87, 112, 161, 146, 105,
            177, 99, 119, 149, 43, 128, 115, 82, 175, 3, 39, 148, 155, 184, 109, 215, 147, 92, 198,
            70, 117, 171, 125, 175, 31, 99, 108, 111, 153, 73, 172, 128, 69, 189, 99, 77, 164, 140,
            176, 59, 159, 69, 67, 121, 37, 129, 110, 47, 179, 127, 36, 158, 156, 176, 69, 179, 56,
            52, 111, 25, 114, 104, 39, 175, 136, 23, 156, 161,
        ];
        let mut random_actual = Vec::new();
        for y in 0..7 {
            for x in 0..5 {
                let sx = ((f64::from(x) + 0.5) * 4.0 / 5.0 - 0.5).clamp(0.0, 3.0);
                let sy = ((f64::from(y) + 0.5) * 3.0 / 7.0 - 0.5).clamp(0.0, 2.0);
                random_actual.extend(bilinear_u8(random_image, 4, 3, 4, 3, sx, sy).unwrap());
            }
        }
        assert!(
            random_actual
                .iter()
                .zip(random_reference)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1),
            "random_actual={random_actual:?}"
        );
    }

    #[test]
    fn fill_poly_integer_masks_match_opencv_4_13_reference() {
        let fixtures = [
            (
                [[1.0, 1.0], [8.0, 3.0], [6.0, 9.0], [2.0, 7.0]],
                &[
                    13, 14, 25, 26, 27, 28, 29, 30, 37, 38, 39, 40, 41, 42, 43, 44, 49, 50, 51, 52,
                    53, 54, 55, 56, 62, 63, 64, 65, 66, 67, 74, 75, 76, 77, 78, 79, 86, 87, 88, 89,
                    90, 91, 100, 101, 102, 114,
                ][..],
            ),
            (
                [[2.0, 2.0], [8.0, 5.0], [2.0, 8.0], [4.0, 5.0]],
                &[
                    26, 27, 39, 40, 41, 51, 52, 53, 54, 55, 64, 65, 66, 67, 68, 75, 76, 77, 78, 79,
                    87, 88, 89, 98, 99,
                ][..],
            ),
        ];
        for (polygon, reference) in fixtures {
            let mut scan_steps = 0;
            let mut no_checkpoint = || Ok(());
            let actual = (0..144)
                .filter(|index| {
                    opencv_fill_poly_contains(
                        index % 12,
                        index / 12,
                        &polygon,
                        &mut scan_steps,
                        &mut no_checkpoint,
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, reference);
        }
    }

    #[test]
    fn retr_list_candidate_order_matches_opencv_4_13() {
        let (width, height) = (60, 30);
        let mut bitmap = vec![0; width * height];
        for (left, top) in [(2, 2), (20, 2), (2, 15)] {
            for y in top..top + 6 {
                for x in left..left + 6 {
                    bitmap[y * width + x] = 1;
                }
            }
        }
        let context = context();
        let fixed = width * height * (1 + std::mem::size_of::<i32>()) + MAX_CANDIDATES * 256;
        let mut reservation = context.reserve_memory(fixed as u64).unwrap();
        let contours = scan_contours(
            &bitmap,
            width,
            height,
            &DetectionConfig {
                max_source_pixels: width * height,
                max_model_pixels: width * height,
                max_contour_events: 16,
                max_contour_points: width * height,
                max_score_pixels: width * height,
                max_score_work: width * height * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
                max_offset_points: 64,
            },
            &context,
            &mut reservation,
        )
        .unwrap();
        let top_left = contours
            .iter()
            .map(|contour| {
                (
                    contour.iter().map(|point| point.x).min().unwrap(),
                    contour.iter().map(|point| point.y).min().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(top_left, [(2, 15), (20, 2), (2, 2)]);

        let mut ring = vec![0; width * height];
        for y in 2..25 {
            for x in 2..25 {
                if !(8..18).contains(&x) || !(8..18).contains(&y) {
                    ring[y * width + x] = 1;
                }
            }
        }
        let fixed = width * height * (1 + std::mem::size_of::<i32>()) + MAX_CANDIDATES * 256;
        let mut ring_reservation = context.reserve_memory(fixed as u64).unwrap();
        let ring_contours = scan_contours(
            &ring,
            width,
            height,
            &DetectionConfig {
                max_source_pixels: width * height,
                max_model_pixels: width * height,
                max_contour_events: 16,
                max_contour_points: width * height,
                max_score_pixels: width * height,
                max_score_work: width * height * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
                max_offset_points: 64,
            },
            &context,
            &mut ring_reservation,
        )
        .unwrap();
        let bounds = ring_contours
            .iter()
            .map(|contour| {
                let min_x = contour.iter().map(|point| point.x).min().unwrap();
                let max_x = contour.iter().map(|point| point.x).max().unwrap();
                let min_y = contour.iter().map(|point| point.y).min().unwrap();
                let max_y = contour.iter().map(|point| point.y).max().unwrap();
                (min_x, min_y, max_x - min_x + 1, max_y - min_y + 1)
            })
            .collect::<Vec<_>>();
        assert_eq!(bounds, [(7, 7, 12, 12), (2, 2, 23, 23)]);
    }

    #[test]
    fn max_candidates_keeps_opencv_reverse_scan_prefix() {
        let (width, height) = (180, 180);
        let mut bitmap = vec![0; width * height];
        let mut discovered = Vec::new();
        for y in (1..height).step_by(3) {
            for x in (1..width).step_by(3) {
                bitmap[y * width + x] = 1;
                discovered.push((i32::try_from(x).unwrap(), i32::try_from(y).unwrap()));
            }
        }
        assert!(discovered.len() > MAX_CANDIDATES);
        let context = context();
        let fixed = width * height * (1 + std::mem::size_of::<i32>()) + MAX_CANDIDATES * 256;
        let mut reservation = context.reserve_memory(fixed as u64).unwrap();
        let contours = scan_contours(
            &bitmap,
            width,
            height,
            &DetectionConfig {
                max_source_pixels: width * height,
                max_model_pixels: width * height,
                max_contour_events: discovered.len() + 1,
                max_contour_points: discovered.len() + 1,
                max_score_pixels: width * height,
                max_score_work: width * height * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
                max_offset_points: 64,
            },
            &context,
            &mut reservation,
        )
        .unwrap();
        assert_eq!(contours.len(), MAX_CANDIDATES);
        assert_eq!(
            contours.iter().take(3).map(|contour| contour[0]).collect::<Vec<_>>(),
            [Point::new(178, 178), Point::new(175, 178), Point::new(172, 178)]
        );
        assert_eq!(
            contours[0][0],
            Point::new(discovered.last().unwrap().0, discovered.last().unwrap().1)
        );
        let cutoff = discovered.len() - MAX_CANDIDATES;
        assert_eq!(
            contours[MAX_CANDIDATES - 1][0],
            Point::new(discovered[cutoff].0, discovered[cutoff].1)
        );
        assert_eq!(contours[MAX_CANDIDATES - 1][0], Point::new(1, 31));
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
        assert_eq!(prepared.tensor.shape, [1, 3, 736, 736]);
        let plane = 736 * 736;
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
        assert_eq!(pixel_bgr(image, 0, 0).unwrap(), [30, 20, 10]);
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
        let reference = [(30.0, 0.0), (107.0, 51.0), (75.0, 94.0), (0.0, 37.0)];
        for (actual, expected) in q.into_iter().zip(reference) {
            assert!((actual.0 - expected.0).abs() <= 1.0, "actual={q:?}");
            assert!((actual.1 - expected.1).abs() <= 1.0, "actual={q:?}");
        }
        assert!(
            (region.confidence - 0.899_893_46).abs() <= 1e-5,
            "confidence={}",
            region.confidence
        );
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
    fn score_scan_observes_cancellation_and_timeout_after_work_begins() {
        let width = 128;
        let values = vec![0.9; width * width];
        let polygon = [[1.0, 1.0], [126.0, 1.0], [126.0, 126.0], [1.0, 126.0]];

        let cancellation = CancellationToken::new();
        let cancelled_context = ExecutionContext::new(
            ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let mut callbacks = 0;
        let mut budget = ScoreBudget {
            pixels: 0,
            max_pixels: width * width,
            work: 0,
            max_work: width * width * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
        };
        let cancelled =
            polygon_score_with_checkpoint(&values, width, width, polygon, &mut budget, || {
                callbacks += 1;
                if callbacks == 2 {
                    cancellation.cancel();
                }
                cancelled_context.checkpoint()
            });
        assert!(matches!(cancelled, Err(ConversionError::Cancelled)));
        assert_eq!(callbacks, 2, "cancellation must occur at an in-scan checkpoint");

        let timed_context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_millis(5)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let mut callbacks = 0;
        let mut budget = ScoreBudget {
            pixels: 0,
            max_pixels: width * width,
            work: 0,
            max_work: width * width * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
        };
        let timed_out =
            polygon_score_with_checkpoint(&values, width, width, polygon, &mut budget, || {
                callbacks += 1;
                if callbacks == 2 {
                    std::thread::sleep(Duration::from_millis(10));
                }
                timed_context.checkpoint()
            });
        assert!(matches!(timed_out, Err(ConversionError::Timeout)));
        assert_eq!(callbacks, 2, "timeout must occur at an in-scan checkpoint");
    }

    #[test]
    fn nested_rings_hit_aggregate_score_limit_before_unbounded_scans() {
        let (width, height) = (128, 128);
        let mut values = vec![0.0; width * height];
        for y in 0..height {
            for x in 0..width {
                let edge = x.min(y).min(width - 1 - x).min(height - 1 - y);
                if (8..56).contains(&edge) && ((edge - 8) / 4).is_multiple_of(2) {
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
                max_contour_events: width * height,
                max_contour_points: width * height,
                max_score_pixels: width * height,
                max_score_work: width * height * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
                max_offset_points: 512,
            },
            &context(),
        );
        assert!(matches!(result, Err(ConversionError::ResourceLimit { limit: "scorePixels", .. })));
    }

    #[test]
    fn score_work_bound_is_checked_before_scan_entry() {
        let width = 64;
        let values = vec![0.9; width * width];
        let polygon = [[1.0, 1.0], [62.0, 1.0], [62.0, 62.0], [1.0, 62.0]];
        let mut budget = ScoreBudget {
            pixels: 0,
            max_pixels: width * width,
            work: 0,
            max_work: SCORE_WORK_PER_PIXEL_UPPER_BOUND - 1,
        };
        let checkpoints = Cell::new(0);
        let result =
            polygon_score_with_checkpoint(&values, width, width, polygon, &mut budget, || {
                checkpoints.set(checkpoints.get() + 1);
                Ok(())
            });
        assert!(matches!(result, Err(ConversionError::ResourceLimit { limit: "scoreWork", .. })));
        assert_eq!(checkpoints.get(), 0, "score scan began before work preflight");
    }

    #[test]
    fn offset_cap_and_memory_are_checked_before_third_party_entry() {
        let polygon = [[10.0, 10.0], [50.0, 10.0], [50.0, 30.0], [10.0, 30.0]];
        let called = Cell::new(false);
        let capped = unclip_with_inflater(polygon, UNCLIP_RATIO, 3, &context(), |_, _| {
            called.set(true);
            Vec::new()
        });
        assert!(matches!(
            capped,
            Err(ConversionError::ResourceLimit { limit: "offsetPoints", .. })
        ));
        assert!(!called.get(), "inflater ran before caller-cap rejection");

        let limited = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 64, ..ResourceLimits::default() },
        );
        let called = Cell::new(false);
        let unreserved = unclip_with_inflater(
            polygon,
            UNCLIP_RATIO,
            OFFSET_MAX_GENERATED_POINTS,
            &limited,
            |_, _| {
                called.set(true);
                Vec::new()
            },
        );
        assert!(matches!(unreserved, Err(ConversionError::ResourceLimit { .. })));
        assert!(!called.get(), "inflater ran before logical-memory reservation");

        let reserved_bytes = offset_reservation_bytes(OFFSET_MAX_GENERATED_POINTS).unwrap();
        let exact = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: reserved_bytes, ..ResourceLimits::default() },
        );
        let first =
            unclip(polygon, UNCLIP_RATIO, OFFSET_MAX_GENERATED_POINTS, &exact).unwrap().unwrap();
        assert!(matches!(
            unclip(polygon, UNCLIP_RATIO, OFFSET_MAX_GENERATED_POINTS, &exact),
            Err(ConversionError::ResourceLimit { .. })
        ));
        drop(first);
        assert!(
            unclip(polygon, UNCLIP_RATIO, OFFSET_MAX_GENERATED_POINTS, &exact).unwrap().is_some()
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
    fn foreground_touching_image_edges_uses_virtual_background() {
        let (width, height) = (32, 32);
        let values = vec![0.9; width * height];
        let bytes = vec![0; width * height];
        let result = postprocess(
            &[Tensor { shape: vec![1, 1, height, width], values }],
            view(&bytes, width, height),
            Transform { oriented_w: width, oriented_h: height, model_w: width, model_h: height },
            &DetectionConfig {
                max_source_pixels: width * height,
                max_model_pixels: width * height,
                max_contour_events: 128,
                max_contour_points: width * height,
                max_score_pixels: width * height,
                max_score_work: width * height * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
                max_offset_points: 128,
            },
            &context(),
        )
        .unwrap();
        assert_eq!(result.regions.len(), 1);
    }

    #[test]
    fn contour_memory_and_event_limits_fail_before_unbounded_geometry() {
        let (width, height) = (32, 32);
        let mut values = vec![0.0; width * height];
        for (x, y) in [(2, 2), (20, 20)] {
            for row in y..y + 4 {
                for column in x..x + 4 {
                    values[row * width + column] = 1.0;
                }
            }
        }
        let bytes = vec![0; width * height];
        let config = DetectionConfig {
            max_source_pixels: width * height,
            max_model_pixels: width * height,
            max_contour_events: 1,
            max_contour_points: width * height,
            max_score_pixels: width * height,
            max_score_work: width * height * SCORE_WORK_PER_PIXEL_UPPER_BOUND,
            max_offset_points: 64,
        };
        assert!(
            postprocess(
                &[Tensor { shape: vec![1, 1, height, width], values: values.clone() }],
                view(&bytes, width, height),
                Transform {
                    oriented_w: width,
                    oriented_h: height,
                    model_w: width,
                    model_h: height
                },
                &config,
                &context(),
            )
            .is_err()
        );

        let limited = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 64, ..ResourceLimits::default() },
        );
        assert!(
            postprocess(
                &[Tensor { shape: vec![1, 1, height, width], values }],
                view(&bytes, width, height),
                Transform {
                    oriented_w: width,
                    oriented_h: height,
                    model_w: width,
                    model_h: height
                },
                &DetectionConfig { max_contour_events: 8, ..config },
                &limited,
            )
            .is_err()
        );
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

        let mut strict_boundary = vec![region(30.0, 0.0), region(0.0, 10.0)];
        sort_reading_order(&mut strict_boundary);
        assert_eq!(strict_boundary[0].polygon[0], (30.0, 0.0));

        let tall = |x: f32, y: f32| DetectedTextRegion {
            polygon: [(x, y), (x + 10.0, y), (x + 10.0, y + 100.0), (x, y + 100.0)],
            angle_degrees: 0.0,
            confidence: 1.0,
            crop: CropDescriptor {
                polygon: [(x, y), (x + 10.0, y), (x + 10.0, y + 100.0), (x, y + 100.0)],
                width: 10,
                height: 100,
            },
        };
        let mut not_same_line = vec![tall(30.0, 0.0), tall(0.0, 40.0)];
        sort_reading_order(&mut not_same_line);
        assert_eq!(not_same_line[0].polygon[0], (30.0, 0.0));
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
        assert_eq!(call.1, [1, 3, 736, 736]);
        assert!((call.2 - (1.0 - MEAN[0]) / STD[0]).abs() < 1e-6);
    }
}
