use super::meter::Meter;
use super::{Summary, limit, malformed};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceLimits};

pub(super) fn validate(
    bytes: &[u8],
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<Summary, ConversionError> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(malformed("JPEG SOI signature is invalid"));
    }
    let mut meter = Meter::new(context);
    meter.consume(2)?;
    let mut cursor = 2_usize;
    let mut pending = None;
    let mut saw_frame = false;
    let mut markers = 0_u32;
    loop {
        let marker = if let Some(marker) = pending.take() {
            marker
        } else {
            read_marker(bytes, &mut cursor, &mut meter, &mut markers, limits.max_archive_entries)?
        };
        match marker {
            0xd9 => {
                if cursor != bytes.len() || !saw_frame {
                    return Err(malformed("JPEG EOI must follow a frame and end exactly at EOF"));
                }
                return Ok(Summary { frames: 1, animated: false });
            }
            0xd8 | 0x00 => return Err(malformed("JPEG contains an invalid structural marker")),
            0x01 | 0xd0..=0xd7 => {}
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                let end = segment_end(bytes, cursor)?;
                if end.saturating_sub(cursor) < 8 {
                    return Err(malformed("JPEG frame header is truncated"));
                }
                let height = u16::from_be_bytes([bytes[cursor + 3], bytes[cursor + 4]]);
                let width = u16::from_be_bytes([bytes[cursor + 5], bytes[cursor + 6]]);
                if width == 0 || height == 0 || saw_frame {
                    return Err(malformed("JPEG requires one non-empty frame header"));
                }
                saw_frame = true;
                meter.consume(end - cursor)?;
                cursor = end;
            }
            0xda => {
                if !saw_frame {
                    return Err(malformed("JPEG scan precedes its frame header"));
                }
                let end = segment_end(bytes, cursor)?;
                meter.consume(end - cursor)?;
                cursor = end;
                pending = Some(read_entropy_marker(
                    bytes,
                    &mut cursor,
                    &mut meter,
                    &mut markers,
                    limits.max_archive_entries,
                )?);
            }
            _ => {
                let end = segment_end(bytes, cursor)?;
                meter.consume(end - cursor)?;
                cursor = end;
            }
        }
    }
}

fn read_marker(
    bytes: &[u8],
    cursor: &mut usize,
    meter: &mut Meter<'_>,
    markers: &mut u32,
    max_markers: u32,
) -> Result<u8, ConversionError> {
    if bytes.get(*cursor) != Some(&0xff) {
        return Err(malformed("JPEG data appears outside a marker or entropy scan"));
    }
    while bytes.get(*cursor) == Some(&0xff) {
        let start = *cursor;
        let end = start.saturating_add(meter.next_batch());
        while *cursor < end && bytes.get(*cursor) == Some(&0xff) {
            *cursor = cursor
                .checked_add(1)
                .ok_or_else(|| limit("max_input_bytes", "JPEG marker offset overflowed"))?;
        }
        meter.consume(*cursor - start)?;
    }
    let marker = *bytes.get(*cursor).ok_or_else(|| malformed("JPEG marker is truncated"))?;
    *cursor = cursor
        .checked_add(1)
        .ok_or_else(|| limit("max_input_bytes", "JPEG marker offset overflowed"))?;
    meter.consume(1)?;
    *markers = markers
        .checked_add(1)
        .ok_or_else(|| limit("image_chunks", "JPEG marker count overflowed"))?;
    if *markers > max_markers {
        return Err(limit("image_chunks", "JPEG has too many markers"));
    }
    Ok(marker)
}

fn read_entropy_marker(
    bytes: &[u8],
    cursor: &mut usize,
    meter: &mut Meter<'_>,
    markers: &mut u32,
    max_markers: u32,
) -> Result<u8, ConversionError> {
    loop {
        let start = *cursor;
        let end = start.saturating_add(meter.next_batch());
        while *cursor < end && bytes.get(*cursor).is_some_and(|byte| *byte != 0xff) {
            *cursor = cursor
                .checked_add(1)
                .ok_or_else(|| limit("max_input_bytes", "JPEG scan offset overflowed"))?;
        }
        meter.consume(*cursor - start)?;
        if *cursor < bytes.len() && bytes.get(*cursor) != Some(&0xff) {
            continue;
        }
        if *cursor == bytes.len() {
            return Err(malformed("JPEG entropy scan has no EOI"));
        }
        let marker = read_marker(bytes, cursor, meter, markers, max_markers)?;
        if !matches!(marker, 0x00 | 0xd0..=0xd7) {
            return Ok(marker);
        }
    }
}

fn segment_end(bytes: &[u8], offset: usize) -> Result<usize, ConversionError> {
    let raw = bytes
        .get(offset..offset.saturating_add(2))
        .ok_or_else(|| malformed("JPEG segment length is truncated"))?;
    let length = usize::from(u16::from_be_bytes([raw[0], raw[1]]));
    if length < 2 {
        return Err(malformed("JPEG segment length is smaller than its header"));
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| limit("max_input_bytes", "JPEG segment offset overflowed"))?;
    if end > bytes.len() {
        return Err(malformed("JPEG segment exceeds source bytes"));
    }
    Ok(end)
}
