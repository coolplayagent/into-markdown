use crate::workbook::error::{limit, malformed, warning};
use crate::workbook::model::PackagePreflight;
use crate::workbook::xlsx::emitter::{PreparedSheet, emit};
use crate::workbook::xlsx::formulas::{DisplayProfile, read_date_system, read_number_formats};
use crate::workbook::xlsx::regions::{MergeRange, SparseRegion};
use crate::workbook::xlsx::shared_strings::read_selected;
use crate::workbook::xlsx::sheet_index::{read_cells_into, read_layout};
use crate::workbook::xlsx::staging::{StagingTelemetry, StagingWriter};
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, ExecutionContext, SourceLocator,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Cursor};

pub(in crate::workbook) fn convert_xlsx(
    bytes: &[u8],
    preflight: &PackagePreflight,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| malformed(None, format!("cannot reopen authenticated XLSX: {error}")))?;
    let (layouts, required_shared) = prepare_layouts(&mut archive, preflight, options, context)?;
    let shared_strings = read_shared_strings(&mut archive, &required_shared, options, context)?;
    let display = read_display_profile(&mut archive, options, context)?;
    let (prepared, telemetry, diagnostics) =
        stage_sheets(&mut archive, layouts, preflight, options, context)?;
    let (mut document, diagnostics) =
        emit(prepared, &display, &shared_strings, diagnostics, options, context)?;
    attach_telemetry(
        &mut document,
        telemetry,
        preflight.sheet_order.len(),
        !required_shared.is_empty(),
    );
    if telemetry.staged_bytes > options.limits.max_temporary_bytes {
        return Err(limit(
            "max_temporary_bytes",
            "native worksheet staging exceeded the authenticated temporary budget",
        ));
    }
    Ok(ConverterOutput::new(document, Vec::new(), diagnostics))
}

struct LayoutPlan {
    name: String,
    part: String,
    layout: crate::workbook::xlsx::sheet_index::SheetLayout,
    regions: Vec<SparseRegion>,
    merges: Vec<MergeRange>,
}

fn prepare_layouts(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    preflight: &PackagePreflight,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<LayoutPlan>, BTreeSet<u64>), ConversionError> {
    let mut layouts = Vec::new();
    let mut required_shared = BTreeSet::new();
    for name in &preflight.sheet_order {
        context.checkpoint()?;
        let (part, expected_cells) = preflight.xml_sheets.get(name).ok_or_else(|| {
            malformed(Some("xl/workbook.xml"), format!("worksheet {name} was not prepared"))
        })?;
        let layout = if let Some(layout) = preflight.xml_layouts.get(name) {
            layout.clone()
        } else {
            let entry = archive.by_name(part).map_err(|error| {
                malformed(Some(part), format!("worksheet part is missing: {error}"))
            })?;
            read_layout(BufReader::with_capacity(64 * 1024, entry), part, options, context)?
        };
        if layout.physical_cells != *expected_cells {
            return Err(malformed(
                Some(part),
                "worksheet physical cell count changed after preflight",
            ));
        }
        required_shared.extend(layout.required_shared.iter().copied());
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
        let regions = preflight.xml_regions.get(name).cloned().ok_or_else(|| {
            malformed(Some(part), format!("worksheet {name} region plan was not prepared"))
        })?;
        layouts.push(LayoutPlan {
            name: name.clone(),
            part: part.clone(),
            layout,
            regions,
            merges,
        });
    }
    Ok((layouts, required_shared))
}

fn read_shared_strings(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    required: &BTreeSet<u64>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<BTreeMap<u64, String>, ConversionError> {
    if required.is_empty() {
        Ok(BTreeMap::default())
    } else {
        let part = "xl/sharedStrings.xml";
        let entry = archive.by_name(part).map_err(|error| {
            malformed(Some(part), format!("shared-string part is missing: {error}"))
        })?;
        read_selected(BufReader::with_capacity(64 * 1024, entry), required, part, options, context)
    }
}

fn read_display_profile(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<DisplayProfile, ConversionError> {
    let mut display = if let Ok(entry) = archive.by_name("xl/styles.xml") {
        read_number_formats(
            BufReader::with_capacity(64 * 1024, entry),
            "xl/styles.xml",
            options,
            context,
        )?
    } else {
        DisplayProfile::default()
    };
    if let Ok(entry) = archive.by_name("xl/workbook.xml") {
        display = display.with_date_system(read_date_system(
            BufReader::with_capacity(16 * 1024, entry),
            "xl/workbook.xml",
            context,
        )?);
    }
    Ok(display)
}

fn stage_sheets(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    layouts: Vec<LayoutPlan>,
    preflight: &PackagePreflight,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<
    (Vec<PreparedSheet>, StagingTelemetry, Vec<into_markdown_core::Diagnostic>),
    ConversionError,
> {
    let mut prepared = Vec::new();
    let mut telemetry = StagingTelemetry::default();
    let mut diagnostics = vec![warning(
        "spreadsheet.dataPlaneFallback",
        "worksheet data was read through the bounded native SpreadsheetML adapter".into(),
        None,
    )];
    for plan in layouts {
        let LayoutPlan { name, part, layout, regions, merges } = plan;
        context.checkpoint()?;
        let entry = archive.by_name(&part).map_err(|error| {
            malformed(Some(&part), format!("worksheet part is missing: {error}"))
        })?;
        let mut writer = StagingWriter::new(context)?;
        let data_layout = read_cells_into(
            BufReader::with_capacity(64 * 1024, entry),
            &part,
            options,
            context,
            &mut |cell| writer.push(&cell, context),
        )?;
        if data_layout.physical_cells != layout.physical_cells {
            return Err(malformed(
                Some(&part),
                "worksheet data pass disagrees with the layout pass",
            ));
        }
        let cells = writer.finish(context)?;
        let sheet_telemetry = cells.telemetry();
        absorb_telemetry(&mut telemetry, sheet_telemetry);
        if layout
            .declared_bounds
            .zip(layout.bounds)
            .is_some_and(|(declared, actual)| declared.0 < actual.0 || declared.1 < actual.1)
        {
            if options.error_policy != into_markdown_core::ErrorPolicy::BestEffort {
                return Err(malformed(
                    Some(&part),
                    "worksheet dimension under-reports actual cells",
                ));
            }
            if !preflight.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "spreadsheet.dimension.corrected"
                    && diagnostic.locator.as_ref().and_then(|locator| locator.sheet.as_deref())
                        == Some(name.as_str())
            }) {
                diagnostics.push(warning(
                    "spreadsheet.dimension.corrected",
                    format!("worksheet {name} dimension under-reported actual cells"),
                    Some(SourceLocator {
                        sheet: Some(name.clone()),
                        part: Some(part.clone()),
                        ..SourceLocator::default()
                    }),
                ));
            }
        }
        prepared.push(PreparedSheet {
            name: name.clone(),
            bounds: preflight
                .sheet_bounds
                .get(&name)
                .copied()
                .into_iter()
                .chain(regions.iter().map(|region| (region.last_row, region.last_column)))
                .reduce(|left, right| (left.0.max(right.0), left.1.max(right.1))),
            regions,
            merges,
            cells: Some(cells),
            physical_cells: layout.physical_cells,
            extras: preflight.extras.get(&name).cloned().unwrap_or_default(),
        });
    }
    Ok((prepared, telemetry, diagnostics))
}

fn attach_telemetry(
    document: &mut into_markdown_core::Document,
    telemetry: StagingTelemetry,
    sheet_count: usize,
    shared_strings_read: bool,
) {
    for (name, value) in [
        ("layoutPasses", u64::try_from(sheet_count).unwrap_or(u64::MAX)),
        ("dataPasses", u64::try_from(sheet_count).unwrap_or(u64::MAX)),
        ("sharedStringPasses", u64::from(shared_strings_read)),
        ("stagingWrites", telemetry.writes),
        ("stagingFlushes", telemetry.flushes),
        ("stagingReads", telemetry.reads),
        ("stagingSeeks", telemetry.seeks),
        ("stagingBytes", telemetry.staged_bytes),
        ("stagingTemporaryHighWater", telemetry.temporary_high_water),
    ] {
        document
            .metadata
            .properties
            .insert(format!("spreadsheet.native.{name}"), value.to_string());
    }
}

fn absorb_telemetry(total: &mut StagingTelemetry, sheet: StagingTelemetry) {
    total.writes = total.writes.saturating_add(sheet.writes);
    total.flushes = total.flushes.saturating_add(sheet.flushes);
    total.reads = total.reads.saturating_add(sheet.reads);
    total.seeks = total.seeks.saturating_add(sheet.seeks);
    total.staged_bytes = total.staged_bytes.saturating_add(sheet.staged_bytes);
    total.temporary_high_water = total.temporary_high_water.max(sheet.temporary_high_water);
}

#[cfg(test)]
mod tests {
    use crate::workbook::xlsx::regions::compact_declared_region;

    #[test]
    fn declared_region_consumes_the_shared_empty_cell_budget() {
        assert!(compact_declared_region((9, 9), 1, 0).is_some());
        assert!(compact_declared_region((1_999, 9), 19_995, 5).is_some());
        assert!(compact_declared_region((1_999, 9), 19_995, 4).is_none());
        assert!(compact_declared_region((1_048_575, 10), 1_000, 100).is_none());
    }
}
