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
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterEventSink, ConverterOutput,
    ConverterStream, ConverterStreamCompletion, ConverterStreamMode, ExecutionContext,
    FormatCandidate, InputFormat, LocalBoxFuture, ProbeOutcome, ResolvedInput, Services,
    StreamConsumerKind, stream_converter_output,
};

const FORMATS: &[InputFormat] = &[InputFormat::Xlsx];
const PROVIDER_ID: &str = "builtin.converter.workbook";
const COLLECTING_STREAM_MIN_WORKSHEET_BYTES: u64 = 256 * 1024;

/// Strict workbook converter. Formulae are preserved but never evaluated.
#[derive(Debug, Default)]
pub struct WorkbookConverter;

pub(crate) fn convert_legacy_xls(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    calamine_adapter::convert_xls(bytes, options, context)
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

    fn stream_mode_for(
        &self,
        input: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        consumer: StreamConsumerKind,
    ) -> ConverterStreamMode {
        if consumer == StreamConsumerKind::Collecting && !has_large_worksheet_payload(&input.bytes)
        {
            ConverterStreamMode::AggregateAdapter
        } else {
            ConverterStreamMode::Native
        }
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

fn has_large_worksheet_payload(bytes: &[u8]) -> bool {
    const EOCD: &[u8; 4] = b"PK\x05\x06";
    const CENTRAL: &[u8; 4] = b"PK\x01\x02";
    let search_start = bytes.len().saturating_sub(65_557);
    let Some(eocd) = bytes[search_start..]
        .windows(EOCD.len())
        .rposition(|window| window == EOCD)
        .map(|offset| search_start + offset)
    else {
        return false;
    };
    let Some(mut cursor) = checked_add(eocd, 16)
        .and_then(|offset| le32_at(bytes, offset))
        .and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    let Some(entries) =
        checked_add(eocd, 10).and_then(|offset| le16_at(bytes, offset)).map(usize::from)
    else {
        return false;
    };
    let mut worksheet_bytes = 0_u64;
    for _ in 0..entries {
        let Some(signature_end) = checked_add(cursor, 4) else {
            return false;
        };
        if bytes.get(cursor..signature_end) != Some(CENTRAL) {
            return false;
        }
        let Some(name_len) =
            checked_add(cursor, 28).and_then(|offset| le16_at(bytes, offset)).map(usize::from)
        else {
            return false;
        };
        let Some(extra_len) =
            checked_add(cursor, 30).and_then(|offset| le16_at(bytes, offset)).map(usize::from)
        else {
            return false;
        };
        let Some(comment_len) =
            checked_add(cursor, 32).and_then(|offset| le16_at(bytes, offset)).map(usize::from)
        else {
            return false;
        };
        let Some(name_start) = checked_add(cursor, 46) else {
            return false;
        };
        let Some(name_end) = name_start.checked_add(name_len) else {
            return false;
        };
        let Some(name) = bytes.get(name_start..name_end) else {
            return false;
        };
        if name.starts_with(b"xl/worksheets/")
            && (name.ends_with(b".xml") || name.ends_with(b".bin"))
        {
            let Some(size) = checked_add(cursor, 24).and_then(|offset| le32_at(bytes, offset))
            else {
                return false;
            };
            worksheet_bytes = worksheet_bytes.saturating_add(u64::from(size));
            if worksheet_bytes >= COLLECTING_STREAM_MIN_WORKSHEET_BYTES {
                return true;
            }
        }
        let Some(next) =
            name_end.checked_add(extra_len).and_then(|value| value.checked_add(comment_len))
        else {
            return false;
        };
        cursor = next;
    }
    false
}

fn le16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..checked_add(offset, 2)?)?.try_into().ok()?))
}

fn le32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..checked_add(offset, 4)?)?.try_into().ok()?))
}

fn checked_add(left: usize, right: usize) -> Option<usize> {
    left.checked_add(right)
}
