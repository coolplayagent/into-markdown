//! Pixel-layout validation and interpolation primitives for OCR crops.

use super::ocr;
use super::preprocess::CropPlan;
use crate::{PixelFormat, PixelView};
use into_markdown_core::ConversionError;

#[allow(clippy::many_single_char_names)] // Standard homography coefficients.
pub(super) fn perspective_source(
    crop: CropPlan,
    x: f64,
    y: f64,
) -> Result<(f64, f64), ConversionError> {
    // The upstream destination points use `width` and `height`, while the
    // bounded warp output contains the half-open ranges below those points.
    let u = x / crop.width as f64;
    let v = y / crop.height as f64;
    let [p0, p1, p2, p3] = crop.polygon;
    let (x0, y0) = (f64::from(p0.0), f64::from(p0.1));
    let (x1, y1) = (f64::from(p1.0), f64::from(p1.1));
    let (x2, y2) = (f64::from(p2.0), f64::from(p2.1));
    let (x3, y3) = (f64::from(p3.0), f64::from(p3.1));
    let dx1 = x1 - x2;
    let dx2 = x3 - x2;
    let dx3 = x0 - x1 + x2 - x3;
    let dy1 = y1 - y2;
    let dy2 = y3 - y2;
    let dy3 = y0 - y1 + y2 - y3;
    let (g, h) = if dx3.abs() <= f64::EPSILON && dy3.abs() <= f64::EPSILON {
        (0.0, 0.0)
    } else {
        let denominator = dx1 * dy2 - dx2 * dy1;
        if !denominator.is_finite() || denominator.abs() <= f64::EPSILON {
            return Err(ocr("invalidRecognitionCrop"));
        }
        ((dx3 * dy2 - dx2 * dy3) / denominator, (dx1 * dy3 - dx3 * dy1) / denominator)
    };
    let a = x1 - x0 + g * x1;
    let b = x3 - x0 + h * x3;
    let d = y1 - y0 + g * y1;
    let e = y3 - y0 + h * y3;
    let denominator = g * u + h * v + 1.0;
    if !denominator.is_finite() || denominator.abs() <= f64::EPSILON {
        return Err(ocr("invalidRecognitionCrop"));
    }
    let source = ((a * u + b * v + x0) / denominator, (d * u + e * v + y0) / denominator);
    if !source.0.is_finite()
        || !source.1.is_finite()
        || source.0 < -1.0
        || source.1 < -1.0
        || source.0 > image_coordinate_guard(crop.polygon, true)
        || source.1 > image_coordinate_guard(crop.polygon, false)
    {
        return Err(ocr("invalidRecognitionCrop"));
    }
    Ok(source)
}

fn image_coordinate_guard(polygon: [(f32, f32); 4], x_axis: bool) -> f64 {
    polygon
        .iter()
        .map(|point| if x_axis { f64::from(point.0) } else { f64::from(point.1) })
        .fold(0.0, f64::max)
        + 1.0
}

pub(super) fn cubic_bgr(image: PixelView<'_>, x: f64, y: f64) -> Result<[u8; 3], ConversionError> {
    let base_x = x.floor() as isize;
    let base_y = y.floor() as isize;
    let maximum_x = isize::try_from(image.width - 1).map_err(|_| ocr("invalidRecognitionCrop"))?;
    let maximum_y = isize::try_from(image.height - 1).map_err(|_| ocr("invalidRecognitionCrop"))?;
    let mut sums = [0.0_f64; 3];
    for offset_y in -1_isize..=2 {
        let sample_y = (base_y + offset_y).clamp(0, maximum_y) as usize;
        let weight_y = cubic_weight(y - (base_y + offset_y) as f64);
        for offset_x in -1_isize..=2 {
            let sample_x = (base_x + offset_x).clamp(0, maximum_x) as usize;
            let weight = weight_y * cubic_weight(x - (base_x + offset_x) as f64);
            let pixel = raw_bgr(image, sample_x, sample_y)?;
            for channel in 0..3 {
                sums[channel] += f64::from(pixel[channel]) * weight;
            }
        }
    }
    Ok(sums.map(|value| value.round_ties_even().clamp(0.0, 255.0) as u8))
}

fn cubic_weight(value: f64) -> f64 {
    let value = value.abs();
    if value <= 1.0 {
        1.25 * value * value * value - 2.25 * value * value + 1.0
    } else if value < 2.0 {
        -0.75 * value * value * value + 3.75 * value * value - 6.0 * value + 3.0
    } else {
        0.0
    }
}

pub(super) fn raw_bgr(
    image: PixelView<'_>,
    x: usize,
    y: usize,
) -> Result<[u8; 3], ConversionError> {
    let channels = match image.format {
        PixelFormat::Gray8 => 1,
        PixelFormat::Rgb8 | PixelFormat::Bgr8 => 3,
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
    };
    let offset = y
        .checked_mul(image.row_stride)
        .and_then(|value| value.checked_add(x.checked_mul(channels)?))
        .ok_or_else(|| ocr("invalidPixelStride"))?;
    let pixel =
        image.bytes.get(offset..offset + channels).ok_or_else(|| ocr("truncatedPixelBuffer"))?;
    Ok(match image.format {
        PixelFormat::Gray8 => [pixel[0]; 3],
        PixelFormat::Rgb8 | PixelFormat::Rgba8 => [pixel[2], pixel[1], pixel[0]],
        PixelFormat::Bgr8 | PixelFormat::Bgra8 => [pixel[0], pixel[1], pixel[2]],
    })
}

pub(super) fn validate_pixels(image: PixelView<'_>) -> Result<(), ConversionError> {
    if image.width == 0 || image.height == 0 {
        return Err(ocr("emptyImage"));
    }
    let channels = match image.format {
        PixelFormat::Gray8 => 1,
        PixelFormat::Rgb8 | PixelFormat::Bgr8 => 3,
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
    };
    let row = image.width.checked_mul(channels).ok_or_else(|| ocr("invalidPixelStride"))?;
    if image.row_stride < row {
        return Err(ocr("invalidPixelStride"));
    }
    let required = image
        .height
        .checked_sub(1)
        .and_then(|rows| rows.checked_mul(image.row_stride))
        .and_then(|bytes| bytes.checked_add(row))
        .ok_or_else(|| ocr("truncatedPixelBuffer"))?;
    if image.bytes.len() < required {
        return Err(ocr("truncatedPixelBuffer"));
    }
    Ok(())
}
