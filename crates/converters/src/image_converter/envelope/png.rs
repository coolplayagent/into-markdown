use super::meter::Meter;
use super::{Summary, limit, malformed, unsupported};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceLimits};

const MAX_CHUNKS: usize = 100_000;

pub(super) fn validate(
    bytes: &[u8],
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<Summary, ConversionError> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(malformed("PNG signature is invalid"));
    }
    let mut meter = Meter::new(context);
    meter.consume(8)?;
    let mut offset = 8_usize;
    let mut chunks = 0_usize;
    let mut saw_ihdr = false;
    let mut color_type = None;
    let mut saw_plte = false;
    let mut saw_idat = false;
    let mut ended_idat = false;
    loop {
        chunks = chunks
            .checked_add(1)
            .ok_or_else(|| limit("image_chunks", "PNG chunk count overflowed"))?;
        if chunks > MAX_CHUNKS || chunks > limits.max_archive_entries as usize {
            return Err(limit("image_chunks", "PNG has too many chunks"));
        }
        let header = bytes
            .get(offset..offset.saturating_add(8))
            .ok_or_else(|| malformed("PNG chunk header is truncated"))?;
        let length = usize::try_from(u32::from_be_bytes(
            header[..4].try_into().map_err(|_| malformed("PNG chunk length is truncated"))?,
        ))
        .map_err(|_| limit("max_input_bytes", "PNG chunk length is unrepresentable"))?;
        let kind: &[u8; 4] =
            header[4..8].try_into().map_err(|_| malformed("PNG chunk type is truncated"))?;
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(malformed("PNG chunk type is invalid"));
        }
        if kind[0].is_ascii_uppercase() && !matches!(kind, b"IHDR" | b"PLTE" | b"IDAT" | b"IEND") {
            return Err(unsupported(format!(
                "unknown critical PNG chunk {}",
                String::from_utf8_lossy(kind)
            )));
        }
        if matches!(kind, b"acTL" | b"fcTL" | b"fdAT") {
            return Err(unsupported("animated PNG is outside the supported image-frame policy"));
        }
        let data_start = offset
            .checked_add(8)
            .ok_or_else(|| limit("max_input_bytes", "PNG offset overflowed"))?;
        let data_end = data_start
            .checked_add(length)
            .ok_or_else(|| limit("max_input_bytes", "PNG chunk length overflowed"))?;
        let end = data_end
            .checked_add(4)
            .ok_or_else(|| limit("max_input_bytes", "PNG CRC offset overflowed"))?;
        let data = bytes
            .get(data_start..data_end)
            .ok_or_else(|| malformed("PNG chunk data is truncated"))?;
        let stored =
            bytes.get(data_end..end).ok_or_else(|| malformed("PNG chunk CRC is truncated"))?;
        let stored = u32::from_be_bytes(
            stored.try_into().map_err(|_| malformed("PNG chunk CRC is truncated"))?,
        );
        meter.consume(4)?;
        if crc(kind, data, &mut meter)? != stored {
            return Err(malformed("PNG chunk CRC mismatch"));
        }
        meter.consume(4)?;
        match kind {
            b"IHDR" => {
                if saw_ihdr || chunks != 1 || length != 13 {
                    return Err(malformed("PNG requires one 13-byte IHDR as its first chunk"));
                }
                color_type = Some(validate_ihdr(data)?);
                saw_ihdr = true;
            }
            b"PLTE" => {
                validate_palette(saw_ihdr, saw_plte, saw_idat, length, color_type)?;
                saw_plte = true;
            }
            b"IDAT" => {
                if !saw_ihdr || ended_idat || color_type == Some(3) && !saw_plte {
                    return Err(malformed("PNG IDAT ordering is invalid"));
                }
                saw_idat = true;
            }
            b"IEND" => {
                if length != 0 || end != bytes.len() || !saw_ihdr || !saw_idat {
                    return Err(malformed(
                        "PNG IEND must be unique, follow image data, and end at EOF",
                    ));
                }
                return Ok(Summary { frames: 1, animated: false });
            }
            _ => {
                if saw_idat {
                    ended_idat = true;
                }
            }
        }
        offset = end;
    }
}

fn validate_palette(
    saw_ihdr: bool,
    saw_plte: bool,
    saw_idat: bool,
    length: usize,
    color_type: Option<u8>,
) -> Result<(), ConversionError> {
    if !saw_ihdr
        || saw_plte
        || saw_idat
        || length == 0
        || length > 768
        || !length.is_multiple_of(3)
        || matches!(color_type, Some(0 | 4))
    {
        return Err(malformed("PNG palette ordering or length is invalid"));
    }
    Ok(())
}

fn validate_ihdr(data: &[u8]) -> Result<u8, ConversionError> {
    let width =
        u32::from_be_bytes(data[..4].try_into().map_err(|_| malformed("PNG width is truncated"))?);
    let height = u32::from_be_bytes(
        data[4..8].try_into().map_err(|_| malformed("PNG height is truncated"))?,
    );
    if width == 0 || height == 0 {
        return Err(malformed("PNG dimensions must be non-zero"));
    }
    let bit_depth = data[8];
    let color = data[9];
    if !matches!(
        (color, bit_depth),
        (0, 1 | 2 | 4 | 8 | 16) | (2 | 4 | 6, 8 | 16) | (3, 1 | 2 | 4 | 8)
    ) || data[10] != 0
        || data[11] != 0
        || data[12] > 1
    {
        return Err(malformed("PNG IHDR encoding fields are invalid"));
    }
    Ok(color)
}

fn crc(kind: &[u8], data: &[u8], meter: &mut Meter<'_>) -> Result<u32, ConversionError> {
    let mut value = u32::MAX;
    for source in [kind, data] {
        let mut remaining = source;
        while !remaining.is_empty() {
            let length = remaining.len().min(meter.next_batch());
            let (batch, rest) = remaining.split_at(length);
            for byte in batch {
                value ^= u32::from(*byte);
                for _ in 0..8 {
                    value = if value & 1 == 0 { value >> 1 } else { (value >> 1) ^ 0xedb8_8320 };
                }
            }
            meter.consume(batch.len())?;
            remaining = rest;
        }
    }
    Ok(!value)
}
