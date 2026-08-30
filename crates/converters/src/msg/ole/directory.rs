use super::binary::{le16, le32, le64, to_usize, try_vec_capacity};
use super::{
    CompoundBudget, CompoundCompatibility, CompoundRecoveries, CompoundRecovery, ConversionError,
    END, EntryKind, FREE, NONE, limit, malformed,
};

#[derive(Debug)]
pub(super) struct DirectoryEntry {
    pub(super) name: String,
    pub(super) kind: EntryKind,
    pub(super) left: u32,
    pub(super) right: u32,
    pub(super) child: u32,
    pub(super) start: u32,
    pub(super) size: u64,
    pub(super) parent: Option<usize>,
}

pub(super) fn cfb_directory_memory_plan(entries: usize) -> Result<u64, ConversionError> {
    const MAX_DIRECTORY_NAME_UTF8_BYTES: usize = 31 * 3;
    let bytes_per_entry = std::mem::size_of::<DirectoryEntry>()
        .checked_add(MAX_DIRECTORY_NAME_UTF8_BYTES)
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<bool>()))
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Option<Vec<u8>>>()))
        .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<u32>()))
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<(usize, String)>()))
        .and_then(|bytes| bytes.checked_add(MAX_DIRECTORY_NAME_UTF8_BYTES))
        .ok_or_else(|| limit("max_memory_bytes", "CFB directory entry plan overflowed"))?;
    u64::try_from(entries)
        .unwrap_or(u64::MAX)
        .checked_mul(u64::try_from(bytes_per_entry).unwrap_or(u64::MAX))
        .ok_or_else(|| limit("max_memory_bytes", "CFB directory memory plan overflowed"))
}

pub(super) fn parse_directory<B: CompoundBudget + ?Sized>(
    bytes: &[u8],
    major: u16,
    budget: &mut B,
    compatibility: CompoundCompatibility,
    recoveries: &mut CompoundRecoveries,
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
        let name = match parse_directory_name(raw, compatibility, recoveries) {
            Ok(name) => name,
            Err(_)
                if index == 0 && compatibility == CompoundCompatibility::LegacyOfficeBestEffort =>
            {
                recover_root_name_with_uncounted_terminator(raw)?
            }
            Err(error) => return Err(error),
        };
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

fn recover_root_name_with_uncounted_terminator(raw: &[u8]) -> Result<String, ConversionError> {
    let name_len = usize::from(le16(raw, 64, "cfb/directory")?);
    if !(2..=62).contains(&name_len) || !name_len.is_multiple_of(2) {
        return Err(malformed("cfb/directory", "invalid UTF-16 directory name length"));
    }
    let units = raw[..64]
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    let declared_units = name_len / 2;
    if units[..declared_units].contains(&0) || units[declared_units..].iter().any(|unit| *unit != 0)
    {
        return Err(malformed("cfb/directory", "directory name is not singly NUL terminated"));
    }
    let name = String::from_utf16(&units[..declared_units])
        .map_err(|_| malformed("cfb/directory", "directory name contains invalid UTF-16"))?;
    if name.is_empty() || name.contains(['/', '\\']) {
        return Err(malformed("cfb/directory", "directory name is empty or contains a separator"));
    }
    Ok(name)
}

pub(super) fn parse_directory_name(
    raw: &[u8],
    compatibility: CompoundCompatibility,
    recoveries: &mut CompoundRecoveries,
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
                && index < declared_units
                && units[index + 1..].iter().all(|unit| *unit == 0) =>
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
pub(super) fn assign_paths<B: CompoundBudget + ?Sized>(
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
    validate_unique_names(entries, budget)
}

pub(super) fn visit_tree<B: CompoundBudget + ?Sized>(
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
    let (left, right, child, kind) = {
        let entry = &entries[index];
        (entry.left, entry.right, entry.child, entry.kind)
    };
    visit_tree(entries, left, parent, storage_depth, tree_depth + 1, seen, budget)?;
    entries[index].parent = parent;
    if matches!(kind, EntryKind::Storage) {
        visit_tree(entries, child, Some(index), storage_depth + 1, 0, seen, budget)?;
    }
    visit_tree(entries, right, parent, storage_depth, tree_depth + 1, seen, budget)
}

pub(super) fn validate_unique_names<B: CompoundBudget + ?Sized>(
    entries: &[DirectoryEntry],
    budget: &mut B,
) -> Result<(), ConversionError> {
    let count = entries.iter().filter(|entry| entry.parent.is_some()).count();
    let comparison_levels = usize::try_from(usize::BITS - count.saturating_sub(1).leading_zeros())
        .unwrap_or(usize::MAX);
    let comparison_bound = count
        .checked_mul(comparison_levels)
        .ok_or_else(|| limit("max_memory_bytes", "CFB name comparison plan overflowed"))?;
    budget.cfb_work(u64::try_from(comparison_bound).unwrap_or(u64::MAX))?;
    let mut names = try_vec_capacity(count, "CFB normalized sibling names")?;
    for entry in entries.iter().filter(|entry| entry.parent.is_some()) {
        names.push((entry.parent.unwrap_or(0), entry.name.to_ascii_lowercase()));
    }
    names.sort_unstable();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(malformed(
            "cfb/directory",
            "storage contains duplicate case-insensitive names",
        ));
    }
    budget.cfb_work(u64::try_from(count).unwrap_or(u64::MAX))
}

pub(super) fn stable_path(entries: &[DirectoryEntry], index: usize) -> String {
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
