use crate::workbook::error::{limit, malformed, warning};
use crate::workbook::model::PackagePreflight;
use crate::workbook::xlsx::emitter::{PreparedSheet, emit};
use crate::workbook::xlsx::regions::{MergeRange, SparseRegion};
use crate::workbook::xlsx::sheet_index::{read_cells_into, read_layout};
use crate::workbook::xlsx::staging::{StagingTelemetry, StagingWriter};
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, ExecutionContext, SourceLocator,
};
use std::io::{BufReader, Cursor};
use std::sync::{Arc, Mutex};

pub(in crate::workbook) fn convert_xlsx(
    bytes: &[u8],
    preflight: &PackagePreflight,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| malformed(None, format!("cannot reopen authenticated XLSX: {error}")))?;
    let layouts = prepare_layouts(&mut archive, preflight, options, context)?;
    let staged = stage_sheets(&mut archive, layouts, preflight, options, context)?;
    let (mut document, diagnostics) = emit(
        staged.prepared,
        preflight.xml_display_profile.as_ref().ok_or_else(|| {
            malformed(Some("xl/styles.xml"), "prepared display profile is missing")
        })?,
        &preflight.xml_shared_strings,
        staged.diagnostics,
        options,
        context,
    )?;
    let telemetry = collect_telemetry(&staged.telemetry_handles)?;
    attach_telemetry(&mut document, telemetry, preflight);
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
    populated_merge_subordinates: bool,
}

struct StagedSheets {
    prepared: Vec<PreparedSheet>,
    telemetry_handles: Vec<Arc<Mutex<StagingTelemetry>>>,
    diagnostics: Vec<into_markdown_core::Diagnostic>,
}

fn prepare_layouts(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    preflight: &PackagePreflight,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<LayoutPlan>, ConversionError> {
    let mut layouts = Vec::new();
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
        let populated_merge_subordinates = has_populated_merge_subordinates(&layout.runs, &merges);
        let regions = preflight.xml_regions.get(name).cloned().ok_or_else(|| {
            malformed(Some(part), format!("worksheet {name} region plan was not prepared"))
        })?;
        layouts.push(LayoutPlan {
            name: name.clone(),
            part: part.clone(),
            layout,
            regions,
            merges,
            populated_merge_subordinates,
        });
    }
    Ok(layouts)
}

fn stage_sheets(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    layouts: Vec<LayoutPlan>,
    preflight: &PackagePreflight,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<StagedSheets, ConversionError> {
    let mut prepared = Vec::new();
    let mut telemetry_handles = Vec::new();
    let mut diagnostics = Vec::new();
    for plan in layouts {
        let LayoutPlan { name, part, layout, regions, merges, populated_merge_subordinates } = plan;
        diagnostics.extend(layout.diagnostics.iter().cloned());
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
        telemetry_handles.push(cells.telemetry_handle());
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
            populated_merge_subordinates,
            cells: Some(cells),
            physical_cells: layout.physical_cells,
            extras: preflight.extras.get(&name).cloned().unwrap_or_default(),
        });
    }
    Ok(StagedSheets { prepared, telemetry_handles, diagnostics })
}

fn has_populated_merge_subordinates(
    runs: &[crate::workbook::xlsx::sheet_index::SheetRun],
    merges: &[MergeRange],
) -> bool {
    let mut ordered = merges.to_vec();
    ordered.sort_unstable_by_key(|range| {
        (range.first_row, range.first_column, range.last_row, range.last_column)
    });
    let mut active = Vec::<MergeRange>::new();
    let mut next = 0;
    for run in runs {
        active.retain(|range| range.last_row >= run.row);
        while ordered.get(next).is_some_and(|range| range.first_row <= run.row) {
            active.push(ordered[next]);
            next += 1;
        }
        for range in &active {
            let first = run.first_column.max(range.first_column);
            let last = run.last_column.min(range.last_column);
            if first <= last
                && (run.row != range.first_row
                    || first != range.first_column
                    || last != range.first_column)
            {
                return true;
            }
        }
    }
    false
}

fn attach_telemetry(
    document: &mut into_markdown_core::Document,
    telemetry: StagingTelemetry,
    preflight: &PackagePreflight,
) {
    for (name, value) in [
        ("workbookPasses", preflight.xml_workbook_passes),
        ("layoutPasses", u64::try_from(preflight.xml_layouts.len()).unwrap_or(u64::MAX)),
        ("dataPasses", telemetry.data_passes),
        ("stylePasses", preflight.xml_styles_passes),
        ("sharedStringPasses", preflight.xml_shared_string_passes),
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
    total.data_passes = total.data_passes.saturating_add(sheet.data_passes);
    total.writes = total.writes.saturating_add(sheet.writes);
    total.flushes = total.flushes.saturating_add(sheet.flushes);
    total.reads = total.reads.saturating_add(sheet.reads);
    total.seeks = total.seeks.saturating_add(sheet.seeks);
    total.staged_bytes = total.staged_bytes.saturating_add(sheet.staged_bytes);
    total.temporary_high_water = total.temporary_high_water.max(sheet.temporary_high_water);
}

fn collect_telemetry(
    handles: &[Arc<Mutex<StagingTelemetry>>],
) -> Result<StagingTelemetry, ConversionError> {
    let mut total = StagingTelemetry::default();
    for handle in handles {
        let telemetry =
            *handle.lock().map_err(|_| malformed(None, "staging telemetry lock is poisoned"))?;
        absorb_telemetry(&mut total, telemetry);
    }
    Ok(total)
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
