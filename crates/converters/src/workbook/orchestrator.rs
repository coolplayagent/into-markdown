use crate::workbook::calamine_adapter::convert_xlsb;
use crate::workbook::error::limit;
use crate::workbook::model::WorkbookKind;
use crate::workbook::preflight::preflight_package;
use crate::workbook::xlsx::adapter::convert_xlsx;
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, ExecutionContext, SourceContentEvidence,
    document_is_empty,
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
        WorkbookKind::Xml => convert_xlsx(bytes, &preflight, options, context)?,
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
    if document_is_empty(&output.document)
        && output.assets.is_empty()
        && output.diagnostics.is_empty()
    {
        output = output.with_source_content_evidence(SourceContentEvidence::Empty);
    }
    // The engine validates and accounts the retained IR after parser workspace is released.
    Ok(output)
}
