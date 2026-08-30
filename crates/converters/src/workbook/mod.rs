//! Bounded, offline `SpreadsheetML` (`.xlsx`/`.xlsm`) and XLSB conversion.
//!
//! The converter treats both encodings as OPC packages. Package structure and
//! resource bounds are checked before the format parser is allowed to allocate.
//! VBA, embedded OLE, external-workbook links, and formula evaluation are never
//! invoked.

mod budget;
mod calamine_adapter;
mod cell;
mod error;
mod extras;
mod images;
mod legacy_xls_emit;
mod model;
mod opc;
mod orchestrator;
mod output;
mod preflight;
mod resource_profile;
mod schema;
mod xlsb;
mod xlsx;

#[cfg(test)]
mod tests;

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterEventSink, ConverterOutput,
    ConverterStream, ConverterStreamCompletion, ConverterStreamMode, ExecutionContext,
    FormatCandidate, InputFormat, LocalBoxFuture, ProbeOutcome, ResolvedInput, ResourceReservation,
    Services, SourceContentEvidence, document_is_empty, stream_converter_output,
};
use std::collections::{BTreeMap, BTreeSet};

const FORMATS: &[InputFormat] = &[InputFormat::Xlsx];
const PROVIDER_ID: &str = "builtin.converter.workbook";

/// Strict workbook converter. Formulae are preserved but never evaluated.
#[derive(Debug, Default)]
pub struct WorkbookConverter;

#[derive(Debug)]
pub(crate) struct LegacyFormulaCache {
    pub(crate) sheet_index: usize,
    pub(crate) row: u32,
    pub(crate) column: u32,
    pub(crate) value: String,
}

#[derive(Debug)]
pub(crate) struct LegacyCellFormat {
    pub(crate) sheet_index: usize,
    pub(crate) row: u32,
    pub(crate) column: u32,
    pub(crate) format_index: u16,
}

#[derive(Debug)]
pub(crate) struct LegacyFormulaExpression {
    pub(crate) sheet_index: usize,
    pub(crate) row: u32,
    pub(crate) column: u32,
    pub(crate) value: Option<String>,
    pub(crate) token_sha256: [u8; 32],
}

#[derive(Debug, Default)]
pub(crate) struct LegacyXlsHints {
    pub(crate) authenticated_bounds: BTreeMap<String, (u32, u32)>,
    pub(crate) authenticated_empty_sheets: BTreeSet<String>,
    pub(crate) formula_caches: Vec<LegacyFormulaCache>,
    pub(crate) cell_formats: Vec<LegacyCellFormat>,
    pub(crate) format_codes: BTreeMap<u16, String>,
    pub(crate) formula_expressions: Vec<LegacyFormulaExpression>,
    pub(crate) recovered_format_records: usize,
    pub(crate) _memory: Option<ResourceReservation>,
}

pub(crate) fn convert_legacy_xls(
    bytes: &[u8],
    hints: &LegacyXlsHints,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let output = calamine_adapter::convert_xls(bytes, hints, options, context)?;
    if document_is_empty(&output.document)
        && output.assets.is_empty()
        && output.diagnostics.is_empty()
    {
        Ok(output.with_source_content_evidence(SourceContentEvidence::Empty))
    } else {
        Ok(output)
    }
}

impl Converter for WorkbookConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        250
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
    }

    fn stream_support(&self) -> Option<&dyn ConverterStream> {
        Some(self)
    }

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if candidate.format != InputFormat::Xlsx {
                return Ok(ProbeOutcome::NotApplicable);
            }
            let zip = input.bytes.starts_with(b"PK\x03\x04")
                || input.bytes.starts_with(b"PK\x05\x06")
                || input.bytes.starts_with(b"PK\x07\x08");
            Ok(if candidate.explicit || candidate.detector_id == "builtin.detector.hints" || zip {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
            })
        })
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                orchestrator::convert_workbook(&input.bytes, options, context)
            }))
            .unwrap_or_else(|_| {
                Err(error::malformed(None, "workbook parser rejected invalid structure"))
            })
        })
    }

    fn planned_output_bytes(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        // Package dimensions are untrusted and must be read under the engine's
        // full authenticated request credit. Structural scanning proves the
        // combined peak and only takes concrete child reservations for its ZIP
        // directory/materialization and codec working sets.
        Ok(context.available_memory_bytes())
    }
}

impl ConverterStream for WorkbookConverter {
    fn stream_mode(&self) -> ConverterStreamMode {
        ConverterStreamMode::Native
    }

    fn convert_stream<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
        sink: &'a mut dyn ConverterEventSink,
    ) -> LocalBoxFuture<'a, Result<ConverterStreamCompletion, ConversionError>> {
        Box::pin(async move {
            let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                orchestrator::convert_workbook(&input.bytes, options, context)
            }))
            .unwrap_or_else(|_| {
                Err(error::malformed(None, "workbook parser rejected invalid structure"))
            })?;
            stream_converter_output(output, sink)
        })
    }
}
