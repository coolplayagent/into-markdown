use super::budget::{MsgBudget, limit, malformed};
use into_markdown_core::ConversionError;
use std::collections::{BTreeMap, BTreeSet};

const SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
const FREE: u32 = 0xffff_ffff;
const END: u32 = 0xffff_fffe;
const FAT: u32 = 0xffff_fffd;
const DIFAT: u32 = 0xffff_fffc;
const NONE: u32 = 0xffff_ffff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
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
    path: Vec<String>,
}

#[derive(Debug)]
pub(super) struct CompoundFile {
    entries: Vec<DirectoryEntry>,
    streams: BTreeMap<usize, Vec<u8>>,
}

impl CompoundFile {
    pub(super) fn open(bytes: &[u8], budget: &mut MsgBudget<'_>) -> Result<Self, ConversionError> {
        Header::parse(bytes)?.open(bytes, budget)
    }

    pub(super) fn root(&self) -> Storage<'_> {
        Storage { file: self, index: 0 }
    }

    fn children(&self, parent: usize) -> impl Iterator<Item = (usize, &DirectoryEntry)> {
        self.entries.iter().enumerate().filter(move |(_, entry)| {
            entry.path.len() == self.entries[parent].path.len() + 1
                && entry.path.starts_with(&self.entries[parent].path)
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Storage<'a> {
    file: &'a CompoundFile,
    index: usize,
}

impl<'a> Storage<'a> {
    pub(super) fn path(&self) -> String {
        stable_path(&self.file.entries[self.index].path)
    }

    pub(super) fn stream(&self, name: &str) -> Option<&'a [u8]> {
        self.file
            .children(self.index)
            .find(|(_, entry)| {
                entry.kind == EntryKind::Stream && entry.name.eq_ignore_ascii_case(name)
            })
            .and_then(|(index, _)| self.file.streams.get(&index).map(Vec::as_slice))
    }

    pub(super) fn storages(&self) -> impl Iterator<Item = Storage<'a>> + 'a {
        self.file.children(self.index).filter_map(|(index, entry)| {
            (entry.kind == EntryKind::Storage).then_some(Storage { file: self.file, index })
        })
    }

    pub(super) fn storage(&self, name: &str) -> Option<Storage<'a>> {
        self.storages()
            .find(|storage| storage.file.entries[storage.index].name.eq_ignore_ascii_case(name))
    }

    pub(super) fn name(&self) -> &'a str {
        &self.file.entries[self.index].name
    }
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
    fn open(
        self,
        bytes: &[u8],
        budget: &mut MsgBudget<'_>,
    ) -> Result<CompoundFile, ConversionError> {
        if bytes.len() < self.sector_size || !bytes.len().is_multiple_of(self.sector_size) {
            return Err(malformed("cfb/header", "CFB file length is not sector aligned"));
        }
        if self.major == 4 && bytes[512..self.sector_size].iter().any(|byte| *byte != 0) {
            return Err(malformed("cfb/header", "non-zero version 4 header padding"));
        }
        let sector_count = bytes.len() / self.sector_size - 1;
        let mut owners = vec![None; sector_count];
        let (fat_sector_ids, difat_sector_ids) = self.read_difat(bytes, sector_count, budget)?;
        for id in &difat_sector_ids {
            claim(&mut owners, *id, "cfb/difat")?;
        }
        for id in &fat_sector_ids {
            claim(&mut owners, *id, "cfb/fat")?;
        }
        let mut fat = Vec::new();
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
                return Err(malformed("cfb/fat", "FAT sector is not marked FATSECT"));
            }
        }
        for id in &difat_sector_ids {
            if fat.get(to_usize(*id)?).copied() != Some(DIFAT) {
                return Err(malformed("cfb/difat", "DIFAT sector is not marked DIFSECT"));
            }
        }
        validate_fat_targets(&fat[..sector_count], sector_count)?;

        let directory_expected = (self.major == 4).then_some(self.directory_sectors);
        let directory_chain = walk_chain(
            self.first_directory,
            &fat,
            sector_count,
            directory_expected,
            false,
            "cfb/directory",
        )?;
        if directory_chain.is_empty() {
            return Err(malformed("cfb/directory", "CFB has no directory sector"));
        }
        for id in &directory_chain {
            claim(&mut owners, *id, "cfb/directory")?;
        }
        let directory_bytes = concatenate(bytes, self.sector_size, &directory_chain)?;
        let mut entries = parse_directory(&directory_bytes, self.major, budget)?;
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
                false,
                "cfb/minifat",
            )?
        };
        for id in &minifat_chain {
            claim(&mut owners, *id, "cfb/minifat")?;
        }
        let minifat_bytes = concatenate(bytes, self.sector_size, &minifat_chain)?;
        let minifat = minifat_bytes
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect::<Vec<_>>();

        let root =
            entries.first().ok_or_else(|| malformed("cfb/directory", "missing root entry"))?;
        let root_chain =
            regular_stream_chain(root, &fat, sector_count, self.sector_size, "cfb/root")?;
        for id in &root_chain {
            claim(&mut owners, *id, "cfb/root-mini-stream")?;
        }
        let mut root_mini_stream = concatenate(bytes, self.sector_size, &root_chain)?;
        root_mini_stream.truncate(to_usize64(root.size, "cfb/root")?);
        budget.expanded(root.size)?;

        let mut mini_owners: Vec<Option<String>> =
            vec![None; root_mini_stream.len().div_ceil(self.mini_sector_size)];
        let mut streams = BTreeMap::new();
        for (index, entry) in
            entries.iter().enumerate().filter(|(_, entry)| entry.kind == EntryKind::Stream)
        {
            budget.expanded(entry.size)?;
            let part = stable_path(&entry.path);
            let data = if entry.size < u64::from(self.mini_cutoff) {
                read_mini_stream(
                    entry,
                    &minifat,
                    &root_mini_stream,
                    &mut mini_owners,
                    self.mini_sector_size,
                    &part,
                )?
            } else {
                let chain =
                    regular_stream_chain(entry, &fat, sector_count, self.sector_size, &part)?;
                for id in &chain {
                    claim(&mut owners, *id, &part)?;
                }
                let mut data = concatenate(bytes, self.sector_size, &chain)?;
                data.truncate(to_usize64(entry.size, &part)?);
                data
            };
            streams.insert(index, data);
        }
        Ok(CompoundFile { entries, streams })
    }

    fn read_difat(
        self,
        bytes: &[u8],
        sector_count: usize,
        budget: &mut MsgBudget<'_>,
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
        let mut current = self.first_difat;
        for _ in 0..self.difat_sectors {
            budget.work(1)?;
            validate_physical(current, sector_count, "cfb/difat")?;
            if !difat_ids.insert_unique(current) {
                return Err(malformed("cfb/difat", "DIFAT chain contains a cycle"));
            }
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
trait InsertUnique {
    fn insert_unique(&mut self, value: u32) -> bool;
}

impl InsertUnique for Vec<u32> {
    fn insert_unique(&mut self, value: u32) -> bool {
        if self.contains(&value) {
            false
        } else {
            self.push(value);
            true
        }
    }
}
fn parse_directory(
    bytes: &[u8],
    major: u16,
    budget: &mut MsgBudget<'_>,
) -> Result<Vec<DirectoryEntry>, ConversionError> {
    if !bytes.len().is_multiple_of(128) {
        return Err(malformed("cfb/directory", "directory stream is not entry aligned"));
    }
    let mut entries = Vec::with_capacity(bytes.len() / 128);
    for (index, raw) in bytes.chunks_exact(128).enumerate() {
        budget.entry()?;
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
                path: Vec::new(),
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
        let name_len = usize::from(le16(raw, 64, "cfb/directory")?);
        if !(2..=64).contains(&name_len) || !name_len.is_multiple_of(2) {
            return Err(malformed("cfb/directory", "invalid UTF-16 directory name length"));
        }
        let units = raw[..name_len]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        if units.last() != Some(&0) || units[..units.len() - 1].contains(&0) {
            return Err(malformed("cfb/directory", "directory name is not singly NUL terminated"));
        }
        let name = String::from_utf16(&units[..units.len() - 1])
            .map_err(|_| malformed("cfb/directory", "directory name contains invalid UTF-16"))?;
        if name.is_empty() || name.contains(['/', '\\']) {
            return Err(malformed(
                "cfb/directory",
                "directory name is empty or contains a separator",
            ));
        }
        let child = le32(raw, 76, "cfb/directory")?;
        if kind == EntryKind::Stream && child != NONE {
            return Err(malformed("cfb/directory", "stream directory entry has children"));
        }
        let raw_size = le64(raw, 120, "cfb/directory")?;
        let size = if major == 3 { raw_size & u64::from(u32::MAX) } else { raw_size };
        let start = le32(raw, 116, "cfb/directory")?;
        if kind == EntryKind::Storage && (size != 0 || !matches!(start, END | FREE)) {
            return Err(malformed(
                "cfb/directory",
                "storage entry declares stream sectors or a non-zero size",
            ));
        }
        entries.push(DirectoryEntry {
            name,
            kind,
            left: le32(raw, 68, "cfb/directory")?,
            right: le32(raw, 72, "cfb/directory")?,
            child,
            start,
            size,
            path: Vec::new(),
        });
    }
    if entries.first().is_none_or(|entry| entry.kind != EntryKind::Root) {
        return Err(malformed("cfb/directory", "entry zero is not the root storage"));
    }
    if entries[0].name != "Root Entry" {
        return Err(malformed("cfb/directory", "root storage has an invalid name"));
    }
    if entries[0].left != NONE || entries[0].right != NONE {
        return Err(malformed("cfb/directory", "root entry has sibling links"));
    }
    Ok(entries)
}
fn assign_paths(
    entries: &mut [DirectoryEntry],
    budget: &mut MsgBudget<'_>,
) -> Result<(), ConversionError> {
    let mut seen = vec![false; entries.len()];
    seen[0] = true;
    let child = entries[0].child;
    visit_tree(entries, child, &[], 1, 0, &mut seen, budget)?;
    if entries
        .iter()
        .enumerate()
        .any(|(index, entry)| index != 0 && !entry.name.is_empty() && !seen[index])
    {
        return Err(malformed("cfb/directory", "non-empty directory entry is unreachable"));
    }
    Ok(())
}

fn visit_tree(
    entries: &mut [DirectoryEntry],
    index: u32,
    parent: &[String],
    storage_depth: u16,
    tree_depth: u16,
    seen: &mut [bool],
    budget: &mut MsgBudget<'_>,
) -> Result<(), ConversionError> {
    if index == NONE {
        return Ok(());
    }
    budget.depth(storage_depth.max(tree_depth), "cfb/directory")?;
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
            && entry.path.len() == parent.len() + 1
            && entry.path.starts_with(parent)
            && entry.name.eq_ignore_ascii_case(&name)
    }) {
        return Err(malformed(
            "cfb/directory",
            "storage contains duplicate case-insensitive names",
        ));
    }
    let mut path = parent.to_vec();
    path.push(name);
    entries[index].path.clone_from(&path);
    if matches!(kind, EntryKind::Storage) {
        visit_tree(entries, child, &path, storage_depth + 1, 0, seen, budget)?;
    }
    visit_tree(entries, right, parent, storage_depth, tree_depth + 1, seen, budget)
}

fn read_mini_stream(
    entry: &DirectoryEntry,
    minifat: &[u32],
    root: &[u8],
    owners: &mut [Option<String>],
    mini_size: usize,
    part: &str,
) -> Result<Vec<u8>, ConversionError> {
    let expected = to_usize64(entry.size, part)?.div_ceil(mini_size);
    let chain = walk_chain(
        entry.start,
        minifat,
        owners.len(),
        Some(u32::try_from(expected).unwrap_or(u32::MAX)),
        true,
        part,
    )?;
    let mut output = Vec::with_capacity(expected.saturating_mul(mini_size));
    for id in chain {
        let index = to_usize(id)?;
        if let Some(owner) = owners.get(index).and_then(Option::as_ref) {
            return Err(malformed(part, format!("mini-sector overlaps {owner}")));
        }
        owners[index] = Some(part.to_owned());
        let start = index
            .checked_mul(mini_size)
            .ok_or_else(|| limit("max_decompressed_bytes", "mini-sector offset overflowed"))?;
        output.extend_from_slice(
            root.get(start..start + mini_size)
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
) -> Result<Vec<u32>, ConversionError> {
    let count = to_usize64(entry.size, part)?.div_ceil(sector_size);
    walk_chain(
        entry.start,
        fat,
        sector_count,
        Some(u32::try_from(count).unwrap_or(u32::MAX)),
        true,
        part,
    )
}

fn walk_chain(
    start: u32,
    table: &[u32],
    addressable: usize,
    expected: Option<u32>,
    allow_extra: bool,
    part: &str,
) -> Result<Vec<u32>, ConversionError> {
    let expected_usize = expected.map(to_usize).transpose()?;
    if expected_usize == Some(0) {
        if !matches!(start, END | FREE) {
            return Err(malformed(part, "zero-length chain has a start sector"));
        }
        return Ok(Vec::new());
    }
    if matches!(start, END | FREE | FAT | DIFAT) {
        return Err(malformed(part, "non-empty chain has an invalid start sector"));
    }
    let mut output = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = start;
    loop {
        validate_physical(current, addressable, part)?;
        if !seen.insert(current) {
            return Err(malformed(part, "sector chain contains a cycle"));
        }
        output.push(current);
        if output.len() > addressable {
            return Err(malformed(part, "sector chain exceeds addressable sectors"));
        }
        current = *table
            .get(to_usize(current)?)
            .ok_or_else(|| malformed(part, "sector chain exceeds allocation table"))?;
        if current == END {
            break;
        }
        if matches!(current, FREE | FAT | DIFAT) {
            return Err(malformed(part, "sector chain enters a reserved marker"));
        }
    }
    if expected_usize
        .is_some_and(|count| output.len() < count || (!allow_extra && output.len() != count))
    {
        return Err(malformed(part, "sector chain length does not match declared stream size"));
    }
    Ok(output)
}

fn validate_fat_targets(fat: &[u32], sector_count: usize) -> Result<(), ConversionError> {
    for value in fat {
        if !matches!(*value, FREE | END | FAT | DIFAT) {
            validate_physical(*value, sector_count, "cfb/fat")?;
        }
    }
    Ok(())
}

fn claim(owners: &mut [Option<String>], id: u32, owner: &str) -> Result<(), ConversionError> {
    let slot =
        owners.get_mut(to_usize(id)?).ok_or_else(|| malformed(owner, "sector is out of bounds"))?;
    if let Some(previous) = slot {
        return Err(malformed(owner, format!("sector overlaps {previous}")));
    }
    *slot = Some(owner.to_owned());
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
    let mut output = Vec::with_capacity(capacity);
    for id in chain {
        output.extend_from_slice(read_sector(bytes, sector_size, *id)?);
    }
    Ok(output)
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

fn stable_path(path: &[String]) -> String {
    let mut output = String::from("msg");
    for component in path {
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
