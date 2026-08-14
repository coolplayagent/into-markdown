//! Bounded OPC entry access and canonical package paths.

use crate::workbook::error::{limit, malformed};
use into_markdown_core::ConversionError;
use std::io::{Cursor, Read};
use std::path::{Component, Path};

#[derive(Debug, Clone)]
pub(in crate::workbook) struct PackageEntry {
    pub(in crate::workbook) index: usize,
}

pub(in crate::workbook) fn has_extension(path: &str, extension: &str) -> bool {
    opc_extension(path).is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

pub(in crate::workbook) fn opc_extension(path: &str) -> Option<&str> {
    path.rsplit('/')
        .next()?
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

pub(in crate::workbook) fn canonical_part_name(raw: &str) -> Result<String, ConversionError> {
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.contains('\\')
        || raw.contains('%')
        || raw.bytes().any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(malformed(Some(raw), "unsafe OPC part name"));
    }
    let path = Path::new(raw);
    if path.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err(malformed(Some(raw), "unsafe OPC path component"));
    }
    Ok(raw.to_owned())
}

pub(in crate::workbook) fn read_entry(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    index: usize,
    name: &str,
) -> Result<Vec<u8>, ConversionError> {
    let mut entry = zip
        .by_index(index)
        .map_err(|error| malformed(Some(name), format!("cannot open ZIP part: {error}")))?;
    let expected = usize::try_from(entry.size())
        .map_err(|_| limit("max_decompressed_bytes", "part size does not fit memory"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(expected)
        .map_err(|_| limit("max_memory_bytes", format!("cannot allocate part {name}")))?;
    entry
        .read_to_end(&mut bytes)
        .map_err(|error| malformed(Some(name), format!("cannot decompress ZIP part: {error}")))?;
    if bytes.len() != expected {
        return Err(malformed(Some(name), "ZIP part length disagrees with central directory"));
    }
    Ok(bytes)
}
