//! Classic TIFF and `BigTIFF` directory-envelope validation.

use super::meter::Meter;
use super::{Summary, limit, malformed, read_u16, read_u32, read_u64};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceLimits};

#[allow(clippy::too_many_lines)]
pub(super) fn validate(
    bytes: &[u8],
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<Summary, ConversionError> {
    let (little, layout, mut ifd) = header(bytes)?;
    scan_all(bytes, context)?;
    if ifd == 0 {
        return Err(malformed("TIFF has no image file directory"));
    }
    validate_ifd_chain(bytes, layout, little, ifd, limits.max_pages, context)?;

    let mut frames = 0_u32;
    let mut total_entries = 0_u64;
    let mut reachable_end = match layout {
        Layout::Classic => 8_usize,
        Layout::Big => 16_usize,
    };
    while ifd != 0 {
        context.checkpoint()?;
        if frames >= limits.max_pages {
            return Err(limit("max_pages", "TIFF image directory count exceeds max_pages"));
        }
        let offset =
            usize::try_from(ifd).map_err(|_| malformed("TIFF directory offset overflow"))?;
        let (count, entries_offset, next_offset) = layout.directory(bytes, offset, little)?;
        let inline_size = usize::try_from(layout.inline_size())
            .map_err(|_| malformed("TIFF inline field size is unrepresentable"))?;
        reachable_end = reachable_end.max(
            next_offset
                .checked_add(inline_size)
                .ok_or_else(|| malformed("TIFF directory end overflow"))?,
        );
        total_entries = total_entries
            .checked_add(count)
            .ok_or_else(|| limit("max_archive_entries", "TIFF entry count overflow"))?;
        if total_entries > u64::from(limits.max_archive_entries) {
            return Err(limit(
                "max_archive_entries",
                format!("TIFF has {total_entries} directory entries"),
            ));
        }
        let mut strip_offsets = None;
        let mut strip_byte_counts = None;
        let mut tile_offsets = None;
        let mut tile_byte_counts = None;
        for index in 0..count {
            if index % 256 == 0 {
                context.checkpoint()?;
            }
            let entry = entries_offset
                .checked_add(
                    usize::try_from(index)
                        .ok()
                        .and_then(|value| value.checked_mul(layout.entry_size()))
                        .ok_or_else(|| malformed("TIFF directory entry offset overflow"))?,
                )
                .ok_or_else(|| malformed("TIFF directory entry offset overflow"))?;
            reachable_end = reachable_end.max(layout.validate_entry(bytes, entry, little)?);
            let tag = read_u16(bytes, entry, little)
                .ok_or_else(|| malformed("truncated TIFF field tag"))?;
            match tag {
                273 => {
                    if strip_offsets.is_some() {
                        return Err(malformed("TIFF contains duplicate strip offset fields"));
                    }
                    strip_offsets = Some(layout.unsigned_field(
                        bytes,
                        entry,
                        little,
                        limits.max_archive_entries,
                    )?);
                }
                279 => {
                    if strip_byte_counts.is_some() {
                        return Err(malformed("TIFF contains duplicate strip byte-count fields"));
                    }
                    strip_byte_counts = Some(layout.unsigned_field(
                        bytes,
                        entry,
                        little,
                        limits.max_archive_entries,
                    )?);
                }
                324 => {
                    if tile_offsets.is_some() {
                        return Err(malformed("TIFF contains duplicate tile offset fields"));
                    }
                    tile_offsets = Some(layout.unsigned_field(
                        bytes,
                        entry,
                        little,
                        limits.max_archive_entries,
                    )?);
                }
                325 => {
                    if tile_byte_counts.is_some() {
                        return Err(malformed("TIFF contains duplicate tile byte-count fields"));
                    }
                    tile_byte_counts = Some(layout.unsigned_field(
                        bytes,
                        entry,
                        little,
                        limits.max_archive_entries,
                    )?);
                }
                _ => {}
            }
        }
        let ((Some(offsets), Some(counts), None, None) | (None, None, Some(offsets), Some(counts))) =
            (strip_offsets, strip_byte_counts, tile_offsets, tile_byte_counts)
        else {
            return Err(malformed("TIFF requires one complete strip or tile range pair"));
        };
        if offsets.count == 0 || offsets.count != counts.count {
            return Err(malformed("TIFF pixel offset and byte-count fields disagree"));
        }
        for index in 0..offsets.count {
            if index % 256 == 0 {
                context.checkpoint()?;
            }
            let data_offset = offsets.value(bytes, index)?;
            let byte_count = counts.value(bytes, index)?;
            if byte_count == 0 {
                return Err(malformed("TIFF pixel segment has zero length"));
            }
            let end = data_offset
                .checked_add(byte_count)
                .ok_or_else(|| malformed("TIFF pixel segment range overflow"))?;
            if end > bytes.len() as u64 {
                return Err(malformed("TIFF pixel segment exceeds the file envelope"));
            }
            reachable_end = reachable_end.max(
                usize::try_from(end)
                    .map_err(|_| malformed("TIFF pixel segment is unrepresentable"))?,
            );
        }
        ifd = layout.read_offset(bytes, next_offset, little)?;
        frames += 1;
    }
    if reachable_end != bytes.len() {
        return Err(malformed(
            "TIFF contains bytes beyond every declared directory and pixel range",
        ));
    }
    Ok(Summary { frames, animated: frames > 1 })
}

#[derive(Debug, Clone, Copy)]
enum Layout {
    Classic,
    Big,
}

impl Layout {
    const fn entry_size(self) -> usize {
        match self {
            Self::Classic => 12,
            Self::Big => 20,
        }
    }

    const fn inline_size(self) -> u64 {
        match self {
            Self::Classic => 4,
            Self::Big => 8,
        }
    }

    fn read_offset(
        self,
        bytes: &[u8],
        offset: usize,
        little: bool,
    ) -> Result<u64, ConversionError> {
        match self {
            Self::Classic => read_u32(bytes, offset, little).map(u64::from),
            Self::Big => read_u64(bytes, offset, little),
        }
        .ok_or_else(|| malformed("truncated TIFF offset"))
    }

    fn directory(
        self,
        bytes: &[u8],
        offset: usize,
        little: bool,
    ) -> Result<(u64, usize, usize), ConversionError> {
        let (count, prefix, suffix) = match self {
            Self::Classic => (read_u16(bytes, offset, little).map(u64::from), 2_usize, 4_usize),
            Self::Big => (read_u64(bytes, offset, little), 8, 8),
        };
        let count = count.ok_or_else(|| malformed("truncated TIFF directory count"))?;
        let table_bytes = usize::try_from(count)
            .ok()
            .and_then(|value| value.checked_mul(self.entry_size()))
            .ok_or_else(|| malformed("TIFF directory table size overflow"))?;
        let entries = offset
            .checked_add(prefix)
            .ok_or_else(|| malformed("TIFF directory offset overflow"))?;
        let next = entries
            .checked_add(table_bytes)
            .ok_or_else(|| malformed("TIFF directory table offset overflow"))?;
        let end =
            next.checked_add(suffix).ok_or_else(|| malformed("TIFF directory end overflow"))?;
        if end > bytes.len() {
            return Err(malformed("TIFF directory exceeds the file envelope"));
        }
        Ok((count, entries, next))
    }

    fn validate_entry(
        self,
        bytes: &[u8],
        offset: usize,
        little: bool,
    ) -> Result<usize, ConversionError> {
        let field_type = read_u16(bytes, offset + 2, little)
            .ok_or_else(|| malformed("truncated TIFF field type"))?;
        let element_bytes = field_type_size(field_type)
            .ok_or_else(|| malformed(format!("unsupported TIFF field type {field_type}")))?;
        let (count_offset, value_offset) = match self {
            Self::Classic => (offset + 4, offset + 8),
            Self::Big => (offset + 4, offset + 12),
        };
        let count = match self {
            Self::Classic => read_u32(bytes, count_offset, little).map(u64::from),
            Self::Big => read_u64(bytes, count_offset, little),
        }
        .ok_or_else(|| malformed("truncated TIFF field count"))?;
        let payload_bytes = count
            .checked_mul(element_bytes)
            .ok_or_else(|| malformed("TIFF field payload size overflow"))?;
        if payload_bytes <= self.inline_size() {
            let inline_size = usize::try_from(self.inline_size())
                .map_err(|_| malformed("TIFF inline field size is unrepresentable"))?;
            return value_offset
                .checked_add(inline_size)
                .ok_or_else(|| malformed("TIFF inline field end overflow"));
        }
        let payload_offset = self.read_offset(bytes, value_offset, little)?;
        let end = payload_offset
            .checked_add(payload_bytes)
            .ok_or_else(|| malformed("TIFF field payload range overflow"))?;
        if end > bytes.len() as u64 {
            return Err(malformed("TIFF field payload exceeds the file envelope"));
        }
        usize::try_from(end).map_err(|_| malformed("TIFF field payload is unrepresentable"))
    }

    fn unsigned_field(
        self,
        bytes: &[u8],
        entry: usize,
        little: bool,
        max_values: u32,
    ) -> Result<UnsignedField, ConversionError> {
        let field_type = read_u16(bytes, entry + 2, little)
            .ok_or_else(|| malformed("truncated TIFF unsigned field type"))?;
        let element_size = match field_type {
            3 => 2_usize,
            4 | 13 => 4,
            16 | 18 => 8,
            _ => return Err(malformed("TIFF pixel ranges require unsigned integer fields")),
        };
        let (count, value_position) = match self {
            Self::Classic => (read_u32(bytes, entry + 4, little).map(u64::from), entry + 8),
            Self::Big => (read_u64(bytes, entry + 4, little), entry + 12),
        };
        let count = count.ok_or_else(|| malformed("truncated TIFF unsigned field count"))?;
        if count > u64::from(max_values) {
            return Err(limit(
                "max_archive_entries",
                format!("TIFF pixel range field has {count} values"),
            ));
        }
        let payload_bytes = usize::try_from(count)
            .ok()
            .and_then(|value| value.checked_mul(element_size))
            .ok_or_else(|| malformed("TIFF pixel range array size overflow"))?;
        let start = if payload_bytes as u64 <= self.inline_size() {
            value_position
        } else {
            usize::try_from(self.read_offset(bytes, value_position, little)?)
                .map_err(|_| malformed("TIFF pixel range array offset is unrepresentable"))?
        };
        let end = start
            .checked_add(payload_bytes)
            .ok_or_else(|| malformed("TIFF pixel range array end overflow"))?;
        if end > bytes.len() {
            return Err(malformed("TIFF pixel range array exceeds the file envelope"));
        }
        Ok(UnsignedField {
            start,
            count: usize::try_from(count)
                .map_err(|_| malformed("TIFF pixel range count is unrepresentable"))?,
            element_size,
            little,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct UnsignedField {
    start: usize,
    count: usize,
    element_size: usize,
    little: bool,
}

impl UnsignedField {
    fn value(self, bytes: &[u8], index: usize) -> Result<u64, ConversionError> {
        let offset = index
            .checked_mul(self.element_size)
            .and_then(|relative| self.start.checked_add(relative))
            .ok_or_else(|| malformed("TIFF pixel range value offset overflow"))?;
        match self.element_size {
            2 => read_u16(bytes, offset, self.little).map(u64::from),
            4 => read_u32(bytes, offset, self.little).map(u64::from),
            8 => read_u64(bytes, offset, self.little),
            _ => None,
        }
        .ok_or_else(|| malformed("truncated TIFF pixel range value"))
    }
}

fn validate_ifd_chain(
    bytes: &[u8],
    layout: Layout,
    little: bool,
    first: u64,
    max_pages: u32,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut slow = first;
    let mut fast = first;
    for _ in 0..max_pages {
        context.checkpoint()?;
        slow = next_ifd(bytes, layout, little, slow)?;
        fast = next_ifd(bytes, layout, little, fast)?;
        if fast != 0 {
            fast = next_ifd(bytes, layout, little, fast)?;
        }
        if slow == 0 || fast == 0 {
            return Ok(());
        }
        if slow == fast {
            return Err(malformed("TIFF image directory chain contains a cycle"));
        }
    }
    Err(limit("max_pages", "TIFF image directory count exceeds max_pages"))
}

fn next_ifd(bytes: &[u8], layout: Layout, little: bool, ifd: u64) -> Result<u64, ConversionError> {
    if ifd == 0 {
        return Ok(0);
    }
    let offset = usize::try_from(ifd).map_err(|_| malformed("TIFF directory offset overflow"))?;
    let (_, _, next_offset) = layout.directory(bytes, offset, little)?;
    layout.read_offset(bytes, next_offset, little)
}

fn header(bytes: &[u8]) -> Result<(bool, Layout, u64), ConversionError> {
    let little = match bytes.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Err(malformed("invalid TIFF byte-order marker")),
    };
    match read_u16(bytes, 2, little) {
        Some(42) => {
            let first = read_u32(bytes, 4, little)
                .map(u64::from)
                .ok_or_else(|| malformed("truncated TIFF header"))?;
            Ok((little, Layout::Classic, first))
        }
        Some(43) => {
            if read_u16(bytes, 4, little) != Some(8) || read_u16(bytes, 6, little) != Some(0) {
                return Err(malformed("unsupported BigTIFF offset layout"));
            }
            let first =
                read_u64(bytes, 8, little).ok_or_else(|| malformed("truncated BigTIFF header"))?;
            Ok((little, Layout::Big, first))
        }
        _ => Err(malformed("invalid TIFF magic")),
    }
}

fn field_type_size(field_type: u16) -> Option<u64> {
    Some(match field_type {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16 | 17 | 18 => 8,
        _ => return None,
    })
}

fn scan_all(bytes: &[u8], context: &ExecutionContext) -> Result<(), ConversionError> {
    let mut meter = Meter::new(context);
    let mut remaining = bytes.len();
    while remaining != 0 {
        let batch = remaining.min(meter.next_batch());
        meter.consume(batch)?;
        remaining -= batch;
    }
    Ok(())
}
