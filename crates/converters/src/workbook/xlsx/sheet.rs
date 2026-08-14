use crate::workbook::cell::{parse_cell_range, parse_cell_ref, within};
use crate::workbook::error::{limit, malformed};
use crate::workbook::model::{CellCoordinate, WorkbookInventory, XlsxSheetScan, max_optional};
use crate::workbook::opc::relationships::{decode_attr, require_spreadsheet_namespace};
use crate::workbook::schema::MAX_EXCEL_ROWS;
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
struct SharedFormulaAnchor {
    start: CellCoordinate,
    end: CellCoordinate,
    derived_cells: u64,
}

#[allow(clippy::too_many_lines)] // One pass authenticates ordering, counts, coordinates, and text.
pub(in crate::workbook) fn scan_xlsx_sheet(
    xml: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<XlsxSheetScan, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut declared = None;
    let mut actual = None;
    let mut inventory = WorkbookInventory::default();
    let mut saw_root = false;
    let mut saw_sheet_data = false;
    let mut ended_sheet_data = false;
    let mut current_row = None;
    let mut next_row = 0_u32;
    let mut next_column = 0_u32;
    let mut last_cell = None;
    let mut in_cell = false;
    let mut current_cell_has_formula = false;
    let mut current_cell_shared = false;
    let mut in_shared_value = false;
    let mut shared_value = 0_u64;
    let mut shared_digits = 0_u8;
    let mut in_formula = false;
    let mut current_formula_is_derived = false;
    let mut current_formula_requires_body = false;
    let mut current_formula_bytes = 0_u64;
    let mut shared_formula_slots = 0_u64;
    let mut shared_formulas = BTreeMap::<u64, SharedFormulaAnchor>::new();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                require_spreadsheet_namespace(&namespace, part)?;
                match event.local_name().as_ref() {
                    b"worksheet" => {
                        if saw_root || is_empty {
                            return Err(malformed(Some(part), "invalid worksheet root"));
                        }
                        saw_root = true;
                    }
                    b"dimension" => {
                        if !saw_root || saw_sheet_data || declared.is_some() || !is_empty {
                            return Err(malformed(Some(part), "invalid worksheet dimension state"));
                        }
                        let mut reference = None;
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("dimension: {error}"))
                            })?;
                            if attr.key.local_name().as_ref() == b"ref"
                                && reference.replace(decode_attr(&attr, part)?).is_some()
                            {
                                return Err(malformed(Some(part), "duplicate dimension reference"));
                            }
                        }
                        let reference = reference
                            .ok_or_else(|| malformed(Some(part), "dimension ref is missing"))?;
                        let (_, end) = parse_cell_range(&reference)?;
                        declared = Some(end);
                    }
                    b"sheetData" => {
                        if !saw_root || saw_sheet_data || ended_sheet_data {
                            return Err(malformed(Some(part), "invalid sheetData state"));
                        }
                        saw_sheet_data = true;
                        if is_empty {
                            ended_sheet_data = true;
                        }
                    }
                    b"row" => {
                        if !saw_sheet_data || ended_sheet_data || current_row.is_some() || in_cell {
                            return Err(malformed(Some(part), "invalid worksheet row state"));
                        }
                        let mut row = None;
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("row attribute: {error}"))
                            })?;
                            if attr.key.local_name().as_ref() == b"r" {
                                let value = decode_attr(&attr, part)?;
                                let parsed = value
                                    .parse::<u32>()
                                    .ok()
                                    .and_then(|value| value.checked_sub(1))
                                    .filter(|value| *value < MAX_EXCEL_ROWS)
                                    .ok_or_else(|| malformed(Some(part), "invalid row index"))?;
                                if row.replace(parsed).is_some() {
                                    return Err(malformed(Some(part), "duplicate row index"));
                                }
                            }
                        }
                        let row = row.unwrap_or(next_row);
                        if row < next_row {
                            return Err(malformed(Some(part), "worksheet rows are out of order"));
                        }
                        current_row = Some(row);
                        next_column = 0;
                        if is_empty {
                            current_row = None;
                            next_row = row.saturating_add(1);
                        }
                    }
                    b"c" => {
                        let row = current_row.ok_or_else(|| {
                            malformed(Some(part), "cell record lies outside worksheet row")
                        })?;
                        if in_cell {
                            return Err(malformed(Some(part), "nested worksheet cell"));
                        }
                        let mut coordinate = None;
                        let mut cell_type = None;
                        let mut style_index = None;
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("cell attribute: {error}"))
                            })?;
                            match attr.key.local_name().as_ref() {
                                b"r" => {
                                    if coordinate
                                        .replace(parse_cell_ref(&decode_attr(&attr, part)?)?)
                                        .is_some()
                                    {
                                        return Err(malformed(
                                            Some(part),
                                            "duplicate cell coordinate",
                                        ));
                                    }
                                }
                                b"t" => cell_type = Some(decode_attr(&attr, part)?),
                                b"s" => {
                                    let value =
                                        decode_attr(&attr, part)?.parse::<u64>().map_err(|_| {
                                            malformed(Some(part), "invalid style index")
                                        })?;
                                    if style_index.replace(value).is_some() {
                                        return Err(malformed(
                                            Some(part),
                                            "duplicate cell style index",
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }
                        let coordinate = coordinate.unwrap_or((row, next_column));
                        if coordinate.0 != row || coordinate.1 < next_column {
                            return Err(malformed(
                                Some(part),
                                "worksheet cells are duplicated or out of order",
                            ));
                        }
                        if last_cell.is_some_and(|previous| coordinate <= previous) {
                            return Err(malformed(
                                Some(part),
                                "worksheet cells are duplicated or out of order",
                            ));
                        }
                        last_cell = Some(coordinate);
                        next_column = coordinate.1.saturating_add(1);
                        actual = Some(actual.map_or(coordinate, |current: CellCoordinate| {
                            (current.0.max(coordinate.0), current.1.max(coordinate.1))
                        }));
                        inventory.cells = inventory.cells.saturating_add(1);
                        inventory.max_style_index =
                            max_optional(inventory.max_style_index, style_index);
                        current_cell_shared = cell_type.as_deref() == Some("s");
                        current_cell_has_formula = false;
                        in_cell = !is_empty;
                    }
                    b"f" => {
                        if !in_cell || in_formula || current_cell_has_formula {
                            return Err(malformed(
                                Some(part),
                                "formula lies outside a cell or is duplicated",
                            ));
                        }
                        let formula_cell = last_cell.ok_or_else(|| {
                            malformed(Some(part), "formula cell coordinate is missing")
                        })?;
                        current_cell_has_formula = true;
                        inventory.formulas = inventory.formulas.saturating_add(1);
                        let mut formula_type = None;
                        let mut shared_index = None;
                        let mut formula_range = None;
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("formula attribute: {error}"))
                            })?;
                            match attr.key.local_name().as_ref() {
                                b"t" => {
                                    if formula_type.replace(decode_attr(&attr, part)?).is_some() {
                                        return Err(malformed(
                                            Some(part),
                                            "duplicate formula type",
                                        ));
                                    }
                                }
                                b"si" => {
                                    let value =
                                        decode_attr(&attr, part)?.parse::<u64>().map_err(|_| {
                                            malformed(Some(part), "invalid shared formula index")
                                        })?;
                                    if shared_index.replace(value).is_some() {
                                        return Err(malformed(
                                            Some(part),
                                            "duplicate shared formula index",
                                        ));
                                    }
                                }
                                b"ref" => {
                                    let value = parse_cell_range(&decode_attr(&attr, part)?)?;
                                    if formula_range.replace(value).is_some() {
                                        return Err(malformed(
                                            Some(part),
                                            "duplicate formula range",
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }
                        match formula_type.as_deref().unwrap_or("normal") {
                            "normal" => {
                                if shared_index.is_some() || formula_range.is_some() {
                                    return Err(malformed(
                                        Some(part),
                                        "normal formula has shared or range metadata",
                                    ));
                                }
                            }
                            "shared" => {
                                let index = shared_index.ok_or_else(|| {
                                    malformed(Some(part), "shared formula index is missing")
                                })?;
                                if index >= options.limits.max_table_cells {
                                    return Err(limit(
                                        "max_table_cells",
                                        "shared formula index exceeds worksheet budget",
                                    ));
                                }
                                shared_formula_slots = shared_formula_slots.max(index + 1);
                                if let Some((start, end)) = formula_range {
                                    if formula_cell != start {
                                        return Err(malformed(
                                            Some(part),
                                            "shared formula anchor is not the range origin",
                                        ));
                                    }
                                    if shared_formulas
                                        .insert(
                                            index,
                                            SharedFormulaAnchor { start, end, derived_cells: 0 },
                                        )
                                        .is_some()
                                    {
                                        return Err(malformed(
                                            Some(part),
                                            "duplicate shared formula anchor",
                                        ));
                                    }
                                    actual = Some(actual.map_or(end, |current: CellCoordinate| {
                                        (current.0.max(end.0), current.1.max(end.1))
                                    }));
                                    if is_empty {
                                        return Err(malformed(
                                            Some(part),
                                            "shared formula anchor has no formula body",
                                        ));
                                    }
                                } else {
                                    let anchor =
                                        shared_formulas.get_mut(&index).ok_or_else(|| {
                                            malformed(
                                                Some(part),
                                                "shared formula derived cell precedes its anchor",
                                            )
                                        })?;
                                    if !within(formula_cell, anchor.start, anchor.end) {
                                        return Err(malformed(
                                            Some(part),
                                            "shared formula derived cell lies outside its range",
                                        ));
                                    }
                                    anchor.derived_cells =
                                        anchor.derived_cells.checked_add(1).ok_or_else(|| {
                                            limit(
                                                "max_table_cells",
                                                "shared formula derived count overflow",
                                            )
                                        })?;
                                }
                            }
                            "array" => {
                                if shared_index.is_some() {
                                    return Err(malformed(
                                        Some(part),
                                        "array formula has a shared formula index",
                                    ));
                                }
                                let (start, end) = formula_range.ok_or_else(|| {
                                    malformed(Some(part), "array formula range is missing")
                                })?;
                                if formula_cell != start || is_empty {
                                    return Err(malformed(
                                        Some(part),
                                        "array formula anchor is invalid",
                                    ));
                                }
                                actual = Some(actual.map_or(end, |current: CellCoordinate| {
                                    (current.0.max(end.0), current.1.max(end.1))
                                }));
                            }
                            "dataTable" => {
                                return Err(ConversionError::Unsupported {
                                    detail: format!(
                                        "worksheet data-table formula is unsupported ({part})"
                                    ),
                                });
                            }
                            value => {
                                return Err(ConversionError::Unsupported {
                                    detail: format!(
                                        "worksheet formula type {value} is unsupported ({part})"
                                    ),
                                });
                            }
                        }
                        current_formula_bytes = 0;
                        current_formula_is_derived =
                            formula_type.as_deref() == Some("shared") && formula_range.is_none();
                        current_formula_requires_body =
                            matches!(formula_type.as_deref(), Some("shared" | "array"))
                                && formula_range.is_some();
                        in_formula = !is_empty;
                    }
                    b"v" => {
                        if !in_cell || in_shared_value {
                            return Err(malformed(Some(part), "invalid cell value state"));
                        }
                        if current_cell_shared {
                            in_shared_value = !is_empty;
                            shared_value = 0;
                            shared_digits = 0;
                        }
                    }
                    b"mergeCell" | b"hyperlink" => {
                        if !is_empty {
                            return Err(malformed(Some(part), "range element must be empty"));
                        }
                        let mut reference = None;
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("range attribute: {error}"))
                            })?;
                            if matches!(attr.key.local_name().as_ref(), b"ref") {
                                reference = Some(decode_attr(&attr, part)?);
                            }
                        }
                        let (_, end) = parse_cell_range(&reference.ok_or_else(|| {
                            malformed(Some(part), "worksheet range reference is missing")
                        })?)?;
                        actual = Some(actual.map_or(end, |current: CellCoordinate| {
                            (current.0.max(end.0), current.1.max(end.1))
                        }));
                        if event.local_name().as_ref() == b"mergeCell" {
                            inventory.merge_ranges = inventory.merge_ranges.saturating_add(1);
                        } else {
                            inventory.hyperlink_ranges =
                                inventory.hyperlink_ranges.saturating_add(1);
                        }
                    }
                    _ => {}
                }
            }
            Ok((_, Event::Text(text))) if in_formula => {
                let length = u64::try_from(text.iter().len()).unwrap_or(u64::MAX);
                if current_formula_is_derived && length != 0 {
                    return Err(malformed(
                        Some(part),
                        "shared formula derived cell contains a formula body",
                    ));
                }
                inventory.formula_bytes = inventory.formula_bytes.saturating_add(length);
                current_formula_bytes = current_formula_bytes.saturating_add(length);
            }
            Ok((_, Event::CData(text))) if in_formula => {
                let length = u64::try_from(text.iter().len()).unwrap_or(u64::MAX);
                if current_formula_is_derived && length != 0 {
                    return Err(malformed(
                        Some(part),
                        "shared formula derived cell contains a formula body",
                    ));
                }
                inventory.formula_bytes = inventory.formula_bytes.saturating_add(length);
                current_formula_bytes = current_formula_bytes.saturating_add(length);
            }
            Ok((_, Event::GeneralRef(reference))) if in_formula => {
                let length = u64::try_from(reference.iter().len()).unwrap_or(u64::MAX);
                if current_formula_is_derived && length != 0 {
                    return Err(malformed(
                        Some(part),
                        "shared formula derived cell contains a formula body",
                    ));
                }
                inventory.formula_bytes = inventory.formula_bytes.saturating_add(length);
                current_formula_bytes = current_formula_bytes.saturating_add(length);
            }
            Ok((_, Event::Text(text))) if in_shared_value => {
                for byte in text.iter() {
                    if !byte.is_ascii_digit() || shared_digits >= 20 {
                        return Err(malformed(Some(part), "invalid shared-string cell index"));
                    }
                    shared_value = shared_value
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(u64::from(*byte - b'0')))
                        .ok_or_else(|| malformed(Some(part), "shared-string index overflow"))?;
                    shared_digits += 1;
                }
                inventory.cell_value_bytes = inventory
                    .cell_value_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((_, Event::Text(text))) if in_cell => {
                inventory.cell_value_bytes = inventory
                    .cell_value_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((_, Event::CData(text))) if in_cell => {
                inventory.cell_value_bytes = inventory
                    .cell_value_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((_, Event::End(event))) if event.local_name().as_ref() == b"f" => {
                if !in_formula
                    || current_formula_requires_body && current_formula_bytes == 0
                    || current_formula_bytes > options.limits.max_field_bytes
                {
                    return Err(if current_formula_bytes > options.limits.max_field_bytes {
                        limit("max_field_bytes", "worksheet formula is too large")
                    } else if current_formula_requires_body && current_formula_bytes == 0 {
                        malformed(Some(part), "formula anchor body is empty")
                    } else {
                        malformed(Some(part), "formula end without start")
                    });
                }
                inventory.max_formula_bytes =
                    inventory.max_formula_bytes.max(current_formula_bytes);
                in_formula = false;
                current_formula_is_derived = false;
                current_formula_requires_body = false;
            }
            Ok((_, Event::End(event))) if event.local_name().as_ref() == b"v" => {
                if in_shared_value {
                    if shared_digits == 0 {
                        return Err(malformed(Some(part), "empty shared-string cell index"));
                    }
                    inventory.max_shared_string_index =
                        max_optional(inventory.max_shared_string_index, Some(shared_value));
                    in_shared_value = false;
                }
            }
            Ok((_, Event::End(event))) if event.local_name().as_ref() == b"c" => {
                if !in_cell || in_formula || in_shared_value {
                    return Err(malformed(Some(part), "invalid cell closing state"));
                }
                in_cell = false;
                current_cell_shared = false;
                current_cell_has_formula = false;
            }
            Ok((_, Event::End(event))) if event.local_name().as_ref() == b"row" => {
                let row = current_row
                    .take()
                    .ok_or_else(|| malformed(Some(part), "row end without start"))?;
                if in_cell {
                    return Err(malformed(Some(part), "row closes inside a cell"));
                }
                next_row = row.saturating_add(1);
            }
            Ok((_, Event::End(event))) if event.local_name().as_ref() == b"sheetData" => {
                if !saw_sheet_data || ended_sheet_data || current_row.is_some() || in_cell {
                    return Err(malformed(Some(part), "invalid sheetData closing state"));
                }
                ended_sheet_data = true;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid worksheet XML: {error}")));
            }
            _ => {}
        }
        if inventory.cells > options.limits.max_table_cells
            || inventory.merge_ranges > options.limits.max_table_cells
            || inventory.hyperlink_ranges > options.limits.max_table_cells
        {
            return Err(limit("max_table_cells", "too many worksheet records"));
        }
        if inventory.formula_bytes.saturating_add(inventory.cell_value_bytes)
            > options.limits.max_decompressed_bytes
        {
            return Err(limit("max_decompressed_bytes", "worksheet text is too large"));
        }
    }
    if !saw_root
        || !saw_sheet_data
        || !ended_sheet_data
        || current_row.is_some()
        || in_cell
        || in_formula
        || in_shared_value
    {
        return Err(malformed(Some(part), "incomplete worksheet state"));
    }
    for anchor in shared_formulas.values() {
        let range_cells = u64::from(anchor.end.0 - anchor.start.0 + 1)
            .checked_mul(u64::from(anchor.end.1 - anchor.start.1 + 1))
            .ok_or_else(|| limit("max_table_cells", "shared formula range area overflow"))?;
        if anchor.derived_cells != range_cells.saturating_sub(1) {
            return Err(malformed(
                Some(part),
                "shared formula range is missing an empty-body derived cell",
            ));
        }
    }
    let bounds = match (declared, actual) {
        (Some(declared), Some(actual)) => {
            if declared.0 < actual.0 || declared.1 < actual.1 {
                return Err(malformed(
                    Some(part),
                    "worksheet dimension under-reports actual cells",
                ));
            }
            Some(actual)
        }
        (Some(_), None) => None,
        (None, actual) => actual,
    };
    inventory.shared_formula_slots = shared_formula_slots;
    Ok((bounds, declared, inventory))
}
