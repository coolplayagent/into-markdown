use super::budget::{LegacyBudget, limit, malformed};
use super::builder::{PROVIDER_ID, locator};
use super::normalize_xls_output;
use crate::msg::ole::Storage;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput,
    Diagnostic, DiagnosticSeverity, ErrorPolicy, ExecutionContext, NodeId, Provenance,
    ProvenanceKind,
};

mod binary;
mod inventory;
mod objects;
mod preflight;
mod wrapper;

use binary::{read_u16, read_u32};
use inventory::scan_workbook_inventory;
use objects::retain_safe_images;
use preflight::{
    PreflightFlag, append_preflight_diagnostics, enforce_document_node_limit, preflight,
};
use wrapper::{build_cfb_wrapper, cfb_wrapper_layout, normalize_raw_biff4, raw_biff4_plan};

const WORKBOOK: &str = "Workbook";
const BOF: u16 = 0x0809;
const BOF4: u16 = 0x0409;
const BIFF8: u16 = 0x0600;
const BIFF5: u16 = 0x0500;
const BIFF4: u16 = 0x0400;
const FILE_PASS: u16 = 0x002f;
const EOF: u16 = 0x000a;
const FORMULA: u16 = 0x0006;
const STRING: u16 = 0x0207;
const CONTINUE: u16 = 0x003c;
const SHARED_FORMULA: u16 = 0x04bc;
const DIMENSIONS: u16 = 0x0200;
const BOUND_SHEET: u16 = 0x0085;
const SUP_BOOK: u16 = 0x01ae;
const EXTERN_SHEET: u16 = 0x0017;
const WINDOW1: u16 = 0x003d;
const WINDOW2: u16 = 0x023e;
const PANE: u16 = 0x0041;
const SELECTION: u16 = 0x001d;
const SCL: u16 = 0x00a0;
const OBJ: u16 = 0x005d;
const MSO_DRAWING_GROUP: u16 = 0x00eb;
const MSO_DRAWING: u16 = 0x00ec;
const CFB_FREE: u32 = 0xffff_ffff;
const CFB_END: u32 = 0xffff_fffe;
const CFB_FAT: u32 = 0xffff_fffd;
const CFB_DIFAT: u32 = 0xffff_fffc;
const CFB_SECTOR_BYTES: usize = 512;
const CFB_FAT_ENTRIES: usize = CFB_SECTOR_BYTES / 4;
const CFB_HEADER_DIFAT_ENTRIES: usize = 109;
const CFB_DIFAT_ENTRIES: usize = CFB_FAT_ENTRIES - 1;

pub(super) fn looks_like_raw_biff(bytes: &[u8]) -> bool {
    bytes.get(..4).is_some_and(|header| {
        u16::from_le_bytes([header[0], header[1]]) == BOF4
            && usize::from(u16::from_le_bytes([header[2], header[3]])) >= 4
    })
}

pub(super) fn convert_raw(
    bytes: &[u8],
    budget: &mut LegacyBudget<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let mut preflight = preflight(bytes, WORKBOOK, budget, options.error_policy)?;
    if preflight.biff_version != BIFF4 {
        return Err(malformed(WORKBOOK, "raw XLS stream is not BIFF4"));
    }
    let plan = raw_biff4_plan(&bytes[..preflight.logical_end])?;
    let normalized_memory =
        context.reserve_memory(u64::try_from(plan.capacity).unwrap_or(u64::MAX))?;
    let normalized = normalize_raw_biff4(&bytes[..preflight.logical_end], plan)?;
    let layout = cfb_wrapper_layout(normalized.len())?;
    let wrapper_memory = context.reserve_memory(
        u64::try_from(layout.output_bytes.saturating_add(layout.total_sectors * 4))
            .unwrap_or(u64::MAX),
    )?;
    let wrapper = build_cfb_wrapper(
        &normalized,
        preflight.has(PreflightFlag::DimensionMetadata),
        preflight.has(PreflightFlag::FormulaCacheMetadata),
        BIFF4,
        layout,
    )?;
    preflight.hints = scan_workbook_inventory(
        &normalized,
        preflight.biff_version,
        WORKBOOK,
        budget,
        context,
        options.error_policy,
    )?;
    drop(normalized);
    drop(normalized_memory);
    let mut output =
        crate::workbook::convert_legacy_xls(&wrapper, &preflight.hints, options, context)?;
    drop(wrapper);
    drop(wrapper_memory);
    normalize_xls_output(&mut output);
    output.document.metadata.properties.insert("legacyOffice.xls.biff".into(), "4".into());
    output.diagnostics.push(Diagnostic {
        code: "legacyOffice.xls.rawBiffRecovered".into(),
        severity: DiagnosticSeverity::Info,
        message: "a bounded raw BIFF4 worksheet stream was normalized into an inert workbook view"
            .into(),
        locator: Some(locator(WORKBOOK)),
    });
    append_preflight_diagnostics(&mut output, &preflight, WORKBOOK);
    append_inventory_diagnostics(&mut output, &preflight.hints, WORKBOOK);
    enforce_document_node_limit(&output)?;
    Ok(output)
}

pub(super) fn convert(
    bytes: &[u8],
    root: Storage<'_>,
    budget: &mut LegacyBudget<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
    container_view_required: bool,
) -> Result<ConverterOutput, ConversionError> {
    let workbook_stream = root.stream(WORKBOOK);
    let book_stream = root.stream("Book");
    let (part, workbook) = match (workbook_stream, book_stream) {
        (Some(_), Some(_)) => {
            return Err(malformed("CFB directory", "XLS has ambiguous Workbook and Book streams"));
        }
        (Some(stream), None) => (WORKBOOK, stream),
        (None, Some(stream)) => ("Book", stream),
        (None, None) => return Err(malformed("CFB directory", "XLS has no Workbook stream")),
    };
    let mut preflight = preflight(workbook, part, budget, options.error_policy)?;

    let workbook_view = &workbook[..preflight.logical_end];
    preflight.hints = scan_workbook_inventory(
        workbook_view,
        preflight.biff_version,
        part,
        budget,
        context,
        options.error_policy,
    )?;
    let wrapper_layout = (container_view_required
        || preflight.has(PreflightFlag::DimensionMetadata)
        || preflight.has(PreflightFlag::FormulaCacheMetadata)
        || preflight.logical_end != workbook.len())
    .then(|| cfb_wrapper_layout(workbook_view.len()))
    .transpose()?;
    let wrapper_memory = wrapper_layout
        .as_ref()
        .map(|layout| {
            context.reserve_memory(
                u64::try_from(layout.output_bytes.saturating_add(layout.total_sectors * 4))
                    .unwrap_or(u64::MAX),
            )
        })
        .transpose()?;
    let wrapper = wrapper_layout
        .as_ref()
        .map(|layout| {
            build_cfb_wrapper(
                workbook_view,
                preflight.has(PreflightFlag::DimensionMetadata),
                preflight.has(PreflightFlag::FormulaCacheMetadata),
                preflight.biff_version,
                *layout,
            )
        })
        .transpose()?;
    let conversion_bytes = wrapper.as_deref().unwrap_or(bytes);
    let mut output =
        crate::workbook::convert_legacy_xls(conversion_bytes, &preflight.hints, options, context)?;
    drop(wrapper);
    drop(wrapper_memory);
    normalize_xls_output(&mut output);
    output.document.metadata.properties.insert(
        "legacyOffice.xls.biff".into(),
        match preflight.biff_version {
            BIFF4 => "4",
            BIFF5 => "5",
            _ => "8",
        }
        .into(),
    );

    append_preflight_diagnostics(&mut output, &preflight, part);
    append_inventory_diagnostics(&mut output, &preflight.hints, part);
    if !preflight.has(PreflightFlag::EmbeddedObjects)
        && (root.storage("ObjectPool").is_some() || root.storage("ActiveX").is_some())
    {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.embeddedObjectsSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "embedded OLE, drawing, and ActiveX objects were not executed or exported"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if root.storage("_VBA_PROJECT_CUR").is_some() || root.stream("_VBA_PROJECT_CUR").is_some() {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.macrosSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "VBA project data was not executed or exposed as active content".into(),
            locator: Some(locator("_VBA_PROJECT_CUR")),
        });
    }
    retain_safe_images(workbook_view, part, &mut output, budget, options.error_policy)?;
    enforce_document_node_limit(&output)?;
    Ok(output)
}

fn append_inventory_diagnostics(
    output: &mut ConverterOutput,
    hints: &crate::workbook::LegacyXlsHints,
    part: &str,
) {
    let recovered_formula_continuations = hints.formula_caches.len();
    if recovered_formula_continuations > 0 {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.formulaStringContinuationRecovered".into(),
            severity: DiagnosticSeverity::Info,
            message: format!(
                "recovered {recovered_formula_continuations} cached formula string(s) split across bounded BIFF Continue records"
            ),
            locator: Some(locator(part)),
        });
    }
    let recovered_format_records = hints.recovered_format_records;
    if recovered_format_records > 0 {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.formatMetadataRecovered".into(),
            severity: DiagnosticSeverity::Info,
            message: format!(
                "ignored {recovered_format_records} malformed or duplicate optional BIFF Format record(s) while retaining the first authenticated definition"
            ),
            locator: Some(locator(part)),
        });
    }
}

#[cfg(test)]
mod tests;
