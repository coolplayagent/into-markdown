use into_markdown_core::ConversionError;
use std::io::Cursor;

pub(super) fn find_central_start(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
) -> Result<u64, ConversionError> {
    let mut start = u64::MAX;
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| malformed(format!("cannot inspect central entry {index}: {error}")))?;
        start = start.min(entry.central_header_start());
    }
    if start == u64::MAX {
        return Ok(archive.offset());
    }
    Ok(start)
}

pub(super) fn validate_headers(
    bytes: &[u8],
    entry: &zip::read::ZipFile<'_>,
    central_start: u64,
) -> Result<(u64, u64), ConversionError> {
    let local_offset = usize::try_from(entry.header_start())
        .map_err(|_| malformed("local header offset exceeds address space"))?;
    let central_offset = usize::try_from(entry.central_header_start())
        .map_err(|_| malformed("central header offset exceeds address space"))?;
    let local = slice(bytes, local_offset, 30, "local header")?;
    let central = slice(bytes, central_offset, 46, "central header")?;
    if &local[..4] != b"PK\x03\x04" || &central[..4] != b"PK\x01\x02" {
        return Err(malformed("local or central header signature is invalid"));
    }
    let local_flags = le16(local, 6)?;
    let central_flags = le16(central, 8)?;
    let local_method = le16(local, 8)?;
    let central_method = le16(central, 10)?;
    let decoded_method = match entry.compression() {
        zip::CompressionMethod::Stored => 0,
        zip::CompressionMethod::Deflated => 8,
        _ => return Err(malformed("unsupported compression method reached header validation")),
    };
    if local_flags != central_flags
        || local_method != central_method
        || central_method != decoded_method
    {
        return Err(malformed(format!("header flags or method disagree for {:?}", entry.name())));
    }
    if central_flags & 0x2041 != 0 {
        return Err(ConversionError::Encrypted);
    }
    if central_flags & !0x080e != 0 || central_method == 0 && central_flags & 0x0006 != 0 {
        return Err(malformed(format!(
            "entry {:?} uses unsupported general-purpose flags {central_flags:#06x}",
            entry.name()
        )));
    }
    let central_crc = le32(central, 16)?;
    let central_compressed = u64::from(le32(central, 20)?);
    let central_expanded = u64::from(le32(central, 24)?);
    if central_compressed == u64::from(u32::MAX) || central_expanded == u64::from(u32::MAX) {
        return Err(malformed("ZIP64 members are not accepted by the recursive converter"));
    }
    if central_crc != entry.crc32()
        || central_compressed != entry.compressed_size()
        || central_expanded != entry.size()
    {
        return Err(malformed(format!("central metadata disagrees for {:?}", entry.name())));
    }
    let local_name_len = usize::from(le16(local, 26)?);
    let local_extra_len = usize::from(le16(local, 28)?);
    let central_name_len = usize::from(le16(central, 28)?);
    let central_name_offset = central_offset
        .checked_add(46)
        .ok_or_else(|| malformed("central name offset overflowed"))?;
    let local_name_offset =
        local_offset.checked_add(30).ok_or_else(|| malformed("local name offset overflowed"))?;
    let central_name = slice(bytes, central_name_offset, central_name_len, "central name")?;
    let local_name = slice(bytes, local_name_offset, local_name_len, "local name")?;
    if local_name != central_name || local_name != entry.name_raw() {
        return Err(malformed(format!("local and central names disagree for {:?}", entry.name())));
    }
    let expected_data = local_offset
        .checked_add(30 + local_name_len + local_extra_len)
        .ok_or_else(|| malformed("local header length overflowed"))?;
    if u64::try_from(expected_data).ok() != Some(entry.data_start()) {
        return Err(malformed(format!("local data offset disagrees for {:?}", entry.name())));
    }
    let descriptor = central_flags & 0x0008 != 0;
    if descriptor {
        let local_values = (le32(local, 14)?, le32(local, 18)?, le32(local, 22)?);
        let exact = local_values.0 == central_crc
            && u64::from(local_values.1) == central_compressed
            && u64::from(local_values.2) == central_expanded;
        if local_values != (0, 0, 0) && !exact {
            return Err(malformed(format!(
                "local descriptor placeholders disagree for {:?}",
                entry.name()
            )));
        }
    } else if le32(local, 14)? != central_crc
        || u64::from(le32(local, 18)?) != central_compressed
        || u64::from(le32(local, 22)?) != central_expanded
    {
        return Err(malformed(format!("local sizes/CRC disagree for {:?}", entry.name())));
    }
    let data_end = entry
        .data_start()
        .checked_add(entry.compressed_size())
        .ok_or_else(|| malformed("compressed data range overflowed"))?;
    let end = if descriptor {
        validate_descriptor(bytes, data_end, central_crc, central_compressed, central_expanded)?
    } else {
        data_end
    };
    if end > central_start {
        return Err(malformed(format!("entry {:?} overlaps the central directory", entry.name())));
    }
    Ok((entry.header_start(), end))
}

fn validate_descriptor(
    bytes: &[u8],
    offset: u64,
    crc: u32,
    compressed: u64,
    expanded: u64,
) -> Result<u64, ConversionError> {
    let start = usize::try_from(offset).map_err(|_| malformed("descriptor offset overflowed"))?;
    let signature_end =
        start.checked_add(4).ok_or_else(|| malformed("descriptor signature range overflowed"))?;
    let signature = bytes.get(start..signature_end) == Some(b"PK\x07\x08");
    let base = start
        .checked_add(if signature { 4 } else { 0 })
        .ok_or_else(|| malformed("descriptor body offset overflowed"))?;
    let descriptor = slice(bytes, base, 12, "data descriptor")?;
    if le32(descriptor, 0)? != crc
        || u64::from(le32(descriptor, 4)?) != compressed
        || u64::from(le32(descriptor, 8)?) != expanded
    {
        return Err(malformed("data descriptor disagrees with central metadata"));
    }
    offset
        .checked_add(if signature { 16 } else { 12 })
        .ok_or_else(|| malformed("descriptor range overflowed"))
}

fn slice<'a>(
    bytes: &'a [u8],
    offset: usize,
    length: usize,
    label: &str,
) -> Result<&'a [u8], ConversionError> {
    bytes
        .get(offset..offset.saturating_add(length))
        .ok_or_else(|| malformed(format!("{label} is truncated")))
}

fn le16(bytes: &[u8], offset: usize) -> Result<u16, ConversionError> {
    let value: [u8; 2] =
        slice(bytes, offset, 2, "u16")?.try_into().map_err(|_| malformed("invalid u16"))?;
    Ok(u16::from_le_bytes(value))
}

fn le32(bytes: &[u8], offset: usize) -> Result<u32, ConversionError> {
    let value: [u8; 4] =
        slice(bytes, offset, 4, "u32")?.try_into().map_err(|_| malformed("invalid u32"))?;
    Ok(u32::from_le_bytes(value))
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: None, detail: format!("ZIP: {}", detail.into()) }
}
