use crate::workbook::error::{limit, malformed};
use crate::workbook::model::WorkbookInventory;
use crate::workbook::opc::relationships::{decode_attr, require_spreadsheet_namespace};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use std::collections::BTreeSet;

#[allow(clippy::too_many_lines)] // Root, declaration, and actual-entry state is intentionally one pass.
pub(in crate::workbook) fn scan_xml_shared_strings(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<WorkbookInventory, ConversionError> {
    let part = "xl/sharedStrings.xml";
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut inventory = WorkbookInventory::default();
    let mut declared_unique = None;
    let mut declared_total = None;
    let mut saw_root = false;
    let mut ended_root = false;
    let mut depth = 0_u16;
    let mut string_depth = None;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                require_spreadsheet_namespace(&namespace, part)?;
                let local = event.local_name();
                match local.as_ref() {
                    b"sst" => {
                        if saw_root || depth != 0 {
                            return Err(malformed(Some(part), "invalid shared-string root"));
                        }
                        saw_root = true;
                        let mut attributes = BTreeSet::new();
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("sst attribute: {error}"))
                            })?;
                            if !attributes.insert(attr.key.as_ref().to_vec()) {
                                return Err(malformed(Some(part), "duplicate sst attribute"));
                            }
                            let target = match attr.key.local_name().as_ref() {
                                b"uniqueCount" => Some(&mut declared_unique),
                                b"count" => Some(&mut declared_total),
                                _ => None,
                            };
                            if let Some(target) = target {
                                let value = decode_attr(&attr, part)?
                                    .parse::<u64>()
                                    .map_err(|_| malformed(Some(part), "invalid sst count"))?;
                                if value > options.limits.max_table_cells {
                                    return Err(limit(
                                        "max_table_cells",
                                        "shared string declaration is too large",
                                    ));
                                }
                                *target = Some(value);
                            }
                        }
                        if is_empty {
                            return Err(malformed(Some(part), "empty shared-string root"));
                        }
                    }
                    b"si" => {
                        if !saw_root
                            || ended_root
                            || depth != 1
                            || string_depth.is_some()
                            || is_empty
                        {
                            return Err(malformed(Some(part), "invalid shared-string item state"));
                        }
                        inventory.shared_strings = inventory.shared_strings.saturating_add(1);
                        string_depth = Some(depth);
                    }
                    _ if !saw_root || ended_root || depth == 0 => {
                        return Err(malformed(Some(part), "invalid shared-string hierarchy"));
                    }
                    _ => {}
                }
                if !is_empty {
                    depth = depth.checked_add(1).ok_or_else(|| {
                        limit("max_nesting_depth", "shared-string depth overflow")
                    })?;
                    if depth > options.limits.max_nesting_depth {
                        return Err(limit("max_nesting_depth", "shared strings are too deep"));
                    }
                }
            }
            Ok((_, Event::Text(text))) if string_depth.is_some() => {
                inventory.shared_string_bytes = inventory
                    .shared_string_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((_, Event::CData(text))) if string_depth.is_some() => {
                inventory.shared_string_bytes = inventory
                    .shared_string_bytes
                    .saturating_add(u64::try_from(text.iter().len()).unwrap_or(u64::MAX));
            }
            Ok((namespace, Event::End(event))) => {
                require_spreadsheet_namespace(&namespace, part)?;
                if depth == 0 {
                    return Err(malformed(Some(part), "unbalanced shared-string element"));
                }
                match event.local_name().as_ref() {
                    b"si" => {
                        if string_depth != Some(depth - 1) {
                            return Err(malformed(Some(part), "invalid shared-string item end"));
                        }
                        string_depth = None;
                    }
                    b"sst" => {
                        if depth != 1 || string_depth.is_some() || ended_root {
                            return Err(malformed(Some(part), "invalid shared-string root end"));
                        }
                        ended_root = true;
                    }
                    _ => {}
                }
                depth -= 1;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => return Err(malformed(Some(part), format!("invalid SST XML: {error}"))),
            _ => {}
        }
        if inventory.shared_strings > options.limits.max_table_cells {
            return Err(limit("max_table_cells", "too many shared strings"));
        }
        if inventory.shared_string_bytes > options.limits.max_decompressed_bytes {
            return Err(limit("max_decompressed_bytes", "shared string text is too large"));
        }
    }
    if !saw_root || !ended_root || depth != 0 || string_depth.is_some() {
        return Err(malformed(Some(part), "incomplete shared-string document"));
    }
    if declared_unique.is_some_and(|value| value != inventory.shared_strings) {
        return Err(malformed(Some(part), "uniqueCount disagrees with shared string entries"));
    }
    if declared_total
        .is_some_and(|total| total < declared_unique.unwrap_or(inventory.shared_strings))
    {
        return Err(malformed(Some(part), "sst count is smaller than uniqueCount"));
    }
    Ok(inventory)
}

#[allow(clippy::too_many_lines)] // Count and actual-entry checks share one parser state.
pub(in crate::workbook) fn scan_xml_style_counts(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<WorkbookInventory, ConversionError> {
    let part = "xl/styles.xml";
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut depth = 0_u16;
    let mut saw_root = false;
    let mut ended_root = false;
    let mut saw_cell_xfs = false;
    let mut saw_num_formats = false;
    let mut saw_fonts = false;
    let mut cell_xfs_depth = None;
    let mut num_formats_depth = None;
    let mut fonts_depth = None;
    let mut declared_xfs = None;
    let mut declared_num_formats = None;
    let mut declared_fonts = None;
    let mut output = WorkbookInventory::default();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                require_spreadsheet_namespace(&namespace, part)?;
                let local = event.local_name();
                if local.as_ref() == b"styleSheet" {
                    if saw_root || depth != 0 || is_empty {
                        return Err(malformed(Some(part), "invalid styleSheet root"));
                    }
                    saw_root = true;
                } else if !saw_root || ended_root || depth == 0 {
                    return Err(malformed(Some(part), "invalid styleSheet hierarchy"));
                } else if local.as_ref() == b"cellXfs" {
                    if depth != 1 || saw_cell_xfs || cell_xfs_depth.is_some() {
                        return Err(malformed(Some(part), "duplicate or nested cellXfs"));
                    }
                    saw_cell_xfs = true;
                    declared_xfs = style_collection_count(&event, part, options)?;
                    if !is_empty {
                        cell_xfs_depth = Some(depth);
                    }
                } else if cell_xfs_depth.is_some() && local.as_ref() == b"xf" {
                    if depth != 2 {
                        return Err(malformed(Some(part), "nested cellXfs entry"));
                    }
                    output.styles = output.styles.saturating_add(1);
                } else if local.as_ref() == b"fonts" {
                    if depth != 1 || saw_fonts || fonts_depth.is_some() {
                        return Err(malformed(Some(part), "duplicate or nested fonts"));
                    }
                    saw_fonts = true;
                    declared_fonts = style_collection_count(&event, part, options)?;
                    if !is_empty {
                        fonts_depth = Some(depth);
                    }
                } else if fonts_depth.is_some() && local.as_ref() == b"font" {
                    if depth != 2 {
                        return Err(malformed(Some(part), "nested font entry"));
                    }
                    output.fonts = output.fonts.saturating_add(1);
                } else if local.as_ref() == b"numFmts" {
                    if depth != 1 || saw_num_formats || num_formats_depth.is_some() {
                        return Err(malformed(Some(part), "duplicate or nested numFmts"));
                    }
                    saw_num_formats = true;
                    declared_num_formats = style_collection_count(&event, part, options)?;
                    if !is_empty {
                        num_formats_depth = Some(depth);
                    }
                } else if num_formats_depth.is_some() && local.as_ref() == b"numFmt" {
                    if depth != 2 {
                        return Err(malformed(Some(part), "nested number-format entry"));
                    }
                    output.number_formats = output.number_formats.saturating_add(1);
                    for attr in event.attributes().with_checks(false) {
                        let attr = attr.map_err(|error| {
                            malformed(Some(part), format!("numFmt attribute: {error}"))
                        })?;
                        if attr.key.local_name().as_ref() == b"formatCode" {
                            let value = decode_attr(&attr, part)?;
                            let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
                            if length > options.limits.max_field_bytes {
                                return Err(limit("max_field_bytes", "number format is too large"));
                            }
                            output.style_format_bytes =
                                output.style_format_bytes.saturating_add(length);
                        }
                    }
                }
                if !is_empty {
                    depth = depth
                        .checked_add(1)
                        .ok_or_else(|| limit("max_nesting_depth", "styleSheet depth overflow"))?;
                    if depth > options.limits.max_nesting_depth {
                        return Err(limit("max_nesting_depth", "styleSheet is too deep"));
                    }
                }
            }
            Ok((namespace, Event::End(event))) => {
                require_spreadsheet_namespace(&namespace, part)?;
                if depth == 0 {
                    return Err(malformed(Some(part), "unbalanced styleSheet element"));
                }
                match event.local_name().as_ref() {
                    b"cellXfs" => {
                        if cell_xfs_depth != Some(depth - 1) {
                            return Err(malformed(Some(part), "invalid cellXfs end"));
                        }
                        cell_xfs_depth = None;
                    }
                    b"numFmts" => {
                        if num_formats_depth != Some(depth - 1) {
                            return Err(malformed(Some(part), "invalid numFmts end"));
                        }
                        num_formats_depth = None;
                    }
                    b"fonts" => {
                        if fonts_depth != Some(depth - 1) {
                            return Err(malformed(Some(part), "invalid fonts end"));
                        }
                        fonts_depth = None;
                    }
                    b"styleSheet" => {
                        if depth != 1
                            || cell_xfs_depth.is_some()
                            || num_formats_depth.is_some()
                            || fonts_depth.is_some()
                            || ended_root
                        {
                            return Err(malformed(Some(part), "invalid styleSheet root end"));
                        }
                        ended_root = true;
                    }
                    _ => {}
                }
                depth -= 1;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid styles XML: {error}")));
            }
            _ => {}
        }
        if output.styles > options.limits.max_table_cells
            || output.fonts > options.limits.max_table_cells
            || output.number_formats > options.limits.max_table_cells
        {
            return Err(limit("max_table_cells", "too many workbook style records"));
        }
        if output.style_format_bytes > options.limits.max_decompressed_bytes {
            return Err(limit("max_decompressed_bytes", "number formats are too large"));
        }
    }
    if !saw_root
        || !ended_root
        || depth != 0
        || cell_xfs_depth.is_some()
        || num_formats_depth.is_some()
        || fonts_depth.is_some()
    {
        return Err(malformed(Some(part), "incomplete styleSheet document"));
    }
    if declared_xfs.is_some_and(|value| value != output.styles)
        || declared_num_formats.is_some_and(|value| value != output.number_formats)
        || declared_fonts.is_some_and(|value| value != output.fonts)
    {
        return Err(malformed(Some(part), "cellXfs count disagrees with style entries"));
    }
    Ok(output)
}

fn style_collection_count(
    event: &quick_xml::events::BytesStart<'_>,
    part: &str,
    options: &ConversionOptions,
) -> Result<Option<u64>, ConversionError> {
    let mut declared = None;
    let mut attributes = BTreeSet::new();
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("style attribute: {error}")))?;
        if !attributes.insert(attr.key.as_ref().to_vec()) {
            return Err(malformed(Some(part), "duplicate style collection attribute"));
        }
        if attr.key.local_name().as_ref() == b"count" {
            let count = decode_attr(&attr, part)?
                .parse::<u64>()
                .map_err(|_| malformed(Some(part), "invalid style collection count"))?;
            if count > options.limits.max_table_cells {
                return Err(limit("max_table_cells", "style declaration is too large"));
            }
            declared = Some(count);
        }
    }
    Ok(declared)
}
