use crate::workbook::error::{limit, malformed};
use crate::workbook::model::WorkbookInventory;
use crate::workbook::xlsb::records::{
    binary_declared_count, le_u32, validate_xlsb_rich_string, visit_xlsb_records, xlsb_wide_string,
};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};

pub(in crate::workbook) fn scan_binary_shared_strings(
    data: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<WorkbookInventory, ConversionError> {
    let part = "xl/sharedStrings.bin";
    let mut output = WorkbookInventory::default();
    let mut declared = None;
    let mut ended = false;
    let mut future_depth = 0_u8;
    visit_xlsb_records(data, part, context, |typ, payload| {
        match typ {
            0x009f => {
                if declared.is_some() || ended || payload.len() != 8 {
                    return Err(malformed(Some(part), "invalid BrtBeginSst"));
                }
                let total = u64::from(le_u32(&payload[0..4]));
                let unique = u64::from(le_u32(&payload[4..8]));
                if unique > total || unique > options.limits.max_table_cells {
                    return Err(limit(
                        "max_table_cells",
                        "XLSB shared string declaration is too large",
                    ));
                }
                declared = Some(unique);
            }
            0x0013 => {
                if payload.len() < 5 || declared.is_none() || ended || future_depth != 0 {
                    return Err(malformed(Some(part), "invalid BrtSSTItem"));
                }
                let value = validate_xlsb_rich_string(payload, part, options)?;
                output.shared_strings = output.shared_strings.saturating_add(1);
                output.shared_string_bytes = output
                    .shared_string_bytes
                    .saturating_add(u64::try_from(value.len()).unwrap_or(u64::MAX));
            }
            0x00a0 => {
                if declared.is_none() || ended || future_depth != 0 || !payload.is_empty() {
                    return Err(malformed(Some(part), "invalid BrtEndSst"));
                }
                ended = true;
            }
            0x0023 => {
                if declared.is_none() || ended || future_depth != 0 {
                    return Err(malformed(Some(part), "invalid SST future-record block"));
                }
                future_depth = 1;
            }
            0x0024 => {
                if future_depth != 1 {
                    return Err(malformed(Some(part), "unbalanced SST future-record block"));
                }
                future_depth = 0;
            }
            _ => {}
        }
        Ok(())
    })?;
    if declared != Some(output.shared_strings) || !ended || future_depth != 0 {
        return Err(malformed(Some(part), "BrtBeginSst count disagrees with BrtSSTItem records"));
    }
    if output.shared_string_bytes > options.limits.max_decompressed_bytes {
        return Err(limit("max_decompressed_bytes", "XLSB shared string text is too large"));
    }
    Ok(output)
}

pub(in crate::workbook) fn scan_binary_style_counts(
    data: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<WorkbookInventory, ConversionError> {
    let part = "xl/styles.bin";
    let mut expected_formats = None;
    let mut actual_formats = 0_u64;
    let mut expected_xfs = None;
    let mut actual_xfs = 0_u64;
    let mut in_formats = false;
    let mut ended_formats = false;
    let mut in_xfs = false;
    let mut ended_xfs = false;
    let mut output = WorkbookInventory::default();
    visit_xlsb_records(data, part, context, |typ, payload| {
        match typ {
            0x0267 => {
                if expected_formats.is_some() || in_formats || ended_formats || in_xfs {
                    return Err(malformed(Some(part), "duplicate BrtBeginFmts"));
                }
                expected_formats = Some(binary_declared_count(payload, part, options)?);
                in_formats = true;
            }
            0x002c => {
                if !in_formats || payload.len() < 6 {
                    return Err(malformed(Some(part), "truncated BrtFmt"));
                }
                let (format, consumed) = xlsb_wide_string(payload, 2, false, part)?;
                if consumed != payload.len() {
                    return Err(malformed(Some(part), "invalid BrtFmt payload"));
                }
                let bytes = u64::try_from(format.len()).unwrap_or(u64::MAX);
                if bytes > options.limits.max_field_bytes {
                    return Err(limit("max_field_bytes", "XLSB number format is too large"));
                }
                output.style_format_bytes = output.style_format_bytes.saturating_add(bytes);
                actual_formats = actual_formats.saturating_add(1);
            }
            0x0268 => {
                if !in_formats || ended_formats || !payload.is_empty() {
                    return Err(malformed(Some(part), "invalid BrtEndFmts"));
                }
                in_formats = false;
                ended_formats = true;
            }
            0x0269 => {
                if !ended_formats || expected_xfs.is_some() || in_xfs || ended_xfs {
                    return Err(malformed(Some(part), "duplicate BrtBeginCellXFs"));
                }
                expected_xfs = Some(binary_declared_count(payload, part, options)?);
                in_xfs = true;
            }
            0x002f => {
                if !in_xfs || payload.len() < 4 {
                    return Err(malformed(Some(part), "truncated BrtXF"));
                }
                actual_xfs = actual_xfs.saturating_add(1);
            }
            0x026a => {
                if !in_xfs || ended_xfs || !payload.is_empty() {
                    return Err(malformed(Some(part), "invalid BrtEndCellXFs"));
                }
                in_xfs = false;
                ended_xfs = true;
            }
            _ => {}
        }
        Ok(())
    })?;
    if expected_formats.is_some_and(|value| value != actual_formats)
        || expected_xfs != Some(actual_xfs)
        || in_formats
        || !ended_formats
        || in_xfs
        || !ended_xfs
    {
        return Err(malformed(Some(part), "XLSB style declaration disagrees with records"));
    }
    if output.style_format_bytes > options.limits.max_decompressed_bytes {
        return Err(limit("max_decompressed_bytes", "XLSB number formats are too large"));
    }
    output.styles = actual_xfs;
    output.number_formats = actual_formats;
    Ok(output)
}
