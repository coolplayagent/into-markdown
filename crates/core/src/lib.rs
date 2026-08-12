//! Stable contracts and provenance-aware intermediate representation for
//! `into-markdown`.
//!
//! This crate deliberately contains no format parser, network client, model
//! runtime, or provider implementation. Implementations depend on this crate;
//! the dependency never points in the opposite direction.

mod error;
mod format;
mod input;
mod ir;
mod options;
mod spi;

pub use error::{ConversionError, ErrorCode};
pub use format::{FormatCandidate, InputFormat};
pub use input::{FormatHint, InputRef, ResolvedInput, SourceMetadata};
pub use ir::{
    Asset, AssetId, Block, BlockNode, Cell, CellRef, DOCUMENT_SCHEMA_VERSION, Diagnostic,
    DiagnosticSeverity, Document, DocumentMetadata, Inline, InlineMark, IrError, IrErrorCode,
    ListItem, ListKind, NodeId, Provenance, ProvenanceKind, Rect, SourceLocator, TableRow,
    TimeRange,
};
pub use options::{
    AiMode, AiOptions, AssetMode, ConversionOptions, NetworkOptions, OcrOptions, OcrPolicy,
    OutputOptions, ResourceLimits,
};
pub use spi::{
    AiCapability, AiInput, AiOutput, AiProvider, AiRequest, BoxFuture, ConversionRequest,
    ConversionResult, Converter, ConverterOutput, DetectionRequest, DetectionResult, DocumentPatch,
    FormatDetector, MarkdownRenderer, OcrEngine, OcrRegion, OcrRequest, OcrResult, PatchOperation,
    ProbeOutcome, Services, SourceResolver, Tensor, TensorRuntime, Transcriber,
    TranscriptionRequest, TranscriptionResult,
};
