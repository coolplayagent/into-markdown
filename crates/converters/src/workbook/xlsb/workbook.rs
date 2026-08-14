use crate::workbook::error::{limit, malformed};
use crate::workbook::model::{BinaryFormulaContext, WorkbookInventory};
use crate::workbook::xlsb::formulas::validate_xlsb_formula_tokens;
use crate::workbook::xlsb::records::{
    le_u32, read_xlsb_varint, visit_xlsb_records, xlsb_wide_string,
};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use std::collections::BTreeSet;

pub(in crate::workbook) fn parse_binary_workbook_sheets(
    data: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<(String, String)>, ConversionError> {
    let part = "xl/workbook.bin";
    let mut offset = 0_usize;
    let mut output = Vec::new();
    let mut names = BTreeSet::new();
    while offset < data.len() {
        context.checkpoint()?;
        let typ = u16::try_from(read_xlsb_varint(data, &mut offset, 2, part)?)
            .map_err(|_| malformed(Some(part), "XLSB record type overflow"))?;
        let len = usize::try_from(read_xlsb_varint(data, &mut offset, 4, part)?)
            .map_err(|_| malformed(Some(part), "XLSB record length overflow"))?;
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated workbook record"))?;
        if typ == 0x009c {
            let payload = &data[offset..end];
            if payload.len() < 16 {
                return Err(malformed(Some(part), "invalid BrtBundleSh record"));
            }
            let (relationship_id, consumed) = xlsb_wide_string(payload, 8, false, part)?;
            let (name, consumed) = xlsb_wide_string(payload, consumed, false, part)?;
            if consumed != payload.len() || le_u32(&payload[0..4]) > 2 {
                return Err(malformed(Some(part), "invalid BrtBundleSh payload"));
            }
            if name.is_empty()
                || u64::try_from(name.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes
                || !names.insert(name.clone())
            {
                return Err(malformed(Some(part), "invalid or duplicate XLSB sheet name"));
            }
            output.push((name, relationship_id));
            if u32::try_from(output.len()).unwrap_or(u32::MAX) > options.limits.max_archive_entries
            {
                return Err(limit("max_archive_entries", "too many XLSB workbook sheets"));
            }
        }
        offset = end;
    }
    Ok(output)
}

#[allow(clippy::too_many_lines)] // BIFF12 global records are validated in parser-consumption order.
pub(in crate::workbook) fn scan_binary_workbook_surface(
    data: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(WorkbookInventory, BinaryFormulaContext), ConversionError> {
    let part = "xl/workbook.bin";
    let mut output = WorkbookInventory::default();
    let mut formula_context = BinaryFormulaContext::default();
    let mut saw_bundle_end = false;
    let mut saw_workbook_properties = false;
    let mut bundled_sheets = 0_u64;
    let mut saw_external_sheets = false;
    let mut saw_metadata_end = false;
    visit_xlsb_records(data, part, context, |typ, payload| {
        output.record_bytes =
            output.record_bytes.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        match typ {
            0x0099 => {
                if saw_workbook_properties || saw_bundle_end || payload.is_empty() {
                    return Err(malformed(Some(part), "truncated BrtWbProp"));
                }
                saw_workbook_properties = true;
            }
            0x0090 => {
                if saw_bundle_end || !payload.is_empty() {
                    return Err(malformed(Some(part), "invalid BrtEndBundleShs"));
                }
                saw_bundle_end = true;
            }
            0x009c => {
                if saw_bundle_end || payload.len() < 16 {
                    return Err(malformed(Some(part), "truncated BrtBundleSh"));
                }
                if le_u32(&payload[0..4]) > 2 {
                    return Err(malformed(Some(part), "invalid XLSB sheet visibility"));
                }
                let (relationship_id, relationship_end) =
                    xlsb_wide_string(payload, 8, false, part)?;
                let (name, consumed) = xlsb_wide_string(payload, relationship_end, false, part)?;
                if relationship_id.is_empty() || name.is_empty() || consumed != payload.len() {
                    return Err(malformed(Some(part), "invalid BrtBundleSh payload"));
                }
                bundled_sheets = bundled_sheets.saturating_add(1);
                output.max_formula_reference_bytes = output
                    .max_formula_reference_bytes
                    .max(u64::try_from(name.len()).unwrap_or(u64::MAX));
            }
            0x016a => {
                if !saw_bundle_end || saw_external_sheets || payload.len() < 4 {
                    return Err(malformed(Some(part), "invalid BrtExternSheet state"));
                }
                let count = usize::try_from(le_u32(&payload[..4]))
                    .map_err(|_| malformed(Some(part), "external-sheet count overflow"))?;
                if u64::try_from(count).unwrap_or(u64::MAX) > options.limits.max_table_cells {
                    return Err(limit("max_table_cells", "too many XLSB external-sheet slots"));
                }
                let expected =
                    4_usize
                        .checked_add(count.checked_mul(12).ok_or_else(|| {
                            malformed(Some(part), "external-sheet payload overflow")
                        })?)
                        .ok_or_else(|| malformed(Some(part), "external-sheet payload overflow"))?;
                if expected != payload.len() {
                    return Err(malformed(Some(part), "invalid BrtExternSheet payload"));
                }
                formula_context.external_sheets = count;
                output.external_sheet_slots = u64::try_from(count).unwrap_or(u64::MAX);
                saw_external_sheets = true;
            }
            0x0027 => {
                if !saw_bundle_end || saw_metadata_end || payload.len() < 13 {
                    return Err(malformed(Some(part), "truncated BrtName"));
                }
                let (name, name_end) = xlsb_wide_string(payload, 9, false, part)?;
                if u64::try_from(name.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
                    return Err(limit("max_field_bytes", "XLSB defined name is too large"));
                }
                let length_end = name_end
                    .checked_add(4)
                    .filter(|end| *end <= payload.len())
                    .ok_or_else(|| malformed(Some(part), "truncated BrtName formula length"))?;
                let formula_length = usize::try_from(le_u32(&payload[name_end..length_end]))
                    .map_err(|_| malformed(Some(part), "BrtName formula length overflow"))?;
                let formula_end = length_end
                    .checked_add(formula_length)
                    .filter(|end| *end <= payload.len())
                    .ok_or_else(|| malformed(Some(part), "truncated BrtName formula"))?;
                validate_xlsb_formula_tokens(
                    &payload[length_end..formula_end],
                    formula_context,
                    part,
                    options,
                    context,
                    0,
                )?;
                output.defined_names = output.defined_names.saturating_add(1);
                output.defined_name_bytes = output
                    .defined_name_bytes
                    .saturating_add(u64::try_from(name.len()).unwrap_or(u64::MAX))
                    .saturating_add(u64::try_from(formula_length).unwrap_or(u64::MAX));
                output.max_formula_reference_bytes = output
                    .max_formula_reference_bytes
                    .max(u64::try_from(name.len()).unwrap_or(u64::MAX));
                // Formula extra-data may follow rgce; Calamine does not read it,
                // but it remains bounded by the authenticated record payload.
                formula_context.defined_names =
                    usize::try_from(output.defined_names).unwrap_or(usize::MAX);
            }
            0x009d | 0x0225 | 0x018d | 0x0180 | 0x009a | 0x0252 | 0x0229 | 0x009b | 0x0084 => {
                if !saw_bundle_end {
                    return Err(malformed(Some(part), "workbook metadata terminates too early"));
                }
                saw_metadata_end = true;
            }
            _ => {}
        }
        if output.defined_names > options.limits.max_table_cells
            || bundled_sheets > u64::from(options.limits.max_archive_entries)
            || output.defined_name_bytes > options.limits.max_decompressed_bytes
        {
            return Err(limit("max_table_cells", "XLSB defined-name budget exceeded"));
        }
        Ok(())
    })?;
    if !saw_workbook_properties || !saw_bundle_end || !saw_metadata_end {
        return Err(malformed(Some(part), "incomplete XLSB workbook record state"));
    }
    Ok((output, formula_context))
}
