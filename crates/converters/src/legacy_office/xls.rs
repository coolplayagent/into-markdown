use super::budget::{LegacyBudget, limit, malformed};
use super::builder::{PROVIDER_ID, locator};
use super::normalize_xls_output;
use crate::msg::ole::Storage;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, ConverterOutput,
    Diagnostic, DiagnosticSeverity, ErrorPolicy, ExecutionContext, Inline, IrErrorCode, NodeId,
    Provenance, ProvenanceKind, ValidationLimits,
};

mod binary;
mod objects;
mod preflight;
mod wrapper;

use binary::{read_u16, read_u32};
use objects::retain_safe_images;
use preflight::{
    PreflightFlag, append_preflight_diagnostics, enforce_document_node_limit, preflight,
    recover_continued_formula_string_caches,
};
use wrapper::{build_cfb_wrapper, cfb_wrapper_layout, normalize_raw_biff4};

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
    let preflight = preflight(bytes, WORKBOOK, budget, options.error_policy)?;
    if preflight.biff_version != BIFF4 {
        return Err(malformed(WORKBOOK, "raw XLS stream is not BIFF4"));
    }
    let normalized_limit = bytes
        .len()
        .checked_mul(2)
        .and_then(|value| value.checked_add(64))
        .ok_or_else(|| limit("max_memory_bytes", "normalized BIFF4 size overflowed"))?;
    let _normalized_memory =
        context.reserve_memory(u64::try_from(normalized_limit).unwrap_or(u64::MAX))?;
    let normalized = normalize_raw_biff4(&bytes[..preflight.logical_end])?;
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
    let mut output = crate::workbook::convert_legacy_xls(&wrapper, options, context)?;
    drop(wrapper);
    drop(wrapper_memory);
    enforce_document_node_limit(&output)?;
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
    let (part, workbook) = root
        .stream(WORKBOOK)
        .map(|stream| (WORKBOOK, stream))
        .or_else(|| root.stream("Book").map(|stream| ("Book", stream)))
        .ok_or_else(|| malformed("CFB directory", "XLS has no Workbook stream"))?;
    let preflight = preflight(workbook, part, budget, options.error_policy)?;

    let workbook_view = &workbook[..preflight.logical_end];
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
    let mut output = crate::workbook::convert_legacy_xls(conversion_bytes, options, context)?;
    let recovered_formula_continuations = if preflight.has(PreflightFlag::FormulaCacheMetadata) {
        recover_continued_formula_string_caches(workbook_view, &mut output, part, budget, options)?
    } else {
        0
    };
    drop(wrapper);
    drop(wrapper_memory);
    enforce_document_node_limit(&output)?;
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
    retain_safe_images(workbook, part, &mut output, budget)?;
    Ok(output)
}

#[cfg(test)]
mod tests;
