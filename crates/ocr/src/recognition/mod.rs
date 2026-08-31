//! Bounded PP-OCRv6 recognition preprocessing and CTC decoding.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use crate::{BoundRecognition, CropDescriptor, ModelManager, PageDetection, PixelView};
use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, ResourceReservation, TensorRuntime,
};
use std::sync::Arc;

pub(crate) mod authority;
mod budget;
mod ctc;
pub(crate) mod model_authority;
mod pixels;
mod preprocess;
#[cfg(test)]
mod tests;

use authority::{load_characters, validate_authority};
use budget::{reserve_tensors, reserve_vec, to_u64, validate_config, validate_language_hint};
use ctc::decode_output;
use pixels::validate_pixels;
use preprocess::{prepare_batch, validated_crop};

const PROVIDER: &str = "builtin.ocr.ppocrv6-recognizer";
const MODEL_ID: &str = "pp-ocrv6-tiny-recognizer-onnx";
const HEIGHT: usize = 48;
const BASE_WIDTH: usize = 320;
const MAX_WIDTH: usize = 3200;
const CLASSES: usize = 6906;
const BLANK: usize = 0;
const SCALE: f32 = 1.0 / 255.0;
const MAX_REGIONS: usize = 3000;
const MAX_CROP_PIXELS: usize = 32_000_000;
const MAX_TENSOR_ELEMENTS: usize = 32_000_000;
const MAX_OUTPUT_TIMESTEPS: usize = 1024;
const MAX_DECODED_BYTES: usize = 16 * 1024 * 1024;

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
            max_regions: MAX_REGIONS,
            max_batch_size: 8,
            max_crop_pixels: MAX_CROP_PIXELS,
            max_tensor_elements: MAX_TENSOR_ELEMENTS,
            max_output_timesteps: MAX_OUTPUT_TIMESTEPS,
            max_decoded_bytes: MAX_DECODED_BYTES,
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
#[derive(Debug, Clone, Default)]
pub struct RecognitionResult {
    pub regions: Arc<[RecognizedText]>,
    pub provider: Arc<str>,
    pub language_hint: Option<Arc<str>>,
    pub(crate) _memory_lease: Option<Arc<ResourceReservation>>,
}

impl PartialEq for RecognitionResult {
    fn eq(&self, other: &Self) -> bool {
        self.regions == other.regions
            && self.provider == other.provider
            && self.language_hint == other.language_hint
    }
}

/// Offline recognizer over the audited tensor-runtime seam.
pub struct PpOcrTextRecognizer {
    runtime: Arc<dyn TensorRuntime>,
    characters: Arc<[String]>,
    _character_lease: Arc<ResourceReservation>,
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
            .map_err(crate::recognizer_model::map_manager_error)?;
        Self::new(runtime, &artifact.bytes, config, context)
    }
    pub fn new(
        runtime: Arc<dyn TensorRuntime>,
        dictionary: &[u8],
        config: RecognitionConfig,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        validate_authority()?;
        validate_config(&config)?;
        let (characters, character_lease) = load_characters(dictionary, context)?;
        Ok(Self { runtime, characters, _character_lease: character_lease, config })
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
            let mut order_reservation =
                reserve_vec::<(usize, preprocess::CropPlan)>(crops.len(), context)?;
            let mut order = Vec::new();
            order.try_reserve_exact(crops.len()).map_err(|_| limit("recognitionMemory"))?;
            if order.capacity() > crops.len() {
                order_reservation.grow(to_u64(
                    (order.capacity() - crops.len())
                        .checked_mul(std::mem::size_of::<(usize, preprocess::CropPlan)>())
                        .ok_or_else(|| limit("recognitionMemory"))?,
                )?)?;
            }
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
            let actual_slots_bytes = slots
                .capacity()
                .checked_mul(std::mem::size_of::<Option<RecognizedText>>())
                .ok_or_else(|| limit("recognitionMemory"))?;
            if actual_slots_bytes > result_slots_bytes {
                result_reservation.grow(to_u64(actual_slots_bytes - result_slots_bytes)?)?;
            }
            slots.resize_with(crops.len(), || None);
            let mut total_decoded_bytes = 0_usize;

            for batch in order.chunks(self.config.max_batch_size) {
                context.checkpoint()?;
                let prepared = prepare_batch(image, batch, &self.config, context)?;
                let outputs = self.runtime.run(MODEL_ID, &[prepared.tensor], context).await?;
                let _output_reservation = reserve_tensors(&outputs, context)?;
                context.checkpoint()?;
                let decoded = decode_output(
                    &outputs,
                    batch,
                    &self.characters,
                    &self.config,
                    context,
                    &mut result_reservation,
                    &mut total_decoded_bytes,
                )?;
                for item in decoded.items {
                    let index = item.source_index;
                    if slots.get(index).is_none() || slots[index].is_some() {
                        return Err(ocr("recognitionOrderMismatch"));
                    }
                    slots[index] = Some(item);
                }
            }
            let requested_region_bytes = crops
                .len()
                .checked_mul(std::mem::size_of::<RecognizedText>())
                .ok_or_else(|| limit("recognitionMemory"))?;
            result_reservation.grow(to_u64(requested_region_bytes)?)?;
            let mut regions = Vec::new();
            regions.try_reserve_exact(crops.len()).map_err(|_| limit("recognitionMemory"))?;
            let actual_region_bytes = regions
                .capacity()
                .checked_mul(std::mem::size_of::<RecognizedText>())
                .ok_or_else(|| limit("recognitionMemory"))?;
            if actual_region_bytes > requested_region_bytes {
                result_reservation.grow(to_u64(actual_region_bytes - requested_region_bytes)?)?;
            }
            for slot in slots {
                regions.push(slot.ok_or_else(|| ocr("recognitionOrderMismatch"))?);
            }
            result_reservation.shrink(to_u64(actual_slots_bytes)?)?;
            let region_vec_bytes = regions
                .capacity()
                .checked_mul(std::mem::size_of::<RecognizedText>())
                .ok_or_else(|| limit("recognitionMemory"))?;
            let region_slice_bytes = regions
                .len()
                .checked_mul(std::mem::size_of::<RecognizedText>())
                .ok_or_else(|| limit("recognitionMemory"))?;
            result_reservation.grow(to_u64(region_slice_bytes)?)?;
            let regions = Arc::<[RecognizedText]>::from(regions);
            result_reservation.shrink(to_u64(region_vec_bytes)?)?;

            result_reservation.grow(to_u64(PROVIDER.len())?)?;
            let provider = Arc::<str>::from(PROVIDER);
            if let Some(hint) = language_hint {
                result_reservation.grow(to_u64(hint.len())?)?;
            }
            let language_hint = language_hint.map(Arc::<str>::from);
            Ok(RecognitionResult {
                regions,
                provider,
                language_hint,
                _memory_lease: Some(Arc::new(result_reservation)),
            })
        })
    }

    /// Recognize regions from a detector-produced, page-scoped batch binding.
    #[must_use]
    pub fn recognize_page<'a>(
        &'a self,
        image: PixelView<'a>,
        detection: &'a PageDetection,
        language_hint: Option<&'a str>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<BoundRecognition, ConversionError>> {
        Box::pin(async move {
            detection.identity.validate(&detection.result)?;
            if (image.width as f32).to_bits() != detection.page_width().to_bits()
                || (image.height as f32).to_bits() != detection.page_height().to_bits()
            {
                return Err(ocr("recognitionPageGeometryMismatch"));
            }
            let mut crop_reservation =
                reserve_vec::<CropDescriptor>(detection.result.regions.len(), context)?;
            let mut crops = Vec::new();
            crops
                .try_reserve_exact(detection.result.regions.len())
                .map_err(|_| limit("recognitionMemory"))?;
            let requested = detection
                .result
                .regions
                .len()
                .checked_mul(std::mem::size_of::<CropDescriptor>())
                .ok_or_else(|| limit("recognitionMemory"))?;
            let actual = crops
                .capacity()
                .checked_mul(std::mem::size_of::<CropDescriptor>())
                .ok_or_else(|| limit("recognitionMemory"))?;
            if actual > requested {
                crop_reservation.grow(to_u64(actual - requested)?)?;
            }
            crops.extend(detection.result.regions.iter().map(|region| region.crop.clone()));
            let output = self.recognize(image, &crops, language_hint, context).await?;
            BoundRecognition::new(output, detection.identity.clone(), context)
        })
    }
}

fn ocr(detail: &str) -> ConversionError {
    ConversionError::Ocr { provider: PROVIDER.into(), detail: detail.into() }
}

fn limit(detail: &'static str) -> ConversionError {
    ConversionError::ResourceLimit {
        limit: detail,
        detail: format!("PP-OCRv6 recognition bound exceeded: {detail}"),
    }
}
