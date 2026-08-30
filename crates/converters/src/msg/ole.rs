mod chain;

use super::budget::{limit, malformed};
use chain::{walk_chain, walk_chain_with_declared_tail};
use into_markdown_core::{ConversionError, ResourceReservation};
use std::collections::{BTreeMap, BTreeSet};

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
struct DirectoryEntry {
    name: String,
    kind: EntryKind,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
    parent: Option<usize>,
}

#[derive(Debug)]
pub(crate) struct CompoundFile {
    entries: Vec<DirectoryEntry>,
    streams: BTreeMap<usize, Vec<u8>>,
    recoveries: BTreeSet<CompoundRecovery>,
    _memory: Option<ResourceReservation>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
        Header::parse(bytes)?.open(bytes, budget, compatibility)
    }

    pub(crate) fn root(&self) -> Storage<'_> {
        Storage { file: self, index: 0 }
    }

    pub(crate) fn recoveries(&self) -> impl Iterator<Item = CompoundRecovery> + '_ {
        self.recoveries.iter().copied()
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
            .and_then(|(index, _)| self.file.streams.get(&index).map(Vec::as_slice))
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

#[derive(Clone, Copy)]
struct Header {
    major: u16,
    sector_size: usize,
    mini_sector_size: usize,
    directory_sectors: u32,
    fat_sectors: u32,
    first_directory: u32,
    mini_cutoff: u32,
    first_minifat: u32,
    minifat_sectors: u32,
    first_difat: u32,
    difat_sectors: u32,
}

impl Header {
    fn parse(bytes: &[u8]) -> Result<Self, ConversionError> {
        let header =
            bytes.get(..512).ok_or_else(|| malformed("cfb/header", "truncated CFB header"))?;
        if header[..8] != SIGNATURE {
            return Err(malformed("cfb/header", "invalid CFB signature"));
        }
        if header[8..24].iter().any(|byte| *byte != 0) {
            return Err(malformed("cfb/header", "non-zero CFB header CLSID"));
        }
        let major = le16(header, 26, "cfb/header")?;
        if le16(header, 28, "cfb/header")? != 0xfffe {
            return Err(malformed("cfb/header", "unsupported CFB byte order"));
        }
        let sector_shift = le16(header, 30, "cfb/header")?;
        if !matches!((major, sector_shift), (3, 9) | (4, 12)) {
            return Err(malformed("cfb/header", "inconsistent CFB version and sector shift"));
        }
        if le16(header, 32, "cfb/header")? != 6 {
            return Err(malformed("cfb/header", "unsupported CFB mini-sector shift"));
        }
        if header[34..40].iter().any(|byte| *byte != 0) {
            return Err(malformed("cfb/header", "non-zero reserved CFB header bytes"));
        }
        let directory_sectors = le32(header, 40, "cfb/header")?;
        if major == 3 && directory_sectors != 0 {
            return Err(malformed("cfb/header", "version 3 CFB declares directory sector count"));
        }
        let mini_cutoff = le32(header, 56, "cfb/header")?;
        if mini_cutoff != 4096 {
            return Err(malformed("cfb/header", "unsupported CFB mini-stream cutoff"));
        }
        Ok(Self {
            major,
            sector_size: 1_usize << sector_shift,
            mini_sector_size: 64,
            directory_sectors,
            fat_sectors: le32(header, 44, "cfb/header")?,
            first_directory: le32(header, 48, "cfb/header")?,
            mini_cutoff,
            first_minifat: le32(header, 60, "cfb/header")?,
            minifat_sectors: le32(header, 64, "cfb/header")?,
            first_difat: le32(header, 68, "cfb/header")?,
            difat_sectors: le32(header, 72, "cfb/header")?,
        })
    }

    #[allow(clippy::too_many_lines)] // Sector ownership stays adjacent to every chain read.
    fn open<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        budget: &mut B,
        compatibility: CompoundCompatibility,
    ) -> Result<CompoundFile, ConversionError> {
        if bytes.len() < self.sector_size {
            return Err(malformed("cfb/header", "CFB file length is not sector aligned"));
        }
        let trailing_recovery = !bytes.len().is_multiple_of(self.sector_size);
        if trailing_recovery && compatibility == CompoundCompatibility::Strict {
            return Err(malformed("cfb/header", "CFB file length is not sector aligned"));
        }
        if self.major == 4 && bytes[512..self.sector_size].iter().any(|byte| *byte != 0) {
            return Err(malformed("cfb/header", "non-zero version 4 header padding"));
        }
        let sector_count = bytes.len() / self.sector_size - 1;
        let partial_stream_sector = (compatibility
            == CompoundCompatibility::LegacyOfficeBestEffort
            && !bytes.len().is_multiple_of(self.sector_size))
        .then_some(sector_count);
        let stream_sector_count = sector_count + usize::from(partial_stream_sector.is_some());
        // Hold one authenticated lifetime lease before the first attacker-sized allocation.
        // The plan covers the retained FAT/miniFAT, directory inventory, root mini-stream,
        // non-overlapping stream payloads and their bounded phase scratch. Directory ancestry is
        // stored by parent index, so it cannot grow quadratically with nesting depth.
        let memory = (compatibility == CompoundCompatibility::LegacyOfficeBestEffort)
            .then(|| {
                let memory_plan = cfb_memory_plan(bytes.len(), stream_sector_count)?;
                budget.cfb_memory(memory_plan)
            })
            .transpose()?;
        let mut recoveries = BTreeSet::new();
        if trailing_recovery {
            // Sector identifiers can only address complete sectors. A short physical tail is
            // unreachable by every authenticated chain and can therefore be ignored safely.
            recoveries.insert(CompoundRecovery::TrailingFileBytes);
        }
        let mut owners = try_sized_vec(stream_sector_count, false, "CFB sector owner table")?;
        let (fat_sector_ids, difat_sector_ids) = self.read_difat(bytes, sector_count, budget)?;
        for id in &difat_sector_ids {
            claim(&mut owners, *id, "cfb/difat")?;
        }
        for id in &fat_sector_ids {
            claim(&mut owners, *id, "cfb/fat")?;
        }
        let fat_capacity = fat_sector_ids
            .len()
            .checked_mul(self.sector_size / 4)
            .ok_or_else(|| limit("max_decompressed_bytes", "CFB FAT capacity overflowed"))?;
        let mut fat = try_vec_capacity(fat_capacity, "CFB FAT")?;
        for id in &fat_sector_ids {
            fat.extend(
                read_sector(bytes, self.sector_size, *id)?
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])),
            );
        }
        if fat.len() < sector_count {
            return Err(malformed("cfb/fat", "FAT does not address every physical sector"));
        }
        for id in &fat_sector_ids {
            if fat.get(to_usize(*id)?).copied() != Some(FAT) {
                if compatibility == CompoundCompatibility::Strict {
                    return Err(malformed("cfb/fat", "FAT sector is not marked FATSECT"));
                }
                // DIFAT is the authoritative, bounded list of FAT sectors.  `claim` below still
                // prevents any stream or metadata chain from reusing this physical sector.
                recoveries.insert(CompoundRecovery::FatSectorMarker);
            }
        }
        for id in &difat_sector_ids {
            if fat.get(to_usize(*id)?).copied() != Some(DIFAT) {
                return Err(malformed("cfb/difat", "DIFAT sector is not marked DIFSECT"));
            }
        }
        if compatibility == CompoundCompatibility::Strict {
            validate_fat_targets(&fat[..sector_count], sector_count)?;
        } else if fat[..sector_count]
            .iter()
            .any(|value| fat_target_is_out_of_bounds(*value, sector_count))
        {
            // Every reachable chain is still walked with `sector_count` and fails on an invalid
            // transition.  Stale targets belonging only to unreachable sectors are inert.
            recoveries.insert(CompoundRecovery::UnreachableFatTarget);
        }

        let directory_expected = (self.major == 4).then_some(self.directory_sectors);
        let directory_chain = walk_chain(
            self.first_directory,
            &fat,
            stream_sector_count,
            directory_expected,
            "cfb/directory",
        )?;
        if directory_chain.is_empty() {
            return Err(malformed("cfb/directory", "CFB has no directory sector"));
        }
        for id in &directory_chain {
            claim(&mut owners, *id, "cfb/directory")?;
        }
        let directory_bytes = concatenate(bytes, self.sector_size, &directory_chain)?;
        let mut entries =
            parse_directory(&directory_bytes, self.major, budget, compatibility, &mut recoveries)?;
        assign_paths(&mut entries, budget)?;

        let minifat_chain = if self.minifat_sectors == 0 {
            if !matches!(self.first_minifat, END | FREE) {
                return Err(malformed("cfb/minifat", "empty miniFAT has a start sector"));
            }
            Vec::new()
        } else {
            walk_chain(
                self.first_minifat,
                &fat,
                sector_count,
                Some(self.minifat_sectors),
                "cfb/minifat",
            )?
        };
        for id in &minifat_chain {
            claim(&mut owners, *id, "cfb/minifat")?;
        }
        let minifat_bytes = concatenate(bytes, self.sector_size, &minifat_chain)?;
        let mut minifat = try_vec_capacity(minifat_bytes.len() / 4, "CFB miniFAT")?;
        minifat.extend(
            minifat_bytes
                .chunks_exact(4)
                .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])),
        );

        let root =
            entries.first().ok_or_else(|| malformed("cfb/directory", "missing root entry"))?;
        let root_chain = regular_stream_chain(
            root,
            &fat,
            sector_count,
            self.sector_size,
            "cfb/root",
            compatibility,
            &mut recoveries,
        )?;
        for id in &root_chain {
            claim(&mut owners, *id, "cfb/root-mini-stream")?;
        }
        let (root_mini_stream, root_partial_tail_consumed) = materialize_regular_stream(
            bytes,
            self.sector_size,
            &root_chain,
            root.size,
            partial_stream_sector,
            "cfb/root",
            budget,
        )?;
        if let Some(consumed_all_tail) = root_partial_tail_consumed {
            if consumed_all_tail {
                recoveries.remove(&CompoundRecovery::TrailingFileBytes);
            }
            recoveries.insert(CompoundRecovery::PartialStreamSector);
        }
        let mut mini_owners = try_sized_vec(
            root_mini_stream.len().div_ceil(self.mini_sector_size),
            false,
            "CFB mini-sector owner table",
        )?;
        let mut streams = BTreeMap::new();
        for (index, entry) in
            entries.iter().enumerate().filter(|(_, entry)| entry.kind == EntryKind::Stream)
        {
            budget.cfb_expanded(entry.size)?;
            let part = stable_path(&entries, index);
            let data = if entry.size < u64::from(self.mini_cutoff) {
                let mut mini_context = MiniStreamContext {
                    minifat: &minifat,
                    root: &root_mini_stream,
                    owners: &mut mini_owners,
                    mini_size: self.mini_sector_size,
                    compatibility,
                    recoveries: &mut recoveries,
                };
                read_mini_stream(entry, &part, &mut mini_context)?
            } else {
                let chain = regular_stream_chain(
                    entry,
                    &fat,
                    stream_sector_count,
                    self.sector_size,
                    &part,
                    compatibility,
                    &mut recoveries,
                )?;
                for id in &chain {
                    claim(&mut owners, *id, &part)?;
                }
                let (data, partial_tail_consumed) = concatenate_regular_stream(
                    bytes,
                    self.sector_size,
                    &chain,
                    entry.size,
                    partial_stream_sector,
                    &part,
                )?;
                if let Some(consumed_all_tail) = partial_tail_consumed {
                    if consumed_all_tail {
                        recoveries.remove(&CompoundRecovery::TrailingFileBytes);
                    }
                    recoveries.insert(CompoundRecovery::PartialStreamSector);
                }
                data
            };
            streams.insert(index, data);
        }
        Ok(CompoundFile { entries, streams, recoveries, _memory: memory })
    }

    fn read_difat<B: CompoundBudget + ?Sized>(
        self,
        bytes: &[u8],
        sector_count: usize,
        budget: &mut B,
    ) -> Result<(Vec<u32>, Vec<u32>), ConversionError> {
        let mut fat_ids = Vec::new();
        for offset in (76..512).step_by(4) {
            let id = le32(bytes, offset, "cfb/difat")?;
            if id != FREE {
                validate_physical(id, sector_count, "cfb/difat")?;
                fat_ids.push(id);
            }
        }
        let mut difat_ids = Vec::new();
        let mut difat_seen = BTreeSet::new();
        let mut current = self.first_difat;
        for _ in 0..self.difat_sectors {
            budget.cfb_work(1)?;
            validate_physical(current, sector_count, "cfb/difat")?;
            if !difat_seen.insert(current) {
                return Err(malformed("cfb/difat", "DIFAT chain contains a cycle"));
            }
            difat_ids.push(current);
            let sector = read_sector(bytes, self.sector_size, current)?;
            for chunk in sector[..self.sector_size - 4].chunks_exact(4) {
                let id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                if id != FREE {
                    validate_physical(id, sector_count, "cfb/difat")?;
                    fat_ids.push(id);
                }
            }
            current = le32(sector, self.sector_size - 4, "cfb/difat")?;
        }
        if self.difat_sectors == 0 {
            if !matches!(current, END | FREE) {
                return Err(malformed("cfb/difat", "empty DIFAT chain has a start sector"));
            }
        } else if current != END {
            return Err(malformed("cfb/difat", "DIFAT chain is longer than declared"));
        }
        if fat_ids.len() != to_usize(self.fat_sectors)? {
            return Err(malformed("cfb/difat", "declared FAT sector count does not match DIFAT"));
        }
        let unique = fat_ids.iter().copied().collect::<BTreeSet<_>>();
        if unique.len() != fat_ids.len() {
            return Err(malformed("cfb/difat", "DIFAT repeats a FAT sector"));
        }
        Ok((fat_ids, difat_ids))
    }
}

fn cfb_memory_plan(input_bytes: usize, sector_count: usize) -> Result<u64, ConversionError> {
    let input = u64::try_from(input_bytes).unwrap_or(u64::MAX);
    let sectors = u64::try_from(sector_count).unwrap_or(u64::MAX);
    let entries = input / 128;
    let retained_payloads = input
        .checked_mul(8)
        .ok_or_else(|| limit("max_memory_bytes", "CFB retained-payload memory plan overflowed"))?;
    let directory_inventory = entries
        .checked_mul(
            u64::try_from(std::mem::size_of::<DirectoryEntry>() + 64 + 160).unwrap_or(u64::MAX),
        )
        .ok_or_else(|| limit("max_memory_bytes", "CFB directory memory plan overflowed"))?;
    let chain_inventory = sectors
        .checked_mul(96)
        .ok_or_else(|| limit("max_memory_bytes", "CFB chain memory plan overflowed"))?;
    retained_payloads
        .checked_add(directory_inventory)
        .and_then(|bytes| bytes.checked_add(chain_inventory))
        .and_then(|bytes| bytes.checked_add(64 * 1024))
        .ok_or_else(|| limit("max_memory_bytes", "CFB memory plan overflowed"))
}
fn parse_directory<B: CompoundBudget + ?Sized>(
    bytes: &[u8],
    major: u16,
    budget: &mut B,
    compatibility: CompoundCompatibility,
    recoveries: &mut BTreeSet<CompoundRecovery>,
) -> Result<Vec<DirectoryEntry>, ConversionError> {
    if !bytes.len().is_multiple_of(128) {
        return Err(malformed("cfb/directory", "directory stream is not entry aligned"));
    }
    let mut entries = try_vec_capacity(bytes.len() / 128, "CFB directory entries")?;
    for (index, raw) in bytes.chunks_exact(128).enumerate() {
        budget.cfb_entry()?;
        let object_type = raw[66];
        if object_type == 0 {
            entries.push(DirectoryEntry {
                name: String::new(),
                kind: EntryKind::Stream,
                left: NONE,
                right: NONE,
                child: NONE,
                start: END,
                size: 0,
                parent: None,
            });
            continue;
        }
        let kind = match object_type {
            1 => EntryKind::Storage,
            2 => EntryKind::Stream,
            5 if index == 0 => EntryKind::Root,
            _ => return Err(malformed("cfb/directory", "invalid directory object type")),
        };
        if !matches!(raw[67], 0 | 1) {
            return Err(malformed("cfb/directory", "invalid red-black directory color"));
        }
        let name = parse_directory_name(raw, compatibility, recoveries)?;
        let child = le32(raw, 76, "cfb/directory")?;
        if kind == EntryKind::Stream && child != NONE {
            return Err(malformed("cfb/directory", "stream directory entry has children"));
        }
        let raw_size = le64(raw, 120, "cfb/directory")?;
        let size = if major == 3 { raw_size & u64::from(u32::MAX) } else { raw_size };
        let start = le32(raw, 116, "cfb/directory")?;
        if kind == EntryKind::Storage && (size != 0 || !matches!(start, END | FREE)) {
            if compatibility == CompoundCompatibility::Strict {
                return Err(malformed(
                    "cfb/directory",
                    "storage entry declares stream sectors or a non-zero size",
                ));
            }
            // Storage entries never own a stream.  Ignoring these redundant fields cannot make
            // their alleged chain reachable, while all real stream chains remain authenticated.
            recoveries.insert(CompoundRecovery::StorageStreamMetadata);
        }
        entries.push(DirectoryEntry {
            name,
            kind,
            left: le32(raw, 68, "cfb/directory")?,
            right: le32(raw, 72, "cfb/directory")?,
            child,
            start,
            size,
            parent: None,
        });
    }
    if entries.first().is_none_or(|entry| entry.kind != EntryKind::Root) {
        return Err(malformed("cfb/directory", "entry zero is not the root storage"));
    }
    if entries[0].name != "Root Entry" {
        if compatibility == CompoundCompatibility::Strict {
            return Err(malformed("cfb/directory", "root storage has an invalid name"));
        }
        // Entry zero and object type 5 identify the root.  Its display name is not used for
        // lookup or path resolution, so non-canonical producer labels are safe to ignore.
        recoveries.insert(CompoundRecovery::RootStorageName);
    }
    if entries[0].left != NONE || entries[0].right != NONE {
        return Err(malformed("cfb/directory", "root entry has sibling links"));
    }
    Ok(entries)
}

fn try_vec_capacity<T>(capacity: usize, label: &str) -> Result<Vec<T>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve {label} capacity {capacity}: {error}"))
    })?;
    Ok(output)
}

fn try_sized_vec<T: Clone>(
    length: usize,
    value: T,
    label: &str,
) -> Result<Vec<T>, ConversionError> {
    let mut output = try_vec_capacity(length, label)?;
    output.resize(length, value);
    Ok(output)
}

fn parse_directory_name(
    raw: &[u8],
    compatibility: CompoundCompatibility,
    recoveries: &mut BTreeSet<CompoundRecovery>,
) -> Result<String, ConversionError> {
    let name_len = usize::from(le16(raw, 64, "cfb/directory")?);
    if !(2..=64).contains(&name_len) || !name_len.is_multiple_of(2) {
        return Err(malformed("cfb/directory", "invalid UTF-16 directory name length"));
    }
    let units = raw[..64]
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    let declared_units = name_len / 2;
    let terminator = units.iter().position(|unit| *unit == 0);
    let name_units = match terminator {
        Some(index) if index + 1 == declared_units => &units[..index],
        Some(index)
            if compatibility == CompoundCompatibility::LegacyOfficeBestEffort
                && index > 0
                && index <= declared_units =>
        {
            recoveries.insert(CompoundRecovery::DirectoryNameTerminator);
            &units[..index]
        }
        _ => {
            return Err(malformed("cfb/directory", "directory name is not singly NUL terminated"));
        }
    };
    let name = String::from_utf16(name_units)
        .map_err(|_| malformed("cfb/directory", "directory name contains invalid UTF-16"))?;
    if name.is_empty() || name.contains(['/', '\\']) {
        return Err(malformed("cfb/directory", "directory name is empty or contains a separator"));
    }
    Ok(name)
}
fn assign_paths<B: CompoundBudget + ?Sized>(
    entries: &mut [DirectoryEntry],
    budget: &mut B,
) -> Result<(), ConversionError> {
    let mut seen = vec![false; entries.len()];
    seen[0] = true;
    let child = entries[0].child;
    visit_tree(entries, child, Some(0), 1, 0, &mut seen, budget)?;
    if entries
        .iter()
        .enumerate()
        .any(|(index, entry)| index != 0 && !entry.name.is_empty() && !seen[index])
    {
        return Err(malformed("cfb/directory", "non-empty directory entry is unreachable"));
    }
    Ok(())
}

fn visit_tree<B: CompoundBudget + ?Sized>(
    entries: &mut [DirectoryEntry],
    index: u32,
    parent: Option<usize>,
    storage_depth: u16,
    tree_depth: u16,
    seen: &mut [bool],
    budget: &mut B,
) -> Result<(), ConversionError> {
    if index == NONE {
        return Ok(());
    }
    budget.cfb_depth(storage_depth.max(tree_depth), "cfb/directory")?;
    let index = to_usize(index)?;
    if index >= entries.len() || entries[index].name.is_empty() {
        return Err(malformed("cfb/directory", "directory link is out of bounds or empty"));
    }
    if seen[index] {
        return Err(malformed("cfb/directory", "directory graph contains a cycle or shared child"));
    }
    seen[index] = true;
    let (left, right, child, kind, name) = {
        let entry = &entries[index];
        (entry.left, entry.right, entry.child, entry.kind, entry.name.clone())
    };
    visit_tree(entries, left, parent, storage_depth, tree_depth + 1, seen, budget)?;
    if entries.iter().enumerate().any(|(other, entry)| {
        other != index
            && seen[other]
            && entry.parent == parent
            && entry.name.eq_ignore_ascii_case(&name)
    }) {
        return Err(malformed(
            "cfb/directory",
            "storage contains duplicate case-insensitive names",
        ));
    }
    entries[index].parent = parent;
    if matches!(kind, EntryKind::Storage) {
        visit_tree(entries, child, Some(index), storage_depth + 1, 0, seen, budget)?;
    }
    visit_tree(entries, right, parent, storage_depth, tree_depth + 1, seen, budget)
}

struct MiniStreamContext<'a> {
    minifat: &'a [u32],
    root: &'a [u8],
    owners: &'a mut [bool],
    mini_size: usize,
    compatibility: CompoundCompatibility,
    recoveries: &'a mut BTreeSet<CompoundRecovery>,
}

fn read_mini_stream(
    entry: &DirectoryEntry,
    part: &str,
    context: &mut MiniStreamContext<'_>,
) -> Result<Vec<u8>, ConversionError> {
    let expected = to_usize64(entry.size, part)?.div_ceil(context.mini_size);
    let (chain, recovered_tail) = walk_chain_with_declared_tail(
        entry.start,
        context.minifat,
        context.owners.len(),
        Some(u32::try_from(expected).unwrap_or(u32::MAX)),
        part,
        context.compatibility == CompoundCompatibility::LegacyOfficeBestEffort,
    )?;
    if recovered_tail {
        context.recoveries.insert(CompoundRecovery::StreamChainTail);
    }
    let capacity = expected
        .checked_mul(context.mini_size)
        .ok_or_else(|| limit("max_decompressed_bytes", "mini-stream capacity overflowed"))?;
    let mut output = try_vec_capacity(capacity, "CFB mini stream")?;
    for id in chain {
        let index = to_usize(id)?;
        if context.owners.get(index).copied().unwrap_or(true) {
            return Err(malformed(part, "mini-sector overlaps another stream"));
        }
        context.owners[index] = true;
        let start = index
            .checked_mul(context.mini_size)
            .ok_or_else(|| limit("max_decompressed_bytes", "mini-sector offset overflowed"))?;
        output.extend_from_slice(
            context
                .root
                .get(start..start + context.mini_size)
                .ok_or_else(|| malformed(part, "mini-sector exceeds root mini stream"))?,
        );
    }
    output.truncate(to_usize64(entry.size, part)?);
    Ok(output)
}

fn regular_stream_chain(
    entry: &DirectoryEntry,
    fat: &[u32],
    sector_count: usize,
    sector_size: usize,
    part: &str,
    compatibility: CompoundCompatibility,
    recoveries: &mut BTreeSet<CompoundRecovery>,
) -> Result<Vec<u32>, ConversionError> {
    let count = to_usize64(entry.size, part)?.div_ceil(sector_size);
    let (chain, recovered_tail) = walk_chain_with_declared_tail(
        entry.start,
        fat,
        sector_count,
        Some(u32::try_from(count).unwrap_or(u32::MAX)),
        part,
        compatibility == CompoundCompatibility::LegacyOfficeBestEffort,
    )?;
    if recovered_tail {
        recoveries.insert(CompoundRecovery::StreamChainTail);
    }
    Ok(chain)
}

fn validate_fat_targets(fat: &[u32], sector_count: usize) -> Result<(), ConversionError> {
    for value in fat {
        if !matches!(*value, FREE | END | FAT | DIFAT) {
            validate_physical(*value, sector_count, "cfb/fat")?;
        }
    }
    Ok(())
}

fn fat_target_is_out_of_bounds(value: u32, sector_count: usize) -> bool {
    !matches!(value, FREE | END | FAT | DIFAT)
        && usize::try_from(value).map_or(true, |value| value >= sector_count)
}

fn claim(owners: &mut [bool], id: u32, owner: &str) -> Result<(), ConversionError> {
    let slot =
        owners.get_mut(to_usize(id)?).ok_or_else(|| malformed(owner, "sector is out of bounds"))?;
    if *slot {
        return Err(malformed(owner, "sector overlaps another CFB chain"));
    }
    *slot = true;
    Ok(())
}

fn concatenate(
    bytes: &[u8],
    sector_size: usize,
    chain: &[u32],
) -> Result<Vec<u8>, ConversionError> {
    let capacity = chain
        .len()
        .checked_mul(sector_size)
        .ok_or_else(|| limit("max_decompressed_bytes", "sector chain byte size overflowed"))?;
    let mut output = try_vec_capacity(capacity, "CFB sector chain")?;
    for id in chain {
        output.extend_from_slice(read_sector(bytes, sector_size, *id)?);
    }
    Ok(output)
}

fn concatenate_regular_stream(
    bytes: &[u8],
    sector_size: usize,
    chain: &[u32],
    logical_size: u64,
    partial_sector: Option<usize>,
    part: &str,
) -> Result<(Vec<u8>, Option<bool>), ConversionError> {
    let logical_size = to_usize64(logical_size, part)?;
    let mut output = try_vec_capacity(logical_size, "CFB regular stream")?;
    let mut partial_tail_consumed = None;
    for (index, id) in chain.iter().enumerate() {
        let physical = to_usize(*id)?;
        if Some(physical) == partial_sector {
            if index + 1 != chain.len() || partial_tail_consumed.is_some() {
                return Err(malformed(part, "partial physical sector is not terminal"));
            }
            let remaining = logical_size
                .checked_sub(output.len())
                .ok_or_else(|| malformed(part, "stream chain exceeds declared size"))?;
            let start = physical
                .checked_add(1)
                .and_then(|value| value.checked_mul(sector_size))
                .ok_or_else(|| malformed(part, "partial sector offset overflowed"))?;
            let tail = bytes
                .get(start..)
                .ok_or_else(|| malformed(part, "partial sector is outside source bytes"))?;
            if remaining == 0 || remaining > tail.len() || remaining >= sector_size {
                return Err(malformed(
                    part,
                    "partial terminal sector does not satisfy the declared stream size",
                ));
            }
            output.extend_from_slice(&tail[..remaining]);
            partial_tail_consumed = Some(remaining == tail.len());
        } else {
            output.extend_from_slice(read_sector(bytes, sector_size, *id)?);
        }
    }
    if output.len() < logical_size {
        return Err(malformed(part, "stream chain is shorter than its declared size"));
    }
    output.truncate(logical_size);
    Ok((output, partial_tail_consumed))
}

fn materialize_regular_stream<B: CompoundBudget + ?Sized>(
    bytes: &[u8],
    sector_size: usize,
    chain: &[u32],
    logical_size: u64,
    partial_sector: Option<usize>,
    part: &str,
    budget: &mut B,
) -> Result<(Vec<u8>, Option<bool>), ConversionError> {
    budget.cfb_expanded(logical_size)?;
    concatenate_regular_stream(bytes, sector_size, chain, logical_size, partial_sector, part)
}

fn read_sector(bytes: &[u8], sector_size: usize, id: u32) -> Result<&[u8], ConversionError> {
    let index = to_usize(id)?;
    let start = index
        .checked_add(1)
        .and_then(|value| value.checked_mul(sector_size))
        .ok_or_else(|| malformed("cfb/sector", "sector offset overflowed"))?;
    bytes
        .get(start..start + sector_size)
        .ok_or_else(|| malformed("cfb/sector", "sector exceeds source bytes"))
}

fn validate_physical(id: u32, count: usize, part: &str) -> Result<(), ConversionError> {
    if to_usize(id)? >= count {
        return Err(malformed(part, "sector identifier is out of bounds"));
    }
    Ok(())
}

fn stable_path(entries: &[DirectoryEntry], index: usize) -> String {
    let mut components = Vec::new();
    let mut current = Some(index);
    while let Some(entry_index) = current {
        let entry = &entries[entry_index];
        if entry.kind != EntryKind::Root {
            components.push(entry.name.as_str());
        }
        current = entry.parent;
    }
    let mut output = String::from("msg");
    for component in components.into_iter().rev() {
        output.push('/');
        for byte in component.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'#') {
                output.push(char::from(byte));
            } else {
                use std::fmt::Write as _;
                let _ = write!(output, "%{byte:02X}");
            }
        }
    }
    output
}

fn le16(bytes: &[u8], offset: usize, part: &str) -> Result<u16, ConversionError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| malformed(part, "truncated little-endian integer"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn le32(bytes: &[u8], offset: usize, part: &str) -> Result<u32, ConversionError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| malformed(part, "truncated little-endian integer"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn le64(bytes: &[u8], offset: usize, part: &str) -> Result<u64, ConversionError> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| malformed(part, "truncated little-endian integer"))?;
    Ok(u64::from_le_bytes(raw.try_into().map_err(|_| malformed(part, "truncated 64-bit integer"))?))
}

fn to_usize(value: u32) -> Result<usize, ConversionError> {
    usize::try_from(value).map_err(|_| malformed("cfb", "32-bit index cannot be represented"))
}

fn to_usize64(value: u64, part: &str) -> Result<usize, ConversionError> {
    usize::try_from(value).map_err(|_| {
        limit("max_decompressed_bytes", format!("stream {part} is too large for this platform"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::ErrorCode;

    fn stream(size: u64) -> DirectoryEntry {
        DirectoryEntry {
            name: "stream".into(),
            kind: EntryKind::Stream,
            left: NONE,
            right: NONE,
            child: NONE,
            start: 0,
            size,
            parent: None,
        }
    }

    struct RejectExpandedBudget {
        expanded_calls: Vec<u64>,
        context: into_markdown_core::ExecutionContext,
    }

    impl CompoundBudget for RejectExpandedBudget {
        fn cfb_memory(&self, bytes: u64) -> Result<ResourceReservation, ConversionError> {
            self.context.reserve_memory(bytes)
        }

        fn cfb_entry(&mut self) -> Result<(), ConversionError> {
            Ok(())
        }

        fn cfb_expanded(&mut self, bytes: u64) -> Result<(), ConversionError> {
            self.expanded_calls.push(bytes);
            Err(limit("max_decompressed_bytes", "test budget rejected stream"))
        }

        fn cfb_depth(&self, _depth: u16, _part: &str) -> Result<(), ConversionError> {
            Ok(())
        }

        fn cfb_work(&mut self, _units: u64) -> Result<(), ConversionError> {
            Ok(())
        }
    }

    #[test]
    fn regular_stream_budget_is_checked_before_materialization() {
        let mut budget = RejectExpandedBudget {
            expanded_calls: Vec::new(),
            context: into_markdown_core::ExecutionContext::new(
                into_markdown_core::ExecutionOptions::default(),
                into_markdown_core::ResourceLimits::default(),
            ),
        };
        let declared = 64 * 1024 * 1024;
        let error =
            materialize_regular_stream(&[], 512, &[0], declared, None, "cfb/root", &mut budget)
                .unwrap_err();

        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert_eq!(budget.expanded_calls, vec![declared]);
    }

    #[test]
    fn compound_lifetime_memory_plan_has_exact_boundary_and_releases_on_drop() {
        let plan = cfb_memory_plan(8 * 1024, 15).unwrap();
        let limits = into_markdown_core::ResourceLimits {
            max_memory_bytes: plan,
            ..into_markdown_core::ResourceLimits::default()
        };
        let exact = into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            limits.clone(),
        );
        let lease = exact.reserve_memory(plan).unwrap();
        assert!(exact.reserve_memory(1).is_err());
        drop(lease);
        assert!(exact.reserve_memory(plan).is_ok());

        let limits = into_markdown_core::ResourceLimits {
            max_memory_bytes: plan - 1,
            ..into_markdown_core::ResourceLimits::default()
        };
        let below = into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            limits,
        );
        let error = below.reserve_memory(plan).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
    }

    #[test]
    fn compound_memory_plan_rejects_attacker_sized_overflow_without_allocating() {
        let error = cfb_memory_plan(usize::MAX, usize::MAX).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
    }

    #[test]
    fn compound_memory_reservation_observes_cancellation_without_leaking() {
        let cancellation = into_markdown_core::CancellationToken::new();
        let context = into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions {
                cancellation: cancellation.clone(),
                ..into_markdown_core::ExecutionOptions::default()
            },
            into_markdown_core::ResourceLimits::default(),
        );
        cancellation.cancel();
        let error = context.reserve_memory(1024).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
    }

    #[test]
    fn mini_stream_reader_rejects_a_chain_longer_than_declared_data() {
        let mut owners = vec![false, false];
        let mut recoveries = BTreeSet::new();
        let mut context = MiniStreamContext {
            minifat: &[1, END],
            root: &[0; 128],
            owners: &mut owners,
            mini_size: 64,
            compatibility: CompoundCompatibility::Strict,
            recoveries: &mut recoveries,
        };
        let error = read_mini_stream(&stream(1), "mini", &mut context).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
    }

    #[test]
    fn regular_stream_reader_rejects_a_chain_longer_than_declared_data() {
        let error = regular_stream_chain(
            &stream(1),
            &[1, END],
            2,
            512,
            "regular",
            CompoundCompatibility::Strict,
            &mut BTreeSet::new(),
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
    }

    #[test]
    fn partial_terminal_sector_uses_only_the_declared_stream_prefix() {
        let mut bytes = vec![0; 2 * 512];
        bytes.extend_from_slice(&[1, 2, 3]);
        let (stream, recovered) =
            concatenate_regular_stream(&bytes, 512, &[0, 1], 515, Some(1), "xls/Workbook").unwrap();
        assert_eq!(stream.len(), 515);
        assert_eq!(&stream[512..], &[1, 2, 3]);
        assert_eq!(recovered, Some(true));

        let mut bytes_with_trailer = bytes.clone();
        bytes_with_trailer.extend_from_slice(&[4, 5]);
        let (stream, recovered) = concatenate_regular_stream(
            &bytes_with_trailer,
            512,
            &[0, 1],
            515,
            Some(1),
            "xls/Workbook",
        )
        .unwrap();
        assert_eq!(&stream[512..], &[1, 2, 3]);
        assert_eq!(recovered, Some(false));

        assert!(
            concatenate_regular_stream(&bytes, 512, &[0, 1], 516, Some(1), "xls/Workbook").is_err()
        );
    }
}
