use super::budget::{PRESENTATION_ALLOCATION_BASE, ZIP_METADATA_BYTES_PER_ENTRY};
use super::error::{limit, malformed};
use super::model::PackageOpenPlan;
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};

#[allow(clippy::too_many_lines)]
fn zip_central_plan(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<(u32, u64, u64), ConversionError> {
    const EOCD_BYTES: usize = 22;
    const MAX_COMMENT_BYTES: usize = 65_535;
    if bytes.len() < EOCD_BYTES {
        return Err(malformed(None, "ZIP end record is missing"));
    }
    let search_start = bytes.len().saturating_sub(EOCD_BYTES + MAX_COMMENT_BYTES);
    let mut end_record = None;
    for position in (search_start..=bytes.len() - EOCD_BYTES).rev() {
        if position.is_multiple_of(4096) {
            context.checkpoint()?;
        }
        if bytes.get(position..position + 4) != Some(&[0x50, 0x4b, 0x05, 0x06]) {
            continue;
        }
        let comment = usize::from(zip_u16(bytes, position + 20)?);
        if position.checked_add(EOCD_BYTES).and_then(|end| end.checked_add(comment))
            == Some(bytes.len())
        {
            end_record = Some(position);
            break;
        }
    }
    let end_record = end_record.ok_or_else(|| malformed(None, "ZIP end record is invalid"))?;
    if zip_u16(bytes, end_record + 4)? != 0 || zip_u16(bytes, end_record + 6)? != 0 {
        return Err(malformed(None, "multi-disk ZIP archives are not supported"));
    }
    let entries_on_disk = zip_u16(bytes, end_record + 8)?;
    let entries = zip_u16(bytes, end_record + 10)?;
    let central_size_32 = zip_u32(bytes, end_record + 12)?;
    let central_offset_32 = zip_u32(bytes, end_record + 16)?;
    let zip64 = entries_on_disk == u16::MAX
        || entries == u16::MAX
        || central_size_32 == u32::MAX
        || central_offset_32 == u32::MAX;
    let (entry_count, central_size, central_offset, central_limit) = if zip64 {
        let locator = end_record
            .checked_sub(20)
            .ok_or_else(|| malformed(None, "ZIP64 locator is missing"))?;
        if bytes.get(locator..locator + 4) != Some(&[0x50, 0x4b, 0x06, 0x07])
            || zip_u32(bytes, locator + 4)? != 0
            || zip_u32(bytes, locator + 16)? != 1
        {
            return Err(malformed(None, "ZIP64 locator is invalid"));
        }
        let zip64_offset = usize::try_from(zip_u64(bytes, locator + 8)?)
            .map_err(|_| malformed(None, "ZIP64 end offset cannot be represented"))?;
        if bytes.get(zip64_offset..zip64_offset.saturating_add(4))
            != Some(&[0x50, 0x4b, 0x06, 0x06])
        {
            return Err(malformed(None, "ZIP64 end record is invalid"));
        }
        let record_size = usize::try_from(zip_u64(bytes, zip64_offset + 4)?)
            .map_err(|_| malformed(None, "ZIP64 end size cannot be represented"))?;
        if record_size < 44
            || zip64_offset.checked_add(12).and_then(|end| end.checked_add(record_size))
                != Some(locator)
            || zip_u32(bytes, zip64_offset + 16)? != 0
            || zip_u32(bytes, zip64_offset + 20)? != 0
        {
            return Err(malformed(None, "ZIP64 end record is inconsistent"));
        }
        let on_disk = zip_u64(bytes, zip64_offset + 24)?;
        let total = zip_u64(bytes, zip64_offset + 32)?;
        if on_disk != total {
            return Err(malformed(None, "ZIP64 entry counts disagree"));
        }
        (
            u32::try_from(total)
                .map_err(|_| limit("max_archive_entries", "ZIP64 entry count exceeds u32"))?,
            zip_u64(bytes, zip64_offset + 40)?,
            zip_u64(bytes, zip64_offset + 48)?,
            zip64_offset,
        )
    } else {
        if entries_on_disk != entries {
            return Err(malformed(None, "ZIP entry counts disagree"));
        }
        (u32::from(entries), u64::from(central_size_32), u64::from(central_offset_32), end_record)
    };
    let mut cursor = usize::try_from(central_offset)
        .map_err(|_| malformed(None, "ZIP central offset cannot be represented"))?;
    let central_size = usize::try_from(central_size)
        .map_err(|_| malformed(None, "ZIP central size cannot be represented"))?;
    let central_end = cursor
        .checked_add(central_size)
        .filter(|end| *end <= central_limit && *end <= bytes.len())
        .ok_or_else(|| malformed(None, "ZIP central directory is out of bounds"))?;
    let mut name_bytes = 0_u64;
    for index in 0..entry_count {
        if index.is_multiple_of(1024) {
            context.checkpoint()?;
        }
        if bytes.get(cursor..cursor.saturating_add(4)) != Some(&[0x50, 0x4b, 0x01, 0x02]) {
            return Err(malformed(None, "ZIP central file header is invalid"));
        }
        let name = usize::from(zip_u16(bytes, cursor + 28)?);
        let extra = usize::from(zip_u16(bytes, cursor + 30)?);
        let comment = usize::from(zip_u16(bytes, cursor + 32)?);
        name_bytes = name_bytes
            .checked_add(u64::try_from(name).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("max_memory_bytes", "ZIP name-byte plan overflow"))?;
        cursor = cursor
            .checked_add(46)
            .and_then(|value| value.checked_add(name))
            .and_then(|value| value.checked_add(extra))
            .and_then(|value| value.checked_add(comment))
            .filter(|value| *value <= central_end)
            .ok_or_else(|| malformed(None, "ZIP central file header is truncated"))?;
    }
    if cursor != central_end {
        return Err(malformed(None, "ZIP central directory has trailing records"));
    }
    let variable_bytes = u64::try_from(central_size)
        .unwrap_or(u64::MAX)
        .checked_add(u64::from(zip_u16(bytes, end_record + 20)?))
        .ok_or_else(|| limit("max_memory_bytes", "ZIP variable metadata plan overflow"))?;
    Ok((entry_count, name_bytes, variable_bytes))
}

fn zip_u16(bytes: &[u8], offset: usize) -> Result<u16, ConversionError> {
    let value = bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|value| <[u8; 2]>::try_from(value).ok())
        .ok_or_else(|| malformed(None, "truncated ZIP integer"))?;
    Ok(u16::from_le_bytes(value))
}

fn zip_u32(bytes: &[u8], offset: usize) -> Result<u32, ConversionError> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
        .ok_or_else(|| malformed(None, "truncated ZIP integer"))?;
    Ok(u32::from_le_bytes(value))
}

fn zip_u64(bytes: &[u8], offset: usize) -> Result<u64, ConversionError> {
    let value = bytes
        .get(offset..offset.saturating_add(8))
        .and_then(|value| <[u8; 8]>::try_from(value).ok())
        .ok_or_else(|| malformed(None, "truncated ZIP integer"))?;
    Ok(u64::from_le_bytes(value))
}

pub(super) fn package_open_plan(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<PackageOpenPlan, ConversionError> {
    let input_size = u64::try_from(bytes.len())
        .map_err(|_| limit("max_input_bytes", "presentation size overflow"))?;
    if input_size > options.limits.max_input_bytes {
        return Err(limit(
            "max_input_bytes",
            format!("{input_size} > {}", options.limits.max_input_bytes),
        ));
    }
    let (entry_count, name_bytes, variable_bytes) = zip_central_plan(bytes, context)?;
    if entry_count > options.limits.max_archive_entries {
        return Err(limit(
            "max_archive_entries",
            format!("{entry_count} > {}", options.limits.max_archive_entries),
        ));
    }
    let memory_charge = u64::from(entry_count)
        .checked_mul(ZIP_METADATA_BYTES_PER_ENTRY)
        .and_then(|value| value.checked_add(name_bytes.checked_mul(3)?))
        .and_then(|value| value.checked_add(variable_bytes.checked_mul(2)?))
        .and_then(|value| value.checked_add(PRESENTATION_ALLOCATION_BASE))
        .ok_or_else(|| limit("max_memory_bytes", "ZIP metadata budget overflow"))?;
    Ok(PackageOpenPlan { entry_count, name_bytes, memory_charge })
}
