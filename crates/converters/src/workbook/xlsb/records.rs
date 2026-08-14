use crate::workbook::error::{limit, malformed};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};

pub(in crate::workbook) fn validate_xlsb_rich_string(
    payload: &[u8],
    part: &str,
    options: &ConversionOptions,
) -> Result<String, ConversionError> {
    let flags =
        *payload.first().ok_or_else(|| malformed(Some(part), "truncated XLSB RichStr flags"))?;
    // The upper six bits are undefined by MS-XLSB and MUST be ignored.
    let flags = flags & 0x03;
    if flags & 0x02 != 0 {
        return Err(ConversionError::Unsupported {
            detail: format!("phonetic XLSB RichStr is unsupported ({part})"),
        });
    }
    let (value, consumed) = xlsb_wide_string(payload, 1, false, part)?;
    if flags == 0 {
        if consumed != payload.len() {
            return Err(malformed(Some(part), "trailing plain XLSB RichStr bytes"));
        }
        return Ok(value);
    }
    let count_end = consumed
        .checked_add(4)
        .filter(|end| *end <= payload.len())
        .ok_or_else(|| malformed(Some(part), "truncated XLSB RichStr runs"))?;
    let count = usize::try_from(le_u32(&payload[consumed..count_end]))
        .map_err(|_| malformed(Some(part), "XLSB RichStr run count overflow"))?;
    if count == 0 {
        return Err(malformed(Some(part), "empty XLSB RichStr run table"));
    }
    if count > 0x7fff || u64::try_from(count).unwrap_or(u64::MAX) > options.limits.max_table_cells {
        return Err(limit("max_table_cells", "too many XLSB RichStr runs"));
    }
    let end = count_end
        .checked_add(
            count
                .checked_mul(4)
                .ok_or_else(|| malformed(Some(part), "XLSB RichStr run size overflow"))?,
        )
        .filter(|end| *end == payload.len())
        .ok_or_else(|| malformed(Some(part), "invalid XLSB RichStr run payload"))?;
    let character_count = u32::try_from(value.encode_utf16().count())
        .map_err(|_| malformed(Some(part), "XLSB RichStr text is too long"))?;
    let mut previous = None;
    for run in payload[count_end..end].chunks_exact(4) {
        let index = u32::from(u16::from_le_bytes([run[0], run[1]]));
        if index >= character_count || previous.is_some_and(|value| index <= value) {
            return Err(malformed(Some(part), "invalid XLSB RichStr run range"));
        }
        previous = Some(index);
    }
    Ok(value)
}

pub(super) fn binary_declared_count(
    payload: &[u8],
    part: &str,
    options: &ConversionOptions,
) -> Result<u64, ConversionError> {
    if payload.len() != 4 {
        return Err(malformed(Some(part), "invalid XLSB collection count"));
    }
    let value = u64::from(le_u32(payload));
    if value > options.limits.max_table_cells {
        return Err(limit("max_table_cells", "XLSB collection declaration is too large"));
    }
    Ok(value)
}

pub(super) fn visit_xlsb_records(
    data: &[u8],
    part: &str,
    context: &ExecutionContext,
    mut visit: impl FnMut(u16, &[u8]) -> Result<(), ConversionError>,
) -> Result<(), ConversionError> {
    let mut offset = 0_usize;
    while offset < data.len() {
        context.checkpoint()?;
        let typ = u16::try_from(read_xlsb_varint(data, &mut offset, 2, part)?)
            .map_err(|_| malformed(Some(part), "XLSB record type overflow"))?;
        let len = usize::try_from(read_xlsb_varint(data, &mut offset, 4, part)?)
            .map_err(|_| malformed(Some(part), "XLSB record length overflow"))?;
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated XLSB record"))?;
        visit(typ, &data[offset..end])?;
        offset = end;
    }
    Ok(())
}

pub(in crate::workbook) fn read_xlsb_varint(
    data: &[u8],
    offset: &mut usize,
    max_bytes: usize,
    part: &str,
) -> Result<u32, ConversionError> {
    let mut value = 0_u32;
    for shift in 0..max_bytes {
        let byte = *data
            .get(*offset)
            .ok_or_else(|| malformed(Some(part), "truncated XLSB record header"))?;
        *offset += 1;
        value |= u32::from(byte & 0x7f) << (7 * shift);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(malformed(Some(part), "overlong XLSB record header"))
}

pub(in crate::workbook) fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(in crate::workbook) fn xlsb_wide_string(
    data: &[u8],
    offset: usize,
    nullable: bool,
    part: &str,
) -> Result<(String, usize), ConversionError> {
    let length_end = offset
        .checked_add(4)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| malformed(Some(part), "truncated XLSB string length"))?;
    let length = le_u32(&data[offset..length_end]);
    if length == u32::MAX {
        return if nullable {
            Ok((String::new(), length_end))
        } else {
            Err(malformed(Some(part), "unexpected null XLSB string"))
        };
    }
    let bytes = usize::try_from(length)
        .ok()
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| malformed(Some(part), "XLSB string length overflow"))?;
    let end = length_end
        .checked_add(bytes)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| malformed(Some(part), "truncated XLSB string"))?;
    let units = data[length_end..end]
        .chunks_exact(2)
        .map(|value| u16::from_le_bytes([value[0], value[1]]))
        .collect::<Vec<_>>();
    let value = String::from_utf16(&units)
        .map_err(|_| malformed(Some(part), "invalid UTF-16 in XLSB string"))?;
    Ok((value, end))
}
