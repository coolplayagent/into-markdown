//! Process-isolated conversion for legacy binary Office formats.

mod remap;

#[cfg(test)]
mod tests;

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    FormatCandidate, FormatHint, InputFormat, NestedConversionRequest, ProbeOutcome, ResolvedInput,
    ResourceReservation, Services, SourceMetadata,
};
use into_markdown_legacy_office::{LegacyOfficeRuntime, NormalizedFormat};
use std::fmt;
use std::sync::Arc;

const FORMATS: &[InputFormat] = &[InputFormat::Doc, InputFormat::Ppt, InputFormat::Xls];
const PROVIDER_ID: &str = "builtin.converter.legacy-office";
const EXCLUDED: &[&str] = &[PROVIDER_ID];
const CFBF_MAGIC: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";

struct AdapterOutput {
    bytes: Box<[u8]>,
    format: NormalizedFormat,
    version: String,
    artifact_sha256: String,
    target: String,
    memory: ResourceReservation,
}

trait CompatibilityAdapter: Send + Sync {
    fn normalize(
        &self,
        bytes: &[u8],
        source: InputFormat,
        maximum_output_bytes: u64,
        context: &ExecutionContext,
    ) -> Result<AdapterOutput, ConversionError>;
}

struct RuntimeAdapter {
    runtime: Option<LegacyOfficeRuntime>,
}

impl CompatibilityAdapter for RuntimeAdapter {
    fn normalize(
        &self,
        bytes: &[u8],
        source: InputFormat,
        maximum_output_bytes: u64,
        context: &ExecutionContext,
    ) -> Result<AdapterOutput, ConversionError> {
        let packaged;
        let runtime = if let Some(runtime) = &self.runtime {
            runtime
        } else {
            packaged = LegacyOfficeRuntime::packaged()?;
            &packaged
        };
        let package = runtime.convert(bytes, source, maximum_output_bytes, context)?;
        Ok(AdapterOutput {
            bytes: package.bytes,
            format: package.format,
            version: package.runtime.version().to_owned(),
            artifact_sha256: package.runtime.artifact_sha256().to_owned(),
            target: package.runtime.target().to_owned(),
            memory: package.memory,
        })
    }
}

/// Isolated converter for DOC, PPT/PPS/POT, and XLS compound documents.
pub struct LegacyOfficeConverter {
    adapter: Arc<dyn CompatibilityAdapter>,
}

impl LegacyOfficeConverter {
    /// Use one explicitly configured, authority-validated compatibility runtime.
    #[must_use]
    pub fn with_runtime(runtime: LegacyOfficeRuntime) -> Self {
        Self { adapter: Arc::new(RuntimeAdapter { runtime: Some(runtime) }) }
    }
}

impl Default for LegacyOfficeConverter {
    fn default() -> Self {
        Self { adapter: Arc::new(RuntimeAdapter { runtime: None }) }
    }
}

impl fmt::Debug for LegacyOfficeConverter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("LegacyOfficeConverter").finish_non_exhaustive()
    }
}

impl Converter for LegacyOfficeConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        230
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
            if !FORMATS.contains(&candidate.format) || !input.bytes.starts_with(CFBF_MAGIC) {
                return Ok(ProbeOutcome::NotApplicable);
            }
            Ok(ProbeOutcome::Match { confidence: 1.0 })
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
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            let nested =
                services.nested.as_ref().ok_or_else(|| ConversionError::ComponentUnavailable {
                    component: "nested-conversion".into(),
                    detail: "the engine did not provide normalized Office dispatch".into(),
                })?;
            let maximum_output = options
                .limits
                .max_archive_entry_bytes
                .min(context.available_memory_bytes())
                .min(into_markdown_legacy_office::MAX_NORMALIZED_PACKAGE_BYTES);
            let normalized =
                self.adapter.normalize(&input.bytes, candidate.format, maximum_output, context)?;
            if normalized.format != expected_output(candidate.format)? {
                return Err(ConversionError::ComponentUnavailable {
                    component: "legacy-office-worker".into(),
                    detail: "workerProtocol".into(),
                });
            }
            let AdapterOutput {
                bytes,
                format,
                version,
                artifact_sha256,
                target,
                memory: normalized_memory,
            } = normalized;
            let length =
                u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "normalized Office package size overflowed".into(),
                })?;
            let shared_plan = length
                .checked_add(u64::try_from(std::mem::size_of::<usize>() * 2).unwrap_or(u64::MAX))
                .ok_or_else(|| ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "normalized Office shared-buffer plan overflowed".into(),
                })?;
            let shared_memory = context.reserve_memory(shared_plan)?;
            let bytes: Arc<[u8]> = Arc::from(bytes);
            drop(normalized_memory);
            let name = format!("normalized.{}", format.extension());
            let normalized_input = ResolvedInput {
                bytes,
                metadata: SourceMetadata {
                    name: Some(name.clone()),
                    media_type: Some(normalized_media_type(format).into()),
                    uri: None,
                    size: length,
                },
            };
            let hint = FormatHint {
                format: Some(format.input_format()),
                filename: Some(name),
                extension: Some(format.extension().into()),
                media_type: Some(normalized_media_type(format).into()),
                charset: None,
            };
            let mut output = nested
                .convert(
                    NestedConversionRequest {
                        input: &normalized_input,
                        hint: &hint,
                        options,
                        excluded_converter_ids: EXCLUDED,
                    },
                    context,
                )
                .await?;
            drop(normalized_input);
            drop(shared_memory);
            remap::remap(
                &mut output,
                candidate.format,
                format,
                &version,
                &artifact_sha256,
                &target,
            )?;
            Ok(output)
        })
    }
}

fn normalized_media_type(format: NormalizedFormat) -> &'static str {
    match format {
        NormalizedFormat::Docx => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        NormalizedFormat::Pptx => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        NormalizedFormat::Xlsx => {
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        }
    }
}

fn expected_output(source: InputFormat) -> Result<NormalizedFormat, ConversionError> {
    match source {
        InputFormat::Doc => Ok(NormalizedFormat::Docx),
        InputFormat::Ppt => Ok(NormalizedFormat::Pptx),
        InputFormat::Xls => Ok(NormalizedFormat::Xlsx),
        _ => Err(ConversionError::Unsupported {
            detail: "legacy Office converter accepts only DOC, PPT/PPS/POT, or XLS".into(),
        }),
    }
}
