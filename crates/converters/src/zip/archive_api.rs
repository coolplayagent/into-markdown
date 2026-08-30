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
        let index =
            self.entries.binary_search_by(|entry| entry.name.as_str().cmp(path)).map_err(|_| {
                ConversionError::Malformed {
                    part: Some(path.into()),
                    detail: format!("EPUB package part {path:?} is missing"),
                }
            })?;
        if self.entries[index].verified {
            return Err(ConversionError::Internal {
                detail: format!("validated archive member {path:?} was requested more than once"),
            });
        }
        let meta = self.entries[index].clone();
        if meta.kind != EntryKind::File {
            return Err(ConversionError::Malformed {
                part: Some(path.into()),
                detail: format!("EPUB package part {path:?} is a directory"),
            });
        }
        let entry = self.inner.read_entry(&meta, &mut self.budget)?;
        self.entries[index].verified = true;
        let (bytes, memory) = entry.into_parts();
        Ok(OwnedEntry { bytes, memory })
    }

    /// Stream every unread file through the decoder so CRC and declared length
    /// are validated without retaining unused package members.
    pub(crate) fn validate_remaining(&mut self) -> Result<(), ConversionError> {
        for index in 0..self.entries.len() {
            if self.entries[index].kind != EntryKind::File || self.entries[index].verified {
                continue;
            }
            let meta = self.entries[index].clone();
            self.inner.validate_entry(&meta, &mut self.budget)?;
            self.entries[index].verified = true;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};
    use std::io::{Cursor, Write as _};
    use zip::write::SimpleFileOptions;

    fn stored(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn remaining_crc_is_streamed_without_recharging_previously_read_entries() {
        let first = b"already-read";
        let second = b"unused-content";
        let bytes = stored(&[("a.txt", first), ("unused.bin", second)]);
        let mut options = ConversionOptions::default();
        options.limits.max_decompressed_bytes = (first.len() + second.len()) as u64;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut archive = SafeArchive::open(&bytes, &options, &context).unwrap();
        let metadata_memory = context.reserved_memory_bytes();
        drop(archive.read("a.txt").unwrap());
        assert!(matches!(archive.read("a.txt"), Err(ConversionError::Internal { .. })));
        archive.validate_remaining().unwrap();
        assert_eq!(context.reserved_memory_bytes(), metadata_memory);
        assert_eq!(context.reserved_temporary_bytes(), 0);
        drop(archive);
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.reserved_temporary_bytes(), 0);

        let mut corrupt = bytes;
        let offset = corrupt.windows(second.len()).position(|window| window == second).unwrap();
        corrupt[offset] ^= 1;
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let mut archive = SafeArchive::open(&corrupt, &options, &context).unwrap();
        drop(archive.read("a.txt").unwrap());
        let error = archive.validate_remaining().unwrap_err();
        assert!(matches!(error, ConversionError::Malformed { .. }));
        assert!(error.to_string().contains("CRC/length"));
        drop(archive);
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }
}
