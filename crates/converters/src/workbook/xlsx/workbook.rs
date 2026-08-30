use crate::workbook::error::{limit, malformed};
use crate::workbook::model::WorkbookInventory;
use crate::workbook::opc::relationships::{decode_attr, require_spreadsheet_namespace};
use crate::workbook::schema::{
    OFFICE_REL_NS, OFFICE_REL_STRICT_NS, SPREADSHEET_BETA_NS, SPREADSHEET_NS, SPREADSHEET_STRICT_NS,
};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use std::collections::BTreeSet;

pub(in crate::workbook) struct XmlWorkbookSurface {
    pub(in crate::workbook) sheets: Vec<(String, String)>,
    pub(in crate::workbook) inventory: WorkbookInventory,
    pub(in crate::workbook) date_1904: bool,
}

#[derive(Default)]
struct WorkbookScanState {
    inventory: WorkbookInventory,
    sheets: Vec<(String, String)>,
    names: BTreeSet<String>,
    date_1904: bool,
    in_defined_name: bool,
}

pub(in crate::workbook) fn scan_xml_workbook_surface(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<XmlWorkbookSurface, ConversionError> {
    let part = "xl/workbook.xml";
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut state = WorkbookScanState::default();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                match &namespace {
                    ResolveResult::Bound(value)
                        if value.as_ref() == SPREADSHEET_NS
                            || value.as_ref() == SPREADSHEET_STRICT_NS
                            || value.as_ref() == SPREADSHEET_BETA_NS => {}
                    ResolveResult::Bound(_) => continue,
                    _ => require_spreadsheet_namespace(&namespace, part)?,
                }
                handle_workbook_start(&event, is_empty, &reader, &mut state, options, part)?;
            }
            Ok((_, Event::Text(text))) if state.in_defined_name => {
                state.inventory.defined_name_bytes = state
                    .inventory
                    .defined_name_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((_, Event::CData(text))) if state.in_defined_name => {
                state.inventory.defined_name_bytes = state
                    .inventory
                    .defined_name_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((_, Event::End(event))) if event.local_name().as_ref() == b"definedName" => {
                state.in_defined_name = false;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid workbook XML: {error}")));
            }
            _ => {}
        }
        if state.inventory.defined_names > options.limits.max_table_cells
            || state.inventory.record_bytes > u64::from(options.limits.max_archive_entries)
        {
            return Err(limit("max_table_cells", "too many workbook metadata records"));
        }
        if state.inventory.defined_name_bytes > options.limits.max_decompressed_bytes {
            return Err(limit("max_decompressed_bytes", "defined-name text is too large"));
        }
    }
    if state.in_defined_name {
        return Err(malformed(Some(part), "truncated definedName"));
    }
    Ok(XmlWorkbookSurface {
        sheets: state.sheets,
        inventory: state.inventory,
        date_1904: state.date_1904,
    })
}

fn handle_workbook_start(
    event: &quick_xml::events::BytesStart<'_>,
    is_empty: bool,
    reader: &quick_xml::reader::NsReader<&[u8]>,
    state: &mut WorkbookScanState,
    options: &ConversionOptions,
    part: &str,
) -> Result<(), ConversionError> {
    match event.local_name().as_ref() {
        b"sheet" => record_sheet(event, reader, state, options, part),
        b"workbookPr" => record_workbook_properties(event, state, part),
        b"definedName" => record_defined_name(event, is_empty, state, options, part),
        _ => Ok(()),
    }
}

fn record_sheet(
    event: &quick_xml::events::BytesStart<'_>,
    reader: &quick_xml::reader::NsReader<&[u8]>,
    state: &mut WorkbookScanState,
    options: &ConversionOptions,
    part: &str,
) -> Result<(), ConversionError> {
    state.inventory.record_bytes = state.inventory.record_bytes.saturating_add(1);
    let mut name = None;
    let mut relationship_id = None;
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("sheet attribute: {error}")))?;
        if attr.key.local_name().as_ref() == b"name" {
            name = Some(decode_attr(&attr, part)?);
        } else if attr.key.local_name().as_ref() == b"id"
            && matches!(
                reader.resolve_attribute(attr.key),
                (ResolveResult::Bound(namespace), _)
                    if namespace.as_ref() == OFFICE_REL_NS
                        || namespace.as_ref() == OFFICE_REL_STRICT_NS
            )
        {
            relationship_id = Some(decode_attr(&attr, part)?);
        }
    }
    let name = name.ok_or_else(|| malformed(Some(part), "sheet name is missing"))?;
    let relationship_id =
        relationship_id.ok_or_else(|| malformed(Some(part), "sheet relationship id is missing"))?;
    if name.is_empty()
        || u64::try_from(name.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes
        || !state.names.insert(name.clone())
    {
        return Err(malformed(Some(part), "invalid or duplicate sheet name"));
    }
    state.inventory.max_formula_reference_bytes = state
        .inventory
        .max_formula_reference_bytes
        .max(u64::try_from(name.len()).unwrap_or(u64::MAX));
    state.sheets.push((name, relationship_id));
    if u32::try_from(state.sheets.len()).unwrap_or(u32::MAX) > options.limits.max_archive_entries {
        return Err(limit("max_archive_entries", "too many workbook sheets"));
    }
    Ok(())
}

fn record_workbook_properties(
    event: &quick_xml::events::BytesStart<'_>,
    state: &mut WorkbookScanState,
    part: &str,
) -> Result<(), ConversionError> {
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("workbookPr attribute: {error}")))?;
        if attr.key.local_name().as_ref() == b"date1904" {
            state.date_1904 = matches!(decode_attr(&attr, part)?.as_str(), "1" | "true");
        }
    }
    Ok(())
}

fn record_defined_name(
    event: &quick_xml::events::BytesStart<'_>,
    is_empty: bool,
    state: &mut WorkbookScanState,
    options: &ConversionOptions,
    part: &str,
) -> Result<(), ConversionError> {
    if state.in_defined_name {
        return Err(malformed(Some(part), "nested definedName"));
    }
    state.inventory.defined_names = state.inventory.defined_names.saturating_add(1);
    state.in_defined_name = !is_empty;
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("definedName attribute: {error}")))?;
        if attr.key.local_name().as_ref() != b"name" {
            continue;
        }
        let length = u64::try_from(decode_attr(&attr, part)?.len()).unwrap_or(u64::MAX);
        if length > options.limits.max_field_bytes {
            return Err(limit("max_field_bytes", "defined name is too large"));
        }
        state.inventory.defined_name_bytes =
            state.inventory.defined_name_bytes.saturating_add(length);
        state.inventory.max_formula_reference_bytes =
            state.inventory.max_formula_reference_bytes.max(length);
    }
    Ok(())
}
