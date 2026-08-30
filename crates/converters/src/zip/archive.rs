use super::budget::ArchiveBudget;
use super::entry_policy::EntryKind;
use super::raw_central::RawInventory;
use into_markdown_core::{ConversionError, ResourceReservation};
use std::io::{Cursor, Read};

const VALIDATION_SCRATCH_BYTES: usize = 64 * 1024;
const VALIDATION_SCRATCH_MEMORY_BYTES: u64 = 64 * 1024;
const DEFLATE_WORKSPACE_BYTES: u64 = 256 * 1024;

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
    pub(super) physical_start: usize,
    pub(super) central_extra_len: usize,
    pub(super) local_extra_len: usize,
    pub(super) verified: bool,
}

pub(super) struct EntryData {
    pub(super) bytes: Vec<u8>,
    memory: ResourceReservation,
}

impl EntryData {
    pub(super) fn into_parts(self) -> (Vec<u8>, ResourceReservation) {
        (self.bytes, self.memory)
    }
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
        Ok(EntryData { bytes, memory })
    }

    /// Stream one member through the decoder so length and CRC are checked
    /// without retaining its expanded bytes.
    pub(super) fn validate_entry(
        &mut self,
        meta: &EntryMeta,
        budget: &mut ArchiveBudget<'_>,
    ) -> Result<(), ConversionError> {
        budget.validate_member(&meta.name, meta.compressed_size, meta.expanded_size)?;
        budget.charge_expanded(&meta.name, meta.expanded_size)?;
        let working_bytes = VALIDATION_SCRATCH_MEMORY_BYTES
            + if meta.deflated { DEFLATE_WORKSPACE_BYTES } else { 0 };
        let mut working_memory = budget.context().reserve_memory(working_bytes)?;
        let mut scratch = Vec::new();
        scratch.try_reserve_exact(VALIDATION_SCRATCH_BYTES).map_err(|error| {
            memory_limit(format!("reserve ZIP validation scratch buffer: {error}"))
        })?;
        let actual_scratch = u64::try_from(scratch.capacity()).unwrap_or(u64::MAX);
        if actual_scratch > VALIDATION_SCRATCH_MEMORY_BYTES {
            working_memory.grow(actual_scratch - VALIDATION_SCRATCH_MEMORY_BYTES)?;
        } else {
            working_memory.shrink(VALIDATION_SCRATCH_MEMORY_BYTES - actual_scratch)?;
        }
        scratch.resize(VALIDATION_SCRATCH_BYTES, 0);
        let mut entry = self.inner.by_index(meta.index).map_err(|error| {
            malformed(format!("cannot open archive member {:?}: {error}", meta.name))
        })?;
        let mut expanded = 0_u64;
        loop {
            budget.context().checkpoint()?;
            let read = entry.read(&mut scratch).map_err(|error| {
                malformed(format!(
                    "archive member {:?} failed CRC/length validation: {error}",
                    meta.name
                ))
            })?;
            if read == 0 {
                break;
            }
            expanded = expanded
                .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
                .ok_or_else(|| malformed("archive member expanded length overflowed"))?;
            if expanded > meta.expanded_size {
                return Err(malformed(format!(
                    "archive member {:?} expands beyond its declared size",
                    meta.name
                )));
            }
        }
        if expanded != meta.expanded_size {
            return Err(malformed(format!(
                "archive member {:?} expanded to {expanded} bytes, expected {}",
                meta.name, meta.expanded_size
            )));
        }
        Ok(())
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
