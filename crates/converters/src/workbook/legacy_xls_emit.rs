use crate::workbook::cell::cell_name;
use crate::workbook::error::{limit, malformed};
use crate::workbook::output::{data_text, provenance};
use crate::workbook::{
    LegacyCellFormat, LegacyFormulaCache, LegacyFormulaExpression, LegacyXlsHints,
};
use calamine::{Data, Dimensions, Range};
use into_markdown_core::{
    Block, BlockNode, ConversionError, ConversionOptions, ExecutionContext, NodeId,
};
use std::collections::BTreeMap;

const PAGED_TSV_ROWS: u32 = 2_048;

pub(super) struct LegacyHintCursor<'a> {
    formula_caches: &'a [LegacyFormulaCache],
    formula_cache_cursor: usize,
    cell_formats: &'a [LegacyCellFormat],
    format_codes: Option<&'a BTreeMap<u16, String>>,
    cell_format_cursor: usize,
    formula_expressions: &'a [LegacyFormulaExpression],
    formula_expression_cursor: usize,
}

impl<'a> LegacyHintCursor<'a> {
    pub(super) fn new(hints: Option<&'a LegacyXlsHints>) -> Self {
        Self {
            formula_caches: hints.map_or(&[], |value| value.formula_caches.as_slice()),
            formula_cache_cursor: 0,
            cell_formats: hints.map_or(&[], |value| value.cell_formats.as_slice()),
            format_codes: hints.map(|value| &value.format_codes),
            cell_format_cursor: 0,
            formula_expressions: hints.map_or(&[], |value| value.formula_expressions.as_slice()),
            formula_expression_cursor: 0,
        }
    }

    pub(super) fn formula_cache_at(
        &mut self,
        sheet: usize,
        row: u32,
        column: u32,
    ) -> Option<&'a str> {
        ordered_hint_at(
            self.formula_caches,
            &mut self.formula_cache_cursor,
            (sheet, row, column),
            |value| (value.sheet_index, value.row, value.column),
        )
        .map(|value| value.value.as_str())
    }

    pub(super) fn cell_format_at(
        &mut self,
        sheet: usize,
        row: u32,
        column: u32,
    ) -> Option<&'a str> {
        ordered_hint_at(
            self.cell_formats,
            &mut self.cell_format_cursor,
            (sheet, row, column),
            |value| (value.sheet_index, value.row, value.column),
        )
        .and_then(|value| {
            self.format_codes.and_then(|codes| display_format_code(value.format_index, codes))
        })
    }

    pub(super) fn formula_hint_at(
        &mut self,
        sheet: usize,
        row: u32,
        column: u32,
    ) -> Option<&'a LegacyFormulaExpression> {
        ordered_hint_at(
            self.formula_expressions,
            &mut self.formula_expression_cursor,
            (sheet, row, column),
            |value| (value.sheet_index, value.row, value.column),
        )
    }
}

fn display_format_code(index: u16, custom: &BTreeMap<u16, String>) -> Option<&str> {
    custom.get(&index).map(String::as_str).or(match index {
        9 => Some("0%"),
        10 => Some("0.00%"),
        _ => None,
    })
}

fn ordered_hint_at<'a, T>(
    values: &'a [T],
    cursor: &mut usize,
    coordinate: (usize, u32, u32),
    coordinate_of: impl Fn(&T) -> (usize, u32, u32),
) -> Option<&'a T> {
    while let Some(value) = values.get(*cursor) {
        if coordinate_of(value) >= coordinate {
            break;
        }
        *cursor += 1;
    }
    values.get(*cursor).filter(|value| coordinate_of(value) == coordinate)
}

pub(super) struct PagedSheet<'a> {
    pub(super) values: &'a Range<Data>,
    pub(super) formulas: &'a Range<String>,
    pub(super) name: &'a str,
    pub(super) index: usize,
    pub(super) last_row: u32,
    pub(super) last_column: u32,
}

pub(super) fn paged_tsv_blocks(
    sheet: &PagedSheet<'_>,
    legacy: &mut LegacyHintCursor<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let page_count = sheet.last_row / PAGED_TSV_ROWS + 1;
    let mut blocks = Vec::new();
    blocks.try_reserve_exact(usize::try_from(page_count).unwrap_or(usize::MAX)).map_err(
        |error| limit("max_memory_bytes", format!("cannot reserve worksheet pages: {error}")),
    )?;
    let mut page = String::new();
    let mut page_index = 0_u32;
    for row in 0..=sheet.last_row {
        context.checkpoint()?;
        append_tsv_row(&mut page, sheet, legacy, row, options)?;
        if (row + 1) % PAGED_TSV_ROWS == 0 || row == sheet.last_row {
            blocks.push(BlockNode {
                id: NodeId(format!("workbook-page-{}-{page_index}", sheet.index)),
                block: Block::Code {
                    language: Some("tsv".into()),
                    text: std::mem::take(&mut page),
                },
                provenance: provenance(sheet.name, Some(row + 1 - (row % PAGED_TSV_ROWS)), None),
            });
            page_index += 1;
        }
    }
    Ok(blocks)
}

fn append_tsv_row(
    page: &mut String,
    sheet: &PagedSheet<'_>,
    legacy: &mut LegacyHintCursor<'_>,
    row: u32,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    for column in 0..=sheet.last_column {
        if column != 0 {
            page.push('\t');
        }
        let value = sheet.values.get_value((row, column)).unwrap_or(&Data::Empty);
        let parsed_cache = data_text(value);
        let formatted_cache = legacy
            .cell_format_at(sheet.index, row, column)
            .and_then(|code| formatted_numeric(value, code));
        let cached = legacy
            .formula_cache_at(sheet.index, row, column)
            .or(formatted_cache.as_deref())
            .unwrap_or(parsed_cache.as_ref());
        let parsed_formula = sheet.formulas.get_value((row, column)).map_or("", String::as_str);
        let formula_hint = legacy.formula_hint_at(sheet.index, row, column);
        let formula = formula_hint.and_then(|hint| hint.value.as_deref()).unwrap_or(parsed_formula);
        let formula_bytes = formula.len().saturating_add(formula_hint.map_or(0, |_| 82));
        if u64::try_from(cached.len().max(formula_bytes)).unwrap_or(u64::MAX)
            > options.limits.max_field_bytes
        {
            return Err(limit(
                "max_field_bytes",
                format!("{}!{} exceeds field limit", sheet.name, cell_name(row, column)),
            ));
        }
        append_tsv_cell(page, formula, formula_hint.map(|hint| &hint.token_sha256), cached)?;
    }
    page.push('\n');
    Ok(())
}

fn append_tsv_cell(
    page: &mut String,
    formula: &str,
    token_sha256: Option<&[u8; 32]>,
    cached: &str,
) -> Result<(), ConversionError> {
    if formula.is_empty() {
        return append_tsv_value(page, cached);
    }
    append_tsv_value(page, "=")?;
    append_tsv_value(page, formula.strip_prefix('=').unwrap_or(formula))?;
    if let Some(digest) = token_sha256 {
        append_tsv_value(page, " [biff-sha256:")?;
        append_digest(page, digest);
        append_tsv_value(page, "]")?;
    }
    if !cached.is_empty() {
        append_tsv_value(page, " [cached: ")?;
        append_tsv_value(page, cached)?;
        append_tsv_value(page, "]")?;
    }
    Ok(())
}

pub(super) fn append_digest(output: &mut String, digest: &[u8; 32]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        output.push(char::from(HEX[usize::from(*byte >> 4)]));
        output.push(char::from(HEX[usize::from(*byte & 0x0f)]));
    }
}

pub(super) fn formatted_numeric(value: &Data, code: &str) -> Option<String> {
    let number = match value {
        Data::Int(value) => value.to_string().parse().ok()?,
        Data::Float(value) => *value,
        _ => return None,
    };
    let section = code.split(';').next().unwrap_or(code);
    let percent = section.contains('%');
    let symbol = ['$', '€', '£', '¥'].into_iter().find(|symbol| section.contains(*symbol));
    if !percent && symbol.is_none() {
        return None;
    }
    let decimal_end = section.find('%').unwrap_or(section.len());
    let decimals = section[..decimal_end].rfind('.').map_or(0, |point| {
        section[point + 1..decimal_end]
            .chars()
            .take_while(|character| matches!(character, '0' | '#'))
            .count()
    });
    let scaled = if percent { number * 100.0 } else { number };
    let negative = scaled.is_sign_negative();
    let rendered = format!("{:.*}", decimals, scaled.abs());
    let (integer, fraction) = rendered.split_once('.').unwrap_or((&rendered, ""));
    let grouped = grouped_integer(integer, section.contains("#,##"));
    let mut number = if fraction.is_empty() { grouped } else { format!("{grouped}.{fraction}") };
    if negative {
        number.insert(0, '-');
    }
    if percent {
        number.push('%');
        return Some(number);
    }
    let symbol = symbol?;
    let first_placeholder = section.find(['0', '#']).unwrap_or(usize::MAX);
    if section.find(symbol).unwrap_or(0) < first_placeholder {
        Some(format!("{symbol}{number}"))
    } else {
        Some(format!("{number} {symbol}"))
    }
}

fn grouped_integer(integer: &str, grouped: bool) -> String {
    if !grouped {
        return integer.to_owned();
    }
    let mut output = String::new();
    for (index, character) in integer.chars().enumerate() {
        if index != 0 && (integer.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn append_tsv_value(output: &mut String, value: &str) -> Result<(), ConversionError> {
    let extra = value
        .len()
        .checked_mul(2)
        .ok_or_else(|| limit("max_memory_bytes", "TSV field size overflow"))?;
    output.try_reserve(extra).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve paged TSV output: {error}"))
    })?;
    for character in value.chars() {
        match character {
            '\t' => output.push_str("\\t"),
            '\r' => output.push_str("\\r"),
            '\n' => output.push_str("\\n"),
            '\\' => output.push_str("\\\\"),
            '`' => output.push_str("\\`"),
            value => output.push(value),
        }
    }
    Ok(())
}

pub(super) fn serialized_merges(
    merges: &[Dimensions],
    last_row: u32,
    last_column: u32,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<String, ConversionError> {
    use std::fmt::Write as _;

    let estimated = merges
        .len()
        .checked_mul(48)
        .ok_or_else(|| limit("max_memory_bytes", "merged-range metadata size overflow"))?;
    if u64::try_from(estimated).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
        return Err(limit("max_field_bytes", "merged-range metadata exceeds field limit"));
    }
    let mut output = String::new();
    output.try_reserve_exact(estimated).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve merged-range metadata: {error}"))
    })?;
    for (index, dimension) in merges.iter().enumerate() {
        context.checkpoint()?;
        if dimension.start.0 > dimension.end.0
            || dimension.start.1 > dimension.end.1
            || dimension.end.0 > last_row
            || dimension.end.1 > last_column
        {
            return Err(malformed(None, "merged range lies outside worksheet bounds"));
        }
        if index != 0 {
            output.push(';');
        }
        write!(
            output,
            "{},{},{},{}",
            dimension.start.0, dimension.start.1, dimension.end.0, dimension.end.1
        )
        .map_err(|_| limit("max_memory_bytes", "cannot render merged-range metadata"))?;
    }
    if u64::try_from(output.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
        return Err(limit("max_field_bytes", "merged-range metadata exceeds field limit"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::append_tsv_value;

    #[test]
    fn paged_tsv_uses_reversible_single_line_escaping_and_bounds_fences() {
        let mut output = String::new();
        append_tsv_value(&mut output, "a\tb\r\nc\\d```").unwrap();
        assert_eq!(output, "a\\tb\\r\\nc\\\\d\\`\\`\\`");
        assert!(!output.contains("```"));
    }
}
