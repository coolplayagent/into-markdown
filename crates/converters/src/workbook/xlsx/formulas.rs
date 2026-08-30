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
    if !formula.is_ascii() {
        return formula.to_owned();
    }
    let row_delta = i64::from(target.0) - i64::from(anchor.0);
    let column_delta = i64::from(target.1) - i64::from(anchor.1);
    let bytes = formula.as_bytes();
    let mut output = String::with_capacity(formula.len());
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let column_absolute = bytes.get(index) == Some(&b'$');
        if column_absolute {
            index += 1;
        }
        let letters_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
            index += 1;
        }
        let letters_end = index;
        let row_absolute = bytes.get(index) == Some(&b'$');
        if row_absolute {
            index += 1;
        }
        let digits_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let boundary_before =
            start == 0 || !bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_';
        let boundary_after =
            index == bytes.len() || !bytes[index].is_ascii_alphanumeric() && bytes[index] != b'_';
        let candidate = letters_end > letters_start
            && letters_end - letters_start <= 3
            && index > digits_start
            && boundary_before
            && boundary_after;
        if candidate
            && let Some(column) = parse_column(&bytes[letters_start..letters_end])
            && let Ok(row_one_based) = formula[digits_start..index].parse::<i64>()
        {
            let row = row_one_based - 1;
            let translated_row = if row_absolute { row } else { row + row_delta };
            let translated_column =
                if column_absolute { i64::from(column) } else { i64::from(column) + column_delta };
            if (0..1_048_576).contains(&translated_row) && (0..16_384).contains(&translated_column)
            {
                if column_absolute {
                    output.push('$');
                }
                output.push_str(&column_name(u32::try_from(translated_column).unwrap_or_default()));
                if row_absolute {
                    output.push('$');
                }
                output.push_str(&(translated_row + 1).to_string());
                continue;
            }
        }
        output.push_str(&formula[start..index.max(start + 1)]);
        if index == start {
            index += 1;
        }
    }
    output
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
    use super::DisplayProfile;
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
}
