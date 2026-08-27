use crate::{CapabilityKind, PluginManifest, ProviderBinding, ResourceEnvelope};
use base64::Engine as _;
use into_markdown_core::{
    Block, BoundOcrResult, BoundOcrResultDto, BoxFuture, ConversionError, ConversionOptions,
    DiarizationRequest, DiarizationResult, Diarizer, Document, ExecutionContext, InputFormat,
    LegacyOfficeNormalizer, LegacyOfficeRequest, LegacyOfficeResult, OcrEngine, OcrOutputPlan,
    OcrRecognition, OcrRequest, OcrResult, Transcriber, TranscriptionRequest, TranscriptionResult,
};
use into_markdown_plugin_manager::PreparedProcessPlugin;
use into_markdown_process_plugin::{PluginError, PluginErrorCode, PluginRequest, RuntimePolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const PROTOCOL_VERSION: u32 = 1;
const FRAME_OVERHEAD: u64 = 1024 * 1024;
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;
// RLIMIT_AS accounts every mapping in the provider coordinator, not only
// resident capability data. A provider that is authorized to launch an
// authenticated helper therefore needs fixed virtual-address headroom while
// the helper installs its own model-derived hard limit before loading ORT.
const LINUX_CHILD_COORDINATOR_ADDRESS_SPACE_BYTES: u64 = 512 * 1024 * 1024;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn process_address_space_limit(
    declared_memory: u64,
    allow_child_processes: bool,
) -> Result<u64, ConversionError> {
    if cfg!(target_os = "linux") && allow_child_processes {
        declared_memory
            .checked_add(LINUX_CHILD_COORDINATOR_ADDRESS_SPACE_BYTES)
            .ok_or_else(|| unavailable("process-v1", "provider address-space limit overflowed"))
    } else {
        Ok(declared_memory)
    }
}

/// Authenticated process runtime bound to one self-contained installed capability.
pub struct ProcessCapability {
    process: PreparedProcessPlugin,
    binding: ProviderBinding,
    kind: CapabilityKind,
    resources: ResourceEnvelope,
}

impl std::fmt::Debug for ProcessCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessCapability")
            .field("binding", &self.binding)
            .field("kind", &self.kind)
            .field("resources", &self.resources)
            .finish_non_exhaustive()
    }
}

impl ProcessCapability {
    /// Build the sandbox policy declared by one signed capability manifest.
    ///
    /// # Errors
    ///
    /// Returns an unavailable-component error when the binding or resource envelope is invalid.
    pub fn runtime_policy(
        manifest: &PluginManifest,
        binding: &ProviderBinding,
    ) -> Result<RuntimePolicy, ConversionError> {
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
        let policy = RuntimePolicy {
            max_frame_bytes: u32::try_from(max_frame).map_err(|_| {
                unavailable(&binding.provider_id, "capability frame limit is invalid")
            })?,
            max_output_bytes: capability.resources.max_output_bytes,
            max_memory_bytes: process_address_space_limit(
                capability.resources.max_memory_bytes,
                manifest.permissions.child_processes,
            )?,
            max_file_bytes: capability
                .resources
                .max_input_bytes
                .max(capability.resources.max_temporary_bytes),
            request_timeout: Duration::from_millis(capability.resources.timeout_ms),
            max_open_files: if manifest.permissions.child_processes { 1024 } else { 64 },
            read_only_roots: Vec::new(),
            allow_child_processes: manifest.permissions.child_processes,
            macos_compatibility_child: CapabilityKind::LegacyOffice == capability.kind,
            ..RuntimePolicy::default()
        };
        Ok(policy)
    }

    /// Bind a manager-verified immutable process snapshot to one capability.
    ///
    /// # Errors
    ///
    /// Returns an unavailable-component error when the binding is absent or has changed.
    pub fn new(
        process: PreparedProcessPlugin,
        manifest: &PluginManifest,
        binding: ProviderBinding,
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
        })
    }

    /// Adapt this exact binding as OCR.
    ///
    /// # Errors
    ///
    /// Returns an unavailable-component error when the binding is not an OCR capability.
    pub fn ocr(self, options: ConversionOptions) -> Result<ProcessOcrEngine, ConversionError> {
        self.require(CapabilityKind::Ocr)?;
        Ok(ProcessOcrEngine { capability: self, options })
    }

    /// Adapt this exact binding as transcription.
    ///
    /// # Errors
    ///
    /// Returns an unavailable-component error when the binding is not transcription.
    pub fn transcriber(
        self,
        options: ConversionOptions,
    ) -> Result<ProcessTranscriber, ConversionError> {
        self.require(CapabilityKind::Transcription)?;
        Ok(ProcessTranscriber { capability: self, options })
    }

    /// Adapt this exact binding as diarization.
    ///
    /// # Errors
    ///
    /// Returns an unavailable-component error when the binding is not diarization.
    pub fn diarizer(self, options: ConversionOptions) -> Result<ProcessDiarizer, ConversionError> {
        self.require(CapabilityKind::Diarization)?;
        Ok(ProcessDiarizer { capability: self, options })
    }

    /// Adapt this exact binding as legacy Office normalization.
    ///
    /// # Errors
    ///
    /// Returns an unavailable-component error when the binding is not legacy Office.
    pub fn legacy_office(self) -> Result<ProcessLegacyOfficeNormalizer, ConversionError> {
        self.require(CapabilityKind::LegacyOffice)?;
        Ok(ProcessLegacyOfficeNormalizer { capability: self })
    }

    /// Ask the isolated provider to verify its exact model and native runtime without inference.
    ///
    /// # Errors
    ///
    /// Returns a conversion error when sandbox execution or readiness validation fails.
    pub fn verify_ready(
        &self,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        let parameters = ReadinessParameters {
            schema_version: PROTOCOL_VERSION,
            capability_id: self.binding.capability_id.clone(),
            options: options.clone(),
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

/// Legacy Office adapter for one exact installed process capability.
#[derive(Debug)]
pub struct ProcessLegacyOfficeNormalizer {
    capability: ProcessCapability,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct ReadinessParameters {
    pub schema_version: u32,
    pub capability_id: String,
    pub options: ConversionOptions,
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
    pub media_type: String,
    pub languages: Vec<String>,
    pub options: ConversionOptions,
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
    pub result: BoundOcrResultDto,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct TranscriptionParameters {
    pub schema_version: u32,
    pub capability_id: String,
    pub media_type: String,
    pub language: Option<String>,
    pub options: ConversionOptions,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct DiarizationParameters {
    pub schema_version: u32,
    pub capability_id: String,
    pub media_type: String,
    pub segments: Vec<into_markdown_core::BlockNode>,
    pub expected_speakers: Option<u16>,
    pub max_speakers: u16,
    pub options: ConversionOptions,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct LegacyOfficeParameters {
    pub schema_version: u32,
    pub capability_id: String,
    pub source_format: InputFormat,
    pub maximum_output_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(missing_docs)]
pub struct LegacyOfficeCapabilityResponse {
    pub schema_version: u32,
    pub capability_id: String,
    pub provider_id: String,
    pub bytes_base64: String,
    pub format: InputFormat,
    pub version: String,
    pub artifact_sha256: String,
    pub target: String,
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
            media_type: request.media_type.to_owned(),
            languages: request.languages.iter().map(|value| (*value).to_owned()).collect(),
            options: self.options.clone(),
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
            // The process sandbox independently enforces the signed child-memory ceiling. The
            // host request budget accounts for the bounded response retained in this process;
            // charging the child's entire ceiling here both double-counted it and made a valid
            // self-contained Speech plugin impossible under the default host budget.
            let _result_memory =
                context.reserve_memory(self.capability.resources.max_output_bytes)?;
            let parameters = TranscriptionParameters {
                schema_version: PROTOCOL_VERSION,
                capability_id: self.capability.binding.capability_id.clone(),
                media_type: request.media_type.to_owned(),
                language: request.language.map(str::to_owned),
                options: self.options.clone(),
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
            let _result_memory =
                context.reserve_memory(self.capability.resources.max_output_bytes)?;
            let parameters = DiarizationParameters {
                schema_version: PROTOCOL_VERSION,
                capability_id: self.capability.binding.capability_id.clone(),
                media_type: request.media_type.to_owned(),
                segments: request.segments.to_vec(),
                expected_speakers: request.expected_speakers,
                max_speakers: request.max_speakers,
                options: self.options.clone(),
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
            validate_diarization_result(
                self.id(),
                &result.provider,
                &result.model,
                request.segments,
                &result.segments,
            )?;
            Ok(result)
        })
    }
}

impl LegacyOfficeNormalizer for ProcessLegacyOfficeNormalizer {
    fn id(&self) -> &str {
        &self.capability.binding.provider_id
    }

    fn normalize<'a>(
        &'a self,
        request: LegacyOfficeRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<LegacyOfficeResult, ConversionError>> {
        Box::pin(async move {
            let parameters = LegacyOfficeParameters {
                schema_version: PROTOCOL_VERSION,
                capability_id: self.capability.binding.capability_id.clone(),
                source_format: request.source_format,
                maximum_output_bytes: request.maximum_output_bytes,
            };
            let json = self.capability.execute(
                "legacy-office",
                "application/x-ole-storage",
                request.document,
                &parameters,
                context,
            )?;
            let response: LegacyOfficeCapabilityResponse =
                serde_json::from_str(&json).map_err(|_| {
                    unavailable(self.id(), "provider returned invalid legacy Office JSON")
                })?;
            if response.schema_version != PROTOCOL_VERSION
                || response.capability_id != self.capability.binding.capability_id
                || response.provider_id != self.id()
            {
                return Err(unavailable(
                    self.id(),
                    "legacy Office provider identity does not match its manifest",
                ));
            }
            let bytes =
                base64::engine::general_purpose::STANDARD.decode(&response.bytes_base64).map_err(
                    |_| unavailable(self.id(), "normalized Office payload is not canonical Base64"),
                )?;
            if base64::engine::general_purpose::STANDARD.encode(&bytes) != response.bytes_base64
                || bytes.len() as u64 > request.maximum_output_bytes
                || bytes.len() as u64 > self.capability.resources.max_output_bytes
            {
                return Err(ConversionError::ResourceLimit {
                    limit: "provider.max_output_bytes",
                    detail: "normalized Office payload exceeds its signed output envelope".into(),
                });
            }
            let memory = context.reserve_memory(bytes.len() as u64)?;
            Ok(LegacyOfficeResult {
                bytes: bytes.into_boxed_slice(),
                format: response.format,
                provider: response.provider_id,
                version: response.version,
                artifact_sha256: response.artifact_sha256,
                target: response.target,
                memory,
            })
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
    let model_provenance = format!("{expected_provider}/{model}");
    if segments.iter().any(|node| {
        !matches!(node.block, Block::TimedSegment { .. })
            || node.provenance.kind != into_markdown_core::ProvenanceKind::AiProvider
            || (node.provenance.provider != expected_provider
                && node.provenance.provider != model_provenance)
    }) {
        return Err(unavailable(
            expected_provider,
            "media provider returned an invalid timed node or provenance identity",
        ));
    }
    let document = Document { blocks: segments.to_vec(), ..Document::default() };
    document
        .validate()
        .map_err(|_| unavailable(expected_provider, "media provider returned invalid IR"))
}

fn validate_diarization_result(
    expected_provider: &str,
    provider: &str,
    model: &str,
    input: &[into_markdown_core::BlockNode],
    output: &[into_markdown_core::BlockNode],
) -> Result<(), ConversionError> {
    if provider != expected_provider || model.trim().is_empty() || model.trim() != model {
        return Err(unavailable(expected_provider, "provider or model identity is invalid"));
    }
    let source_providers = input
        .iter()
        .filter_map(|node| match node.block {
            Block::TimedSegment { .. }
                if node.provenance.kind == into_markdown_core::ProvenanceKind::AiProvider =>
            {
                Some(node.provenance.provider.as_str())
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    if source_providers.is_empty()
        || output.iter().any(|node| {
            !matches!(node.block, Block::TimedSegment { .. })
                || node.provenance.kind != into_markdown_core::ProvenanceKind::AiProvider
                || !source_providers.contains(node.provenance.provider.as_str())
        })
    {
        return Err(unavailable(
            expected_provider,
            "diarization output does not preserve transcription provenance",
        ));
    }

    let token_evidence = |nodes: &[into_markdown_core::BlockNode]| {
        nodes
            .iter()
            .flat_map(|node| match &node.block {
                Block::TimedSegment { tokens, .. } => tokens
                    .iter()
                    .map(|token| (token.range, token.text.clone(), token.confidence))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>()
    };
    let input_tokens = token_evidence(input);
    if input_tokens.is_empty() {
        let strip_speakers = |nodes: &[into_markdown_core::BlockNode]| {
            nodes
                .iter()
                .cloned()
                .map(|mut node| {
                    if let Block::TimedSegment { speaker, speaker_confidence, tokens, .. } =
                        &mut node.block
                    {
                        *speaker = None;
                        *speaker_confidence = None;
                        for token in tokens {
                            token.speaker = None;
                            token.speaker_confidence = None;
                        }
                    }
                    node
                })
                .collect::<Vec<_>>()
        };
        if strip_speakers(input) != strip_speakers(output) {
            return Err(unavailable(
                expected_provider,
                "diarization output changed transcript content or timing",
            ));
        }
    } else if input_tokens != token_evidence(output) {
        return Err(unavailable(
            expected_provider,
            "diarization output changed token text, timing, confidence, or order",
        ));
    }

    let document = Document { blocks: output.to_vec(), ..Document::default() };
    document
        .validate()
        .map_err(|_| unavailable(expected_provider, "diarization provider returned invalid IR"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        BlockNode, Inline, NodeId, Provenance, ProvenanceKind, SourceLocator, TimeRange,
    };

    fn segment(provenance_provider: &str) -> BlockNode {
        let range = TimeRange { start_ms: 0, end_ms: 1000 };
        BlockNode {
            id: NodeId("segment-1".into()),
            block: Block::TimedSegment {
                range,
                speaker: None,
                speaker_confidence: None,
                tokens: Vec::new(),
                content: vec![Inline::Text { value: "hello".into(), marks: Vec::new() }],
            },
            provenance: Provenance {
                kind: ProvenanceKind::AiProvider,
                provider: provenance_provider.into(),
                locator: SourceLocator { time: Some(range), ..SourceLocator::default() },
                confidence: Some(0.9),
            },
        }
    }

    #[test]
    fn child_provider_address_space_keeps_declared_memory_separate() {
        let declared = 768 * 1024 * 1024;
        let expected = if cfg!(target_os = "linux") {
            declared + LINUX_CHILD_COORDINATOR_ADDRESS_SPACE_BYTES
        } else {
            declared
        };
        assert_eq!(process_address_space_limit(declared, true).unwrap(), expected);
        assert_eq!(process_address_space_limit(declared, false).unwrap(), declared);
        if cfg!(target_os = "linux") {
            assert!(process_address_space_limit(u64::MAX, true).is_err());
        }
    }

    #[test]
    fn media_validation_accepts_provider_and_provider_model_provenance_only() {
        assert!(
            validate_media_result(
                "provider.local",
                "provider.local",
                "model",
                &[segment("provider.local")]
            )
            .is_ok()
        );
        assert!(
            validate_media_result(
                "provider.local",
                "provider.local",
                "model",
                &[segment("provider.local/model")]
            )
            .is_ok()
        );
        assert!(
            validate_media_result(
                "provider.local",
                "provider.local",
                "model",
                &[segment("provider.other/model")]
            )
            .is_err()
        );
    }

    #[test]
    fn diarization_validation_preserves_the_transcriber_as_material_provenance() {
        let input = vec![segment("provider.transcriber/model")];
        let mut output = input.clone();
        let Block::TimedSegment { speaker, speaker_confidence, .. } = &mut output[0].block else {
            unreachable!();
        };
        *speaker = Some("speaker-1".into());
        *speaker_confidence = Some(0.9);
        assert!(
            validate_diarization_result(
                "provider.diarizer",
                "provider.diarizer",
                "speaker-model",
                &input,
                &output,
            )
            .is_ok()
        );

        output[0].provenance.provider = "provider.forged/model".into();
        assert!(
            validate_diarization_result(
                "provider.diarizer",
                "provider.diarizer",
                "speaker-model",
                &input,
                &output,
            )
            .is_err()
        );
    }
}
