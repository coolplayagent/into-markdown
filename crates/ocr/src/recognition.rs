//! Bounded PP-OCRv6 recognition preprocessing and CTC decoding.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use crate::{CropDescriptor, ModelManager, PixelFormat, PixelView};
use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, ResourceReservation, Tensor, TensorRuntime,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema_version: u32,
    model_id: String,
    upstream_repository: String,
    upstream_commit: String,
    preprocess_reference: String,
    postprocess_reference: String,
    runtime_archive_url: String,
    runtime_archive_size: u64,
    runtime_archive_sha256: String,
    runtime_model_member: String,
    runtime_model_size: u64,
    runtime_model_sha256: String,
    runtime_config_member: String,
    runtime_config_size: u64,
    runtime_config_sha256: String,
    character_table_url: String,
    character_table_size: u64,
    character_table_sha256: String,
    character_table_entries: usize,
    classes: usize,
    blank_index: usize,
    append_space: bool,
    license: String,
    ir_version: u64,
    opset_domain: String,
    opset_version: u64,
    input_name: String,
    input_dtype: String,
    input_shape: [String; 4],
    output_name: String,
    output_dtype: String,
    output_shape: [String; 3],
    input_color: String,
    resize_interpolation: String,
    normalization_scale: f64,
    normalization_mean: f32,
    normalization_standard_deviation: f32,
    maximum_width: usize,
}

fn authority() -> Result<Authority, ConversionError> {
    let value: Authority = serde_json::from_str(include_str!(
        "../../../models/ppocrv6-tiny-recognizer-authority.json"
    ))
    .map_err(|_| ocr("invalidRecognizerAuthority"))?;
    if value.schema_version != 1
        || value.model_id != MODEL_ID
        || value.upstream_repository != "https://github.com/PaddlePaddle/PaddleOCR"
        || value.upstream_commit != "2661c7c0ef5c613e8f93c6e93b2e052399f0f854"
        || value.preprocess_reference != "tools/infer/predict_rec.py"
        || value.postprocess_reference != "ppocr/postprocess/rec_postprocess.py"
        || value.runtime_archive_url
            != "https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/paddle3.0.0/PP-OCRv6_tiny_rec_onnx_infer.tar"
        || value.runtime_archive_size != 4_526_080
        || value.runtime_archive_sha256
            != "1e13b22717b1edd89d4cde4fda272b6c17d5b505c97c2baea99da1a3a2d54b29"
        || value.runtime_model_member != "PP-OCRv6_tiny_rec_onnx_infer/inference.onnx"
        || value.runtime_model_size != 4_462_639
        || value.runtime_model_sha256
            != "9ef676d6ed3c88256a2d92c640c44f25b0c40947e111b14b8be8f594091563e6"
        || value.runtime_config_member != "PP-OCRv6_tiny_rec_onnx_infer/inference.yml"
        || value.runtime_config_size != 55_571
        || value.runtime_config_sha256
            != "66170210bad538e83fff3c4a3867e547d6bf20b50d64b20347c4b913f3034ea1"
        || value.character_table_url
            != "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/2661c7c0ef5c613e8f93c6e93b2e052399f0f854/ppocr/utils/dict/ppocrv6_tiny_dict.txt"
        || value.character_table_size != 27_156
        || value.character_table_sha256
            != "c5cbe34ef40c29c4df07ed012bf96569cb69a2d2a01a07027e9f13cb832bd9cd"
        || value.character_table_entries != 6904
        || value.classes != CLASSES
        || value.blank_index != BLANK
        || !value.append_space
        || value.license != "Apache-2.0"
        || value.ir_version != 6
        || !value.opset_domain.is_empty()
        || value.opset_version != 11
        || value.input_name != "x"
        || value.input_dtype != "float32"
        || value.input_shape != ["N", "3", "48", "W"]
        || value.output_name != "fetch_name_0"
        || value.output_dtype != "float32"
        || value.output_shape != ["N", "T", "6906"]
        || value.input_color != "BGR"
        || value.resize_interpolation != "OpenCV-INTER_LINEAR"
        || (value.normalization_scale as f32).to_bits() != SCALE.to_bits()
        || value.normalization_mean.to_bits() != 0.5_f32.to_bits()
        || value.normalization_standard_deviation.to_bits() != 0.5_f32.to_bits()
        || value.maximum_width != MAX_WIDTH
    {
        return Err(ocr("recognizerAuthorityDrift"));
    }
    Ok(value)
}

fn validate_authority() -> Result<(), ConversionError> {
    authority().map(drop)
}

fn load_characters(bytes: &[u8]) -> Result<Vec<String>, ConversionError> {
    let expected = authority()?;
    if bytes.len() as u64 != expected.character_table_size
        || format!("{:x}", Sha256::digest(bytes)) != expected.character_table_sha256
    {
        return Err(ocr("characterTableHashMismatch"));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| ocr("invalidCharacterTable"))?;
    if source.contains('\r') || !source.ends_with('\n') {
        return Err(ocr("invalidCharacterTable"));
    }
    let mut result = Vec::new();
    result.try_reserve_exact(CLASSES - 1).map_err(|_| limit("recognitionMemory"))?;
    let mut unique = std::collections::BTreeSet::new();
    for line in source.lines() {
        if line.is_empty() || !unique.insert(line) {
            return Err(ocr("invalidCharacterTable"));
        }
        result.push(line.to_owned());
    }
    if result.len() != expected.character_table_entries || !unique.insert(" ") {
        return Err(ocr("invalidCharacterTable"));
    }
    result.push(" ".into());
    Ok(result)
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

#[derive(Clone, Copy)]
struct CropPlan {
    polygon: [(f32, f32); 4],
    width: usize,
    height: usize,
    rotate: bool,
    ratio: f64,
}

fn validated_crop(
    crop: &CropDescriptor,
    image: PixelView<'_>,
    config: &RecognitionConfig,
) -> Result<CropPlan, ConversionError> {
    if crop.width == 0 || crop.height == 0 {
        return Err(ocr("invalidRecognitionCrop"));
    }
    let width = crop.width as usize;
    let height = crop.height as usize;
    if width.checked_mul(height).is_none_or(|pixels| pixels > config.max_crop_pixels) {
        return Err(limit("recognitionCropPixels"));
    }
    for &(x, y) in &crop.polygon {
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x > (image.width - 1) as f32
            || y > (image.height - 1) as f32
        {
            return Err(ocr("invalidRecognitionCrop"));
        }
    }
    let area = crop.polygon.iter().enumerate().fold(0.0_f64, |sum, (index, &(x, y))| {
        let (nx, ny) = crop.polygon[(index + 1) % 4];
        sum + f64::from(x) * f64::from(ny) - f64::from(nx) * f64::from(y)
    });
    if !area.is_finite() || area.abs() <= 1.0 {
        return Err(ocr("invalidRecognitionCrop"));
    }
    let rotate = height as f64 / width as f64 >= 1.5;
    let ratio = if rotate { height as f64 / width as f64 } else { width as f64 / height as f64 };
    if !ratio.is_finite() || ratio * HEIGHT as f64 > MAX_WIDTH as f64 {
        return Err(limit("recognitionWidth"));
    }
    Ok(CropPlan { polygon: crop.polygon, width, height, rotate, ratio })
}

struct PreparedBatch {
    tensor: Tensor,
    _reservation: ResourceReservation,
}

fn prepare_batch(
    image: PixelView<'_>,
    batch: &[(usize, CropPlan)],
    config: &RecognitionConfig,
    context: &ExecutionContext,
) -> Result<PreparedBatch, ConversionError> {
    let width = batch
        .iter()
        .map(|(_, crop)| (crop.ratio * HEIGHT as f64).ceil() as usize)
        .max()
        .unwrap_or(BASE_WIDTH)
        .max(BASE_WIDTH);
    if width > MAX_WIDTH {
        return Err(limit("recognitionWidth"));
    }
    let elements = batch
        .len()
        .checked_mul(3)
        .and_then(|value| value.checked_mul(HEIGHT))
        .and_then(|value| value.checked_mul(width))
        .ok_or_else(|| limit("recognitionTensorElements"))?;
    if elements > config.max_tensor_elements {
        return Err(limit("recognitionTensorElements"));
    }
    let bytes = elements
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| limit("recognitionMemory"))?;
    let reservation = context.reserve_memory(to_u64(bytes)?)?;
    let mut values = Vec::new();
    values.try_reserve_exact(elements).map_err(|_| limit("recognitionMemory"))?;
    values.resize(elements, 0.0);
    for (batch_index, (_, crop)) in batch.iter().enumerate() {
        context.checkpoint()?;
        let resized_width = ((crop.ratio * HEIGHT as f64).ceil() as usize).clamp(1, width);
        write_crop_tensor(image, *crop, batch_index, width, resized_width, &mut values, context)?;
    }
    Ok(PreparedBatch {
        tensor: Tensor { shape: vec![batch.len(), 3, HEIGHT, width], values },
        _reservation: reservation,
    })
}

fn write_crop_tensor(
    image: PixelView<'_>,
    crop: CropPlan,
    batch: usize,
    tensor_width: usize,
    resized_width: usize,
    values: &mut [f32],
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for y in 0..HEIGHT {
        if y % 8 == 0 {
            context.checkpoint()?;
        }
        for x in 0..resized_width {
            let (crop_x, crop_y) = if crop.rotate {
                (
                    (y as f64 + 0.5) * crop.width as f64 / HEIGHT as f64 - 0.5,
                    (crop.height as f64 - 1.0)
                        - ((x as f64 + 0.5) * crop.height as f64 / resized_width as f64 - 0.5),
                )
            } else {
                (
                    (x as f64 + 0.5) * crop.width as f64 / resized_width as f64 - 0.5,
                    (y as f64 + 0.5) * crop.height as f64 / HEIGHT as f64 - 0.5,
                )
            };
            let source = perspective_source(crop, crop_x, crop_y)?;
            let pixel = bilinear_bgr(image, source.0, source.1)?;
            for (channel, byte) in pixel.into_iter().enumerate() {
                let offset = (((batch * 3 + channel) * HEIGHT + y) * tensor_width) + x;
                let scaled = f32::from(byte) * SCALE;
                values[offset] = (scaled - 0.5_f32) / 0.5_f32;
            }
        }
    }
    Ok(())
}

fn perspective_source(crop: CropPlan, x: f64, y: f64) -> Result<(f64, f64), ConversionError> {
    let u = if crop.width <= 1 { 0.0 } else { x / (crop.width - 1) as f64 };
    let v = if crop.height <= 1 { 0.0 } else { y / (crop.height - 1) as f64 };
    let [p0, p1, p2, p3] = crop.polygon;
    let (x0, y0) = (f64::from(p0.0), f64::from(p0.1));
    let (x1, y1) = (f64::from(p1.0), f64::from(p1.1));
    let (x2, y2) = (f64::from(p2.0), f64::from(p2.1));
    let (x3, y3) = (f64::from(p3.0), f64::from(p3.1));
    let dx1 = x1 - x2;
    let dx2 = x3 - x2;
    let dx3 = x0 - x1 + x2 - x3;
    let dy1 = y1 - y2;
    let dy2 = y3 - y2;
    let dy3 = y0 - y1 + y2 - y3;
    let (g, h) = if dx3.abs() <= f64::EPSILON && dy3.abs() <= f64::EPSILON {
        (0.0, 0.0)
    } else {
        let denominator = dx1 * dy2 - dx2 * dy1;
        if !denominator.is_finite() || denominator.abs() <= f64::EPSILON {
            return Err(ocr("invalidRecognitionCrop"));
        }
        ((dx3 * dy2 - dx2 * dy3) / denominator, (dx1 * dy3 - dx3 * dy1) / denominator)
    };
    let a = x1 - x0 + g * x1;
    let b = x3 - x0 + h * x3;
    let d = y1 - y0 + g * y1;
    let e = y3 - y0 + h * y3;
    let denominator = g * u + h * v + 1.0;
    if !denominator.is_finite() || denominator.abs() <= f64::EPSILON {
        return Err(ocr("invalidRecognitionCrop"));
    }
    let source = ((a * u + b * v + x0) / denominator, (d * u + e * v + y0) / denominator);
    if !source.0.is_finite()
        || !source.1.is_finite()
        || source.0 < -1.0
        || source.1 < -1.0
        || source.0 > image_coordinate_guard(crop.polygon, true)
        || source.1 > image_coordinate_guard(crop.polygon, false)
    {
        return Err(ocr("invalidRecognitionCrop"));
    }
    Ok(source)
}

fn image_coordinate_guard(polygon: [(f32, f32); 4], x_axis: bool) -> f64 {
    polygon
        .iter()
        .map(|point| if x_axis { f64::from(point.0) } else { f64::from(point.1) })
        .fold(0.0, f64::max)
        + 1.0
}

fn bilinear_bgr(image: PixelView<'_>, x: f64, y: f64) -> Result<[u8; 3], ConversionError> {
    let x = x.clamp(0.0, (image.width - 1) as f64);
    let y = y.clamp(0.0, (image.height - 1) as f64);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(image.width - 1);
    let y1 = (y0 + 1).min(image.height - 1);
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let samples = [
        raw_bgr(image, x0, y0)?,
        raw_bgr(image, x1, y0)?,
        raw_bgr(image, x0, y1)?,
        raw_bgr(image, x1, y1)?,
    ];
    let mut output = [0_u8; 3];
    for channel in 0..3 {
        let top = f64::from(samples[0][channel]) * (1.0 - fx) + f64::from(samples[1][channel]) * fx;
        let bottom =
            f64::from(samples[2][channel]) * (1.0 - fx) + f64::from(samples[3][channel]) * fx;
        output[channel] =
            (top * (1.0 - fy) + bottom * fy).round_ties_even().clamp(0.0, 255.0) as u8;
    }
    Ok(output)
}

fn raw_bgr(image: PixelView<'_>, x: usize, y: usize) -> Result<[u8; 3], ConversionError> {
    let channels = match image.format {
        PixelFormat::Gray8 => 1,
        PixelFormat::Rgb8 | PixelFormat::Bgr8 => 3,
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
    };
    let offset = y
        .checked_mul(image.row_stride)
        .and_then(|value| value.checked_add(x.checked_mul(channels)?))
        .ok_or_else(|| ocr("invalidPixelStride"))?;
    let pixel =
        image.bytes.get(offset..offset + channels).ok_or_else(|| ocr("truncatedPixelBuffer"))?;
    Ok(match image.format {
        PixelFormat::Gray8 => [pixel[0]; 3],
        PixelFormat::Rgb8 | PixelFormat::Rgba8 => [pixel[2], pixel[1], pixel[0]],
        PixelFormat::Bgr8 | PixelFormat::Bgra8 => [pixel[0], pixel[1], pixel[2]],
    })
}

fn validate_pixels(image: PixelView<'_>) -> Result<(), ConversionError> {
    if image.width == 0 || image.height == 0 {
        return Err(ocr("emptyImage"));
    }
    let channels = match image.format {
        PixelFormat::Gray8 => 1,
        PixelFormat::Rgb8 | PixelFormat::Bgr8 => 3,
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
    };
    let row = image.width.checked_mul(channels).ok_or_else(|| ocr("invalidPixelStride"))?;
    if image.row_stride < row {
        return Err(ocr("invalidPixelStride"));
    }
    let required = image
        .height
        .checked_sub(1)
        .and_then(|rows| rows.checked_mul(image.row_stride))
        .and_then(|bytes| bytes.checked_add(row))
        .ok_or_else(|| ocr("truncatedPixelBuffer"))?;
    if image.bytes.len() < required {
        return Err(ocr("truncatedPixelBuffer"));
    }
    Ok(())
}

fn decode_output(
    outputs: &[Tensor],
    batch: &[(usize, CropPlan)],
    characters: &[String],
    config: &RecognitionConfig,
    context: &ExecutionContext,
    reservation: &mut ResourceReservation,
) -> Result<Vec<RecognizedText>, ConversionError> {
    if outputs.len() != 1 {
        return Err(ocr("recognitionOutputCountMismatch"));
    }
    let output = &outputs[0];
    if output.shape.len() != 3
        || output.shape[0] != batch.len()
        || output.shape[2] != CLASSES
        || output.shape[1] == 0
        || output.shape[1] > config.max_output_timesteps
    {
        return Err(ocr("recognitionOutputShapeMismatch"));
    }
    let expected = batch
        .len()
        .checked_mul(output.shape[1])
        .and_then(|value| value.checked_mul(CLASSES))
        .ok_or_else(|| limit("recognitionOutputElements"))?;
    if output.values.len() != expected || output.values.iter().any(|value| !value.is_finite()) {
        return Err(ocr("invalidRecognitionOutput"));
    }
    if output.values.iter().any(|value| !(0.0..=1.0).contains(value)) {
        return Err(ocr("invalidRecognitionProbability"));
    }
    let decoded_peak = batch
        .len()
        .checked_mul(output.shape[1])
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| limit("recognitionDecodedBytes"))?
        .min(config.max_decoded_bytes);
    reservation.grow(to_u64(decoded_peak)?)?;
    let mut decoded = Vec::new();
    decoded.try_reserve_exact(batch.len()).map_err(|_| limit("recognitionMemory"))?;
    let mut total_bytes = 0_usize;
    for (batch_index, &(source_index, _)) in batch.iter().enumerate() {
        context.checkpoint()?;
        let mut text = String::new();
        let mut probability_sum = 0.0_f64;
        let mut kept = 0_usize;
        let mut previous = usize::MAX;
        for timestep in 0..output.shape[1] {
            if timestep % 32 == 0 {
                context.checkpoint()?;
            }
            let start = (batch_index * output.shape[1] + timestep) * CLASSES;
            let scores = &output.values[start..start + CLASSES];
            let mut best = 0_usize;
            for index in 1..CLASSES {
                if scores[index] > scores[best] {
                    best = index;
                }
            }
            if best != previous && best != BLANK {
                let character =
                    characters.get(best - 1).ok_or_else(|| ocr("invalidCharacterIndex"))?;
                total_bytes = total_bytes
                    .checked_add(character.len())
                    .ok_or_else(|| limit("recognitionDecodedBytes"))?;
                if total_bytes > config.max_decoded_bytes {
                    return Err(limit("recognitionDecodedBytes"));
                }
                text.try_reserve_exact(character.len()).map_err(|_| limit("recognitionMemory"))?;
                text.push_str(character);
                probability_sum += f64::from(scores[best]);
                kept += 1;
            }
            previous = best;
        }
        let confidence = if kept == 0 { 0.0 } else { (probability_sum / kept as f64) as f32 };
        decoded.push(RecognizedText { source_index, text, confidence });
    }
    Ok(decoded)
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn characters() -> Vec<String> {
        load_characters(include_bytes!("../../../models/ppocrv6_tiny_dict.txt")).unwrap()
    }

    #[test]
    fn official_authority_and_character_table_are_exact() {
        let value = authority().unwrap();
        assert_eq!(value.classes, characters().len() + 1);
        assert_eq!(SCALE.to_bits(), 0x3b808081);
    }

    #[test]
    fn ctc_collapses_before_blank_and_ties_choose_lowest_index() {
        let chars = characters();
        let steps = [1_usize, 1, 0, 1, 2, 2, 0];
        let mut values = vec![0.0; steps.len() * CLASSES];
        for (time, &class) in steps.iter().enumerate() {
            values[time * CLASSES + class] = 0.75;
        }
        // Equal score at a later index must not replace the earlier argmax.
        values[4 * CLASSES + 3] = 0.75;
        let output = Tensor { shape: vec![1, steps.len(), CLASSES], values };
        let batch = [(
            7,
            CropPlan { polygon: [(0.0, 0.0); 4], width: 1, height: 1, rotate: false, ratio: 1.0 },
        )];
        let mut reservation = context().reserve_memory(1024).unwrap();
        let decoded = decode_output(
            &[output],
            &batch,
            &chars,
            &RecognitionConfig::default(),
            &context(),
            &mut reservation,
        )
        .unwrap();
        assert_eq!(decoded[0].source_index, 7);
        assert_eq!(decoded[0].text, format!("{}{}{}", chars[0], chars[0], chars[1]));
        assert_eq!(decoded[0].confidence.to_bits(), 0.75_f32.to_bits());
    }

    struct ShapeRuntime;

    impl TensorRuntime for ShapeRuntime {
        fn id(&self) -> &'static str {
            "test.shape"
        }

        fn run<'a>(
            &'a self,
            _: &'a str,
            inputs: &'a [Tensor],
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
            Box::pin(async move {
                let batch = inputs[0].shape[0];
                let mut values = vec![0.0; batch * CLASSES];
                for item in 0..batch {
                    values[item * CLASSES + 1] = 1.0;
                }
                Ok(vec![Tensor { shape: vec![batch, 1, CLASSES], values }])
            })
        }
    }

    #[test]
    fn stable_width_sort_is_restored_to_source_order() {
        let recognizer = PpOcrTextRecognizer::new(
            Arc::new(ShapeRuntime),
            include_bytes!("../../../models/ppocrv6_tiny_dict.txt"),
            RecognitionConfig { max_batch_size: 2, ..RecognitionConfig::default() },
        )
        .unwrap();
        let bytes = vec![128; 40 * 40 * 3];
        let image = PixelView {
            width: 40,
            height: 40,
            row_stride: 120,
            format: PixelFormat::Bgr8,
            orientation: crate::ImageOrientation::Rotate90,
            bytes: &bytes,
        };
        let crops = [
            CropDescriptor {
                polygon: [(0.0, 0.0), (29.0, 0.0), (29.0, 9.0), (0.0, 9.0)],
                width: 30,
                height: 10,
            },
            CropDescriptor {
                polygon: [(0.0, 20.0), (9.0, 20.0), (9.0, 29.0), (0.0, 29.0)],
                width: 10,
                height: 10,
            },
        ];
        let result =
            block_on(recognizer.recognize(image, &crops, Some("zh-Hant"), &context())).unwrap();
        assert_eq!(result.regions.iter().map(|item| item.source_index).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(result.language_hint.as_deref(), Some("zh-Hant"));
    }

    #[test]
    fn crop_coordinates_are_raw_source_coordinates_even_with_exif_orientation() {
        let bytes = [
            0, 0, 255, 0, 255, 0, // red, green in BGR
            255, 0, 0, 255, 255, 255, // blue, white
        ];
        let image = PixelView {
            width: 2,
            height: 2,
            row_stride: 6,
            format: PixelFormat::Bgr8,
            orientation: crate::ImageOrientation::Rotate270,
            bytes: &bytes,
        };
        assert_eq!(raw_bgr(image, 0, 0).unwrap(), [0, 0, 255]);
        assert_eq!(raw_bgr(image, 1, 0).unwrap(), [0, 255, 0]);
    }

    #[test]
    fn malformed_outputs_and_limits_fail_closed() {
        let chars = characters();
        let batch = [(
            0,
            CropPlan { polygon: [(0.0, 0.0); 4], width: 1, height: 1, rotate: false, ratio: 1.0 },
        )];
        for output in [
            Tensor { shape: vec![1, 1, CLASSES - 1], values: vec![0.0; CLASSES - 1] },
            Tensor { shape: vec![1, 1, CLASSES], values: vec![f32::NAN; CLASSES] },
            Tensor { shape: vec![1, 2, CLASSES], values: vec![0.0; CLASSES] },
        ] {
            let mut reservation = context().reserve_memory(1024).unwrap();
            assert!(
                decode_output(
                    &[output],
                    &batch,
                    &chars,
                    &RecognitionConfig::default(),
                    &context(),
                    &mut reservation,
                )
                .is_err()
            );
        }
    }
}
