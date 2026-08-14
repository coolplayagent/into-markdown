use crate::workbook::error::{limit, malformed};
use crate::workbook::extras::metadata::push_compact_range;
use crate::workbook::model::{BinaryFormulaContext, BinaryHyperlink, CellCoordinate, max_optional};
use crate::workbook::schema::{MAX_EXCEL_COLUMNS, MAX_EXCEL_ROWS};
use crate::workbook::xlsb::formulas::{parse_binary_hyperlink, validate_xlsb_formula_tokens};
use crate::workbook::xlsb::records::{le_u32, read_xlsb_varint, xlsb_wide_string};
use calamine::Dimensions;
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};

#[derive(Debug, Default)]
pub(in crate::workbook) struct XlsbScan {
    pub(in crate::workbook) dimensions: Option<(u32, u32)>,
    pub(in crate::workbook) declared_dimensions: Option<(u32, u32)>,
    pub(in crate::workbook) formula_preallocation_cells: u64,
    pub(in crate::workbook) merges: Vec<Dimensions>,
    pub(in crate::workbook) hyperlinks: Vec<BinaryHyperlink>,
    pub(in crate::workbook) hidden_rows: Vec<(u32, u32)>,
    pub(in crate::workbook) hidden_columns: Vec<(u32, u32)>,
    pub(in crate::workbook) cells: u64,
    pub(in crate::workbook) formulas: u64,
    pub(in crate::workbook) formula_bytes: u64,
    pub(in crate::workbook) max_formula_bytes: u64,
    pub(in crate::workbook) record_bytes: u64,
    pub(in crate::workbook) cell_value_bytes: u64,
    pub(in crate::workbook) max_shared_string_index: Option<u64>,
    pub(in crate::workbook) max_style_index: Option<u64>,
    pub(in crate::workbook) drawing_relationship_ids: Vec<String>,
}

#[allow(clippy::too_many_lines)] // BIFF12 worksheet ordering and payload checks are interdependent.
pub(in crate::workbook) fn scan_xlsb_sheet(
    data: &[u8],
    part: &str,
    formula_context: Option<BinaryFormulaContext>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<XlsbScan, ConversionError> {
    let mut offset = 0_usize;
    let mut records = 0_u64;
    let mut result = XlsbScan::default();
    let mut current_row = None;
    let mut last_row = None;
    let mut last_column = None;
    let mut saw_begin_sheet = false;
    let mut saw_end_sheet = false;
    let mut saw_dimension = false;
    let mut declared_dimensions = None;
    let mut in_sheet_data = false;
    let mut ended_sheet_data = false;
    let mut actual_bounds = None;
    let mut hidden_row_field_bytes = 0_u64;
    let mut hidden_column_field_bytes = 0_u64;
    while offset < data.len() {
        context.checkpoint()?;
        records = records.saturating_add(1);
        if records > u64::try_from(data.len()).unwrap_or(u64::MAX).saturating_add(1) {
            return Err(malformed(Some(part), "invalid XLSB record stream"));
        }
        let typ = u16::try_from(read_xlsb_varint(data, &mut offset, 2, part)?)
            .map_err(|_| malformed(Some(part), "XLSB record type overflow"))?;
        let len = usize::try_from(read_xlsb_varint(data, &mut offset, 4, part)?)
            .map_err(|_| malformed(Some(part), "XLSB record length overflow"))?;
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated XLSB record"))?;
        let payload = &data[offset..end];
        // Calamine 0.36.1 does not expose the authenticated expansion state
        // needed to preserve these worksheet formula containers. Once the
        // BIFF12 record header and complete declared payload are available,
        // reject them independently of worksheet ordering/state so they can
        // never be silently discarded by the third-party reader.
        if matches!(typ, 0x01aa..=0x01ac) {
            let name = match typ {
                0x01aa => "BrtArrFmla",
                0x01ab => "BrtShrFmla",
                0x01ac => "BrtTable",
                _ => unreachable!(),
            };
            return Err(ConversionError::Unsupported {
                detail: format!("unexpanded XLSB {name} record is unsupported ({part})"),
            });
        }
        if saw_end_sheet {
            return Err(malformed(Some(part), "record follows BrtEndSheet"));
        }
        if !saw_begin_sheet && typ != 0x0081 {
            return Err(malformed(Some(part), "BrtBeginSheet must be the first worksheet record"));
        }
        result.record_bytes =
            result.record_bytes.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        match typ {
            0x0081 => {
                if saw_begin_sheet || records != 1 || !payload.is_empty() {
                    return Err(malformed(Some(part), "invalid BrtBeginSheet"));
                }
                saw_begin_sheet = true;
            }
            0x0082 => {
                if !saw_begin_sheet
                    || saw_end_sheet
                    || in_sheet_data
                    || !ended_sheet_data
                    || !payload.is_empty()
                {
                    return Err(malformed(Some(part), "invalid BrtEndSheet state"));
                }
                saw_end_sheet = true;
            }
            // BrtRowHdr stores the zero-based row index followed by formatting and
            // row flags. Bit 4 of the row flags is fHidden.
            0x0000 => {
                if !in_sheet_data || payload.len() < 12 {
                    return Err(malformed(Some(part), "invalid BrtRowHdr length"));
                }
                let row = le_u32(&payload[0..4]);
                if row >= MAX_EXCEL_ROWS || last_row.is_some_and(|previous| row <= previous) {
                    return Err(malformed(Some(part), "invalid or out-of-order XLSB row index"));
                }
                if payload[11] & 0x10 != 0 {
                    push_compact_range(
                        &mut result.hidden_rows,
                        &mut hidden_row_field_bytes,
                        (row, row),
                        true,
                        part,
                        options,
                    )?;
                }
                current_row = Some(row);
                last_row = Some(row);
                last_column = None;
            }
            // BrtColInfo contains inclusive zero-based bounds and a 16-bit flag
            // field. Bit 0 is fHidden.
            0x003c => {
                if !saw_dimension || in_sheet_data || ended_sheet_data || payload.len() < 18 {
                    return Err(malformed(Some(part), "invalid BrtColInfo length"));
                }
                let first = le_u32(&payload[0..4]);
                let last = le_u32(&payload[4..8]);
                if first > last || last >= MAX_EXCEL_COLUMNS {
                    return Err(malformed(Some(part), "invalid XLSB column range"));
                }
                if u16::from_le_bytes([payload[16], payload[17]]) & 1 != 0 {
                    push_compact_range(
                        &mut result.hidden_columns,
                        &mut hidden_column_field_bytes,
                        (first, last),
                        false,
                        part,
                        options,
                    )?;
                }
            }
            0x0094 => {
                if !saw_begin_sheet || saw_dimension || in_sheet_data || payload.len() != 16 {
                    return Err(malformed(Some(part), "invalid BrtWsDim length"));
                }
                let first_row = le_u32(&payload[0..4]);
                let last_row = le_u32(&payload[4..8]);
                let first_col = le_u32(&payload[8..12]);
                let last_col = le_u32(&payload[12..16]);
                if first_row > last_row
                    || first_col > last_col
                    || last_row >= MAX_EXCEL_ROWS
                    || last_col >= MAX_EXCEL_COLUMNS
                {
                    return Err(malformed(Some(part), "invalid XLSB worksheet dimensions"));
                }
                declared_dimensions = Some((last_row, last_col));
                // Calamine 0.36.1's `Xlsb::worksheet_formula` constructs a
                // `Vec<Cell<String>>` with `BrtWsDim::len().min(1_000_000)`
                // capacity even when no formula/cell record exists. Model that
                // exact pinned transient independently from actual output bounds.
                result.formula_preallocation_cells = u64::from(last_row - first_row + 1)
                    .checked_mul(u64::from(last_col - first_col + 1))
                    .ok_or_else(|| limit("max_memory_bytes", "XLSB dimension area overflow"))?
                    .min(1_000_000);
                saw_dimension = true;
            }
            0x00b0 => {
                if !ended_sheet_data || in_sheet_data || payload.len() != 16 {
                    return Err(malformed(Some(part), "invalid BrtMergeCell length"));
                }
                let start = (le_u32(&payload[0..4]), le_u32(&payload[8..12]));
                let end = (le_u32(&payload[4..8]), le_u32(&payload[12..16]));
                if start.0 > end.0
                    || start.1 > end.1
                    || end.0 >= MAX_EXCEL_ROWS
                    || end.1 >= MAX_EXCEL_COLUMNS
                {
                    return Err(malformed(Some(part), "invalid XLSB merged range"));
                }
                result.merges.push(Dimensions { start, end });
                actual_bounds = Some(actual_bounds.map_or(end, |current: CellCoordinate| {
                    (current.0.max(end.0), current.1.max(end.1))
                }));
            }
            0x01ee => {
                if !ended_sheet_data || in_sheet_data {
                    return Err(malformed(Some(part), "BrtHLink is out of worksheet order"));
                }
                let hyperlink = parse_binary_hyperlink(payload, part)?;
                actual_bounds =
                    Some(actual_bounds.map_or(hyperlink.end, |current: CellCoordinate| {
                        (current.0.max(hyperlink.end.0), current.1.max(hyperlink.end.1))
                    }));
                result.hyperlinks.push(hyperlink);
            }
            // BrtDrawing contains exactly one non-null RelID to a DrawingML
            // part. It is the XLSB equivalent of worksheet/drawing/@r:id.
            0x0226 => {
                if in_sheet_data || !ended_sheet_data {
                    return Err(malformed(Some(part), "BrtDrawing is out of worksheet order"));
                }
                let (relationship_id, consumed) = xlsb_wide_string(payload, 0, false, part)?;
                if relationship_id.is_empty()
                    || consumed != payload.len()
                    || !result.drawing_relationship_ids.is_empty()
                {
                    return Err(malformed(Some(part), "invalid or duplicate BrtDrawing"));
                }
                result.drawing_relationship_ids.push(relationship_id);
            }
            0x0091 => {
                if !saw_dimension || in_sheet_data || ended_sheet_data || !payload.is_empty() {
                    return Err(malformed(Some(part), "invalid BrtBeginSheetData state"));
                }
                in_sheet_data = true;
            }
            0x0092 => {
                if !in_sheet_data || !payload.is_empty() {
                    return Err(malformed(Some(part), "invalid BrtEndSheetData state"));
                }
                in_sheet_data = false;
                ended_sheet_data = true;
                current_row = None;
                last_column = None;
            }
            0x0001..=0x000b => {
                if !in_sheet_data {
                    return Err(malformed(Some(part), "XLSB cell is outside sheet data"));
                }
                let row = current_row
                    .ok_or_else(|| malformed(Some(part), "XLSB cell precedes BrtRowHdr"))?;
                validate_xlsb_cell_record(
                    typ,
                    payload,
                    part,
                    formula_context,
                    options,
                    context,
                    &mut result,
                )?;
                let column = le_u32(&payload[0..4]);
                if column >= MAX_EXCEL_COLUMNS
                    || last_column.is_some_and(|previous| column <= previous)
                {
                    return Err(malformed(Some(part), "invalid or out-of-order XLSB cell column"));
                }
                last_column = Some(column);
                actual_bounds =
                    Some(actual_bounds.map_or((row, column), |current: CellCoordinate| {
                        (current.0.max(row), current.1.max(column))
                    }));
            }
            _ => {}
        }
        offset = end;
        if result.merges.len() as u64 > options.limits.max_table_cells
            || result.hyperlinks.len() as u64 > options.limits.max_table_cells
            || result.hidden_rows.len() as u64 > options.limits.max_table_cells
            || result.hidden_columns.len() as u64 > options.limits.max_table_cells
            || result.cells > options.limits.max_table_cells
        {
            return Err(limit("max_table_cells", "too many XLSB worksheet metadata records"));
        }
    }
    if in_sheet_data || !saw_begin_sheet || !saw_end_sheet || !ended_sheet_data || !saw_dimension {
        return Err(malformed(Some(part), "incomplete XLSB worksheet record state"));
    }
    if let (Some(declared), Some(actual)) = (declared_dimensions, actual_bounds)
        && (declared.0 < actual.0 || declared.1 < actual.1)
    {
        return Err(malformed(Some(part), "BrtWsDim under-reports actual XLSB cells"));
    }
    // BrtWsDim is only a consistency declaration. Stale over-reports do not
    // expand dense Calamine/IR allocations or reject an otherwise small sheet.
    result.declared_dimensions = declared_dimensions;
    result.dimensions = actual_bounds;
    Ok(result)
}

fn validate_xlsb_cell_record(
    typ: u16,
    payload: &[u8],
    part: &str,
    formula_context: Option<BinaryFormulaContext>,
    options: &ConversionOptions,
    context: &ExecutionContext,
    result: &mut XlsbScan,
) -> Result<(), ConversionError> {
    if payload.len() < 8 {
        return Err(malformed(Some(part), format!("truncated XLSB cell record 0x{typ:04x}")));
    }
    let style_index =
        u64::from(payload[4]) | (u64::from(payload[5]) << 8) | (u64::from(payload[6]) << 16);
    result.max_style_index = max_optional(result.max_style_index, Some(style_index));
    match typ {
        0x0001 if payload.len() == 8 => {}
        0x0002 if payload.len() == 12 => {}
        0x0003 if payload.len() == 9 => validate_xlsb_error(payload[8], part)?,
        0x0004 if payload.len() == 9 && payload[8] <= 1 => {}
        0x0005 if payload.len() == 16 => {}
        0x0006 => {
            let (_, consumed) = xlsb_wide_string(payload, 8, false, part)?;
            if consumed != payload.len() {
                return Err(malformed(Some(part), "invalid BrtCellSt payload"));
            }
            result.cell_value_bytes = result
                .cell_value_bytes
                .saturating_add(u64::try_from(consumed.saturating_sub(12)).unwrap_or(u64::MAX));
        }
        0x0007 if payload.len() == 12 => {
            let index = u64::from(le_u32(&payload[8..12]));
            result.max_shared_string_index =
                max_optional(result.max_shared_string_index, Some(index));
        }
        0x0008 => {
            let (_, consumed) = xlsb_wide_string(payload, 8, false, part)?;
            let minimum = consumed
                .checked_add(6)
                .ok_or_else(|| malformed(Some(part), "BrtFmlaString size overflow"))?;
            if payload.len() < minimum {
                return Err(malformed(Some(part), "truncated BrtFmlaString"));
            }
            result.cell_value_bytes = result
                .cell_value_bytes
                .saturating_add(u64::try_from(consumed.saturating_sub(12)).unwrap_or(u64::MAX));
        }
        0x0009 if payload.len() >= 22 => {}
        0x000a if payload.len() >= 15 && payload[8] <= 1 => {}
        0x000b if payload.len() >= 15 => validate_xlsb_error(payload[8], part)?,
        _ => {
            return Err(malformed(Some(part), format!("invalid XLSB cell record 0x{typ:04x}")));
        }
    }
    result.cells = result.cells.saturating_add(1);
    if matches!(typ, 0x0008..=0x000b) {
        let formula_offset = match typ {
            0x0008 => {
                let length = usize::try_from(le_u32(&payload[8..12]))
                    .map_err(|_| malformed(Some(part), "XLSB cached string length overflow"))?;
                14_usize
                    .checked_add(
                        length.checked_mul(2).ok_or_else(|| {
                            malformed(Some(part), "XLSB cached string size overflow")
                        })?,
                    )
                    .ok_or_else(|| malformed(Some(part), "XLSB cached string offset overflow"))?
            }
            0x0009 => 18,
            _ => 11,
        };
        let length_end = formula_offset
            .checked_add(4)
            .filter(|end| *end <= payload.len())
            .ok_or_else(|| malformed(Some(part), "truncated XLSB formula length"))?;
        let formula_bytes = usize::try_from(le_u32(&payload[formula_offset..length_end]))
            .map_err(|_| malformed(Some(part), "XLSB formula length overflow"))?;
        let formula_end = length_end
            .checked_add(formula_bytes)
            .filter(|end| *end <= payload.len())
            .ok_or_else(|| malformed(Some(part), "truncated XLSB formula tokens"))?;
        if let Some(formula_context) = formula_context {
            validate_xlsb_formula_tokens(
                &payload[length_end..formula_end],
                formula_context,
                part,
                options,
                context,
                0,
            )?;
        }
        result.formulas = result.formulas.saturating_add(1);
        result.formula_bytes =
            result.formula_bytes.saturating_add(u64::try_from(formula_bytes).unwrap_or(u64::MAX));
        result.max_formula_bytes =
            result.max_formula_bytes.max(u64::try_from(formula_bytes).unwrap_or(u64::MAX));
        if result.formula_bytes > options.limits.max_decompressed_bytes {
            return Err(limit("max_decompressed_bytes", "XLSB formula tokens are too large"));
        }
    }
    if result.cell_value_bytes > options.limits.max_decompressed_bytes {
        return Err(limit("max_decompressed_bytes", "XLSB cached cell text is too large"));
    }
    Ok(())
}

fn validate_xlsb_error(value: u8, part: &str) -> Result<(), ConversionError> {
    if matches!(value, 0x00 | 0x07 | 0x0f | 0x17 | 0x1d | 0x24 | 0x2a | 0x2b) {
        Ok(())
    } else {
        Err(malformed(Some(part), "invalid XLSB cached error value"))
    }
}
