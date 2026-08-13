use super::budget::ArchiveBudget;
use super::entry_policy::EntryKind;
use super::raw_central::RawInventory;
use into_markdown_core::{ConversionError, ResourceReservation};
use std::io::{Cursor, Read};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static CONSTRUCTOR_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[derive(Debug, Clone)]
pub(super) struct EntryMeta {
    pub(super) index: usize,
    pub(super) name: String,
    pub(super) kind: EntryKind,
    pub(super) compressed_size: u64,
    pub(super) expanded_size: u64,
    pub(super) deflated: bool,
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
        let RawInventory { entries, memory } = super::raw_central::preflight(bytes, depth, budget)?;
        #[cfg(test)]
        CONSTRUCTOR_CALLS.with(|calls| calls.set(calls.get() + 1));
        let inner = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|error| malformed(format!("invalid ZIP structure: {error}")))?;
        if inner.len() != entries.len() {
            return Err(malformed(
                "third-party ZIP inventory disagrees with the validated raw central directory",
            ));
        }
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

#[cfg(test)]
pub(super) fn reset_constructor_calls() {
    CONSTRUCTOR_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(super) fn constructor_calls() -> usize {
    CONSTRUCTOR_CALLS.with(Cell::get)
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: None, detail: format!("ZIP: {}", detail.into()) }
}

fn memory_limit(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}
