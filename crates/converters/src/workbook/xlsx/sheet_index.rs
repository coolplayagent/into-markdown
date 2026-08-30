use crate::workbook::cell::{parse_cell_range, parse_cell_ref};
use crate::workbook::error::{limit, malformed};
use crate::workbook::model::CellCoordinate;
use crate::workbook::opc::relationships::{
    decode_attr, is_spreadsheet_namespace, validate_xml_reference,
};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CellValueToken {
    Shared(u64),
    Raw(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CellToken {
    pub(super) coordinate: CellCoordinate,
    pub(super) value: CellValueToken,
    pub(super) formula: String,
    pub(super) cell_type: String,
    pub(super) style_index: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::workbook) struct SheetRun {
    pub(in crate::workbook) row: u32,
    pub(in crate::workbook) first_column: u32,
    pub(in crate::workbook) last_column: u32,
    pub(in crate::workbook) occupied_cells: u64,
}

#[derive(Debug, Clone, Default)]
pub(in crate::workbook) struct SheetLayout {
    pub(in crate::workbook) runs: Vec<SheetRun>,
    pub(in crate::workbook) merges: Vec<(CellCoordinate, CellCoordinate)>,
    pub(in crate::workbook) required_shared: BTreeSet<u64>,
    pub(in crate::workbook) physical_cells: u64,
    pub(in crate::workbook) populated_cells: u64,
    pub(in crate::workbook) formulas: u64,
    pub(in crate::workbook) formula_bytes: u64,
    pub(in crate::workbook) max_formula_bytes: u64,
    pub(in crate::workbook) value_bytes: u64,
    pub(in crate::workbook) max_value_bytes: u64,
    pub(in crate::workbook) max_style_index: Option<u64>,
    pub(in crate::workbook) shared_formula_slots: u64,
    pub(in crate::workbook) bounds: Option<CellCoordinate>,
    pub(in crate::workbook) declared_bounds: Option<CellCoordinate>,
}

pub(in crate::workbook) fn read_layout<R: BufRead>(
    input: R,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<SheetLayout, ConversionError> {
    parse_sheet(input, part, None, options, context)
}

pub(super) fn read_cells_into<R: BufRead>(
    input: R,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
    consumer: &mut dyn FnMut(CellToken) -> Result<(), ConversionError>,
) -> Result<SheetLayout, ConversionError> {
    parse_sheet(input, part, Some(consumer), options, context)
}

#[derive(Default)]
struct CurrentCell {
    coordinate: Option<CellCoordinate>,
    cell_type: String,
    style_index: Option<u64>,
    value: String,
    formula: String,
    in_value: bool,
    in_formula: bool,
    in_inline_text: bool,
    formula_type: String,
    shared_formula_index: Option<u64>,
    formula_reference: Option<(CellCoordinate, CellCoordinate)>,
}

fn parse_sheet<R: BufRead>(
    input: R,
    part: &str,
    mut consumer: Option<&mut dyn FnMut(CellToken) -> Result<(), ConversionError>>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<SheetLayout, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(input);
    reader.config_mut().check_end_names = true;
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut state = SheetState::default();
    let mut opaque_depth = 0_u16;
    loop {
        context.checkpoint()?;
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|error| malformed(Some(part), format!("invalid worksheet XML: {error}")))?;
        let core = is_spreadsheet_namespace(&namespace);
        match event {
            Event::DocType(_) => return Err(malformed(Some(part), "DTD is forbidden")),
            Event::GeneralRef(reference) => validate_xml_reference(reference.as_ref(), part)?,
            Event::Start(_) if opaque_depth > 0 => {
                opaque_depth = opaque_depth
                    .checked_add(1)
                    .ok_or_else(|| limit("max_nesting_depth", "extension depth overflow"))?;
            }
            Event::Start(_) if !core => {
                if options.error_policy != into_markdown_core::ErrorPolicy::BestEffort {
                    return Err(malformed(Some(part), "unsupported worksheet extension namespace"));
                }
                opaque_depth = 1;
            }
            Event::Empty(_) if opaque_depth > 0 || !core => {}
            raw @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(raw, Event::Empty(_));
                let (Event::Start(element) | Event::Empty(element)) = raw else { unreachable!() };
                state.start(&element, empty, &mut consumer, options, part)?;
            }
            Event::Text(text) if opaque_depth == 0 && state.in_cell => {
                let decoded = text.xml_content().map_err(|error| {
                    malformed(Some(part), format!("invalid worksheet text: {error}"))
                })?;
                state.text(&decoded);
            }
            Event::CData(text) if opaque_depth == 0 && state.in_cell => {
                let decoded = text.decode().map_err(|error| {
                    malformed(Some(part), format!("invalid worksheet CDATA: {error}"))
                })?;
                state.text(&decoded);
            }
            Event::End(_) if opaque_depth > 0 => opaque_depth -= 1,
            Event::End(element) if core => {
                state.end(element.local_name().as_ref(), &mut consumer, options, part)?;
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if opaque_depth != 0 || state.in_cell || state.current_row.is_some() {
        return Err(malformed(Some(part), "incomplete worksheet data"));
    }
    state.layout.shared_formula_slots =
        u64::try_from(state.shared_formulas.len()).unwrap_or(u64::MAX);
    if state.layout.physical_cells > options.limits.max_table_cells {
        return Err(limit("max_table_cells", "worksheet has too many physical cells"));
    }
    Ok(state.layout)
}

#[derive(Default)]
struct SheetState {
    layout: SheetLayout,
    current_row: Option<u32>,
    next_row: u32,
    next_column: u32,
    last_coordinate: Option<CellCoordinate>,
    current_cell: CurrentCell,
    shared_formulas: BTreeMap<u64, SharedFormula>,
    in_cell: bool,
}

impl SheetState {
    fn start(
        &mut self,
        element: &quick_xml::events::BytesStart<'_>,
        empty: bool,
        consumer: &mut Option<&mut dyn FnMut(CellToken) -> Result<(), ConversionError>>,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        match element.local_name().as_ref() {
            b"dimension" => self.dimension(element, part),
            b"row" => self.start_row(element, empty, part),
            b"c" => self.start_cell(element, empty, consumer, options, part),
            b"v" if self.in_cell => {
                self.current_cell.in_value = !empty;
                Ok(())
            }
            b"f" if self.in_cell => self.start_formula(element, empty, part),
            b"t" if self.in_cell && self.current_cell.cell_type == "inlineStr" => {
                self.current_cell.in_inline_text = !empty;
                Ok(())
            }
            b"mergeCell" => self.merge(element, part),
            _ => Ok(()),
        }
    }

    fn dimension(
        &mut self,
        element: &quick_xml::events::BytesStart<'_>,
        part: &str,
    ) -> Result<(), ConversionError> {
        if let Some(reference) = attribute(element, b"ref", part)? {
            self.layout.declared_bounds = Some(parse_cell_range(&reference)?.1);
        }
        Ok(())
    }

    fn start_row(
        &mut self,
        element: &quick_xml::events::BytesStart<'_>,
        empty: bool,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.current_row.is_some() || self.in_cell {
            return Err(malformed(Some(part), "nested or duplicate worksheet row"));
        }
        let row = attribute(element, b"r", part)?
            .map(|value| {
                value
                    .parse::<u32>()
                    .ok()
                    .and_then(|value| value.checked_sub(1))
                    .ok_or_else(|| malformed(Some(part), "invalid row index"))
            })
            .transpose()?
            .unwrap_or(self.next_row);
        if row < self.next_row {
            return Err(malformed(Some(part), "worksheet rows are out of order"));
        }
        self.current_row = Some(row);
        self.next_column = 0;
        if empty {
            self.current_row = None;
            self.next_row = row.saturating_add(1);
        }
        Ok(())
    }

    fn start_cell(
        &mut self,
        element: &quick_xml::events::BytesStart<'_>,
        empty: bool,
        consumer: &mut Option<&mut dyn FnMut(CellToken) -> Result<(), ConversionError>>,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        let row = self
            .current_row
            .ok_or_else(|| malformed(Some(part), "cell record lies outside worksheet row"))?;
        if self.in_cell {
            return Err(malformed(Some(part), "nested worksheet cell"));
        }
        let coordinate = attribute(element, b"r", part)?
            .map(|value| parse_cell_ref(&value))
            .transpose()?
            .unwrap_or((row, self.next_column));
        if coordinate.0 != row || self.last_coordinate.is_some_and(|last| coordinate <= last) {
            return Err(malformed(Some(part), "duplicate or out-of-order worksheet cell"));
        }
        self.current_cell = CurrentCell {
            coordinate: Some(coordinate),
            cell_type: attribute(element, b"t", part)?.unwrap_or_default(),
            style_index: parse_u64_attribute(element, b"s", part)?,
            ..CurrentCell::default()
        };
        self.in_cell = true;
        self.next_column = coordinate.1.saturating_add(1);
        self.last_coordinate = Some(coordinate);
        record_coordinate(&mut self.layout, coordinate)?;
        if empty {
            self.finish(consumer, options, part)?;
        }
        Ok(())
    }

    fn start_formula(
        &mut self,
        element: &quick_xml::events::BytesStart<'_>,
        empty: bool,
        part: &str,
    ) -> Result<(), ConversionError> {
        self.current_cell.formula_type = attribute(element, b"t", part)?.unwrap_or_default();
        self.current_cell.shared_formula_index = parse_u64_attribute(element, b"si", part)?;
        self.current_cell.formula_reference =
            attribute(element, b"ref", part)?.map(|value| parse_cell_range(&value)).transpose()?;
        if self.current_cell.formula_type == "dataTable" {
            return Err(ConversionError::Unsupported {
                detail: format!("data-table formulas are unsupported ({part})"),
            });
        }
        self.current_cell.in_formula = !empty;
        Ok(())
    }

    fn merge(
        &mut self,
        element: &quick_xml::events::BytesStart<'_>,
        part: &str,
    ) -> Result<(), ConversionError> {
        let reference = attribute(element, b"ref", part)?
            .ok_or_else(|| malformed(Some(part), "merged range reference is missing"))?;
        let range = parse_cell_range(&reference)?;
        if range.0 != range.1 {
            self.layout.merges.push(range);
        }
        Ok(())
    }

    fn text(&mut self, value: &str) {
        if self.current_cell.in_formula {
            self.current_cell.formula.push_str(value);
        } else if self.current_cell.in_value || self.current_cell.in_inline_text {
            self.current_cell.value.push_str(value);
        }
    }

    fn end(
        &mut self,
        name: &[u8],
        consumer: &mut Option<&mut dyn FnMut(CellToken) -> Result<(), ConversionError>>,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        match name {
            b"v" => self.current_cell.in_value = false,
            b"f" => self.current_cell.in_formula = false,
            b"t" => self.current_cell.in_inline_text = false,
            b"c" => self.finish(consumer, options, part)?,
            b"row" => {
                let row = self
                    .current_row
                    .take()
                    .ok_or_else(|| malformed(Some(part), "row end without start"))?;
                if self.in_cell {
                    return Err(malformed(Some(part), "row closes inside a cell"));
                }
                self.next_row = row.saturating_add(1);
                self.last_coordinate = None;
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(
        &mut self,
        consumer: &mut Option<&mut dyn FnMut(CellToken) -> Result<(), ConversionError>>,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        if !self.in_cell {
            return Err(malformed(Some(part), "cell end without start"));
        }
        finish_cell(
            &mut self.current_cell,
            &mut self.layout,
            consumer,
            &mut self.shared_formulas,
            options,
            part,
        )?;
        self.in_cell = false;
        Ok(())
    }
}

struct SharedFormula {
    start: CellCoordinate,
    anchor: CellCoordinate,
    end: CellCoordinate,
    formula: String,
}

fn parse_u64_attribute(
    element: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
    part: &str,
) -> Result<Option<u64>, ConversionError> {
    attribute(element, name, part)?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| malformed(Some(part), "invalid numeric worksheet attribute"))
        })
        .transpose()
}

fn attribute(
    element: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
    part: &str,
) -> Result<Option<String>, ConversionError> {
    let mut output = None;
    for attribute in element.attributes().with_checks(false) {
        let attribute = attribute
            .map_err(|error| malformed(Some(part), format!("invalid attribute: {error}")))?;
        if attribute.key.local_name().as_ref() == name {
            let decoded = decode_attr(&attribute, part)?;
            if output.replace(decoded).is_some() {
                return Err(malformed(Some(part), "duplicate worksheet attribute"));
            }
        }
    }
    Ok(output)
}

fn record_coordinate(
    layout: &mut SheetLayout,
    coordinate: CellCoordinate,
) -> Result<(), ConversionError> {
    layout.physical_cells = layout
        .physical_cells
        .checked_add(1)
        .ok_or_else(|| limit("max_table_cells", "worksheet cell count overflow"))?;
    layout.bounds =
        Some(layout.bounds.map_or(coordinate, |current| {
            (current.0.max(coordinate.0), current.1.max(coordinate.1))
        }));
    Ok(())
}

fn record_populated_coordinate(layout: &mut SheetLayout, coordinate: CellCoordinate) {
    if let Some(last) = layout.runs.last_mut()
        && last.row == coordinate.0
        && last.last_column.checked_add(1) == Some(coordinate.1)
    {
        last.last_column = coordinate.1;
        last.occupied_cells = last.occupied_cells.saturating_add(1);
    } else {
        layout.runs.push(SheetRun {
            row: coordinate.0,
            first_column: coordinate.1,
            last_column: coordinate.1,
            occupied_cells: 1,
        });
    }
}

fn finish_cell(
    cell: &mut CurrentCell,
    layout: &mut SheetLayout,
    consumer: &mut Option<&mut dyn FnMut(CellToken) -> Result<(), ConversionError>>,
    shared_formulas: &mut BTreeMap<u64, SharedFormula>,
    options: &ConversionOptions,
    part: &str,
) -> Result<(), ConversionError> {
    let coordinate = cell
        .coordinate
        .take()
        .ok_or_else(|| malformed(Some(part), "worksheet cell has no coordinate"))?;
    let field_bytes = u64::try_from(cell.value.len().max(cell.formula.len())).unwrap_or(u64::MAX);
    if field_bytes > options.limits.max_field_bytes {
        return Err(limit("max_field_bytes", "worksheet cell value is too large"));
    }
    let value = if cell.cell_type == "s" {
        if cell.value.trim().is_empty()
            && options.error_policy == into_markdown_core::ErrorPolicy::BestEffort
        {
            CellValueToken::Raw(String::new())
        } else {
            let index = cell
                .value
                .trim()
                .parse::<u64>()
                .map_err(|_| malformed(Some(part), "invalid shared-string index"))?;
            layout.required_shared.insert(index);
            CellValueToken::Shared(index)
        }
    } else {
        CellValueToken::Raw(std::mem::take(&mut cell.value))
    };
    finish_formula(cell, shared_formulas, coordinate, part)?;
    let populated = !cell.formula.is_empty()
        || match &value {
            CellValueToken::Shared(_) => true,
            CellValueToken::Raw(value) => !value.is_empty(),
        };
    if populated {
        layout.populated_cells = layout.populated_cells.saturating_add(1);
        record_populated_coordinate(layout, coordinate);
    }
    if !cell.formula.is_empty() {
        let formula_bytes = u64::try_from(cell.formula.len()).unwrap_or(u64::MAX);
        layout.formulas = layout.formulas.saturating_add(1);
        layout.formula_bytes = layout.formula_bytes.saturating_add(formula_bytes);
        layout.max_formula_bytes = layout.max_formula_bytes.max(formula_bytes);
    }
    let value_bytes = match &value {
        CellValueToken::Shared(_) => 0,
        CellValueToken::Raw(value) => u64::try_from(value.len()).unwrap_or(u64::MAX),
    };
    layout.value_bytes = layout.value_bytes.saturating_add(value_bytes);
    layout.max_value_bytes = layout.max_value_bytes.max(value_bytes);
    if let Some(style_index) = cell.style_index {
        layout.max_style_index =
            Some(layout.max_style_index.map_or(style_index, |current| current.max(style_index)));
    }
    if populated && let Some(consumer) = consumer.as_deref_mut() {
        consumer(CellToken {
            coordinate,
            value,
            formula: std::mem::take(&mut cell.formula),
            cell_type: std::mem::take(&mut cell.cell_type),
            style_index: cell.style_index.take(),
        })?;
    }
    Ok(())
}

fn finish_formula(
    cell: &mut CurrentCell,
    shared_formulas: &mut BTreeMap<u64, SharedFormula>,
    coordinate: CellCoordinate,
    part: &str,
) -> Result<(), ConversionError> {
    if cell.formula_type == "array" {
        let range = cell
            .formula_reference
            .ok_or_else(|| malformed(Some(part), "array formula range is missing"))?;
        if cell.formula.is_empty() || !contains_coordinate(range, coordinate) {
            return Err(malformed(Some(part), "invalid array formula anchor"));
        }
    } else if cell.formula_type == "shared" {
        let index = cell
            .shared_formula_index
            .ok_or_else(|| malformed(Some(part), "shared formula index is missing"))?;
        if cell.formula.is_empty() {
            let shared = shared_formulas
                .get_mut(&index)
                .ok_or_else(|| malformed(Some(part), "shared formula anchor is missing"))?;
            if !contains_coordinate((shared.start, shared.end), coordinate) {
                return Err(malformed(Some(part), "shared formula lies outside its range"));
            }
            cell.formula = crate::workbook::xlsx::formulas::translate_shared_formula(
                &shared.formula,
                shared.anchor,
                coordinate,
            );
        } else {
            let (start, end) = cell
                .formula_reference
                .ok_or_else(|| malformed(Some(part), "shared formula range is missing"))?;
            if !contains_coordinate((start, end), coordinate) {
                return Err(malformed(Some(part), "invalid shared formula anchor"));
            }
            shared_formulas.insert(
                index,
                SharedFormula { start, anchor: coordinate, end, formula: cell.formula.clone() },
            );
        }
    } else if cell.shared_formula_index.is_some() || cell.formula_reference.is_some() {
        return Err(malformed(Some(part), "formula metadata has no supported formula type"));
    }
    Ok(())
}

fn contains_coordinate(
    (start, end): (CellCoordinate, CellCoordinate),
    coordinate: CellCoordinate,
) -> bool {
    start.0 <= coordinate.0
        && coordinate.0 <= end.0
        && start.1 <= coordinate.1
        && coordinate.1 <= end.1
}

#[cfg(test)]
mod tests {
    use super::{CellToken, read_cells_into, read_layout};
    use into_markdown_core::{
        ConversionOptions, ExecutionContext, ExecutionOptions, ResourceLimits,
    };
    use std::io::Cursor;

    #[test]
    fn empty_physical_cells_are_counted_but_not_staged_as_data() {
        let xml = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"/></row><row r="1048576"><c r="XFD1048576"/></row></sheetData></worksheet>"#;
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let layout = read_layout(
            Cursor::new(xml),
            "xl/worksheets/sheet1.xml",
            &ConversionOptions::default(),
            &context,
        )
        .unwrap();
        assert_eq!(layout.physical_cells, 2);
        assert_eq!(layout.populated_cells, 0);
        assert!(layout.runs.is_empty());
        let mut cells = Vec::<CellToken>::new();
        read_cells_into(
            Cursor::new(xml),
            "xl/worksheets/sheet1.xml",
            &ConversionOptions::default(),
            &context,
            &mut |cell| {
                cells.push(cell);
                Ok(())
            },
        )
        .unwrap();
        assert!(cells.is_empty());
    }

    #[test]
    fn best_effort_treats_foreign_extension_subtrees_as_opaque() {
        let xml = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:x="urn:producer" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>safe</t></is></c><x:c r="B1"><x:v>spoof</x:v><c r="C1"><v>also-spoof</v></c></x:c></row></sheetData><mc:AlternateContent><mc:Choice><sheetData><row r="2"><c r="A2"><v>choice-spoof</v></c></row></sheetData></mc:Choice></mc:AlternateContent></worksheet>"#;
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let mut cells = Vec::new();
        let layout = read_cells_into(
            Cursor::new(xml),
            "xl/worksheets/sheet1.xml",
            &ConversionOptions::default(),
            &context,
            &mut |cell| {
                cells.push(cell);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(layout.physical_cells, 1);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].coordinate, (0, 0));

        let strict = ConversionOptions {
            error_policy: into_markdown_core::ErrorPolicy::Strict,
            ..ConversionOptions::default()
        };
        assert!(
            read_layout(Cursor::new(xml), "xl/worksheets/sheet1.xml", &strict, &context,).is_err()
        );
    }
}
