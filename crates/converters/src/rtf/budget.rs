//! Checked allocation accounting and stable RTF errors.

use into_markdown_core::{ConversionError, ResourceReservation, SourceLocator};
use std::mem::size_of;

pub(super) fn parameter_i32(parameter: Option<i64>, name: &str) -> Result<i32, ConversionError> {
    i32::try_from(parameter.ok_or_else(|| malformed(format!("{name} requires a parameter")))?)
        .map_err(|_| limit("rtf_numeric_value", format!("{name} is outside signed 32-bit range")))
}

pub(super) fn parameter_u16(parameter: Option<i64>, name: &str) -> Result<u16, ConversionError> {
    u16::try_from(parameter.ok_or_else(|| malformed(format!("{name} requires a parameter")))?)
        .map_err(|_| limit("rtf_numeric_value", format!("{name} is outside unsigned 16-bit range")))
}

pub(super) fn reserve_vec<T>(
    value: &mut Vec<T>,
    additional: usize,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    if additional <= value.capacity().saturating_sub(value.len()) {
        return Ok(());
    }
    let old = value.capacity();
    let requested = additional.saturating_sub(value.capacity().saturating_sub(value.len()));
    let bytes = u64::try_from(
        requested
            .checked_mul(size_of::<T>())
            .ok_or_else(|| limit("max_memory_bytes", "vector capacity overflow"))?,
    )
    .map_err(|_| limit("max_memory_bytes", "vector capacity cannot be represented"))?;
    memory.grow(bytes)?;
    value
        .try_reserve_exact(additional)
        .map_err(|error| limit("max_memory_bytes", format!("vector allocation failed: {error}")))?;
    let actual = value.capacity().saturating_sub(old);
    if actual > requested {
        memory.grow(
            u64::try_from((actual - requested).saturating_mul(size_of::<T>())).unwrap_or(u64::MAX),
        )?;
    }
    Ok(())
}

pub(super) fn reserve_string(
    value: &mut String,
    additional: usize,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    if additional <= value.capacity().saturating_sub(value.len()) {
        return Ok(());
    }
    let old = value.capacity();
    let requested = additional.saturating_sub(value.capacity().saturating_sub(value.len()));
    memory.grow(u64::try_from(requested).unwrap_or(u64::MAX))?;
    value
        .try_reserve_exact(additional)
        .map_err(|error| limit("max_memory_bytes", format!("string allocation failed: {error}")))?;
    let actual = value.capacity().saturating_sub(old);
    if actual > requested {
        memory.grow(u64::try_from(actual - requested).unwrap_or(u64::MAX))?;
    }
    Ok(())
}

pub(super) fn locator(start: usize, end: usize) -> SourceLocator {
    SourceLocator {
        byte_start: u64::try_from(start).ok(),
        byte_end: u64::try_from(end).ok(),
        ..SourceLocator::default()
    }
}

pub(super) fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(super) fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("rtf".into()), detail: detail.into() }
}

pub(super) fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}
