use crate::NormalizedFormat;
use into_markdown_core::{ConversionError, ExecutionContext};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use std::collections::BTreeSet;
use std::io::{Cursor, Read as _};

mod fixture;
mod xml;
pub(crate) use fixture::fixture_package;

const EOCD_BYTES: usize = 22;
const MAX_EOCD_SEARCH: usize = EOCD_BYTES + 65_535;
const CENTRAL_BYTES: usize = 46;
const LOCAL_BYTES: usize = 30;
const MAX_ENTRIES: usize = 32_768;
const MAX_NAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_XML_BYTES: u64 = 4 * 1024 * 1024;
const MAX_XML_EVENTS: u64 = 100_000;
const MAX_XML_DEPTH: u16 = 64;

#[derive(Debug)]
struct Entry {
    index: usize,
    name: String,
    local_start: usize,
    physical_end: usize,
    expanded: u64,
    crc32: u32,
}

#[derive(Clone, Copy)]
struct Layout {
    central_start: usize,
    central_end: usize,
    entries: usize,
}

pub(crate) fn audit(
    bytes: &[u8],
    expected: NormalizedFormat,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    if bytes.is_empty()
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > crate::MAX_NORMALIZED_PACKAGE_BYTES
    {
        return Err(limit("normalized_package_bytes", "normalized package size is invalid"));
    }
    let layout = locate(bytes)?;
    if layout.entries == 0 || layout.entries > MAX_ENTRIES {
        return Err(limit("normalized_package_entries", "normalized package entry count exceeded"));
    }
    let entries = collect_entries(bytes, layout, context)?;
    validate_physical_layout(&entries, layout.central_start)?;
    validate_expanded_entries(bytes, &entries, expected, context)
}

fn locate(bytes: &[u8]) -> Result<Layout, ConversionError> {
    if bytes.len() < EOCD_BYTES {
        return Err(malformed("ZIP end-of-central-directory record is missing"));
    }
    let lower = bytes.len().saturating_sub(MAX_EOCD_SEARCH);
    let mut match_at = None;
    for candidate in lower..=bytes.len() - EOCD_BYTES {
        if bytes.get(candidate..candidate + 4) != Some(b"PK\x05\x06") {
            continue;
        }
        let comment = usize::from(le16(bytes, candidate + 20)?);
        if candidate.checked_add(EOCD_BYTES).and_then(|value| value.checked_add(comment))
            == Some(bytes.len())
            && match_at.replace(candidate).is_some()
        {
            return Err(malformed("ZIP end-of-central-directory record is ambiguous"));
        }
    }
    let eocd = match_at.ok_or_else(|| malformed("ZIP envelope has trailing or missing data"))?;
    if eocd >= 20 && bytes.get(eocd - 20..eocd - 16) == Some(b"PK\x06\x07") {
        return Err(malformed("ZIP64 normalized packages are not accepted"));
    }
    let disk = le16(bytes, eocd + 4)?;
    let central_disk = le16(bytes, eocd + 6)?;
    let disk_entries = le16(bytes, eocd + 8)?;
    let total_entries = le16(bytes, eocd + 10)?;
    let central_size = le32(bytes, eocd + 12)?;
    let central_offset = le32(bytes, eocd + 16)?;
    if disk != 0 || central_disk != 0 || disk_entries != total_entries {
        return Err(malformed("multi-disk normalized packages are forbidden"));
    }
    if total_entries == u16::MAX || central_size == u32::MAX || central_offset == u32::MAX {
        return Err(malformed("ZIP64 normalized packages are not accepted"));
    }
    let central_start = usize::try_from(central_offset)
        .map_err(|_| malformed("ZIP central-directory offset overflowed"))?;
    let central_size = usize::try_from(central_size)
        .map_err(|_| malformed("ZIP central-directory size overflowed"))?;
    if central_start.checked_add(central_size) != Some(eocd) {
        return Err(malformed("ZIP central directory does not exactly precede EOCD"));
    }
    Ok(Layout { central_start, central_end: eocd, entries: usize::from(total_entries) })
}

fn collect_entries(
    bytes: &[u8],
    layout: Layout,
    context: &ExecutionContext,
) -> Result<Vec<Entry>, ConversionError> {
    let mut entries = Vec::new();
    entries.try_reserve_exact(layout.entries).map_err(|_| {
        limit("normalized_package_metadata", "cannot reserve normalized ZIP metadata")
    })?;
    let mut names = BTreeSet::new();
    let mut total_name_bytes = 0_usize;
    let mut cursor = layout.central_start;
    for index in 0..layout.entries {
        context.checkpoint()?;
        let header = slice(bytes, cursor, CENTRAL_BYTES, "central header")?;
        if header.get(..4) != Some(b"PK\x01\x02") {
            return Err(malformed("ZIP central header signature is invalid"));
        }
        let flags = le16(header, 8)?;
        let method = le16(header, 10)?;
        validate_flags(flags, method)?;
        let crc = le32(header, 16)?;
        let compressed = le32(header, 20)?;
        let expanded = le32(header, 24)?;
        let name_len = usize::from(le16(header, 28)?);
        let extra_len = usize::from(le16(header, 30)?);
        let comment_len = usize::from(le16(header, 32)?);
        if le16(header, 34)? != 0
            || compressed == u32::MAX
            || expanded == u32::MAX
            || le32(header, 42)? == u32::MAX
        {
            return Err(malformed("ZIP64 or multi-disk member is forbidden"));
        }
        reject_link(header)?;
        let name_start =
            cursor.checked_add(CENTRAL_BYTES).ok_or_else(|| malformed("ZIP name offset"))?;
        let extra_start =
            name_start.checked_add(name_len).ok_or_else(|| malformed("ZIP extra offset"))?;
        let comment_start =
            extra_start.checked_add(extra_len).ok_or_else(|| malformed("ZIP comment offset"))?;
        let next = comment_start
            .checked_add(comment_len)
            .ok_or_else(|| malformed("ZIP central record length"))?;
        if next > layout.central_end {
            return Err(malformed("ZIP central record is truncated"));
        }
        let raw_name = slice(bytes, name_start, name_len, "central name")?;
        if !raw_name.is_ascii() && flags & 0x0800 == 0 {
            return Err(malformed("non-ASCII ZIP name lacks UTF-8 flag"));
        }
        let name =
            std::str::from_utf8(raw_name).map_err(|_| malformed("ZIP member name is not UTF-8"))?;
        validate_name(name)?;
        total_name_bytes = total_name_bytes
            .checked_add(name.len())
            .ok_or_else(|| limit("normalized_package_metadata", "ZIP name inventory overflowed"))?;
        if total_name_bytes > MAX_NAME_BYTES || !names.insert(name.to_owned()) {
            return Err(malformed("ZIP member names are duplicated or excessive"));
        }
        validate_extra(slice(bytes, extra_start, extra_len, "central extra")?)?;
        let local_start = usize::try_from(le32(header, 42)?)
            .map_err(|_| malformed("ZIP local offset overflowed"))?;
        let physical_end = validate_local(
            bytes,
            layout.central_start,
            local_start,
            raw_name,
            flags,
            method,
            crc,
            compressed,
            expanded,
        )?;
        entries.push(Entry {
            index,
            name: name.to_owned(),
            local_start,
            physical_end,
            expanded: u64::from(expanded),
            crc32: crc,
        });
        cursor = next;
    }
    if cursor != layout.central_end {
        return Err(malformed("ZIP central count and size disagree"));
    }
    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
fn validate_local(
    bytes: &[u8],
    central_start: usize,
    local_start: usize,
    name: &[u8],
    flags: u16,
    method: u16,
    crc: u32,
    compressed: u32,
    expanded: u32,
) -> Result<usize, ConversionError> {
    if local_start >= central_start {
        return Err(malformed("ZIP member starts inside the central directory"));
    }
    let local = slice(bytes, local_start, LOCAL_BYTES, "local header")?;
    if local.get(..4) != Some(b"PK\x03\x04")
        || le16(local, 6)? != flags
        || le16(local, 8)? != method
    {
        return Err(malformed("ZIP local and central headers disagree"));
    }
    let local_name_len = usize::from(le16(local, 26)?);
    let local_extra_len = usize::from(le16(local, 28)?);
    let name_start =
        local_start.checked_add(LOCAL_BYTES).ok_or_else(|| malformed("ZIP local name"))?;
    let extra_start =
        name_start.checked_add(local_name_len).ok_or_else(|| malformed("ZIP local extra"))?;
    let data_start =
        extra_start.checked_add(local_extra_len).ok_or_else(|| malformed("ZIP data offset"))?;
    if slice(bytes, name_start, local_name_len, "local name")? != name {
        return Err(malformed("ZIP local and central names disagree"));
    }
    validate_extra(slice(bytes, extra_start, local_extra_len, "local extra")?)?;
    let local_values = (le32(local, 14)?, le32(local, 18)?, le32(local, 22)?);
    let descriptor = flags & 0x0008 != 0;
    if descriptor {
        if local_values != (0, 0, 0) && local_values != (crc, compressed, expanded) {
            return Err(malformed("ZIP descriptor placeholders disagree"));
        }
    } else if local_values != (crc, compressed, expanded) {
        return Err(malformed("ZIP local sizes or CRC disagree"));
    }
    let data_end = data_start
        .checked_add(usize::try_from(compressed).map_err(|_| malformed("ZIP data size"))?)
        .ok_or_else(|| malformed("ZIP data range overflowed"))?;
    let end = if descriptor {
        validate_descriptor(bytes, data_end, crc, compressed, expanded)?
    } else {
        data_end
    };
    if end > central_start {
        return Err(malformed("ZIP member overlaps central directory"));
    }
    Ok(end)
}

fn validate_descriptor(
    bytes: &[u8],
    start: usize,
    crc: u32,
    compressed: u32,
    expanded: u32,
) -> Result<usize, ConversionError> {
    let signature = bytes.get(start..start.saturating_add(4)) == Some(b"PK\x07\x08");
    let body = start
        .checked_add(if signature { 4 } else { 0 })
        .ok_or_else(|| malformed("ZIP descriptor"))?;
    if (le32(bytes, body)?, le32(bytes, body + 4)?, le32(bytes, body + 8)?)
        != (crc, compressed, expanded)
    {
        return Err(malformed("ZIP data descriptor disagrees"));
    }
    start
        .checked_add(if signature { 16 } else { 12 })
        .ok_or_else(|| malformed("ZIP descriptor range"))
}

fn validate_physical_layout(
    entries: &[Entry],
    central_start: usize,
) -> Result<(), ConversionError> {
    let mut ranges =
        entries.iter().map(|entry| (entry.local_start, entry.physical_end)).collect::<Vec<_>>();
    ranges.sort_unstable();
    let mut expected_start = 0_usize;
    for (start, end) in ranges {
        if start != expected_start || end <= start {
            return Err(malformed("ZIP members overlap or hide unclaimed bytes"));
        }
        expected_start = end;
    }
    if expected_start != central_start {
        return Err(malformed("ZIP local records do not exactly reach the central directory"));
    }
    Ok(())
}

fn validate_expanded_entries(
    bytes: &[u8],
    entries: &[Entry],
    expected: NormalizedFormat,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        malformed(format!("ZIP constructor rejected validated envelope: {error}"))
    })?;
    if archive.len() != entries.len() {
        return Err(malformed("ZIP parser inventory disagrees with central directory"));
    }
    let mut expanded_total = 0_u64;
    let mut content_types = None;
    let mut relationships = None;
    for entry in entries {
        context.checkpoint()?;
        expanded_total = expanded_total.checked_add(entry.expanded).ok_or_else(|| {
            limit("normalized_package_expanded", "normalized package expansion overflowed")
        })?;
        if expanded_total > crate::MAX_NORMALIZED_PACKAGE_BYTES {
            return Err(limit("normalized_package_expanded", "normalized package expands too far"));
        }
        let capture = matches!(entry.name.as_str(), "[Content_Types].xml" | "_rels/.rels");
        let expanded = read_and_validate_entry(&mut archive, entry, capture, context)?;
        match entry.name.as_str() {
            "[Content_Types].xml" => content_types = Some(expanded),
            "_rels/.rels" => relationships = Some(expanded),
            _ => {}
        }
    }
    let content_types = content_types.ok_or_else(|| malformed("[Content_Types].xml is missing"))?;
    let relationships = relationships.ok_or_else(|| malformed("_rels/.rels is missing"))?;
    let (main_part, main_type) = expected_authority(expected);
    if !entries.iter().any(|entry| entry.name == main_part) {
        return Err(malformed("declared OOXML main part is missing"));
    }
    validate_content_types(&content_types, main_part, main_type, context)?;
    xml::validate_root_relationships(&relationships, main_part, context)
}

fn read_and_validate_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    metadata: &Entry,
    capture: bool,
    context: &ExecutionContext,
) -> Result<Vec<u8>, ConversionError> {
    if capture && metadata.expanded > MAX_XML_BYTES {
        return Err(limit("normalized_package_xml", "OPC authority XML is too large"));
    }
    let mut file = archive.by_index(metadata.index).map_err(|error| {
        malformed(format!("cannot open normalized member {}: {error}", metadata.name))
    })?;
    if file.encrypted() || file.name() != metadata.name || file.size() != metadata.expanded {
        return Err(malformed("ZIP parser metadata disagrees with validated member"));
    }
    let capacity = if capture {
        usize::try_from(metadata.expanded)
            .map_err(|_| limit("normalized_package_xml", "OPC XML size overflowed"))?
    } else {
        0
    };
    let mut captured = Vec::new();
    captured
        .try_reserve_exact(capacity)
        .map_err(|_| limit("normalized_package_metadata", "cannot reserve OPC authority XML"))?;
    let mut total = 0_u64;
    let mut crc = crc32fast::Hasher::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        context.checkpoint()?;
        let count = file.read(&mut buffer).map_err(|error| {
            malformed(format!("normalized member {} failed CRC/decode: {error}", metadata.name))
        })?;
        if count == 0 {
            break;
        }
        total = total.checked_add(u64::try_from(count).unwrap_or(u64::MAX)).ok_or_else(|| {
            limit("normalized_package_expanded", "normalized member length overflowed")
        })?;
        if total > metadata.expanded {
            return Err(malformed("normalized member exceeds declared length"));
        }
        crc.update(&buffer[..count]);
        if capture {
            captured.extend_from_slice(&buffer[..count]);
        }
    }
    if total != metadata.expanded || crc.finalize() != metadata.crc32 {
        return Err(malformed("normalized member length or CRC is invalid"));
    }
    Ok(captured)
}

fn validate_content_types(
    xml: &[u8],
    expected_part: &str,
    expected_type: &str,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = true;
    let mut depth = 0_u16;
    let mut events = 0_u64;
    let mut root = false;
    let mut matching = 0_u8;
    let mut recognized = 0_u8;
    loop {
        context.checkpoint()?;
        events = events.checked_add(1).ok_or_else(|| malformed("content types event overflow"))?;
        if events > MAX_XML_EVENTS {
            return Err(limit("normalized_package_xml", "content types has too many XML events"));
        }
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                depth = depth.checked_add(1).ok_or_else(|| malformed("content types depth"))?;
                if depth > MAX_XML_DEPTH {
                    return Err(limit("normalized_package_xml", "content types XML is too deep"));
                }
                if depth == 1 {
                    root = xml::root_namespace(
                        &event,
                        b"Types",
                        "http://schemas.openxmlformats.org/package/2006/content-types",
                    )?;
                } else if depth == 2 && event.name().as_ref() == b"Override" {
                    inspect_override(
                        &event,
                        expected_part,
                        expected_type,
                        &mut matching,
                        &mut recognized,
                    )?;
                }
            }
            Ok(Event::Empty(event)) => {
                if depth == 1 && event.name().as_ref() == b"Override" {
                    inspect_override(
                        &event,
                        expected_part,
                        expected_type,
                        &mut matching,
                        &mut recognized,
                    )?;
                }
            }
            Ok(Event::End(_)) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| malformed("content types XML is unbalanced"))?;
            }
            Ok(Event::DocType(_)) => return Err(malformed("DTD is forbidden in content types")),
            Ok(Event::Eof) => break,
            Err(error) => return Err(malformed(format!("invalid content types XML: {error}"))),
            _ => {}
        }
    }
    if !root || depth != 0 || matching != 1 || recognized != 1 {
        return Err(malformed("content types do not uniquely identify the expected OOXML family"));
    }
    Ok(())
}

fn inspect_override(
    event: &BytesStart<'_>,
    expected_part: &str,
    expected_type: &str,
    matching: &mut u8,
    recognized: &mut u8,
) -> Result<(), ConversionError> {
    let mut part = None;
    let mut kind = None;
    let mut seen = BTreeSet::new();
    for attribute in event.attributes().with_checks(false) {
        let attribute = attribute.map_err(|_| malformed("invalid content type attribute"))?;
        if !seen.insert(attribute.key.as_ref().to_vec()) {
            return Err(malformed("duplicate content type attribute"));
        }
        let value = attribute
            .unescape_value()
            .map_err(|_| malformed("invalid content type attribute value"))?
            .into_owned();
        match attribute.key.as_ref() {
            b"PartName" => part = Some(value),
            b"ContentType" => kind = Some(value),
            _ => {}
        }
    }
    let part = part.unwrap_or_default();
    let kind = kind.unwrap_or_default();
    if is_main_content_type(&kind) {
        *recognized =
            recognized.checked_add(1).ok_or_else(|| malformed("too many main content types"))?;
        if part == format!("/{expected_part}") && kind == expected_type {
            *matching = matching
                .checked_add(1)
                .ok_or_else(|| malformed("duplicate expected content type"))?;
        }
    }
    Ok(())
}

fn validate_flags(flags: u16, method: u16) -> Result<(), ConversionError> {
    if flags & 0x2041 != 0 {
        return Err(ConversionError::Encrypted);
    }
    if !matches!(method, 0 | 8) || flags & !0x080e != 0 || method == 0 && flags & 0x0006 != 0 {
        return Err(malformed("unsupported ZIP compression flags or method"));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), ConversionError> {
    if name.is_empty()
        || name.len() > 1_024
        || name.starts_with('/')
        || name.contains('\\')
        || name.bytes().any(|byte| byte.is_ascii_control() || byte == b':')
    {
        return Err(malformed("ZIP member path is not canonical"));
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    if trimmed.is_empty()
        || trimmed.split('/').any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(malformed("ZIP member path escapes or has empty components"));
    }
    Ok(())
}

fn validate_extra(extra: &[u8]) -> Result<(), ConversionError> {
    let mut cursor = 0_usize;
    while cursor < extra.len() {
        let header = slice(extra, cursor, 4, "ZIP extra field")?;
        let id = le16(header, 0)?;
        let size = usize::from(le16(header, 2)?);
        cursor = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(size))
            .ok_or_else(|| malformed("ZIP extra field length overflowed"))?;
        if cursor > extra.len() || id == 0x0001 {
            return Err(malformed("truncated or ZIP64 extra field is forbidden"));
        }
    }
    Ok(())
}

fn reject_link(header: &[u8]) -> Result<(), ConversionError> {
    let creator = (le16(header, 4)? >> 8) as u8;
    let mode = le32(header, 38)? >> 16;
    if creator == 3 && mode & 0o170_000 == 0o120_000 {
        return Err(malformed("symbolic-link ZIP members are forbidden"));
    }
    Ok(())
}

fn expected_authority(format: NormalizedFormat) -> (&'static str, &'static str) {
    match format {
        NormalizedFormat::Docx => (
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        ),
        NormalizedFormat::Pptx => (
            "ppt/presentation.xml",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        ),
        NormalizedFormat::Xlsx => (
            "xl/workbook.xml",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
        ),
    }
}

fn is_main_content_type(value: &str) -> bool {
    matches!(
        value,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"
    )
}

fn slice<'a>(
    bytes: &'a [u8],
    start: usize,
    length: usize,
    field: &str,
) -> Result<&'a [u8], ConversionError> {
    let end = start.checked_add(length).ok_or_else(|| malformed(format!("{field} overflowed")))?;
    bytes.get(start..end).ok_or_else(|| malformed(format!("{field} is truncated")))
}

fn le16(bytes: &[u8], start: usize) -> Result<u16, ConversionError> {
    let value = slice(bytes, start, 2, "ZIP u16")?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn le32(bytes: &[u8], start: usize) -> Result<u32, ConversionError> {
    let value = slice(bytes, start, 4, "ZIP u32")?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("normalized-package".into()), detail: detail.into() }
}

fn limit(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}

#[cfg(test)]
mod tests;
