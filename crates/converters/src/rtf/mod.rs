//! Bounded, non-executing Rich Text Format conversion.

mod budget;
mod control;
mod destinations;
mod image;
mod parser;
mod table;
mod text;

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
};

const FORMATS: &[InputFormat] = &[InputFormat::Rtf];
pub(super) const PROVIDER_ID: &str = "builtin.converter.rtf";
const MAX_NUMERIC_DIGITS: usize = 10;

/// Strict, offline RTF converter. Embedded objects and active destinations are never executed.
#[derive(Debug, Default)]
pub struct RtfConverter;

impl Converter for RtfConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        240
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
            if candidate.format != InputFormat::Rtf {
                return Ok(ProbeOutcome::NotApplicable);
            }
            Ok(if strict_header(&input.bytes).is_some() {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
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
        Ok(context.available_memory_bytes())
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { convert_rtf_bytes(&input.bytes, options, context) })
    }
}

/// Parse already-decoded container payload bytes with the caller's exact request context.
///
/// This intentionally accepts neither [`Services`] nor a fresh limits object, so container
/// converters such as MSG can reuse the parser without gaining network, filesystem, or budget
/// reset capabilities.
pub(crate) fn convert_rtf_bytes(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    parser::parse_rtf(bytes, options, context)
}

pub(super) fn strict_header(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(b"{\\rtf") {
        return None;
    }
    let mut offset = 5;
    let first = *bytes.get(offset)?;
    if !first.is_ascii_digit() {
        return None;
    }
    while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
        offset += 1;
        if offset - 5 > MAX_NUMERIC_DIGITS {
            return None;
        }
    }
    matches!(bytes.get(offset), Some(b' ' | b'\\' | b'{' | b'}' | b'\r' | b'\n')).then_some(offset)
}

#[cfg(test)]
mod tests;
