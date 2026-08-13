//! Bounded PP-OCRv6 recognition preprocessing and CTC decoding.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use crate::{CropDescriptor, ModelManager, PixelView};
use into_markdown_core::{BoxFuture, ConversionError, ExecutionContext, TensorRuntime};
use std::sync::Arc;

mod authority;
mod ctc;
mod preprocess;
#[cfg(test)]
mod tests;

use authority::{load_characters, validate_authority};
use ctc::decode_output;
use preprocess::{prepare_batch, validate_pixels, validated_crop};

const PROVIDER: &str = "builtin.ocr.ppocrv6-recognizer";
const MODEL_ID: &str = "pp-ocrv6-tiny-recognizer-onnx";
const HEIGHT: usize = 48;
const BASE_WIDTH: usize = 320;
const MAX_WIDTH: usize = 3200;
const CLASSES: usize = 6906;
const BLANK: usize = 0;
const SCALE: f32 = 1.0 / 255.0;

/// Recognition resource and batching bounds.
#[derive(Debug, Clone)]
pub struct RecognitionConfig {
    pub max_regions: usize,
    pub max_batch_size: usize,
    pub max_crop_pixels: usize,
    pub max_tensor_elements: usize,
    pub max_output_timesteps: usize,
    pub max_decoded_bytes: usize,
}

impl Default for RecognitionConfig {
    fn default() -> Self {
        Self {
            max_regions: 3000,
            max_batch_size: 8,
            max_crop_pixels: 32_000_000,
            max_tensor_elements: 32_000_000,
            max_output_timesteps: 1024,
            max_decoded_bytes: 16 * 1024 * 1024,
        }
    }
}

/// One recognition result in the caller's source-region order.
#[derive(Debug, Clone, PartialEq)]
pub struct RecognizedText {
    pub source_index: usize,
    pub text: String,
    pub confidence: f32,
}

/// Structured recognizer-only output. Document IR integration belongs to the OCR pipeline owner.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecognitionResult {
    pub regions: Vec<RecognizedText>,
    pub provider: String,
    pub language_hint: Option<String>,
}

/// Offline recognizer over the audited tensor-runtime seam.
pub struct PpOcrTextRecognizer {
    runtime: Arc<dyn TensorRuntime>,
    characters: Arc<[String]>,
    config: RecognitionConfig,
}

impl PpOcrTextRecognizer {
    pub fn from_installed(
        runtime: Arc<dyn TensorRuntime>,
        manager: &ModelManager,
        config: RecognitionConfig,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let artifact = manager
            .verified_runtime_artifact(MODEL_ID, "character-table", context)
            .map_err(|_| ConversionError::ComponentUnavailable {
                component: "ocr-recognizer".into(),
                detail: "ModelUnavailable".into(),
            })?;
        Self::new(runtime, &artifact.bytes, config)
    }
    pub fn new(
        runtime: Arc<dyn TensorRuntime>,
        dictionary: &[u8],
        config: RecognitionConfig,
    ) -> Result<Self, ConversionError> {
        validate_authority()?;
        validate_config(&config)?;
        let characters = load_characters(dictionary)?;
        Ok(Self { runtime, characters: Arc::from(characters), config })
    }

    #[must_use]
    pub fn recognize<'a>(
        &'a self,
        image: PixelView<'a>,
        crops: &'a [CropDescriptor],
        language_hint: Option<&'a str>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<RecognitionResult, ConversionError>> {
        Box::pin(async move {
            validate_pixels(image)?;
            let language_hint = validate_language_hint(language_hint)?;
            if crops.len() > self.config.max_regions {
                return Err(limit("recognitionRegions"));
            }
            context.checkpoint()?;
            let mut order = Vec::new();
            order.try_reserve_exact(crops.len()).map_err(|_| limit("recognitionMemory"))?;
            for (source_index, crop) in crops.iter().enumerate() {
                order.push((source_index, validated_crop(crop, image, &self.config)?));
            }
            order.sort_by(|left, right| {
                left.1.ratio.total_cmp(&right.1.ratio).then_with(|| left.0.cmp(&right.0))
            });

            let result_slots_bytes = crops
                .len()
                .checked_mul(std::mem::size_of::<Option<RecognizedText>>())
                .ok_or_else(|| limit("recognitionMemory"))?;
            let mut result_reservation = context.reserve_memory(to_u64(result_slots_bytes)?)?;
            let mut slots = Vec::new();
            slots.try_reserve_exact(crops.len()).map_err(|_| limit("recognitionMemory"))?;
            slots.resize_with(crops.len(), || None);

            for batch in order.chunks(self.config.max_batch_size) {
                context.checkpoint()?;
                let prepared = prepare_batch(image, batch, &self.config, context)?;
                let outputs = self.runtime.run(MODEL_ID, &[prepared.tensor], context).await?;
                context.checkpoint()?;
                let decoded = decode_output(
                    &outputs,
                    batch,
                    &self.characters,
                    &self.config,
                    context,
                    &mut result_reservation,
                )?;
                for item in decoded {
                    let index = item.source_index;
                    if slots.get(index).is_none() || slots[index].is_some() {
                        return Err(ocr("recognitionOrderMismatch"));
                    }
                    slots[index] = Some(item);
                }
            }
            let regions = slots
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| ocr("recognitionOrderMismatch"))?;
            Ok(RecognitionResult {
                regions,
                provider: PROVIDER.into(),
                language_hint: language_hint.map(str::to_owned),
            })
        })
    }
}

fn validate_config(config: &RecognitionConfig) -> Result<(), ConversionError> {
    if config.max_regions == 0
        || config.max_batch_size == 0
        || config.max_batch_size > 8
        || config.max_crop_pixels == 0
        || config.max_tensor_elements == 0
        || config.max_output_timesteps == 0
        || config.max_output_timesteps > 1024
        || config.max_decoded_bytes == 0
    {
        return Err(ocr("invalidRecognitionConfig"));
    }
    Ok(())
}

fn validate_language_hint(hint: Option<&str>) -> Result<Option<&str>, ConversionError> {
    match hint {
        None => Ok(None),
        Some("zh" | "zh-Hans" | "zh-Hant" | "en") => Ok(hint),
        Some(_) => Err(ocr("unsupportedRecognitionLanguage")),
    }
}

fn to_u64(value: usize) -> Result<u64, ConversionError> {
    u64::try_from(value).map_err(|_| limit("recognitionMemory"))
}

fn ocr(detail: &str) -> ConversionError {
    ConversionError::Ocr { provider: PROVIDER.into(), detail: detail.into() }
}

fn limit(detail: &str) -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("PP-OCRv6 recognition bound exceeded: {detail}"),
    }
}
