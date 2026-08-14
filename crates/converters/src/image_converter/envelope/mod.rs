//! Complete encoded-file envelope validation before codec entry.

mod bmp;
mod jpeg;
pub(super) mod meter;
mod png;
mod tiff;
mod webp;

use super::format::RasterFormat;
use into_markdown_core::{ConversionError, ExecutionContext, ResourceLimits};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) frames: u32,
    pub(super) animated: bool,
}

pub(super) fn validate(
    format: RasterFormat,
    bytes: &[u8],
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<Summary, ConversionError> {
    context.checkpoint()?;
    let summary = match format {
        RasterFormat::Png => png::validate(bytes, limits, context)?,
        RasterFormat::Jpeg => jpeg::validate(bytes, limits, context)?,
        RasterFormat::Bmp => bmp::validate(bytes, context)?,
        RasterFormat::WebP => webp::validate(bytes, limits, context)?,
        RasterFormat::Tiff => tiff::validate(bytes, limits, context)?,
    };
    if summary.frames == 0 || summary.frames > limits.max_pages {
        return Err(limit(
            "max_pages",
            format!("{} image frame(s) > {}", summary.frames, limits.max_pages),
        ));
    }
    Ok(summary)
}

pub(super) fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("image".into()), detail: detail.into() }
}

pub(super) fn unsupported(detail: impl Into<String>) -> ConversionError {
    ConversionError::Unsupported { detail: detail.into() }
}

pub(super) fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}

fn read_u16(bytes: &[u8], offset: usize, little: bool) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(if little { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) })
}

fn read_u32(bytes: &[u8], offset: usize, little: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(if little { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) })
}

fn read_u64(bytes: &[u8], offset: usize, little: bool) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(if little { u64::from_le_bytes(raw) } else { u64::from_be_bytes(raw) })
}
