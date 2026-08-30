use crate::workbook::budget::{
    enforce_grid, enforce_total_cells, extras_retained_memory, requires_paged_grid,
};
use crate::workbook::calamine_adapter::validate_extras_fields;
use crate::workbook::error::{limit, malformed, warning};
use crate::workbook::extras::{ExtractedSheetExtras, extract_sheet_extras};
use crate::workbook::model::{PackagePreflight, WorkbookKind, max_optional};
use crate::workbook::opc::authority::{
    reject_external_workbook_relationships, root_workbook_authority, validate_xml_part,
    workbook_sheet_parts,
};
use crate::workbook::opc::content_types::{
    parse_content_types, require_content_type, validate_content_type_authority,
};
use crate::workbook::opc::package::{PackageEntry, canonical_part_name, has_extension, read_entry};
use crate::workbook::resource_profile::xlsx_auto_profile;
use crate::workbook::schema::{
    XLSB_SHARED_STRINGS_CT, XLSB_STYLES_CT, XML_SHARED_STRINGS_CT, XML_STYLES_CT,
};
use crate::workbook::xlsb::sheet::scan_xlsb_sheet;
use crate::workbook::xlsb::tables::{scan_binary_shared_strings, scan_binary_style_counts};
use crate::workbook::xlsx::emitter::{NATIVE_TABLE_NODE_CEILING, table_node_upper_bound_for};
use crate::workbook::xlsx::regions::plan_sheet_regions;
use crate::workbook::xlsx::regions::{MergeRange, SparseRegion, paginate_region};
use crate::workbook::xlsx::sheet_index::read_layout;
use crate::workbook::xlsx::tables::{scan_xml_shared_strings, scan_xml_style_counts};
use into_markdown_core::{
    ConversionError, ConversionOptions, ErrorPolicy, ExecutionContext, ResourceReservation,
    SourceLocator,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::Path;

#[allow(clippy::too_many_lines)]
pub(super) fn preflight_package(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
    available_memory: u64,
) -> Result<(PackagePreflight, ResourceReservation), ConversionError> {
    const SCANNER_BOOTSTRAP_BYTES: u64 = 8 * 1024;
    let caller_options = options;
    // Even the allocation-free central-directory walk may need to construct a
    // stable error detail. Authenticate that bounded owner before inspecting
    // attacker bytes, then grow this same permit as successive plans become
    // known instead of replacing it.
    let mut package_permit = context.reserve_memory(SCANNER_BOOTSTRAP_BYTES)?;
    if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
        return Err(ConversionError::Encrypted);
    }
    let input_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if input_size > options.limits.max_input_bytes {
        return Err(limit(
            "max_input_bytes",
            format!("{input_size} > {}", options.limits.max_input_bytes),
        ));
    }
    let validated_entry_count = crate::zip_preflight(bytes)
        .map_err(|detail| malformed(None, format!("ZIP directory preflight: {detail}")))?;
    if u32::try_from(validated_entry_count).unwrap_or(u32::MAX) > options.limits.max_archive_entries
    {
        return Err(limit(
            "max_archive_entries",
            format!("{validated_entry_count} > {}", options.limits.max_archive_entries),
        ));
    }
    let directory_memory = u64::try_from(validated_entry_count)
        .unwrap_or(u64::MAX)
        .checked_mul(1_024)
        .and_then(|value| value.checked_add(input_size))
        .and_then(|value| value.checked_add(8 * 1024 * 1024))
        .ok_or_else(|| limit("max_memory_bytes", "ZIP directory memory model overflow"))?;
    if directory_memory > available_memory {
        return Err(limit(
            "max_memory_bytes",
            format!("ZIP directory preflight requires {directory_memory} > {available_memory}"),
        ));
    }
    package_permit.grow(directory_memory.saturating_sub(SCANNER_BOOTSTRAP_BYTES))?;
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| malformed(None, format!("invalid OPC ZIP: {error}")))?;
    if zip.len() != validated_entry_count {
        return Err(malformed(None, "ZIP entry count changed after directory preflight"));
    }
    let count = u32::try_from(zip.len()).unwrap_or(u32::MAX);
    if count > options.limits.max_archive_entries {
        return Err(limit(
            "max_archive_entries",
            format!("{count} > {}", options.limits.max_archive_entries),
        ));
    }

    let mut exact = BTreeSet::new();
    let mut folded = BTreeSet::new();
    let mut entries = Vec::new();
    let mut entry_map = BTreeMap::new();
    let mut expanded = 0_u64;
    let mut media_bytes = 0_u64;
    let mut macro_present = false;
    let mut diagnostics = Vec::new();
    for index in 0..zip.len() {
        context.checkpoint()?;
        let entry = zip.by_index_raw(index).map_err(|error| {
            malformed(None, format!("cannot inspect ZIP entry {index}: {error}"))
        })?;
        if entry.encrypted() {
            return Err(ConversionError::Encrypted);
        }
        if entry.is_dir() {
            continue;
        }
        let name = canonical_part_name(entry.name())?;
        if !exact.insert(name.clone()) || !folded.insert(name.to_ascii_lowercase()) {
            return Err(malformed(Some(&name), "duplicate or case-colliding OPC part"));
        }
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| limit("max_decompressed_bytes", "expanded size overflow"))?;
        if expanded > options.limits.max_decompressed_bytes {
            return Err(limit(
                "max_decompressed_bytes",
                format!("{expanded} > {}", options.limits.max_decompressed_bytes),
            ));
        }
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("xl/media/") {
            if entry.size() > options.limits.max_asset_bytes {
                return Err(limit(
                    "max_asset_bytes",
                    format!("{}: {} > {}", name, entry.size(), options.limits.max_asset_bytes),
                ));
            }
            media_bytes = media_bytes
                .checked_add(entry.size())
                .ok_or_else(|| limit("max_total_asset_bytes", "asset size overflow"))?;
            if media_bytes > options.limits.max_total_asset_bytes {
                return Err(limit(
                    "max_total_asset_bytes",
                    format!("{media_bytes} > {}", options.limits.max_total_asset_bytes),
                ));
            }
        }
        if lower.ends_with("vbaproject.bin") || lower.contains("/macrosheets/") {
            macro_present = true;
        }
        if lower.starts_with("xl/embeddings/") {
            diagnostics.push(warning(
                "spreadsheet.ole.omitted",
                format!("embedded OLE part {name} was not opened"),
                Some(SourceLocator { part: Some(name.clone()), ..SourceLocator::default() }),
            ));
        }
        let size = entry.size();
        entry_map.insert(name.clone(), PackageEntry { index });
        entries.push((index, name, size));
    }

    let package_materialization = input_size
        .checked_add(
            expanded
                .checked_mul(3)
                .ok_or_else(|| limit("max_memory_bytes", "package memory model overflow"))?,
        )
        .and_then(|value| value.checked_add(directory_memory))
        .ok_or_else(|| limit("max_memory_bytes", "package memory model overflow"))?;
    if package_materialization > available_memory {
        return Err(limit(
            "max_memory_bytes",
            format!("package preflight requires {package_materialization} > {available_memory}"),
        ));
    }
    package_permit.grow(package_materialization.saturating_sub(directory_memory))?;

    let content_index = entries
        .iter()
        .find(|(_, name, _)| name == "[Content_Types].xml")
        .map(|(index, _, _)| *index)
        .ok_or_else(|| malformed(Some("[Content_Types].xml"), "required part is missing"))?;
    let content_types_xml = read_entry(&mut zip, content_index, "[Content_Types].xml")?;
    let content_types = parse_content_types(&content_types_xml, options, context)?;
    let untyped_parts = exact
        .iter()
        .filter(|part| {
            part.as_str() != "[Content_Types].xml" && content_types.for_part(part).is_none()
        })
        .count();
    if untyped_parts > 0 && options.error_policy == ErrorPolicy::BestEffort {
        diagnostics.push(warning(
            "spreadsheet.extension.omitted",
            format!(
                "{untyped_parts} unreferenced part(s) without content type authority were ignored"
            ),
            None,
        ));
    }
    match validate_content_type_authority(&content_types, &exact, options.error_policy) {
        Err(ConversionError::Malformed { part, detail })
            if options.error_policy == ErrorPolicy::BestEffort
                && detail == "orphan content-type Override" =>
        {
            diagnostics.push(warning(
                "spreadsheet.extension.omitted",
                format!(
                    "orphan content-type declaration was ignored ({})",
                    part.as_deref().unwrap_or("unknown part")
                ),
                part.map(|part| SourceLocator { part: Some(part), ..SourceLocator::default() }),
            ));
        }
        result => result?,
    }
    let (kind, content_macro) =
        root_workbook_authority(&mut zip, &entry_map, &content_types, options, context)?;
    macro_present |= content_macro;
    let profiled_options =
        (kind == WorkbookKind::Xml).then(|| xlsx_auto_profile(options, available_memory));
    let options = profiled_options.as_ref().unwrap_or(options);
    if let Some((_, name, size)) =
        entries.iter().find(|(_, _, size)| *size > options.limits.max_archive_entry_bytes)
    {
        return Err(limit(
            "max_archive_entry_bytes",
            format!("{name}: {size} > {}", options.limits.max_archive_entry_bytes),
        ));
    }

    // Every XML part is parsed once under bounded depth before the third-party
    // workbook parser sees it. DTD/entity declarations are always rejected.
    let xml_indices = entries
        .iter()
        .filter(|(_, name, _)| name.to_ascii_lowercase().ends_with(".xml"))
        .map(|(index, name, _)| (*index, name.clone()))
        .collect::<Vec<_>>();
    for (index, name) in xml_indices {
        context.checkpoint()?;
        if options.error_policy == ErrorPolicy::BestEffort
            && name.to_ascii_lowercase().starts_with("xl/worksheets/")
        {
            // The native layout scanner below performs the security and XML
            // checks together with coordinate authentication. Parsing the
            // same worksheet here would make prepare duplicate work.
            continue;
        }
        let xml = read_entry(&mut zip, index, &name)?;
        validate_xml_part(&xml, &name, options, context)?;
    }

    // Reject external-workbook semantics. Ordinary cell hyperlinks are handled
    // as inert Markdown links by the auxiliary extractor.
    for (index, name, _) in &entries {
        if has_extension(name, "rels") {
            let rels = read_entry(&mut zip, *index, name)?;
            match reject_external_workbook_relationships(&rels, name, options, context) {
                Err(ConversionError::Unsupported { detail })
                    if options.error_policy == ErrorPolicy::BestEffort =>
                {
                    diagnostics.push(warning(
                        "spreadsheet.externalRelationship.omitted",
                        detail,
                        Some(SourceLocator {
                            part: Some(name.clone()),
                            ..SourceLocator::default()
                        }),
                    ));
                }
                result => result?,
            }
        }
    }

    let workbook_parts =
        workbook_sheet_parts(&mut zip, kind, &exact, &content_types, options, context)?;
    diagnostics.extend(workbook_parts.diagnostics);
    let sheet_order = workbook_parts.sheet_order;
    let sheet_parts = workbook_parts.sheets;
    let reachable_sheets: BTreeSet<_> = sheet_parts.values().map(String::as_str).collect();
    let mut authenticated_bounds = BTreeMap::<String, (u32, u32)>::new();
    let mut declared_bounds = BTreeMap::<String, (u32, u32)>::new();
    let mut inventory = workbook_parts.inventory;
    let mut xml_cell_counts = BTreeMap::<String, u64>::new();
    let mut xml_layouts = BTreeMap::new();
    for (index, name, _) in &entries {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("xl/worksheets/")
            && matches!(
                Path::new(&lower).extension().and_then(|value| value.to_str()),
                Some("xml" | "bin")
            )
            && !reachable_sheets.contains(name.as_str())
        {
            return Err(malformed(Some(name), "orphan worksheet part is forbidden"));
        }
        let (bounds, declared_bound) = if reachable_sheets.contains(name.as_str())
            && has_extension(&lower, "xml")
        {
            let data = read_entry(&mut zip, *index, name)?;
            let layout = read_layout(Cursor::new(&data), name, options, context)?;
            if layout.physical_cells > 1_000_000
                && layout.populated_cells.saturating_mul(64) < layout.physical_cells
            {
                return Err(limit(
                    "max_table_cells",
                    "worksheet contains an excessive number of empty physical cell records",
                ));
            }
            let bounds = layout.bounds;
            let declared = layout.declared_bounds;
            xml_cell_counts.insert(name.clone(), layout.physical_cells);
            inventory.cells = inventory.cells.saturating_add(layout.physical_cells);
            inventory.formulas = inventory.formulas.saturating_add(layout.formulas);
            inventory.formula_bytes = inventory.formula_bytes.saturating_add(layout.formula_bytes);
            inventory.max_formula_bytes = inventory.max_formula_bytes.max(layout.max_formula_bytes);
            inventory.cell_value_bytes =
                inventory.cell_value_bytes.saturating_add(layout.value_bytes);
            inventory.max_cell_value_bytes =
                inventory.max_cell_value_bytes.max(layout.max_value_bytes);
            inventory.merge_ranges = inventory
                .merge_ranges
                .saturating_add(u64::try_from(layout.merges.len()).unwrap_or(u64::MAX));
            inventory.max_shared_string_index = max_optional(
                inventory.max_shared_string_index,
                layout.required_shared.iter().next_back().copied(),
            );
            inventory.max_style_index =
                max_optional(inventory.max_style_index, layout.max_style_index);
            inventory.shared_formula_slots =
                inventory.shared_formula_slots.saturating_add(layout.shared_formula_slots);
            xml_layouts.insert(name.clone(), layout);
            (bounds, declared)
        } else if reachable_sheets.contains(name.as_str()) && has_extension(&lower, "bin") {
            let data = read_entry(&mut zip, *index, name)?;
            let scan = scan_xlsb_sheet(
                &data,
                name,
                Some(workbook_parts.binary_formula_context),
                options,
                context,
            )?;
            inventory.cells = inventory.cells.saturating_add(scan.cells);
            inventory.formulas = inventory.formulas.saturating_add(scan.formulas);
            inventory.formula_bytes = inventory.formula_bytes.saturating_add(scan.formula_bytes);
            inventory.max_formula_bytes = inventory.max_formula_bytes.max(scan.max_formula_bytes);
            inventory.record_bytes = inventory.record_bytes.saturating_add(scan.record_bytes);
            inventory.xlsb_formula_preallocation_cells =
                inventory.xlsb_formula_preallocation_cells.max(scan.formula_preallocation_cells);
            inventory.cell_value_bytes =
                inventory.cell_value_bytes.saturating_add(scan.cell_value_bytes);
            inventory.max_shared_string_index =
                max_optional(inventory.max_shared_string_index, scan.max_shared_string_index);
            inventory.max_style_index =
                max_optional(inventory.max_style_index, scan.max_style_index);
            (scan.dimensions, scan.declared_dimensions)
        } else {
            (None, None)
        };
        if let Some(declared) = declared_bound {
            declared_bounds.insert(name.clone(), declared);
        }
        if let Some((last_row, last_column)) = bounds {
            let rows = u64::from(last_row) + 1;
            let columns = u64::from(last_column) + 1;
            if kind == WorkbookKind::Binary {
                enforce_grid(rows, columns, options)?;
            }
            authenticated_bounds.insert(name.clone(), (last_row, last_column));
        }
    }
    let xml_layouts = sheet_parts
        .iter()
        .filter_map(|(sheet_name, part)| {
            xml_layouts.remove(part).map(|layout| (sheet_name.clone(), layout))
        })
        .collect::<BTreeMap<_, _>>();
    match kind {
        WorkbookKind::Xml => {
            if exact.contains("xl/styles.xml") {
                require_content_type(&content_types, "xl/styles.xml", &[XML_STYLES_CT])?;
                let entry = entry_map.get("xl/styles.xml").unwrap();
                let data = read_entry(&mut zip, entry.index, "xl/styles.xml")?;
                match scan_xml_style_counts(&data, caller_options, context) {
                    Ok(styles) => {
                        inventory.styles = styles.styles;
                        inventory.fonts = styles.fonts;
                        inventory.number_formats = styles.number_formats;
                        inventory.style_format_bytes = styles.style_format_bytes;
                    }
                    Err(ConversionError::Malformed { detail, .. })
                        if options.error_policy == ErrorPolicy::BestEffort =>
                    {
                        diagnostics.push(warning(
                            "spreadsheet.extension.omitted",
                            format!("non-core style metadata was omitted: {detail}"),
                            Some(SourceLocator {
                                part: Some("xl/styles.xml".into()),
                                ..SourceLocator::default()
                            }),
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
            if exact.contains("xl/sharedStrings.xml") {
                require_content_type(
                    &content_types,
                    "xl/sharedStrings.xml",
                    &[XML_SHARED_STRINGS_CT],
                )?;
                let entry = entry_map.get("xl/sharedStrings.xml").unwrap();
                let data = read_entry(&mut zip, entry.index, "xl/sharedStrings.xml")?;
                match scan_xml_shared_strings(&data, caller_options, context) {
                    Ok(strings) => {
                        inventory.shared_strings = strings.shared_strings;
                        inventory.shared_string_bytes = strings.shared_string_bytes;
                        inventory.max_shared_string_bytes = strings.max_shared_string_bytes;
                    }
                    Err(ConversionError::Malformed { detail, .. })
                        if options.error_policy == ErrorPolicy::BestEffort =>
                    {
                        inventory.shared_strings = inventory
                            .max_shared_string_index
                            .map_or(0, |index| index.saturating_add(1));
                        inventory.shared_string_bytes =
                            entry_size(&entries, "xl/sharedStrings.xml");
                        diagnostics.push(warning(
                            "spreadsheet.extension.omitted",
                            format!("non-core shared-string metadata was normalized: {detail}"),
                            Some(SourceLocator {
                                part: Some("xl/sharedStrings.xml".into()),
                                ..SourceLocator::default()
                            }),
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        WorkbookKind::Binary => {
            if exact.contains("xl/styles.bin") {
                require_content_type(&content_types, "xl/styles.bin", &[XLSB_STYLES_CT])?;
                let entry = entry_map.get("xl/styles.bin").unwrap();
                let data = read_entry(&mut zip, entry.index, "xl/styles.bin")?;
                let styles = scan_binary_style_counts(&data, options, context)?;
                inventory.styles = styles.styles;
                inventory.number_formats = styles.number_formats;
                inventory.style_format_bytes = styles.style_format_bytes;
            }
            if exact.contains("xl/sharedStrings.bin") {
                require_content_type(
                    &content_types,
                    "xl/sharedStrings.bin",
                    &[XLSB_SHARED_STRINGS_CT],
                )?;
                let entry = entry_map.get("xl/sharedStrings.bin").unwrap();
                let data = read_entry(&mut zip, entry.index, "xl/sharedStrings.bin")?;
                let strings = scan_binary_shared_strings(&data, options, context)?;
                inventory.shared_strings = strings.shared_strings;
                inventory.shared_string_bytes = strings.shared_string_bytes;
            }
        }
    }
    if inventory.max_shared_string_index.is_some_and(|index| index >= inventory.shared_strings) {
        return Err(malformed(
            None,
            "worksheet shared-string reference exceeds the authenticated string table",
        ));
    }
    if inventory.max_style_index.is_some_and(|index| index != 0 && index >= inventory.styles) {
        if options.error_policy == ErrorPolicy::BestEffort {
            diagnostics.push(warning(
                "spreadsheet.style.omitted",
                "worksheet references missing optional style records; affected cells use raw display"
                    .into(),
                None,
            ));
        } else {
            return Err(malformed(
                None,
                "worksheet style reference exceeds the authenticated style table",
            ));
        }
    }
    let ExtractedSheetExtras { sheets: mut extras, mut assets, diagnostics: extras_diagnostics } =
        extract_sheet_extras(
            &mut zip,
            kind,
            &sheet_parts,
            &exact,
            &content_types,
            options,
            context,
        )?;
    diagnostics.extend(extras_diagnostics);
    let mut xml_regions = BTreeMap::new();
    let mut empty_cell_slack = inventory.cells.div_ceil(10);
    for sheet_name in &sheet_order {
        if let Some(layout) = xml_layouts.get(sheet_name) {
            let plan = plan_sheet_regions(layout, empty_cell_slack)?;
            empty_cell_slack = empty_cell_slack.saturating_sub(plan.empty_cells_used);
            xml_regions.insert(sheet_name.clone(), plan.regions);
        }
    }
    validate_extras_fields(&extras, options)?;
    let mut cell_capacity = 0_u64;
    let mut sheet_bounds = BTreeMap::new();
    for (sheet_name, sheet_part) in &sheet_parts {
        let mut bounds = authenticated_bounds.get(sheet_part).copied();
        let mut corrected_related_bounds = false;
        if let Some(sheet_extras) = extras.get(sheet_name) {
            for end in sheet_extras
                .hyperlinks
                .iter()
                .map(|value| value.end)
                .chain(sheet_extras.annotations.iter().map(|value| value.cell))
                .chain(sheet_extras.chart_titles.iter().map(|value| value.end))
                .chain(sheet_extras.images.iter().map(|value| value.end))
                .chain(sheet_extras.hidden_rows.iter().map(|value| (value.1, 0)))
                .chain(sheet_extras.hidden_columns.iter().map(|value| (0, value.1)))
            {
                if declared_bounds
                    .get(sheet_part)
                    .is_some_and(|declared| end.0 > declared.0 || end.1 > declared.1)
                {
                    if options.error_policy != ErrorPolicy::BestEffort {
                        return Err(malformed(
                            Some(sheet_part),
                            "worksheet dimension under-reports related sheet metadata",
                        ));
                    }
                    if !corrected_related_bounds {
                        diagnostics.push(warning(
                            "spreadsheet.dimension.corrected",
                            format!(
                                "worksheet {sheet_name} dimension was extended to related metadata"
                            ),
                            Some(SourceLocator {
                                sheet: Some(sheet_name.clone()),
                                part: Some(sheet_part.clone()),
                                ..SourceLocator::default()
                            }),
                        ));
                        corrected_related_bounds = true;
                    }
                }
                bounds = Some(
                    bounds.map_or(end, |current| (current.0.max(end.0), current.1.max(end.1))),
                );
            }
        }
        if let Some((last_row, last_column)) = bounds {
            let rows = u64::from(last_row) + 1;
            let columns = u64::from(last_column) + 1;
            if kind == WorkbookKind::Binary {
                enforce_grid(rows, columns, options)?;
            }
            let planned_cells = if kind == WorkbookKind::Xml {
                xml_cell_counts.get(sheet_part).copied().unwrap_or_default()
            } else {
                rows.checked_mul(columns)
                    .ok_or_else(|| limit("max_table_cells", "worksheet cell count overflow"))?
            };
            cell_capacity = cell_capacity
                .checked_add(planned_cells)
                .ok_or_else(|| limit("max_table_cells", "workbook cell count overflow"))?;
            sheet_bounds.insert(sheet_name.clone(), (last_row, last_column));
        }
    }
    enforce_total_cells(cell_capacity, options)?;
    media_bytes = assets.total_bytes;
    let extras_memory = extras_retained_memory(&extras, &sheet_parts)?;
    let extras_count = extras.values().try_fold(0_u64, |total, sheet| {
        let count = sheet
            .hyperlinks
            .len()
            .saturating_add(sheet.annotations.len())
            .saturating_add(sheet.chart_titles.len())
            .saturating_add(sheet.images.len())
            .saturating_add(sheet.hidden_rows.len())
            .saturating_add(sheet.hidden_columns.len());
        total
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("max_memory_bytes", "worksheet extras count overflow"))
    })?;
    let text_multiplier = if kind == WorkbookKind::Xml { 1 } else { 4 };
    let text_memory = inventory
        .text_bytes()?
        .checked_mul(text_multiplier)
        .ok_or_else(|| limit("max_memory_bytes", "workbook text memory overflow"))?;
    let formula_materialized_memory =
        if kind == WorkbookKind::Xml { 0 } else { inventory.formula_materialized_bytes()? };
    let xlsb_formula_preallocation_memory = inventory
        .xlsb_formula_preallocation_cells
        .checked_mul(
            u64::try_from(std::mem::size_of::<calamine::Cell<String>>()).unwrap_or(u64::MAX),
        )
        .ok_or_else(|| limit("max_memory_bytes", "XLSB formula preallocation overflow"))?;
    let metadata_memory = inventory
        .styles
        .saturating_add(inventory.shared_strings)
        .saturating_add(inventory.formulas)
        .saturating_add(inventory.shared_formula_slots)
        .saturating_add(inventory.defined_names)
        .saturating_add(inventory.external_sheet_slots)
        .saturating_add(inventory.fonts)
        .saturating_add(inventory.merge_ranges.saturating_mul(2))
        .saturating_add(inventory.hyperlink_ranges.saturating_mul(2))
        .saturating_add(inventory.number_formats)
        .checked_mul(128)
        .ok_or_else(|| limit("max_memory_bytes", "workbook metadata memory overflow"))?;
    let paged = kind == WorkbookKind::Xml || requires_paged_grid(cell_capacity, 1);
    // Large sheets use bounded TSV page blocks rather than one paragraph node per
    // cell. Calamine's dense value/formula ranges still coexist with accumulated
    // page text, but validation and provenance scale with pages rather than cells.
    let page_nodes = sheet_bounds
        .values()
        .fold(0_u64, |total, (row, _)| total.saturating_add((u64::from(*row) + 2_048) / 2_048));
    let calamine_range_memory = if kind == WorkbookKind::Xml {
        // SpreadsheetML cells are streamed to the request-scoped staged data
        // plane and never coexist as Calamine value/formula ranges.
        0
    } else {
        cell_capacity
            .checked_mul(if paged { 64 } else { 512 })
            .ok_or_else(|| limit("max_memory_bytes", "Calamine range memory overflow"))?
    };
    let retained_ir_memory = if paged {
        cell_capacity
            .checked_mul(4)
            .and_then(|value| value.checked_add(text_memory))
            .ok_or_else(|| limit("max_memory_bytes", "paged workbook output overflow"))?
    } else {
        cell_capacity
            .checked_mul(2_048)
            .ok_or_else(|| limit("max_memory_bytes", "workbook retained IR overflow"))?
    };
    let (validation_nodes, validation_memory) = if kind == WorkbookKind::Xml {
        let (table_nodes, tsv_nodes) =
            xml_validation_nodes(&sheet_order, &xml_layouts, &xml_regions, &extras)?;
        let nodes = table_nodes
            .checked_add(tsv_nodes)
            .ok_or_else(|| limit("max_memory_bytes", "workbook node count overflow"))?;
        let memory = table_nodes
            .checked_mul(12_288)
            .and_then(|value| value.checked_add(tsv_nodes.saturating_mul(1_024)))
            .and_then(|value| value.checked_add(extras_count.saturating_mul(8_192)))
            .ok_or_else(|| limit("max_memory_bytes", "workbook validation memory overflow"))?;
        (nodes, memory)
    } else if paged {
        (page_nodes, page_nodes.saturating_mul(12_288))
    } else {
        (cell_capacity, cell_capacity.saturating_mul(12_288))
    };
    let extra_validation_memory =
        if kind == WorkbookKind::Xml { 0 } else { extras_count.saturating_mul(8_192) };
    let validation_memory = validation_memory
        .checked_add(extra_validation_memory)
        .ok_or_else(|| limit("max_memory_bytes", "workbook validation memory overflow"))?;
    let provenance_nodes = validation_nodes
        .checked_add(extras_count)
        .and_then(|value| value.checked_add(u64::try_from(sheet_parts.len()).unwrap_or(u64::MAX)))
        .ok_or_else(|| limit("max_memory_bytes", "workbook provenance count overflow"))?;
    let provenance_memory = provenance_nodes
        .checked_mul(1_024)
        .ok_or_else(|| limit("max_memory_bytes", "workbook provenance memory overflow"))?;
    let parser_text_memory = if paged { 0 } else { text_memory };
    let calamine_peak = package_materialization
        .checked_add(calamine_range_memory)
        .and_then(|value| value.checked_add(retained_ir_memory))
        .and_then(|value| value.checked_add(validation_memory))
        .and_then(|value| value.checked_add(provenance_memory))
        .and_then(|value| value.checked_add(parser_text_memory))
        .and_then(|value| value.checked_add(formula_materialized_memory))
        .and_then(|value| value.checked_add(xlsb_formula_preallocation_memory))
        .and_then(|value| value.checked_add(metadata_memory))
        .and_then(|value| value.checked_add(extras_memory))
        .and_then(|value| value.checked_add(media_bytes))
        .and_then(|value| value.checked_add(8 * 1024 * 1024))
        .ok_or_else(|| limit("max_memory_bytes", "workbook peak memory overflow"))?;
    let decode_peak = package_materialization
        .checked_add(assets.decode_working_set_peak)
        .ok_or_else(|| limit("max_memory_bytes", "image decode peak overflow"))?;
    let memory_peak = calamine_peak.max(decode_peak);
    if memory_peak > available_memory {
        return Err(limit("max_memory_bytes", format!("{memory_peak} > {available_memory}")));
    }
    // Codec construction is deliberately last: malformed image bytes are
    // decoded only after the complete retained/transient workbook peak has
    // passed against the engine-authenticated request credit.
    let invalid_assets = assets.validate_images(options, context)?;
    if !invalid_assets.is_empty() {
        for (sheet_name, sheet_extras) in &mut extras {
            let before = sheet_extras.images.len();
            sheet_extras.images.retain(|image| !invalid_assets.contains(&image.asset));
            let omitted = before.saturating_sub(sheet_extras.images.len());
            if omitted > 0 {
                diagnostics.push(warning(
                    "spreadsheet.image.omitted",
                    format!("{omitted} unsupported or invalid image(s) were omitted"),
                    Some(SourceLocator {
                        sheet: Some(sheet_name.clone()),
                        ..SourceLocator::default()
                    }),
                ));
            }
        }
        assets.remove_invalid(&invalid_assets);
        media_bytes = assets.total_bytes;
    }
    // Transfer the concrete package allocation reservation into a single exact
    // workbook-owner reservation before any preflight-owned allocation can
    // outlive its original package model. This same authenticated reservation
    // remains live through Calamine range construction and accumulated IR.
    package_permit.grow(memory_peak.saturating_sub(package_materialization))?;

    let xml_sheets = if kind == WorkbookKind::Xml {
        sheet_order
            .iter()
            .filter_map(|sheet| {
                sheet_parts.get(sheet).map(|part| {
                    (
                        sheet.clone(),
                        (part.clone(), xml_cell_counts.get(part).copied().unwrap_or_default()),
                    )
                })
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    Ok((
        PackagePreflight {
            kind,
            macro_present,
            media_bytes,
            sheet_parts,
            sheet_order,
            xml_sheets,
            xml_layouts,
            xml_regions,
            sheet_bounds,
            extras,
            diagnostics,
            memory_peak,
            inventory,
            assets: assets.assets,
        },
        package_permit,
    ))
}

fn entry_size(entries: &[(usize, String, u64)], target: &str) -> u64 {
    entries.iter().find_map(|(_, name, size)| (name == target).then_some(*size)).unwrap_or_default()
}

fn xml_validation_nodes(
    sheet_order: &[String],
    layouts: &BTreeMap<String, crate::workbook::xlsx::sheet_index::SheetLayout>,
    region_plans: &BTreeMap<String, Vec<SparseRegion>>,
    extras: &BTreeMap<String, crate::workbook::model::SheetExtras>,
) -> Result<(u64, u64), ConversionError> {
    let mut table_total = 0_u64;
    let mut paged_total = 0_u64;
    for sheet_name in sheet_order {
        let layout = layouts
            .get(sheet_name)
            .ok_or_else(|| malformed(None, format!("missing layout for worksheet {sheet_name}")))?;
        let regions = region_plans.get(sheet_name).ok_or_else(|| {
            malformed(None, format!("missing region plan for worksheet {sheet_name}"))
        })?;
        let sheet_extras = extras.get(sheet_name).cloned().unwrap_or_default();
        let extras_nodes = u64::try_from(
            sheet_extras.hyperlinks.len()
                + sheet_extras.annotations.len()
                + sheet_extras.chart_titles.len()
                + sheet_extras.images.len(),
        )
        .unwrap_or(u64::MAX);
        let table_nodes = table_node_upper_bound_for(regions, layout.physical_cells, extras_nodes)?;
        let nodes = if regions.len() == 1 && table_nodes <= NATIVE_TABLE_NODE_CEILING {
            table_nodes
        } else {
            let merges = layout
                .merges
                .iter()
                .map(|(start, end)| MergeRange {
                    first_row: start.0,
                    last_row: end.0,
                    first_column: start.1,
                    last_column: end.1,
                })
                .collect::<Vec<_>>();
            regions.iter().try_fold(1_u64.saturating_add(extras_nodes), |count, region| {
                let pages =
                    u64::try_from(paginate_region(*region, &merges)?.len()).unwrap_or(u64::MAX);
                count
                    .checked_add(pages)
                    .ok_or_else(|| limit("max_memory_bytes", "worksheet page count overflow"))
            })?
        };
        let target = if regions.len() == 1 && table_nodes <= NATIVE_TABLE_NODE_CEILING {
            &mut table_total
        } else {
            &mut paged_total
        };
        *target = target
            .checked_add(nodes)
            .ok_or_else(|| limit("max_memory_bytes", "workbook node count overflow"))?;
    }
    Ok((table_total, paged_total))
}
