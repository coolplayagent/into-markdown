use super::preflight::{
    biff_record, decode_continued_formula_string, has_noncanonical_formula_string_cache,
};
use super::{
    BIFF4, BIFF5, BOF, BOF4, EOF, ErrorPolicy, FORMULA, LegacyBudget, malformed, read_u16, read_u32,
};
use crate::workbook::{
    LegacyCellFormat, LegacyFormulaCache, LegacyFormulaExpression, LegacyXlsHints,
};
use into_markdown_core::{ConversionError, ExecutionContext};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const BOUND_SHEET: u16 = 0x0085;
const MERGED_CELLS: u16 = 0x00e5;
const MUL_RK: u16 = 0x00bd;
const MUL_BLANK: u16 = 0x00be;
const FORMAT: u16 = 0x041e;
const XF: u16 = 0x00e0;

#[derive(Debug)]
struct BoundSheet {
    offset: usize,
    name: String,
}

#[derive(Default)]
struct GlobalInventory {
    sheets: Vec<BoundSheet>,
    formats: BTreeMap<u16, String>,
    xfs: Vec<u16>,
    recovered_format_records: usize,
}

#[derive(Default)]
struct SheetBounds {
    row: Option<u32>,
    column: Option<u32>,
}

impl SheetBounds {
    fn include(&mut self, row: u32, column: u32) {
        self.row = Some(self.row.map_or(row, |value| value.max(row)));
        self.column = Some(self.column.map_or(column, |value| value.max(column)));
    }

    fn end(&self) -> Option<(u32, u32)> {
        Some((self.row?, self.column?))
    }
}

pub(super) fn scan_workbook_inventory(
    bytes: &[u8],
    biff_version: u16,
    part: &str,
    budget: &mut LegacyBudget<'_>,
    context: &ExecutionContext,
    error_policy: ErrorPolicy,
) -> Result<LegacyXlsHints, ConversionError> {
    let memory = context.reserve_memory(inventory_memory_plan(bytes.len())?)?;
    let mut global = collect_globals(bytes, biff_version, part, budget, error_policy)?;
    if global.sheets.is_empty() {
        global.sheets.push(BoundSheet { offset: 0, name: "Sheet 1".into() });
    }
    let offsets = authenticate_sheets(&global.sheets, bytes.len(), part)?;
    let inventory = collect_sheet_inventory(bytes, biff_version, part, budget, &global, &offsets)?;
    finalize_inventory(global, inventory, memory, part)
}

fn authenticate_sheets(
    sheets: &[BoundSheet],
    workbook_bytes: usize,
    part: &str,
) -> Result<BTreeMap<usize, usize>, ConversionError> {
    let mut offsets = BTreeMap::new();
    let mut names = BTreeSet::new();
    for (index, sheet) in sheets.iter().enumerate() {
        if sheet.offset >= workbook_bytes || offsets.insert(sheet.offset, index).is_some() {
            return Err(malformed(part, "duplicate or out-of-range BoundSheet offset"));
        }
        if !names.insert(sheet.name.to_lowercase()) {
            return Err(malformed(part, "duplicate worksheet name"));
        }
    }
    Ok(offsets)
}

struct SheetInventory {
    bounds: Vec<SheetBounds>,
    completed: Vec<bool>,
    formula_caches: Vec<LegacyFormulaCache>,
    cell_formats: Vec<LegacyCellFormat>,
    formula_expressions: Vec<LegacyFormulaExpression>,
}

#[derive(Default)]
struct InventorySubstreams {
    stack: [u16; 16],
    depth: usize,
    current_sheet: Option<usize>,
}

impl InventorySubstreams {
    fn open(
        &mut self,
        body: &[u8],
        record_start: usize,
        offsets: &BTreeMap<usize, usize>,
        completed: &[bool],
        part: &str,
    ) -> Result<(), ConversionError> {
        let substream = read_u16(body, 2, part)?;
        if self.depth == self.stack.len() {
            return Err(malformed(part, "BIFF inventory nesting is too deep"));
        }
        if self.depth != 0 && (self.stack[self.depth - 1] != 0x0010 || substream != 0x0020) {
            return Err(malformed(part, "unsupported nested BIFF inventory substream"));
        }
        self.stack[self.depth] = substream;
        self.depth += 1;
        if substream == 0x0010 {
            if self.current_sheet.is_some() || self.depth != 1 {
                return Err(malformed(part, "nested BIFF worksheet substream"));
            }
            let index = offsets.get(&record_start).copied().ok_or_else(|| {
                malformed(part, "worksheet BOF is not authenticated by BoundSheet")
            })?;
            if completed[index] {
                return Err(malformed(part, "duplicate worksheet substream"));
            }
            self.current_sheet = Some(index);
        }
        Ok(())
    }

    fn close(&mut self, completed: &mut [bool], part: &str) -> Result<(), ConversionError> {
        if self.depth == 0 {
            return Err(malformed(part, "BIFF inventory EOF has no BOF"));
        }
        self.depth -= 1;
        if self.stack[self.depth] == 0x0010 {
            let index = self
                .current_sheet
                .take()
                .ok_or_else(|| malformed(part, "worksheet inventory has no open sheet"))?;
            completed[index] = true;
        }
        Ok(())
    }

    fn in_worksheet(&self) -> bool {
        self.current_sheet.is_some() && self.depth == 1
    }
}

fn collect_sheet_inventory(
    bytes: &[u8],
    biff_version: u16,
    part: &str,
    budget: &mut LegacyBudget<'_>,
    global: &GlobalInventory,
    offsets: &BTreeMap<usize, usize>,
) -> Result<SheetInventory, ConversionError> {
    let mut output = SheetInventory {
        bounds: (0..global.sheets.len()).map(|_| SheetBounds::default()).collect(),
        completed: vec![false; global.sheets.len()],
        formula_caches: Vec::new(),
        cell_formats: Vec::new(),
        formula_expressions: Vec::new(),
    };
    let mut substreams = InventorySubstreams::default();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        budget.work(1, part)?;
        let record_start = cursor;
        let (kind, body, end) = biff_record(bytes, cursor, part)?;
        match kind {
            BOF | BOF4 => {
                substreams.open(body, record_start, offsets, &output.completed, part)?;
            }
            EOF => {
                substreams.close(&mut output.completed, part)?;
            }
            MERGED_CELLS if substreams.in_worksheet() => {
                include_merged_ranges(
                    body,
                    &mut output.bounds[substreams.current_sheet.unwrap_or(0)],
                    part,
                    budget,
                )?;
            }
            MUL_RK | MUL_BLANK if substreams.in_worksheet() => {
                let (row, first, last) = multi_cell_range(kind, body, part)?;
                let sheet_index = substreams.current_sheet.unwrap_or(0);
                let sheet = &mut output.bounds[sheet_index];
                sheet.include(row, u32::from(first));
                sheet.include(row, u32::from(last));
                collect_multi_cell_formats(
                    kind,
                    body,
                    &MultiCellOrigin { sheet_index, row, first_column: first },
                    global,
                    &mut output.cell_formats,
                    part,
                )?;
            }
            FORMULA if substreams.in_worksheet() => {
                collect_formula(
                    FormulaRecord {
                        bytes,
                        next_record: end,
                        body,
                        sheet_index: substreams.current_sheet.unwrap_or(0),
                        biff_version,
                    },
                    global,
                    &mut output,
                    part,
                    budget,
                )?;
            }
            _ if substreams.in_worksheet() && is_single_cell_record(kind, biff_version) => {
                let row = u32::from(read_u16(body, 0, part)?);
                let column = u32::from(read_u16(body, 2, part)?);
                let sheet_index = substreams.current_sheet.unwrap_or(0);
                output.bounds[sheet_index].include(row, column);
                collect_cell_format(
                    body,
                    sheet_index,
                    row,
                    column,
                    global,
                    &mut output.cell_formats,
                    part,
                )?;
            }
            _ => {}
        }
        cursor = end;
    }
    if substreams.depth != 0
        || substreams.current_sheet.is_some()
        || output.completed.iter().any(|value| !value)
    {
        return Err(malformed(part, "BoundSheet inventory is not closed by worksheet EOF records"));
    }
    Ok(output)
}

#[derive(Clone, Copy)]
struct FormulaRecord<'a> {
    bytes: &'a [u8],
    next_record: usize,
    body: &'a [u8],
    sheet_index: usize,
    biff_version: u16,
}

fn collect_formula(
    record: FormulaRecord<'_>,
    global: &GlobalInventory,
    output: &mut SheetInventory,
    part: &str,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let row = u32::from(read_u16(record.body, 0, part)?);
    let column = u32::from(read_u16(record.body, 2, part)?);
    output.bounds[record.sheet_index].include(row, column);
    collect_cell_format(
        record.body,
        record.sheet_index,
        row,
        column,
        global,
        &mut output.cell_formats,
        part,
    )?;
    if has_noncanonical_formula_string_cache(record.body)
        && let Some(value) = decode_continued_formula_string(
            record.bytes,
            record.next_record,
            part,
            budget,
            budget.max_field_bytes(),
        )?
    {
        output.formula_caches.push(LegacyFormulaCache {
            sheet_index: record.sheet_index,
            row,
            column,
            value,
        });
    }
    let (value, tokens) = formula_expression(record.body, record.biff_version, part)?;
    output.formula_expressions.push(LegacyFormulaExpression {
        sheet_index: record.sheet_index,
        row,
        column,
        value,
        token_sha256: Sha256::digest(tokens).into(),
    });
    Ok(())
}

fn finalize_inventory(
    mut global: GlobalInventory,
    mut inventory: SheetInventory,
    memory: into_markdown_core::ResourceReservation,
    part: &str,
) -> Result<LegacyXlsHints, ConversionError> {
    inventory.formula_caches.sort_by_key(|cache| (cache.sheet_index, cache.row, cache.column));
    if inventory.formula_caches.windows(2).any(|pair| {
        (pair[0].sheet_index, pair[0].row, pair[0].column)
            == (pair[1].sheet_index, pair[1].row, pair[1].column)
    }) {
        return Err(malformed(part, "duplicate formula cache coordinate"));
    }
    let mut authenticated_bounds = BTreeMap::new();
    let mut authenticated_empty_sheets = BTreeSet::new();
    for (sheet, bounds) in global.sheets.drain(..).zip(inventory.bounds) {
        if let Some(end) = bounds.end() {
            authenticated_bounds.insert(sheet.name, end);
        } else {
            authenticated_empty_sheets.insert(sheet.name);
        }
    }
    inventory.cell_formats.sort_by_key(|format| (format.sheet_index, format.row, format.column));
    if inventory.cell_formats.windows(2).any(|pair| {
        (pair[0].sheet_index, pair[0].row, pair[0].column)
            == (pair[1].sheet_index, pair[1].row, pair[1].column)
    }) {
        return Err(malformed(part, "duplicate formatted-cell coordinate"));
    }
    inventory
        .formula_expressions
        .sort_by_key(|formula| (formula.sheet_index, formula.row, formula.column));
    if inventory.formula_expressions.windows(2).any(|pair| {
        (pair[0].sheet_index, pair[0].row, pair[0].column)
            == (pair[1].sheet_index, pair[1].row, pair[1].column)
    }) {
        return Err(malformed(part, "duplicate formula-expression coordinate"));
    }
    Ok(LegacyXlsHints {
        authenticated_bounds,
        authenticated_empty_sheets,
        formula_caches: inventory.formula_caches,
        cell_formats: inventory.cell_formats,
        format_codes: global.formats,
        formula_expressions: inventory.formula_expressions,
        recovered_format_records: global.recovered_format_records,
        _memory: Some(memory),
    })
}

pub(super) fn inventory_memory_plan(bytes: usize) -> Result<u64, ConversionError> {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    let indexed_format_multiplier =
        u64::try_from(std::mem::size_of::<LegacyCellFormat>()).unwrap_or(u64::MAX).div_ceil(2);
    // MULBLANK can encode one formatted coordinate in two bytes. The indexed
    // coordinate inventory therefore dominates the retained working set; three
    // additional input-sized units cover formula hashes/caches, bounds, maps,
    // and sorting scratch without cloning format strings per cell.
    let multiplier = indexed_format_multiplier
        .checked_add(3)
        .ok_or_else(|| super::limit("max_memory_bytes", "legacy XLS inventory plan overflowed"))?;
    bytes
        .checked_mul(multiplier)
        .and_then(|value| value.checked_add(4_096))
        .ok_or_else(|| super::limit("max_memory_bytes", "legacy XLS inventory plan overflowed"))
}

fn formula_expression<'a>(
    body: &'a [u8],
    _biff_version: u16,
    part: &str,
) -> Result<(Option<String>, &'a [u8]), ConversionError> {
    // Raw BIFF4 Formula records are normalized to the shared four-byte-reserved
    // framing before inventory; container BIFF5/8 records already use it.
    let length_offset = 20;
    let token_bytes = usize::from(read_u16(body, length_offset, part)?);
    let tokens = body
        .get(length_offset + 2..length_offset + 2 + token_bytes)
        .ok_or_else(|| malformed(part, "truncated Formula token stream"))?;
    if tokens.len() != 5 || !matches!(tokens[0], 0x01 | 0x02) {
        return Ok((None, tokens));
    }
    let row = u32::from(u16::from_le_bytes([tokens[1], tokens[2]])) + 1;
    let column = u32::from(u16::from_le_bytes([tokens[3], tokens[4]])) + 1;
    let function = if tokens[0] == 0x01 { "SHARED" } else { "TABLE" };
    Ok((Some(format!("{function}(R{row}C{column})")), tokens))
}

fn collect_globals(
    bytes: &[u8],
    biff_version: u16,
    part: &str,
    budget: &mut LegacyBudget<'_>,
    error_policy: ErrorPolicy,
) -> Result<GlobalInventory, ConversionError> {
    let mut output = GlobalInventory::default();
    let mut cursor = 0_usize;
    let mut legacy_format_index = 0_u16;
    let mut closed = false;
    while cursor < bytes.len() {
        budget.work(1, part)?;
        let (kind, body, end) = biff_record(bytes, cursor, part)?;
        match kind {
            BOUND_SHEET => output.sheets.push(parse_bound_sheet(body, biff_version, part)?),
            FORMAT => {
                let index = if biff_version == BIFF4 {
                    let index = legacy_format_index;
                    legacy_format_index = legacy_format_index
                        .checked_add(1)
                        .ok_or_else(|| malformed(part, "BIFF4 Format index overflowed"))?;
                    index
                } else {
                    read_u16(body, 0, part)?
                };
                match parse_format_string(&body[2..], biff_version, part) {
                    Ok(value) => {
                        if output.formats.contains_key(&index) {
                            if error_policy == ErrorPolicy::Strict {
                                return Err(malformed(part, "duplicate BIFF Format index"));
                            }
                            output.recovered_format_records += 1;
                        }
                        output.formats.entry(index).or_insert(value);
                    }
                    Err(_) if error_policy == ErrorPolicy::BestEffort => {
                        output.recovered_format_records += 1;
                    }
                    Err(error) => return Err(error),
                }
            }
            XF => {
                if body.len() < 4 {
                    return Err(malformed(part, "truncated BIFF XF record"));
                }
                output.xfs.push(read_u16(body, 2, part)?);
            }
            EOF => {
                closed = true;
                break;
            }
            _ => {}
        }
        cursor = end;
    }
    if !closed {
        return Err(malformed(part, "BIFF global substream has no EOF"));
    }
    Ok(output)
}

fn parse_format_string(
    bytes: &[u8],
    biff_version: u16,
    part: &str,
) -> Result<String, ConversionError> {
    if matches!(biff_version, BIFF4 | BIFF5) {
        let characters = usize::from(
            *bytes.first().ok_or_else(|| malformed(part, "truncated legacy BIFF Format string"))?,
        );
        let data = bytes
            .get(1..1 + characters)
            .ok_or_else(|| malformed(part, "truncated legacy BIFF Format string"))?;
        if bytes.len() != 1 + characters {
            return Err(malformed(part, "legacy BIFF Format string has trailing data"));
        }
        return Ok(data.iter().map(|byte| char::from(*byte)).collect());
    }
    parse_biff_unicode_string(bytes, part)
}

fn parse_biff_unicode_string(bytes: &[u8], part: &str) -> Result<String, ConversionError> {
    let characters = usize::from(read_u16(bytes, 0, part)?);
    let flags = *bytes.get(2).ok_or_else(|| malformed(part, "truncated BIFF string flags"))?;
    if flags & !1 != 0 {
        return Err(malformed(part, "unsupported BIFF Format string flags"));
    }
    let width = if flags == 0 { 1 } else { 2 };
    let byte_count = characters
        .checked_mul(width)
        .ok_or_else(|| malformed(part, "BIFF Format string size overflowed"))?;
    let data = bytes
        .get(3..3 + byte_count)
        .ok_or_else(|| malformed(part, "truncated BIFF Format string"))?;
    if bytes.len() != 3 + byte_count {
        return Err(malformed(part, "BIFF Format string has trailing data"));
    }
    if width == 1 {
        Ok(data.iter().map(|byte| char::from(*byte)).collect())
    } else {
        let units = data
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&units)
            .map_err(|_| malformed(part, "BIFF Format string contains invalid UTF-16"))
    }
}

fn collect_cell_format(
    body: &[u8],
    sheet_index: usize,
    row: u32,
    column: u32,
    global: &GlobalInventory,
    output: &mut Vec<LegacyCellFormat>,
    part: &str,
) -> Result<(), ConversionError> {
    let xf = usize::from(read_u16(body, 4, part)?);
    push_cell_format(sheet_index, row, column, xf, global, output, part)
}

struct MultiCellOrigin {
    sheet_index: usize,
    row: u32,
    first_column: u16,
}

fn collect_multi_cell_formats(
    kind: u16,
    body: &[u8],
    origin: &MultiCellOrigin,
    global: &GlobalInventory,
    output: &mut Vec<LegacyCellFormat>,
    part: &str,
) -> Result<(), ConversionError> {
    let stride = if kind == MUL_RK { 6 } else { 2 };
    let entries = (body.len() - 6) / stride;
    for index in 0..entries {
        let xf = usize::from(read_u16(body, 4 + index * stride, part)?);
        let column = u32::from(origin.first_column)
            .checked_add(u32::try_from(index).unwrap_or(u32::MAX))
            .ok_or_else(|| malformed(part, "multi-cell format coordinate overflowed"))?;
        push_cell_format(origin.sheet_index, origin.row, column, xf, global, output, part)?;
    }
    Ok(())
}

fn push_cell_format(
    sheet_index: usize,
    row: u32,
    column: u32,
    xf: usize,
    global: &GlobalInventory,
    output: &mut Vec<LegacyCellFormat>,
    part: &str,
) -> Result<(), ConversionError> {
    if global.xfs.is_empty() {
        return Ok(());
    }
    let format_index = *global
        .xfs
        .get(xf)
        .ok_or_else(|| malformed(part, "cell references an out-of-range XF record"))?;
    if display_format_code(format_index, &global.formats).is_none() {
        return Ok(());
    }
    output.push(LegacyCellFormat { sheet_index, row, column, format_index });
    Ok(())
}

fn display_format_code(index: u16, custom: &BTreeMap<u16, String>) -> Option<&str> {
    let code = custom.get(&index).map(String::as_str).or(match index {
        9 => Some("0%"),
        10 => Some("0.00%"),
        _ => None,
    });
    code.filter(|code| {
        code.contains('%')
            || code.contains('$')
            || code.contains('€')
            || code.contains('£')
            || code.contains('¥')
    })
}

fn parse_bound_sheet(
    body: &[u8],
    biff_version: u16,
    part: &str,
) -> Result<BoundSheet, ConversionError> {
    let offset = usize::try_from(read_u32(body, 0, part)?)
        .map_err(|_| malformed(part, "BoundSheet offset is not representable"))?;
    let characters = usize::from(
        *body.get(6).ok_or_else(|| malformed(part, "truncated BoundSheet name length"))?,
    );
    if characters == 0 || characters > 31 {
        return Err(malformed(part, "invalid BoundSheet name length"));
    }
    let (name, consumed) = if matches!(biff_version, BIFF4 | BIFF5) {
        let bytes = body
            .get(7..7 + characters)
            .ok_or_else(|| malformed(part, "truncated legacy BoundSheet name"))?;
        (bytes.iter().map(|byte| char::from(*byte)).collect(), 7 + characters)
    } else {
        let flags = *body.get(7).ok_or_else(|| malformed(part, "truncated BoundSheet flags"))?;
        if flags & !1 != 0 {
            return Err(malformed(part, "invalid BoundSheet encoding flags"));
        }
        let width = if flags == 0 { 1 } else { 2 };
        let byte_count = characters
            .checked_mul(width)
            .ok_or_else(|| malformed(part, "BoundSheet name size overflowed"))?;
        let bytes = body
            .get(8..8 + byte_count)
            .ok_or_else(|| malformed(part, "truncated BoundSheet name"))?;
        let name = if width == 1 {
            bytes.iter().map(|byte| char::from(*byte)).collect()
        } else {
            let units = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>();
            String::from_utf16(&units)
                .map_err(|_| malformed(part, "BoundSheet name contains invalid UTF-16"))?
        };
        (name, 8 + byte_count)
    };
    if consumed != body.len() || name.contains(['/', '\\', '\0']) {
        return Err(malformed(part, "BoundSheet name contains trailing or unsafe data"));
    }
    Ok(BoundSheet { offset, name })
}

fn is_single_cell_record(kind: u16, biff_version: u16) -> bool {
    matches!(kind, 0x0001..=0x0006)
        || matches!(kind, 0x0201 | 0x0203 | 0x0204 | 0x0205 | 0x027e | 0x00d6 | 0x00fd)
        || (biff_version == BIFF4 && matches!(kind, 0x0401..=0x0406))
}

fn multi_cell_range(
    kind: u16,
    body: &[u8],
    part: &str,
) -> Result<(u32, u16, u16), ConversionError> {
    if body.len() < 6 {
        return Err(malformed(part, "truncated multi-cell record"));
    }
    let row = u32::from(read_u16(body, 0, part)?);
    let first = read_u16(body, 2, part)?;
    let last = read_u16(body, body.len() - 2, part)?;
    if last < first {
        return Err(malformed(part, "reversed multi-cell column range"));
    }
    let entries = usize::from(last - first) + 1;
    let expected = if kind == MUL_RK {
        6_usize.checked_add(
            entries
                .checked_mul(6)
                .ok_or_else(|| malformed(part, "MulRK entry count overflowed"))?,
        )
    } else {
        6_usize.checked_add(
            entries
                .checked_mul(2)
                .ok_or_else(|| malformed(part, "MulBlank entry count overflowed"))?,
        )
    }
    .ok_or_else(|| malformed(part, "multi-cell record size overflowed"))?;
    if body.len() != expected {
        return Err(malformed(part, "multi-cell record length disagrees with its range"));
    }
    Ok((row, first, last))
}

fn include_merged_ranges(
    body: &[u8],
    bounds: &mut SheetBounds,
    part: &str,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let count = usize::from(read_u16(body, 0, part)?);
    let expected = 2_usize
        .checked_add(count.checked_mul(8).ok_or_else(|| malformed(part, "merge count overflowed"))?)
        .ok_or_else(|| malformed(part, "merged-cell record size overflowed"))?;
    if body.len() != expected {
        return Err(malformed(part, "merged-cell record length disagrees with its count"));
    }
    for range in body[2..].chunks_exact(8) {
        budget.work(1, part)?;
        let first_row = u32::from(u16::from_le_bytes([range[0], range[1]]));
        let last_row = u32::from(u16::from_le_bytes([range[2], range[3]]));
        let first_column = u32::from(u16::from_le_bytes([range[4], range[5]]));
        let last_column = u32::from(u16::from_le_bytes([range[6], range[7]]));
        if last_row < first_row || last_column < first_column {
            return Err(malformed(part, "reversed merged-cell range"));
        }
        bounds.include(first_row, first_column);
        bounds.include(last_row, last_column);
    }
    Ok(())
}
