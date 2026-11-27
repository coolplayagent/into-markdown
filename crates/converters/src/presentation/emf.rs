//! MS-EMF framing and source Unicode text. Graphics and comments remain opaque.
use super::error::{limit, malformed};
use super::model::Package;
use into_markdown_core::{ConversionError, ExecutionContext};

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

// MS-EMF 2.2.5 and 2.3.5.8. Offsets are relative to the record. Skip glyphs,
// byte-sized characters, and records whose string or spacing overlaps another field.
fn wide_text(record: &[u8]) -> Option<&[u8]> {
    let count = usize::try_from(read_u32(record, 44)?).ok()?;
    let start = usize::try_from(read_u32(record, 48)?).ok()?;
    let options = read_u32(record, 52)?;
    let header_len = if options & 0x100 != 0 { 60 } else { 76 };
    let spacing = usize::try_from(read_u32(record, header_len - 4)?).ok()?;
    if options & (0x10 | 0x200) != 0 || count == 0 || start < header_len || !start.is_multiple_of(2)
    {
        return None;
    }
    let end = start.checked_add(count.checked_mul(2)?)?;
    let text = record.get(start..end)?;
    if spacing != 0 {
        let stride = if options & 0x2000 != 0 { 8 } else { 4 };
        let spacing_end = spacing.checked_add(count.checked_mul(stride)?)?;
        if !spacing.is_multiple_of(4)
            || spacing < header_len
            || spacing_end > record.len()
            || (spacing < end && start < spacing_end)
        {
            return None;
        }
    }
    Some(text)
}

fn characters(
    bytes: &[u8],
) -> impl Iterator<Item = Result<char, std::char::DecodeUtf16Error>> + '_ {
    char::decode_utf16(bytes.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])))
}

fn text_length(bytes: &[u8], context: &ExecutionContext) -> Result<usize, ConversionError> {
    let mut length = 0_usize;
    for (i, ch) in characters(bytes).enumerate() {
        if i.is_multiple_of(4096) {
            context.checkpoint()?;
        }
        let Ok(ch) = ch else { return Ok(0) };
        if ch.is_control() && !matches!(ch, '\t' | '\r' | '\n') {
            return Ok(0);
        }
        length = length
            .checked_add(ch.len_utf8())
            .ok_or_else(|| limit("max_memory_bytes", "EMF text length overflow"))?;
    }
    Ok(length)
}

pub(super) fn source_text(
    bytes: &[u8],
    part: &str,
    context: &ExecutionContext,
    package: &mut Package<'_>,
) -> Result<String, ConversionError> {
    let mut length = 0_usize;
    visit_records(bytes, part, context, |encoded| {
        let added = text_length(encoded, context)?;
        if added != 0 {
            length = length
                .checked_add(usize::from(length != 0))
                .and_then(|n| n.checked_add(added))
                .ok_or_else(|| limit("max_memory_bytes", "EMF text length overflow"))?;
        }
        Ok(())
    })?;
    let mut text = String::new();
    if length != 0 {
        package.grow_memory(length as u64)?;
        text.try_reserve_exact(length)
            .map_err(|error| limit("max_memory_bytes", error.to_string()))?;
        visit_records(bytes, part, context, |encoded| {
            if text_length(encoded, context)? != 0 {
                if !text.is_empty() {
                    text.push('\n');
                }
                for (index, ch) in characters(encoded).flatten().enumerate() {
                    if index.is_multiple_of(4096) {
                        context.checkpoint()?;
                    }
                    text.push(ch);
                }
            }
            Ok(())
        })?;
    }
    Ok(text)
}

fn visit_records(
    bytes: &[u8],
    part: &str,
    context: &ExecutionContext,
    mut visit: impl FnMut(&[u8]) -> Result<(), ConversionError>,
) -> Result<(), ConversionError> {
    let invalid = || malformed(Some(part), "invalid EMF record framing");
    let word = |data: &[u8], offset| read_u32(data, offset).ok_or_else(invalid);
    if bytes.get(40..44) != Some(b" EMF")
        || word(bytes, 0)? != 1
        || word(bytes, 4)? < 88
        || word(bytes, 44)? != 0x10000
        || usize::try_from(word(bytes, 48)?).ok() != Some(bytes.len())
    {
        return Err(invalid());
    }
    let declared_count = word(bytes, 52)?;
    let mut remaining = bytes;
    let mut count = 0_u32;
    while !remaining.is_empty() {
        if count.is_multiple_of(256) {
            context.checkpoint()?;
        }
        let kind = word(remaining, 0)?;
        let length = usize::try_from(word(remaining, 4)?).map_err(|_| invalid())?;
        if length < 8 || !length.is_multiple_of(4) || (kind == 1 && count != 0) {
            return Err(invalid());
        }
        let (record, rest) = remaining.split_at_checked(length).ok_or_else(invalid)?;
        count += 1;
        if kind == 14 {
            return if rest.is_empty()
                && count == declared_count
                && length >= 20
                && usize::try_from(word(record, length - 4)?).ok() == Some(length)
            {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        if kind == 84
            && let Some(text) = wide_text(record)
        {
            visit(text)?;
        }
        remaining = rest;
    }
    Err(invalid())
}
