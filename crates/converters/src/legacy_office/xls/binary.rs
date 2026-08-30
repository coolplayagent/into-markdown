use super::{ConversionError, malformed};

pub(super) fn read_u16(bytes: &[u8], offset: usize, part: &str) -> Result<u16, ConversionError> {
    let raw = bytes.get(offset..offset + 2).ok_or_else(|| malformed(part, "truncated BIFF u16"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

pub(super) fn read_u32(bytes: &[u8], offset: usize, part: &str) -> Result<u32, ConversionError> {
    let raw = bytes.get(offset..offset + 4).ok_or_else(|| malformed(part, "truncated BIFF u32"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}
