//! Narrow, allocation-gated ZIP access shared by package converters.
//!
//! The third-party ZIP reader remains unreachable until `raw_central` has
//! validated the complete physical inventory and `EntryPolicy` has proved a
//! portable, alias-free namespace.

use super::archive::{Archive, EntryMeta};
use super::budget::ArchiveBudget;
use super::entry_policy::EntryKind;
use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, ResourceReservation,
};

/// Validated metadata exposed without leaking the underlying ZIP reader.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryInfo<'a> {
    pub(crate) path: &'a str,
    pub(crate) directory: bool,
    pub(crate) stored: bool,
    pub(crate) compressed_size: u64,
    pub(crate) expanded_size: u64,
    pub(crate) physical_start: usize,
    pub(crate) central_extra_len: usize,
    pub(crate) local_extra_len: usize,
}

/// One decompressed member with its live request-scoped memory charge.
pub(crate) struct OwnedEntry {
    pub(crate) bytes: Vec<u8>,
    memory: ResourceReservation,
}

impl OwnedEntry {
    /// Transfer the retained bytes and their authenticated memory owner.
    pub(crate) fn into_parts(self) -> (Vec<u8>, ResourceReservation) {
        (self.bytes, self.memory)
    }
}

/// A safe, in-memory archive view using one budget for the complete package.
pub(crate) struct SafeArchive<'bytes, 'request> {
    inner: Archive<'bytes>,
    entries: Vec<EntryMeta>,
    budget: ArchiveBudget<'request>,
}

impl<'bytes, 'request> SafeArchive<'bytes, 'request> {
    pub(crate) fn open(
        bytes: &'bytes [u8],
        options: &'request ConversionOptions,
        context: &'request ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let mut budget = ArchiveBudget::new(options, context);
        let mut inner = Archive::open(bytes, 1, &mut budget)?;
        let entries = inner.take_entries();
        Ok(Self { inner, entries, budget })
    }

    pub(crate) fn contains(&self, path: &str) -> bool {
        self.meta(path).is_some_and(|entry| entry.kind == EntryKind::File)
    }

    pub(crate) fn info(&self, path: &str) -> Option<EntryInfo<'_>> {
        self.meta(path).map(entry_info)
    }

    pub(crate) fn first_physical_entry(&self) -> Option<EntryInfo<'_>> {
        self.entries.iter().min_by_key(|entry| entry.physical_start).map(entry_info)
    }

    pub(crate) fn read(&mut self, path: &str) -> Result<OwnedEntry, ConversionError> {
        let meta = self.meta(path).cloned().ok_or_else(|| ConversionError::Malformed {
            part: Some(path.into()),
            detail: format!("EPUB package part {path:?} is missing"),
        })?;
        if meta.kind != EntryKind::File {
            return Err(ConversionError::Malformed {
                part: Some(path.into()),
                detail: format!("EPUB package part {path:?} is a directory"),
            });
        }
        let entry = self.inner.read_entry(&meta, &mut self.budget)?;
        let (bytes, memory) = entry.into_parts();
        Ok(OwnedEntry { bytes, memory })
    }

    fn meta(&self, path: &str) -> Option<&EntryMeta> {
        self.entries
            .binary_search_by(|entry| entry.name.as_str().cmp(path))
            .ok()
            .and_then(|index| self.entries.get(index))
    }
}

fn entry_info(entry: &EntryMeta) -> EntryInfo<'_> {
    EntryInfo {
        path: &entry.name,
        directory: entry.kind == EntryKind::Directory,
        stored: !entry.deflated,
        compressed_size: entry.compressed_size,
        expanded_size: entry.expanded_size,
        physical_start: entry.physical_start,
        central_extra_len: entry.central_extra_len,
        local_extra_len: entry.local_extra_len,
    }
}

/// Canonicalize a URI-resolved part through the same portable identity policy
/// used for every raw archive entry.
pub(crate) fn portable_identity(path: &str, directory: bool) -> Result<String, ConversionError> {
    super::entry_policy::portable_identity(path, directory)
}
