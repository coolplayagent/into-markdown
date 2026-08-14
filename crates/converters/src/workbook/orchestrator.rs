use crate::workbook::calamine_adapter::{convert_xlsb, convert_xlsx};
use crate::workbook::error::limit;
use crate::workbook::model::WorkbookKind;
use crate::workbook::preflight::preflight_package;
use into_markdown_core::{
    Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput, ExecutionContext,
    estimate_retained_output, estimate_validation_working_set,
};

pub(super) fn convert_workbook(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    // The engine has already acquired an authenticated parent permit before
    // invoking this converter. Derive the complete child credit before ZIP,
    // XML, BIFF12, or Calamine can allocate.
    let available = context.available_memory_bytes();
    let (mut preflight, _allocation_permit) =
        preflight_package(bytes, options, context, available)?;
    if preflight.memory_peak > available {
        return Err(limit("max_memory_bytes", format!("{} > {available}", preflight.memory_peak)));
    }
    context.checkpoint()?;

    let mut output = match preflight.kind {
        WorkbookKind::Xml => {
            convert_xlsx(bytes, &preflight.sheet_bounds, &preflight.extras, options, context)?
        }
        WorkbookKind::Binary => convert_xlsb(
            bytes,
            &preflight.sheet_parts,
            &preflight.sheet_bounds,
            &preflight.extras,
            options,
            context,
        )?,
    };
    output.assets = std::mem::take(&mut preflight.assets);
    output.diagnostics.extend(preflight.diagnostics);
    output.document.metadata.properties.insert(
        "spreadsheet.encoding".into(),
        match preflight.kind {
            WorkbookKind::Xml => "spreadsheetml",
            WorkbookKind::Binary => "xlsb",
        }
        .into(),
    );
    output
        .document
        .metadata
        .properties
        .insert("spreadsheet.macrosPresent".into(), preflight.macro_present.to_string());
    output
        .document
        .metadata
        .properties
        .insert("spreadsheet.formulasEvaluated".into(), "false".into());
    output
        .document
        .metadata
        .properties
        .insert("spreadsheet.formulaStylePolicy".into(), "codeSemanticsOverrideCellMarks".into());
    output
        .document
        .metadata
        .properties
        .insert("spreadsheet.mediaBytes".into(), preflight.media_bytes.to_string());
    output
        .document
        .metadata
        .properties
        .insert("spreadsheet.preflight.memoryPeak".into(), preflight.memory_peak.to_string());
    for (name, value) in [
        ("cells", preflight.inventory.cells),
        ("formulas", preflight.inventory.formulas),
        ("sharedStrings", preflight.inventory.shared_strings),
        ("styles", preflight.inventory.styles),
        ("fonts", preflight.inventory.fonts),
        ("numberFormats", preflight.inventory.number_formats),
        ("externalSheetSlots", preflight.inventory.external_sheet_slots),
        ("recordBytes", preflight.inventory.record_bytes),
    ] {
        output
            .document
            .metadata
            .properties
            .insert(format!("spreadsheet.preflight.{name}"), value.to_string());
    }
    output.document.validate().map_err(|error| ConversionError::Internal {
        detail: format!("workbook converter produced invalid IR: {error}"),
    })?;
    let engine_owned_peak =
        estimate_retained_output(&output.document, &output.assets, &output.diagnostics)?
            .checked_add(estimate_validation_working_set(
                &output.document,
                &output.assets,
                &output.diagnostics,
            )?)
            .and_then(|value| value.checked_add(workbook_provenance_plan(&output.document.blocks)))
            .ok_or_else(|| limit("max_memory_bytes", "engine workbook validation peak overflow"))?;
    if engine_owned_peak > preflight.memory_peak {
        return Err(limit(
            "max_memory_bytes",
            format!(
                "engine workbook validation requires {engine_owned_peak} > {}",
                preflight.memory_peak
            ),
        ));
    }
    Ok(output)
}

fn workbook_provenance_plan(nodes: &[BlockNode]) -> u64 {
    fn walk(nodes: &[BlockNode], total: &mut u64) {
        for node in nodes {
            *total = total.saturating_add(1_024);
            match &node.block {
                Block::List { items, .. } => {
                    for item in items {
                        walk(&item.blocks, total);
                    }
                }
                Block::Table { rows, .. } => {
                    for cell in rows.iter().flat_map(|row| &row.cells) {
                        walk(&cell.blocks, total);
                    }
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => walk(blocks, total),
                _ => {}
            }
        }
    }
    let mut total = 4_096;
    walk(nodes, &mut total);
    total
}
