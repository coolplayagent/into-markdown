//! Checked raw-source crop planning and BGR tensor preprocessing.

use super::budget::to_u64;
use super::pixels::{cubic_bgr, perspective_source};
use super::{BASE_WIDTH, HEIGHT, MAX_WIDTH, RecognitionConfig, SCALE, limit, ocr};
use crate::{CropDescriptor, PixelView};
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
        || expected_width.to_bits() != f64::from(crop.width).to_bits()
        || expected_height.to_bits() != f64::from(crop.height).to_bits()
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
    let mut reservation = context.reserve_memory(to_u64(bytes)?)?;
    let mut values = Vec::new();
    values.try_reserve_exact(elements).map_err(|_| limit("recognitionMemory"))?;
    if values.capacity() > elements {
        reservation.grow(to_u64(
            (values.capacity() - elements)
                .checked_mul(std::mem::size_of::<f32>())
                .ok_or_else(|| limit("recognitionMemory"))?,
        )?)?;
    }
    values.resize(elements, 0.0);
    for (batch_index, (_, crop)) in batch.iter().enumerate() {
        context.checkpoint()?;
        let resized_width = ((crop.ratio * HEIGHT as f64).ceil() as usize).clamp(1, width);
        write_crop_tensor(image, *crop, batch_index, width, resized_width, &mut values, context)?;
    }
    let mut shape = Vec::new();
    shape.try_reserve_exact(4).map_err(|_| limit("recognitionMemory"))?;
    reservation.grow(to_u64(
        shape
            .capacity()
            .checked_mul(std::mem::size_of::<usize>())
            .ok_or_else(|| limit("recognitionMemory"))?,
    )?)?;
    shape.extend_from_slice(&[batch.len(), 3, HEIGHT, width]);
    Ok(PreparedBatch { tensor: Tensor { shape, values }, _reservation: reservation })
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
    let warped = warp_crop(image, crop, context)?;
    let (rotated_width, rotated_height) =
        if crop.rotate { (crop.height, crop.width) } else { (crop.width, crop.height) };
    for y in 0..HEIGHT {
        if y % 8 == 0 {
            context.checkpoint()?;
        }
        for x in 0..resized_width {
            let rotated_x = ((x as f64 + 0.5) * rotated_width as f64 / resized_width as f64 - 0.5)
                .clamp(0.0, (rotated_width - 1) as f64);
            let rotated_y = ((y as f64 + 0.5) * rotated_height as f64 / HEIGHT as f64 - 0.5)
                .clamp(0.0, (rotated_height - 1) as f64);
            let (crop_x, crop_y) = rotated_to_crop(crop, rotated_x, rotated_y);
            let pixel = bilinear_crop_bgr(&warped.bytes, crop.width, crop.height, crop_x, crop_y)?;
            for (channel, byte) in pixel.into_iter().enumerate() {
                let offset = (((batch * 3 + channel) * HEIGHT + y) * tensor_width) + x;
                let scaled = f32::from(byte) * SCALE;
                values[offset] = (scaled - 0.5_f32) / 0.5_f32;
            }
        }
    }
    Ok(())
}

pub(super) fn rotated_to_crop(crop: CropPlan, rotated_x: f64, rotated_y: f64) -> (f64, f64) {
    if crop.rotate {
        ((crop.width - 1) as f64 - rotated_y, rotated_x)
    } else {
        (rotated_x, rotated_y)
    }
}

struct WarpedCrop {
    bytes: Vec<u8>,
    _reservation: ResourceReservation,
}

fn warp_crop(
    image: PixelView<'_>,
    crop: CropPlan,
    context: &ExecutionContext,
) -> Result<WarpedCrop, ConversionError> {
    let bytes = crop
        .width
        .checked_mul(crop.height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| limit("recognitionCropMemory"))?;
    let mut reservation = context.reserve_memory(to_u64(bytes)?)?;
    let mut output = Vec::new();
    output.try_reserve_exact(bytes).map_err(|_| limit("recognitionCropMemory"))?;
    if output.capacity() > bytes {
        reservation.grow(to_u64(output.capacity() - bytes)?)?;
    }
    output.resize(bytes, 0);
    for y in 0..crop.height {
        if y % 16 == 0 {
            context.checkpoint()?;
        }
        for x in 0..crop.width {
            let (source_x, source_y) = perspective_source(crop, x as f64, y as f64)?;
            let pixel = cubic_bgr(image, source_x, source_y)?;
            let offset = (y * crop.width + x) * 3;
            output[offset..offset + 3].copy_from_slice(&pixel);
        }
    }
    Ok(WarpedCrop { bytes: output, _reservation: reservation })
}

fn bilinear_crop_bgr(
    bytes: &[u8],
    width: usize,
    height: usize,
    x: f64,
    y: f64,
) -> Result<[u8; 3], ConversionError> {
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let pixel = |sample_x: usize, sample_y: usize| -> Result<&[u8], ConversionError> {
        let offset = sample_y
            .checked_mul(width)
            .and_then(|value| value.checked_add(sample_x))
            .and_then(|value| value.checked_mul(3))
            .ok_or_else(|| ocr("invalidRecognitionCrop"))?;
        bytes.get(offset..offset + 3).ok_or_else(|| ocr("invalidRecognitionCrop"))
    };
    let samples = [pixel(x0, y0)?, pixel(x1, y0)?, pixel(x0, y1)?, pixel(x1, y1)?];
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
