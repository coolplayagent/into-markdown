use super::{ConversionError, limit, malformed};

pub(super) fn try_vec_capacity<T>(capacity: usize, label: &str) -> Result<Vec<T>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve {label} capacity {capacity}: {error}"))
    })?;
    Ok(output)
}

pub(super) fn try_sized_vec<T: Clone>(
    length: usize,
    value: T,
    label: &str,
) -> Result<Vec<T>, ConversionError> {
    let mut output = try_vec_capacity(length, label)?;
    output.resize(length, value);
    Ok(output)
}

pub(super) fn read_sector(
    bytes: &[u8],
    sector_size: usize,
    id: u32,
) -> Result<&[u8], ConversionError> {
    let index = to_usize(id)?;
    let start = index
        .checked_add(1)
        .and_then(|value| value.checked_mul(sector_size))
        .ok_or_else(|| malformed("cfb/sector", "sector offset overflowed"))?;
    bytes
        .get(start..start + sector_size)
        .ok_or_else(|| malformed("cfb/sector", "sector exceeds source bytes"))
}

pub(super) fn validate_physical(id: u32, count: usize, part: &str) -> Result<(), ConversionError> {
    if to_usize(id)? >= count {
        return Err(malformed(part, "sector identifier is out of bounds"));
    }
    Ok(())
}

pub(super) fn le16(bytes: &[u8], offset: usize, part: &str) -> Result<u16, ConversionError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| malformed(part, "truncated little-endian integer"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

pub(super) fn le32(bytes: &[u8], offset: usize, part: &str) -> Result<u32, ConversionError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| malformed(part, "truncated little-endian integer"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

pub(super) fn le64(bytes: &[u8], offset: usize, part: &str) -> Result<u64, ConversionError> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| malformed(part, "truncated little-endian integer"))?;
    Ok(u64::from_le_bytes(raw.try_into().map_err(|_| malformed(part, "truncated 64-bit integer"))?))
}

pub(super) fn to_usize(value: u32) -> Result<usize, ConversionError> {
    usize::try_from(value).map_err(|_| malformed("cfb", "32-bit index cannot be represented"))
}

pub(super) fn to_usize64(value: u64, part: &str) -> Result<usize, ConversionError> {
    usize::try_from(value).map_err(|_| {
        limit("max_decompressed_bytes", format!("stream {part} is too large for this platform"))
    })
}
