//! Strict offline recursive ZIP conversion.

mod allocation;
mod archive;
mod budget;
mod entry_policy;
mod headers;
mod merge;
mod recursive;

#[cfg(test)]
mod security_tests;

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, InputFormat, ProbeOutcome, ResolvedInput, Services,
};

const FORMATS: &[InputFormat] = &[InputFormat::Zip];

/// Security-hardened recursive ZIP converter.
#[derive(Debug, Default)]
pub struct ZipConverter;

impl Converter for ZipConverter {
    fn id(&self) -> &'static str {
        "builtin.converter.zip"
    }

    fn priority(&self) -> i32 {
        220
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
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

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if candidate.format != InputFormat::Zip {
                return Ok(ProbeOutcome::NotApplicable);
            }
            let magic = input.bytes.starts_with(b"PK\x03\x04")
                || input.bytes.starts_with(b"PK\x05\x06")
                || input.bytes.starts_with(b"PK\x07\x08");
            Ok(if magic {
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
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { recursive::convert(&input.bytes, options, services, context).await })
    }
}
