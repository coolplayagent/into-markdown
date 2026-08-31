//! Temporary normalization and provider work coexist for one image at a time.

use super::resource;
use into_markdown_core::ConversionError;

pub(super) fn normalization_working_set(width: u32, height: u32) -> Result<u64, ConversionError> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(32))
        .and_then(|bytes| bytes.checked_add(64 * 1024))
        .ok_or_else(|| resource("max_memory_bytes", "normalization plan overflow"))
}

pub(super) fn recognition_working_set(
    normalization: u64,
    provider: u64,
) -> Result<u64, ConversionError> {
    normalization.checked_add(provider).ok_or_else(|| {
        resource("max_memory_bytes", "normalization and OCR working-set plan overflow")
    })
}
