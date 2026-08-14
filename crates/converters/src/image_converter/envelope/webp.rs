use super::meter::Meter;
use super::{Summary, limit, malformed};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceLimits};

pub(super) fn validate(
    bytes: &[u8],
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<Summary, ConversionError> {
    if bytes.len() < 20 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(malformed("WebP RIFF signature is invalid"));
    }
    let declared = usize::try_from(u32::from_le_bytes(
        bytes[4..8].try_into().map_err(|_| malformed("WebP RIFF size is truncated"))?,
    ))
    .map_err(|_| limit("max_input_bytes", "WebP RIFF size is unrepresentable"))?;
    if declared.checked_add(8) != Some(bytes.len()) {
        return Err(malformed("WebP RIFF size must end exactly at EOF"));
    }
    let mut meter = Meter::new(context);
    meter.consume(12)?;
    let mut offset = 12_usize;
    let mut vp8x = None;
    let mut primary = 0_u32;
    let mut animation_header = 0_u32;
    let mut frames = 0_u32;
    let mut chunks = 0_u32;
    while offset < bytes.len() {
        bump_chunk(&mut chunks, limits.max_archive_entries)?;
        let chunk = chunk(bytes, offset)?;
        meter.consume(8)?;
        scan_payload(chunk.data, &mut meter)?;
        if let Some(pad) = chunk.pad {
            if pad != 0 {
                return Err(malformed("WebP RIFF padding byte must be zero"));
            }
            meter.consume(1)?;
        }
        match chunk.kind {
            b"VP8X" => {
                if vp8x.is_some() || offset != 12 || chunk.data.len() != 10 {
                    return Err(malformed("WebP requires one 10-byte VP8X first chunk"));
                }
                if chunk.data[0] & 0xc1 != 0 || chunk.data[1..4] != [0, 0, 0] {
                    return Err(malformed("WebP VP8X reserved flags or bytes are non-zero"));
                }
                vp8x = Some(chunk.data[0]);
            }
            b"VP8 " | b"VP8L" => primary = primary.saturating_add(1),
            b"ANIM" => {
                animation_header = animation_header.saturating_add(1);
                if chunk.data.len() != 6 {
                    return Err(malformed("WebP ANIM header length is invalid"));
                }
            }
            b"ANMF" => {
                frames = frames
                    .checked_add(1)
                    .ok_or_else(|| limit("max_pages", "WebP frame count overflowed"))?;
                if frames > limits.max_pages {
                    return Err(limit(
                        "max_pages",
                        format!("{frames} WebP frames > {}", limits.max_pages),
                    ));
                }
                validate_frame(chunk.data, &mut chunks, limits.max_archive_entries)?;
            }
            _ => {}
        }
        offset = chunk.end;
    }
    let animated_flag = vp8x.is_some_and(|flags| flags & 0x02 != 0);
    if animated_flag {
        if animation_header != 1 || frames == 0 || primary != 0 {
            return Err(malformed(
                "animated WebP requires one ANIM and ANMF frames without a top-level image payload",
            ));
        }
        Ok(Summary { frames, animated: true })
    } else {
        if animation_header != 0 || frames != 0 || primary != 1 {
            return Err(malformed(
                "static WebP requires exactly one VP8/VP8L payload and no animation chunks",
            ));
        }
        Ok(Summary { frames: 1, animated: false })
    }
}

struct Chunk<'a> {
    kind: &'a [u8; 4],
    data: &'a [u8],
    pad: Option<u8>,
    end: usize,
}

fn chunk(bytes: &[u8], offset: usize) -> Result<Chunk<'_>, ConversionError> {
    let header = bytes
        .get(offset..offset.saturating_add(8))
        .ok_or_else(|| malformed("WebP chunk header is truncated"))?;
    let kind = header[..4].try_into().map_err(|_| malformed("WebP chunk type is truncated"))?;
    let length = usize::try_from(u32::from_le_bytes(
        header[4..8].try_into().map_err(|_| malformed("WebP chunk size is truncated"))?,
    ))
    .map_err(|_| limit("max_input_bytes", "WebP chunk size is unrepresentable"))?;
    let data_start =
        offset.checked_add(8).ok_or_else(|| limit("max_input_bytes", "WebP offset overflowed"))?;
    let data_end = data_start
        .checked_add(length)
        .ok_or_else(|| limit("max_input_bytes", "WebP chunk length overflowed"))?;
    let padded_end = data_end
        .checked_add(length & 1)
        .ok_or_else(|| limit("max_input_bytes", "WebP padding offset overflowed"))?;
    let data = bytes
        .get(data_start..data_end)
        .ok_or_else(|| malformed("WebP chunk payload is truncated"))?;
    let pad = if length & 1 == 1 {
        Some(*bytes.get(data_end).ok_or_else(|| malformed("WebP padding is truncated"))?)
    } else {
        None
    };
    if padded_end > bytes.len() {
        return Err(malformed("WebP chunk exceeds the RIFF envelope"));
    }
    Ok(Chunk { kind, data, pad, end: padded_end })
}

fn scan_payload(bytes: &[u8], meter: &mut Meter<'_>) -> Result<(), ConversionError> {
    let mut remaining = bytes.len();
    while remaining != 0 {
        let length = remaining.min(meter.next_batch());
        meter.consume(length)?;
        remaining -= length;
    }
    Ok(())
}

fn validate_frame(bytes: &[u8], chunks: &mut u32, max_chunks: u32) -> Result<(), ConversionError> {
    if bytes.len() < 24 {
        return Err(malformed("WebP ANMF frame is truncated"));
    }
    let flags = bytes[15];
    if flags & !0x03 != 0 {
        return Err(malformed("WebP ANMF reserved flags are non-zero"));
    }
    let mut offset = 16_usize;
    let mut primary = 0_u32;
    while offset < bytes.len() {
        bump_chunk(chunks, max_chunks)?;
        let nested = chunk(bytes, offset)?;
        if matches!(nested.kind, b"VP8 " | b"VP8L") {
            primary = primary.saturating_add(1);
        } else if !matches!(nested.kind, b"ALPH") {
            return Err(malformed("WebP ANMF contains an unsupported nested chunk"));
        }
        offset = nested.end;
    }
    if primary != 1 {
        return Err(malformed("WebP ANMF requires exactly one VP8/VP8L payload"));
    }
    Ok(())
}

fn bump_chunk(chunks: &mut u32, max_chunks: u32) -> Result<(), ConversionError> {
    *chunks = chunks
        .checked_add(1)
        .ok_or_else(|| limit("image_chunks", "WebP chunk count overflowed"))?;
    if *chunks > max_chunks {
        return Err(limit("image_chunks", "WebP has too many chunks"));
    }
    Ok(())
}
