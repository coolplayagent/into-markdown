use crate::workbook::error::malformed;
use crate::workbook::opc::relationships::{decode_attr, is_spreadsheet_namespace};
use crate::workbook::output::data_text;
use crate::workbook::xlsx::sheet_index::{CellToken, CellValueToken};
use calamine::{Data, ExcelDateTime, ExcelDateTimeType};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use std::collections::BTreeMap;
use std::io::BufRead;

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
enum NumberKind {
    DateTime,
    Duration,
}

#[derive(Default)]
pub(super) struct DisplayProfile {
    styles: BTreeMap<u64, NumberKind>,
    is_1904: bool,
}

impl DisplayProfile {
    pub(super) fn with_date_system(mut self, is_1904: bool) -> Self {
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

pub(super) fn read_number_formats<R: BufRead>(
    input: R,
    part: &str,
    _options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<DisplayProfile, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(input);
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut custom = BTreeMap::<u64, NumberKind>::new();
    let mut profile = DisplayProfile::default();
    let mut depth = 0_u16;
    let mut cell_xfs_depth = None;
    let mut style_index = 0_u64;
    loop {
        context.checkpoint()?;
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|error| malformed(Some(part), format!("invalid styles XML: {error}")))?;
        let core = is_spreadsheet_namespace(&namespace);
        match event {
            raw @ (Event::Start(_) | Event::Empty(_)) if core => {
                let empty = matches!(raw, Event::Empty(_));
                let (Event::Start(element) | Event::Empty(element)) = raw else { unreachable!() };
                match element.local_name().as_ref() {
                    b"cellXfs" => cell_xfs_depth = Some(depth),
                    b"numFmt" => {
                        let id = attribute(&element, b"numFmtId", part)?
                            .and_then(|value| value.parse::<u64>().ok());
                        let format_code = attribute(&element, b"formatCode", part)?;
                        if let Some((id, kind)) =
                            id.zip(format_code.as_deref().and_then(detect_number_kind))
                        {
                            custom.insert(id, kind);
                        }
                    }
                    b"xf" if cell_xfs_depth.is_some_and(|value| value + 1 == depth) => {
                        let id = attribute(&element, b"numFmtId", part)?
                            .and_then(|value| value.parse::<u64>().ok())
                            .unwrap_or_default();
                        if let Some(kind) =
                            builtin_number_kind(id).or_else(|| custom.get(&id).copied())
                        {
                            profile.styles.insert(style_index, kind);
                        }
                        style_index = style_index.saturating_add(1);
                    }
                    _ => {}
                }
                if !empty {
                    depth = depth.saturating_add(1);
                }
            }
            Event::End(element) if core => {
                depth = depth.saturating_sub(1);
                if element.local_name().as_ref() == b"cellXfs" {
                    cell_xfs_depth = None;
                }
            }
            Event::DocType(_) => return Err(malformed(Some(part), "DTD is forbidden")),
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(profile)
}

pub(super) fn read_date_system<R: BufRead>(
    input: R,
    part: &str,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(input);
    let mut buffer = Vec::with_capacity(8 * 1024);
    loop {
        context.checkpoint()?;
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|error| malformed(Some(part), format!("invalid workbook XML: {error}")))?;
        if is_spreadsheet_namespace(&namespace)
            && let Event::Start(ref element) | Event::Empty(ref element) = event
            && element.local_name().as_ref() == b"workbookPr"
        {
            return Ok(matches!(
                attribute(element, b"date1904", part)?.as_deref(),
                Some("1" | "true")
            ));
        }
        if matches!(event, Event::DocType(_)) {
            return Err(malformed(Some(part), "DTD is forbidden"));
        }
        if matches!(event, Event::Eof) {
            return Ok(false);
        }
        buffer.clear();
    }
}

fn attribute(
    element: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
    part: &str,
) -> Result<Option<String>, ConversionError> {
    for attribute in element.attributes().with_checks(false) {
        let attribute = attribute
            .map_err(|error| malformed(Some(part), format!("invalid style attribute: {error}")))?;
        if attribute.key.local_name().as_ref() == name {
            return decode_attr(&attribute, part).map(Some);
        }
    }
    Ok(None)
}

fn builtin_number_kind(id: u64) -> Option<NumberKind> {
    match id {
        14..=22 | 45 | 47 => Some(NumberKind::DateTime),
        46 => Some(NumberKind::Duration),
        _ => None,
    }
}

fn detect_number_kind(format: &str) -> Option<NumberKind> {
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
