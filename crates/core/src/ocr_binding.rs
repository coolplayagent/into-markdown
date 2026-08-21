//! Additive OCR identity binding without changing legacy result literals.

use crate::{ConversionError, OcrEvidenceStage, OcrEvidenceStep};
use serde::{Deserialize, Serialize};

/// Conservative retained-output and cardinality plan for bound OCR execution.
///
/// Fields remain private so providers cannot bypass validation with a struct
/// literal. The plan covers the provider result and the structured OCR IR that
/// consumes it; callers reserve it before invoking the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrOutputPlan {
    retained_budget: u64,
    working_budget: u64,
    region_limit: u32,
    text_bytes_cap: u64,
}

impl OcrOutputPlan {
    /// Construct a non-zero, internally consistent OCR output plan.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::ResourceLimit`] for a zero bound or when the
    /// cardinality bounds cannot fit inside the retained-byte bound.
    pub fn try_new(
        max_retained_bytes: u64,
        max_regions: u32,
        max_text_bytes: u64,
    ) -> Result<Self, ConversionError> {
        let structural = u64::from(max_regions)
            .checked_mul(256)
            .and_then(|bytes| bytes.checked_add(max_text_bytes))
            .ok_or_else(|| plan_error("OCR output plan overflow"))?;
        if max_retained_bytes == 0
            || max_regions == 0
            || max_text_bytes == 0
            || structural > max_retained_bytes
        {
            return Err(plan_error("OCR output plan is zero or smaller than its declared payload"));
        }
        Ok(Self {
            retained_budget: max_retained_bytes,
            working_budget: 0,
            region_limit: max_regions,
            text_bytes_cap: max_text_bytes,
        })
    }

    /// Construct a plan that also declares the provider's peak transient working set.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::ResourceLimit`] when the retained-output
    /// bounds are zero, overflow, or cannot contain the declared payload.
    pub fn try_new_with_working(
        max_retained_bytes: u64,
        max_working_bytes: u64,
        max_regions: u32,
        max_text_bytes: u64,
    ) -> Result<Self, ConversionError> {
        let mut plan = Self::try_new(max_retained_bytes, max_regions, max_text_bytes)?;
        plan.working_budget = max_working_bytes;
        Ok(plan)
    }

    /// Maximum bytes retained by the bound result and emitted OCR IR.
    #[must_use]
    pub const fn max_retained_bytes(self) -> u64 {
        self.retained_budget
    }

    /// Maximum transient bytes needed while the provider produces the result.
    #[must_use]
    pub const fn max_working_bytes(self) -> u64 {
        self.working_budget
    }

    /// Maximum recognized regions returned by the provider.
    #[must_use]
    pub const fn max_regions(self) -> u32 {
        self.region_limit
    }

    /// Maximum total UTF-8 text bytes across all regions.
    #[must_use]
    pub const fn max_text_bytes(self) -> u64 {
        self.text_bytes_cap
    }
}

fn plan_error(detail: &str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
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

/// OCR output with detector confidence and exact model identity bound to each result.
///
/// The payload is intentionally private. Providers construct this value through
/// [`BoundOcrResult::try_new`], which validates the additive evidence without
/// changing the source-compatible [`OcrRegion`] and [`OcrResult`] contracts.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundOcrResult {
    result: OcrResult,
    detection_confidences: Vec<f32>,
    evidence_chain: Vec<OcrEvidenceStep>,
    input_identity: Option<OcrInputIdentity>,
}

/// Serializable representation used at an isolated OCR provider boundary.
///
/// The host must convert this DTO through [`BoundOcrResult::try_from_dto`]
/// before exposing it as trusted OCR evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoundOcrResultDto {
    /// Legacy-compatible OCR result.
    pub result: OcrResult,
    /// Detector confidence in exact region order.
    pub detection_confidences: Vec<f32>,
    /// Exact detector and recognizer identities.
    pub evidence_chain: Vec<OcrEvidenceStep>,
    /// Optional normalized input identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_identity: Option<OcrInputIdentityDto>,
}

/// Serializable normalized-image identity for isolated OCR providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OcrInputIdentityDto {
    /// Lowercase SHA-256 of the exact normalized image bytes.
    pub sha256: String,
    /// Normalized image width.
    pub width: u32,
    /// Normalized image height.
    pub height: u32,
    /// Zero-based frame ordinal.
    pub frame: u32,
}

/// Cryptographic identity of the exact normalized image accepted by an OCR provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrInputIdentity {
    sha256: [u8; 32],
    width: u32,
    height: u32,
    frame: u32,
}

impl OcrInputIdentity {
    /// Construct an identity for one normalized image frame.
    ///
    /// # Errors
    ///
    /// Returns an OCR error when either image dimension is zero.
    pub fn try_new(
        sha256: [u8; 32],
        width: u32,
        height: u32,
        frame: u32,
    ) -> Result<Self, ConversionError> {
        if width == 0 || height == 0 {
            return Err(ConversionError::Ocr {
                provider: "ocr-binding".into(),
                detail: "OCR input identity dimensions must be non-zero".into(),
            });
        }
        Ok(Self { sha256, width, height, frame })
    }

    /// SHA-256 of the exact normalized encoded bytes supplied to OCR.
    #[must_use]
    pub const fn sha256(self) -> [u8; 32] {
        self.sha256
    }

    /// Normalized image width.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Normalized image height.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    /// Zero-based frame ordinal.
    #[must_use]
    pub const fn frame(self) -> u32 {
        self.frame
    }
}

impl BoundOcrResult {
    /// Bind exact detector confidence and detection/recognition identities.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::Ocr`] when evidence is incomplete,
    /// non-finite, out of range, or inconsistent with the recognized regions.
    pub fn try_new(
        result: OcrResult,
        detection_confidences: Vec<f32>,
        evidence_chain: Vec<OcrEvidenceStep>,
    ) -> Result<Self, ConversionError> {
        let provider = if result.provider.trim().is_empty() {
            "ocr-binding".to_owned()
        } else {
            result.provider.clone()
        };
        if detection_confidences.len() != result.regions.len() {
            return Err(ConversionError::Ocr {
                provider,
                detail: "detector confidence count does not match OCR region count".into(),
            });
        }
        if detection_confidences
            .iter()
            .any(|confidence| !confidence.is_finite() || !(0.0..=1.0).contains(confidence))
        {
            return Err(ConversionError::Ocr {
                provider,
                detail: "detector confidence must be finite and between zero and one".into(),
            });
        }
        let expected = [OcrEvidenceStage::Detection, OcrEvidenceStage::Recognition];
        if evidence_chain.len() != expected.len()
            || evidence_chain.iter().zip(expected).any(|(step, stage)| {
                step.stage != stage
                    || step.provider.trim().is_empty()
                    || step.model.as_ref().is_none_or(|model| model.trim().is_empty())
            })
        {
            return Err(ConversionError::Ocr {
                provider,
                detail: "bound OCR evidence must contain exact detection and recognition models"
                    .into(),
            });
        }
        if evidence_chain.get(1).is_some_and(|step| step.provider != result.provider) {
            return Err(ConversionError::Ocr {
                provider,
                detail: "OCR result provider does not match the bound recognition provider".into(),
            });
        }
        Ok(Self { result, detection_confidences, evidence_chain, input_identity: None })
    }

    /// Bind validated OCR evidence to the exact normalized input image.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::try_new`].
    pub fn try_new_for_input(
        result: OcrResult,
        detection_confidences: Vec<f32>,
        evidence_chain: Vec<OcrEvidenceStep>,
        input_identity: OcrInputIdentity,
    ) -> Result<Self, ConversionError> {
        let mut bound = Self::try_new(result, detection_confidences, evidence_chain)?;
        bound.input_identity = Some(input_identity);
        Ok(bound)
    }

    /// Read the legacy-compatible OCR payload.
    #[must_use]
    pub fn result(&self) -> &OcrResult {
        &self.result
    }

    /// Read detector confidences in exact region order.
    #[must_use]
    pub fn detection_confidences(&self) -> &[f32] {
        &self.detection_confidences
    }

    /// Read the validated detection/recognition evidence chain.
    #[must_use]
    pub fn evidence_chain(&self) -> &[OcrEvidenceStep] {
        &self.evidence_chain
    }

    /// Exact normalized input identity, when supplied by the provider.
    #[must_use]
    pub const fn input_identity(&self) -> Option<OcrInputIdentity> {
        self.input_identity
    }

    /// Consume the bound result after the caller has validated provider identity.
    #[must_use]
    pub fn into_parts(self) -> (OcrResult, Vec<f32>, Vec<OcrEvidenceStep>) {
        (self.result, self.detection_confidences, self.evidence_chain)
    }

    /// Convert validated evidence into its isolated-provider wire DTO.
    #[must_use]
    pub fn to_dto(&self) -> BoundOcrResultDto {
        BoundOcrResultDto {
            result: self.result.clone(),
            detection_confidences: self.detection_confidences.clone(),
            evidence_chain: self.evidence_chain.clone(),
            input_identity: self.input_identity.map(|identity| OcrInputIdentityDto {
                sha256: hex_sha256(identity.sha256),
                width: identity.width,
                height: identity.height,
                frame: identity.frame,
            }),
        }
    }

    /// Validate and bind an isolated-provider DTO.
    ///
    /// # Errors
    ///
    /// Returns an OCR error for malformed hashes, non-finite geometry,
    /// confidence outside zero-to-one, empty identities, or inconsistent
    /// detector and recognizer evidence.
    pub fn try_from_dto(dto: BoundOcrResultDto) -> Result<Self, ConversionError> {
        validate_wire_result(&dto.result)?;
        let identity = dto.input_identity.map(parse_input_identity).transpose()?;
        match identity {
            Some(identity) => Self::try_new_for_input(
                dto.result,
                dto.detection_confidences,
                dto.evidence_chain,
                identity,
            ),
            None => Self::try_new(dto.result, dto.detection_confidences, dto.evidence_chain),
        }
    }
}

fn validate_wire_result(result: &OcrResult) -> Result<(), ConversionError> {
    let provider = if result.provider.trim().is_empty() {
        "ocr-binding".to_owned()
    } else {
        result.provider.clone()
    };
    if result.provider.trim() != result.provider || result.provider.is_empty() {
        return Err(ConversionError::Ocr {
            provider,
            detail: "OCR provider identity is empty or not canonical".into(),
        });
    }
    for region in &result.regions {
        if region.text.len() > 16 * 1024 * 1024
            || !region.confidence.is_finite()
            || !(0.0..=1.0).contains(&region.confidence)
            || region
                .polygon
                .iter()
                .any(|(x, y)| !x.is_finite() || !y.is_finite() || *x < 0.0 || *y < 0.0)
        {
            return Err(ConversionError::Ocr {
                provider,
                detail: "OCR region text, geometry, or confidence is invalid".into(),
            });
        }
    }
    Ok(())
}

fn parse_input_identity(dto: OcrInputIdentityDto) -> Result<OcrInputIdentity, ConversionError> {
    if dto.sha256.len() != 64
        || !dto.sha256.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ConversionError::Ocr {
            provider: "ocr-binding".into(),
            detail: "OCR input identity SHA-256 is invalid".into(),
        });
    }
    let mut digest = [0_u8; 32];
    for (index, output) in digest.iter_mut().enumerate() {
        let offset = index * 2;
        *output = u8::from_str_radix(&dto.sha256[offset..offset + 2], 16).map_err(|_| {
            ConversionError::Ocr {
                provider: "ocr-binding".into(),
                detail: "OCR input identity SHA-256 is invalid".into(),
            }
        })?;
    }
    OcrInputIdentity::try_new(digest, dto.width, dto.height, dto.frame)
}

fn hex_sha256(digest: [u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Compatibility result for OCR engines adopting bound structured evidence.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum OcrRecognition {
    /// Exact detector and recognizer evidence is present and validated.
    Bound(BoundOcrResult),
    /// Legacy OCR output without evidence sufficient for `Inline::OcrText`.
    Unbound(OcrResult),
}
