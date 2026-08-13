use super::budget::ArchiveBudget;
use super::entry_policy::{EntryKind, EntryPolicy};
use super::headers::{find_central_start, validate_headers};
use into_markdown_core::{ConversionError, ResourceReservation};
use std::io::{Cursor, Read};

#[derive(Debug, Clone)]
pub(super) struct EntryMeta {
    pub(super) index: usize,
    pub(super) name: String,
    pub(super) kind: EntryKind,
    pub(super) compressed_size: u64,
    pub(super) expanded_size: u64,
    deflated: bool,
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
            // Canonical name, stable-sort copy, alias/prefix sets, component
            // inventory, and validation scratch coexist at policy peak.
            let name_charge =
                u64::try_from(entry.name_raw().len()).unwrap_or(u64::MAX).saturating_mul(16);
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
                deflated: entry.compression() == zip::CompressionMethod::Deflated,
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

    /// Move the validated, stably sorted inventory into the recursive walker.
    /// This avoids cloning the complete attacker-controlled name inventory.
    pub(super) fn take_entries(&mut self) -> Vec<EntryMeta> {
        std::mem::take(&mut self.entries)
    }

    pub(super) fn read_entry(
        &mut self,
        meta: &EntryMeta,
        budget: &mut ArchiveBudget<'_>,
    ) -> Result<EntryData, ConversionError> {
        budget.validate_member(&meta.name, meta.compressed_size, meta.expanded_size)?;
        budget.charge_expanded(&meta.name, meta.expanded_size)?;
        let mut memory = budget.context().reserve_memory(meta.expanded_size)?;
        // miniz/flate state is implementation-owned; reserve a conservative
        // bounded window before constructing the decoder.
        let _decoder_memory =
            budget.context().reserve_memory(if meta.deflated { 256 * 1024 } else { 0 })?;
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

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: None, detail: format!("ZIP: {}", detail.into()) }
}

fn memory_limit(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}
