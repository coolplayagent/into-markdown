use super::budget::{LegacyBudget, limit, malformed};
use super::builder::{PROVIDER_ID, locator};
use super::normalize_xls_output;
use crate::msg::ole::Storage;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput,
    Diagnostic, DiagnosticSeverity, ErrorPolicy, ExecutionContext, NodeId, Provenance,
    ProvenanceKind,
};

const WORKBOOK: &str = "Workbook";
const BOF: u16 = 0x0809;
const BOF4: u16 = 0x0409;
const BIFF8: u16 = 0x0600;
const BIFF5: u16 = 0x0500;
const BIFF4: u16 = 0x0400;
const FILE_PASS: u16 = 0x002f;
const EOF: u16 = 0x000a;
const DIMENSIONS: u16 = 0x0200;
const SUP_BOOK: u16 = 0x01ae;
const EXTERN_SHEET: u16 = 0x0017;
const CFB_FREE: u32 = 0xffff_ffff;
const CFB_END: u32 = 0xffff_fffe;
const CFB_FAT: u32 = 0xffff_fffd;
const CFB_DIFAT: u32 = 0xffff_fffc;
const CFB_SECTOR_BYTES: usize = 512;
const CFB_FAT_ENTRIES: usize = CFB_SECTOR_BYTES / 4;
const CFB_HEADER_DIFAT_ENTRIES: usize = 109;
const CFB_DIFAT_ENTRIES: usize = CFB_FAT_ENTRIES - 1;

pub(super) fn looks_like_raw_biff(bytes: &[u8]) -> bool {
    bytes.get(..4).is_some_and(|header| {
        u16::from_le_bytes([header[0], header[1]]) == BOF4
            && usize::from(u16::from_le_bytes([header[2], header[3]])) >= 4
    })
}

pub(super) fn convert_raw(
    bytes: &[u8],
    budget: &mut LegacyBudget<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let preflight = preflight(bytes, WORKBOOK, budget, options.error_policy)?;
    if preflight.biff_version != BIFF4 {
        return Err(malformed(WORKBOOK, "raw XLS stream is not BIFF4"));
    }
    let normalized_limit = bytes
        .len()
        .checked_mul(2)
        .and_then(|value| value.checked_add(64))
        .ok_or_else(|| limit("max_memory_bytes", "normalized BIFF4 size overflowed"))?;
    let _normalized_memory =
        context.reserve_memory(u64::try_from(normalized_limit).unwrap_or(u64::MAX))?;
    let normalized = normalize_raw_biff4(bytes)?;
    let layout = cfb_wrapper_layout(normalized.len())?;
    let _wrapper_memory = context.reserve_memory(
        u64::try_from(layout.output_bytes.saturating_add(layout.total_sectors * 4))
            .unwrap_or(u64::MAX),
    )?;
    let wrapper = build_cfb_wrapper(&normalized, &[], layout)?;
    let mut output = crate::workbook::convert_legacy_xls(&wrapper, options, context)?;
    normalize_xls_output(&mut output);
    output.document.metadata.properties.insert("legacyOffice.xls.biff".into(), "4".into());
    output.diagnostics.push(Diagnostic {
        code: "legacyOffice.xls.rawBiffRecovered".into(),
        severity: DiagnosticSeverity::Info,
        message: "a bounded raw BIFF4 worksheet stream was normalized into an inert workbook view"
            .into(),
        locator: Some(locator(WORKBOOK)),
    });
    Ok(output)
}

pub(super) fn convert(
    bytes: &[u8],
    root: Storage<'_>,
    budget: &mut LegacyBudget<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
    container_view_required: bool,
) -> Result<ConverterOutput, ConversionError> {
    let (part, workbook) = root
        .stream(WORKBOOK)
        .map(|stream| (WORKBOOK, stream))
        .or_else(|| root.stream("Book").map(|stream| ("Book", stream)))
        .ok_or_else(|| malformed("CFB directory", "XLS has no Workbook stream"))?;
    let preflight = preflight(workbook, part, budget, options.error_policy)?;

    let wrapper_layout = (container_view_required
        || !preflight.omitted_dimension_records.is_empty())
    .then(|| cfb_wrapper_layout(workbook.len()))
    .transpose()?;
    let wrapper_memory = wrapper_layout
        .as_ref()
        .map(|layout| {
            context.reserve_memory(
                u64::try_from(layout.output_bytes.saturating_add(layout.total_sectors * 4))
                    .unwrap_or(u64::MAX),
            )
        })
        .transpose()?;
    let wrapper = wrapper_layout
        .as_ref()
        .map(|layout| build_cfb_wrapper(workbook, &preflight.omitted_dimension_records, *layout))
        .transpose()?;
    let conversion_bytes = wrapper.as_deref().unwrap_or(bytes);
    let mut output = crate::workbook::convert_legacy_xls(conversion_bytes, options, context)?;
    drop(wrapper_memory);
    normalize_xls_output(&mut output);
    output.document.metadata.properties.insert(
        "legacyOffice.xls.biff".into(),
        match preflight.biff_version {
            BIFF4 => "4",
            BIFF5 => "5",
            _ => "8",
        }
        .into(),
    );

    if preflight.external_bindings {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.externalBindingsSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "external workbook bindings were retained as inert formula text and were not resolved"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if preflight.tail_padding_ignored {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.trailingPaddingIgnored".into(),
            severity: DiagnosticSeverity::Info,
            message: "incomplete zero padding after a complete BIFF substream was ignored".into(),
            locator: Some(locator(part)),
        });
    }
    if matches!(preflight.biff_version, BIFF4 | BIFF5) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.legacyBiffRecovered".into(),
            severity: DiagnosticSeverity::Info,
            message: format!(
                "BIFF{} workbook data was converted through the bounded legacy reader",
                if preflight.biff_version == BIFF4 { 4 } else { 5 }
            ),
            locator: Some(locator(part)),
        });
    }
    if !preflight.omitted_dimension_records.is_empty() {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.dimensionMetadataRecovered".into(),
            severity: DiagnosticSeverity::Info,
            message: "non-canonical Dimensions metadata was authenticated and omitted from the compatibility reader"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if root.storage("_VBA_PROJECT_CUR").is_some() || root.stream("_VBA_PROJECT_CUR").is_some() {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.macrosSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "VBA project data was not executed or exposed as active content".into(),
            locator: Some(locator("_VBA_PROJECT_CUR")),
        });
    }
    retain_safe_images(workbook, part, &mut output, budget)?;
    Ok(output)
}

#[derive(Default)]
struct Preflight {
    external_bindings: bool,
    tail_padding_ignored: bool,
    biff_version: u16,
    omitted_dimension_records: Vec<usize>,
}

fn preflight(
    bytes: &[u8],
    part: &str,
    budget: &mut LegacyBudget<'_>,
    error_policy: ErrorPolicy,
) -> Result<Preflight, ConversionError> {
    let mut cursor = 0usize;
    let mut biff_version = None;
    let mut at_substream_boundary = false;
    let mut zero_padding_start = None;
    let mut result = Preflight::default();
    while cursor < bytes.len() {
        budget.work(1, part)?;
        let Some(header) = bytes.get(cursor..cursor.saturating_add(4)) else {
            if error_policy == ErrorPolicy::BestEffort
                && (at_substream_boundary || zero_padding_start.is_some())
                && bytes[cursor..].iter().all(|byte| *byte == 0)
            {
                result.tail_padding_ignored = true;
                break;
            }
            return Err(malformed(part, "truncated BIFF record header"));
        };
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let body_start = cursor
            .checked_add(4)
            .ok_or_else(|| malformed(part, "BIFF record offset overflowed"))?;
        let end = body_start
            .checked_add(length)
            .ok_or_else(|| malformed(part, "BIFF record length overflowed"))?;
        let body = bytes
            .get(body_start..end)
            .ok_or_else(|| malformed(part, "truncated BIFF record body"))?;
        if at_substream_boundary && kind == 0 && length == 0 {
            zero_padding_start.get_or_insert(cursor);
            cursor = end;
            continue;
        }
        at_substream_boundary = false;
        zero_padding_start = None;
        match kind {
            BOF | BOF4 => {
                let version = if kind == BOF4 { BIFF4 } else { read_u16(body, 0, part)? };
                let supported = version == BIFF8
                    || (matches!(version, BIFF4 | BIFF5)
                        && error_policy == ErrorPolicy::BestEffort);
                if !supported {
                    return Err(ConversionError::Unsupported {
                        detail: format!(
                            "XLS BIFF version 0x{version:04x} predates Office 97-2003 BIFF8"
                        ),
                    });
                }
                if biff_version.is_some_and(|current| current != version) {
                    return Err(malformed(part, "BIFF substreams disagree on workbook version"));
                }
                biff_version = Some(version);
            }
            FILE_PASS => return Err(ConversionError::Encrypted),
            EOF => at_substream_boundary = true,
            DIMENSIONS => preflight_dimensions(
                body,
                cursor,
                biff_version,
                part,
                budget,
                error_policy,
                &mut result.omitted_dimension_records,
            )?,
            SUP_BOOK | EXTERN_SHEET => result.external_bindings = true,
            _ => {}
        }
        cursor = end;
    }
    if zero_padding_start.is_some() {
        if error_policy == ErrorPolicy::Strict {
            return Err(malformed(part, "zero padding follows the final BIFF substream"));
        }
        result.tail_padding_ignored = true;
    }
    let Some(biff_version) = biff_version else {
        return Err(malformed(part, "Workbook stream has no BIFF8 BOF record"));
    };
    result.biff_version = biff_version;
    Ok(result)
}

fn preflight_dimensions(
    body: &[u8],
    cursor: usize,
    biff_version: Option<u16>,
    part: &str,
    budget: &mut LegacyBudget<'_>,
    error_policy: ErrorPolicy,
    omitted_records: &mut Vec<usize>,
) -> Result<(), ConversionError> {
    let legacy = matches!(biff_version, Some(BIFF4 | BIFF5));
    let expected_length = if legacy { 10 } else { 14 };
    if body.len() != expected_length {
        if error_policy == ErrorPolicy::Strict {
            return Err(malformed(part, "non-canonical BIFF Dimensions record length"));
        }
        omitted_records.push(cursor);
    }
    let (first_row, last_row, first_column, last_column) = if legacy {
        if body.len() < 8 {
            return Err(malformed(part, "truncated BIFF5 Dimensions record"));
        }
        (
            u64::from(read_u16(body, 0, part)?),
            u64::from(read_u16(body, 2, part)?),
            u64::from(read_u16(body, 4, part)?),
            u64::from(read_u16(body, 6, part)?),
        )
    } else {
        if body.len() < 12 {
            return Err(malformed(part, "truncated BIFF8 Dimensions record"));
        }
        (
            u64::from(read_u32(body, 0, part)?),
            u64::from(read_u32(body, 4, part)?),
            u64::from(read_u16(body, 8, part)?),
            u64::from(read_u16(body, 10, part)?),
        )
    };
    if last_row < first_row || last_column < first_column {
        return Err(malformed(part, "BIFF8 Dimensions range is reversed"));
    }
    budget.table_shape(
        usize::try_from(last_row - first_row).unwrap_or(usize::MAX),
        usize::try_from(last_column - first_column).unwrap_or(usize::MAX),
    )
}

fn normalize_raw_biff4(bytes: &[u8]) -> Result<Vec<u8>, ConversionError> {
    let first_length = usize::from(read_u16(bytes, 2, WORKBOOK)?);
    let mut cursor = 4usize
        .checked_add(first_length)
        .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 BOF length overflowed"))?;
    let global_prefix_start = cursor;
    let mut global_prefix_end = None;
    let mut formula_count = 0usize;
    while cursor < bytes.len() {
        let header_end = cursor
            .checked_add(4)
            .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 record offset overflowed"))?;
        let header = bytes
            .get(cursor..header_end)
            .ok_or_else(|| malformed(WORKBOOK, "truncated raw BIFF4 record header"))?;
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 record length overflowed"))?;
        bytes.get(cursor..end).ok_or_else(|| malformed(WORKBOOK, "truncated raw BIFF4 record"))?;
        if global_prefix_end.is_none() && kind == DIMENSIONS {
            global_prefix_end = Some(cursor);
        }
        if global_prefix_end.is_none() && kind == EOF {
            break;
        }
        if kind == 0x0406 {
            formula_count = formula_count
                .checked_add(1)
                .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 Formula count overflowed"))?;
        }
        cursor = end;
    }
    let Some(global_prefix_end) = global_prefix_end else {
        return Err(malformed(WORKBOOK, "raw BIFF4 stream has no Dimensions record"));
    };

    let global_prefix = bytes
        .get(global_prefix_start..global_prefix_end)
        .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 global record range is invalid"))?;
    let formula_extra = formula_count
        .checked_mul(4)
        .ok_or_else(|| malformed(WORKBOOK, "normalized BIFF4 Formula bytes overflowed"))?;
    let capacity = bytes
        .len()
        .checked_add(global_prefix.len())
        .and_then(|value| value.checked_add(formula_extra))
        .and_then(|value| value.checked_add(64))
        .ok_or_else(|| malformed(WORKBOOK, "normalized BIFF4 size overflowed"))?;
    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve normalized BIFF4 stream: {error}"))
    })?;
    push_biff_record(&mut output, BOF, &[0x00, 0x04, 0x05, 0x00, 0, 0, 0, 0])?;
    output.extend_from_slice(global_prefix);
    append_normalized_biff4_sheet_header(&mut output)?;

    cursor = 0;
    while cursor < bytes.len() {
        let body_start = cursor
            .checked_add(4)
            .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 record offset overflowed"))?;
        let header = bytes
            .get(cursor..body_start)
            .ok_or_else(|| malformed(WORKBOOK, "truncated raw BIFF4 record header"))?;
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 record length overflowed"))?;
        let body = bytes
            .get(body_start..end)
            .ok_or_else(|| malformed(WORKBOOK, "truncated raw BIFF4 record"))?;
        match kind {
            BOF4 => {
                let mut bof = body.to_vec();
                let version = bof
                    .get_mut(..2)
                    .ok_or_else(|| malformed(WORKBOOK, "raw BIFF4 BOF body is truncated"))?;
                version.copy_from_slice(&BIFF4.to_le_bytes());
                push_biff_record(&mut output, BOF, &bof)?;
            }
            0x0406 => push_normalized_biff4_formula(&mut output, body)?,
            _ => push_biff_record(&mut output, kind, body)?,
        }
        cursor = end;
    }
    Ok(output)
}

fn append_normalized_biff4_sheet_header(output: &mut Vec<u8>) -> Result<(), ConversionError> {
    let sheet_name = b"Sheet1";
    let bound_sheet_record_bytes = 4usize
        .checked_add(6)
        .and_then(|value| value.checked_add(1 + sheet_name.len()))
        .ok_or_else(|| malformed(WORKBOOK, "BIFF4 BoundSheet size overflowed"))?;
    let sheet_offset = output
        .len()
        .checked_add(bound_sheet_record_bytes)
        .and_then(|value| value.checked_add(4))
        .ok_or_else(|| malformed(WORKBOOK, "BIFF4 sheet offset overflowed"))?;
    let mut bound_sheet = Vec::with_capacity(bound_sheet_record_bytes - 4);
    bound_sheet.extend_from_slice(&to_u32(sheet_offset)?.to_le_bytes());
    bound_sheet.extend_from_slice(&[0, 0, u8::try_from(sheet_name.len()).unwrap_or(u8::MAX)]);
    bound_sheet.extend_from_slice(sheet_name);
    push_biff_record(output, 0x0085, &bound_sheet)?;
    push_biff_record(output, EOF, &[])
}

fn push_normalized_biff4_formula(output: &mut Vec<u8>, body: &[u8]) -> Result<(), ConversionError> {
    // BIFF4 places `cce` immediately after the two-byte option flags at offset 16.  The
    // bounded downstream reader shares its BIFF8 record framing and expects the four-byte
    // reserved field before `cce`.  Inserting that inert field preserves the cached value,
    // options, token bytes, and BIFF4 token semantics selected by the BOF version.
    if body.len() < 18 {
        return Err(malformed(WORKBOOK, "raw BIFF4 Formula record is truncated"));
    }
    let normalized_length = body
        .len()
        .checked_add(4)
        .ok_or_else(|| malformed(WORKBOOK, "normalized BIFF4 Formula size overflowed"))?;
    output.extend_from_slice(&0x0006_u16.to_le_bytes());
    output.extend_from_slice(&to_u16(normalized_length)?.to_le_bytes());
    output.extend_from_slice(&body[..16]);
    output.extend_from_slice(&[0; 4]);
    output.extend_from_slice(&body[16..]);
    Ok(())
}

fn push_biff_record(output: &mut Vec<u8>, kind: u16, body: &[u8]) -> Result<(), ConversionError> {
    let length = to_u16(body.len())?;
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(body);
    Ok(())
}

#[derive(Clone, Copy)]
struct CfbWrapperLayout {
    stream_sectors: usize,
    fat_sectors: usize,
    difat_sectors: usize,
    total_sectors: usize,
    output_bytes: usize,
}

fn cfb_wrapper_layout(workbook_bytes: usize) -> Result<CfbWrapperLayout, ConversionError> {
    let logical_bytes = workbook_bytes.max(4096);
    let stream_sectors = logical_bytes.div_ceil(CFB_SECTOR_BYTES);
    let mut fat_sectors = 1usize;
    loop {
        let difat_sectors =
            fat_sectors.saturating_sub(CFB_HEADER_DIFAT_ENTRIES).div_ceil(CFB_DIFAT_ENTRIES);
        let total_sectors = stream_sectors
            .checked_add(1)
            .and_then(|value| value.checked_add(fat_sectors))
            .and_then(|value| value.checked_add(difat_sectors))
            .ok_or_else(|| {
                malformed(WORKBOOK, "CFB compatibility wrapper sector count overflowed")
            })?;
        let required_fat = total_sectors.div_ceil(CFB_FAT_ENTRIES);
        if required_fat == fat_sectors {
            let output_bytes = total_sectors
                .checked_add(1)
                .and_then(|value| value.checked_mul(CFB_SECTOR_BYTES))
                .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility wrapper size overflowed"))?;
            return Ok(CfbWrapperLayout {
                stream_sectors,
                fat_sectors,
                difat_sectors,
                total_sectors,
                output_bytes,
            });
        }
        fat_sectors = required_fat;
    }
}

fn build_cfb_wrapper(
    workbook: &[u8],
    omitted_dimension_records: &[usize],
    layout: CfbWrapperLayout,
) -> Result<Vec<u8>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve_exact(layout.output_bytes).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve CFB compatibility wrapper: {error}"))
    })?;
    output.resize(layout.output_bytes, 0);
    write_cfb_wrapper_header(&mut output, layout)?;
    let fat_start = layout.stream_sectors + 1;
    let difat_start = fat_start + layout.fat_sectors;
    let stream_start = CFB_SECTOR_BYTES;
    output[stream_start..stream_start + workbook.len()].copy_from_slice(workbook);
    for offset in omitted_dimension_records {
        let physical = stream_start.checked_add(*offset).ok_or_else(|| {
            malformed(WORKBOOK, "Dimensions compatibility patch offset overflowed")
        })?;
        let record_type = output.get_mut(physical..physical + 2).ok_or_else(|| {
            malformed(WORKBOOK, "Dimensions compatibility patch is outside Workbook stream")
        })?;
        record_type.copy_from_slice(&0xffff_u16.to_le_bytes());
    }

    let directory_offset = sector_offset_in_wrapper(layout.stream_sectors)?;
    write_directory_entry(
        &mut output,
        directory_offset,
        DirectoryEntrySpec {
            name: "Root Entry",
            kind: 5,
            left: CFB_FREE,
            right: CFB_FREE,
            child: 1,
            start: CFB_END,
            size: 0,
        },
    )?;
    write_directory_entry(
        &mut output,
        directory_offset + 128,
        DirectoryEntrySpec {
            name: WORKBOOK,
            kind: 2,
            left: CFB_FREE,
            right: CFB_FREE,
            child: CFB_FREE,
            start: 0,
            size: u64::try_from(workbook.len().max(4096)).unwrap_or(u64::MAX),
        },
    )?;

    let mut fat = Vec::new();
    fat.try_reserve_exact(layout.total_sectors).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve CFB compatibility FAT: {error}"))
    })?;
    fat.resize(layout.total_sectors, CFB_FREE);
    for (index, slot) in fat.iter_mut().take(layout.stream_sectors).enumerate() {
        *slot = if index + 1 == layout.stream_sectors { CFB_END } else { to_u32(index + 1)? };
    }
    fat[layout.stream_sectors] = CFB_END;
    for slot in fat.iter_mut().skip(fat_start).take(layout.fat_sectors) {
        *slot = CFB_FAT;
    }
    for slot in fat.iter_mut().skip(difat_start).take(layout.difat_sectors) {
        *slot = CFB_DIFAT;
    }
    write_cfb_wrapper_allocation_tables(&mut output, &fat, layout, fat_start, difat_start)?;
    Ok(output)
}

fn write_cfb_wrapper_header(
    output: &mut [u8],
    layout: CfbWrapperLayout,
) -> Result<(), ConversionError> {
    output[..8].copy_from_slice(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1");
    put_u16(output, 24, 0x003e)?;
    put_u16(output, 26, 3)?;
    put_u16(output, 28, 0xfffe)?;
    put_u16(output, 30, 9)?;
    put_u16(output, 32, 6)?;
    put_u32(output, 44, to_u32(layout.fat_sectors)?)?;
    let directory_sector = to_u32(layout.stream_sectors)?;
    put_u32(output, 48, directory_sector)?;
    put_u32(output, 56, 4096)?;
    put_u32(output, 60, CFB_END)?;
    let fat_start = layout.stream_sectors + 1;
    let difat_start = fat_start + layout.fat_sectors;
    put_u32(output, 68, if layout.difat_sectors == 0 { CFB_END } else { to_u32(difat_start)? })?;
    put_u32(output, 72, to_u32(layout.difat_sectors)?)?;
    for index in 0..CFB_HEADER_DIFAT_ENTRIES {
        let value = if index < layout.fat_sectors.min(CFB_HEADER_DIFAT_ENTRIES) {
            to_u32(fat_start + index)?
        } else {
            CFB_FREE
        };
        put_u32(output, 76 + index * 4, value)?;
    }
    Ok(())
}

fn write_cfb_wrapper_allocation_tables(
    output: &mut [u8],
    fat: &[u32],
    layout: CfbWrapperLayout,
    fat_start: usize,
    difat_start: usize,
) -> Result<(), ConversionError> {
    for fat_index in 0..layout.fat_sectors {
        let offset = sector_offset_in_wrapper(fat_start + fat_index)?;
        for entry in 0..CFB_FAT_ENTRIES {
            let value = fat.get(fat_index * CFB_FAT_ENTRIES + entry).copied().unwrap_or(CFB_FREE);
            put_u32(output, offset + entry * 4, value)?;
        }
    }
    let remaining_fat = layout.fat_sectors.saturating_sub(CFB_HEADER_DIFAT_ENTRIES);
    for difat_index in 0..layout.difat_sectors {
        let offset = sector_offset_in_wrapper(difat_start + difat_index)?;
        for entry in 0..CFB_DIFAT_ENTRIES {
            let index = difat_index * CFB_DIFAT_ENTRIES + entry;
            let value = if index < remaining_fat {
                to_u32(fat_start + CFB_HEADER_DIFAT_ENTRIES + index)?
            } else {
                CFB_FREE
            };
            put_u32(output, offset + entry * 4, value)?;
        }
        let next = if difat_index + 1 == layout.difat_sectors {
            CFB_END
        } else {
            to_u32(difat_start + difat_index + 1)?
        };
        put_u32(output, offset + CFB_SECTOR_BYTES - 4, next)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct DirectoryEntrySpec<'a> {
    name: &'a str,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

fn write_directory_entry(
    output: &mut [u8],
    offset: usize,
    entry: DirectoryEntrySpec<'_>,
) -> Result<(), ConversionError> {
    let mut encoded = entry.name.encode_utf16().collect::<Vec<_>>();
    encoded.push(0);
    if encoded.len() > 32 {
        return Err(malformed(WORKBOOK, "CFB compatibility stream name is too long"));
    }
    for (index, unit) in encoded.iter().enumerate() {
        put_u16(output, offset + index * 2, *unit)?;
    }
    put_u16(output, offset + 64, to_u16(encoded.len() * 2)?)?;
    *output
        .get_mut(offset + 66)
        .ok_or_else(|| malformed(WORKBOOK, "CFB directory entry is truncated"))? = entry.kind;
    *output
        .get_mut(offset + 67)
        .ok_or_else(|| malformed(WORKBOOK, "CFB directory entry is truncated"))? = 1;
    put_u32(output, offset + 68, entry.left)?;
    put_u32(output, offset + 72, entry.right)?;
    put_u32(output, offset + 76, entry.child)?;
    put_u32(output, offset + 116, entry.start)?;
    put_u64(output, offset + 120, entry.size)
}

fn sector_offset_in_wrapper(sector: usize) -> Result<usize, ConversionError> {
    sector
        .checked_add(1)
        .and_then(|value| value.checked_mul(CFB_SECTOR_BYTES))
        .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility sector offset overflowed"))
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) -> Result<(), ConversionError> {
    output
        .get_mut(offset..offset + 2)
        .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility write is out of bounds"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) -> Result<(), ConversionError> {
    output
        .get_mut(offset..offset + 4)
        .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility write is out of bounds"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64(output: &mut [u8], offset: usize, value: u64) -> Result<(), ConversionError> {
    output
        .get_mut(offset..offset + 8)
        .ok_or_else(|| malformed(WORKBOOK, "CFB compatibility write is out of bounds"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn to_u16(value: usize) -> Result<u16, ConversionError> {
    u16::try_from(value).map_err(|_| malformed(WORKBOOK, "CFB compatibility u16 overflowed"))
}

fn to_u32(value: usize) -> Result<u32, ConversionError> {
    u32::try_from(value).map_err(|_| malformed(WORKBOOK, "CFB compatibility u32 overflowed"))
}

fn retain_safe_images(
    bytes: &[u8],
    part: &str,
    output: &mut ConverterOutput,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let mut cursor = 0usize;
    let mut count = 0usize;
    while let Some((start, end, media_type)) = super::doc::find_image(&bytes[cursor..]) {
        let start = cursor + start;
        let end = cursor + end;
        budget.raster(&bytes[start..end], media_type, "Workbook/image")?;
        budget.asset(end - start, "Workbook/image")?;
        count += 1;
        let extension = if media_type == "image/png" { "png" } else { "jpg" };
        let id = AssetId(format!("legacy-xls-asset-{count}"));
        output.assets.push(Asset {
            id: id.clone(),
            filename: Some(format!("workbook-image-{count}.{extension}")),
            media_type: media_type.into(),
            bytes: bytes[start..end].to_vec(),
            external_uri: None,
        });
        output.document.blocks.push(BlockNode {
            id: NodeId(format!("legacy-xls-image-{count}")),
            block: Block::Image { asset: id, alt: None },
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: locator(part),
                confidence: None,
            },
        });
        cursor = end;
    }
    if count > 0 {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.imagePlacementRecovered".into(),
            severity: DiagnosticSeverity::Warning,
            message: "safe embedded image payloads were retained in workbook stream order because drawing anchors were incomplete"
                .into(),
            locator: Some(locator(part)),
        });
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize, part: &str) -> Result<u16, ConversionError> {
    let raw = bytes.get(offset..offset + 2).ok_or_else(|| malformed(part, "truncated BIFF u16"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32(bytes: &[u8], offset: usize, part: &str) -> Result<u32, ConversionError> {
    let raw = bytes.get(offset..offset + 4).ok_or_else(|| malformed(part, "truncated BIFF u32"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::ole::CompoundFile;
    use into_markdown_core::{Cell, ExecutionOptions, Inline, ResourceLimits};

    fn budget<'a>(
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> LegacyBudget<'a> {
        LegacyBudget::new(64, options, context).unwrap()
    }

    fn raw_biff4_with_label(text: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_biff_record(&mut bytes, BOF4, &[0, 0, 0x10, 0, 0, 0]).unwrap();

        let mut dimensions = Vec::new();
        dimensions.extend_from_slice(&0u16.to_le_bytes());
        dimensions.extend_from_slice(&1u16.to_le_bytes());
        dimensions.extend_from_slice(&0u16.to_le_bytes());
        dimensions.extend_from_slice(&1u16.to_le_bytes());
        dimensions.extend_from_slice(&0u16.to_le_bytes());
        push_biff_record(&mut bytes, DIMENSIONS, &dimensions).unwrap();

        let mut label = vec![0; 6];
        label.extend_from_slice(&u16::try_from(text.len()).unwrap().to_le_bytes());
        label.extend_from_slice(text);
        push_biff_record(&mut bytes, 0x0204, &label).unwrap();
        push_biff_record(&mut bytes, EOF, &[]).unwrap();
        bytes
    }

    fn convert_fixture(bytes: &[u8]) -> ConverterOutput {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut compound_budget = LegacyBudget::new(bytes.len(), &options, &context).unwrap();
        let compound = CompoundFile::open(bytes, &mut compound_budget).unwrap();
        convert(bytes, compound.root(), &mut compound_budget, &options, &context, false).unwrap()
    }

    fn table(output: &ConverterOutput) -> (&str, &[into_markdown_core::TableRow]) {
        let Block::Sheet { name, blocks } = &output.document.blocks[0].block else {
            panic!("fixture did not emit a worksheet")
        };
        let Block::Table { rows, .. } = &blocks[0].block else {
            panic!("fixture worksheet did not emit a table")
        };
        (name, rows)
    }

    fn cell_text(cell: &Cell) -> String {
        let Some(block) = cell.blocks.first() else { return String::new() };
        let Block::Paragraph(inlines) = &block.block else {
            panic!("fixture cell did not emit a paragraph")
        };
        inlines
            .iter()
            .map(|inline| match inline {
                Inline::Text { value, .. } | Inline::Code(value) => value.as_str(),
                _ => panic!("fixture cell emitted an unexpected inline"),
            })
            .collect()
    }

    fn workbook_with_merge() -> Vec<u8> {
        const FIXTURE: &[u8] =
            include_bytes!("../../../../tools/macos-release/fixtures/normal.xls");
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut compound_budget = LegacyBudget::new(FIXTURE.len(), &options, &context).unwrap();
        let compound = CompoundFile::open(FIXTURE, &mut compound_budget).unwrap();
        let mut workbook = compound.root().stream(WORKBOOK).unwrap().to_vec();
        let mut cursor = 0usize;
        let mut final_eof = None;
        while let Some(header) = workbook.get(cursor..cursor.saturating_add(4)) {
            let kind = u16::from_le_bytes([header[0], header[1]]);
            let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
            let Some(end) = cursor.checked_add(4).and_then(|value| value.checked_add(length))
            else {
                break;
            };
            if end > workbook.len() {
                break;
            }
            if kind == EOF {
                final_eof = Some(cursor);
            }
            cursor = end;
        }
        let mut merged = Vec::new();
        push_biff_record(&mut merged, 0x00e5, &[1, 0, 0, 0, 0, 0, 0, 0, 1, 0]).unwrap();
        workbook.splice(final_eof.unwrap()..final_eof.unwrap(), merged);
        let layout = cfb_wrapper_layout(workbook.len()).unwrap();
        build_cfb_wrapper(&workbook, &[], layout).unwrap()
    }

    #[test]
    fn rejects_pre_biff8_and_filepass() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut old = vec![0x09, 0x08, 4, 0, 0x00, 0x05, 0, 0];
        assert!(matches!(
            preflight(&old, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict),
            Err(ConversionError::Unsupported { .. })
        ));
        let recovered =
            preflight(&old, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
                .unwrap();
        assert_eq!(recovered.biff_version, BIFF5);
        old[4..6].copy_from_slice(&BIFF8.to_le_bytes());
        old.extend_from_slice(&[0x2f, 0, 0, 0]);
        assert!(matches!(
            preflight(&old, WORKBOOK, &mut budget(&options, &context), options.error_policy),
            Err(ConversionError::Encrypted)
        ));
    }

    #[test]
    fn dimensions_use_table_resource_limits() {
        let limits = ResourceLimits { max_table_rows: 10, ..ResourceLimits::default() };
        let options = ConversionOptions { limits, ..ConversionOptions::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut bytes = vec![0x09, 0x08, 4, 0, 0, 6, 0, 0];
        bytes.extend_from_slice(&[0x00, 0x02, 12, 0]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        assert!(matches!(
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context), options.error_policy),
            Err(ConversionError::ResourceLimit { limit: "max_table_rows", .. })
        ));
    }

    #[test]
    fn best_effort_accepts_only_zero_tail_after_complete_substream() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut bytes = vec![0x09, 0x08, 4, 0, 0, 6, 0, 0, 0x0a, 0, 0, 0, 0];
        let recovered =
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
                .unwrap();
        assert!(recovered.tail_padding_ignored);

        assert!(matches!(
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict,),
            Err(ConversionError::Malformed { .. })
        ));
        *bytes.last_mut().unwrap() = 1;
        assert!(matches!(
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort,),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn noncanonical_dimensions_are_omitted_only_in_best_effort() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut bytes = Vec::new();
        push_biff_record(&mut bytes, BOF, &[0, 6, 0, 0]).unwrap();
        let mut dimensions = Vec::new();
        dimensions.extend_from_slice(&0u32.to_le_bytes());
        dimensions.extend_from_slice(&1u32.to_le_bytes());
        dimensions.extend_from_slice(&0u16.to_le_bytes());
        dimensions.extend_from_slice(&1u16.to_le_bytes());
        dimensions.extend_from_slice(&[0; 4]);
        push_biff_record(&mut bytes, DIMENSIONS, &dimensions).unwrap();
        push_biff_record(&mut bytes, EOF, &[]).unwrap();

        let recovered =
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
                .unwrap();
        assert_eq!(recovered.omitted_dimension_records.len(), 1);
        assert!(matches!(
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict,),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn compatibility_wrapper_is_a_bounded_readable_cfb() {
        let workbook = raw_biff4_with_label(b"ok");
        let dimensions_offset =
            workbook.windows(4).position(|window| window == [0x00, 0x02, 0x0a, 0x00]).unwrap();
        let layout = cfb_wrapper_layout(workbook.len()).unwrap();
        let wrapper = build_cfb_wrapper(&workbook, &[dimensions_offset], layout).unwrap();
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut wrapper_budget = LegacyBudget::new(wrapper.len(), &options, &context).unwrap();
        let compound = CompoundFile::open(&wrapper, &mut wrapper_budget).unwrap();
        let recovered = compound.root().stream(WORKBOOK).unwrap();

        assert_eq!(&recovered[dimensions_offset..dimensions_offset + 2], &[0xff, 0xff]);
        assert_eq!(&recovered[..dimensions_offset], &workbook[..dimensions_offset]);
    }

    #[test]
    fn xls_content_cell_order_display_values_and_merges_are_stable() {
        const FIXTURE: &[u8] =
            include_bytes!("../../../../tools/macos-release/fixtures/normal.xls");
        let output = convert_fixture(FIXTURE);
        let (name, rows) = table(&output);
        assert_eq!(name, "Corpus");
        let values = rows
            .iter()
            .map(|row| row.cells.iter().map(cell_text).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                ["Corpus", "=TRUE [cached: true]", "42.5"],
                ["2024-01-01 00:00:00", "=SUM(1,2) [cached: 3]", "=cmd"],
            ]
        );
        for (row_index, row) in rows.iter().enumerate() {
            for (column_index, cell) in row.cells.iter().enumerate() {
                let locator = &cell.blocks[0].provenance.locator;
                assert_eq!(locator.sheet.as_deref(), Some("Corpus"));
                let reference = locator.cell.as_ref().unwrap();
                assert_eq!(reference.row, u32::try_from(row_index).unwrap());
                assert_eq!(reference.column, u32::try_from(column_index).unwrap());
            }
        }

        let merged = convert_fixture(&workbook_with_merge());
        let (_, rows) = table(&merged);
        assert_eq!(rows[0].cells.len(), 2);
        assert_eq!(rows[0].cells[0].row_span, 1);
        assert_eq!(rows[0].cells[0].column_span, 2);
        assert_eq!(cell_text(&rows[0].cells[0]), "Corpus");
        assert_eq!(cell_text(&rows[0].cells[1]), "42.5");
    }

    #[test]
    fn biff4_formula_framing_preserves_value_options_and_tokens() {
        let mut body = (0u8..16).collect::<Vec<_>>();
        body.extend_from_slice(&3u16.to_le_bytes());
        body.extend_from_slice(&[0x1e, 1, 0]);
        let mut record = Vec::new();
        push_normalized_biff4_formula(&mut record, &body).unwrap();

        assert_eq!(&record[..2], &0x0006_u16.to_le_bytes());
        let expected_record_size = u16::try_from(body.len())
            .expect("fixture body fits BIFF record size")
            .checked_add(4)
            .expect("normalized formula record size fits u16");
        assert_eq!(u16::from_le_bytes([record[2], record[3]]), expected_record_size);
        assert_eq!(&record[4..20], &body[..16]);
        assert_eq!(&record[20..24], &[0; 4]);
        assert_eq!(&record[24..], &body[16..]);
    }

    #[test]
    fn raw_biff4_is_normalized_but_strict_mode_rejects_it() {
        let bytes = raw_biff4_with_label(b"ok");
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut best_effort_budget = LegacyBudget::new(bytes.len(), &options, &context).unwrap();
        let output = convert_raw(&bytes, &mut best_effort_budget, &options, &context).unwrap();

        assert!(format!("{:?}", output.document.blocks).contains("ok"));
        assert!(output.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "legacyOffice.xls.rawBiffRecovered"
                && diagnostic.severity == DiagnosticSeverity::Info
        }));

        let strict_options =
            ConversionOptions { error_policy: ErrorPolicy::Strict, ..ConversionOptions::default() };
        let strict_context =
            ExecutionContext::new(ExecutionOptions::default(), strict_options.limits.clone());
        let mut strict_budget =
            LegacyBudget::new(bytes.len(), &strict_options, &strict_context).unwrap();
        assert!(matches!(
            convert_raw(&bytes, &mut strict_budget, &strict_options, &strict_context),
            Err(ConversionError::Unsupported { .. })
        ));
    }
}
