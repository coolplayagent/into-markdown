mod binary;
mod chain;
mod directory;
mod open;
mod ownership;
mod stream;

use super::budget::{limit, malformed};
use directory::{DirectoryEntry, stable_path};
use into_markdown_core::{ConversionError, ResourceReservation};

const SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const FREE: u32 = 0xffff_ffff;
const END: u32 = 0xffff_fffe;
const FAT: u32 = 0xffff_fffd;
const DIFAT: u32 = 0xffff_fffc;
const NONE: u32 = 0xffff_ffff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Storage,
    Stream,
    Root,
}

#[derive(Debug)]
pub(crate) struct CompoundFile {
    pub(in crate::msg::ole) entries: Vec<DirectoryEntry>,
    pub(in crate::msg::ole) streams: Vec<Option<Vec<u8>>>,
    pub(in crate::msg::ole) recoveries: CompoundRecoveries,
    pub(in crate::msg::ole) _memory: Vec<ResourceReservation>,
}

/// Narrow compatibility recoveries for safely addressable legacy Office CFB files.
///
/// The strict reader remains the default for MSG and all audit-oriented callers.  The
/// best-effort policy only relaxes redundant metadata; chain bounds, ownership, cycles,
/// duplicate names, and resource accounting are identical in both modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompoundCompatibility {
    Strict,
    LegacyOfficeBestEffort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CompoundRecovery {
    TrailingFileBytes,
    FatSectorMarker,
    UnreachableFatTarget,
    DirectoryNameTerminator,
    RootStorageName,
    StorageStreamMetadata,
    StreamChainTail,
    PartialStreamSector,
}

impl CompoundRecovery {
    const ALL: [Self; 8] = [
        Self::TrailingFileBytes,
        Self::FatSectorMarker,
        Self::UnreachableFatTarget,
        Self::DirectoryNameTerminator,
        Self::RootStorageName,
        Self::StorageStreamMetadata,
        Self::StreamChainTail,
        Self::PartialStreamSector,
    ];
}

#[derive(Debug, Default)]
pub(super) struct CompoundRecoveries(u16);

impl CompoundRecoveries {
    pub(super) fn insert(&mut self, recovery: CompoundRecovery) {
        self.0 |= 1_u16 << recovery as u8;
    }

    pub(super) fn remove(&mut self, recovery: CompoundRecovery) {
        self.0 &= !(1_u16 << recovery as u8);
    }

    fn iter(&self) -> impl Iterator<Item = CompoundRecovery> + '_ {
        CompoundRecovery::ALL.into_iter().filter(|recovery| self.contains(*recovery))
    }

    fn contains(&self, recovery: CompoundRecovery) -> bool {
        self.0 & (1_u16 << recovery as u8) != 0
    }
}

pub(super) struct CompoundMemory {
    leases: Vec<ResourceReservation>,
}

impl CompoundMemory {
    pub(super) fn new<B: CompoundBudget + ?Sized>(
        initial_bytes: u64,
        budget: &B,
    ) -> Result<Self, ConversionError> {
        let mut leases = Vec::new();
        if initial_bytes != 0 {
            leases.push(budget.cfb_memory(initial_bytes)?);
        }
        Ok(Self { leases })
    }

    pub(super) fn grow<B: CompoundBudget + ?Sized>(
        &mut self,
        bytes: u64,
        budget: &B,
    ) -> Result<(), ConversionError> {
        if bytes != 0 {
            self.leases.push(budget.cfb_memory(bytes)?);
        }
        Ok(())
    }

    pub(super) fn into_leases(self) -> Vec<ResourceReservation> {
        self.leases
    }
}

impl CompoundFile {
    pub(crate) fn open<B: CompoundBudget + ?Sized>(
        bytes: &[u8],
        budget: &mut B,
    ) -> Result<Self, ConversionError> {
        Self::open_with_compatibility(bytes, budget, CompoundCompatibility::Strict)
    }

    pub(crate) fn open_with_compatibility<B: CompoundBudget + ?Sized>(
        bytes: &[u8],
        budget: &mut B,
        compatibility: CompoundCompatibility,
    ) -> Result<Self, ConversionError> {
        open::open(bytes, budget, compatibility)
    }

    pub(crate) fn root(&self) -> Storage<'_> {
        Storage { file: self, index: 0 }
    }

    pub(crate) fn recoveries(&self) -> impl Iterator<Item = CompoundRecovery> + '_ {
        self.recoveries.iter()
    }

    fn children(&self, parent: usize) -> impl Iterator<Item = (usize, &DirectoryEntry)> {
        self.entries.iter().enumerate().filter(move |(_, entry)| entry.parent == Some(parent))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Storage<'a> {
    file: &'a CompoundFile,
    index: usize,
}

impl<'a> Storage<'a> {
    pub(crate) fn path(&self) -> String {
        stable_path(&self.file.entries, self.index)
    }

    pub(crate) fn stream(&self, name: &str) -> Option<&'a [u8]> {
        self.file
            .children(self.index)
            .find(|(_, entry)| {
                entry.kind == EntryKind::Stream && entry.name.eq_ignore_ascii_case(name)
            })
            .and_then(|(index, _)| self.file.streams.get(index)?.as_deref())
    }

    pub(crate) fn storages(&self) -> impl Iterator<Item = Storage<'a>> + 'a {
        self.file.children(self.index).filter_map(|(index, entry)| {
            (entry.kind == EntryKind::Storage).then_some(Storage { file: self.file, index })
        })
    }

    pub(crate) fn storage(&self, name: &str) -> Option<Storage<'a>> {
        self.storages()
            .find(|storage| storage.file.entries[storage.index].name.eq_ignore_ascii_case(name))
    }

    pub(crate) fn name(&self) -> &'a str {
        &self.file.entries[self.index].name
    }
}

/// Resource accounting required by the shared CFB/OLE reader.
///
/// Format frontends retain ownership of their public limit names and diagnostics while the
/// container reader enforces one audited sector/directory implementation.
pub(crate) trait CompoundBudget {
    fn cfb_memory(&self, bytes: u64) -> Result<ResourceReservation, ConversionError>;
    fn cfb_entry(&mut self) -> Result<(), ConversionError>;
    fn cfb_expanded(&mut self, bytes: u64) -> Result<(), ConversionError>;
    fn cfb_depth(&self, depth: u16, part: &str) -> Result<(), ConversionError>;
    fn cfb_work(&mut self, units: u64) -> Result<(), ConversionError>;
}

#[cfg(test)]
mod tests;
