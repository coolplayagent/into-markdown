//! Product OCR engine joining the audited image, detector, and recognizer boundaries.

use crate::{
    DetectionConfig, Dimension, ImageOrientation, ModelContract, ModelManager, PixelFormat,
    PixelView, PpOcrTextDetector, PpOcrTextRecognizer, RecognitionConfig,
};
use image::{DynamicImage, ImageFormat, ImageReader};
use into_markdown_core::{
    BoundOcrResult, BoxFuture, ConversionError, ConversionOptions, ExecutionContext, OcrEngine,
    OcrEvidenceStage, OcrEvidenceStep, OcrInputIdentity, OcrOutputPlan, OcrRecognition, OcrRegion,
    OcrRequest, OcrResult, ResourceLimits, Tensor, TensorRuntime,
};
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::sync::Arc;

const PROVIDER: &str = "builtin.ocr.ppocrv6-image";
const DETECTOR_PROVIDER: &str = "builtin.ocr.ppocrv6-detector";
const RECOGNIZER_PROVIDER: &str = "builtin.ocr.ppocrv6-recognizer";
const MAX_DIMENSION: u32 = 32_768;
const MAX_REGIONS: u32 = 3000;
const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;
const OUTPUT_BYTES_PER_REGION: u64 = 2048;
const OUTPUT_FIXED_BYTES: u64 = 16 * 1024;

/// Installed, offline PP-OCRv6 image pipeline.
pub struct PpOcrImageEngine {
    runtime: Arc<dyn TensorRuntime>,
    manager: Arc<ModelManager>,
    limits: ResourceLimits,
}

impl PpOcrImageEngine {
    /// Validate that the fixed product pipeline can retain its bounded output
    /// within one invocation before loading the native runtime.
    ///
    /// # Errors
    ///
    /// Returns cancellation or timeout from `context`, or a resource error when
    /// the configured region, text, decompression, or memory envelope cannot
    /// support a bounded OCR invocation.
    pub fn validate_service_limits(
        limits: &ResourceLimits,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        context.checkpoint()?;
        validate_engine_limits(limits)?;
        let plan = output_plan(limits, 1, 1)?;
        if plan.max_retained_bytes() > limits.max_memory_bytes
            || plan.max_retained_bytes() > context.available_memory_bytes()
        {
            return Err(resource(
                "max_memory_bytes",
                "OCR output plan exceeds the service memory envelope",
            ));
        }
        Ok(())
    }

    /// Construct only after both exact pipeline components are locally installed and verified.
    pub fn from_installed(
        runtime: Arc<dyn TensorRuntime>,
        manager: Arc<ModelManager>,
        limits: ResourceLimits,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        Self::validate_service_limits(&limits, context)?;
        manager
            .verify_with_context(crate::detector_model::PIPELINE_ID, context)
            .map_err(map_manager_error)?;
        Ok(Self { runtime, manager, limits })
    }

    async fn execute(
        &self,
        request: OcrRequest<'_>,
        context: &ExecutionContext,
    ) -> Result<BoundOcrResult, ConversionError> {
        context.checkpoint()?;
        self.manager
            .verify_with_context(crate::detector_model::PIPELINE_ID, context)
            .map_err(map_manager_error)?;
        let limits = effective_limits(&self.limits, context.resource_limits());
        let max_source_pixels = source_pixel_limit(&limits)?;
        let max_regions =
            usize::try_from(limits.max_archive_entries.min(MAX_REGIONS)).map_err(|_| {
                resource("max_archive_entries", "OCR region bound is not representable")
            })?;
        let detector = PpOcrTextDetector::new(
            Arc::clone(&self.runtime),
            DetectionConfig {
                max_source_pixels,
                max_contour_events: max_source_pixels.min(16_000_000),
                max_contour_points: max_source_pixels.min(16_000_000),
                max_score_pixels: max_source_pixels.min(32_000_000),
                ..DetectionConfig::default()
            },
        )?;
        let recognizer = PpOcrTextRecognizer::from_installed(
            Arc::clone(&self.runtime),
            &self.manager,
            RecognitionConfig {
                max_regions,
                max_decoded_bytes: usize::try_from(limits.max_field_bytes.min(MAX_TEXT_BYTES))
                    .map_err(|_| {
                        resource("max_field_bytes", "OCR text bound is not representable")
                    })?,
                ..RecognitionConfig::default()
            },
            context,
        )?;
        let (pixels, _memory) = decode_normalized_png(request, &limits, context)?;
        let image = PixelView {
            width: pixels.width() as usize,
            height: pixels.height() as usize,
            row_stride: pixels.width() as usize * 3,
            format: PixelFormat::Rgb8,
            orientation: ImageOrientation::Normal,
            bytes: pixels.as_raw(),
        };
        let detected = detector.detect_page(1, image, context).await?;
        let language = language_hint(request.languages)?;
        let recognition_result =
            recognizer.recognize_page(image, &detected, language, context).await?;
        context.checkpoint()?;
        let identity = OcrInputIdentity::try_new(
            Sha256::digest(request.image).into(),
            pixels.width(),
            pixels.height(),
            0,
        )?;
        bind_result(&detected, recognition_result.result(), identity, context)
    }
}

impl OcrEngine for PpOcrImageEngine {
    fn id(&self) -> &'static str {
        PROVIDER
    }

    fn recognize<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(
            async move { self.execute(request, context).await.map(|value| value.into_parts().0) },
        )
    }

    fn planned_bound_output(
        &self,
        request: OcrRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        context.checkpoint()?;
        let limits = effective_limits(&self.limits, &options.limits);
        let (width, height) = validate_png_header(request, &limits)?;
        output_plan(&limits, width, height)
    }

    fn planned_normalized_png_output(
        &self,
        width: u32,
        height: u32,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        context.checkpoint()?;
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(resource(
                "image_dimensions",
                "OCR PNG dimensions are outside product bounds",
            ));
        }
        let decoded = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| resource("max_decompressed_bytes", "OCR PNG size overflow"))?;
        let limits = effective_limits(&self.limits, &options.limits);
        if decoded > limits.max_decompressed_bytes {
            return Err(resource("max_decompressed_bytes", "OCR PNG dimensions exceed limits"));
        }
        output_plan(&limits, width, height)
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move { self.execute(request, context).await.map(OcrRecognition::Bound) })
    }
}

fn effective_limits(service: &ResourceLimits, request: &ResourceLimits) -> ResourceLimits {
    let mut limits = request.clone();
    limits.max_input_bytes = limits.max_input_bytes.min(service.max_input_bytes);
    limits.max_decompressed_bytes =
        limits.max_decompressed_bytes.min(service.max_decompressed_bytes);
    limits.max_archive_entries = limits.max_archive_entries.min(service.max_archive_entries);
    limits.max_memory_bytes = limits.max_memory_bytes.min(service.max_memory_bytes);
    limits.max_field_bytes = limits.max_field_bytes.min(service.max_field_bytes);
    limits
}

fn decode_normalized_png(
    request: OcrRequest<'_>,
    limits: &ResourceLimits,
    context: &ExecutionContext,
) -> Result<(image::RgbImage, into_markdown_core::ResourceReservation), ConversionError> {
    let (width, height) = validate_png_header(request, limits)?;
    let working = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(16))
        .ok_or_else(|| resource("max_memory_bytes", "OCR image working set overflow"))?;
    let memory = context.reserve_memory(working)?;
    context.checkpoint()?;
    let mut reader = ImageReader::with_format(Cursor::new(request.image), ImageFormat::Png);
    let mut decoder_limits = image::Limits::default();
    decoder_limits.max_image_width = Some(MAX_DIMENSION);
    decoder_limits.max_image_height = Some(MAX_DIMENSION);
    decoder_limits.max_alloc = Some(limits.max_decompressed_bytes.min(limits.max_memory_bytes));
    reader.limits(decoder_limits);
    let decoded = reader.decode().map_err(|error| map_image_error(&error))?;
    if decoded.width() != width || decoded.height() != height {
        return Err(malformed("PNG decoder dimensions disagree with IHDR"));
    }
    context.checkpoint()?;
    Ok((DynamicImage::into_rgb8(decoded), memory))
}

fn validate_png_header(
    request: OcrRequest<'_>,
    limits: &ResourceLimits,
) -> Result<(u32, u32), ConversionError> {
    if request.media_type != "image/png" {
        return Err(ConversionError::Unsupported {
            detail: "PP-OCRv6 image input must be normalized image/png".into(),
        });
    }
    if u64::try_from(request.image.len()).unwrap_or(u64::MAX) > limits.max_input_bytes {
        return Err(resource("max_input_bytes", "OCR PNG exceeds the source-byte bound"));
    }
    if request.image.len() < 33
        || request.image[..8] != [137, 80, 78, 71, 13, 10, 26, 10]
        || request.image[8..12] != [0, 0, 0, 13]
        || &request.image[12..16] != b"IHDR"
    {
        return Err(malformed("OCR input is not a canonical PNG envelope"));
    }
    let width = u32::from_be_bytes(request.image[16..20].try_into().expect("fixed slice"));
    let height = u32::from_be_bytes(request.image[20..24].try_into().expect("fixed slice"));
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(resource("image_dimensions", "OCR PNG dimensions are outside product bounds"));
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| resource("max_decompressed_bytes", "OCR pixel count overflow"))?;
    if pixels > u64::try_from(source_pixel_limit(limits)?).unwrap_or(u64::MAX)
        || pixels.saturating_mul(4) > limits.max_decompressed_bytes
    {
        return Err(resource("max_decompressed_bytes", "OCR PNG pixel payload exceeds limits"));
    }
    Ok((width, height))
}

fn bind_result(
    detected: &crate::PageDetection,
    recognized: &crate::RecognitionResult,
    identity: OcrInputIdentity,
    context: &ExecutionContext,
) -> Result<BoundOcrResult, ConversionError> {
    if recognized.provider.as_ref() != RECOGNIZER_PROVIDER
        || recognized.regions.len() != detected.result().regions.len()
    {
        return Err(ocr("detector and recognizer payloads disagree"));
    }
    let count = recognized.regions.len();
    let mut regions = Vec::new();
    let mut confidences = Vec::new();
    regions
        .try_reserve_exact(count)
        .map_err(|_| resource("max_memory_bytes", "OCR result allocation failed"))?;
    confidences
        .try_reserve_exact(count)
        .map_err(|_| resource("max_memory_bytes", "OCR confidence allocation failed"))?;
    for (index, (text, detection)) in
        recognized.regions.iter().zip(&detected.result().regions).enumerate()
    {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        if text.source_index != index {
            return Err(ocr("recognizer source order disagrees with detector order"));
        }
        regions.push(OcrRegion {
            text: text.text.clone(),
            polygon: detection.polygon,
            confidence: text.confidence,
        });
        confidences.push(detection.confidence);
    }
    BoundOcrResult::try_new_for_input(
        OcrResult { regions, provider: RECOGNIZER_PROVIDER.into() },
        confidences,
        vec![
            OcrEvidenceStep {
                stage: OcrEvidenceStage::Detection,
                provider: DETECTOR_PROVIDER.into(),
                model: Some(crate::detector_model::DETECTOR_MODEL_ID.into()),
            },
            OcrEvidenceStep {
                stage: OcrEvidenceStage::Recognition,
                provider: RECOGNIZER_PROVIDER.into(),
                model: Some(crate::recognizer_model::RECOGNIZER_MODEL_ID.into()),
            },
        ],
        identity,
    )
}

fn language_hint<'a>(languages: &'a [&'a str]) -> Result<Option<&'a str>, ConversionError> {
    let mut selected = None;
    for language in languages {
        if !matches!(*language, "zh-Hans" | "zh-Hant" | "en") {
            return Err(ocr("unsupported OCR language hint"));
        }
        if selected.is_some_and(|value| value != *language) {
            return Ok(None);
        }
        selected = Some(*language);
    }
    Ok(selected)
}

fn source_pixel_limit(limits: &ResourceLimits) -> Result<usize, ConversionError> {
    usize::try_from(limits.max_decompressed_bytes / 4)
        .map(|pixels| pixels.min(100_000_000))
        .map_err(|_| resource("max_decompressed_bytes", "OCR pixel limit is not representable"))
}

fn validate_engine_limits(limits: &ResourceLimits) -> Result<(), ConversionError> {
    if limits.max_archive_entries.min(MAX_REGIONS) == 0
        || limits.max_field_bytes.min(MAX_TEXT_BYTES) == 0
    {
        return Err(resource("max_archive_entries", "OCR region and text bounds must be non-zero"));
    }
    source_pixel_limit(limits).map(|_| ())
}

fn output_plan(
    limits: &ResourceLimits,
    width: u32,
    height: u32,
) -> Result<OcrOutputPlan, ConversionError> {
    let max_regions = limits.max_archive_entries.min(MAX_REGIONS);
    let max_text = limits.max_field_bytes.min(MAX_TEXT_BYTES);
    let retained = u64::from(max_regions)
        .checked_mul(OUTPUT_BYTES_PER_REGION)
        .and_then(|bytes| bytes.checked_add(max_text.checked_mul(2)?))
        .and_then(|bytes| bytes.checked_add(OUTPUT_FIXED_BYTES))
        .ok_or_else(|| resource("max_memory_bytes", "OCR output plan overflow"))?;
    let working = provider_working_plan(width, height)?;
    OcrOutputPlan::try_new_with_working(retained, working, max_regions, max_text)
}

fn provider_working_plan(width: u32, height: u32) -> Result<u64, ConversionError> {
    let decoded = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(16))
        .ok_or_else(|| resource("max_memory_bytes", "OCR decoded working-set plan overflow"))?;
    let detector = model_run_phase(&crate::ppocrv6_detector_contract())?;
    let recognizer = model_run_phase(&crate::ppocrv6_recognizer_contract())?;
    decoded
        .checked_add(detector.max(recognizer))
        .ok_or_else(|| resource("max_memory_bytes", "OCR provider working-set plan overflow"))
}

fn model_run_phase(contract: &ModelContract) -> Result<u64, ConversionError> {
    let input_storage = max_tensor_storage(&contract.inputs)?;
    let output_storage = max_tensor_storage(&contract.outputs)?;
    let input_entries = u64::try_from(contract.inputs.len())
        .ok()
        .and_then(|count| count.checked_mul(std::mem::size_of::<(String, Tensor)>() as u64))
        .ok_or_else(|| resource("max_memory_bytes", "OCR input plan overflow"))?;
    let output_entries = u64::try_from(contract.outputs.len())
        .ok()
        .and_then(|count| count.checked_mul(std::mem::size_of::<Tensor>() as u64))
        .ok_or_else(|| resource("max_memory_bytes", "OCR output plan overflow"))?;
    // The prepared input remains live while the runtime clones it. Runtime
    // output is charged twice (native backing plus the checked Rust copy),
    // matching the executable runtime boundary's run_memory_peak contract.
    input_storage
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(input_entries))
        .and_then(|bytes| bytes.checked_add(output_entries))
        .and_then(|bytes| {
            output_storage.checked_mul(2).and_then(|output| bytes.checked_add(output))
        })
        .and_then(|bytes| bytes.checked_add(contract.run_memory_bytes))
        .ok_or_else(|| resource("max_memory_bytes", "OCR model working-set plan overflow"))
}

fn max_tensor_storage(specs: &[crate::TensorSpec]) -> Result<u64, ConversionError> {
    specs.iter().try_fold(0_u64, |total, spec| {
        let elements = spec.dimensions.iter().try_fold(1_u64, |count, dimension| {
            let maximum = match dimension {
                Dimension::Exact(value) => *value,
                Dimension::Dynamic { max, .. } => *max,
            };
            count
                .checked_mul(u64::try_from(maximum).map_err(|_| {
                    resource("max_memory_bytes", "OCR tensor dimension is not representable")
                })?)
                .ok_or_else(|| resource("max_memory_bytes", "OCR tensor plan overflow"))
        })?;
        let shape = u64::try_from(spec.dimensions.len())
            .ok()
            .and_then(|rank| rank.checked_mul(std::mem::size_of::<usize>() as u64))
            .ok_or_else(|| resource("max_memory_bytes", "OCR tensor shape plan overflow"))?;
        total
            .checked_add(
                elements
                    .checked_mul(std::mem::size_of::<f32>() as u64)
                    .and_then(|bytes| bytes.checked_add(shape))
                    .ok_or_else(|| resource("max_memory_bytes", "OCR tensor plan overflow"))?,
            )
            .ok_or_else(|| resource("max_memory_bytes", "OCR tensor plan overflow"))
    })
}

fn map_manager_error(error: crate::ModelManagerError) -> ConversionError {
    match error {
        crate::ModelManagerError::Execution(error) => error,
        crate::ModelManagerError::Corrupt(_)
        | crate::ModelManagerError::DataDirectoryUnsafe
        | crate::ModelManagerError::UnsafePath => ocr("installed OCR pipeline is corrupt"),
        _ => ConversionError::ComponentUnavailable {
            component: crate::detector_model::PIPELINE_ID.into(),
            detail: "install the exact detector and recognizer components before OCR".into(),
        },
    }
}

fn map_image_error(error: &image::ImageError) -> ConversionError {
    match error {
        image::ImageError::Limits(_) => resource("max_decompressed_bytes", error.to_string()),
        _ => malformed(format!("PNG decoder rejected normalized OCR input: {error}")),
    }
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("ocr.image".into()), detail: detail.into() }
}

fn resource(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}

fn ocr(detail: impl Into<String>) -> ConversionError {
    ConversionError::Ocr { provider: PROVIDER.into(), detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_limits_never_exceed_request_or_service_envelope() {
        let service = ResourceLimits::default();
        let mut request = service.clone();
        request.max_input_bytes = 12;
        request.max_decompressed_bytes = 34;
        request.max_archive_entries = 2;
        request.max_memory_bytes = 56;
        request.max_field_bytes = 7;

        let effective = effective_limits(&service, &request);
        assert_eq!(effective.max_input_bytes, 12);
        assert_eq!(effective.max_decompressed_bytes, 34);
        assert_eq!(effective.max_archive_entries, 2);
        assert_eq!(effective.max_memory_bytes, 56);
        assert_eq!(effective.max_field_bytes, 7);

        let mut tighter_service = service;
        tighter_service.max_archive_entries = 1;
        assert_eq!(effective_limits(&tighter_service, &request).max_archive_entries, 1);
    }
}
