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
    max_retained_bytes: u64,
    max_regions: u32,
    max_text_bytes: u64,
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
        Ok(Self { max_retained_bytes, max_regions, max_text_bytes })
    }

    /// Maximum bytes retained by the bound result and emitted OCR IR.
    #[must_use]
    pub const fn max_retained_bytes(self) -> u64 {
        self.max_retained_bytes
    }

    /// Maximum recognized regions returned by the provider.
    #[must_use]
    pub const fn max_regions(self) -> u32 {
        self.max_regions
    }

    /// Maximum total UTF-8 text bytes across all regions.
    #[must_use]
    pub const fn max_text_bytes(self) -> u64 {
        self.max_text_bytes
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
        Ok(Self { result, detection_confidences, evidence_chain })
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

    /// Consume the bound result after the caller has validated provider identity.
    #[must_use]
    pub fn into_parts(self) -> (OcrResult, Vec<f32>, Vec<OcrEvidenceStep>) {
        (self.result, self.detection_confidences, self.evidence_chain)
    }
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
