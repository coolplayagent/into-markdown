//! Allocation-gated parsing of the raw ZIP central directory.
//!
//! No third-party ZIP constructor runs until the EOCD, every central/local
//! header, the complete physical range inventory, and the metadata working set
//! have been validated against the request-scoped limits.

use super::archive::EntryMeta;
use super::budget::ArchiveBudget;
use super::entry_policy::{EntryKind, EntryPolicy};
use into_markdown_core::{ConversionError, ResourceReservation};

const EOCD_LEN: usize = 22;
const MAX_EOCD_SEARCH: usize = EOCD_LEN + 65_535;
const CENTRAL_LEN: usize = 46;
const LOCAL_LEN: usize = 30;
const ENTRY_WORK_BYTES: u64 = 4_096;
const CENTRAL_WORK_FACTOR: u64 = 8;
const NAME_WORK_FACTOR: u64 = 32;
const FIXED_WORK_BYTES: u64 = 4_096;

pub(super) struct RawInventory {
    pub(super) entries: Vec<EntryMeta>,
    pub(super) memory: ResourceReservation,
}

#[derive(Clone, Copy)]
struct Layout {
    central_start: usize,
    central_end: usize,
    archive_offset: usize,
    entries: usize,
    name_bytes: u64,
}

#[derive(Clone)]
struct Record<'a> {
    raw_name: &'a [u8],
    name: String,
    kind: EntryKind,
    mode: Option<u32>,
    method: u16,
    compressed: u64,
    expanded: u64,
    local_start: usize,
    physical_end: usize,
    central_extra_len: usize,
    local_extra_len: usize,
}

pub(super) fn preflight(
    bytes: &[u8],
    depth: u16,
    budget: &mut ArchiveBudget<'_>,
) -> Result<RawInventory, ConversionError> {
    let mut layout = locate(bytes)?;
    budget.enter_archive(depth, layout.entries)?;
    scan_records(bytes, &mut layout, budget)?;
    let plan = allocation_plan(layout)?;
    let memory = budget.context().reserve_memory(plan)?;
    let entries = collect_records(bytes, layout, budget)?;
    Ok(RawInventory { entries, memory })
}

fn locate(bytes: &[u8]) -> Result<Layout, ConversionError> {
    if bytes.len() < EOCD_LEN {
        return Err(malformed("end-of-central-directory record is missing"));
    }
    let lower = bytes.len().saturating_sub(MAX_EOCD_SEARCH);
    let mut candidate = bytes.len() - EOCD_LEN;
    loop {
        if bytes.get(candidate..candidate + 4) == Some(b"PK\x05\x06") {
            let comment = usize::from(le16(bytes, candidate + 20)?);
            if candidate.checked_add(EOCD_LEN).and_then(|end| end.checked_add(comment))
                == Some(bytes.len())
            {
                return parse_eocd(bytes, candidate);
            }
        }
        if candidate == lower {
            break;
        }
        candidate -= 1;
    }
    Err(malformed("end-of-central-directory record is missing or ambiguous"))
}

fn parse_eocd(bytes: &[u8], eocd: usize) -> Result<Layout, ConversionError> {
    if eocd >= 20 && bytes.get(eocd - 20..eocd - 16) == Some(b"PK\x06\x07") {
        return Err(malformed("ZIP64 archives are not accepted by the recursive converter"));
    }
    let disk = le16(bytes, eocd + 4)?;
    let central_disk = le16(bytes, eocd + 6)?;
    let disk_entries = le16(bytes, eocd + 8)?;
    let total_entries = le16(bytes, eocd + 10)?;
    let central_size = le32(bytes, eocd + 12)?;
    let central_offset = le32(bytes, eocd + 16)?;
    if disk != 0 || central_disk != 0 || disk_entries != total_entries {
        return Err(malformed("multi-disk ZIP archives are not accepted"));
    }
    if total_entries == u16::MAX || central_size == u32::MAX || central_offset == u32::MAX {
        return Err(malformed("ZIP64 archives are not accepted by the recursive converter"));
    }
    let central_size = usize::try_from(central_size).map_err(|_| malformed("central size"))?;
    let central_offset =
        usize::try_from(central_offset).map_err(|_| malformed("central offset"))?;
    let central_start = eocd
        .checked_sub(central_size)
        .ok_or_else(|| malformed("central directory starts before the input"))?;
    let archive_offset = central_start
        .checked_sub(central_offset)
        .ok_or_else(|| malformed("central directory offset exceeds its physical position"))?;
    if central_start.checked_add(central_size) != Some(eocd) {
        return Err(malformed("central directory does not end at EOCD"));
    }
    Ok(Layout {
        central_start,
        central_end: eocd,
        archive_offset,
        entries: usize::from(total_entries),
        name_bytes: 0,
    })
}

/// First pass: fixed scratch only. This validates every record and computes an
/// exact upper bound for attacker-controlled name bytes before any collection.
fn scan_records(
    bytes: &[u8],
    layout: &mut Layout,
    budget: &ArchiveBudget<'_>,
) -> Result<(), ConversionError> {
    let mut cursor = layout.central_start;
    let mut names = 0_u64;
    for index in 0..layout.entries {
        budget.context().checkpoint()?;
        let (record, next) = record_at(bytes, *layout, cursor, budget.zip_charset())?;
        budget.validate_member(&record.name, record.compressed, record.expanded)?;
        names = names
            .checked_add(u64::try_from(record.raw_name.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| memory_limit("ZIP name inventory overflowed"))?;
        cursor = next;
        if cursor > layout.central_end {
            return Err(malformed(format!("central record {index} exceeds the directory")));
        }
    }
    if cursor != layout.central_end {
        return Err(malformed("central record count/size disagree with EOCD"));
    }
    layout.name_bytes = names;
    Ok(())
}

fn collect_records(
    bytes: &[u8],
    layout: Layout,
    budget: &ArchiveBudget<'_>,
) -> Result<Vec<EntryMeta>, ConversionError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(layout.entries)
        .map_err(|error| memory_limit(format!("reserve ZIP metadata: {error}")))?;
    let mut occupied = Vec::new();
    occupied
        .try_reserve_exact(layout.entries)
        .map_err(|error| memory_limit(format!("reserve ZIP physical ranges: {error}")))?;
    let mut policy = EntryPolicy::default();
    let mut cursor = layout.central_start;
    for index in 0..layout.entries {
        budget.context().checkpoint()?;
        let (record, next) = record_at(bytes, layout, cursor, budget.zip_charset())?;
        let (name, kind) =
            policy.accept(&record.name, record.mode, record.kind == EntryKind::Directory)?;
        occupied.push((record.local_start, record.physical_end, name.clone()));
        entries.push(EntryMeta {
            index,
            name,
            kind,
            compressed_size: record.compressed,
            expanded_size: record.expanded,
            deflated: record.method == 8,
            physical_start: record.local_start,
            central_extra_len: record.central_extra_len,
            local_extra_len: record.local_extra_len,
            verified: false,
        });
        cursor = next;
    }
    occupied.sort_by_key(|range| range.0);
    for pair in occupied.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(malformed(format!(
                "entry data ranges overlap: {:?} and {:?}",
                pair[0].2, pair[1].2
            )));
        }
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name).then(left.index.cmp(&right.index)));
    Ok(entries)
}

fn record_at<'a>(
    bytes: &'a [u8],
    layout: Layout,
    central: usize,
    explicit_charset: Option<&str>,
) -> Result<(Record<'a>, usize), ConversionError> {
    let header = slice(bytes, central, CENTRAL_LEN, "central header")?;
    if header.get(..4) != Some(b"PK\x01\x02") {
        return Err(malformed("central header signature is invalid"));
    }
    let flags = le16(header, 8)?;
    let method = le16(header, 10)?;
    validate_flags(flags, method)?;
    let crc = le32(header, 16)?;
    let compressed32 = le32(header, 20)?;
    let expanded32 = le32(header, 24)?;
    let local_relative = le32(header, 42)?;
    if compressed32 == u32::MAX || expanded32 == u32::MAX || local_relative == u32::MAX {
        return Err(malformed("ZIP64 members are not accepted by the recursive converter"));
    }
    if le16(header, 34)? != 0 {
        return Err(malformed("multi-disk ZIP members are not accepted"));
    }
    let name_len = usize::from(le16(header, 28)?);
    let extra_len = usize::from(le16(header, 30)?);
    let comment_len = usize::from(le16(header, 32)?);
    let name_start = central.checked_add(CENTRAL_LEN).ok_or_else(|| malformed("name offset"))?;
    let extra_start = name_start.checked_add(name_len).ok_or_else(|| malformed("extra offset"))?;
    let comment_start =
        extra_start.checked_add(extra_len).ok_or_else(|| malformed("comment offset"))?;
    let next = comment_start.checked_add(comment_len).ok_or_else(|| malformed("record length"))?;
    if next > layout.central_end {
        return Err(malformed("central variable fields are truncated"));
    }
    let raw_name = slice(bytes, name_start, name_len, "central name")?;
    let extra = slice(bytes, extra_start, extra_len, "central extra field")?;
    validate_extra(extra)?;
    let name = decode_name(raw_name, flags, extra, explicit_charset)?;
    let compressed = u64::from(compressed32);
    let expanded = u64::from(expanded32);
    let local_start = layout
        .archive_offset
        .checked_add(usize::try_from(local_relative).map_err(|_| malformed("local offset"))?)
        .ok_or_else(|| malformed("local header offset overflowed"))?;
    if local_start >= layout.central_start {
        return Err(malformed("local header starts in the central directory"));
    }
    let (physical_end, local_extra_len) = validate_local(
        bytes,
        layout.central_start,
        local_start,
        raw_name,
        flags,
        method,
        crc,
        compressed32,
        expanded32,
    )?;
    let made_by = le16(header, 4)?;
    let external = le32(header, 38)?;
    let mode = unix_mode((made_by >> 8) as u8, external);
    let kind = if raw_name.ends_with(b"/") { EntryKind::Directory } else { EntryKind::File };
    Ok((
        Record {
            raw_name,
            name,
            kind,
            mode,
            method,
            compressed,
            expanded,
            local_start,
            physical_end,
            central_extra_len: extra_len,
            local_extra_len,
        },
        next,
    ))
}

fn decode_name(
    raw_name: &[u8],
    flags: u16,
    extra: &[u8],
    explicit_charset: Option<&str>,
) -> Result<String, ConversionError> {
    if let Some(name) = unicode_path_name(raw_name, extra)? {
        return Ok(name);
    }
    if flags & 0x0800 != 0 {
        return String::from_utf8(raw_name.to_vec())
            .map_err(|_| malformed("UTF-8 flagged entry name is not valid UTF-8"));
    }
    if let Some(label) = explicit_charset {
        let encoding = encoding_rs::Encoding::for_label(label.trim().as_bytes())
            .ok_or_else(|| malformed(format!("unsupported --zip-charset label {label:?}")))?;
        let (decoded, had_errors) = encoding.decode_without_bom_handling(raw_name);
        if had_errors {
            return Err(malformed(format!("entry name is invalid for --zip-charset {label:?}")));
        }
        return Ok(decoded.into_owned());
    }
    Ok(decode_cp437(raw_name))
}

fn unicode_path_name(raw_name: &[u8], extra: &[u8]) -> Result<Option<String>, ConversionError> {
    let mut cursor = 0;
    let mut decoded = None;
    while cursor < extra.len() {
        let header = slice(extra, cursor, 4, "extra-field header")?;
        let id = le16(header, 0)?;
        let size = usize::from(le16(header, 2)?);
        let body_start = cursor.checked_add(4).ok_or_else(|| malformed("extra field offset"))?;
        let body = slice(extra, body_start, size, "extra-field body")?;
        cursor = body_start.checked_add(size).ok_or_else(|| malformed("extra field end"))?;
        if id != 0x7075 || body.len() < 5 || body[0] != 1 {
            continue;
        }
        let expected_crc = le32(body, 1)?;
        if crc32fast::hash(raw_name) != expected_crc {
            continue;
        }
        let candidate = std::str::from_utf8(&body[5..])
            .map_err(|_| malformed("Unicode Path extra field is not valid UTF-8"))?;
        if decoded.as_deref().is_some_and(|previous| previous != candidate) {
            return Err(malformed("ambiguous Unicode Path extra fields"));
        }
        decoded = Some(candidate.to_owned());
    }
    Ok(decoded)
}

fn decode_cp437(raw_name: &[u8]) -> String {
    const HIGH: [char; 128] = [
        'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ',
        'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú',
        'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', 'Á',
        'Â', 'À', '©', '╣', '║', '╗', '╝', '¢', '¥', '┐', '└', '┴', '┬', '├', '─', '┼', 'ã', 'Ã',
        '╚', '╔', '╩', '╦', '╠', '═', '╬', '¤', 'ð', 'Ð', 'Ê', 'Ë', 'È', 'ı', 'Í', 'Î', 'Ï', '┘',
        '┌', '█', '▄', '¦', 'Ì', '▀', 'Ó', 'ß', 'Ô', 'Ò', 'õ', 'Õ', 'µ', 'þ', 'Þ', 'Ú', 'Û', 'Ù',
        'ý', 'Ý', '¯', '´', '≡', '±', '‗', '¾', '¶', '§', '÷', '¸', '°', '¨', '·', '¹', '³', '²',
        '■', '\u{a0}',
    ];
    raw_name
        .iter()
        .map(
            |byte| {
                if byte.is_ascii() { char::from(*byte) } else { HIGH[usize::from(*byte - 0x80)] }
            },
        )
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn validate_local(
    bytes: &[u8],
    central_start: usize,
    local_start: usize,
    central_name: &[u8],
    flags: u16,
    method: u16,
    crc: u32,
    compressed: u32,
    expanded: u32,
) -> Result<(usize, usize), ConversionError> {
    let local = slice(bytes, local_start, LOCAL_LEN, "local header")?;
    if local.get(..4) != Some(b"PK\x03\x04") {
        return Err(malformed("local header signature is invalid"));
    }
    if le16(local, 6)? != flags || le16(local, 8)? != method {
        return Err(malformed("local and central flags or methods disagree"));
    }
    let name_len = usize::from(le16(local, 26)?);
    let extra_len = usize::from(le16(local, 28)?);
    let name_start = local_start.checked_add(LOCAL_LEN).ok_or_else(|| malformed("local name"))?;
    let extra_start = name_start.checked_add(name_len).ok_or_else(|| malformed("local extra"))?;
    let data_start = extra_start.checked_add(extra_len).ok_or_else(|| malformed("data offset"))?;
    let local_name = slice(bytes, name_start, name_len, "local name")?;
    if local_name != central_name {
        return Err(malformed("local and central names disagree"));
    }
    validate_extra(slice(bytes, extra_start, extra_len, "local extra field")?)?;
    let descriptor = flags & 0x0008 != 0;
    let local_values = (le32(local, 14)?, le32(local, 18)?, le32(local, 22)?);
    if descriptor {
        if local_values != (0, 0, 0) && local_values != (crc, compressed, expanded) {
            return Err(malformed("local descriptor placeholders disagree"));
        }
    } else if local_values != (crc, compressed, expanded) {
        return Err(malformed("local sizes or CRC disagree with central metadata"));
    }
    let data_end = data_start
        .checked_add(usize::try_from(compressed).map_err(|_| malformed("data size"))?)
        .ok_or_else(|| malformed("compressed data range overflowed"))?;
    let physical_end = if descriptor {
        validate_descriptor(bytes, data_end, crc, compressed, expanded)?
    } else {
        data_end
    };
    if physical_end > central_start {
        return Err(malformed("entry overlaps the central directory"));
    }
    Ok((physical_end, extra_len))
}

fn validate_descriptor(
    bytes: &[u8],
    start: usize,
    crc: u32,
    compressed: u32,
    expanded: u32,
) -> Result<usize, ConversionError> {
    let signature = bytes.get(start..start.saturating_add(4)) == Some(b"PK\x07\x08");
    let body =
        start.checked_add(if signature { 4 } else { 0 }).ok_or_else(|| malformed("descriptor"))?;
    if (le32(bytes, body)?, le32(bytes, body + 4)?, le32(bytes, body + 8)?)
        != (crc, compressed, expanded)
    {
        return Err(malformed("data descriptor disagrees with central metadata"));
    }
    start.checked_add(if signature { 16 } else { 12 }).ok_or_else(|| malformed("descriptor range"))
}

fn validate_flags(flags: u16, method: u16) -> Result<(), ConversionError> {
    if flags & 0x2041 != 0 {
        return Err(ConversionError::Encrypted);
    }
    if !matches!(method, 0 | 8) {
        return Err(malformed(format!("unsupported compression method {method}")));
    }
    // Bits 1-2 are compressor hints for methods that define them. Some
    // producers retain those inert hints on stored members; they do not alter
    // framing, encryption, sizes, CRC validation, or the selected decoder.
    if flags & !0x080e != 0 {
        return Err(malformed(format!("unsupported general-purpose flags {flags:#06x}")));
    }
    Ok(())
}

fn validate_extra(extra: &[u8]) -> Result<(), ConversionError> {
    let mut cursor = 0;
    while cursor < extra.len() {
        let header = slice(extra, cursor, 4, "extra-field header")?;
        let id = le16(header, 0)?;
        let size = usize::from(le16(header, 2)?);
        cursor = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(size))
            .ok_or_else(|| malformed("extra-field length overflowed"))?;
        if cursor > extra.len() {
            return Err(malformed("extra field is truncated"));
        }
        if id == 0x0001 {
            return Err(malformed("ZIP64 extra fields are not accepted"));
        }
        if id == 0x9901 {
            return Err(ConversionError::Encrypted);
        }
    }
    Ok(())
}

fn unix_mode(system: u8, external: u32) -> Option<u32> {
    if external == 0 {
        None
    } else if system == 3 {
        Some(external >> 16)
    } else if system == 0 {
        Some(if external & 0x10 != 0 { 0o040_775 } else { 0o100_664 })
    } else {
        None
    }
}

fn allocation_plan(layout: Layout) -> Result<u64, ConversionError> {
    let entries = u64::try_from(layout.entries).map_err(|_| memory_limit("entry count"))?;
    let central = u64::try_from(layout.central_end - layout.central_start)
        .map_err(|_| memory_limit("central directory size"))?;
    FIXED_WORK_BYTES
        .checked_add(
            entries.checked_mul(ENTRY_WORK_BYTES).ok_or_else(|| memory_limit("entry work"))?,
        )
        .and_then(|value| value.checked_add(central.checked_mul(CENTRAL_WORK_FACTOR)?))
        .and_then(|value| value.checked_add(layout.name_bytes.checked_mul(NAME_WORK_FACTOR)?))
        .ok_or_else(|| memory_limit("ZIP metadata working set overflowed"))
}

#[cfg(test)]
pub(super) fn planned_memory(bytes: &[u8]) -> Result<u64, ConversionError> {
    let mut layout = locate(bytes)?;
    let options = into_markdown_core::ConversionOptions::default();
    let context = into_markdown_core::ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        options.limits.clone(),
    );
    let budget = ArchiveBudget::new(&options, &context);
    scan_records(bytes, &mut layout, &budget)?;
    allocation_plan(layout)
}

fn slice<'a>(
    bytes: &'a [u8],
    offset: usize,
    length: usize,
    label: &str,
) -> Result<&'a [u8], ConversionError> {
    let end =
        offset.checked_add(length).ok_or_else(|| malformed(format!("{label} range overflowed")))?;
    bytes.get(offset..end).ok_or_else(|| malformed(format!("{label} is truncated")))
}

fn le16(bytes: &[u8], offset: usize) -> Result<u16, ConversionError> {
    Ok(u16::from_le_bytes(
        slice(bytes, offset, 2, "u16")?.try_into().map_err(|_| malformed("invalid u16"))?,
    ))
}

fn le32(bytes: &[u8], offset: usize) -> Result<u32, ConversionError> {
    Ok(u32::from_le_bytes(
        slice(bytes, offset, 4, "u32")?.try_into().map_err(|_| malformed("invalid u32"))?,
    ))
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: None, detail: format!("ZIP: {}", detail.into()) }
}

fn memory_limit(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}

#[cfg(test)]
mod filename_tests {
    use super::*;

    #[test]
    fn decoding_obeys_unicode_utf8_override_and_cp437_precedence() {
        assert_eq!(decode_name(b"caf\x82.txt", 0, &[], None).unwrap(), "café.txt");
        assert_eq!(
            decode_name(
                &[0xd6, 0xd0, 0xce, 0xc4, b'.', b't', b'x', b't'],
                0,
                &[],
                Some("gb18030"),
            )
            .unwrap(),
            "中文.txt"
        );
        assert!(decode_name(&[0xff], 0x0800, &[], Some("windows-1252")).is_err());

        let raw = b"legacy.txt";
        let unicode = "统一.txt";
        let mut body = vec![1];
        body.extend_from_slice(&crc32fast::hash(raw).to_le_bytes());
        body.extend_from_slice(unicode.as_bytes());
        let mut extra = Vec::new();
        extra.extend_from_slice(&0x7075_u16.to_le_bytes());
        extra.extend_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
        extra.extend_from_slice(&body);
        assert_eq!(decode_name(raw, 0, &extra, Some("shift_jis")).unwrap(), unicode);
    }

    #[test]
    fn invalid_explicit_zip_charset_is_stable() {
        let error = decode_name(&[0x80], 0, &[], Some("definitely-not-an-encoding")).unwrap_err();
        assert!(matches!(error, ConversionError::Malformed { .. }));
    }

    #[test]
    fn inert_compression_hints_do_not_broaden_zip_authority() {
        validate_flags(0x0802, 0).unwrap();
        validate_flags(0x0806, 8).unwrap();
        assert!(validate_flags(0x0810, 8).is_err());
        assert!(matches!(validate_flags(0x0801, 8), Err(ConversionError::Encrypted)));
        assert!(validate_flags(0x0802, 9).is_err());
    }
}
