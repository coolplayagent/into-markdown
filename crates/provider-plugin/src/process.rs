use crate::{CapabilityKind, PluginManifest, ProviderBinding, ResourceEnvelope};
use into_markdown_core::{
    Block, BoundOcrResult, BoundOcrResultDto, BoxFuture, ConversionError, ConversionOptions,
    DiarizationRequest, DiarizationResult, Diarizer, Document, ExecutionContext, OcrEngine,
    OcrOutputPlan, OcrRecognition, OcrRequest, OcrResult, Transcriber, TranscriptionRequest,
    TranscriptionResult,
};
use into_markdown_plugin_manager::PreparedProcessPlugin;
use into_markdown_process_plugin::{PluginError, PluginErrorCode, PluginRequest, RuntimePolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const PROTOCOL_VERSION: u32 = 1;
const FRAME_OVERHEAD: u64 = 1024 * 1024;
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Authenticated process runtime bound to one installed capability and model.
pub struct ProcessCapability {
    process: PreparedProcessPlugin,
    binding: ProviderBinding,
    kind: CapabilityKind,
    resources: ResourceEnvelope,
    model_roots: Vec<PathBuf>,
}

impl std::fmt::Debug for ProcessCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessCapability")
            .field("binding", &self.binding)
            .field("kind", &self.kind)
            .field("resources", &self.resources)
            .field("model_roots", &self.model_roots)
            .finish_non_exhaustive()
    }
}

impl ProcessCapability {
    /// Build the sandbox policy declared by one signed capability manifest.
    ///
    /// `read_only_roots` normally contains the writable and packaged model stores. No other host
    /// path is exposed to the provider.
    pub fn runtime_policy(
        manifest: &PluginManifest,
        binding: &ProviderBinding,
        read_only_roots: Vec<PathBuf>,
    ) -> Result<(RuntimePolicy, Vec<PathBuf>), ConversionError> {
        if binding.plugin_id != manifest.id {
            return Err(unavailable(&binding.provider_id, "installed plugin binding changed"));
        }
        let capability = manifest
            .capabilities
            .iter()
            .find(|candidate| candidate.id == binding.capability_id)
            .ok_or_else(|| {
                unavailable(&binding.provider_id, "capability is absent from manifest")
            })?;
        if capability.provider_id != binding.provider_id {
            return Err(unavailable(&binding.provider_id, "provider identity changed"));
        }
        let max_frame = capability
            .resources
            .max_output_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(FRAME_OVERHEAD))
            .filter(|bytes| *bytes <= MAX_FRAME_BYTES)
            .ok_or_else(|| {
                unavailable(&binding.provider_id, "capability output exceeds protocol limits")
            })?;
        let mut roots = Vec::with_capacity(read_only_roots.len());
        for root in read_only_roots {
            if !root.is_dir() {
                continue;
            }
            let canonical = root.canonicalize().map_err(|_| {
                unavailable(&binding.provider_id, "model store cannot be canonicalized")
            })?;
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
        let policy = RuntimePolicy {
            max_frame_bytes: u32::try_from(max_frame).map_err(|_| {
                unavailable(&binding.provider_id, "capability frame limit is invalid")
            })?,
            max_output_bytes: capability.resources.max_output_bytes,
            max_memory_bytes: capability.resources.max_memory_bytes,
            max_file_bytes: capability
                .resources
                .max_input_bytes
                .max(capability.resources.max_temporary_bytes),
            request_timeout: Duration::from_millis(capability.resources.timeout_ms),
            read_only_roots: roots.clone(),
            allow_child_processes: manifest.permissions.child_processes,
            ..RuntimePolicy::default()
        };
        Ok((policy, roots))
    }

    /// Bind a manager-verified immutable process snapshot to one capability.
    pub fn new(
        process: PreparedProcessPlugin,
        manifest: &PluginManifest,
        binding: ProviderBinding,
        model_roots: Vec<PathBuf>,
    ) -> Result<Self, ConversionError> {
        if binding.plugin_id != manifest.id {
            return Err(unavailable(&binding.provider_id, "installed plugin binding changed"));
        }
        let capability = manifest
            .capabilities
            .iter()
            .find(|candidate| candidate.id == binding.capability_id)
            .ok_or_else(|| {
                unavailable(&binding.provider_id, "capability is absent from manifest")
            })?;
        if capability.provider_id != binding.provider_id {
            return Err(unavailable(&binding.provider_id, "provider identity changed"));
        }
        Ok(Self {
            process,
            binding,
            kind: capability.kind,
            resources: capability.resources.clone(),
            model_roots,
        })
    }

    /// Adapt this exact binding as OCR.
    pub fn ocr(self, options: ConversionOptions) -> Result<ProcessOcrEngine, ConversionError> {
        self.require(CapabilityKind::Ocr)?;
        Ok(ProcessOcrEngine { capability: self, options })
    }

    /// Adapt this exact binding as transcription.
    pub fn transcriber(
        self,
        options: ConversionOptions,
    ) -> Result<ProcessTranscriber, ConversionError> {
        self.require(CapabilityKind::Transcription)?;
        Ok(ProcessTranscriber { capability: self, options })
    }

    /// Adapt this exact binding as diarization.
    pub fn diarizer(self, options: ConversionOptions) -> Result<ProcessDiarizer, ConversionError> {
        self.require(CapabilityKind::Diarization)?;
        Ok(ProcessDiarizer { capability: self, options })
    }

    /// Ask the isolated provider to verify its exact model and native runtime without inference.
    pub fn verify_ready(
        &self,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        let parameters = ReadinessParameters {
            schema_version: PROTOCOL_VERSION,
            capability_id: self.binding.capability_id.clone(),
            model_bundle: self.binding.model_bundle.clone(),
            options: options.clone(),
            model_roots: self.model_roots.clone(),
        };
        let json =
            self.execute("readiness", "application/octet-stream", &[0], &parameters, context)?;
        let response: ReadinessResponse = serde_json::from_str(&json)
            .map_err(|_| unavailable(&self.binding.provider_id, "readiness response is invalid"))?;
        if response.ready {
            Ok(())
        } else {
            Err(unavailable(&self.binding.provider_id, "provider reported not ready"))
        }
    }

    fn require(&self, expected: CapabilityKind) -> Result<(), ConversionError> {
        if self.kind == expected {
            Ok(())
        } else {
            Err(unavailable(&self.binding.provider_id, "capability kind does not match adapter"))
        }
    }

    fn execute<T: Serialize>(
        &self,
        operation: &'static str,
        _media_type: &str,
        source: &[u8],
        parameters: &T,
        context: &ExecutionContext,
    ) -> Result<String, ConversionError> {
        if source.len() as u64 > self.resources.max_input_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "provider.max_input_bytes",
                detail: "provider input exceeds its signed resource envelope".into(),
            });
        }
        let parameters_json = serde_json::to_string(parameters).map_err(|_| {
            unavailable(&self.binding.provider_id, "provider request cannot be serialized")
        })?;
        let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let request_id = format!("capability-{sequence}");
        self.process
            .execute_raw(
                PluginRequest {
                    request_id: &request_id,
                    input_format: operation,
                    source_name: None,
                    parameters_json: Some(&parameters_json),
                    source,
                },
                context,
            )
            .map(|execution| execution.result_json)
            .map_err(|error| map_error(&self.binding.provider_id, &error))
    }
}

/// OCR adapter for one exact installed process capability.
#[derive(Debug)]
pub struct ProcessOcrEngine {
    capability: ProcessCapability,
    options: ConversionOptions,
}

/// Speech-transcription adapter for one exact installed process capability.
#[derive(Debug)]
pub struct ProcessTranscriber {
    capability: ProcessCapability,
    options: ConversionOptions,
}

/// Speaker-diarization adapter for one exact installed process capability.
#[derive(Debug)]
pub struct ProcessDiarizer {
    capability: ProcessCapability,
    options: ConversionOptions,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct ReadinessParameters {
    pub schema_version: u32,
    pub capability_id: String,
    pub model_bundle: Option<String>,
    pub options: ConversionOptions,
    pub model_roots: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadinessResponse {
    ready: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct OcrParameters {
    pub schema_version: u32,
    pub capability_id: String,
    pub model_bundle: Option<String>,
    pub media_type: String,
    pub languages: Vec<String>,
    pub options: ConversionOptions,
    pub model_roots: Vec<PathBuf>,
}

/// Provider-owned OCR response envelope binding the result to the signed
/// capability identity without rewriting the detector/recognizer evidence.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct OcrCapabilityResponse {
    pub schema_version: u32,
    pub capability_id: String,
    pub provider_id: String,
    pub model_bundle: Option<String>,
    pub result: BoundOcrResultDto,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct TranscriptionParameters {
    pub schema_version: u32,
    pub capability_id: String,
    pub model_bundle: Option<String>,
    pub media_type: String,
    pub language: Option<String>,
    pub options: ConversionOptions,
    pub model_roots: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct DiarizationParameters {
    pub schema_version: u32,
    pub capability_id: String,
    pub model_bundle: Option<String>,
    pub media_type: String,
    pub segments: Vec<into_markdown_core::BlockNode>,
    pub expected_speakers: Option<u16>,
    pub max_speakers: u16,
    pub options: ConversionOptions,
    pub model_roots: Vec<PathBuf>,
}

impl OcrEngine for ProcessOcrEngine {
    fn id(&self) -> &str {
        &self.capability.binding.provider_id
    }

    fn recognize<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async move {
            let OcrRecognition::Bound(result) = self.execute_bound(request, context)? else {
                return Err(unavailable(self.id(), "provider returned unbound OCR evidence"));
            };
            Ok(result.into_parts().0)
        })
    }

    fn planned_bound_output(
        &self,
        _request: OcrRequest<'_>,
        _options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        context.checkpoint()?;
        self.output_plan()
    }

    fn planned_normalized_png_output(
        &self,
        _width: u32,
        _height: u32,
        _options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        context.checkpoint()?;
        self.output_plan()
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move { self.execute_bound(request, context) })
    }
}

impl ProcessOcrEngine {
    fn output_plan(&self) -> Result<OcrOutputPlan, ConversionError> {
        let retained = self.capability.resources.max_output_bytes;
        let regions = 3_000_u32;
        let structural = u64::from(regions) * 256;
        let text = retained.saturating_sub(structural).min(16 * 1024 * 1024);
        OcrOutputPlan::try_new_with_working(
            retained,
            self.capability.resources.max_memory_bytes,
            regions,
            text,
        )
    }

    fn execute_bound(
        &self,
        request: OcrRequest<'_>,
        context: &ExecutionContext,
    ) -> Result<OcrRecognition, ConversionError> {
        let parameters = OcrParameters {
            schema_version: PROTOCOL_VERSION,
            capability_id: self.capability.binding.capability_id.clone(),
            model_bundle: self.capability.binding.model_bundle.clone(),
            media_type: request.media_type.to_owned(),
            languages: request.languages.iter().map(|value| (*value).to_owned()).collect(),
            options: self.options.clone(),
            model_roots: self.capability.model_roots.clone(),
        };
        let json = self.capability.execute(
            "ocr",
            request.media_type,
            request.image,
            &parameters,
            context,
        )?;
        let response: OcrCapabilityResponse = serde_json::from_str(&json)
            .map_err(|_| unavailable(self.id(), "provider returned invalid OCR JSON"))?;
        if response.schema_version != PROTOCOL_VERSION
            || response.capability_id != self.capability.binding.capability_id
            || response.provider_id != self.id()
            || response.model_bundle != self.capability.binding.model_bundle
        {
            return Err(unavailable(
                self.id(),
                "OCR provider identity does not match its manifest",
            ));
        }
        let result = BoundOcrResult::try_from_dto(response.result)?;
        let identity = result
            .input_identity()
            .ok_or_else(|| unavailable(self.id(), "OCR provider omitted input identity"))?;
        if identity.sha256() != Sha256::digest(request.image).as_slice() {
            return Err(unavailable(self.id(), "OCR result is bound to a different input"));
        }
        Ok(OcrRecognition::Bound(result))
    }
}

impl Transcriber for ProcessTranscriber {
    fn id(&self) -> &str {
        &self.capability.binding.provider_id
    }

    fn transcribe<'a>(
        &'a self,
        request: TranscriptionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
        Box::pin(async move {
            let _working_memory =
                context.reserve_memory(self.capability.resources.max_memory_bytes)?;
            let parameters = TranscriptionParameters {
                schema_version: PROTOCOL_VERSION,
                capability_id: self.capability.binding.capability_id.clone(),
                model_bundle: self.capability.binding.model_bundle.clone(),
                media_type: request.media_type.to_owned(),
                language: request.language.map(str::to_owned),
                options: self.options.clone(),
                model_roots: self.capability.model_roots.clone(),
            };
            let json = self.capability.execute(
                "transcription",
                request.media_type,
                request.media,
                &parameters,
                context,
            )?;
            let result: TranscriptionResult = serde_json::from_str(&json).map_err(|_| {
                unavailable(self.id(), "provider returned invalid transcription JSON")
            })?;
            validate_media_result(self.id(), &result.provider, &result.model, &result.segments)?;
            if result
                .language_confidence
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            {
                return Err(unavailable(self.id(), "language confidence is invalid"));
            }
            Ok(result)
        })
    }
}

impl Diarizer for ProcessDiarizer {
    fn id(&self) -> &str {
        &self.capability.binding.provider_id
    }

    fn diarize<'a>(
        &'a self,
        request: DiarizationRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<DiarizationResult, ConversionError>> {
        Box::pin(async move {
            let _working_memory =
                context.reserve_memory(self.capability.resources.max_memory_bytes)?;
            let parameters = DiarizationParameters {
                schema_version: PROTOCOL_VERSION,
                capability_id: self.capability.binding.capability_id.clone(),
                model_bundle: self.capability.binding.model_bundle.clone(),
                media_type: request.media_type.to_owned(),
                segments: request.segments.to_vec(),
                expected_speakers: request.expected_speakers,
                max_speakers: request.max_speakers,
                options: self.options.clone(),
                model_roots: self.capability.model_roots.clone(),
            };
            let json = self.capability.execute(
                "diarization",
                request.media_type,
                request.media,
                &parameters,
                context,
            )?;
            let result: DiarizationResult = serde_json::from_str(&json).map_err(|_| {
                unavailable(self.id(), "provider returned invalid diarization JSON")
            })?;
            validate_media_result(self.id(), &result.provider, &result.model, &result.segments)?;
            Ok(result)
        })
    }
}

fn validate_media_result(
    expected_provider: &str,
    provider: &str,
    model: &str,
    segments: &[into_markdown_core::BlockNode],
) -> Result<(), ConversionError> {
    if provider != expected_provider || model.trim().is_empty() || model.trim() != model {
        return Err(unavailable(expected_provider, "provider or model identity is invalid"));
    }
    if segments.iter().any(|node| !matches!(node.block, Block::TimedSegment { .. })) {
        return Err(unavailable(expected_provider, "media provider returned a non-timed node"));
    }
    let mut document = Document::default();
    document.blocks = segments.to_vec();
    document
        .validate()
        .map_err(|_| unavailable(expected_provider, "media provider returned invalid IR"))
}

fn map_error(provider: &str, error: &PluginError) -> ConversionError {
    match error.code {
        PluginErrorCode::Cancelled => ConversionError::Cancelled,
        PluginErrorCode::Timeout => ConversionError::Timeout,
        PluginErrorCode::ResourceLimit | PluginErrorCode::FrameTooLarge => {
            ConversionError::ResourceLimit { limit: "provider", detail: error.detail.clone() }
        }
        _ => unavailable(provider, &error.detail),
    }
}

fn unavailable(provider: &str, detail: &str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: provider.to_owned(),
        detail: detail.to_owned(),
    }
}
