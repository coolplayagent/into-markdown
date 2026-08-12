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
mod options;
mod spi;

pub use dto::{
    AssetDto, BatchItemDto, BatchItemStatus, BatchReportDto, BundleAssetDto, BundleManifestDto,
    DTO_SCHEMA_VERSION, DiagnosticDto, DiagnosticSeverityDto, DiagnosticsDto, DtoError,
    DtoErrorCode, DtoJsonStyle, DtoLimits, MAX_DTO_ASSETS, MAX_DTO_BASE64_BYTES,
    MAX_DTO_BATCH_ITEMS, MAX_DTO_DEPTH, MAX_DTO_DIAGNOSTICS, MAX_DTO_JSON_BYTES,
    MAX_DTO_PROVENANCE, MAX_DTO_STRING_BYTES, MAX_DTO_TOTAL_STRING_BYTES, MAX_DTO_VALUES,
    ProvenanceDto, ProvenanceKindDto, ProvenanceListDto, ResultDto,
};
pub use error::{ConversionError, ErrorCode};
pub use execution::{
    CancellationToken, CheckedFuture, ExecutionContext, ExecutionOptions, ExecutionStage,
    ProgressEvent, ProgressListener, ResourceReservation, TemporaryFile,
};
pub use format::{FormatCandidate, InputFormat};
pub use input::{FormatHint, InputRef, ResolvedInput, ResolvedSource, SourceMetadata};
pub use ir::{
    Asset, AssetId, Block, BlockNode, Cell, CellRef, DOCUMENT_SCHEMA_VERSION, Diagnostic,
    DiagnosticSeverity, Document, DocumentMetadata, Inline, InlineMark, IrError, IrErrorCode,
    ListItem, ListKind, MAX_DOCUMENT_DEPTH, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_JSON_BYTES,
    MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS, NodeId, Provenance, ProvenanceKind, Rect, SourceLocator,
    TableRow, TimeRange, ValidationLimits,
};
pub use options::{
    AiMode, AiOptions, AssetMode, ConversionOptions, NetworkOptions, OcrOptions, OcrPolicy,
    OutputOptions, ResourceLimits, TextDecodingMode, TextOptions,
};
pub use spi::{
    AiCapability, AiInput, AiOutput, AiProvider, AiRequest, BoxFuture, ConversionRequest,
    ConversionResult, Converter, ConverterOutput, DetectionRequest, DetectionResult, DocumentPatch,
    FormatDetector, MarkdownRenderer, OcrEngine, OcrRegion, OcrRequest, OcrResult, PatchOperation,
    ProbeOutcome, Services, SourceResolver, Tensor, TensorRuntime, Transcriber,
    TranscriptionRequest, TranscriptionResult,
};
