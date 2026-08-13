use super::budget::ArchiveBudget;
use super::entry_policy::{EntryKind, EntryPolicy};
use into_markdown_core::{ConversionError, ResourceReservation};
use std::io::{Cursor, Read};

#[derive(Debug, Clone)]
pub(super) struct EntryMeta {
    pub(super) index: usize,
    pub(super) name: String,
    pub(super) kind: EntryKind,
    pub(super) compressed_size: u64,
    pub(super) expanded_size: u64,
}

pub(super) struct EntryData {
    pub(super) bytes: Vec<u8>,
    pub(super) _memory: ResourceReservation,
}

pub(super) struct Archive<'a> {
    inner: zip::ZipArchive<Cursor<&'a [u8]>>,
    entries: Vec<EntryMeta>,
    _metadata_memory: ResourceReservation,
}

impl<'a> Archive<'a> {
    pub(super) fn open(
        bytes: &'a [u8],
        depth: u16,
        budget: &mut ArchiveBudget<'_>,
    ) -> Result<Self, ConversionError> {
        budget.context().checkpoint()?;
        let mut inner = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|error| malformed(format!("invalid ZIP structure: {error}")))?;
        budget.enter_archive(depth, inner.len())?;
        let fixed = u64::try_from(inner.len())
            .unwrap_or(u64::MAX)
            .checked_mul(256)
            .ok_or_else(|| memory_limit("ZIP metadata size overflowed"))?;
        let mut memory = budget.context().reserve_memory(fixed)?;
        let mut policy = EntryPolicy::default();
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(inner.len())
            .map_err(|error| memory_limit(format!("reserve ZIP metadata: {error}")))?;
        let central_start = find_central_start(&mut inner)?;
        let mut occupied = Vec::new();
        occupied
            .try_reserve_exact(inner.len())
            .map_err(|error| memory_limit(format!("reserve ZIP intervals: {error}")))?;

        for index in 0..inner.len() {
            budget.context().checkpoint()?;
            let entry = inner
                .by_index_raw(index)
                .map_err(|error| malformed(format!("cannot inspect entry {index}: {error}")))?;
            if entry.encrypted() {
                return Err(ConversionError::Encrypted);
            }
            if !matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            ) {
                return Err(malformed(format!(
                    "entry {:?} uses unsupported compression {}",
                    entry.name(),
                    entry.compression()
                )));
            }
            budget.validate_member(entry.name(), entry.compressed_size(), entry.size())?;
            let name_charge =
                u64::try_from(entry.name_raw().len()).unwrap_or(u64::MAX).saturating_mul(4);
            memory.grow(name_charge)?;
            let (name, kind) =
                policy.accept(entry.name_raw(), entry.name(), entry.unix_mode(), entry.is_dir())?;
            let range = validate_headers(bytes, &entry, central_start)?;
            occupied.push((range.0, range.1, name.clone()));
            entries.push(EntryMeta {
                index,
                name,
                kind,
                compressed_size: entry.compressed_size(),
                expanded_size: entry.size(),
            });
        }
        occupied.sort_by_key(|interval| interval.0);
        for pair in occupied.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err(malformed(format!(
                    "entry data ranges overlap: {:?} and {:?}",
                    pair[0].2, pair[1].2
                )));
            }
        }
        entries
            .sort_by(|left, right| left.name.cmp(&right.name).then(left.index.cmp(&right.index)));
        Ok(Self { inner, entries, _metadata_memory: memory })
    }

    pub(super) fn entries(&self) -> &[EntryMeta] {
        &self.entries
    }

    pub(super) fn read_entry(
        &mut self,
        meta: &EntryMeta,
        budget: &mut ArchiveBudget<'_>,
    ) -> Result<EntryData, ConversionError> {
        budget.validate_member(&meta.name, meta.compressed_size, meta.expanded_size)?;
        budget.charge_expanded(&meta.name, meta.expanded_size)?;
        let mut memory = budget.context().reserve_memory(meta.expanded_size)?;
        let capacity =
            usize::try_from(meta.expanded_size).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_archive_entry_bytes",
                detail: format!("archive member {} does not fit address space", meta.name),
            })?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|error| {
            memory_limit(format!("reserve archive member {}: {error}", meta.name))
        })?;
        let actual_capacity = u64::try_from(bytes.capacity()).unwrap_or(u64::MAX);
        if actual_capacity > meta.expanded_size {
            memory.grow(actual_capacity - meta.expanded_size)?;
        }
        bytes.resize(capacity, 0);
        let mut entry = self.inner.by_index(meta.index).map_err(|error| {
            malformed(format!("cannot open archive member {:?}: {error}", meta.name))
        })?;
        let mut offset = 0;
        while offset < bytes.len() {
            budget.context().checkpoint()?;
            let end = (offset + 64 * 1024).min(bytes.len());
            entry.read_exact(&mut bytes[offset..end]).map_err(|error| {
                malformed(format!("cannot decompress archive member {:?}: {error}", meta.name))
            })?;
            offset = end;
        }
        budget.context().checkpoint()?;
        let mut extra = [0_u8; 1];
        let extra_read = entry.read(&mut extra).map_err(|error| {
            malformed(format!(
                "archive member {:?} failed CRC/length validation: {error}",
                meta.name
            ))
        })?;
        if extra_read != 0 {
            return Err(malformed(format!(
                "archive member {:?} expands beyond its declared size",
                meta.name
            )));
        }
        Ok(EntryData { bytes, _memory: memory })
    }
}

fn find_central_start(
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
        let offset = archive.offset();
        return Ok(offset);
    }
    Ok(start)
}

fn validate_headers(
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
    if central_flags & 0x0041 != 0 {
        return Err(ConversionError::Encrypted);
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
    let central_name = slice(bytes, central_offset + 46, central_name_len, "central name")?;
    let local_name = slice(bytes, local_offset + 30, local_name_len, "local name")?;
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
    if !descriptor {
        if le32(local, 14)? != central_crc
            || u64::from(le32(local, 18)?) != central_compressed
            || u64::from(le32(local, 22)?) != central_expanded
        {
            return Err(malformed(format!("local sizes/CRC disagree for {:?}", entry.name())));
        }
    } else {
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
    let signature = bytes.get(start..start + 4) == Some(b"PK\x07\x08");
    let base = start + if signature { 4 } else { 0 };
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

fn memory_limit(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}
