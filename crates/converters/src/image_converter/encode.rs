//! Normalized PNG materialization for oriented pages and inference inputs.

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceLimits, ResourceReservation};

pub(crate) struct EncodedImage {
    pub(super) bytes: Vec<u8>,
    memory: ResourceReservation,
}

impl EncodedImage {
    pub(crate) fn into_parts(self) -> (Vec<u8>, ResourceReservation) {
        (self.bytes, self.memory)
    }
}

pub(crate) fn png(
    pixels: &RgbaImage,
    composite_white: bool,
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<EncodedImage, ConversionError> {
    context.checkpoint()?;
    let raw = u64::from(pixels.width())
        .checked_mul(u64::from(pixels.height()))
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| resource("max_memory_bytes", "normalized image size overflow"))?;
    let capacity = raw
        .checked_mul(2)
        .and_then(|value| value.checked_add(64 * 1024))
        .ok_or_else(|| resource("max_memory_bytes", "PNG output plan overflow"))?;
    if capacity > limits.max_asset_bytes {
        return Err(resource(
            "max_asset_bytes",
            format!("normalized PNG plan {capacity} exceeds the configured asset budget"),
        ));
    }
    let working = capacity
        .checked_add(if composite_white { raw } else { 0 })
        .ok_or_else(|| resource("max_memory_bytes", "PNG working-memory plan overflow"))?;
    if working > limits.max_memory_bytes {
        return Err(resource(
            "max_memory_bytes",
            format!("PNG working-memory plan {working} exceeds max_memory_bytes"),
        ));
    }
    let mut memory = context.reserve_memory(working)?;
    let capacity_usize = usize::try_from(capacity)
        .map_err(|_| resource("max_memory_bytes", "PNG output plan is not representable"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity_usize)
        .map_err(|_| resource("max_memory_bytes", "PNG output allocation failed"))?;
    if composite_white {
        let mut opaque = Vec::new();
        opaque
            .try_reserve_exact(
                usize::try_from(raw)
                    .map_err(|_| resource("max_memory_bytes", "opaque pixel plan overflow"))?,
            )
            .map_err(|_| resource("max_memory_bytes", "opaque pixel allocation failed"))?;
        for (index, pixel) in pixels.pixels().enumerate() {
            if index % 1024 == 0 {
                context.checkpoint()?;
            }
            let [r, g, b, alpha] = pixel.0;
            let alpha = u16::from(alpha);
            opaque.push(blend_white(r, alpha));
            opaque.push(blend_white(g, alpha));
            opaque.push(blend_white(b, alpha));
            opaque.push(255);
        }
        encode(&mut bytes, &opaque, pixels.width(), pixels.height())?;
        drop(opaque);
        memory.shrink(raw)?;
    } else {
        encode(&mut bytes, pixels.as_raw(), pixels.width(), pixels.height())?;
    }
    if bytes.len() as u64 > limits.max_asset_bytes {
        return Err(resource(
            "max_asset_bytes",
            format!("normalized PNG {} exceeds max_asset_bytes", bytes.len()),
        ));
    }
    Ok(EncodedImage { bytes, memory })
}

fn blend_white(channel: u8, alpha: u16) -> u8 {
    let blended = (u16::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255;
    u8::try_from(blended).unwrap_or(u8::MAX)
}

fn encode(
    bytes: &mut Vec<u8>,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<(), ConversionError> {
    PngEncoder::new_with_quality(bytes, CompressionType::Fast, FilterType::Adaptive)
        .write_image(pixels, width, height, ExtendedColorType::Rgba8)
        .map_err(|error| ConversionError::Internal {
            detail: format!("normalized PNG encoding failed: {error}"),
        })
}

fn resource(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}
