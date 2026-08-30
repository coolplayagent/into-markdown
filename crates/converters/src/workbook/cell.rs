//! Excel coordinate parsing and formatting shared by both encodings.

use crate::workbook::error::malformed;
use crate::workbook::model::CellCoordinate;
use crate::workbook::schema::{MAX_EXCEL_COLUMNS, MAX_EXCEL_ROWS};
use into_markdown_core::ConversionError;

pub(crate) fn cell_name(row: u32, column: u32) -> String {
    let mut value = column + 1;
    let mut letters = Vec::new();
    while value > 0 {
        let remainder = (value - 1) % 26;
        letters.push(char::from(b'A' + u8::try_from(remainder).unwrap_or(0)));
        value = (value - 1) / 26;
    }
    letters.reverse();
    format!("{}{}", letters.into_iter().collect::<String>(), row + 1)
}

pub(super) fn parse_cell_ref(value: &str) -> Result<CellCoordinate, ConversionError> {
    if value.len() > 16 {
        return Err(malformed(None, "spreadsheet coordinate is too long"));
    }
    let value = value.replace('$', "");
    let split = value
        .find(|character: char| character.is_ascii_digit())
        .ok_or_else(|| malformed(None, format!("invalid spreadsheet coordinate {value}")))?;
    let (letters, digits) = value.split_at(split);
    if letters.is_empty()
        || digits.is_empty()
        || !letters.bytes().all(|byte| byte.is_ascii_alphabetic())
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(malformed(None, format!("invalid spreadsheet coordinate {value}")));
    }
    let mut column = 0_u32;
    for byte in letters.bytes() {
        column = column
            .checked_mul(26)
            .and_then(|value| value.checked_add(u32::from(byte.to_ascii_uppercase() - b'A') + 1))
            .ok_or_else(|| malformed(None, "spreadsheet column overflow"))?;
    }
    let row = digits
        .parse::<u32>()
        .map_err(|_| malformed(None, format!("invalid spreadsheet row {digits}")))?;
    if row == 0 || row > MAX_EXCEL_ROWS || column == 0 || column > MAX_EXCEL_COLUMNS {
        return Err(malformed(None, format!("spreadsheet coordinate out of range {value}")));
    }
    Ok((row - 1, column - 1))
}

pub(super) fn parse_cell_range(
    value: &str,
) -> Result<(CellCoordinate, CellCoordinate), ConversionError> {
    if value.len() > 33 || value.matches(':').count() > 1 {
        return Err(malformed(None, "spreadsheet range is too long"));
    }
    let (start, end) = value.split_once(':').map_or((value, value), |pair| pair);
    let start = parse_cell_ref(start)?;
    let end = parse_cell_ref(end)?;
    if start.0 > end.0 || start.1 > end.1 {
        return Err(malformed(None, format!("invalid spreadsheet range {value}")));
    }
    Ok((start, end))
}

pub(super) fn within(cell: CellCoordinate, start: CellCoordinate, end: CellCoordinate) -> bool {
    cell.0 >= start.0 && cell.0 <= end.0 && cell.1 >= start.1 && cell.1 <= end.1
}
