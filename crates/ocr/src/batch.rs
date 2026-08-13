use crate::{CropDescriptor, DetectionResult, RecognitionResult};
use into_markdown_core::{ConversionError, ExecutionContext};
use sha2::{Digest, Sha256};

pub(crate) const DETECTOR_MODEL_ID: &str = "pp-ocrv6-tiny-zh-en";
pub(crate) const RECOGNIZER_MODEL_ID: &str = "pp-ocrv6-tiny-recognizer-onnx";

/// Recognizer-produced output bound to one immutable detector batch.
#[derive(Debug, Clone)]
pub struct BoundRecognition {
    result: RecognitionResult,
    batch_identity: BatchIdentity,
    recognizer_model: &'static str,
    recognition_fingerprint: [u8; 32],
}

impl BoundRecognition {
    pub(crate) fn new(
        result: RecognitionResult,
        batch_identity: BatchIdentity,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let recognition_fingerprint = recognition_fingerprint(&result, context)?;
        Ok(Self {
            result,
            batch_identity,
            recognizer_model: RECOGNIZER_MODEL_ID,
            recognition_fingerprint,
        })
    }

    /// Read the immutable raw recognizer result carried by this binding.
    #[must_use]
    pub fn result(&self) -> &RecognitionResult {
        &self.result
    }

    pub(crate) const fn recognizer_model(&self) -> &'static str {
        self.recognizer_model
    }

    pub(crate) fn validate_identity(
        &self,
        detection: &BatchIdentity,
    ) -> Result<(), ConversionError> {
        if &self.batch_identity != detection || self.recognizer_model != RECOGNIZER_MODEL_ID {
            return Err(batch("batchIdentityMismatch"));
        }
        Ok(())
    }

    pub(crate) fn validate_payload(
        &self,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        if self.recognition_fingerprint != recognition_fingerprint(&self.result, context)? {
            return Err(batch("batchIdentityMismatch"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn tamper_result(&mut self, mutate: impl FnOnce(&mut RecognitionResult)) {
        mutate(&mut self.result);
    }

    #[cfg(test)]
    pub(crate) fn tamper_model(&mut self, model: &'static str) {
        self.recognizer_model = model;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchIdentity {
    pub(crate) page: u32,
    pub(crate) page_width_bits: u32,
    pub(crate) page_height_bits: u32,
    pub(crate) detector_model: &'static str,
    pub(crate) region_fingerprint: [u8; 32],
}

impl BatchIdentity {
    pub(crate) fn new(
        page: u32,
        page_width: f32,
        page_height: f32,
        detector_model: &'static str,
        detection: &DetectionResult,
    ) -> Result<Self, ConversionError> {
        if page == 0
            || !page_width.is_finite()
            || !page_height.is_finite()
            || page_width <= 0.0
            || page_height <= 0.0
            || detector_model.trim().is_empty()
        {
            return Err(batch("invalidBatchIdentity"));
        }
        let region_fingerprint = fingerprint(detection)?;
        Ok(Self {
            page,
            page_width_bits: page_width.to_bits(),
            page_height_bits: page_height.to_bits(),
            detector_model,
            region_fingerprint,
        })
    }

    pub(crate) fn validate(&self, detection: &DetectionResult) -> Result<(), ConversionError> {
        if self.page == 0
            || self.detector_model.trim().is_empty()
            || f32::from_bits(self.page_width_bits) <= 0.0
            || f32::from_bits(self.page_height_bits) <= 0.0
            || self.region_fingerprint != fingerprint(detection)?
        {
            return Err(batch("batchIdentityMismatch"));
        }
        Ok(())
    }
}

pub(crate) fn fingerprint(detection: &DetectionResult) -> Result<[u8; 32], ConversionError> {
    let mut digest = Sha256::new();
    digest.update(b"into-markdown/ocr-regions/v1\0");
    digest.update(
        u64::try_from(detection.provider.len())
            .map_err(|_| batch("providerLengthOverflow"))?
            .to_le_bytes(),
    );
    digest.update(detection.provider.as_bytes());
    digest.update(
        u64::try_from(detection.regions.len())
            .map_err(|_| batch("regionCountOverflow"))?
            .to_le_bytes(),
    );
    for (source_index, region) in detection.regions.iter().enumerate() {
        digest.update(
            u64::try_from(source_index).map_err(|_| batch("sourceIndexOverflow"))?.to_le_bytes(),
        );
        digest.update(region.angle_degrees.to_bits().to_le_bytes());
        digest.update(region.confidence.to_bits().to_le_bytes());
        for value in region.polygon.iter().flat_map(|(x, y)| [x.to_bits(), y.to_bits()]) {
            digest.update(value.to_le_bytes());
        }
        fingerprint_crop(&mut digest, &region.crop);
    }
    Ok(digest.finalize().into())
}

fn fingerprint_crop(digest: &mut Sha256, crop: &CropDescriptor) {
    for value in crop.polygon.iter().flat_map(|(x, y)| [x.to_bits(), y.to_bits()]) {
        digest.update(value.to_le_bytes());
    }
    digest.update(crop.width.to_le_bytes());
    digest.update(crop.height.to_le_bytes());
}

fn recognition_fingerprint(
    result: &RecognitionResult,
    context: &ExecutionContext,
) -> Result<[u8; 32], ConversionError> {
    let mut digest = Sha256::new();
    digest.update(b"into-markdown/ocr-recognition/v1\0");
    hash_bytes(&mut digest, result.provider.as_bytes(), context)?;
    match &result.language_hint {
        Some(value) => {
            digest.update([1]);
            hash_bytes(&mut digest, value.as_bytes(), context)?;
        }
        None => digest.update([0]),
    }
    digest.update(
        u64::try_from(result.regions.len())
            .map_err(|_| batch("recognitionCountOverflow"))?
            .to_le_bytes(),
    );
    for region in result.regions.iter() {
        digest.update(
            u64::try_from(region.source_index)
                .map_err(|_| batch("recognitionIndexOverflow"))?
                .to_le_bytes(),
        );
        digest.update(region.confidence.to_bits().to_le_bytes());
        hash_bytes(&mut digest, region.text.as_bytes(), context)?;
    }
    Ok(digest.finalize().into())
}

fn hash_bytes(
    digest: &mut Sha256,
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    digest.update(
        u64::try_from(bytes.len()).map_err(|_| batch("recognitionBytesOverflow"))?.to_le_bytes(),
    );
    for chunk in bytes.chunks(4 * 1024) {
        digest.update(chunk);
        if chunk.len() == 4 * 1024 {
            context.checkpoint()?;
        }
    }
    Ok(())
}

fn batch(detail: &str) -> ConversionError {
    ConversionError::Ocr { provider: "builtin.ocr.batch".into(), detail: detail.into() }
}
