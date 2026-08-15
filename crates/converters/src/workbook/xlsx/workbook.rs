use crate::workbook::error::{limit, malformed};
use crate::workbook::model::WorkbookInventory;
use crate::workbook::opc::relationships::{decode_attr, require_spreadsheet_namespace};
use crate::workbook::schema::{
    OFFICE_REL_NS, OFFICE_REL_STRICT_NS, SPREADSHEET_NS, SPREADSHEET_STRICT_NS,
};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use std::collections::BTreeSet;

pub(in crate::workbook) fn parse_xml_workbook_sheets(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<(String, String)>, ConversionError> {
    let part = "xl/workbook.xml";
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut output = Vec::new();
    let mut names = BTreeSet::new();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event) | Event::Empty(event)))
                if event.local_name().as_ref() == b"sheet" =>
            {
                require_spreadsheet_namespace(&namespace, part)?;
                let mut name = None;
                let mut relationship_id = None;
                for attr in event.attributes().with_checks(false) {
                    let attr = attr.map_err(|error| {
                        malformed(Some(part), format!("invalid sheet attribute: {error}"))
                    })?;
                    if attr.key.local_name().as_ref() == b"name" {
                        name = Some(decode_attr(&attr, part)?);
                    } else if attr.key.local_name().as_ref() == b"id" {
                        match reader.resolve_attribute(attr.key) {
                            (ResolveResult::Bound(namespace), _)
                                if namespace.as_ref() == OFFICE_REL_NS
                                    || namespace.as_ref() == OFFICE_REL_STRICT_NS =>
                            {
                                relationship_id = Some(decode_attr(&attr, part)?);
                            }
                            _ => {}
                        }
                    }
                }
                let name = name.ok_or_else(|| malformed(Some(part), "sheet name is missing"))?;
                let relationship_id = relationship_id
                    .ok_or_else(|| malformed(Some(part), "sheet relationship id is missing"))?;
                if name.is_empty()
                    || u64::try_from(name.len()).unwrap_or(u64::MAX)
                        > options.limits.max_field_bytes
                    || !names.insert(name.clone())
                {
                    return Err(malformed(Some(part), "invalid or duplicate sheet name"));
                }
                output.push((name, relationship_id));
                if u32::try_from(output.len()).unwrap_or(u32::MAX)
                    > options.limits.max_archive_entries
                {
                    return Err(limit("max_archive_entries", "too many workbook sheets"));
                }
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid workbook XML: {error}")));
            }
            _ => {}
        }
    }
    Ok(output)
}

pub(in crate::workbook) fn scan_xml_workbook_surface(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<WorkbookInventory, ConversionError> {
    let part = "xl/workbook.xml";
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut output = WorkbookInventory::default();
    let mut in_defined_name = false;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                match &namespace {
                    ResolveResult::Bound(value)
                        if value.as_ref() == SPREADSHEET_NS
                            || value.as_ref() == SPREADSHEET_STRICT_NS => {}
                    ResolveResult::Bound(_) => continue,
                    _ => require_spreadsheet_namespace(&namespace, part)?,
                }
                match event.local_name().as_ref() {
                    b"sheet" => {
                        output.record_bytes = output.record_bytes.saturating_add(1);
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("sheet attribute: {error}"))
                            })?;
                            if attr.key.local_name().as_ref() == b"name" {
                                let name = decode_attr(&attr, part)?;
                                output.max_formula_reference_bytes = output
                                    .max_formula_reference_bytes
                                    .max(u64::try_from(name.len()).unwrap_or(u64::MAX));
                            }
                        }
                    }
                    b"definedName" => {
                        if in_defined_name {
                            return Err(malformed(Some(part), "nested definedName"));
                        }
                        output.defined_names = output.defined_names.saturating_add(1);
                        in_defined_name = !is_empty;
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("definedName attribute: {error}"))
                            })?;
                            if attr.key.local_name().as_ref() == b"name" {
                                let name = decode_attr(&attr, part)?;
                                let length = u64::try_from(name.len()).unwrap_or(u64::MAX);
                                if length > options.limits.max_field_bytes {
                                    return Err(limit(
                                        "max_field_bytes",
                                        "defined name is too large",
                                    ));
                                }
                                output.defined_name_bytes =
                                    output.defined_name_bytes.saturating_add(length);
                                output.max_formula_reference_bytes =
                                    output.max_formula_reference_bytes.max(length);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok((_, Event::Text(text))) if in_defined_name => {
                output.defined_name_bytes = output
                    .defined_name_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((_, Event::CData(text))) if in_defined_name => {
                output.defined_name_bytes = output
                    .defined_name_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((_, Event::End(event))) if event.local_name().as_ref() == b"definedName" => {
                in_defined_name = false;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid workbook XML: {error}")));
            }
            _ => {}
        }
        if output.defined_names > options.limits.max_table_cells
            || output.record_bytes > u64::from(options.limits.max_archive_entries)
        {
            return Err(limit("max_table_cells", "too many workbook metadata records"));
        }
        if output.defined_name_bytes > options.limits.max_decompressed_bytes {
            return Err(limit("max_decompressed_bytes", "defined-name text is too large"));
        }
    }
    if in_defined_name {
        return Err(malformed(Some(part), "truncated definedName"));
    }
    Ok(output)
}
