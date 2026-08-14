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
mod model;
mod opc;
mod orchestrator;
mod output;
mod preflight;
mod schema;
mod xlsb;
mod xlsx;

#[cfg(test)]
mod tests;

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
};

const FORMATS: &[InputFormat] = &[InputFormat::Xlsx];
const PROVIDER_ID: &str = "builtin.converter.workbook";

/// Strict workbook converter. Formulae are preserved but never evaluated.
#[derive(Debug, Default)]
pub struct WorkbookConverter;

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
