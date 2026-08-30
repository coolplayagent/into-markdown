use crate::workbook::error::malformed;
use crate::workbook::output::data_text;
use crate::workbook::xlsx::sheet_index::{CellToken, CellValueToken};
use calamine::{Data, ExcelDateTime, ExcelDateTimeType};
use into_markdown_core::ConversionError;
use std::collections::BTreeMap;

pub(super) fn translate_shared_formula(
    formula: &str,
    anchor: (u32, u32),
    target: (u32, u32),
) -> String {
    let row_delta = i64::from(target.0) - i64::from(anchor.0);
    let column_delta = i64::from(target.1) - i64::from(anchor.1);
    let bytes = formula.as_bytes();
    let mut output = String::with_capacity(formula.len());
    let mut index = 0;
    while index < bytes.len() {
        if matches!(bytes[index], b'"' | b'\'') {
            let end = quoted_token_end(bytes, index, bytes[index]);
            output.push_str(&formula[index..end]);
            index = end;
            continue;
        }
        if let Some((end, translated)) =
            translated_reference_at(formula, index, row_delta, column_delta)
        {
            output.push_str(&translated);
            index = end;
            continue;
        }
        let character = formula[index..].chars().next().expect("formula index is a UTF-8 boundary");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

fn quoted_token_end(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] != quote {
            index += 1;
        } else if bytes.get(index + 1) == Some(&quote) {
            index += 2;
        } else {
            return index + 1;
        }
    }
    bytes.len()
}

fn translated_reference_at(
    formula: &str,
    start: usize,
    row_delta: i64,
    column_delta: i64,
) -> Option<(usize, String)> {
    let bytes = formula.as_bytes();
    let boundary_before = start == 0 || !is_formula_identifier_byte(bytes[start - 1]);
    if !boundary_before {
        return None;
    }
    let column_absolute = bytes.get(start) == Some(&b'$');
    let mut index = start + usize::from(column_absolute);
    let letters_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
        index += 1;
    }
    let letters_end = index;
    if letters_end == letters_start || letters_end - letters_start > 3 {
        return None;
    }
    let row_absolute = bytes.get(index) == Some(&b'$');
    index += usize::from(row_absolute);
    let digits_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let next = bytes.get(index).copied();
    let boundary_after = next.is_none_or(|byte| !is_formula_identifier_byte(byte));
    if index == digits_start || !boundary_after || matches!(next, Some(b'(' | b'!')) {
        return None;
    }
    let column = parse_column(&bytes[letters_start..letters_end])?;
    let row = formula[digits_start..index].parse::<i64>().ok()?.checked_sub(1)?;
    let translated_row = if row_absolute { row } else { row.checked_add(row_delta)? };
    let translated_column = if column_absolute {
        i64::from(column)
    } else {
        i64::from(column).checked_add(column_delta)?
    };
    if !(0..1_048_576).contains(&translated_row) || !(0..16_384).contains(&translated_column) {
        return None;
    }
    let mut translated = String::new();
    if column_absolute {
        translated.push('$');
    }
    translated.push_str(&column_name(u32::try_from(translated_column).ok()?));
    if row_absolute {
        translated.push('$');
    }
    translated.push_str(&(translated_row + 1).to_string());
    Some((index, translated))
}

fn is_formula_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

fn parse_column(bytes: &[u8]) -> Option<u32> {
    let mut value = 0_u32;
    for byte in bytes {
        value =
            value.checked_mul(26)?.checked_add(u32::from(byte.to_ascii_uppercase() - b'A' + 1))?;
    }
    value.checked_sub(1).filter(|value| *value < 16_384)
}

fn column_name(mut column: u32) -> String {
    let mut reversed = [0_u8; 3];
    let mut length = 0;
    loop {
        reversed[length] = b'A' + u8::try_from(column % 26).unwrap_or_default();
        length += 1;
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    reversed[..length].iter().rev().map(|byte| char::from(*byte)).collect()
}

#[derive(Debug, Clone, Copy)]
pub(in crate::workbook) enum NumberKind {
    DateTime,
    Duration,
}

#[derive(Debug, Default)]
pub(in crate::workbook) struct DisplayProfile {
    pub(in crate::workbook) styles: BTreeMap<u64, NumberKind>,
    pub(in crate::workbook) is_1904: bool,
}

impl DisplayProfile {
    pub(in crate::workbook) fn with_date_system(mut self, is_1904: bool) -> Self {
        self.is_1904 = is_1904;
        self
    }

    pub(super) fn display(
        &self,
        cell: &CellToken,
        shared: &BTreeMap<u64, String>,
    ) -> Result<String, ConversionError> {
        let raw = match &cell.value {
            CellValueToken::Shared(index) => shared.get(index).cloned().ok_or_else(|| {
                malformed(Some("xl/sharedStrings.xml"), "shared-string value is unavailable")
            })?,
            CellValueToken::Raw(value) => value.clone(),
        };
        if cell.cell_type == "b" {
            return Ok(match raw.trim() {
                "0" => "false".into(),
                "1" => "true".into(),
                _ => raw,
            });
        }
        let Some(kind) = cell.style_index.and_then(|index| self.styles.get(&index).copied()) else {
            // Ordinary numeric values deliberately keep the source XML lexeme.
            // Parsing through f64 here would lose integers above 2^53 and
            // significant decimal/exponent digits.
            return Ok(raw);
        };
        let Ok(serial) = raw.trim().parse::<f64>() else { return Ok(raw) };
        let kind = match kind {
            NumberKind::DateTime => ExcelDateTimeType::DateTime,
            NumberKind::Duration => ExcelDateTimeType::TimeDelta,
        };
        Ok(data_text(&Data::DateTime(ExcelDateTime::new(serial, kind, self.is_1904))).into_owned())
    }
}

pub(in crate::workbook) fn builtin_number_kind(id: u64) -> Option<NumberKind> {
    match id {
        14..=22 | 45 | 47 => Some(NumberKind::DateTime),
        46 => Some(NumberKind::Duration),
        _ => None,
    }
}

pub(in crate::workbook) fn detect_number_kind(format: &str) -> Option<NumberKind> {
    let lower = format.to_ascii_lowercase();
    if lower.contains("[h]") || lower.contains("[m]") || lower.contains("[s]") {
        return Some(NumberKind::Duration);
    }
    let unquoted = lower
        .split('"')
        .enumerate()
        .filter(|(index, _)| index.is_multiple_of(2))
        .map(|(_, value)| value)
        .collect::<String>();
    if !unquoted.contains(';')
        && unquoted.chars().any(|value| matches!(value, 'd' | 'm' | 'y' | 'h' | 's'))
    {
        Some(NumberKind::DateTime)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{DisplayProfile, translate_shared_formula};
    use crate::workbook::xlsx::sheet_index::{CellToken, CellValueToken};
    use std::collections::BTreeMap;

    #[test]
    fn ordinary_numeric_lexemes_are_byte_exact() {
        let profile = DisplayProfile::default();
        for raw in ["9007199254740993", "0.12345678901234567", "1.2345678901234567E-20"] {
            let cell = CellToken {
                coordinate: (0, 0),
                value: CellValueToken::Raw(raw.into()),
                formula: String::new(),
                cell_type: "n".into(),
                style_index: None,
            };
            assert_eq!(profile.display(&cell, &BTreeMap::new()).unwrap(), raw);
        }
    }

    #[test]
    fn shared_formula_translation_skips_functions_literals_and_sheet_names() {
        assert_eq!(translate_shared_formula("LOG10(A1)", (0, 0), (1, 0)), "LOG10(A2)");
        assert_eq!(
            translate_shared_formula(r#"IF(A1="A1",A1,"A1""B2")"#, (0, 0), (1, 0)),
            r#"IF(A2="A1",A2,"A1""B2")"#
        );
        assert_eq!(
            translate_shared_formula("'$A1 Data'!$A1+Sheet1!B$2+$C$3+A1:B2+A1!B2", (0, 0), (1, 2),),
            "'$A1 Data'!$A2+Sheet1!D$2+$C$3+C2:D3+A1!D3"
        );
        assert_eq!(
            translate_shared_formula("SUM(数据!A1,数据A1,A1)", (0, 0), (1, 0)),
            "SUM(数据!A2,数据A1,A2)"
        );
    }
}
