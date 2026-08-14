use crate::workbook::budget::checked_field_bytes;
use crate::workbook::cell::parse_cell_ref;
use crate::workbook::error::{limit, malformed};
use crate::workbook::model::CellCoordinate;
use crate::workbook::opc::relationships::{decode_attr, require_spreadsheet_namespace};
use crate::workbook::schema::{MAX_EXCEL_COLUMNS, MAX_EXCEL_ROWS};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext, InlineMark};
use quick_xml::events::Event;
use std::collections::BTreeMap;

pub(super) fn parse_cell_styles(
    xml: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<Vec<InlineMark>>, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut fonts = Vec::<Vec<InlineMark>>::new();
    let mut current_font = None::<Vec<InlineMark>>;
    let mut in_cell_xfs = false;
    let mut styles = Vec::new();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event))) => {
                require_spreadsheet_namespace(&namespace, part)?;
                match event.local_name().as_ref() {
                    b"font" => current_font = Some(Vec::new()),
                    b"cellXfs" => in_cell_xfs = true,
                    b"xf" if in_cell_xfs => {
                        styles.push(style_marks(&event, &fonts, part)?);
                    }
                    b"b" | b"i" | b"strike" | b"u" if current_font.is_some() => {
                        if let Some(marks) = &mut current_font {
                            push_font_mark(marks, event.local_name());
                        }
                    }
                    _ => {}
                }
            }
            Ok((namespace, Event::Empty(event))) => {
                require_spreadsheet_namespace(&namespace, part)?;
                match event.local_name().as_ref() {
                    b"font" => fonts.push(Vec::new()),
                    b"xf" if in_cell_xfs => styles.push(style_marks(&event, &fonts, part)?),
                    b"b" | b"i" | b"strike" | b"u" if current_font.is_some() => {
                        if let Some(marks) = &mut current_font {
                            push_font_mark(marks, event.local_name());
                        }
                    }
                    _ => {}
                }
            }
            Ok((_, Event::End(event))) => match event.local_name().as_ref() {
                b"font" => {
                    let mut marks = current_font
                        .take()
                        .ok_or_else(|| malformed(Some(part), "font end without start"))?;
                    marks.sort_unstable();
                    marks.dedup();
                    fonts.push(marks);
                }
                b"cellXfs" => in_cell_xfs = false,
                _ => {}
            },
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid styles XML: {error}")));
            }
            _ => {}
        }
        if fonts.len() as u64 > options.limits.max_table_cells
            || styles.len() as u64 > options.limits.max_table_cells
        {
            return Err(limit("max_table_cells", "too many workbook styles"));
        }
    }
    Ok(styles)
}

fn style_marks(
    event: &quick_xml::events::BytesStart<'_>,
    fonts: &[Vec<InlineMark>],
    part: &str,
) -> Result<Vec<InlineMark>, ConversionError> {
    let mut font_id = 0_usize;
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("style attribute: {error}")))?;
        if attr.key.local_name().as_ref() == b"fontId" {
            font_id = decode_attr(&attr, part)?
                .parse::<usize>()
                .map_err(|_| malformed(Some(part), "invalid style font id"))?;
        }
    }
    fonts
        .get(font_id)
        .cloned()
        .ok_or_else(|| malformed(Some(part), format!("style references missing font {font_id}")))
}

fn push_font_mark(marks: &mut Vec<InlineMark>, name: quick_xml::name::LocalName<'_>) {
    let mark = match name.as_ref() {
        b"b" => InlineMark::Bold,
        b"i" => InlineMark::Italic,
        b"strike" => InlineMark::Strikethrough,
        b"u" => InlineMark::Underline,
        _ => return,
    };
    marks.push(mark);
}

type CellMetadata = (BTreeMap<CellCoordinate, Vec<InlineMark>>, Vec<(u32, u32)>, Vec<(u32, u32)>);

pub(super) fn parse_sheet_cell_metadata(
    xml: &[u8],
    part: &str,
    styles: &[Vec<InlineMark>],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<CellMetadata, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut marks = BTreeMap::new();
    let mut hidden_rows = Vec::new();
    let mut hidden_columns = Vec::new();
    let mut hidden_row_field_bytes = 0_u64;
    let mut hidden_column_field_bytes = 0_u64;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event) | Event::Empty(event))) => {
                require_spreadsheet_namespace(&namespace, part)?;
                match event.local_name().as_ref() {
                    b"c" => {
                        let mut reference = None;
                        let mut style_id = None;
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("cell metadata: {error}"))
                            })?;
                            match attr.key.local_name().as_ref() {
                                b"r" => reference = Some(decode_attr(&attr, part)?),
                                b"s" => style_id = Some(decode_attr(&attr, part)?),
                                _ => {}
                            }
                        }
                        if let Some(style_id) = style_id {
                            let coordinate = parse_cell_ref(&reference.ok_or_else(|| {
                                malformed(Some(part), "styled cell reference is missing")
                            })?)?;
                            let style_id = style_id
                                .parse::<usize>()
                                .map_err(|_| malformed(Some(part), "invalid cell style id"))?;
                            let style = styles.get(style_id).ok_or_else(|| {
                                malformed(Some(part), format!("missing cell style {style_id}"))
                            })?;
                            if !style.is_empty() {
                                marks.insert(coordinate, style.clone());
                            }
                        }
                    }
                    b"row" if hidden_attribute(&event, part)? => {
                        let row = required_u32_attribute(&event, b"r", part)?;
                        if row == 0 || row > MAX_EXCEL_ROWS {
                            return Err(malformed(Some(part), "hidden row is out of range"));
                        }
                        push_compact_range(
                            &mut hidden_rows,
                            &mut hidden_row_field_bytes,
                            (row - 1, row - 1),
                            true,
                            part,
                            options,
                        )?;
                    }
                    b"col" if hidden_attribute(&event, part)? => {
                        let min = required_u32_attribute(&event, b"min", part)?;
                        let max = required_u32_attribute(&event, b"max", part)?;
                        if min == 0 || min > max || max > MAX_EXCEL_COLUMNS {
                            return Err(malformed(Some(part), "hidden column range is invalid"));
                        }
                        push_compact_range(
                            &mut hidden_columns,
                            &mut hidden_column_field_bytes,
                            (min - 1, max - 1),
                            false,
                            part,
                            options,
                        )?;
                    }
                    _ => {}
                }
            }
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid worksheet metadata: {error}")));
            }
            _ => {}
        }
        if marks.len() as u64 > options.limits.max_table_cells {
            return Err(limit("max_table_cells", "too many styled cells"));
        }
    }
    Ok((marks, hidden_rows, hidden_columns))
}

fn hidden_attribute(
    event: &quick_xml::events::BytesStart<'_>,
    part: &str,
) -> Result<bool, ConversionError> {
    for attr in event.attributes().with_checks(false) {
        let attr = attr.map_err(|error| malformed(Some(part), format!("hidden flag: {error}")))?;
        if attr.key.local_name().as_ref() == b"hidden" {
            return match decode_attr(&attr, part)?.as_str() {
                "1" | "true" => Ok(true),
                "0" | "false" => Ok(false),
                _ => Err(malformed(Some(part), "invalid hidden flag")),
            };
        }
    }
    Ok(false)
}

fn required_u32_attribute(
    event: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
    part: &str,
) -> Result<u32, ConversionError> {
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("numeric attribute: {error}")))?;
        if attr.key.local_name().as_ref() == name {
            return decode_attr(&attr, part)?
                .parse::<u32>()
                .map_err(|_| malformed(Some(part), "invalid numeric attribute"));
        }
    }
    Err(malformed(Some(part), "required numeric attribute is missing"))
}

pub(in crate::workbook) fn push_compact_range(
    ranges: &mut Vec<(u32, u32)>,
    rendered_bytes: &mut u64,
    range: (u32, u32),
    rows: bool,
    part: &str,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if range.0 > range.1 {
        return Err(malformed(Some(part), "hidden range is reversed"));
    }
    if let Some(last) = ranges.last_mut() {
        if range.0 < last.0 {
            return Err(malformed(Some(part), "hidden ranges are out of order"));
        }
        if range.0 <= last.1.saturating_add(1) {
            let merged = (last.0, last.1.max(range.1));
            let previous_range_bytes = compact_range_bytes(*last, rows)?;
            let merged_range_bytes = compact_range_bytes(merged, rows)?;
            let next_bytes = rendered_bytes
                .checked_sub(previous_range_bytes)
                .and_then(|value| value.checked_add(merged_range_bytes))
                .ok_or_else(|| limit("max_field_bytes", "hidden-range metadata size overflow"))?;
            checked_field_bytes(options, "hidden-range metadata", &[next_bytes])?;
            *last = merged;
            *rendered_bytes = next_bytes;
            return Ok(());
        }
    }
    if u64::try_from(ranges.len()).unwrap_or(u64::MAX) >= options.limits.max_table_cells {
        return Err(limit("max_table_cells", "too many hidden ranges"));
    }
    let range_bytes = compact_range_bytes(range, rows)?;
    let next_bytes = rendered_bytes
        .checked_add(u64::from(!ranges.is_empty()))
        .and_then(|value| value.checked_add(range_bytes))
        .ok_or_else(|| limit("max_field_bytes", "hidden-range metadata size overflow"))?;
    checked_field_bytes(options, "hidden-range metadata", &[next_bytes])?;
    ranges.push(range);
    *rendered_bytes = next_bytes;
    Ok(())
}

fn compact_range_bytes((start, end): (u32, u32), rows: bool) -> Result<u64, ConversionError> {
    let start_bytes =
        if rows { decimal_digits(start.saturating_add(1)) } else { column_name_len(start) };
    if start == end {
        Ok(start_bytes)
    } else {
        let end_bytes =
            if rows { decimal_digits(end.saturating_add(1)) } else { column_name_len(end) };
        start_bytes
            .checked_add(1)
            .and_then(|value| value.checked_add(end_bytes))
            .ok_or_else(|| limit("max_field_bytes", "hidden-range metadata size overflow"))
    }
}

fn decimal_digits(mut value: u32) -> u64 {
    let mut digits = 1_u64;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn column_name_len(mut column: u32) -> u64 {
    let mut length = 0_u64;
    loop {
        length += 1;
        if column < 26 {
            return length;
        }
        column = column / 26 - 1;
    }
}

fn push_column_name(output: &mut String, mut column: u32) {
    let mut reversed = [0_u8; 3];
    let mut length = 0_usize;
    loop {
        reversed[length] = b'A' + u8::try_from(column % 26).unwrap_or(0);
        length += 1;
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    for byte in reversed[..length].iter().rev() {
        output.push(char::from(*byte));
    }
}

pub(in crate::workbook) fn display_ranges(
    ranges: &[(u32, u32)],
    rows: bool,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<String, ConversionError> {
    context.checkpoint()?;
    let mut bytes = 0_u64;
    for (index, (start, end)) in ranges.iter().enumerate() {
        context.checkpoint()?;
        let start_bytes =
            if rows { decimal_digits(start.saturating_add(1)) } else { column_name_len(*start) };
        let end_bytes =
            if rows { decimal_digits(end.saturating_add(1)) } else { column_name_len(*end) };
        bytes = bytes
            .checked_add(u64::from(index != 0))
            .and_then(|value| value.checked_add(start_bytes))
            .and_then(|value| {
                if start == end {
                    Some(value)
                } else {
                    value.checked_add(1)?.checked_add(end_bytes)
                }
            })
            .ok_or_else(|| limit("max_field_bytes", "hidden-range metadata size overflow"))?;
    }
    checked_field_bytes(options, "hidden-range metadata", &[bytes])?;
    let capacity = usize::try_from(bytes)
        .map_err(|_| limit("max_memory_bytes", "hidden-range metadata capacity overflow"))?;
    let mut output = String::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| limit("max_memory_bytes", "hidden-range metadata allocation failed"))?;
    for (index, (start, end)) in ranges.iter().enumerate() {
        context.checkpoint()?;
        if index != 0 {
            output.push(',');
        }
        if rows {
            use std::fmt::Write as _;
            write!(&mut output, "{}", start + 1).map_err(|_| ConversionError::Internal {
                detail: "could not render hidden row".into(),
            })?;
        } else {
            push_column_name(&mut output, *start);
        }
        if start != end {
            output.push(':');
            if rows {
                use std::fmt::Write as _;
                write!(&mut output, "{}", end + 1).map_err(|_| ConversionError::Internal {
                    detail: "could not render hidden row range".into(),
                })?;
            } else {
                push_column_name(&mut output, *end);
            }
        }
    }
    debug_assert_eq!(output.len(), capacity);
    Ok(output)
}
