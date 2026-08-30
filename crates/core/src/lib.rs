//! Stable contracts and provenance-aware intermediate representation for
//! `into-markdown`.
//!
//! This crate deliberately contains no format parser, network client, model
//! runtime, or provider implementation. Implementations depend on this crate;
//! the dependency never points in the opposite direction.

mod dto;
mod error;
mod execution;
mod format;
mod input;
mod ir;
mod media_checkpoint;
mod nested;
mod ocr_binding;
mod options;
mod result_policy;
mod spi;
mod stream;
mod summary;

pub use dto::{
    AssetDto, BUNDLE_SCHEMA_VERSION, BatchItemDto, BatchItemOutcome, BatchItemStatus,
    BatchLimitDto, BatchReportDto, BundleAssetDto, BundleManifestDto, DTO_SCHEMA_VERSION,
    DiagnosticDto, DiagnosticSeverityDto, DiagnosticsDto, DtoError, DtoErrorCode, DtoJsonStyle,
    DtoLimits, MAX_DTO_ASSETS, MAX_DTO_BASE64_BYTES, MAX_DTO_BATCH_ITEMS, MAX_DTO_DEPTH,
    MAX_DTO_DIAGNOSTICS, MAX_DTO_JSON_BYTES, MAX_DTO_PROVENANCE, MAX_DTO_STRING_BYTES,
    MAX_DTO_TOTAL_STRING_BYTES, MAX_DTO_VALUES, ProvenanceDto, ProvenanceKindDto,
    ProvenanceListDto, ResultDto, canonical_external_asset_uri,
};
pub use error::{ConversionError, ErrorCode};
pub use execution::{
    CancellationToken, CheckedFuture, ExecutionContext, ExecutionOptions, ExecutionStage,
    PreflightMemoryCredit, ProgressEvent, ProgressListener, ResourceReservation, TemporaryFile,
};
pub use format::{FormatCandidate, InputFormat};
pub use input::{
    FormatHint, InputRef, ResolvedInput, ResolvedSource, SourceMetadata, SourceRedirect,
    SourceResolutionMetadata,
};
pub use ir::{
    Asset, AssetId, Block, BlockNode, Cell, CellRef, DOCUMENT_SCHEMA_VERSION, Diagnostic,
    DiagnosticSeverity, Document, DocumentMetadata, Inline, InlineMark, IrError, IrErrorCode,
    ListItem, ListKind, MAX_DOCUMENT_DEPTH, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_JSON_BYTES,
    MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS, NodeId, OcrEvidence, OcrEvidenceStage, OcrEvidenceStep,
    OcrSourceRegion, Provenance, ProvenanceKind, Rect, SourceLocator, SourcePoint, TableAlignment,
    TableRow, TimeRange, TimedToken, ValidationLimits,
};
pub use media_checkpoint::{
    MEDIA_CHECKPOINT_SCHEMA_VERSION, MediaCheckpoint, MediaCheckpointBackend, MediaCheckpointStage,
    MediaSpeakerCluster, NormalizedAudioIdentity, RecoveredMediaCheckpoint,
};
pub use nested::{NestedConversionRequest, NestedConversionService};
pub use ocr_binding::{
    BoundOcrResult, BoundOcrResultDto, OcrInputIdentity, OcrInputIdentityDto, OcrOutputPlan,
    OcrRecognition, OcrRegion, OcrResult,
};
pub use options::{
    AiMode, AiOptions, ArchiveOptions, AsrOptions, AssetMode, ChineseScript, ConversionOptions,
    DelimitedTextOptions, DiarizationOptions, ErrorPolicy, NetworkOptions, OcrOptions, OcrPolicy,
    OutputOptions, RaggedRowsMode, ResourceLimits, TableHeaderMode, TextDecodingMode, TextOptions,
};
pub use result_policy::{
    ASSET_ONLY_REASON_CODE, EMPTY_SOURCE_REASON_CODE, ResultContent, SourceContentEvidence,
    classify_result, conversion_outcome, markdown_has_visible_content,
};
#[doc(hidden)]
pub use result_policy::{document_is_asset_only, document_is_empty};
pub use spi::{
    AiCapability, AiInput, AiOutput, AiProvider, AiRequest, ArtifactSink, AssetStreamInfo,
    BoxFuture, ConversionOutcome, ConversionRequest, ConversionResult, ConversionSummary,
    Converter, ConverterOutput, DetectionRequest, DetectionResult, DiarizationRequest,
    DiarizationResult, Diarizer, DocumentPatch, EnrichmentPlan, FormatDetector,
    LegacyOfficeNormalizer, LegacyOfficeRequest, LegacyOfficeResult, MarkdownRenderer, OcrEngine,
    OcrRequest, OutputEnricher, PatchOperation, ProbeOutcome, Services, SourceResolver, Tensor,
    TensorRuntime, Transcriber, TranscriptionRequest, TranscriptionResult,
};
#[doc(hidden)]
pub use spi::{
    estimate_retained_blocks, estimate_retained_output, estimate_retained_result,
    estimate_validation_working_set,
};
pub use stream::{
    ConverterEventSink, ConverterStream, ConverterStreamCompletion, ConverterStreamMode,
    LocalBoxFuture, StreamConsumerKind, stream_converter_output,
};
