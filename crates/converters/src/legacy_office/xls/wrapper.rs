use super::preflight::has_noncanonical_formula_string_cache;
use super::{
    BIFF4, BIFF5, BOF, BOF4, CFB_DIFAT, CFB_DIFAT_ENTRIES, CFB_END, CFB_FAT, CFB_FAT_ENTRIES,
    CFB_FREE, CFB_HEADER_DIFAT_ENTRIES, CFB_SECTOR_BYTES, ConversionError, DIMENSIONS, EOF,
    FORMULA, WORKBOOK, limit, malformed, read_u16,
};

#[derive(Clone, Copy)]
pub(super) struct RawBiff4Plan {
    global_prefix_start: usize,
    global_prefix_end: usize,
    pub(super) capacity: usize,
}

pub(super) fn raw_biff4_plan(bytes: &[u8]) -> Result<RawBiff4Plan, ConversionError> {
    let first_length = usize::from(read_u16(bytes, 2, WORKBOOK)?);
    let mut cursor = 4usize
        .checked_add(first_length)
        .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 BOF length overflowed"))?;
    let global_prefix_start = cursor;
    let mut global_prefix_end = None;
    let mut formula_count = 0usize;
    while cursor < bytes.len() {
        let header_end = cursor
            .checked_add(4)
            .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 record offset overflowed"))?;
        let header = bytes
            .get(cursor..header_end)
            .ok_or_else(|| malformed(WORKBOOK, "truncated raw BIFF4 record header"))?;
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 record length overflowed"))?;
        bytes.get(cursor..end).ok_or_else(|| malformed(WORKBOOK, "truncated raw BIFF4 record"))?;
        if global_prefix_end.is_none() && kind == DIMENSIONS {
            global_prefix_end = Some(cursor);
        }
        if global_prefix_end.is_none() && kind == EOF {
            break;
        }
        if kind == 0x0406 {
            formula_count = formula_count
                .checked_add(1)
                .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 Formula count overflowed"))?;
        }
        cursor = end;
    }
    let Some(global_prefix_end) = global_prefix_end else {
        return Err(malformed(WORKBOOK, "raw BIFF4 stream has no Dimensions record"));
    };

    let global_prefix = bytes
        .get(global_prefix_start..global_prefix_end)
        .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 global record range is invalid"))?;
    let formula_extra = formula_count
        .checked_mul(4)
        .ok_or_else(|| malformed(WORKBOOK, "normalized BIFF4 Formula bytes overflowed"))?;
    let capacity = bytes
        .len()
        .checked_add(global_prefix.len())
        .and_then(|value| value.checked_add(formula_extra))
        .and_then(|value| value.checked_add(64))
        .ok_or_else(|| malformed(WORKBOOK, "normalized BIFF4 size overflowed"))?;
    Ok(RawBiff4Plan { global_prefix_start, global_prefix_end, capacity })
}

pub(super) fn normalize_raw_biff4(
    bytes: &[u8],
    plan: RawBiff4Plan,
) -> Result<Vec<u8>, ConversionError> {
    let global_prefix = bytes
        .get(plan.global_prefix_start..plan.global_prefix_end)
        .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 global record range is invalid"))?;
    let mut output = Vec::new();
    output.try_reserve_exact(plan.capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve normalized BIFF4 stream: {error}"))
    })?;
    push_biff_record(&mut output, BOF, &[0x00, 0x04, 0x05, 0x00, 0, 0, 0, 0])?;
    output.extend_from_slice(global_prefix);
    append_normalized_biff4_sheet_header(&mut output)?;

    let mut cursor = 0;
    while cursor < bytes.len() {
        let body_start = cursor
            .checked_add(4)
            .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 record offset overflowed"))?;
        let header = bytes
            .get(cursor..body_start)
            .ok_or_else(|| malformed(WORKBOOK, "truncated raw BIFF4 record header"))?;
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 record length overflowed"))?;
        let body = bytes
            .get(body_start..end)
            .ok_or_else(|| malformed(WORKBOOK, "truncated raw BIFF4 record"))?;
        match kind {
            BOF4 => {
                let mut bof = body.to_vec();
                let version = bof
                    .get_mut(..2)
                    .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 BOF body is truncated"))?;
                version.copy_from_slice(&BIFF4.to_le_bytes());
                push_biff_record(&mut output, BOF, &bof)?;
            }
            0x0406 => push_normalized_biff4_formula(&mut output, body)?,
            _ => push_biff_record(&mut output, kind, body)?,
        }
        cursor = end;
    }
    Ok(output)
}

pub(super) fn append_normalized_biff4_sheet_header(
    output: &mut Vec<u8>,
) -> Result<(), ConversionError> {
    let sheet_name = b"Sheet 1";
    let bound_sheet_record_bytes = 4usize
        .checked_add(6)
        .and_then(|value| value.checked_add(1 + sheet_name.len()))
        .ok_or_else(|| malformed(WORKBOOK, "BIFF4 BoundSheet size overflowed"))?;
    let sheet_offset = output
        .len()
        .checked_add(bound_sheet_record_bytes)
        .and_then(|value| value.checked_add(4))
        .ok_or_else(|| malformed(WORKBOOK, "BIFF4 sheet offset overflowed"))?;
    let mut bound_sheet = Vec::with_capacity(bound_sheet_record_bytes - 4);
    bound_sheet.extend_from_slice(&to_u32(sheet_offset)?.to_le_bytes());
    bound_sheet.extend_from_slice(&[0, 0, u8::try_from(sheet_name.len()).unwrap_or(u8::MAX)]);
    bound_sheet.extend_from_slice(sheet_name);
    push_biff_record(output, 0x0085, &bound_sheet)?;
    push_biff_record(output, EOF, &[])
}

pub(super) fn push_normalized_biff4_formula(
    output: &mut Vec<u8>,
    body: &[u8],
) -> Result<(), ConversionError> {
    // BIFF4 places `cce` immediately after the two-byte option flags at offset 16.  The
    // bounded downstream reader shares its BIFF8 record framing and expects the four-byte
    // reserved field before `cce`.  Inserting that inert field preserves the cached value,
    // options, token bytes, and BIFF4 token semantics selected by the BOF version.
    if body.len() < 18 {
        return Err(malformed(WORKBOOK, "raw BIFF4 Formula record is truncated"));
    }
    let normalized_length = body
        .len()
        .checked_add(4)
        .ok_or_else(|| malformed(WORKBOOK, "normalized BIFF4 Formula size overflowed"))?;
    output.extend_from_slice(&0x0006_u16.to_le_bytes());
    output.extend_from_slice(&to_u16(normalized_length)?.to_le_bytes());
    output.extend_from_slice(&body[..16]);
    output.extend_from_slice(&[0; 4]);
    output.extend_from_slice(&body[16..]);
    Ok(())
}

pub(super) fn push_biff_record(
    output: &mut Vec<u8>,
    kind: u16,
    body: &[u8],
) -> Result<(), ConversionError> {
    let length = to_u16(body.len())?;
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(body);
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct CfbWrapperLayout {
    stream_sectors: usize,
    fat_sectors: usize,
    difat_sectors: usize,
    pub(super) total_sectors: usize,
    pub(super) output_bytes: usize,
}

pub(super) fn cfb_wrapper_layout(
    workbook_bytes: usize,
) -> Result<CfbWrapperLayout, ConversionError> {
    let logical_bytes = workbook_bytes.max(4096);
    let stream_sectors = logical_bytes.div_ceil(CFB_SECTOR_BYTES);
    let mut fat_sectors = 1usize;
    loop {
        let difat_sectors =
            fat_sectors.saturating_sub(CFB_HEADER_DIFAT_ENTRIES).div_ceil(CFB_DIFAT_ENTRIES);
        let total_sectors = stream_sectors
            .checked_add(1)
            .and_then(|value| value.checked_add(fat_sectors))
            .and_then(|value| value.checked_add(difat_sectors))
            .ok_or_else(|| {
                malformed(WORKBOOK, "CFB compatibility wrapper sector count overflowed")
            })?;
        let required_fat = total_sectors.div_ceil(CFB_FAT_ENTRIES);
        if required_fat == fat_sectors {
            let output_bytes = total_sectors
                .checked_add(1)
                .and_then(|value| value.checked_mul(CFB_SECTOR_BYTES))
                .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility wrapper size overflowed"))?;
            return Ok(CfbWrapperLayout {
                stream_sectors,
                fat_sectors,
                difat_sectors,
                total_sectors,
                output_bytes,
            });
        }
        fat_sectors = required_fat;
    }
}

pub(super) fn build_cfb_wrapper(
    workbook: &[u8],
    recover_dimension_metadata: bool,
    recover_formula_cache_metadata: bool,
    biff_version: u16,
    layout: CfbWrapperLayout,
) -> Result<Vec<u8>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve_exact(layout.output_bytes).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve CFB compatibility wrapper: {error}"))
    })?;
    output.resize(layout.output_bytes, 0);
    write_cfb_wrapper_header(&mut output, layout)?;
    let fat_start = layout.stream_sectors + 1;
    let difat_start = fat_start + layout.fat_sectors;
    let stream_start = CFB_SECTOR_BYTES;
    output[stream_start..stream_start + workbook.len()].copy_from_slice(workbook);
    if recover_dimension_metadata {
        patch_noncanonical_dimensions(&mut output, stream_start, workbook, biff_version)?;
    }
    if recover_formula_cache_metadata {
        patch_noncanonical_formula_string_caches(&mut output, stream_start, workbook)?;
    }

    let directory_offset = sector_offset_in_wrapper(layout.stream_sectors)?;
    write_directory_entry(
        &mut output,
        directory_offset,
        DirectoryEntrySpec {
            name: "Root Entry",
            kind: 5,
            left: CFB_FREE,
            right: CFB_FREE,
            child: 1,
            start: CFB_END,
            size: 0,
        },
    )?;
    write_directory_entry(
        &mut output,
        directory_offset + 128,
        DirectoryEntrySpec {
            name: WORKBOOK,
            kind: 2,
            left: CFB_FREE,
            right: CFB_FREE,
            child: CFB_FREE,
            start: 0,
            size: u64::try_from(workbook.len().max(4096)).unwrap_or(u64::MAX),
        },
    )?;

    let mut fat = Vec::new();
    fat.try_reserve_exact(layout.total_sectors).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve CFB compatibility FAT: {error}"))
    })?;
    fat.resize(layout.total_sectors, CFB_FREE);
    for (index, slot) in fat.iter_mut().take(layout.stream_sectors).enumerate() {
        *slot = if index + 1 == layout.stream_sectors { CFB_END } else { to_u32(index + 1)? };
    }
    fat[layout.stream_sectors] = CFB_END;
    for slot in fat.iter_mut().skip(fat_start).take(layout.fat_sectors) {
        *slot = CFB_FAT;
    }
    for slot in fat.iter_mut().skip(difat_start).take(layout.difat_sectors) {
        *slot = CFB_DIFAT;
    }
    write_cfb_wrapper_allocation_tables(&mut output, &fat, layout, fat_start, difat_start)?;
    Ok(output)
}

pub(super) fn patch_noncanonical_formula_string_caches(
    output: &mut [u8],
    stream_start: usize,
    workbook: &[u8],
) -> Result<(), ConversionError> {
    let mut cursor = 0_usize;
    while cursor < workbook.len() {
        let header = workbook
            .get(cursor..cursor.saturating_add(4))
            .ok_or_else(|| malformed(WORKBOOK, "formula-cache patch found a truncated header"))?;
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| malformed(WORKBOOK, "formula-cache patch offset overflowed"))?;
        let body = workbook
            .get(cursor + 4..end)
            .ok_or_else(|| malformed(WORKBOOK, "formula-cache patch found a truncated record"))?;
        if kind == FORMULA && has_noncanonical_formula_string_cache(body) {
            let reserved = stream_start
                .checked_add(cursor)
                .and_then(|value| value.checked_add(11))
                .ok_or_else(|| malformed(WORKBOOK, "formula-cache patch offset overflowed"))?;
            output
                .get_mut(reserved..reserved + 5)
                .ok_or_else(|| {
                    malformed(WORKBOOK, "formula-cache patch is outside Workbook stream")
                })?
                .fill(0);
        }
        cursor = end;
    }
    Ok(())
}

pub(super) fn patch_noncanonical_dimensions(
    output: &mut [u8],
    stream_start: usize,
    workbook: &[u8],
    biff_version: u16,
) -> Result<(), ConversionError> {
    let expected_length = if matches!(biff_version, BIFF4 | BIFF5) { 10 } else { 14 };
    let mut cursor = 0_usize;
    while cursor < workbook.len() {
        let header = workbook
            .get(cursor..cursor.saturating_add(4))
            .ok_or_else(|| malformed(WORKBOOK, "compatibility patch found a truncated header"))?;
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| malformed(WORKBOOK, "compatibility patch offset overflowed"))?;
        if end > workbook.len() {
            return Err(malformed(WORKBOOK, "compatibility patch found a truncated record"));
        }
        if kind == DIMENSIONS && length != expected_length {
            let physical = stream_start.checked_add(cursor).ok_or_else(|| {
                malformed(WORKBOOK, "Dimensions compatibility patch offset overflowed")
            })?;
            output
                .get_mut(physical..physical + 2)
                .ok_or_else(|| {
                    malformed(WORKBOOK, "Dimensions compatibility patch is outside Workbook stream")
                })?
                .copy_from_slice(&0xffff_u16.to_le_bytes());
        }
        cursor = end;
    }
    Ok(())
}

pub(super) fn write_cfb_wrapper_header(
    output: &mut [u8],
    layout: CfbWrapperLayout,
) -> Result<(), ConversionError> {
    output[..8].copy_from_slice(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1");
    put_u16(output, 24, 0x003e)?;
    put_u16(output, 26, 3)?;
    put_u16(output, 28, 0xfffe)?;
    put_u16(output, 30, 9)?;
    put_u16(output, 32, 6)?;
    put_u32(output, 44, to_u32(layout.fat_sectors)?)?;
    let directory_sector = to_u32(layout.stream_sectors)?;
    put_u32(output, 48, directory_sector)?;
    put_u32(output, 56, 4096)?;
    put_u32(output, 60, CFB_END)?;
    let fat_start = layout.stream_sectors + 1;
    let difat_start = fat_start + layout.fat_sectors;
    put_u32(output, 68, if layout.difat_sectors == 0 { CFB_END } else { to_u32(difat_start)? })?;
    put_u32(output, 72, to_u32(layout.difat_sectors)?)?;
    for index in 0..CFB_HEADER_DIFAT_ENTRIES {
        let value = if index < layout.fat_sectors.min(CFB_HEADER_DIFAT_ENTRIES) {
            to_u32(fat_start + index)?
        } else {
            CFB_FREE
        };
        put_u32(output, 76 + index * 4, value)?;
    }
    Ok(())
}

pub(super) fn write_cfb_wrapper_allocation_tables(
    output: &mut [u8],
    fat: &[u32],
    layout: CfbWrapperLayout,
    fat_start: usize,
    difat_start: usize,
) -> Result<(), ConversionError> {
    for fat_index in 0..layout.fat_sectors {
        let offset = sector_offset_in_wrapper(fat_start + fat_index)?;
        for entry in 0..CFB_FAT_ENTRIES {
            let value = fat.get(fat_index * CFB_FAT_ENTRIES + entry).copied().unwrap_or(CFB_FREE);
            put_u32(output, offset + entry * 4, value)?;
        }
    }
    let remaining_fat = layout.fat_sectors.saturating_sub(CFB_HEADER_DIFAT_ENTRIES);
    for difat_index in 0..layout.difat_sectors {
        let offset = sector_offset_in_wrapper(difat_start + difat_index)?;
        for entry in 0..CFB_DIFAT_ENTRIES {
            let index = difat_index * CFB_DIFAT_ENTRIES + entry;
            let value = if index < remaining_fat {
                to_u32(fat_start + CFB_HEADER_DIFAT_ENTRIES + index)?
            } else {
                CFB_FREE
            };
            put_u32(output, offset + entry * 4, value)?;
        }
        let next = if difat_index + 1 == layout.difat_sectors {
            CFB_END
        } else {
            to_u32(difat_start + difat_index + 1)?
        };
        put_u32(output, offset + CFB_SECTOR_BYTES - 4, next)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct DirectoryEntrySpec<'a> {
    name: &'a str,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

pub(super) fn write_directory_entry(
    output: &mut [u8],
    offset: usize,
    entry: DirectoryEntrySpec<'_>,
) -> Result<(), ConversionError> {
    let mut encoded = entry.name.encode_utf16().collect::<Vec<_>>();
    encoded.push(0);
    if encoded.len() > 32 {
        return Err(malformed(WORKBOOK, "CFB compatibility stream name is too long"));
    }
    for (index, unit) in encoded.iter().enumerate() {
        put_u16(output, offset + index * 2, *unit)?;
    }
    put_u16(output, offset + 64, to_u16(encoded.len() * 2)?)?;
    *output
        .get_mut(offset + 66)
        .ok_or_else(|| malformed(WORKBOOK, "CFB directory entry is truncated"))? = entry.kind;
    *output
        .get_mut(offset + 67)
        .ok_or_else(|| malformed(WORKBOOK, "CFB directory entry is truncated"))? = 1;
    put_u32(output, offset + 68, entry.left)?;
    put_u32(output, offset + 72, entry.right)?;
    put_u32(output, offset + 76, entry.child)?;
    put_u32(output, offset + 116, entry.start)?;
    put_u64(output, offset + 120, entry.size)
}

pub(super) fn sector_offset_in_wrapper(sector: usize) -> Result<usize, ConversionError> {
    sector
        .checked_add(1)
        .and_then(|value| value.checked_mul(CFB_SECTOR_BYTES))
        .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility sector offset overflowed"))
}

pub(super) fn put_u16(output: &mut [u8], offset: usize, value: u16) -> Result<(), ConversionError> {
    output
        .get_mut(offset..offset + 2)
        .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility write is out of bounds"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

pub(super) fn put_u32(output: &mut [u8], offset: usize, value: u32) -> Result<(), ConversionError> {
    output
        .get_mut(offset..offset + 4)
        .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility write is out of bounds"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

pub(super) fn put_u64(output: &mut [u8], offset: usize, value: u64) -> Result<(), ConversionError> {
    output
        .get_mut(offset..offset + 8)
        .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility write is out of bounds"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

pub(super) fn to_u16(value: usize) -> Result<u16, ConversionError> {
    u16::try_from(value).map_err(|_| malformed(WORKBOOK, "CFB compatibility u16 overflowed"))
}

pub(super) fn to_u32(value: usize) -> Result<u32, ConversionError> {
    u32::try_from(value).map_err(|_| malformed(WORKBOOK, "CFB compatibility u32 overflowed"))
}
