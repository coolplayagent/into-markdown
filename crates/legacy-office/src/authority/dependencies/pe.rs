use super::{MAX_DEPENDENCIES, Specification, malformed};
use into_markdown_core::ConversionError;
use std::collections::BTreeSet;

const DIRECTORY_IMPORT: usize = 1;
const DIRECTORY_DELAY_IMPORT: usize = 13;
const MAX_SECTIONS: usize = 4_096;

#[derive(Clone, Copy)]
struct Section {
    virtual_start: u32,
    virtual_size: u32,
    raw_start: u32,
    raw_size: u32,
}

#[derive(Clone, Copy)]
struct Directory {
    rva: u32,
    size: u32,
}

pub(super) fn parse(bytes: &[u8]) -> Result<Specification, ConversionError> {
    if bytes.get(..2) != Some(b"MZ") {
        return Err(malformed());
    }
    let pe = usize::try_from(le32(bytes, 0x3c)?).map_err(|_| malformed())?;
    if bytes.get(pe..pe.checked_add(4).ok_or_else(malformed)?) != Some(b"PE\0\0") {
        return Err(malformed());
    }
    let coff = pe.checked_add(4).ok_or_else(malformed)?;
    let section_count = usize::from(le16(bytes, coff + 2)?);
    let optional_bytes = usize::from(le16(bytes, coff + 16)?);
    if section_count == 0 || section_count > MAX_SECTIONS {
        return Err(malformed());
    }
    let optional = coff.checked_add(20).ok_or_else(malformed)?;
    let section_table = optional.checked_add(optional_bytes).ok_or_else(malformed)?;
    let magic = le16(bytes, optional)?;
    let (directory_offset, image_base) = match magic {
        0x10b => (96_usize, u64::from(le32(bytes, optional + 28)?)),
        0x20b => (112_usize, le64(bytes, optional + 24)?),
        _ => return Err(malformed()),
    };
    let directory_count_offset = directory_offset.checked_sub(4).ok_or_else(malformed)?;
    let directory_count = usize::try_from(le32(bytes, optional + directory_count_offset)?)
        .map_err(|_| malformed())?;
    if optional_bytes < directory_offset || directory_count > 128 {
        return Err(malformed());
    }
    let required_directories = directory_count.checked_mul(8).ok_or_else(malformed)?;
    if directory_offset.checked_add(required_directories).ok_or_else(malformed)? > optional_bytes {
        return Err(malformed());
    }
    let sections = sections(bytes, section_table, section_count)?;
    let imports = if directory_count > DIRECTORY_IMPORT {
        directory(bytes, optional + directory_offset, DIRECTORY_IMPORT)?
    } else {
        None
    };
    let delays = if directory_count > DIRECTORY_DELAY_IMPORT {
        directory(bytes, optional + directory_offset, DIRECTORY_DELAY_IMPORT)?
    } else {
        None
    };
    if overlaps(imports, delays) {
        return Err(malformed());
    }
    let mut needed = BTreeSet::new();
    if let Some(directory) = imports {
        parse_descriptors(bytes, &sections, directory, 20, |record| le32(record, 12), &mut needed)?;
    }
    if let Some(directory) = delays {
        parse_descriptors(
            bytes,
            &sections,
            directory,
            32,
            |record| {
                let attributes = le32(record, 0)?;
                let name = u64::from(le32(record, 4)?);
                match attributes {
                    1 => u32::try_from(name).map_err(|_| malformed()),
                    0 => u32::try_from(name.checked_sub(image_base).ok_or_else(malformed)?)
                        .map_err(|_| malformed()),
                    _ => Err(malformed()),
                }
            },
            &mut needed,
        )?;
    }
    Ok(Specification { needed, search: Vec::new() })
}

fn sections(bytes: &[u8], start: usize, count: usize) -> Result<Vec<Section>, ConversionError> {
    let mut sections: Vec<Section> = Vec::new();
    sections.try_reserve_exact(count).map_err(|_| malformed())?;
    for index in 0..count {
        let offset = start
            .checked_add(index.checked_mul(40).ok_or_else(malformed)?)
            .ok_or_else(malformed)?;
        let section = Section {
            virtual_size: le32(bytes, offset + 8)?,
            virtual_start: le32(bytes, offset + 12)?,
            raw_size: le32(bytes, offset + 16)?,
            raw_start: le32(bytes, offset + 20)?,
        };
        checked_end(section.raw_start, section.raw_size)?;
        checked_end(section.virtual_start, section.virtual_size.max(section.raw_size))?;
        let raw_end = usize::try_from(checked_end(section.raw_start, section.raw_size)?)
            .map_err(|_| malformed())?;
        if raw_end > bytes.len()
            || sections.iter().any(|prior| {
                range_overlap(section.raw_start, section.raw_size, prior.raw_start, prior.raw_size)
                    || range_overlap(
                        section.virtual_start,
                        section.virtual_size.max(section.raw_size),
                        prior.virtual_start,
                        prior.virtual_size.max(prior.raw_size),
                    )
            })
        {
            return Err(malformed());
        }
        sections.push(section);
    }
    Ok(sections)
}

fn directory(
    bytes: &[u8],
    start: usize,
    index: usize,
) -> Result<Option<Directory>, ConversionError> {
    let offset =
        start.checked_add(index.checked_mul(8).ok_or_else(malformed)?).ok_or_else(malformed)?;
    let directory = Directory { rva: le32(bytes, offset)?, size: le32(bytes, offset + 4)? };
    match (directory.rva, directory.size) {
        (0, 0) => Ok(None),
        (0, _) | (_, 0) => Err(malformed()),
        _ => {
            checked_end(directory.rva, directory.size)?;
            Ok(Some(directory))
        }
    }
}

fn parse_descriptors(
    bytes: &[u8],
    sections: &[Section],
    directory: Directory,
    record_bytes: usize,
    name_rva: impl Fn(&[u8]) -> Result<u32, ConversionError>,
    needed: &mut BTreeSet<String>,
) -> Result<(), ConversionError> {
    let table = rva_slice(bytes, sections, directory.rva, directory.size)?;
    if table.len() < record_bytes || table.len() % record_bytes != 0 {
        return Err(malformed());
    }
    let mut terminated = false;
    for (index, record) in table.chunks_exact(record_bytes).enumerate() {
        if index >= MAX_DEPENDENCIES {
            return Err(malformed());
        }
        if record.iter().all(|byte| *byte == 0) {
            terminated = true;
            if table[(index + 1) * record_bytes..].iter().any(|byte| *byte != 0) {
                return Err(malformed());
            }
            break;
        }
        let name_rva = name_rva(record)?;
        if name_rva >= directory.rva && name_rva < checked_end(directory.rva, directory.size)? {
            return Err(malformed());
        }
        // PE import identities and the Windows loader are case-insensitive.
        // Normalize only ASCII DLL names; resolved package paths are rebound to
        // the exact manifest spelling by the dependency authority.
        needed.insert(rva_string(bytes, sections, name_rva)?.to_ascii_lowercase());
    }
    terminated.then_some(()).ok_or_else(malformed)
}

fn rva_slice<'a>(
    bytes: &'a [u8],
    sections: &[Section],
    rva: u32,
    size: u32,
) -> Result<&'a [u8], ConversionError> {
    let end = checked_end(rva, size)?;
    let section = sections
        .iter()
        .find(|section| {
            let section_end = section.virtual_start.checked_add(section.raw_size);
            rva >= section.virtual_start && section_end.is_some_and(|value| end <= value)
        })
        .ok_or_else(malformed)?;
    let offset = section
        .raw_start
        .checked_add(rva.checked_sub(section.virtual_start).ok_or_else(malformed)?)
        .ok_or_else(malformed)?;
    let start = usize::try_from(offset).map_err(|_| malformed())?;
    let end =
        start.checked_add(usize::try_from(size).map_err(|_| malformed())?).ok_or_else(malformed)?;
    bytes.get(start..end).ok_or_else(malformed)
}

fn rva_string(bytes: &[u8], sections: &[Section], rva: u32) -> Result<String, ConversionError> {
    let section = sections
        .iter()
        .find(|section| {
            rva >= section.virtual_start
                && rva < section.virtual_start.saturating_add(section.raw_size)
        })
        .ok_or_else(malformed)?;
    let remaining = section
        .raw_size
        .checked_sub(rva.checked_sub(section.virtual_start).ok_or_else(malformed)?)
        .ok_or_else(malformed)?
        .min(1_025);
    let bytes = rva_slice(bytes, sections, rva, remaining)?;
    let end = bytes.iter().position(|byte| *byte == 0).ok_or_else(malformed)?;
    let value = std::str::from_utf8(&bytes[..end]).map_err(|_| malformed())?;
    if value.is_empty()
        || value.len() > 1_024
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control() || matches!(byte, b'/' | b'\\'))
    {
        return Err(malformed());
    }
    Ok(value.to_owned())
}

fn overlaps(left: Option<Directory>, right: Option<Directory>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => range_overlap(left.rva, left.size, right.rva, right.size),
        _ => false,
    }
}

fn range_overlap(a: u32, a_size: u32, b: u32, b_size: u32) -> bool {
    if a_size == 0 || b_size == 0 {
        return false;
    }
    a < b.saturating_add(b_size) && b < a.saturating_add(a_size)
}

fn checked_end(start: u32, size: u32) -> Result<u32, ConversionError> {
    start.checked_add(size).ok_or_else(malformed)
}

fn le16(bytes: &[u8], offset: usize) -> Result<u16, ConversionError> {
    let value =
        bytes.get(offset..offset.checked_add(2).ok_or_else(malformed)?).ok_or_else(malformed)?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn le32(bytes: &[u8], offset: usize) -> Result<u32, ConversionError> {
    let value =
        bytes.get(offset..offset.checked_add(4).ok_or_else(malformed)?).ok_or_else(malformed)?;
    Ok(u32::from_le_bytes(value.try_into().map_err(|_| malformed())?))
}

fn le64(bytes: &[u8], offset: usize) -> Result<u64, ConversionError> {
    let value =
        bytes.get(offset..offset.checked_add(8).ok_or_else(malformed)?).ok_or_else(malformed)?;
    Ok(u64::from_le_bytes(value.try_into().map_err(|_| malformed())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(normal: Option<&str>, delay: Option<&str>, malformed_delay: bool) -> Vec<u8> {
        let mut bytes = vec![0_u8; 0x600];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x86..0x88].copy_from_slice(&1_u16.to_le_bytes());
        bytes[0x94..0x96].copy_from_slice(&240_u16.to_le_bytes());
        let optional = 0x98;
        bytes[optional..optional + 2].copy_from_slice(&0x20b_u16.to_le_bytes());
        bytes[optional + 24..optional + 32].copy_from_slice(&0x0001_4000_0000_u64.to_le_bytes());
        bytes[optional + 108..optional + 112].copy_from_slice(&16_u32.to_le_bytes());
        let sections = optional + 240;
        bytes[sections + 8..sections + 12].copy_from_slice(&0x400_u32.to_le_bytes());
        bytes[sections + 12..sections + 16].copy_from_slice(&0x1000_u32.to_le_bytes());
        bytes[sections + 16..sections + 20].copy_from_slice(&0x400_u32.to_le_bytes());
        bytes[sections + 20..sections + 24].copy_from_slice(&0x200_u32.to_le_bytes());
        if let Some(name) = normal {
            let directory = optional + 112 + 8;
            bytes[directory..directory + 4].copy_from_slice(&0x1000_u32.to_le_bytes());
            bytes[directory + 4..directory + 8].copy_from_slice(&40_u32.to_le_bytes());
            bytes[0x200 + 12..0x200 + 16].copy_from_slice(&0x1100_u32.to_le_bytes());
            bytes[0x300..0x300 + name.len()].copy_from_slice(name.as_bytes());
        }
        if let Some(name) = delay {
            let directory = optional + 112 + 13 * 8;
            bytes[directory..directory + 4].copy_from_slice(&0x1040_u32.to_le_bytes());
            bytes[directory + 4..directory + 8].copy_from_slice(&64_u32.to_le_bytes());
            bytes[0x240..0x244].copy_from_slice(&1_u32.to_le_bytes());
            let name_rva: u32 = if malformed_delay { 0xffff_fff0 } else { 0x1120 };
            bytes[0x244..0x248].copy_from_slice(&name_rva.to_le_bytes());
            bytes[0x320..0x320 + name.len()].copy_from_slice(name.as_bytes());
        }
        bytes
    }

    #[test]
    fn imports_record_ordinal_only_and_delay_only_dll_names() {
        let parsed =
            parse(&fixture(Some("ordinal-only.dll"), Some("delay-only.dll"), false)).unwrap();
        assert_eq!(
            parsed.needed,
            BTreeSet::from(["delay-only.dll".into(), "ordinal-only.dll".into()])
        );
        assert_eq!(parse(&fixture(None, Some("delay-only.dll"), false)).unwrap().needed.len(), 1);
    }

    #[test]
    fn invalid_delay_table_rva_is_rejected() {
        assert!(parse(&fixture(None, Some("delay-only.dll"), true)).is_err());
    }
}
