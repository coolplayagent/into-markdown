use crate::{
    Asset, BlockNode, ConversionError, ConversionOptions, Diagnostic, Document, FormatCandidate,
    FormatHint, InputFormat, InputRef, Provenance, ResolvedInput,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Sendable boxed future used to keep service-provider traits object safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Complete conversion request.
#[derive(Debug, Clone)]
pub struct ConversionRequest {
    /// Source to resolve.
    pub input: InputRef,
    /// Optional format hints.
    pub hint: FormatHint,
    /// Pipeline policy.
    pub options: ConversionOptions,
}

/// Request to resolve and detect an input without converting it.
#[derive(Debug, Clone)]
pub struct DetectionRequest {
    /// Source to resolve.
    pub input: InputRef,
    /// Optional format hints.
    pub hint: FormatHint,
    /// Source, network, and resource policy.
    pub options: ConversionOptions,
}

impl DetectionRequest {
    /// Construct a detection request with safe offline defaults.
    #[must_use]
    pub fn new(input: InputRef) -> Self {
        Self { input, hint: FormatHint::default(), options: ConversionOptions::default() }
    }
}

/// Format hypotheses and safe source metadata returned by detection.
#[derive(Debug, Clone)]
pub struct DetectionResult {
    /// Metadata produced by the selected source resolver.
    pub source: crate::SourceMetadata,
    /// Ordered format candidates.
    pub candidates: Vec<FormatCandidate>,
}

impl ConversionRequest {
    /// Construct a request with safe offline defaults.
    #[must_use]
    pub fn new(input: InputRef) -> Self {
        Self { input, hint: FormatHint::default(), options: ConversionOptions::default() }
    }
}

/// Final conversion result.
#[derive(Debug, Clone)]
pub struct ConversionResult {
    /// Structured document before rendering.
    pub document: Document,
    /// GitHub-Flavored Markdown.
    pub markdown: String,
    /// Embedded/external resources.
    pub assets: Vec<Asset>,
    /// Non-fatal diagnostics.
    pub diagnostics: Vec<Diagnostic>,
    /// Ordered material provenance records for auditing.
    pub provenance: Vec<Provenance>,
}

/// Resolve one source class into bounded in-memory bytes.
pub trait SourceResolver: Send + Sync {
    /// Stable implementation ID.
    fn id(&self) -> &'static str;
    /// Whether this resolver handles the source shape.
    fn supports(&self, input: &InputRef) -> bool;
    /// Resolve the source while enforcing request policy.
    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>>;
}

/// Produce format hypotheses from bytes, metadata, and explicit hints.
pub trait FormatDetector: Send + Sync {
    /// Stable implementation ID.
    fn id(&self) -> &'static str;
    /// Detector priority; larger values run first.
    fn priority(&self) -> i32 {
        0
    }
    /// Detect zero or more candidates.
    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        hint: &'a FormatHint,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>>;
}

/// Result of a cheap converter applicability probe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProbeOutcome {
    /// The converter does not apply; registry fallback may continue.
    NotApplicable,
    /// The converter applies with the supplied confidence.
    Match {
        /// Converter-specific confidence in the inclusive range `0.0..=1.0`.
        confidence: f32,
    },
}

/// Output produced by a format converter before Markdown rendering.
#[derive(Debug, Clone, Default)]
pub struct ConverterOutput {
    /// Unified document IR.
    pub document: Document,
    /// Extracted assets.
    pub assets: Vec<Asset>,
    /// Recoveries and scoped failures.
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse one or more source formats into the unified IR.
pub trait Converter: Send + Sync {
    /// Stable implementation ID, also used as deterministic tie breaker.
    fn id(&self) -> &'static str;
    /// Registry priority; larger values are attempted first after confidence.
    fn priority(&self) -> i32 {
        0
    }
    /// Formats implemented by this converter.
    fn supported_formats(&self) -> &'static [InputFormat];
    /// Cheap applicability check that must not perform full conversion.
    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>>;
    /// Convert a confirmed input. Any error is authoritative and stops
    /// fallback; only `ProbeOutcome::NotApplicable` permits the next attempt.
    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        services: &'a Services,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>>;
}

/// Render the unified IR through a single Markdown policy.
pub trait MarkdownRenderer: Send + Sync {
    /// Stable renderer ID.
    fn id(&self) -> &'static str;
    /// Render a document and its asset inventory.
    fn render<'a>(
        &'a self,
        document: &'a Document,
        assets: &'a [Asset],
        options: &'a ConversionOptions,
    ) -> BoxFuture<'a, Result<String, ConversionError>>;
}

/// OCR request over one decoded image.
#[derive(Debug, Clone, Copy)]
pub struct OcrRequest<'a> {
    /// Encoded image bytes.
    pub image: &'a [u8],
    /// MIME media type.
    pub media_type: &'a str,
    /// Optional language hints such as `zh-Hans`, `zh-Hant`, and `en`.
    pub languages: &'a [&'a str],
}

/// One spatial OCR result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrRegion {
    /// Recognized text.
    pub text: String,
    /// Quadrilateral corners in clockwise source coordinates.
    pub polygon: [(f32, f32); 4],
    /// Recognition confidence.
    pub confidence: f32,
}

/// OCR output before merging into the document IR.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OcrResult {
    /// Ordered recognized regions.
    pub regions: Vec<OcrRegion>,
    /// Provider/model ID.
    pub provider: String,
}

/// Native or remote OCR implementation.
pub trait OcrEngine: Send + Sync {
    /// Stable provider ID.
    fn id(&self) -> &'static str;
    /// Recognize text and geometry.
    fn recognize<'a>(
        &'a self,
        request: OcrRequest<'a>,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>>;
}

/// Audio transcription request.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptionRequest<'a> {
    /// Encoded media bytes.
    pub media: &'a [u8],
    /// MIME media type.
    pub media_type: &'a str,
    /// Optional BCP-47 language hint.
    pub language: Option<&'a str>,
}

/// Time-aligned transcription result represented as IR nodes.
#[derive(Debug, Clone, Default)]
pub struct TranscriptionResult {
    /// Ordered timed segment nodes.
    pub segments: Vec<BlockNode>,
    /// Provider/model ID.
    pub provider: String,
}

/// Local or remote speech-to-text provider.
pub trait Transcriber: Send + Sync {
    /// Stable provider ID.
    fn id(&self) -> &'static str;
    /// Transcribe media.
    fn transcribe<'a>(
        &'a self,
        request: TranscriptionRequest<'a>,
    ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>>;
}

/// Optional AI operation exposed by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AiCapability {
    /// Vision OCR.
    VisionOcr,
    /// Image description.
    ImageDescription,
    /// Layout repair.
    LayoutRepair,
    /// Table repair.
    TableRepair,
    /// Formula repair.
    FormulaRepair,
    /// Audio transcription.
    AudioTranscription,
    /// Markdown post-processing.
    MarkdownPostprocess,
}

/// Borrowed input to an AI operation.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum AiInput<'a> {
    /// Encoded image.
    Image {
        /// Encoded image bytes.
        bytes: &'a [u8],
        /// MIME media type.
        media_type: &'a str,
    },
    /// Structured document.
    Document(&'a Document),
    /// Markdown text.
    Markdown(&'a str),
    /// Encoded audio/video media.
    Media {
        /// Encoded media bytes.
        bytes: &'a [u8],
        /// MIME media type.
        media_type: &'a str,
    },
}

/// AI operation request.
#[derive(Debug, Clone, Copy)]
pub struct AiRequest<'a> {
    /// Required capability.
    pub capability: AiCapability,
    /// Typed input.
    pub input: AiInput<'a>,
    /// Optional user-controlled prompt suffix.
    pub prompt: Option<&'a str>,
}

/// Versioned, validated changes an AI provider may propose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentPatch {
    /// Protocol version. The initial contract is `1`.
    pub version: u32,
    /// Ordered patch operations.
    pub operations: Vec<PatchOperation>,
}

/// Allowed structured IR edits. Raw provider-specific mutation is forbidden.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PatchOperation {
    /// Append new nodes at the document root.
    Append {
        /// Nodes to append at document root.
        nodes: Vec<BlockNode>,
    },
    /// Replace one node while retaining an auditable target ID.
    Replace {
        /// Existing node ID to replace.
        target: crate::NodeId,
        /// Replacement nodes with AI provenance.
        nodes: Vec<BlockNode>,
    },
}

impl DocumentPatch {
    /// Validate the wire version and basic structural invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::Ai`] when the patch protocol version or an
    /// operation is not accepted by this library version.
    pub fn validate(&self) -> Result<(), ConversionError> {
        if self.version != 1 {
            return Err(ConversionError::Ai {
                provider: "patch-validator".into(),
                detail: format!("unsupported document patch version {}", self.version),
            });
        }
        Ok(())
    }
}

/// Structured output returned by an AI provider.
#[derive(Debug, Clone, Default)]
pub struct AiOutput {
    /// Provider-created nodes with AI provenance.
    pub nodes: Vec<BlockNode>,
    /// Optional structured patch.
    pub patch: Option<DocumentPatch>,
    /// Non-fatal diagnostics.
    pub diagnostics: Vec<Diagnostic>,
}

/// Capability-negotiated LLM or multimodal provider.
pub trait AiProvider: Send + Sync {
    /// Stable provider ID.
    fn id(&self) -> &'static str;
    /// Capabilities available under current configuration.
    fn capabilities(&self) -> BTreeSet<AiCapability>;
    /// Execute one explicitly enabled capability.
    fn execute<'a>(
        &'a self,
        request: AiRequest<'a>,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>>;
}

/// Minimal tensor exchange type for inference runtimes.
#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    /// Row-major dimensions.
    pub shape: Vec<usize>,
    /// Float data. Quantized runtimes adapt at the provider boundary.
    pub values: Vec<f32>,
}

/// Model-runtime seam used by local OCR without coupling the OCR API to ORT.
pub trait TensorRuntime: Send + Sync {
    /// Stable runtime ID.
    fn id(&self) -> &'static str;
    /// Execute a named model with ordered input tensors.
    fn run<'a>(
        &'a self,
        model_id: &'a str,
        inputs: &'a [Tensor],
    ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>>;
}

/// Optional services made available to converters.
#[derive(Clone, Default)]
pub struct Services {
    /// OCR implementation.
    pub ocr: Option<Arc<dyn OcrEngine>>,
    /// Speech transcription implementation.
    pub transcriber: Option<Arc<dyn Transcriber>>,
    /// AI provider.
    pub ai: Option<Arc<dyn AiProvider>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_object_safe() {
        let _: Option<&dyn SourceResolver> = None;
        let _: Option<&dyn FormatDetector> = None;
        let _: Option<&dyn Converter> = None;
        let _: Option<&dyn MarkdownRenderer> = None;
        let _: Option<&dyn OcrEngine> = None;
        let _: Option<&dyn Transcriber> = None;
        let _: Option<&dyn AiProvider> = None;
        let _: Option<&dyn TensorRuntime> = None;
    }

    #[test]
    fn service_provider_interfaces_are_object_safe() {
        assert_object_safe();
    }

    #[test]
    fn document_patch_rejects_unknown_versions() {
        let patch = DocumentPatch { version: 2, operations: vec![] };
        assert_eq!(patch.validate().unwrap_err().code(), crate::ErrorCode::Ai);
    }
}
