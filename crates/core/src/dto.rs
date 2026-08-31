//! Stable application wire contracts shared by CLI, HTTP, SSE consumers, and bundles.
//!
//! DTOs intentionally implement neither [`serde::Serialize`] nor [`serde::Deserialize`]. Wire
//! data must cross the versioned, budgeted `to_json` and `from_json` methods instead of a generic
//! framework response or extractor.
//!
//! ```compile_fail
//! use into_markdown_core::ResultDto;
//! let _: ResultDto = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! use into_markdown_core::ResultDto;
//! fn requires_serialize<T: serde::Serialize>() {}
//! requires_serialize::<ResultDto>();
//! ```

use crate::{
    Asset, BatchOcrUsageDto, BatchResourceUsageDto, Diagnostic, DiagnosticSeverity, Document,
    Provenance, ProvenanceKind, SourceLocator, ir::is_safe_container_part_name,
};
use base64::Engine as _;
use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor},
    ser::SerializeSeq,
};
use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Write};
use thiserror::Error;

mod batch_report;
mod resource_usage;
use resource_usage::{RawMemoryBudgetSnapshot, RawOcrRuntimeUsage};

/// Schema version emitted and accepted by application DTOs.
pub const DTO_SCHEMA_VERSION: u32 = 1;
/// Current portable bundle manifest schema version.
pub const BUNDLE_SCHEMA_VERSION: u32 = 2;
/// Maximum JSON bytes accepted by the default DTO decoder.
pub const MAX_DTO_JSON_BYTES: usize = 64 * 1024 * 1024;
/// Maximum JSON nesting accepted by the default DTO decoder.
pub const MAX_DTO_DEPTH: usize = 64;
/// Maximum assets accepted in one result or bundle manifest.
pub const MAX_DTO_ASSETS: usize = 100_000;
/// Maximum decoded bytes carried by all base64 assets in one result.
pub const MAX_DTO_BASE64_BYTES: usize = 32 * 1024 * 1024;
/// Maximum diagnostics accepted in one envelope or result.
pub const MAX_DTO_DIAGNOSTICS: usize = 100_000;
/// Maximum provenance records accepted in one envelope or result.
pub const MAX_DTO_PROVENANCE: usize = 1_000_000;
/// Maximum batch items accepted in one report.
pub const MAX_DTO_BATCH_ITEMS: usize = 1_000_000;
/// Maximum object members and array elements accepted before JSON allocation.
pub const MAX_DTO_VALUES: usize = 2_000_000;
/// Maximum encoded bytes in one JSON string before JSON allocation.
pub const MAX_DTO_STRING_BYTES: usize = 8 * 1024 * 1024;
/// Maximum encoded bytes across JSON strings before JSON allocation.
pub const MAX_DTO_TOTAL_STRING_BYTES: usize = 48 * 1024 * 1024;

/// JSON layout used by the borrowed, budgeted result writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtoJsonStyle {
    /// No insignificant whitespace.
    Compact,
    /// Human-readable indentation produced by `serde_json`.
    Pretty,
}

/// Resource budgets for decoding untrusted application DTO JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtoLimits {
    /// Maximum UTF-8 JSON byte length.
    pub max_json_bytes: usize,
    /// Maximum JSON object/array nesting depth.
    pub max_depth: usize,
    /// Maximum asset count.
    pub max_assets: usize,
    /// Maximum total decoded base64 asset bytes.
    pub max_base64_bytes: usize,
    /// Maximum diagnostic count.
    pub max_diagnostics: usize,
    /// Maximum provenance count.
    pub max_provenance: usize,
    /// Maximum batch item count.
    pub max_batch_items: usize,
    /// Maximum object members and array elements.
    pub max_values: usize,
    /// Maximum encoded bytes in one JSON string.
    pub max_string_bytes: usize,
    /// Maximum encoded bytes across JSON strings.
    pub max_total_string_bytes: usize,
}

impl Default for DtoLimits {
    fn default() -> Self {
        Self {
            max_json_bytes: MAX_DTO_JSON_BYTES,
            max_depth: MAX_DTO_DEPTH,
            max_assets: MAX_DTO_ASSETS,
            max_base64_bytes: MAX_DTO_BASE64_BYTES,
            max_diagnostics: MAX_DTO_DIAGNOSTICS,
            max_provenance: MAX_DTO_PROVENANCE,
            max_batch_items: MAX_DTO_BATCH_ITEMS,
            max_values: MAX_DTO_VALUES,
            max_string_bytes: MAX_DTO_STRING_BYTES,
            max_total_string_bytes: MAX_DTO_TOTAL_STRING_BYTES,
        }
    }
}

/// Stable categories returned while decoding or validating application DTOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DtoErrorCode {
    /// JSON is malformed or a required field has the wrong shape.
    InvalidJson,
    /// The declared application DTO schema version is unsupported.
    UnsupportedSchemaVersion,
    /// A field violates a protocol invariant.
    InvalidField,
    /// Base64 asset data is malformed.
    InvalidBase64,
    /// An identifier is repeated where uniqueness is required.
    DuplicateId,
    /// A decoding or structural budget was exceeded.
    ResourceLimit,
}

impl DtoErrorCode {
    /// Stable lower-camel-case machine representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalidJson",
            Self::UnsupportedSchemaVersion => "unsupportedSchemaVersion",
            Self::InvalidField => "invalidField",
            Self::InvalidBase64 => "invalidBase64",
            Self::DuplicateId => "duplicateId",
            Self::ResourceLimit => "resourceLimit",
        }
    }
}

/// Controlled failure at an application DTO boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {path}: {detail}", code = code.as_str())]
pub struct DtoError {
    /// Stable machine-readable category.
    pub code: DtoErrorCode,
    /// Stable JSON-style path to the rejected field.
    pub path: String,
    /// Human-readable detail; callers must branch on [`Self::code`].
    pub detail: String,
}

impl DtoError {
    fn new(code: DtoErrorCode, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { code, path: path.into(), detail: detail.into() }
    }
}

/// Stable diagnostic severity used by external protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverityDto {
    /// Informational recovery note.
    Info,
    /// Content was skipped or recovered imperfectly.
    Warning,
    /// A scoped operation failed but conversion continued.
    Error,
}

/// One stable non-fatal diagnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticDto {
    /// Stable machine-readable code.
    pub code: String,
    /// Stable severity.
    pub severity: DiagnosticSeverityDto,
    /// Human-readable message.
    pub message: String,
    /// Optional source locator.
    pub locator: Option<SourceLocator>,
}

/// Versioned diagnostics document used by bundles and HTTP responses.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticsDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Ordered diagnostic records.
    pub diagnostics: Vec<DiagnosticDto>,
}

/// Stable provenance origin used by external protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceKindDto {
    /// Deterministic source parser.
    NativeParser,
    /// Local OCR model.
    LocalOcr,
    /// Remote or local AI provider.
    AiProvider,
    /// Container or file metadata.
    Metadata,
    /// Deterministic post-processing.
    Postprocessor,
}

/// One stable material provenance record.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvenanceDto {
    /// Origin class.
    pub kind: ProvenanceKindDto,
    /// Stable implementation or provider identifier.
    pub provider: String,
    /// Source location.
    pub locator: SourceLocator,
    /// Optional confidence in the inclusive range `0..=1`.
    pub confidence: Option<f32>,
}

/// Versioned provenance document used by bundles and HTTP responses.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvenanceListDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Ordered provenance records.
    pub provenance: Vec<ProvenanceDto>,
}

/// Stable asset representation with standard padded base64 content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetDto {
    /// Stable document-scoped identifier.
    pub id: String,
    /// Suggested filename.
    pub filename: Option<String>,
    /// MIME media type.
    pub media_type: String,
    /// Standard padded base64 content; empty for external-only assets.
    pub data_base64: String,
    /// Original external URI, when present.
    pub external_uri: Option<String>,
}

/// Versioned complete conversion response.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Rendered GitHub-Flavored Markdown.
    pub markdown: String,
    /// Versioned, validated document IR.
    pub document: Document,
    /// Ordered assets.
    pub assets: Vec<AssetDto>,
    /// Ordered diagnostics.
    pub diagnostics: Vec<DiagnosticDto>,
    /// Ordered provenance records.
    pub provenance: Vec<ProvenanceDto>,
}

/// One asset entry in a portable bundle manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleAssetDto {
    /// Stable asset identifier.
    pub id: String,
    /// All document-scoped IDs represented by this physical content entry.
    pub source_asset_ids: Vec<String>,
    /// Safe bundle-relative path.
    pub path: String,
    /// MIME media type.
    pub media_type: String,
    /// Uncompressed byte size.
    pub size: u64,
}

/// Stable portable bundle manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleManifestDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Bundle-relative Markdown path.
    pub markdown: String,
    /// Bundle-relative document IR path.
    pub document_ir: String,
    /// Bundle-relative diagnostics path.
    pub diagnostics: String,
    /// Diagnostics member schema governed by this manifest.
    pub diagnostics_schema_version: u32,
    /// Bundle-relative provenance path.
    pub provenance: String,
    /// Provenance member schema governed by this manifest.
    pub provenance_schema_version: u32,
    /// Ordered asset entries.
    pub assets: Vec<BundleAssetDto>,
}

/// Stable completion state for one batch item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchItemStatus {
    /// Conversion completed successfully.
    Success,
    /// Conversion failed.
    Failed,
}

/// Stable semantic outcome for one batch item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchItemOutcome {
    /// Conversion completed without recoverable losses.
    Complete,
    /// Conversion completed after omitting or sanitizing non-critical content.
    Degraded,
    /// Conversion did not commit an output.
    Failed,
}

/// Structured resource-limit detail for a failed batch item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLimitDto {
    /// Stable limit identifier, such as `max_memory_bytes`.
    pub name: String,
    /// Sanitized observed-versus-allowed detail, when available.
    pub detail: Option<String>,
}

/// One item in a machine-readable batch report.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchItemDto {
    /// Display-safe input identifier.
    pub input: String,
    /// Output path, when one was allocated.
    pub output: Option<String>,
    /// Stable detected or explicit format identifier.
    pub format: Option<String>,
    /// Completion state.
    pub status: BatchItemStatus,
    /// Semantic outcome, including successful degraded conversions.
    pub outcome: BatchItemOutcome,
    /// Ordered diagnostics.
    pub diagnostics: Vec<DiagnosticDto>,
    /// Stable failure code.
    pub error_code: Option<String>,
    /// Stable reason code with finer detail than `errorCode` when available.
    pub reason_code: Option<String>,
    /// Component responsible for the failure, when known.
    pub component: Option<String>,
    /// Package part or stream responsible for the failure, when known.
    pub part: Option<String>,
    /// Structured resource-limit detail, when applicable.
    pub limit: Option<BatchLimitDto>,
    /// Human-readable failure detail.
    pub message: Option<String>,
    /// Human-readable warnings produced by output handling.
    pub warnings: Vec<String>,
    /// End-to-end time from worker admission through output commit or failure cleanup.
    pub duration_ms: Option<f64>,
    /// Engine processing time through rendering, excluding output sink and persistence work.
    pub processing_duration_ms: Option<f64>,
}

/// Versioned machine-readable batch report.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchReportDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Number of successful items.
    pub succeeded: u64,
    /// Number of failed items.
    pub failed: u64,
    /// Input-order report items.
    pub items: Vec<BatchItemDto>,
    /// Independent batch wall-clock time; this is never derived by summing item durations.
    pub wall_duration_ms: Option<f64>,
    /// Invocation-wide resource accounting. Older schema-one reports may omit this field.
    pub resource_usage: Option<BatchResourceUsageDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawDiagnosticSeverityDto {
    Info,
    Warning,
    Error,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDiagnosticDto {
    code: String,
    severity: RawDiagnosticSeverityDto,
    message: String,
    locator: Option<SourceLocator>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDiagnosticsDto {
    schema_version: u32,
    diagnostics: Vec<RawDiagnosticDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum RawProvenanceKindDto {
    NativeParser,
    LocalOcr,
    AiProvider,
    Metadata,
    Postprocessor,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProvenanceDto {
    kind: RawProvenanceKindDto,
    provider: String,
    locator: SourceLocator,
    confidence: Option<f32>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProvenanceListDto {
    schema_version: u32,
    provenance: Vec<RawProvenanceDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAssetDto {
    id: String,
    filename: Option<String>,
    media_type: String,
    data_base64: String,
    external_uri: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawResultDto {
    schema_version: u32,
    markdown: String,
    document: Document,
    assets: Vec<RawAssetDto>,
    diagnostics: Vec<RawDiagnosticDto>,
    provenance: Vec<RawProvenanceDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBundleAssetDto {
    id: String,
    #[serde(default)]
    source_asset_ids: Vec<String>,
    path: String,
    media_type: String,
    size: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBundleManifestDto {
    schema_version: u32,
    markdown: String,
    document_ir: String,
    diagnostics: String,
    #[serde(default = "dto_schema_version")]
    diagnostics_schema_version: u32,
    provenance: String,
    #[serde(default = "dto_schema_version")]
    provenance_schema_version: u32,
    assets: Vec<RawBundleAssetDto>,
}

const fn dto_schema_version() -> u32 {
    DTO_SCHEMA_VERSION
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum RawBatchItemStatus {
    Success,
    Failed,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawBatchItemOutcome {
    Complete,
    Degraded,
    Failed,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBatchLimitDto {
    name: String,
    detail: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBatchItemDto {
    input: String,
    output: Option<String>,
    format: Option<String>,
    status: RawBatchItemStatus,
    #[serde(default)]
    outcome: Option<RawBatchItemOutcome>,
    diagnostics: Vec<RawDiagnosticDto>,
    error_code: Option<String>,
    #[serde(default)]
    reason_code: Option<String>,
    #[serde(default)]
    component: Option<String>,
    #[serde(default)]
    part: Option<String>,
    #[serde(default)]
    limit: Option<RawBatchLimitDto>,
    message: Option<String>,
    warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    processing_duration_ms: Option<f64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBatchOcrUsageDto {
    recognized_regions: u64,
    recognized_chars: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBatchResourceUsageDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory: Option<RawMemoryBudgetSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ocr_runtime: Option<RawOcrRuntimeUsage>,
    shared_lease_budget_bytes: u64,
    shared_lease_peak_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ocr: Option<RawBatchOcrUsageDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBatchReportDto {
    schema_version: u32,
    succeeded: u64,
    failed: u64,
    items: Vec<RawBatchItemDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wall_duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resource_usage: Option<RawBatchResourceUsageDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InternalResultWire<'a> {
    schema_version: u32,
    markdown: &'a str,
    document: &'a Document,
    assets: InternalAssetsWire<'a>,
    diagnostics: InternalDiagnosticsWire<'a>,
    provenance: InternalProvenanceWire<'a>,
}

struct InternalAssetsWire<'a> {
    assets: &'a [Asset],
    include_base64: bool,
}

impl Serialize for InternalAssetsWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.assets.len()))?;
        for asset in self.assets {
            sequence.serialize_element(&InternalAssetWire {
                id: &asset.id.0,
                filename: asset.filename.as_deref(),
                media_type: &asset.media_type,
                data_base64: if self.include_base64 {
                    AssetBase64Wire::Encoded(&asset.bytes)
                } else {
                    AssetBase64Wire::Placeholder
                },
                external_uri: asset.external_uri.as_deref(),
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InternalAssetWire<'a> {
    id: &'a str,
    filename: Option<&'a str>,
    media_type: &'a str,
    data_base64: AssetBase64Wire<'a>,
    external_uri: Option<&'a str>,
}

enum AssetBase64Wire<'a> {
    Placeholder,
    Encoded(&'a [u8]),
}

impl Serialize for AssetBase64Wire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Placeholder => serializer.serialize_str(""),
            Self::Encoded(bytes) => serializer.collect_str(&StreamingBase64(bytes)),
        }
    }
}

struct StreamingBase64<'a>(&'a [u8]);

impl fmt::Display for StreamingBase64<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(test)]
        ASSET_BASE64_ENCODE_CALLS.set(ASSET_BASE64_ENCODE_CALLS.get() + 1);
        base64::display::Base64Display::new(self.0, &base64::engine::general_purpose::STANDARD)
            .fmt(formatter)
    }
}

struct InternalDiagnosticsWire<'a>(&'a [Diagnostic]);

impl Serialize for InternalDiagnosticsWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for diagnostic in self.0 {
            sequence.serialize_element(&InternalDiagnosticWire {
                code: &diagnostic.code,
                severity: match diagnostic.severity {
                    DiagnosticSeverity::Info => RawDiagnosticSeverityDto::Info,
                    DiagnosticSeverity::Warning => RawDiagnosticSeverityDto::Warning,
                    DiagnosticSeverity::Error => RawDiagnosticSeverityDto::Error,
                },
                message: &diagnostic.message,
                locator: diagnostic.locator.as_ref(),
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InternalDiagnosticWire<'a> {
    code: &'a str,
    severity: RawDiagnosticSeverityDto,
    message: &'a str,
    locator: Option<&'a SourceLocator>,
}

struct InternalProvenanceWire<'a>(&'a [Provenance]);

impl Serialize for InternalProvenanceWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for provenance in self.0 {
            sequence.serialize_element(&InternalProvenanceItemWire {
                kind: match provenance.kind {
                    ProvenanceKind::NativeParser => RawProvenanceKindDto::NativeParser,
                    ProvenanceKind::LocalOcr => RawProvenanceKindDto::LocalOcr,
                    ProvenanceKind::AiProvider => RawProvenanceKindDto::AiProvider,
                    ProvenanceKind::Metadata => RawProvenanceKindDto::Metadata,
                    ProvenanceKind::Postprocessor => RawProvenanceKindDto::Postprocessor,
                },
                provider: &provenance.provider,
                locator: &provenance.locator,
                confidence: provenance.confidence,
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InternalProvenanceItemWire<'a> {
    kind: RawProvenanceKindDto,
    provider: &'a str,
    locator: &'a SourceLocator,
    confidence: Option<f32>,
}

impl From<RawDiagnosticDto> for DiagnosticDto {
    fn from(value: RawDiagnosticDto) -> Self {
        Self {
            code: value.code,
            severity: match value.severity {
                RawDiagnosticSeverityDto::Info => DiagnosticSeverityDto::Info,
                RawDiagnosticSeverityDto::Warning => DiagnosticSeverityDto::Warning,
                RawDiagnosticSeverityDto::Error => DiagnosticSeverityDto::Error,
            },
            message: value.message,
            locator: value.locator,
        }
    }
}

impl From<RawProvenanceDto> for ProvenanceDto {
    fn from(value: RawProvenanceDto) -> Self {
        Self {
            kind: match value.kind {
                RawProvenanceKindDto::NativeParser => ProvenanceKindDto::NativeParser,
                RawProvenanceKindDto::LocalOcr => ProvenanceKindDto::LocalOcr,
                RawProvenanceKindDto::AiProvider => ProvenanceKindDto::AiProvider,
                RawProvenanceKindDto::Metadata => ProvenanceKindDto::Metadata,
                RawProvenanceKindDto::Postprocessor => ProvenanceKindDto::Postprocessor,
            },
            provider: value.provider,
            locator: value.locator,
            confidence: value.confidence,
        }
    }
}

impl From<RawAssetDto> for AssetDto {
    fn from(value: RawAssetDto) -> Self {
        Self {
            id: value.id,
            filename: value.filename,
            media_type: value.media_type,
            data_base64: value.data_base64,
            external_uri: value.external_uri,
        }
    }
}

impl From<&DiagnosticDto> for RawDiagnosticDto {
    fn from(value: &DiagnosticDto) -> Self {
        Self {
            code: value.code.clone(),
            severity: match value.severity {
                DiagnosticSeverityDto::Info => RawDiagnosticSeverityDto::Info,
                DiagnosticSeverityDto::Warning => RawDiagnosticSeverityDto::Warning,
                DiagnosticSeverityDto::Error => RawDiagnosticSeverityDto::Error,
            },
            message: value.message.clone(),
            locator: value.locator.clone(),
        }
    }
}

impl From<&ProvenanceDto> for RawProvenanceDto {
    fn from(value: &ProvenanceDto) -> Self {
        Self {
            kind: match value.kind {
                ProvenanceKindDto::NativeParser => RawProvenanceKindDto::NativeParser,
                ProvenanceKindDto::LocalOcr => RawProvenanceKindDto::LocalOcr,
                ProvenanceKindDto::AiProvider => RawProvenanceKindDto::AiProvider,
                ProvenanceKindDto::Metadata => RawProvenanceKindDto::Metadata,
                ProvenanceKindDto::Postprocessor => RawProvenanceKindDto::Postprocessor,
            },
            provider: value.provider.clone(),
            locator: value.locator.clone(),
            confidence: value.confidence,
        }
    }
}

impl From<&AssetDto> for RawAssetDto {
    fn from(value: &AssetDto) -> Self {
        Self {
            id: value.id.clone(),
            filename: value.filename.clone(),
            media_type: value.media_type.clone(),
            data_base64: value.data_base64.clone(),
            external_uri: value.external_uri.clone(),
        }
    }
}

fn encode_result(value: &ResultDto) -> RawResultDto {
    RawResultDto {
        schema_version: value.schema_version,
        markdown: value.markdown.clone(),
        document: value.document.clone(),
        assets: value.assets.iter().map(RawAssetDto::from).collect(),
        diagnostics: value.diagnostics.iter().map(RawDiagnosticDto::from).collect(),
        provenance: value.provenance.iter().map(RawProvenanceDto::from).collect(),
    }
}

fn encode_diagnostics(value: &DiagnosticsDto) -> RawDiagnosticsDto {
    RawDiagnosticsDto {
        schema_version: value.schema_version,
        diagnostics: value.diagnostics.iter().map(RawDiagnosticDto::from).collect(),
    }
}

fn encode_provenance(value: &ProvenanceListDto) -> RawProvenanceListDto {
    RawProvenanceListDto {
        schema_version: value.schema_version,
        provenance: value.provenance.iter().map(RawProvenanceDto::from).collect(),
    }
}

fn encode_manifest(value: &BundleManifestDto) -> RawBundleManifestDto {
    RawBundleManifestDto {
        schema_version: value.schema_version,
        markdown: value.markdown.clone(),
        document_ir: value.document_ir.clone(),
        diagnostics: value.diagnostics.clone(),
        diagnostics_schema_version: value.diagnostics_schema_version,
        provenance: value.provenance.clone(),
        provenance_schema_version: value.provenance_schema_version,
        assets: value
            .assets
            .iter()
            .map(|asset| RawBundleAssetDto {
                id: asset.id.clone(),
                source_asset_ids: asset.source_asset_ids.clone(),
                path: asset.path.clone(),
                media_type: asset.media_type.clone(),
                size: asset.size,
            })
            .collect(),
    }
}

fn encode_batch_report(value: &BatchReportDto) -> RawBatchReportDto {
    RawBatchReportDto {
        schema_version: value.schema_version,
        succeeded: value.succeeded,
        failed: value.failed,
        wall_duration_ms: value.wall_duration_ms,
        resource_usage: value.resource_usage.as_ref().map(|usage| RawBatchResourceUsageDto {
            memory: usage.memory.map(Into::into),
            ocr_runtime: usage.ocr_runtime.map(Into::into),
            shared_lease_budget_bytes: usage.shared_lease_budget_bytes,
            shared_lease_peak_bytes: usage.shared_lease_peak_bytes,
            ocr: usage.ocr.map(|ocr| RawBatchOcrUsageDto {
                recognized_regions: ocr.recognized_regions,
                recognized_chars: ocr.recognized_chars,
            }),
        }),
        items: value
            .items
            .iter()
            .map(|item| RawBatchItemDto {
                input: item.input.clone(),
                output: item.output.clone(),
                format: item.format.clone(),
                status: match item.status {
                    BatchItemStatus::Success => RawBatchItemStatus::Success,
                    BatchItemStatus::Failed => RawBatchItemStatus::Failed,
                },
                outcome: Some(match item.outcome {
                    BatchItemOutcome::Complete => RawBatchItemOutcome::Complete,
                    BatchItemOutcome::Degraded => RawBatchItemOutcome::Degraded,
                    BatchItemOutcome::Failed => RawBatchItemOutcome::Failed,
                }),
                diagnostics: item.diagnostics.iter().map(RawDiagnosticDto::from).collect(),
                error_code: item.error_code.clone(),
                reason_code: item.reason_code.clone(),
                component: item.component.clone(),
                part: item.part.clone(),
                limit: item.limit.as_ref().map(|limit| RawBatchLimitDto {
                    name: limit.name.clone(),
                    detail: limit.detail.clone(),
                }),
                message: item.message.clone(),
                warnings: item.warnings.clone(),
                duration_ms: item.duration_ms,
                processing_duration_ms: item.processing_duration_ms,
            })
            .collect(),
    }
}

impl BatchReportDto {
    /// Build a report with derived, checked totals.
    ///
    /// # Errors
    ///
    /// Returns [`DtoErrorCode::ResourceLimit`] if counts cannot be represented.
    pub fn try_new(items: Vec<BatchItemDto>) -> Result<Self, DtoError> {
        Self::try_new_with_wall_duration(items, None)
    }

    /// Build a report with derived totals and an independently measured batch duration.
    ///
    /// # Errors
    ///
    /// Returns [`DtoErrorCode::ResourceLimit`] if counts cannot be represented, or
    /// [`DtoErrorCode::InvalidField`] for a non-finite or negative duration.
    pub fn try_new_with_wall_duration(
        items: Vec<BatchItemDto>,
        wall_duration_ms: Option<f64>,
    ) -> Result<Self, DtoError> {
        Self::try_new_with_resource_usage(items, wall_duration_ms, None)
    }

    /// Build a report with measured batch duration and invocation resource accounting.
    ///
    /// # Errors
    ///
    /// Returns [`DtoErrorCode::ResourceLimit`] if counts cannot be represented, or
    /// [`DtoErrorCode::InvalidField`] for invalid timing or resource telemetry.
    pub fn try_new_with_resource_usage(
        items: Vec<BatchItemDto>,
        wall_duration_ms: Option<f64>,
        resource_usage: Option<BatchResourceUsageDto>,
    ) -> Result<Self, DtoError> {
        let succeeded = items.iter().filter(|item| item.status == BatchItemStatus::Success).count();
        let failed = items.len().saturating_sub(succeeded);
        let report = Self {
            schema_version: DTO_SCHEMA_VERSION,
            succeeded: u64::try_from(succeeded).map_err(|_| {
                DtoError::new(
                    DtoErrorCode::ResourceLimit,
                    "$.succeeded",
                    "successful item count cannot be represented",
                )
            })?,
            failed: u64::try_from(failed).map_err(|_| {
                DtoError::new(
                    DtoErrorCode::ResourceLimit,
                    "$.failed",
                    "failed item count cannot be represented",
                )
            })?,
            items,
            wall_duration_ms,
            resource_usage,
        };
        report.validate(&DtoLimits::default())?;
        Ok(report)
    }
}

impl From<&Diagnostic> for DiagnosticDto {
    fn from(value: &Diagnostic) -> Self {
        Self {
            code: value.code.clone(),
            severity: match value.severity {
                DiagnosticSeverity::Info => DiagnosticSeverityDto::Info,
                DiagnosticSeverity::Warning => DiagnosticSeverityDto::Warning,
                DiagnosticSeverity::Error => DiagnosticSeverityDto::Error,
            },
            message: value.message.clone(),
            locator: value.locator.clone(),
        }
    }
}

impl From<&Provenance> for ProvenanceDto {
    fn from(value: &Provenance) -> Self {
        Self {
            kind: match value.kind {
                ProvenanceKind::NativeParser => ProvenanceKindDto::NativeParser,
                ProvenanceKind::LocalOcr => ProvenanceKindDto::LocalOcr,
                ProvenanceKind::AiProvider => ProvenanceKindDto::AiProvider,
                ProvenanceKind::Metadata => ProvenanceKindDto::Metadata,
                ProvenanceKind::Postprocessor => ProvenanceKindDto::Postprocessor,
            },
            provider: value.provider.clone(),
            locator: value.locator.clone(),
            confidence: value.confidence,
        }
    }
}

impl TryFrom<&Asset> for AssetDto {
    type Error = DtoError;

    fn try_from(value: &Asset) -> Result<Self, Self::Error> {
        preflight_internal_assets(std::slice::from_ref(value), &DtoLimits::default())?;
        let dto = Self {
            id: value.id.0.clone(),
            filename: value.filename.clone(),
            media_type: value.media_type.clone(),
            data_base64: encode_asset_base64(&value.bytes),
            external_uri: value.external_uri.clone(),
        };
        validate_assets(std::slice::from_ref(&dto), &DtoLimits::default())?;
        Ok(dto)
    }
}

impl TryFrom<DiagnosticDto> for Diagnostic {
    type Error = DtoError;

    fn try_from(value: DiagnosticDto) -> Result<Self, Self::Error> {
        validate_diagnostics(std::slice::from_ref(&value), &DtoLimits::default(), "$")?;
        Ok(Self {
            code: value.code,
            severity: match value.severity {
                DiagnosticSeverityDto::Info => DiagnosticSeverity::Info,
                DiagnosticSeverityDto::Warning => DiagnosticSeverity::Warning,
                DiagnosticSeverityDto::Error => DiagnosticSeverity::Error,
            },
            message: value.message,
            locator: value.locator,
        })
    }
}

impl TryFrom<ProvenanceDto> for Provenance {
    type Error = DtoError;

    fn try_from(value: ProvenanceDto) -> Result<Self, Self::Error> {
        validate_provenance(std::slice::from_ref(&value), &DtoLimits::default(), "$")?;
        Ok(Self {
            kind: match value.kind {
                ProvenanceKindDto::NativeParser => ProvenanceKind::NativeParser,
                ProvenanceKindDto::LocalOcr => ProvenanceKind::LocalOcr,
                ProvenanceKindDto::AiProvider => ProvenanceKind::AiProvider,
                ProvenanceKindDto::Metadata => ProvenanceKind::Metadata,
                ProvenanceKindDto::Postprocessor => ProvenanceKind::Postprocessor,
            },
            provider: value.provider,
            locator: value.locator,
            confidence: value.confidence,
        })
    }
}

impl TryFrom<AssetDto> for Asset {
    type Error = DtoError;

    fn try_from(value: AssetDto) -> Result<Self, Self::Error> {
        validate_assets(std::slice::from_ref(&value), &DtoLimits::default())?;
        let bytes =
            base64::engine::general_purpose::STANDARD.decode(&value.data_base64).map_err(|_| {
                DtoError::new(
                    DtoErrorCode::InvalidBase64,
                    "$.dataBase64",
                    "expected canonical standard padded base64",
                )
            })?;
        if base64::engine::general_purpose::STANDARD.encode(&bytes) != value.data_base64 {
            return Err(DtoError::new(
                DtoErrorCode::InvalidBase64,
                "$.dataBase64",
                "expected canonical standard padded base64",
            ));
        }
        Ok(Self {
            id: crate::AssetId(value.id),
            filename: value.filename,
            media_type: value.media_type,
            bytes,
            external_uri: value.external_uri,
        })
    }
}

impl TryFrom<ResultDto> for crate::ConversionResult {
    type Error = DtoError;

    fn try_from(value: ResultDto) -> Result<Self, Self::Error> {
        value.validate(&DtoLimits::default())?;
        Ok(Self {
            document: value.document,
            markdown: value.markdown,
            assets: value.assets.into_iter().map(Asset::try_from).collect::<Result<_, _>>()?,
            diagnostics: value
                .diagnostics
                .into_iter()
                .map(Diagnostic::try_from)
                .collect::<Result<_, _>>()?,
            provenance: value
                .provenance
                .into_iter()
                .map(Provenance::try_from)
                .collect::<Result<_, _>>()?,
            detected_format: None,
            processing_duration_ms: None,
            memory_lease: crate::spi::OutputMemoryLease::default(),
        })
    }
}

macro_rules! json_api {
    ($type:ty, $version:ident, $validate:ident, $preflight:ident, $decode:ident, $encode:ident) => {
        impl $type {
            /// Serialize this DTO after validating protocol invariants.
            ///
            /// # Errors
            ///
            /// Returns a stable [`DtoErrorCode`] for an invalid DTO or serialization failure.
            pub fn to_json(&self) -> Result<String, DtoError> {
                self.$validate(&DtoLimits::default())?;
                let wire = $encode(self);
                let json = serde_json::to_string(&wire).map_err(|error| {
                    DtoError::new(DtoErrorCode::InvalidJson, "$", format!("serialize DTO: {error}"))
                })?;
                validate_wire_json(&json, &DtoLimits::default())?;
                Ok(json)
            }

            /// Serialize this DTO as indented JSON after validating protocol invariants.
            ///
            /// # Errors
            ///
            /// Returns a stable [`DtoErrorCode`] for an invalid DTO or serialization failure.
            pub fn to_pretty_json(&self) -> Result<String, DtoError> {
                self.$validate(&DtoLimits::default())?;
                let wire = $encode(self);
                let json = serde_json::to_string_pretty(&wire).map_err(|error| {
                    DtoError::new(DtoErrorCode::InvalidJson, "$", format!("serialize DTO: {error}"))
                })?;
                validate_wire_json(&json, &DtoLimits::default())?;
                Ok(json)
            }

            /// Decode untrusted JSON with default resource limits.
            ///
            /// # Errors
            ///
            /// Returns a stable [`DtoErrorCode`] for malformed, unsupported, unsafe, or
            /// over-budget input.
            pub fn from_json(json: &str) -> Result<Self, DtoError> {
                Self::from_json_with_limits(json, &DtoLimits::default())
            }

            /// Decode untrusted JSON with caller-provided resource limits.
            ///
            /// # Errors
            ///
            /// Returns a stable [`DtoErrorCode`] for malformed, unsupported, unsafe, or
            /// over-budget input.
            pub fn from_json_with_limits(json: &str, limits: &DtoLimits) -> Result<Self, DtoError> {
                let mut value = decode_value(json, limits)?;
                $version(&value)?;
                $preflight(&mut value, limits)?;
                let decoded: Self = $decode(value)?;
                decoded.$validate(limits)?;
                Ok(decoded)
            }
        }
    };
}

json_api!(
    ResultDto,
    require_version,
    validate,
    preflight_result_document,
    decode_result,
    encode_result
);
json_api!(
    DiagnosticsDto,
    require_version,
    validate,
    no_preflight,
    decode_diagnostics,
    encode_diagnostics
);
json_api!(
    ProvenanceListDto,
    require_version,
    validate,
    no_preflight,
    decode_provenance,
    encode_provenance
);
json_api!(
    BundleManifestDto,
    require_bundle_version,
    validate,
    no_preflight,
    decode_manifest,
    encode_manifest
);
json_api!(
    BatchReportDto,
    require_version,
    validate,
    no_preflight,
    decode_batch_report,
    encode_batch_report
);

impl ResultDto {
    /// Stream a conversion result directly to a JSON writer without constructing an owned DTO or
    /// base64 string. The selected layout is fully budgeted before the first asset is encoded.
    ///
    /// # Errors
    ///
    /// Returns a stable [`DtoErrorCode`] for an invalid or over-budget result, serialization
    /// failure, or destination write failure.
    pub fn write_json_from_result<W: Write>(
        result: &crate::ConversionResult,
        style: DtoJsonStyle,
        writer: &mut W,
    ) -> Result<(), DtoError> {
        Self::write_json_from_result_with_limits(result, style, &DtoLimits::default(), writer)
    }

    /// Stream a conversion result with caller-provided limits. Limits are applied to the actual
    /// selected compact or pretty layout before the destination is touched.
    ///
    /// # Errors
    ///
    /// Returns a stable [`DtoErrorCode`] for an invalid or over-budget result, serialization
    /// failure, or destination write failure.
    pub fn write_json_from_result_with_limits<W: Write>(
        result: &crate::ConversionResult,
        style: DtoJsonStyle,
        limits: &DtoLimits,
        writer: &mut W,
    ) -> Result<(), DtoError> {
        validate_internal_result(result, limits)?;
        account_internal_result_wire(result, style)?.validate(limits)?;
        write_internal_result_wire(result, style, writer)
    }

    /// Serialize a conversion result into its sole owned JSON buffer through the borrowed,
    /// budgeted writer.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::write_json_from_result`].
    pub fn json_from_result(
        result: &crate::ConversionResult,
        style: DtoJsonStyle,
    ) -> Result<String, DtoError> {
        let mut bytes = Vec::new();
        Self::write_json_from_result(result, style, &mut bytes)?;
        String::from_utf8(bytes).map_err(|error| {
            DtoError::new(
                DtoErrorCode::InvalidJson,
                "$",
                format!("result JSON was not UTF-8: {error}"),
            )
        })
    }

    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        validate_version(self.schema_version)?;
        self.document.validate().map_err(|error| {
            DtoError::new(DtoErrorCode::InvalidField, "$.document", error.to_string())
        })?;
        validate_assets(&self.assets, limits)?;
        validate_diagnostics(&self.diagnostics, limits, "$.diagnostics")?;
        validate_provenance(&self.provenance, limits, "$.provenance")
    }
}

impl DiagnosticsDto {
    /// Validate and stream the legacy bundle member directly from internal records.
    ///
    /// # Errors
    ///
    /// Returns a stable [`DtoErrorCode`] for invalid records or serialization failure.
    pub fn write_bundle_json_from_diagnostics<W: std::io::Write>(
        values: &[Diagnostic],
        writer: W,
    ) -> Result<(), DtoError> {
        validate_internal_diagnostics(values, &DtoLimits::default(), "$.diagnostics")?;
        serde_json::to_writer_pretty(writer, &InternalDiagnosticsWire(values)).map_err(|error| {
            DtoError::new(DtoErrorCode::InvalidJson, "$", format!("serialize DTO: {error}"))
        })
    }

    /// Construct and validate a versioned diagnostics envelope from internal records.
    ///
    /// # Errors
    ///
    /// Returns [`DtoErrorCode::InvalidField`] for an invalid internal diagnostic.
    pub fn try_from_diagnostics(values: &[Diagnostic]) -> Result<Self, DtoError> {
        let dto = Self {
            schema_version: DTO_SCHEMA_VERSION,
            diagnostics: values.iter().map(DiagnosticDto::from).collect(),
        };
        dto.validate(&DtoLimits::default())?;
        Ok(dto)
    }

    /// Serialize the diagnostics member of a bundle governed by manifest schema version 1.
    ///
    /// Bundle schema 1 retains the established bare-array member shape. Standalone HTTP and
    /// library responses use the versioned envelope returned by [`Self::to_json`].
    ///
    /// # Errors
    ///
    /// Returns a stable [`DtoErrorCode`] for invalid or over-budget output.
    pub fn to_bundle_pretty_json(&self) -> Result<String, DtoError> {
        self.validate(&DtoLimits::default())?;
        let wire = self.diagnostics.iter().map(RawDiagnosticDto::from).collect::<Vec<_>>();
        let json = serde_json::to_string_pretty(&wire).map_err(|error| {
            DtoError::new(DtoErrorCode::InvalidJson, "$", format!("serialize DTO: {error}"))
        })?;
        validate_wire_json(&json, &DtoLimits::default())?;
        Ok(json)
    }

    /// Decode the diagnostics member of a bundle governed by the supplied manifest version.
    ///
    /// # Errors
    ///
    /// Returns a stable [`DtoErrorCode`] for unsupported, malformed, invalid, or over-budget input.
    pub fn from_bundle_json(json: &str, manifest_schema_version: u32) -> Result<Self, DtoError> {
        validate_version(manifest_schema_version)?;
        let value = decode_value(json, &DtoLimits::default())?;
        let raw: Vec<RawDiagnosticDto> = decode_typed(value)?;
        let dto = Self {
            schema_version: manifest_schema_version,
            diagnostics: raw.into_iter().map(DiagnosticDto::from).collect(),
        };
        dto.validate(&DtoLimits::default())?;
        Ok(dto)
    }

    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        validate_version(self.schema_version)?;
        validate_diagnostics(&self.diagnostics, limits, "$.diagnostics")
    }
}

impl ProvenanceListDto {
    /// Validate and stream the legacy bundle member directly from internal records.
    ///
    /// # Errors
    ///
    /// Returns a stable [`DtoErrorCode`] for invalid records or serialization failure.
    pub fn write_bundle_json_from_provenance<W: std::io::Write>(
        values: &[Provenance],
        writer: W,
    ) -> Result<(), DtoError> {
        validate_internal_provenance(values, &DtoLimits::default(), "$.provenance")?;
        serde_json::to_writer_pretty(writer, &InternalProvenanceWire(values)).map_err(|error| {
            DtoError::new(DtoErrorCode::InvalidJson, "$", format!("serialize DTO: {error}"))
        })
    }

    /// Construct and validate a versioned provenance envelope from internal records.
    ///
    /// # Errors
    ///
    /// Returns [`DtoErrorCode::InvalidField`] for an invalid internal provenance record.
    pub fn try_from_provenance(values: &[Provenance]) -> Result<Self, DtoError> {
        let dto = Self {
            schema_version: DTO_SCHEMA_VERSION,
            provenance: values.iter().map(ProvenanceDto::from).collect(),
        };
        dto.validate(&DtoLimits::default())?;
        Ok(dto)
    }

    /// Serialize the provenance member of a bundle governed by manifest schema version 1.
    ///
    /// # Errors
    ///
    /// Returns a stable [`DtoErrorCode`] for invalid or over-budget output.
    pub fn to_bundle_pretty_json(&self) -> Result<String, DtoError> {
        self.validate(&DtoLimits::default())?;
        let wire = self.provenance.iter().map(RawProvenanceDto::from).collect::<Vec<_>>();
        let json = serde_json::to_string_pretty(&wire).map_err(|error| {
            DtoError::new(DtoErrorCode::InvalidJson, "$", format!("serialize DTO: {error}"))
        })?;
        validate_wire_json(&json, &DtoLimits::default())?;
        Ok(json)
    }

    /// Decode the provenance member of a bundle governed by the supplied manifest version.
    ///
    /// # Errors
    ///
    /// Returns a stable [`DtoErrorCode`] for unsupported, malformed, invalid, or over-budget input.
    pub fn from_bundle_json(json: &str, manifest_schema_version: u32) -> Result<Self, DtoError> {
        validate_version(manifest_schema_version)?;
        let value = decode_value(json, &DtoLimits::default())?;
        let raw: Vec<RawProvenanceDto> = decode_typed(value)?;
        let dto = Self {
            schema_version: manifest_schema_version,
            provenance: raw.into_iter().map(ProvenanceDto::from).collect(),
        };
        dto.validate(&DtoLimits::default())?;
        Ok(dto)
    }

    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        validate_version(self.schema_version)?;
        validate_provenance(&self.provenance, limits, "$.provenance")
    }
}

impl BundleManifestDto {
    #[allow(clippy::too_many_lines)]
    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        if !matches!(self.schema_version, 1 | BUNDLE_SCHEMA_VERSION) {
            return Err(DtoError::new(
                DtoErrorCode::UnsupportedSchemaVersion,
                "$.schemaVersion",
                format!("expected 1 or {BUNDLE_SCHEMA_VERSION}, got {}", self.schema_version),
            ));
        }
        let fixed_entries = [
            ("$.markdown", &self.markdown, "document.md"),
            ("$.documentIr", &self.document_ir, "document.ir.json"),
            ("$.diagnostics", &self.diagnostics, "diagnostics.json"),
            ("$.provenance", &self.provenance, "provenance.json"),
        ];
        let mut paths = BTreeSet::from(["manifest.json".to_owned()]);
        for (path, value, expected) in fixed_entries {
            validate_bundle_path(value, path)?;
            if value != expected {
                return Err(DtoError::new(
                    DtoErrorCode::InvalidField,
                    path,
                    format!("bundle schema 1 requires path {expected}"),
                ));
            }
            if !paths.insert(portable_path_key(value)) {
                return Err(DtoError::new(
                    DtoErrorCode::InvalidField,
                    path,
                    "duplicate or reserved bundle path",
                ));
            }
        }
        if self.diagnostics_schema_version != DTO_SCHEMA_VERSION {
            return Err(DtoError::new(
                DtoErrorCode::UnsupportedSchemaVersion,
                "$.diagnosticsSchemaVersion",
                format!("expected {DTO_SCHEMA_VERSION}, got {}", self.diagnostics_schema_version),
            ));
        }
        if self.provenance_schema_version != DTO_SCHEMA_VERSION {
            return Err(DtoError::new(
                DtoErrorCode::UnsupportedSchemaVersion,
                "$.provenanceSchemaVersion",
                format!("expected {DTO_SCHEMA_VERSION}, got {}", self.provenance_schema_version),
            ));
        }
        if self.assets.len() > limits.max_assets {
            return limit("$.assets", "assets", limits.max_assets);
        }
        let mut ids = BTreeSet::new();
        let mut all_source_ids = BTreeSet::new();
        for (index, asset) in self.assets.iter().enumerate() {
            let path = format!("$.assets[{index}]");
            validate_id(&asset.id, &format!("{path}.id"))?;
            if self.schema_version == 1 {
                if asset.source_asset_ids != [asset.id.clone()] {
                    return Err(DtoError::new(
                        DtoErrorCode::InvalidField,
                        format!("{path}.sourceAssetIds"),
                        "bundle schema 1 represents exactly its canonical asset ID",
                    ));
                }
            } else {
                if asset.source_asset_ids.is_empty()
                    || asset.source_asset_ids.first() != Some(&asset.id)
                {
                    return Err(DtoError::new(
                        DtoErrorCode::InvalidField,
                        format!("{path}.sourceAssetIds"),
                        "source asset IDs must start with the canonical ID",
                    ));
                }
                let mut aliases = BTreeSet::new();
                let mut previous = None;
                for (alias_index, alias) in asset.source_asset_ids.iter().enumerate() {
                    validate_id(alias, &format!("{path}.sourceAssetIds[{alias_index}]"))?;
                    if !aliases.insert(alias) {
                        return Err(DtoError::new(
                            DtoErrorCode::DuplicateId,
                            format!("{path}.sourceAssetIds[{alias_index}]"),
                            "duplicate source asset ID",
                        ));
                    }
                    if previous.is_some_and(|value: &String| value >= alias) {
                        return Err(DtoError::new(
                            DtoErrorCode::InvalidField,
                            format!("{path}.sourceAssetIds"),
                            "source asset IDs must be in stable byte order",
                        ));
                    }
                    previous = Some(alias);
                    if !all_source_ids.insert(alias) {
                        return Err(DtoError::new(
                            DtoErrorCode::DuplicateId,
                            format!("{path}.sourceAssetIds[{alias_index}]"),
                            "source asset ID appears in more than one physical entry",
                        ));
                    }
                }
            }
            validate_bundle_path(&asset.path, &format!("{path}.path"))?;
            if !asset.path.starts_with("assets/") {
                return Err(DtoError::new(
                    DtoErrorCode::InvalidField,
                    format!("{path}.path"),
                    "bundle asset paths must be under assets/",
                ));
            }
            if asset.media_type.trim().is_empty() {
                return Err(DtoError::new(
                    DtoErrorCode::InvalidField,
                    format!("{path}.mediaType"),
                    "media type must not be empty",
                ));
            }
            if !ids.insert(&asset.id) {
                return Err(DtoError::new(
                    DtoErrorCode::DuplicateId,
                    format!("{path}.id"),
                    "duplicate asset ID",
                ));
            }
            if !paths.insert(portable_path_key(&asset.path)) {
                return Err(DtoError::new(
                    DtoErrorCode::InvalidField,
                    format!("{path}.path"),
                    "duplicate bundle path",
                ));
            }
        }
        Ok(())
    }
}

impl BatchReportDto {
    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        validate_version(self.schema_version)?;
        validate_duration(self.wall_duration_ms, "$.wallDurationMs")?;
        if let Some(usage) = &self.resource_usage {
            resource_usage::validate(usage)?;
        }
        if self.items.len() > limits.max_batch_items {
            return limit("$.items", "batchItems", limits.max_batch_items);
        }
        let succeeded =
            self.items.iter().filter(|item| item.status == BatchItemStatus::Success).count();
        let failed = self.items.len().saturating_sub(succeeded);
        let expected_succeeded = u64::try_from(succeeded).map_err(|_| {
            DtoError::new(
                DtoErrorCode::ResourceLimit,
                "$.succeeded",
                "successful item count cannot be represented",
            )
        })?;
        let expected_failed = u64::try_from(failed).map_err(|_| {
            DtoError::new(
                DtoErrorCode::ResourceLimit,
                "$.failed",
                "failed item count cannot be represented",
            )
        })?;
        if self.succeeded != expected_succeeded || self.failed != expected_failed {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                "$",
                "batch totals do not match items",
            ));
        }
        for (index, item) in self.items.iter().enumerate() {
            validate_duration(item.duration_ms, &format!("$.items[{index}].durationMs"))?;
            validate_duration(
                item.processing_duration_ms,
                &format!("$.items[{index}].processingDurationMs"),
            )?;
            validate_diagnostics(
                &item.diagnostics,
                limits,
                &format!("$.items[{index}].diagnostics"),
            )?;
            match item.status {
                BatchItemStatus::Success if item.error_code.is_some() => {
                    return Err(DtoError::new(
                        DtoErrorCode::InvalidField,
                        format!("$.items[{index}].errorCode"),
                        "successful item cannot contain an error code",
                    ));
                }
                BatchItemStatus::Failed if item.error_code.is_none() => {
                    return Err(DtoError::new(
                        DtoErrorCode::InvalidField,
                        format!("$.items[{index}].errorCode"),
                        "failed item requires an error code",
                    ));
                }
                _ => {}
            }
        }
        let total_diagnostics = self.items.iter().try_fold(0_usize, |total, item| {
            total.checked_add(item.diagnostics.len()).ok_or_else(|| {
                DtoError::new(DtoErrorCode::ResourceLimit, "$.items", "diagnostic count overflow")
            })
        })?;
        if total_diagnostics > limits.max_diagnostics {
            return limit("$.items", "diagnostics", limits.max_diagnostics);
        }
        Ok(())
    }
}

fn decode_value(json: &str, limits: &DtoLimits) -> Result<serde_json::Value, DtoError> {
    validate_wire_json(json, limits)?;
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("decode DTO: {error}"))
    })?;
    Ok(value)
}

fn validate_wire_json(json: &str, limits: &DtoLimits) -> Result<(), DtoError> {
    if json.len() > limits.max_json_bytes {
        return limit("$", "dtoJsonBytes", limits.max_json_bytes);
    }
    preflight_json_text(json, limits)?;
    reject_duplicate_object_members(json)
}

#[derive(Clone, Copy)]
struct DuplicateKeySeed;

impl<'de> DeserializeSeed<'de> for DuplicateKeySeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateKeyVisitor)
    }
}

struct DuplicateKeyVisitor;

impl<'de> Visitor<'de> for DuplicateKeyVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object members")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(DuplicateKeySeed)?.is_some() {}
        Ok(())
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = std::collections::HashSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(serde::de::Error::custom("duplicate JSON object member"));
            }
            object.next_value_seed(DuplicateKeySeed)?;
        }
        Ok(())
    }
}

fn reject_duplicate_object_members(json: &str) -> Result<(), DtoError> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    DuplicateKeySeed.deserialize(&mut deserializer).map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("decode DTO: {error}"))
    })?;
    deserializer.end().map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("decode DTO: {error}"))
    })
}

fn preflight_json_text(json: &str, limits: &DtoLimits) -> Result<(), DtoError> {
    let mut depth = 0_usize;
    let mut values = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_bytes = 0_usize;
    let mut total_string_bytes = 0_usize;
    for byte in json.bytes() {
        if in_string {
            if escaped {
                escaped = false;
                string_bytes = string_bytes.saturating_add(1);
            } else if byte == b'\\' {
                escaped = true;
                string_bytes = string_bytes.saturating_add(1);
            } else if byte == b'"' {
                in_string = false;
                total_string_bytes = total_string_bytes.saturating_add(string_bytes);
                if total_string_bytes > limits.max_total_string_bytes {
                    return limit("$", "dtoTotalStringBytes", limits.max_total_string_bytes);
                }
            } else {
                string_bytes = string_bytes.saturating_add(1);
            }
            if string_bytes > limits.max_string_bytes {
                return limit("$", "dtoStringBytes", limits.max_string_bytes);
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string_bytes = 0;
            }
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                values = values.saturating_add(1);
                if depth > limits.max_depth {
                    return limit("$", "dtoDepth", limits.max_depth);
                }
            }
            b':' | b',' => {
                values = values.saturating_add(1);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if values > limits.max_values {
            return limit("$", "dtoValues", limits.max_values);
        }
    }
    Ok(())
}

fn preflight_result_document(
    value: &mut serde_json::Value,
    limits: &DtoLimits,
) -> Result<(), DtoError> {
    let Some(document_value) = value.get("document") else {
        return Ok(());
    };
    let json = serde_json::to_string(document_value).map_err(|error| {
        DtoError::new(
            DtoErrorCode::InvalidJson,
            "$.document",
            format!("serialize document for validation: {error}"),
        )
    })?;
    let ir_limits = crate::ValidationLimits {
        max_json_bytes: limits.max_json_bytes,
        ..crate::ValidationLimits::default()
    };
    let document = Document::from_json_with_limits(&json, &ir_limits).map_err(|error| {
        let code = match error.code {
            crate::IrErrorCode::UnsupportedSchemaVersion => DtoErrorCode::UnsupportedSchemaVersion,
            crate::IrErrorCode::DuplicateNodeId => DtoErrorCode::DuplicateId,
            crate::IrErrorCode::ResourceLimit => DtoErrorCode::ResourceLimit,
            _ => DtoErrorCode::InvalidField,
        };
        DtoError::new(
            code,
            format!("$.document{}", error.path.trim_start_matches('$')),
            error.detail,
        )
    })?;
    let normalized = serde_json::to_value(document).map_err(|error| {
        DtoError::new(
            DtoErrorCode::InvalidJson,
            "$.document",
            format!("normalize validated document: {error}"),
        )
    })?;
    if let Some(slot) = value.get_mut("document") {
        *slot = normalized;
    }
    Ok(())
}

fn no_preflight(value: &mut serde_json::Value, _: &DtoLimits) -> Result<(), DtoError> {
    if value.is_object() {
        Ok(())
    } else {
        Err(DtoError::new(DtoErrorCode::InvalidJson, "$", "DTO root must be an object"))
    }
}

fn require_version(value: &serde_json::Value) -> Result<(), DtoError> {
    let version =
        value.get("schemaVersion").and_then(serde_json::Value::as_u64).ok_or_else(|| {
            DtoError::new(
                DtoErrorCode::InvalidJson,
                "$.schemaVersion",
                "schemaVersion is required and must be an unsigned integer",
            )
        })?;
    if version != u64::from(DTO_SCHEMA_VERSION) {
        return Err(DtoError::new(
            DtoErrorCode::UnsupportedSchemaVersion,
            "$.schemaVersion",
            format!("expected {DTO_SCHEMA_VERSION}, got {version}"),
        ));
    }
    Ok(())
}

fn require_bundle_version(value: &serde_json::Value) -> Result<(), DtoError> {
    let version =
        value.get("schemaVersion").and_then(serde_json::Value::as_u64).ok_or_else(|| {
            DtoError::new(
                DtoErrorCode::InvalidJson,
                "$.schemaVersion",
                "schemaVersion is required and must be an unsigned integer",
            )
        })?;
    if !matches!(version, 1 | 2) {
        return Err(DtoError::new(
            DtoErrorCode::UnsupportedSchemaVersion,
            "$.schemaVersion",
            format!("expected 1 or {BUNDLE_SCHEMA_VERSION}, got {version}"),
        ));
    }
    Ok(())
}

fn decode_typed<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, DtoError> {
    serde_json::from_value(value).map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("decode DTO fields: {error}"))
    })
}

fn decode_result(value: serde_json::Value) -> Result<ResultDto, DtoError> {
    let raw: RawResultDto = decode_typed(value)?;
    Ok(ResultDto {
        schema_version: raw.schema_version,
        markdown: raw.markdown,
        document: raw.document,
        assets: raw.assets.into_iter().map(AssetDto::from).collect(),
        diagnostics: raw.diagnostics.into_iter().map(DiagnosticDto::from).collect(),
        provenance: raw.provenance.into_iter().map(ProvenanceDto::from).collect(),
    })
}

fn decode_diagnostics(value: serde_json::Value) -> Result<DiagnosticsDto, DtoError> {
    let raw: RawDiagnosticsDto = decode_typed(value)?;
    Ok(DiagnosticsDto {
        schema_version: raw.schema_version,
        diagnostics: raw.diagnostics.into_iter().map(DiagnosticDto::from).collect(),
    })
}

fn decode_provenance(value: serde_json::Value) -> Result<ProvenanceListDto, DtoError> {
    let raw: RawProvenanceListDto = decode_typed(value)?;
    Ok(ProvenanceListDto {
        schema_version: raw.schema_version,
        provenance: raw.provenance.into_iter().map(ProvenanceDto::from).collect(),
    })
}

fn decode_manifest(value: serde_json::Value) -> Result<BundleManifestDto, DtoError> {
    let raw: RawBundleManifestDto = decode_typed(value)?;
    let schema_version = raw.schema_version;
    Ok(BundleManifestDto {
        schema_version,
        markdown: raw.markdown,
        document_ir: raw.document_ir,
        diagnostics: raw.diagnostics,
        diagnostics_schema_version: raw.diagnostics_schema_version,
        provenance: raw.provenance,
        provenance_schema_version: raw.provenance_schema_version,
        assets: raw
            .assets
            .into_iter()
            .map(|asset| {
                let id = asset.id;
                let source_asset_ids = if schema_version == 1 && asset.source_asset_ids.is_empty() {
                    vec![id.clone()]
                } else {
                    asset.source_asset_ids
                };
                BundleAssetDto {
                    id,
                    source_asset_ids,
                    path: asset.path,
                    media_type: asset.media_type,
                    size: asset.size,
                }
            })
            .collect(),
    })
}

fn decode_batch_report(value: serde_json::Value) -> Result<BatchReportDto, DtoError> {
    let raw: RawBatchReportDto = decode_typed(value)?;
    Ok(BatchReportDto {
        schema_version: raw.schema_version,
        succeeded: raw.succeeded,
        failed: raw.failed,
        wall_duration_ms: raw.wall_duration_ms,
        resource_usage: raw.resource_usage.map(|usage| BatchResourceUsageDto {
            memory: usage.memory.map(Into::into),
            ocr_runtime: usage.ocr_runtime.map(Into::into),
            shared_lease_budget_bytes: usage.shared_lease_budget_bytes,
            shared_lease_peak_bytes: usage.shared_lease_peak_bytes,
            ocr: usage.ocr.map(|ocr| BatchOcrUsageDto {
                recognized_regions: ocr.recognized_regions,
                recognized_chars: ocr.recognized_chars,
            }),
        }),
        items: raw
            .items
            .into_iter()
            .map(|item| BatchItemDto {
                outcome: match item.outcome {
                    Some(RawBatchItemOutcome::Complete) => BatchItemOutcome::Complete,
                    Some(RawBatchItemOutcome::Degraded) => BatchItemOutcome::Degraded,
                    Some(RawBatchItemOutcome::Failed) => BatchItemOutcome::Failed,
                    None => match item.status {
                        RawBatchItemStatus::Success => BatchItemOutcome::Complete,
                        RawBatchItemStatus::Failed => BatchItemOutcome::Failed,
                    },
                },
                input: item.input,
                output: item.output,
                format: item.format,
                status: match item.status {
                    RawBatchItemStatus::Success => BatchItemStatus::Success,
                    RawBatchItemStatus::Failed => BatchItemStatus::Failed,
                },
                diagnostics: item.diagnostics.into_iter().map(DiagnosticDto::from).collect(),
                error_code: item.error_code,
                reason_code: item.reason_code,
                component: item.component,
                part: item.part,
                limit: item
                    .limit
                    .map(|limit| BatchLimitDto { name: limit.name, detail: limit.detail }),
                message: item.message,
                warnings: item.warnings,
                duration_ms: item.duration_ms,
                processing_duration_ms: item.processing_duration_ms,
            })
            .collect(),
    })
}

fn validate_duration(value: Option<f64>, path: &str) -> Result<(), DtoError> {
    if value.is_some_and(|duration| !duration.is_finite() || duration < 0.0) {
        Err(DtoError::new(
            DtoErrorCode::InvalidField,
            path,
            "duration must be finite and non-negative",
        ))
    } else {
        Ok(())
    }
}

fn validate_version(version: u32) -> Result<(), DtoError> {
    if version == DTO_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(DtoError::new(
            DtoErrorCode::UnsupportedSchemaVersion,
            "$.schemaVersion",
            format!("expected {DTO_SCHEMA_VERSION}, got {version}"),
        ))
    }
}

fn padded_base64_encoded_len(raw_bytes: usize) -> Option<usize> {
    (raw_bytes / 3).checked_mul(4)?.checked_add(if raw_bytes.is_multiple_of(3) { 0 } else { 4 })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WireAccounting {
    json_bytes: usize,
    depth: usize,
    max_depth: usize,
    values: usize,
    in_string: bool,
    escaped: bool,
    string_bytes: usize,
    max_string_bytes: usize,
    total_string_bytes: usize,
}

impl WireAccounting {
    fn add_base64_string(&mut self, encoded_bytes: usize) -> Result<(), DtoError> {
        self.json_bytes = self.json_bytes.checked_add(encoded_bytes).ok_or_else(|| {
            DtoError::new(DtoErrorCode::ResourceLimit, "$", "wire JSON byte count overflow")
        })?;
        self.max_string_bytes = self.max_string_bytes.max(encoded_bytes);
        self.total_string_bytes =
            self.total_string_bytes.checked_add(encoded_bytes).ok_or_else(|| {
                DtoError::new(DtoErrorCode::ResourceLimit, "$", "wire string byte count overflow")
            })?;
        Ok(())
    }

    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        if self.json_bytes > limits.max_json_bytes {
            return limit("$", "dtoJsonBytes", limits.max_json_bytes);
        }
        if self.max_depth > limits.max_depth {
            return limit("$", "dtoDepth", limits.max_depth);
        }
        if self.values > limits.max_values {
            return limit("$", "dtoValues", limits.max_values);
        }
        if self.max_string_bytes > limits.max_string_bytes {
            return limit("$", "dtoStringBytes", limits.max_string_bytes);
        }
        if self.total_string_bytes > limits.max_total_string_bytes {
            return limit("$", "dtoTotalStringBytes", limits.max_total_string_bytes);
        }
        Ok(())
    }
}

impl Write for WireAccounting {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.json_bytes = self
            .json_bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("wire JSON byte count overflow"))?;
        for &byte in buffer {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                    self.string_bytes = self
                        .string_bytes
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("wire string byte count overflow"))?;
                } else if byte == b'\\' {
                    self.escaped = true;
                    self.string_bytes = self
                        .string_bytes
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("wire string byte count overflow"))?;
                } else if byte == b'"' {
                    self.in_string = false;
                    self.max_string_bytes = self.max_string_bytes.max(self.string_bytes);
                    self.total_string_bytes = self
                        .total_string_bytes
                        .checked_add(self.string_bytes)
                        .ok_or_else(|| io::Error::other("wire string byte count overflow"))?;
                } else {
                    self.string_bytes = self
                        .string_bytes
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("wire string byte count overflow"))?;
                }
                continue;
            }
            match byte {
                b'"' => {
                    self.in_string = true;
                    self.string_bytes = 0;
                }
                b'{' | b'[' => {
                    self.depth = self
                        .depth
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("wire depth count overflow"))?;
                    self.max_depth = self.max_depth.max(self.depth);
                    self.values = self
                        .values
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("wire value count overflow"))?;
                }
                b':' | b',' => {
                    self.values = self
                        .values
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("wire value count overflow"))?;
                }
                b'}' | b']' => self.depth = self.depth.saturating_sub(1),
                _ => {}
            }
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_internal_result_placeholder<W: Write>(
    value: &crate::ConversionResult,
    style: DtoJsonStyle,
    writer: &mut W,
) -> Result<(), serde_json::Error> {
    let wire = InternalResultWire {
        schema_version: DTO_SCHEMA_VERSION,
        markdown: &value.markdown,
        document: &value.document,
        assets: InternalAssetsWire { assets: &value.assets, include_base64: false },
        diagnostics: InternalDiagnosticsWire(&value.diagnostics),
        provenance: InternalProvenanceWire(&value.provenance),
    };
    match style {
        DtoJsonStyle::Compact => serde_json::to_writer(writer, &wire),
        DtoJsonStyle::Pretty => serde_json::to_writer_pretty(writer, &wire),
    }
}

fn account_internal_result_wire(
    value: &crate::ConversionResult,
    style: DtoJsonStyle,
) -> Result<WireAccounting, DtoError> {
    let mut accounting = WireAccounting::default();
    serialize_internal_result_placeholder(value, style, &mut accounting).map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("account result wire JSON: {error}"))
    })?;
    for asset in &value.assets {
        let encoded_bytes = padded_base64_encoded_len(asset.bytes.len()).ok_or_else(|| {
            DtoError::new(
                DtoErrorCode::ResourceLimit,
                "$.assets",
                "base64 encoded size cannot be represented",
            )
        })?;
        accounting.add_base64_string(encoded_bytes)?;
    }
    Ok(accounting)
}

fn write_internal_result_wire<W: Write>(
    value: &crate::ConversionResult,
    style: DtoJsonStyle,
    writer: &mut W,
) -> Result<(), DtoError> {
    let wire = InternalResultWire {
        schema_version: DTO_SCHEMA_VERSION,
        markdown: &value.markdown,
        document: &value.document,
        assets: InternalAssetsWire { assets: &value.assets, include_base64: true },
        diagnostics: InternalDiagnosticsWire(&value.diagnostics),
        provenance: InternalProvenanceWire(&value.provenance),
    };
    let result = match style {
        DtoJsonStyle::Compact => serde_json::to_writer(&mut *writer, &wire),
        DtoJsonStyle::Pretty => serde_json::to_writer_pretty(&mut *writer, &wire),
    };
    result.map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("write result wire JSON: {error}"))
    })
}

fn preflight_internal_asset_lengths(
    asset_count: usize,
    lengths: impl IntoIterator<Item = usize>,
    limits: &DtoLimits,
) -> Result<(), DtoError> {
    if asset_count > limits.max_assets {
        return limit("$.assets", "assets", limits.max_assets);
    }
    let mut total_raw = 0_usize;
    let mut total_encoded = 0_usize;
    for (index, raw_bytes) in lengths.into_iter().enumerate() {
        let path = format!("$.assets[{index}].dataBase64");
        let encoded_bytes = padded_base64_encoded_len(raw_bytes).ok_or_else(|| {
            DtoError::new(
                DtoErrorCode::ResourceLimit,
                &path,
                "base64 encoded size cannot be represented",
            )
        })?;
        if encoded_bytes > limits.max_string_bytes {
            return limit(&path, "dtoStringBytes", limits.max_string_bytes);
        }
        total_raw = total_raw.checked_add(raw_bytes).ok_or_else(|| {
            DtoError::new(DtoErrorCode::ResourceLimit, "$.assets", "asset byte size overflow")
        })?;
        if total_raw > limits.max_base64_bytes {
            return limit("$.assets", "base64Bytes", limits.max_base64_bytes);
        }
        total_encoded = total_encoded.checked_add(encoded_bytes).ok_or_else(|| {
            DtoError::new(DtoErrorCode::ResourceLimit, "$.assets", "base64 encoded size overflow")
        })?;
        if total_encoded > limits.max_total_string_bytes {
            return limit("$.assets", "dtoTotalStringBytes", limits.max_total_string_bytes);
        }
        if total_encoded > limits.max_json_bytes {
            return limit("$.assets", "dtoJsonBytes", limits.max_json_bytes);
        }
    }
    Ok(())
}

fn preflight_internal_assets(assets: &[Asset], limits: &DtoLimits) -> Result<(), DtoError> {
    preflight_internal_asset_lengths(
        assets.len(),
        assets.iter().map(|asset| asset.bytes.len()),
        limits,
    )?;
    let mut ids = BTreeSet::new();
    for (index, asset) in assets.iter().enumerate() {
        let path = format!("$.assets[{index}]");
        validate_id(&asset.id.0, &format!("{path}.id"))?;
        if asset.media_type.trim().is_empty() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{path}.mediaType"),
                "media type must not be empty",
            ));
        }
        if let Some(uri) = &asset.external_uri {
            validate_external_uri(uri, &format!("{path}.externalUri"))?;
        }
        if asset.bytes.is_empty() && asset.external_uri.is_none() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                path,
                "asset requires content or a safe external URI",
            ));
        }
        if !ids.insert(&asset.id.0) {
            return Err(DtoError::new(
                DtoErrorCode::DuplicateId,
                format!("{path}.id"),
                "duplicate asset ID",
            ));
        }
    }
    Ok(())
}

fn validate_internal_result(
    result: &crate::ConversionResult,
    limits: &DtoLimits,
) -> Result<(), DtoError> {
    result.document.validate().map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidField, "$.document", error.to_string())
    })?;
    preflight_internal_assets(&result.assets, limits)?;
    if result.diagnostics.len() > limits.max_diagnostics {
        return limit("$.diagnostics", "diagnostics", limits.max_diagnostics);
    }
    for (index, diagnostic) in result.diagnostics.iter().enumerate() {
        if diagnostic.code.is_empty() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("$.diagnostics[{index}].code"),
                "diagnostic code must not be empty",
            ));
        }
        if let Some(locator) = &diagnostic.locator {
            validate_locator(locator, &format!("$.diagnostics[{index}].locator"))?;
        }
    }
    if result.provenance.len() > limits.max_provenance {
        return limit("$.provenance", "provenance", limits.max_provenance);
    }
    for (index, provenance) in result.provenance.iter().enumerate() {
        let path = format!("$.provenance[{index}]");
        if provenance.provider.is_empty() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{path}.provider"),
                "provider must not be empty",
            ));
        }
        if provenance
            .confidence
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{path}.confidence"),
                "confidence must be finite and between 0 and 1",
            ));
        }
        validate_locator(&provenance.locator, &format!("{path}.locator"))?;
    }
    Ok(())
}

#[cfg(test)]
std::thread_local! {
    static ASSET_BASE64_ENCODE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn encode_asset_base64(bytes: &[u8]) -> String {
    #[cfg(test)]
    ASSET_BASE64_ENCODE_CALLS.set(ASSET_BASE64_ENCODE_CALLS.get() + 1);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn validate_assets(assets: &[AssetDto], limits: &DtoLimits) -> Result<(), DtoError> {
    if assets.len() > limits.max_assets {
        return limit("$.assets", "assets", limits.max_assets);
    }
    let mut ids = BTreeSet::new();
    let mut total = 0_usize;
    for (index, asset) in assets.iter().enumerate() {
        let path = format!("$.assets[{index}]");
        validate_id(&asset.id, &format!("{path}.id"))?;
        if asset.media_type.trim().is_empty() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{path}.mediaType"),
                "media type must not be empty",
            ));
        }
        if let Some(uri) = &asset.external_uri {
            validate_external_uri(uri, &format!("{path}.externalUri"))?;
        }
        if asset.data_base64.is_empty() && asset.external_uri.is_none() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                path.clone(),
                "asset requires base64 content or a safe external URI",
            ));
        }
        if !ids.insert(&asset.id) {
            return Err(DtoError::new(
                DtoErrorCode::DuplicateId,
                format!("{path}.id"),
                "duplicate asset ID",
            ));
        }
        let remaining = limits.max_base64_bytes.saturating_sub(total);
        let maximum_encoded = remaining.saturating_mul(4).saturating_add(2) / 3 + 4;
        if asset.data_base64.len() > limits.max_string_bytes {
            return limit(&format!("{path}.dataBase64"), "dtoStringBytes", limits.max_string_bytes);
        }
        if asset.data_base64.len() > maximum_encoded {
            return limit("$.assets", "base64Bytes", limits.max_base64_bytes);
        }
        let decoded =
            base64::engine::general_purpose::STANDARD.decode(&asset.data_base64).map_err(|_| {
                DtoError::new(
                    DtoErrorCode::InvalidBase64,
                    format!("{path}.dataBase64"),
                    "expected canonical standard padded base64",
                )
            })?;
        if base64::engine::general_purpose::STANDARD.encode(&decoded) != asset.data_base64 {
            return Err(DtoError::new(
                DtoErrorCode::InvalidBase64,
                format!("{path}.dataBase64"),
                "expected canonical standard padded base64",
            ));
        }
        total = total.checked_add(decoded.len()).ok_or_else(|| {
            DtoError::new(DtoErrorCode::ResourceLimit, "$.assets", "decoded asset size overflow")
        })?;
        if total > limits.max_base64_bytes {
            return limit("$.assets", "base64Bytes", limits.max_base64_bytes);
        }
    }
    Ok(())
}

fn validate_diagnostics(
    values: &[DiagnosticDto],
    limits: &DtoLimits,
    path: &str,
) -> Result<(), DtoError> {
    if values.len() > limits.max_diagnostics {
        return limit(path, "diagnostics", limits.max_diagnostics);
    }
    for (index, value) in values.iter().enumerate() {
        if value.code.is_empty() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{path}[{index}].code"),
                "diagnostic code must not be empty",
            ));
        }
        if let Some(locator) = &value.locator {
            validate_locator(locator, &format!("{path}[{index}].locator"))?;
        }
    }
    Ok(())
}

fn validate_internal_diagnostics(
    values: &[Diagnostic],
    limits: &DtoLimits,
    path: &str,
) -> Result<(), DtoError> {
    if values.len() > limits.max_diagnostics {
        return limit(path, "diagnostics", limits.max_diagnostics);
    }
    for (index, value) in values.iter().enumerate() {
        if value.code.is_empty() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{path}[{index}].code"),
                "diagnostic code must not be empty",
            ));
        }
        if let Some(locator) = &value.locator {
            validate_locator(locator, &format!("{path}[{index}].locator"))?;
        }
    }
    Ok(())
}

fn validate_provenance(
    values: &[ProvenanceDto],
    limits: &DtoLimits,
    path: &str,
) -> Result<(), DtoError> {
    if values.len() > limits.max_provenance {
        return limit(path, "provenance", limits.max_provenance);
    }
    for (index, value) in values.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        if value.provider.is_empty() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{item_path}.provider"),
                "provider must not be empty",
            ));
        }
        if value
            .confidence
            .is_some_and(|confidence| !confidence.is_finite() || !(0.0..=1.0).contains(&confidence))
        {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{item_path}.confidence"),
                "confidence must be finite and between 0 and 1",
            ));
        }
        validate_locator(&value.locator, &format!("{item_path}.locator"))?;
    }
    Ok(())
}

fn validate_internal_provenance(
    values: &[Provenance],
    limits: &DtoLimits,
    path: &str,
) -> Result<(), DtoError> {
    if values.len() > limits.max_provenance {
        return limit(path, "provenance", limits.max_provenance);
    }
    for (index, value) in values.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        if value.provider.is_empty() {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{item_path}.provider"),
                "provider must not be empty",
            ));
        }
        if value
            .confidence
            .is_some_and(|confidence| !confidence.is_finite() || !(0.0..=1.0).contains(&confidence))
        {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                format!("{item_path}.confidence"),
                "confidence must be finite and between 0 and 1",
            ));
        }
        validate_locator(&value.locator, &format!("{item_path}.locator"))?;
    }
    Ok(())
}

fn validate_locator(locator: &SourceLocator, path: &str) -> Result<(), DtoError> {
    if locator.byte_start.is_some() != locator.byte_end.is_some()
        || locator.byte_start.zip(locator.byte_end).is_some_and(|(start, end)| start > end)
    {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            format!("{path}.byteStart"),
            "byteStart and byteEnd must be present together and form an ordered half-open range",
        ));
    }
    if locator.page == Some(0) {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            format!("{path}.page"),
            "page numbers are one-based",
        ));
    }
    if locator.slide == Some(0) {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            format!("{path}.slide"),
            "slide numbers are one-based",
        ));
    }
    if locator.sheet.as_ref().is_some_and(|value| value.trim().is_empty()) {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            format!("{path}.sheet"),
            "worksheet name must not be empty",
        ));
    }
    if locator.cell.is_some() && locator.sheet.is_none() {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            format!("{path}.cell"),
            "cell coordinates require a worksheet name",
        ));
    }
    if let Some(bounds) = locator.bounds
        && (![bounds.x, bounds.y, bounds.width, bounds.height]
            .iter()
            .all(|value| value.is_finite())
            || bounds.width < 0.0
            || bounds.height < 0.0)
    {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            format!("{path}.bounds"),
            "rectangle values must be finite with non-negative dimensions",
        ));
    }
    if let Some(range) = locator.time
        && range.start_ms >= range.end_ms
    {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            format!("{path}.time"),
            "time range start must precede end",
        ));
    }
    if let Some(part) = &locator.part
        && (part.len() > 1024 || !is_safe_container_part_name(part))
    {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            format!("{path}.part"),
            "expected a bounded safe container-relative part name",
        ));
    }
    Ok(())
}

/// Return the canonical external-only asset URI accepted by every public boundary.
#[must_use]
pub fn canonical_external_asset_uri(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    for index in 0..bytes.len() {
        if bytes[index] == b'%'
            && (index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            return None;
        }
    }
    let mut url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    if url.set_username("").is_err() || url.set_password(None).is_err() {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn validate_external_uri(value: &str, path: &str) -> Result<(), DtoError> {
    let sanitized = canonical_external_asset_uri(value).ok_or_else(|| {
        DtoError::new(
            DtoErrorCode::InvalidField,
            path,
            "external URI must be an absolute HTTP(S) URL without local-file semantics",
        )
    })?;
    if sanitized != value {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            path,
            "external URI must not contain user information, query, or fragment",
        ));
    }
    Ok(())
}

fn validate_id(id: &str, path: &str) -> Result<(), DtoError> {
    if id.is_empty() || id.chars().any(char::is_control) {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            path,
            "ID must be non-empty and contain no control characters",
        ));
    }
    Ok(())
}

fn validate_bundle_path(value: &str, path: &str) -> Result<(), DtoError> {
    const MAX_BUNDLE_PATH_BYTES: usize = 1024;
    const MAX_BUNDLE_SEGMENT_BYTES: usize = 240;
    if value.is_empty()
        || value.len() > MAX_BUNDLE_PATH_BYTES
        || !value.is_ascii()
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.contains('\\')
        || value.contains('\0')
        || value.split('/').any(|part| {
            part.is_empty()
                || part.len() > MAX_BUNDLE_SEGMENT_BYTES
                || matches!(part, "." | "..")
                || part.ends_with(['.', ' '])
                || part.bytes().any(|byte| {
                    !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
                })
                || is_windows_reserved_segment(part)
        })
    {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            path,
            "expected a portable ASCII bundle-relative path",
        ));
    }
    Ok(())
}

fn portable_path_key(value: &str) -> String {
    value.to_ascii_lowercase()
}

fn is_windows_reserved_segment(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || upper.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || upper.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

fn limit<T>(path: &str, name: &str, maximum: usize) -> Result<T, DtoError> {
    Err(DtoError::new(DtoErrorCode::ResourceLimit, path, format!("{name} exceeds limit {maximum}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AssetId, Block, BlockNode, ConversionResult, Inline, NodeId, OcrEvidence, OcrEvidenceStage,
        OcrEvidenceStep, OcrSourceRegion, Rect, SourcePoint,
    };

    fn result_dto() -> ResultDto {
        let result = ConversionResult {
            document: Document::default(),
            markdown: "# Example\n".into(),
            assets: vec![Asset {
                id: AssetId("image-1".into()),
                filename: Some("image.png".into()),
                media_type: "image/png".into(),
                bytes: vec![1, 2, 3],
                external_uri: None,
            }],
            diagnostics: vec![Diagnostic {
                code: "recoveredText".into(),
                severity: DiagnosticSeverity::Warning,
                message: "text was recovered".into(),
                locator: None,
            }],
            provenance: vec![Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: "example-parser".into(),
                locator: SourceLocator::default(),
                confidence: Some(1.0),
            }],
            detected_format: None,
            processing_duration_ms: None,
            memory_lease: crate::spi::OutputMemoryLease::default(),
        };
        ResultDto::from_json(&ResultDto::json_from_result(&result, DtoJsonStyle::Compact).unwrap())
            .unwrap()
    }

    fn result_value() -> serde_json::Value {
        serde_json::from_str(&result_dto().to_json().unwrap()).unwrap()
    }

    #[test]
    fn result_json_golden_and_roundtrip_are_stable() {
        let dto = result_dto();
        let json = dto.to_json().unwrap();
        assert_eq!(
            json,
            r##"{"schemaVersion":1,"markdown":"# Example\n","document":{"schemaVersion":1,"metadata":{"title":null,"authors":[],"properties":{}},"blocks":[]},"assets":[{"id":"image-1","filename":"image.png","mediaType":"image/png","dataBase64":"AQID","externalUri":null}],"diagnostics":[{"code":"recoveredText","severity":"warning","message":"text was recovered","locator":null}],"provenance":[{"kind":"nativeParser","provider":"example-parser","locator":{"page":null,"slide":null,"sheet":null,"cell":null,"bounds":null,"time":null,"part":null},"confidence":1.0}]}"##
        );
        assert_eq!(ResultDto::from_json(&json).unwrap(), dto);
        let internal = ConversionResult::try_from(dto.clone()).unwrap();
        assert_eq!(internal.assets[0].bytes, [1, 2, 3]);
        assert_eq!(ResultDto::json_from_result(&internal, DtoJsonStyle::Compact).unwrap(), json);
        assert_eq!(
            ResultDto::json_from_result(&internal, DtoJsonStyle::Pretty).unwrap(),
            dto.to_pretty_json().unwrap()
        );
    }

    #[test]
    fn source_text_round_trips_through_result_dto() {
        let source = Inline::SourceText {
            value: "文".into(),
            marks: vec![],
            provenance: Box::new(Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: "pdfium".into(),
                locator: SourceLocator {
                    page: Some(1),
                    character_index: Some(7),
                    ..SourceLocator::default()
                },
                confidence: None,
            }),
        };
        let result = ConversionResult {
            document: Document {
                blocks: vec![BlockNode {
                    id: NodeId("source".into()),
                    block: Block::Paragraph(vec![source]),
                    provenance: Provenance {
                        kind: ProvenanceKind::NativeParser,
                        provider: "pdfium".into(),
                        locator: SourceLocator { page: Some(1), ..SourceLocator::default() },
                        confidence: None,
                    },
                }],
                ..Document::default()
            },
            markdown: String::new(),
            assets: Vec::new(),
            diagnostics: Vec::new(),
            provenance: Vec::new(),
            detected_format: None,
            processing_duration_ms: None,
            memory_lease: crate::spi::OutputMemoryLease::default(),
        };
        let json = ResultDto::json_from_result(&result, DtoJsonStyle::Compact).unwrap();
        assert!(json.contains("\"type\":\"sourceText\""));
        let decoded = ConversionResult::try_from(ResultDto::from_json(&json).unwrap()).unwrap();
        assert_eq!(decoded.document, result.document);
    }

    #[test]
    fn ocr_evidence_round_trips_without_changing_the_envelope_schema() {
        let provenance = Provenance {
            kind: ProvenanceKind::LocalOcr,
            provider: "recognizer".into(),
            locator: SourceLocator {
                page: Some(2),
                bounds: Some(Rect { x: 1.0, y: 1.0, width: 4.0, height: 2.0 }),
                page_width: Some(100.0),
                page_height: Some(100.0),
                ..SourceLocator::default()
            },
            confidence: Some(0.91),
        };
        let ocr = Inline::OcrText {
            value: "scan".into(),
            marks: vec![],
            provenance: Box::new(provenance.clone()),
            evidence: Box::new(OcrEvidence {
                page: 2,
                regions: vec![OcrSourceRegion {
                    source_index: 4,
                    polygon: [
                        SourcePoint { x: 1.0, y: 1.0 },
                        SourcePoint { x: 5.0, y: 1.0 },
                        SourcePoint { x: 5.0, y: 3.0 },
                        SourcePoint { x: 1.0, y: 3.0 },
                    ],
                    detection_confidence: 0.95,
                    recognition_confidence: 0.91,
                }],
                chain: vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: "detector".into(),
                        model: Some("det".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: "recognizer".into(),
                        model: Some("rec".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Merge,
                        provider: "merge".into(),
                        model: None,
                    },
                ],
            }),
        };
        let result = ConversionResult::new(
            Document {
                blocks: vec![BlockNode {
                    id: NodeId("ocr".into()),
                    block: Block::Paragraph(vec![ocr]),
                    provenance,
                }],
                ..Document::default()
            },
            String::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let json = ResultDto::json_from_result(&result, DtoJsonStyle::Compact).unwrap();
        assert!(json.contains("\"schemaVersion\":1"));
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"ocrEvidence\":"));
        let decoded = ConversionResult::try_from(ResultDto::from_json(&json).unwrap()).unwrap();
        assert_eq!(decoded.document, result.document);
    }

    #[test]
    fn emitted_wire_json_always_fits_the_default_decoder() {
        let mut internal = ConversionResult::try_from(result_dto()).unwrap();
        internal.markdown = "m".repeat(MAX_DTO_STRING_BYTES - 1024);
        internal.assets[0].bytes = vec![7_u8; 6 * 1024 * 1024];
        let json = ResultDto::json_from_result(&internal, DtoJsonStyle::Compact).unwrap();
        let dto = ResultDto::from_json(&json).unwrap();
        assert_eq!(dto.assets[0].data_base64.len(), MAX_DTO_STRING_BYTES);

        let mut oversized = result_dto();
        oversized.markdown = "x".repeat(49 * 1024 * 1024);
        assert_eq!(oversized.to_json().unwrap_err().code, DtoErrorCode::ResourceLimit);

        let mut oversized_asset = result_dto();
        oversized_asset.assets[0].data_base64 =
            base64::engine::general_purpose::STANDARD.encode(vec![0_u8; 6 * 1024 * 1024 + 1]);
        assert_eq!(oversized_asset.to_json().unwrap_err().code, DtoErrorCode::ResourceLimit);
    }

    #[test]
    fn internal_asset_preflight_is_checked_and_stops_before_encoding() {
        let limits = DtoLimits::default();
        assert_eq!(padded_base64_encoded_len(6 * 1024 * 1024), Some(MAX_DTO_STRING_BYTES));
        assert!(preflight_internal_asset_lengths(1, [6 * 1024 * 1024], &limits).is_ok());
        assert_eq!(
            preflight_internal_asset_lengths(1, [64 * 1024 * 1024], &limits).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );
        assert_eq!(padded_base64_encoded_len(usize::MAX), None);

        let visited = std::cell::Cell::new(0_usize);
        let lengths = [3_usize, 3, usize::MAX].into_iter().inspect(|_| {
            visited.set(visited.get() + 1);
        });
        let tight = DtoLimits { max_base64_bytes: 5, ..limits };
        assert_eq!(
            preflight_internal_asset_lengths(3, lengths, &tight).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );
        assert_eq!(visited.get(), 2);
    }

    #[test]
    fn complete_result_wire_is_budgeted_before_any_base64_encoding() {
        let assets = (0..6)
            .map(|index| Asset {
                id: AssetId(format!("asset-{index}")),
                filename: Some(format!("{}-{index}.bin", "f".repeat(2 * 1024 * 1024))),
                media_type: "application/octet-stream".into(),
                bytes: vec![0_u8; 5 * 1024 * 1024],
                external_uri: None,
            })
            .collect();
        let oversized = ConversionResult {
            document: Document::default(),
            markdown: "m".repeat(MAX_DTO_STRING_BYTES),
            assets,
            diagnostics: vec![],
            provenance: vec![],
            detected_format: None,
            processing_duration_ms: None,
            memory_lease: crate::spi::OutputMemoryLease::default(),
        };
        ASSET_BASE64_ENCODE_CALLS.set(0);
        let mut destination = Vec::new();
        assert_eq!(
            ResultDto::write_json_from_result(&oversized, DtoJsonStyle::Compact, &mut destination)
                .unwrap_err()
                .code,
            DtoErrorCode::ResourceLimit
        );
        assert!(destination.is_empty());
        assert_eq!(ASSET_BASE64_ENCODE_CALLS.get(), 0);

        let long_metadata = ConversionResult {
            document: Document::default(),
            markdown: String::new(),
            assets: vec![Asset {
                id: AssetId("metadata".into()),
                filename: Some("f".repeat(MAX_DTO_STRING_BYTES + 1)),
                media_type: "application/octet-stream".into(),
                bytes: vec![1],
                external_uri: None,
            }],
            diagnostics: vec![],
            provenance: vec![],
            detected_format: None,
            processing_duration_ms: None,
            memory_lease: crate::spi::OutputMemoryLease::default(),
        };
        ASSET_BASE64_ENCODE_CALLS.set(0);
        assert_eq!(
            ResultDto::json_from_result(&long_metadata, DtoJsonStyle::Compact).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );
        assert_eq!(ASSET_BASE64_ENCODE_CALLS.get(), 0);
    }

    #[test]
    fn internal_wire_accounting_matches_private_result_serializer() {
        let mut internal = ConversionResult::try_from(result_dto()).unwrap();
        internal.markdown = "markdown \\\" escaped 中文".into();
        internal.document.metadata.title = Some("document \\\" title".into());
        internal
            .document
            .metadata
            .properties
            .insert("property".into(), "value \\\" escaped".into());
        internal.assets[0].filename = Some("asset \\\" name.png".into());
        internal.diagnostics[0].message = "diagnostic \\\" detail".into();
        internal.provenance[0].provider = "provider \\\" id".into();

        let expected = account_internal_result_wire(&internal, DtoJsonStyle::Compact).unwrap();
        let json = ResultDto::json_from_result(&internal, DtoJsonStyle::Compact).unwrap();
        let mut actual = WireAccounting::default();
        actual.write_all(json.as_bytes()).unwrap();
        assert_eq!(expected, actual);

        let limits = DtoLimits {
            max_total_string_bytes: expected.total_string_bytes - 1,
            ..DtoLimits::default()
        };
        assert_eq!(
            account_internal_result_wire(&internal, DtoJsonStyle::Compact)
                .unwrap()
                .validate(&limits)
                .unwrap_err()
                .code,
            DtoErrorCode::ResourceLimit
        );
    }

    #[test]
    fn pretty_layout_is_rejected_before_base64_when_only_compact_fits() {
        let sizes = [5, 5, 5, 5, 4, 4];
        let assets = sizes
            .into_iter()
            .enumerate()
            .map(|(index, mebibytes)| Asset {
                id: AssetId(format!("asset-{index}")),
                filename: Some(format!("asset-{index}.bin")),
                media_type: "application/octet-stream".into(),
                bytes: vec![0_u8; mebibytes * 1024 * 1024],
                external_uri: None,
            })
            .collect();
        let provenance = (0..85_000)
            .map(|_| Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: "p".into(),
                locator: SourceLocator::default(),
                confidence: None,
            })
            .collect();
        let result = ConversionResult {
            document: Document::default(),
            markdown: "m".repeat(256 * 1024),
            assets,
            diagnostics: vec![],
            provenance,
            detected_format: None,
            processing_duration_ms: None,
            memory_lease: crate::spi::OutputMemoryLease::default(),
        };
        let limits = DtoLimits::default();
        let compact = account_internal_result_wire(&result, DtoJsonStyle::Compact).unwrap();
        let pretty = account_internal_result_wire(&result, DtoJsonStyle::Pretty).unwrap();
        let layout_limit = pretty.json_bytes - 1;
        let limits = DtoLimits { max_json_bytes: layout_limit, ..limits };
        assert!(compact.json_bytes < limits.max_json_bytes);
        assert!(compact.validate(&limits).is_ok(), "compact accounting: {compact:?}");
        assert!(
            pretty.json_bytes > limits.max_json_bytes,
            "compact={compact:?}, pretty={pretty:?}"
        );

        ASSET_BASE64_ENCODE_CALLS.set(0);
        let mut destination = Vec::new();
        assert_eq!(
            ResultDto::write_json_from_result_with_limits(
                &result,
                DtoJsonStyle::Pretty,
                &limits,
                &mut destination,
            )
            .unwrap_err()
            .code,
            DtoErrorCode::ResourceLimit
        );
        assert!(destination.is_empty());
        assert_eq!(ASSET_BASE64_ENCODE_CALLS.get(), 0);
    }

    #[test]
    fn borrowed_writer_streams_base64_and_stops_on_destination_failure() {
        struct FailAfter {
            written: usize,
            maximum: usize,
        }

        impl Write for FailAfter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if self.written == self.maximum {
                    return Err(io::Error::other("destination full"));
                }
                let accepted = buffer.len().min(self.maximum - self.written);
                self.written += accepted;
                Ok(accepted)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let result = ConversionResult {
            document: Document::default(),
            markdown: String::new(),
            assets: vec![Asset {
                id: AssetId("streamed".into()),
                filename: None,
                media_type: "application/octet-stream".into(),
                bytes: vec![1_u8; 6 * 1024 * 1024],
                external_uri: None,
            }],
            diagnostics: vec![],
            provenance: vec![],
            detected_format: None,
            processing_duration_ms: None,
            memory_lease: crate::spi::OutputMemoryLease::default(),
        };
        ASSET_BASE64_ENCODE_CALLS.set(0);
        let mut destination = FailAfter { written: 0, maximum: 1024 };
        assert!(
            ResultDto::write_json_from_result(&result, DtoJsonStyle::Compact, &mut destination)
                .is_err()
        );
        assert_eq!(destination.written, destination.maximum);
        assert_eq!(ASSET_BASE64_ENCODE_CALLS.get(), 1);
    }

    #[test]
    fn user_strings_cannot_be_confused_with_base64_wire_fields() {
        let marker = r#"\"dataBase64\":\"\""#;
        let result = ConversionResult {
            document: Document::default(),
            markdown: format!("before {marker} after"),
            assets: vec![Asset {
                id: AssetId("marker".into()),
                filename: Some(format!("name-{marker}")),
                media_type: "application/octet-stream".into(),
                bytes: vec![1, 2, 3],
                external_uri: None,
            }],
            diagnostics: vec![Diagnostic {
                code: "marker".into(),
                severity: DiagnosticSeverity::Info,
                message: marker.into(),
                locator: None,
            }],
            provenance: vec![],
            detected_format: None,
            processing_duration_ms: None,
            memory_lease: crate::spi::OutputMemoryLease::default(),
        };
        for style in [DtoJsonStyle::Compact, DtoJsonStyle::Pretty] {
            ASSET_BASE64_ENCODE_CALLS.set(0);
            let json = ResultDto::json_from_result(&result, style).unwrap();
            let decoded = ResultDto::from_json(&json).unwrap();
            assert_eq!(decoded.markdown, result.markdown);
            assert_eq!(decoded.assets[0].filename.as_deref(), result.assets[0].filename.as_deref());
            assert_eq!(decoded.assets[0].data_base64, "AQID");
            assert_eq!(decoded.diagnostics[0].message, marker);
            assert_eq!(ASSET_BASE64_ENCODE_CALLS.get(), 1);
        }
    }

    #[test]
    fn auxiliary_envelope_json_goldens_are_stable() {
        let result = result_dto();
        let diagnostics =
            DiagnosticsDto { schema_version: DTO_SCHEMA_VERSION, diagnostics: result.diagnostics };
        assert_eq!(
            diagnostics.to_json().unwrap(),
            r#"{"schemaVersion":1,"diagnostics":[{"code":"recoveredText","severity":"warning","message":"text was recovered","locator":null}]}"#
        );
        let provenance =
            ProvenanceListDto { schema_version: DTO_SCHEMA_VERSION, provenance: result.provenance };
        assert_eq!(
            provenance.to_json().unwrap(),
            r#"{"schemaVersion":1,"provenance":[{"kind":"nativeParser","provider":"example-parser","locator":{"page":null,"slide":null,"sheet":null,"cell":null,"bounds":null,"time":null,"part":null},"confidence":1.0}]}"#
        );
        let manifest = BundleManifestDto {
            schema_version: DTO_SCHEMA_VERSION,
            markdown: "document.md".into(),
            document_ir: "document.ir.json".into(),
            diagnostics: "diagnostics.json".into(),
            diagnostics_schema_version: DTO_SCHEMA_VERSION,
            provenance: "provenance.json".into(),
            provenance_schema_version: DTO_SCHEMA_VERSION,
            assets: vec![],
        };
        assert_eq!(
            manifest.to_json().unwrap(),
            r#"{"schemaVersion":1,"markdown":"document.md","documentIr":"document.ir.json","diagnostics":"diagnostics.json","diagnosticsSchemaVersion":1,"provenance":"provenance.json","provenanceSchemaVersion":1,"assets":[]}"#
        );
        let legacy_manifest = r#"{"schemaVersion":1,"markdown":"document.md","documentIr":"document.ir.json","diagnostics":"diagnostics.json","provenance":"provenance.json","assets":[]}"#;
        assert_eq!(
            BundleManifestDto::from_json(legacy_manifest).unwrap().diagnostics_schema_version,
            DTO_SCHEMA_VERSION
        );
        let report = BatchReportDto::try_new(vec![BatchItemDto {
            input: "report.pdf".into(),
            output: Some("report.md".into()),
            format: Some("pdf".into()),
            status: BatchItemStatus::Success,
            outcome: BatchItemOutcome::Complete,
            diagnostics: vec![],
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: vec![],
            duration_ms: None,
            processing_duration_ms: None,
        }])
        .unwrap();
        assert_eq!(
            report.to_json().unwrap(),
            r#"{"schemaVersion":1,"succeeded":1,"failed":0,"items":[{"input":"report.pdf","output":"report.md","format":"pdf","status":"success","outcome":"complete","diagnostics":[],"errorCode":null,"reasonCode":null,"component":null,"part":null,"limit":null,"message":null,"warnings":[]}]}"#
        );
    }

    #[test]
    fn additive_unknown_fields_are_ignored() {
        let mut value = result_value();
        value.as_object_mut().unwrap().insert("futureField".into(), true.into());
        value["assets"][0].as_object_mut().unwrap().insert("futureAssetField".into(), true.into());
        let decoded = ResultDto::from_json(&value.to_string()).unwrap();
        assert_eq!(decoded.assets[0].id, "image-1");
    }

    #[test]
    fn batch_timing_fields_round_trip_and_legacy_reports_remain_readable() {
        let item = BatchItemDto {
            input: r"C:\work\report.pdf".into(),
            output: Some("C:/work/report.md".into()),
            format: Some("pdf".into()),
            status: BatchItemStatus::Success,
            outcome: BatchItemOutcome::Complete,
            diagnostics: vec![],
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: vec![],
            duration_ms: Some(12.34),
            processing_duration_ms: Some(9.81),
        };
        let report = BatchReportDto::try_new_with_resource_usage(
            vec![item],
            Some(15.72),
            Some(BatchResourceUsageDto {
                memory: None,
                ocr_runtime: None,
                shared_lease_budget_bytes: 2_147_483_648,
                shared_lease_peak_bytes: 123_456_789,
                ocr: Some(BatchOcrUsageDto { recognized_regions: 2, recognized_chars: 7 }),
            }),
        )
        .unwrap();
        let json = report.to_json().unwrap();
        assert!(json.contains(r#""durationMs":12.34"#));
        assert!(json.contains(r#""processingDurationMs":9.81"#));
        assert!(json.contains(r#""wallDurationMs":15.72"#));
        assert!(json.contains(r#""sharedLeaseBudgetBytes":2147483648"#));
        assert!(json.contains(r#""sharedLeasePeakBytes":123456789"#));
        assert!(json.contains(r#""recognizedRegions":2"#));
        assert!(json.contains(r#""recognizedChars":7"#));
        assert_eq!(BatchReportDto::from_json(&json).unwrap(), report);

        let legacy = r#"{"schemaVersion":1,"succeeded":1,"failed":0,"items":[{"input":"old.txt","output":"old.md","format":"text","status":"success","diagnostics":[],"errorCode":null,"message":null,"warnings":[]}]}"#;
        let decoded = BatchReportDto::from_json(legacy).unwrap();
        assert_eq!(decoded.wall_duration_ms, None);
        assert_eq!(decoded.resource_usage, None);
        assert_eq!(decoded.items[0].duration_ms, None);
        assert_eq!(decoded.items[0].processing_duration_ms, None);
    }

    #[test]
    fn batch_resource_usage_rejects_zero_budget_impossible_peak_and_fake_ocr_hits() {
        let valid = BatchResourceUsageDto {
            memory: None,
            ocr_runtime: None,
            shared_lease_budget_bytes: 10,
            shared_lease_peak_bytes: 5,
            ocr: Some(BatchOcrUsageDto { recognized_regions: 0, recognized_chars: 0 }),
        };
        assert!(
            BatchReportDto::try_new_with_resource_usage(Vec::new(), Some(0.0), Some(valid.clone()))
                .is_ok()
        );

        for (usage, path) in [
            (
                BatchResourceUsageDto { shared_lease_budget_bytes: 0, ..valid.clone() },
                "$.resourceUsage.sharedLeaseBudgetBytes",
            ),
            (
                BatchResourceUsageDto { shared_lease_peak_bytes: 11, ..valid.clone() },
                "$.resourceUsage.sharedLeasePeakBytes",
            ),
            (
                BatchResourceUsageDto {
                    ocr: Some(BatchOcrUsageDto { recognized_regions: 1, recognized_chars: 0 }),
                    ..valid
                },
                "$.resourceUsage.ocr",
            ),
        ] {
            let error = BatchReportDto::try_new_with_resource_usage(Vec::new(), None, Some(usage))
                .unwrap_err();
            assert_eq!(error.code, DtoErrorCode::InvalidField);
            assert_eq!(error.path, path);
        }
    }

    #[test]
    fn batch_timing_rejects_negative_and_non_finite_values() {
        let item = BatchItemDto {
            input: "input.txt".into(),
            output: Some("output.md".into()),
            format: Some("text".into()),
            status: BatchItemStatus::Success,
            outcome: BatchItemOutcome::Complete,
            diagnostics: vec![],
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: vec![],
            duration_ms: Some(-1.0),
            processing_duration_ms: None,
        };
        let error = BatchReportDto::try_new(vec![item.clone()]).unwrap_err();
        assert_eq!(error.code, DtoErrorCode::InvalidField);
        assert_eq!(error.path, "$.items[0].durationMs");

        let mut valid_item = item;
        valid_item.duration_ms = Some(0.0);
        let error = BatchReportDto::try_new_with_wall_duration(vec![valid_item], Some(f64::NAN))
            .unwrap_err();
        assert_eq!(error.code, DtoErrorCode::InvalidField);
        assert_eq!(error.path, "$.wallDurationMs");
    }

    #[test]
    fn duplicate_object_members_are_rejected_before_value_decoding() {
        for json in [
            r#"{"schemaVersion":2,"schemaVersion":1,"diagnostics":[]}"#,
            r#"{"schemaVersion":1,"diagnostics":[],"diagnostics":[]}"#,
            r#"{"schemaVersion":1,"diagnostics":[{"code":"a","code":"b","severity":"info","message":"m","locator":null}]}"#,
            r#"{"schemaVersion":1,"diagnostics":[],"future":{"member":1,"member":2}}"#,
            r#"{"schemaVersion":2,"schema\u0056ersion":1,"diagnostics":[]}"#,
        ] {
            let error = DiagnosticsDto::from_json(json).unwrap_err();
            assert_eq!(error.code, DtoErrorCode::InvalidJson, "accepted duplicate in {json}");
            assert!(error.detail.contains("duplicate JSON object member"));
        }
    }

    #[test]
    fn unknown_version_and_missing_required_field_are_stable_errors() {
        let mut value = result_value();
        value["schemaVersion"] = 2.into();
        assert_eq!(
            ResultDto::from_json(&value.to_string()).unwrap_err().code,
            DtoErrorCode::UnsupportedSchemaVersion
        );
        value["schemaVersion"] = 1.into();
        value.as_object_mut().unwrap().remove("markdown");
        assert_eq!(
            ResultDto::from_json(&value.to_string()).unwrap_err().code,
            DtoErrorCode::InvalidJson
        );
    }

    #[test]
    fn malicious_assets_and_duplicate_ids_are_rejected() {
        let mut value = result_value();
        value["assets"][0]["dataBase64"] = "not base64".into();
        assert_eq!(
            ResultDto::from_json(&value.to_string()).unwrap_err().code,
            DtoErrorCode::InvalidBase64
        );

        let mut dto = result_dto();
        dto.assets.push(dto.assets[0].clone());
        assert_eq!(dto.to_json().unwrap_err().code, DtoErrorCode::DuplicateId);

        let invalid = AssetDto {
            id: String::new(),
            filename: None,
            media_type: String::new(),
            data_base64: String::new(),
            external_uri: None,
        };
        assert_eq!(Asset::try_from(invalid).unwrap_err().code, DtoErrorCode::InvalidField);
    }

    #[test]
    fn external_asset_uris_are_audit_only_and_cannot_leak_secrets() {
        let error = ResultDto::json_from_result(
            &ConversionResult {
                document: Document::default(),
                markdown: String::new(),
                assets: vec![Asset {
                    id: AssetId("remote".into()),
                    filename: None,
                    media_type: "image/png".into(),
                    bytes: vec![1],
                    external_uri: Some(
                        "https://user:secret@example.com/image.png?token=secret#fragment".into(),
                    ),
                }],
                diagnostics: vec![],
                provenance: vec![],
                detected_format: None,
                processing_duration_ms: None,
                memory_lease: crate::spi::OutputMemoryLease::default(),
            },
            DtoJsonStyle::Compact,
        )
        .unwrap_err();
        assert_eq!(error.code, DtoErrorCode::InvalidField);

        let mut value = result_value();
        value["assets"][0]["dataBase64"] = "".into();
        value["assets"][0]["externalUri"] = "file:///etc/passwd".into();
        assert_eq!(
            ResultDto::from_json(&value.to_string()).unwrap_err().code,
            DtoErrorCode::InvalidField
        );
    }

    #[test]
    fn canonical_external_asset_uri_handles_ports_ipv6_idn_and_percent_encoding() {
        for accepted in [
            "https://example.com:8443/a%20b.png",
            "http://[2001:db8::1]:8080/image.png",
            "https://xn--fsqu00a.xn--0zwm56d/image.png",
        ] {
            assert_eq!(canonical_external_asset_uri(accepted).as_deref(), Some(accepted));
        }
        for rejected in [
            "https://例子.测试/image.png",
            "HTTPS://EXAMPLE.COM/image.png",
            "https://example.com:443/image.png",
            "http://example.com:80/image.png",
            "https://example.com/a/../image.png",
            "https://user@example.com/image.png",
            "https://example.com/image.png?token=x",
            "https://example.com/image.png#fragment",
            "file:///image.png",
            "https://example.com/%zz.png",
        ] {
            assert_ne!(canonical_external_asset_uri(rejected).as_deref(), Some(rejected));
        }
    }

    #[test]
    fn custom_resource_and_depth_budgets_are_enforced() {
        let json = result_dto().to_json().unwrap();
        let limits = DtoLimits { max_base64_bytes: 2, ..DtoLimits::default() };
        assert_eq!(
            ResultDto::from_json_with_limits(&json, &limits).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );

        let mut value = result_value();
        value
            .as_object_mut()
            .unwrap()
            .insert("future".into(), serde_json::json!({"one": {"two": {"three": true}}}));
        let limits = DtoLimits { max_depth: 3, ..DtoLimits::default() };
        assert_eq!(
            ResultDto::from_json_with_limits(&value.to_string(), &limits).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );

        let limits = DtoLimits { max_string_bytes: 8, ..DtoLimits::default() };
        assert_eq!(
            ResultDto::from_json_with_limits(&json, &limits).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );

        let limits = DtoLimits { max_values: 3, ..DtoLimits::default() };
        assert_eq!(
            ResultDto::from_json_with_limits(&json, &limits).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );
    }

    #[test]
    fn raw_preflight_rejects_excessive_depth_before_json_allocation() {
        let open = "[".repeat(80);
        let close = "]".repeat(80);
        let nested = format!("{{\"schemaVersion\":1,\"future\":{open}null{close} }}");
        assert_eq!(
            DiagnosticsDto::from_json(&nested).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );
    }

    #[test]
    fn raw_preflight_handles_width_escapes_utf8_and_malformed_json() {
        let wide = format!(
            "{{\"schemaVersion\":1,\"future\":[{}]}}",
            std::iter::repeat_n("0", 20).collect::<Vec<_>>().join(",")
        );
        let limits = DtoLimits { max_values: 10, ..DtoLimits::default() };
        assert_eq!(
            DiagnosticsDto::from_json_with_limits(&wide, &limits).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );

        let escaped = r#"{"schemaVersion":1,"diagnostics":[],"future":"中文\\\"quoted\\\\slash"}"#;
        assert!(DiagnosticsDto::from_json(escaped).is_ok());

        let malformed = r#"{"schemaVersion":1,"diagnostics":[],"future":"unterminated}"#;
        assert_eq!(
            DiagnosticsDto::from_json(malformed).unwrap_err().code,
            DtoErrorCode::InvalidJson
        );
    }

    #[test]
    fn result_document_uses_ir_wire_preflight() {
        let mut value = result_value();
        let cells = (0..=crate::MAX_TABLE_COLUMNS)
            .map(|_| serde_json::json!({"rowSpan": 1, "columnSpan": 1, "header": false, "blocks": []}))
            .collect::<Vec<_>>();
        value["document"]["blocks"] = serde_json::json!([{
            "id": "table",
            "block": {"type": "table", "data": {"rows": [{"cells": cells}]}},
            "provenance": {
                "kind": "nativeParser",
                "provider": "test",
                "locator": {"page": null, "slide": null, "sheet": null, "cell": null,
                    "bounds": null, "time": null, "part": null},
                "confidence": 1.0
            }
        }]);
        assert_eq!(
            ResultDto::from_json(&value.to_string()).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );
    }

    #[test]
    fn result_document_preserves_additive_table_alignment() {
        let mut value = result_value();
        value["document"]["blocks"] = serde_json::json!([{
            "id": "table",
            "block": {"type": "table", "data": {
                "rows": [{"cells": [{"rowSpan": 1, "columnSpan": 2,
                    "header": true, "blocks": []}]}],
                "alignments": ["left", "right"]
            }},
            "provenance": {
                "kind": "nativeParser", "provider": "test",
                "locator": {"page": null, "slide": null, "sheet": null, "cell": null,
                    "bounds": null, "time": null, "part": null},
                "confidence": 1.0
            }
        }]);
        let dto = ResultDto::from_json(&value.to_string()).unwrap();
        let encoded = dto.to_json().unwrap();
        assert!(encoded.contains("\"alignments\":[\"left\",\"right\"]"));
        assert_eq!(ResultDto::from_json(&encoded).unwrap(), dto);

        value["document"]["blocks"][0]["block"]["data"]["futureAlignmentPolicy"] =
            serde_json::json!({"mode": "future"});
        assert!(ResultDto::from_json(&value.to_string()).is_ok());
    }

    #[test]
    fn unsafe_bundle_paths_and_duplicate_paths_are_rejected() {
        let manifest = BundleManifestDto {
            schema_version: DTO_SCHEMA_VERSION,
            markdown: "document.md".into(),
            document_ir: "document.ir.json".into(),
            diagnostics: "diagnostics.json".into(),
            diagnostics_schema_version: DTO_SCHEMA_VERSION,
            provenance: "provenance.json".into(),
            provenance_schema_version: DTO_SCHEMA_VERSION,
            assets: vec![BundleAssetDto {
                id: "a".into(),
                source_asset_ids: vec!["a".into()],
                path: "../escape".into(),
                media_type: "text/plain".into(),
                size: 0,
            }],
        };
        assert_eq!(manifest.to_json().unwrap_err().code, DtoErrorCode::InvalidField);

        let collision = BundleManifestDto {
            schema_version: DTO_SCHEMA_VERSION,
            markdown: "document.md".into(),
            document_ir: "document.ir.json".into(),
            diagnostics: "diagnostics.json".into(),
            diagnostics_schema_version: DTO_SCHEMA_VERSION,
            provenance: "provenance.json".into(),
            provenance_schema_version: DTO_SCHEMA_VERSION,
            assets: vec![BundleAssetDto {
                id: "a".into(),
                source_asset_ids: vec!["a".into()],
                path: "document.md".into(),
                media_type: "text/plain".into(),
                size: 0,
            }],
        };
        assert_eq!(collision.to_json().unwrap_err().code, DtoErrorCode::InvalidField);

        for unsafe_path in [
            "assets/CON.txt",
            "assets/PRN",
            "assets/COM1.png",
            "assets/LPT9",
            "assets/name.",
            "assets/name:stream",
            "assets/图片.png",
        ] {
            let mut manifest = collision.clone();
            manifest.assets[0].path = unsafe_path.into();
            assert_eq!(
                manifest.to_json().unwrap_err().code,
                DtoErrorCode::InvalidField,
                "accepted unsafe path {unsafe_path}"
            );
        }

        let mut overlong = collision.clone();
        overlong.assets[0].path = format!("assets/{}", "a".repeat(241));
        assert_eq!(overlong.to_json().unwrap_err().code, DtoErrorCode::InvalidField);

        let mut case_collision = collision.clone();
        case_collision.assets = vec![
            BundleAssetDto {
                id: "a".into(),
                source_asset_ids: vec!["a".into()],
                path: "assets/Image.png".into(),
                media_type: "image/png".into(),
                size: 1,
            },
            BundleAssetDto {
                id: "b".into(),
                source_asset_ids: vec!["b".into()],
                path: "assets/image.png".into(),
                media_type: "image/png".into(),
                size: 1,
            },
        ];
        assert_eq!(case_collision.to_json().unwrap_err().code, DtoErrorCode::InvalidField);

        let mut wrong_fixed_path = collision;
        wrong_fixed_path.assets.clear();
        wrong_fixed_path.markdown = "other.md".into();
        assert_eq!(wrong_fixed_path.to_json().unwrap_err().code, DtoErrorCode::InvalidField);
    }

    #[test]
    fn bundle_manifest_one_migrates_aliases_and_two_enforces_canonical_order() {
        let legacy = r#"{"schemaVersion":1,"markdown":"document.md","documentIr":"document.ir.json","diagnostics":"diagnostics.json","diagnosticsSchemaVersion":1,"provenance":"provenance.json","provenanceSchemaVersion":1,"assets":[{"id":"image","path":"assets/image.png","mediaType":"image/png","size":3}]}"#;
        let decoded = BundleManifestDto::from_json(legacy).unwrap();
        assert_eq!(decoded.schema_version, 1);
        assert_eq!(decoded.assets[0].source_asset_ids, ["image"]);
        assert_eq!(BundleManifestDto::from_json(&decoded.to_json().unwrap()).unwrap(), decoded);

        let mut current = decoded;
        current.schema_version = BUNDLE_SCHEMA_VERSION;
        current.assets[0].id = "a".into();
        current.assets[0].source_asset_ids = vec!["a".into(), "z".into()];
        assert!(current.to_json().is_ok());
        current.assets[0].source_asset_ids.reverse();
        assert_eq!(current.to_json().unwrap_err().code, DtoErrorCode::InvalidField);
    }

    #[test]
    fn non_finite_values_and_inconsistent_batch_items_are_rejected() {
        let mut dto = result_dto();
        dto.provenance[0].confidence = Some(f32::NAN);
        dto.provenance[0].locator.bounds = Some(Rect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 });
        assert_eq!(dto.to_json().unwrap_err().code, DtoErrorCode::InvalidField);

        let error = BatchReportDto::try_new(vec![BatchItemDto {
            input: "input.txt".into(),
            output: None,
            format: Some("text".into()),
            status: BatchItemStatus::Failed,
            outcome: BatchItemOutcome::Failed,
            diagnostics: vec![],
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: Some("failed".into()),
            warnings: vec![],
            duration_ms: None,
            processing_duration_ms: None,
        }])
        .unwrap_err();
        assert_eq!(error.code, DtoErrorCode::InvalidField);

        let invalid = ProvenanceDto {
            kind: ProvenanceKindDto::NativeParser,
            provider: "test".into(),
            locator: SourceLocator { page: Some(0), ..SourceLocator::default() },
            confidence: Some(1.0),
        };
        assert_eq!(Provenance::try_from(invalid).unwrap_err().code, DtoErrorCode::InvalidField);
    }

    #[test]
    fn locator_parts_use_container_names_not_bundle_output_names() {
        let diagnostic = DiagnosticDto {
            code: "presentation.dangerousPartsIgnored".into(),
            severity: DiagnosticSeverityDto::Warning,
            message: "isolated".into(),
            locator: Some(SourceLocator {
                part: Some("[Content_Types].xml".into()),
                ..SourceLocator::default()
            }),
        };
        let report = BatchReportDto::try_new(vec![BatchItemDto {
            input: "macro.pptm".into(),
            output: None,
            format: Some("pptx".into()),
            status: BatchItemStatus::Success,
            outcome: BatchItemOutcome::Degraded,
            diagnostics: vec![diagnostic],
            error_code: None,
            reason_code: None,
            component: None,
            part: None,
            limit: None,
            message: None,
            warnings: vec![],
            duration_ms: None,
            processing_duration_ms: None,
        }])
        .unwrap();
        let json = report.to_json().unwrap();
        let decoded = BatchReportDto::from_json(&json).unwrap();
        assert_eq!(decoded.items[0].diagnostics[0].locator, report.items[0].diagnostics[0].locator);

        for unsafe_part in ["../slide.xml", "/slide.xml", "C:/slide.xml", "a\\slide.xml"] {
            let mut invalid = report.clone();
            invalid.items[0].diagnostics[0].locator.as_mut().unwrap().part =
                Some(unsafe_part.into());
            assert_eq!(invalid.to_json().unwrap_err().code, DtoErrorCode::InvalidField);
        }
    }

    #[test]
    fn unknown_enum_values_are_rejected_within_a_schema_version() {
        let json = r#"{"schemaVersion":1,"succeeded":1,"failed":0,"items":[{"input":"a","output":null,"format":null,"status":"futureStatus","diagnostics":[],"errorCode":null,"message":null,"warnings":[]}]}"#;
        assert_eq!(BatchReportDto::from_json(json).unwrap_err().code, DtoErrorCode::InvalidJson);
    }
}
