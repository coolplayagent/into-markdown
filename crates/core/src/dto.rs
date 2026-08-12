//! Stable application wire contracts shared by CLI, HTTP, SSE consumers, and bundles.

use crate::{
    Asset, Diagnostic, DiagnosticSeverity, Document, Provenance, ProvenanceKind, SourceLocator,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeSet;
use thiserror::Error;

/// Schema version emitted and accepted by application DTOs.
pub const DTO_SCHEMA_VERSION: u32 = 1;
/// Maximum JSON bytes accepted by the default DTO decoder.
pub const MAX_DTO_JSON_BYTES: usize = 64 * 1024 * 1024;
/// Maximum JSON nesting accepted by the default DTO decoder.
pub const MAX_DTO_DEPTH: usize = 64;
/// Maximum assets accepted in one result or bundle manifest.
pub const MAX_DTO_ASSETS: usize = 100_000;
/// Maximum decoded bytes carried by all base64 assets in one result.
pub const MAX_DTO_BASE64_BYTES: usize = 256 * 1024 * 1024;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverityDto {
    /// Informational recovery note.
    Info,
    /// Content was skipped or recovered imperfectly.
    Warning,
    /// A scoped operation failed but conversion continued.
    Error,
}

/// One stable non-fatal diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Ordered diagnostic records.
    pub diagnostics: Vec<DiagnosticDto>,
}

/// Stable provenance origin used by external protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceListDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Ordered provenance records.
    pub provenance: Vec<ProvenanceDto>,
}

/// Stable asset representation with standard padded base64 content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleAssetDto {
    /// Stable asset identifier.
    pub id: String,
    /// Safe bundle-relative path.
    pub path: String,
    /// MIME media type.
    pub media_type: String,
    /// Uncompressed byte size.
    pub size: u64,
}

/// Stable portable bundle manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleManifestDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Bundle-relative Markdown path.
    pub markdown: String,
    /// Bundle-relative document IR path.
    pub document_ir: String,
    /// Bundle-relative diagnostics path.
    pub diagnostics: String,
    /// Bundle-relative provenance path.
    pub provenance: String,
    /// Ordered asset entries.
    pub assets: Vec<BundleAssetDto>,
}

/// Stable completion state for one batch item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BatchItemStatus {
    /// Conversion completed successfully.
    Success,
    /// Conversion failed.
    Failed,
}

/// One item in a machine-readable batch report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItemDto {
    /// Display-safe input identifier.
    pub input: String,
    /// Output path, when one was allocated.
    pub output: Option<String>,
    /// Stable detected or explicit format identifier.
    pub format: Option<String>,
    /// Completion state.
    pub status: BatchItemStatus,
    /// Ordered diagnostics.
    pub diagnostics: Vec<DiagnosticDto>,
    /// Stable failure code.
    pub error_code: Option<String>,
    /// Human-readable failure detail.
    pub message: Option<String>,
    /// Human-readable warnings produced by output handling.
    pub warnings: Vec<String>,
}

/// Versioned machine-readable batch report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchReportDto {
    /// Protocol version.
    pub schema_version: u32,
    /// Number of successful items.
    pub succeeded: u64,
    /// Number of failed items.
    pub failed: u64,
    /// Input-order report items.
    pub items: Vec<BatchItemDto>,
}

impl BatchReportDto {
    /// Build a report with derived, checked totals.
    #[must_use]
    pub fn new(items: Vec<BatchItemDto>) -> Self {
        let succeeded = items.iter().filter(|item| item.status == BatchItemStatus::Success).count();
        let failed = items.len().saturating_sub(succeeded);
        Self {
            schema_version: DTO_SCHEMA_VERSION,
            succeeded: u64::try_from(succeeded).unwrap_or(u64::MAX),
            failed: u64::try_from(failed).unwrap_or(u64::MAX),
            items,
        }
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
        if let Some(uri) = &value.external_uri {
            validate_external_uri(uri, "$.externalUri")?;
        }
        let dto = Self {
            id: value.id.0.clone(),
            filename: value.filename.clone(),
            media_type: value.media_type.clone(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&value.bytes),
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

impl TryFrom<&crate::ConversionResult> for ResultDto {
    type Error = DtoError;

    fn try_from(value: &crate::ConversionResult) -> Result<Self, Self::Error> {
        let dto = Self {
            schema_version: DTO_SCHEMA_VERSION,
            markdown: value.markdown.clone(),
            document: value.document.clone(),
            assets: value.assets.iter().map(AssetDto::try_from).collect::<Result<_, _>>()?,
            diagnostics: value.diagnostics.iter().map(DiagnosticDto::from).collect(),
            provenance: value.provenance.iter().map(ProvenanceDto::from).collect(),
        };
        dto.validate(&DtoLimits::default())?;
        Ok(dto)
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
        })
    }
}

macro_rules! json_api {
    ($type:ty, $validate:ident, $preflight:ident) => {
        impl $type {
            /// Serialize this DTO after validating protocol invariants.
            ///
            /// # Errors
            ///
            /// Returns a stable [`DtoErrorCode`] for an invalid DTO or serialization failure.
            pub fn to_json(&self) -> Result<String, DtoError> {
                self.$validate(&DtoLimits::default())?;
                serde_json::to_string(self).map_err(|error| {
                    DtoError::new(DtoErrorCode::InvalidJson, "$", format!("serialize DTO: {error}"))
                })
            }

            /// Serialize this DTO as indented JSON after validating protocol invariants.
            ///
            /// # Errors
            ///
            /// Returns a stable [`DtoErrorCode`] for an invalid DTO or serialization failure.
            pub fn to_pretty_json(&self) -> Result<String, DtoError> {
                self.$validate(&DtoLimits::default())?;
                serde_json::to_string_pretty(self).map_err(|error| {
                    DtoError::new(DtoErrorCode::InvalidJson, "$", format!("serialize DTO: {error}"))
                })
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
                require_version(&value)?;
                $preflight(&mut value, limits)?;
                let decoded: Self = decode_typed(value)?;
                decoded.$validate(limits)?;
                Ok(decoded)
            }
        }
    };
}

json_api!(ResultDto, validate, preflight_result_document);
json_api!(DiagnosticsDto, validate, no_preflight);
json_api!(ProvenanceListDto, validate, no_preflight);
json_api!(BundleManifestDto, validate, no_preflight);
json_api!(BatchReportDto, validate, no_preflight);

impl ResultDto {
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

    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        validate_version(self.schema_version)?;
        validate_diagnostics(&self.diagnostics, limits, "$.diagnostics")
    }
}

impl ProvenanceListDto {
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

    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        validate_version(self.schema_version)?;
        validate_provenance(&self.provenance, limits, "$.provenance")
    }
}

impl BundleManifestDto {
    fn validate(&self, limits: &DtoLimits) -> Result<(), DtoError> {
        validate_version(self.schema_version)?;
        let fixed_entries = [
            ("$.markdown", &self.markdown),
            ("$.documentIr", &self.document_ir),
            ("$.diagnostics", &self.diagnostics),
            ("$.provenance", &self.provenance),
        ];
        let mut paths = BTreeSet::from(["manifest.json"]);
        for (path, value) in fixed_entries {
            validate_bundle_path(value, path)?;
            if !paths.insert(value.as_str()) {
                return Err(DtoError::new(
                    DtoErrorCode::InvalidField,
                    path,
                    "duplicate or reserved bundle path",
                ));
            }
        }
        if self.assets.len() > limits.max_assets {
            return limit("$.assets", "assets", limits.max_assets);
        }
        let mut ids = BTreeSet::new();
        for (index, asset) in self.assets.iter().enumerate() {
            let path = format!("$.assets[{index}]");
            validate_id(&asset.id, &format!("{path}.id"))?;
            validate_bundle_path(&asset.path, &format!("{path}.path"))?;
            if !ids.insert(&asset.id) {
                return Err(DtoError::new(
                    DtoErrorCode::DuplicateId,
                    format!("{path}.id"),
                    "duplicate asset ID",
                ));
            }
            if !paths.insert(asset.path.as_str()) {
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
        if self.items.len() > limits.max_batch_items {
            return limit("$.items", "batchItems", limits.max_batch_items);
        }
        let succeeded =
            self.items.iter().filter(|item| item.status == BatchItemStatus::Success).count();
        let failed = self.items.len().saturating_sub(succeeded);
        if self.succeeded != u64::try_from(succeeded).unwrap_or(u64::MAX)
            || self.failed != u64::try_from(failed).unwrap_or(u64::MAX)
        {
            return Err(DtoError::new(
                DtoErrorCode::InvalidField,
                "$",
                "batch totals do not match items",
            ));
        }
        for (index, item) in self.items.iter().enumerate() {
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
    if json.len() > limits.max_json_bytes {
        return limit("$", "dtoJsonBytes", limits.max_json_bytes);
    }
    preflight_json_text(json, limits)?;
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("decode DTO: {error}"))
    })?;
    Ok(value)
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

fn decode_typed<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, DtoError> {
    serde_json::from_value(value).map_err(|error| {
        DtoError::new(DtoErrorCode::InvalidJson, "$", format!("decode DTO fields: {error}"))
    })
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

fn validate_locator(locator: &SourceLocator, path: &str) -> Result<(), DtoError> {
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
    if let Some(part) = &locator.part {
        validate_bundle_path(part, &format!("{path}.part"))?;
    }
    Ok(())
}

fn sanitize_external_uri(value: &str) -> Option<String> {
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
    let sanitized = sanitize_external_uri(value).ok_or_else(|| {
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
    if value.is_empty()
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.contains('\\')
        || value.contains('\0')
        || value.split('/').any(|part| part.is_empty() || matches!(part, "." | ".."))
        || value.as_bytes().get(1) == Some(&b':')
    {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            path,
            "expected a normalized safe bundle-relative path",
        ));
    }
    Ok(())
}

fn limit<T>(path: &str, name: &str, maximum: usize) -> Result<T, DtoError> {
    Err(DtoError::new(DtoErrorCode::ResourceLimit, path, format!("{name} exceeds limit {maximum}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssetId, ConversionResult, Rect};

    fn result_dto() -> ResultDto {
        ResultDto::try_from(&ConversionResult {
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
        })
        .unwrap()
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
        let internal = ConversionResult::try_from(dto).unwrap();
        assert_eq!(internal.assets[0].bytes, [1, 2, 3]);
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
            provenance: "provenance.json".into(),
            assets: vec![],
        };
        assert_eq!(
            manifest.to_json().unwrap(),
            r#"{"schemaVersion":1,"markdown":"document.md","documentIr":"document.ir.json","diagnostics":"diagnostics.json","provenance":"provenance.json","assets":[]}"#
        );
        let report = BatchReportDto::new(vec![BatchItemDto {
            input: "report.pdf".into(),
            output: Some("report.md".into()),
            format: Some("pdf".into()),
            status: BatchItemStatus::Success,
            diagnostics: vec![],
            error_code: None,
            message: None,
            warnings: vec![],
        }]);
        assert_eq!(
            report.to_json().unwrap(),
            r#"{"schemaVersion":1,"succeeded":1,"failed":0,"items":[{"input":"report.pdf","output":"report.md","format":"pdf","status":"success","diagnostics":[],"errorCode":null,"message":null,"warnings":[]}]}"#
        );
    }

    #[test]
    fn additive_unknown_fields_are_ignored() {
        let mut value = serde_json::to_value(result_dto()).unwrap();
        value.as_object_mut().unwrap().insert("futureField".into(), true.into());
        value["assets"][0].as_object_mut().unwrap().insert("futureAssetField".into(), true.into());
        let decoded = ResultDto::from_json(&value.to_string()).unwrap();
        assert_eq!(decoded.assets[0].id, "image-1");
    }

    #[test]
    fn unknown_version_and_missing_required_field_are_stable_errors() {
        let mut value = serde_json::to_value(result_dto()).unwrap();
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
        let mut value = serde_json::to_value(result_dto()).unwrap();
        value["assets"][0]["dataBase64"] = "not base64".into();
        assert_eq!(
            ResultDto::from_json(&value.to_string()).unwrap_err().code,
            DtoErrorCode::InvalidBase64
        );

        let mut dto = result_dto();
        dto.assets.push(dto.assets[0].clone());
        assert_eq!(dto.to_json().unwrap_err().code, DtoErrorCode::DuplicateId);
    }

    #[test]
    fn external_asset_uris_are_audit_only_and_cannot_leak_secrets() {
        let error = ResultDto::try_from(&ConversionResult {
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
        })
        .unwrap_err();
        assert_eq!(error.code, DtoErrorCode::InvalidField);

        let mut value = serde_json::to_value(result_dto()).unwrap();
        value["assets"][0]["dataBase64"] = "".into();
        value["assets"][0]["externalUri"] = "file:///etc/passwd".into();
        assert_eq!(
            ResultDto::from_json(&value.to_string()).unwrap_err().code,
            DtoErrorCode::InvalidField
        );
    }

    #[test]
    fn custom_resource_and_depth_budgets_are_enforced() {
        let json = result_dto().to_json().unwrap();
        let limits = DtoLimits { max_base64_bytes: 2, ..DtoLimits::default() };
        assert_eq!(
            ResultDto::from_json_with_limits(&json, &limits).unwrap_err().code,
            DtoErrorCode::ResourceLimit
        );

        let mut value = serde_json::to_value(result_dto()).unwrap();
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
        let mut value = serde_json::to_value(result_dto()).unwrap();
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
    fn unsafe_bundle_paths_and_duplicate_paths_are_rejected() {
        let manifest = BundleManifestDto {
            schema_version: DTO_SCHEMA_VERSION,
            markdown: "document.md".into(),
            document_ir: "document.ir.json".into(),
            diagnostics: "diagnostics.json".into(),
            provenance: "provenance.json".into(),
            assets: vec![BundleAssetDto {
                id: "a".into(),
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
            provenance: "provenance.json".into(),
            assets: vec![BundleAssetDto {
                id: "a".into(),
                path: "document.md".into(),
                media_type: "text/plain".into(),
                size: 0,
            }],
        };
        assert_eq!(collision.to_json().unwrap_err().code, DtoErrorCode::InvalidField);
    }

    #[test]
    fn non_finite_values_and_inconsistent_batch_items_are_rejected() {
        let mut dto = result_dto();
        dto.provenance[0].confidence = Some(f32::NAN);
        dto.provenance[0].locator.bounds = Some(Rect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 });
        assert_eq!(dto.to_json().unwrap_err().code, DtoErrorCode::InvalidField);

        let report = BatchReportDto::new(vec![BatchItemDto {
            input: "input.txt".into(),
            output: None,
            format: Some("text".into()),
            status: BatchItemStatus::Failed,
            diagnostics: vec![],
            error_code: None,
            message: Some("failed".into()),
            warnings: vec![],
        }]);
        assert_eq!(report.to_json().unwrap_err().code, DtoErrorCode::InvalidField);

        let invalid = ProvenanceDto {
            kind: ProvenanceKindDto::NativeParser,
            provider: "test".into(),
            locator: SourceLocator { page: Some(0), ..SourceLocator::default() },
            confidence: Some(1.0),
        };
        assert_eq!(Provenance::try_from(invalid).unwrap_err().code, DtoErrorCode::InvalidField);
    }

    #[test]
    fn unknown_enum_values_are_rejected_within_a_schema_version() {
        let json = r#"{"schemaVersion":1,"succeeded":1,"failed":0,"items":[{"input":"a","output":null,"format":null,"status":"futureStatus","diagnostics":[],"errorCode":null,"message":null,"warnings":[]}]}"#;
        assert_eq!(BatchReportDto::from_json(json).unwrap_err().code, DtoErrorCode::InvalidJson);
    }
}
