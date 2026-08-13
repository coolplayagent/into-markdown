//! Checked raw-source crop planning and BGR tensor preprocessing.

use super::{BASE_WIDTH, HEIGHT, MAX_WIDTH, RecognitionConfig, SCALE, limit, ocr, to_u64};
use crate::{CropDescriptor, PixelFormat, PixelView};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation, Tensor};

#[derive(Clone, Copy)]
pub(super) struct CropPlan {
    pub polygon: [(f32, f32); 4],
    pub width: usize,
    pub height: usize,
    pub rotate: bool,
    pub ratio: f64,
}

pub(super) fn validated_crop(
    crop: &CropDescriptor,
    image: PixelView<'_>,
    config: &RecognitionConfig,
) -> Result<CropPlan, ConversionError> {
    if crop.width == 0 || crop.height == 0 {
        return Err(ocr("invalidRecognitionCrop"));
    }
    let width = crop.width as usize;
    let height = crop.height as usize;
    let distance = |left: (f32, f32), right: (f32, f32)| {
        let dx = f64::from(right.0) - f64::from(left.0);
        let dy = f64::from(right.1) - f64::from(left.1);
        dx.hypot(dy)
    };
    let expected_width = distance(crop.polygon[0], crop.polygon[1])
        .max(distance(crop.polygon[3], crop.polygon[2]))
        .ceil()
        .max(1.0);
    let expected_height = distance(crop.polygon[0], crop.polygon[3])
        .max(distance(crop.polygon[1], crop.polygon[2]))
        .ceil()
        .max(1.0);
    if !expected_width.is_finite()
        || !expected_height.is_finite()
        || expected_width != f64::from(crop.width)
        || expected_height != f64::from(crop.height)
    {
        return Err(ocr("recognitionCropAxisMismatch"));
    }
    if width.checked_mul(height).is_none_or(|pixels| pixels > config.max_crop_pixels) {
        return Err(limit("recognitionCropPixels"));
    }
    for &(x, y) in &crop.polygon {
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x > (image.width - 1) as f32
            || y > (image.height - 1) as f32
        {
            return Err(ocr("invalidRecognitionCrop"));
        }
    }
    let area = crop.polygon.iter().enumerate().fold(0.0_f64, |sum, (index, &(x, y))| {
        let (nx, ny) = crop.polygon[(index + 1) % 4];
        sum + f64::from(x) * f64::from(ny) - f64::from(nx) * f64::from(y)
    });
    if !area.is_finite() || area.abs() <= 1.0 {
        return Err(ocr("invalidRecognitionCrop"));
    }
    let rotate = height as f64 / width as f64 >= 1.5;
    let ratio = if rotate { height as f64 / width as f64 } else { width as f64 / height as f64 };
    if !ratio.is_finite() || ratio * HEIGHT as f64 > MAX_WIDTH as f64 {
        return Err(limit("recognitionWidth"));
    }
    Ok(CropPlan { polygon: crop.polygon, width, height, rotate, ratio })
}

pub(super) struct PreparedBatch {
    pub tensor: Tensor,
    _reservation: ResourceReservation,
}

pub(super) fn prepare_batch(
    image: PixelView<'_>,
    batch: &[(usize, CropPlan)],
    config: &RecognitionConfig,
    context: &ExecutionContext,
) -> Result<PreparedBatch, ConversionError> {
    let width = batch
        .iter()
        .map(|(_, crop)| (crop.ratio * HEIGHT as f64).ceil() as usize)
        .max()
        .unwrap_or(BASE_WIDTH)
        .max(BASE_WIDTH);
    if width > MAX_WIDTH {
        return Err(limit("recognitionWidth"));
    }
    let elements = batch
        .len()
        .checked_mul(3)
        .and_then(|value| value.checked_mul(HEIGHT))
        .and_then(|value| value.checked_mul(width))
        .ok_or_else(|| limit("recognitionTensorElements"))?;
    if elements > config.max_tensor_elements {
        return Err(limit("recognitionTensorElements"));
    }
    let bytes = elements
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| limit("recognitionMemory"))?;
    let reservation = context.reserve_memory(to_u64(bytes)?)?;
    let mut values = Vec::new();
    values.try_reserve_exact(elements).map_err(|_| limit("recognitionMemory"))?;
    values.resize(elements, 0.0);
    for (batch_index, (_, crop)) in batch.iter().enumerate() {
        context.checkpoint()?;
        let resized_width = ((crop.ratio * HEIGHT as f64).ceil() as usize).clamp(1, width);
        write_crop_tensor(image, *crop, batch_index, width, resized_width, &mut values, context)?;
    }
    Ok(PreparedBatch {
        tensor: Tensor { shape: vec![batch.len(), 3, HEIGHT, width], values },
        _reservation: reservation,
    })
}

fn write_crop_tensor(
    image: PixelView<'_>,
    crop: CropPlan,
    batch: usize,
    tensor_width: usize,
    resized_width: usize,
    values: &mut [f32],
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for y in 0..HEIGHT {
        if y % 8 == 0 {
            context.checkpoint()?;
        }
        for x in 0..resized_width {
            let (crop_x, crop_y) = if crop.rotate {
                (
                    (y as f64 + 0.5) * crop.width as f64 / HEIGHT as f64 - 0.5,
                    (crop.height as f64 - 1.0)
                        - ((x as f64 + 0.5) * crop.height as f64 / resized_width as f64 - 0.5),
                )
            } else {
                (
                    (x as f64 + 0.5) * crop.width as f64 / resized_width as f64 - 0.5,
                    (y as f64 + 0.5) * crop.height as f64 / HEIGHT as f64 - 0.5,
                )
            };
            let source = perspective_source(crop, crop_x, crop_y)?;
            let pixel = bilinear_bgr(image, source.0, source.1)?;
            for (channel, byte) in pixel.into_iter().enumerate() {
                let offset = (((batch * 3 + channel) * HEIGHT + y) * tensor_width) + x;
                let scaled = f32::from(byte) * SCALE;
                values[offset] = (scaled - 0.5_f32) / 0.5_f32;
            }
        }
    }
    Ok(())
}

fn perspective_source(crop: CropPlan, x: f64, y: f64) -> Result<(f64, f64), ConversionError> {
    let u = if crop.width <= 1 { 0.0 } else { x / (crop.width - 1) as f64 };
    let v = if crop.height <= 1 { 0.0 } else { y / (crop.height - 1) as f64 };
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

pub(super) fn bilinear_bgr(
    image: PixelView<'_>,
    x: f64,
    y: f64,
) -> Result<[u8; 3], ConversionError> {
    let x = x.clamp(0.0, (image.width - 1) as f64);
    let y = y.clamp(0.0, (image.height - 1) as f64);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(image.width - 1);
    let y1 = (y0 + 1).min(image.height - 1);
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let samples = [
        raw_bgr(image, x0, y0)?,
        raw_bgr(image, x1, y0)?,
        raw_bgr(image, x0, y1)?,
        raw_bgr(image, x1, y1)?,
    ];
    let mut output = [0_u8; 3];
    for channel in 0..3 {
        let top = f64::from(samples[0][channel]) * (1.0 - fx) + f64::from(samples[1][channel]) * fx;
        let bottom =
            f64::from(samples[2][channel]) * (1.0 - fx) + f64::from(samples[3][channel]) * fx;
        output[channel] =
            (top * (1.0 - fy) + bottom * fy).round_ties_even().clamp(0.0, 255.0) as u8;
    }
    Ok(output)
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
