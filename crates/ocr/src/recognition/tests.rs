use super::authority::{authority, load_characters};
use super::ctc::decode_output;
use super::pixels::raw_bgr;
use super::preprocess::{CropPlan, rotated_to_crop};
use super::*;
use crate::{DetectionConfig, ImageOrientation, PixelFormat, PpOcrTextDetector};
use futures::executor::block_on;
use into_markdown_core::{CancellationToken, ExecutionOptions, ResourceLimits, Tensor};

fn context() -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
}

fn characters(
    context: &ExecutionContext,
) -> (Arc<[String]>, Arc<into_markdown_core::ResourceReservation>) {
    load_characters(include_bytes!("../../../../models/ppocrv6_tiny_dict.txt"), context).unwrap()
}

#[test]
fn official_authority_and_character_table_are_exact() {
    let value = authority().unwrap();
    let execution = context();
    let (characters, _lease) = characters(&execution);
    assert_eq!(value.classes, characters.len() + 1);
    assert_eq!(SCALE.to_bits(), 0x3b80_8081);
    assert!(
        super::model_authority::validate_runtime_model_identity(
            MODEL_ID,
            &value.runtime_model_sha256,
            value.runtime_model_size,
        )
        .is_ok()
    );
    assert!(
        super::model_authority::validate_runtime_model_identity(MODEL_ID, &"a".repeat(64), 1)
            .is_err()
    );
}

#[test]
fn public_config_can_only_tighten_product_resource_bounds() {
    for config in [
        RecognitionConfig { max_regions: MAX_REGIONS + 1, ..RecognitionConfig::default() },
        RecognitionConfig { max_crop_pixels: MAX_CROP_PIXELS + 1, ..RecognitionConfig::default() },
        RecognitionConfig {
            max_tensor_elements: MAX_TENSOR_ELEMENTS + 1,
            ..RecognitionConfig::default()
        },
        RecognitionConfig {
            max_output_timesteps: MAX_OUTPUT_TIMESTEPS + 1,
            ..RecognitionConfig::default()
        },
        RecognitionConfig {
            max_decoded_bytes: MAX_DECODED_BYTES + 1,
            ..RecognitionConfig::default()
        },
    ] {
        assert!(
            PpOcrTextRecognizer::new(
                Arc::new(ShapeRuntime),
                include_bytes!("../../../../models/ppocrv6_tiny_dict.txt"),
                config,
                &context(),
            )
            .is_err()
        );
    }
}

#[test]
fn ctc_collapses_before_blank_and_ties_choose_lowest_index() {
    let execution = context();
    let (chars, _lease) = characters(&execution);
    let steps = [1_usize, 1, 0, 1, 2, 2, 0];
    let mut values = vec![0.0; steps.len() * CLASSES];
    for (time, &class) in steps.iter().enumerate() {
        values[time * CLASSES + class] = 0.75;
    }
    values[4 * CLASSES + 3] = 0.75;
    let output = Tensor { shape: vec![1, steps.len(), CLASSES], values };
    let batch = [(
        7,
        CropPlan { polygon: [(0.0, 0.0); 4], width: 1, height: 1, rotate: false, ratio: 1.0 },
    )];
    let mut reservation = context().reserve_memory(1024).unwrap();
    let mut total_bytes = 0;
    let decoded = decode_output(
        &[output],
        &batch,
        &chars,
        &RecognitionConfig::default(),
        &context(),
        &mut reservation,
        &mut total_bytes,
    )
    .unwrap();
    assert_eq!(decoded.items[0].source_index, 7);
    assert_eq!(decoded.items[0].text, format!("{}{}{}", chars[0], chars[0], chars[1]));
    assert_eq!(decoded.items[0].confidence.to_bits(), 0.75_f32.to_bits());
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
        include_bytes!("../../../../models/ppocrv6_tiny_dict.txt"),
        RecognitionConfig { max_batch_size: 2, ..RecognitionConfig::default() },
        &context(),
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
            width: 29,
            height: 9,
        },
        CropDescriptor {
            polygon: [(0.0, 20.0), (9.0, 20.0), (9.0, 29.0), (0.0, 29.0)],
            width: 9,
            height: 9,
        },
    ];
    let result =
        block_on(recognizer.recognize(image, &crops, Some("zh-Hant"), &context())).unwrap();
    assert_eq!(result.regions.iter().map(|item| item.source_index).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(result.language_hint.as_deref(), Some("zh-Hant"));
}

#[test]
fn crop_coordinates_are_raw_source_coordinates_even_with_exif_orientation() {
    let bytes = [0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255];
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
fn tall_crops_use_the_official_counterclockwise_rotation() {
    let crop = CropPlan { polygon: [(0.0, 0.0); 4], width: 2, height: 4, rotate: true, ratio: 2.0 };
    assert_eq!(rotated_to_crop(crop, 0.0, 0.0), (1.0, 0.0));
    assert_eq!(rotated_to_crop(crop, 3.0, 0.0), (1.0, 3.0));
    assert_eq!(rotated_to_crop(crop, 0.0, 1.0), (0.0, 0.0));
}

#[test]
fn malformed_outputs_and_limits_fail_closed() {
    let execution = context();
    let (chars, _lease) = characters(&execution);
    let batch = [(
        0,
        CropPlan { polygon: [(0.0, 0.0); 4], width: 1, height: 1, rotate: false, ratio: 1.0 },
    )];
    for output in [
        Tensor { shape: vec![1, 1], values: vec![0.0; CLASSES] },
        Tensor { shape: vec![1, 1, CLASSES - 1], values: vec![0.0; CLASSES - 1] },
        Tensor { shape: vec![1, 1, CLASSES], values: vec![f32::NAN; CLASSES] },
        Tensor { shape: vec![1, 1, CLASSES], values: vec![1.01; CLASSES] },
        Tensor { shape: vec![1, 2, CLASSES], values: vec![0.0; CLASSES] },
    ] {
        let mut reservation = context().reserve_memory(1024).unwrap();
        let mut total_bytes = 0;
        assert!(
            decode_output(
                &[output],
                &batch,
                &chars,
                &RecognitionConfig::default(),
                &context(),
                &mut reservation,
                &mut total_bytes,
            )
            .is_err()
        );
    }
    let mut reservation = context().reserve_memory(1024).unwrap();
    let mut total_bytes = 0;
    assert!(
        decode_output(
            &[],
            &batch,
            &chars,
            &RecognitionConfig::default(),
            &context(),
            &mut reservation,
            &mut total_bytes,
        )
        .is_err()
    );
}

#[test]
fn cancellation_and_low_memory_fail_without_partial_recognition() {
    let recognizer = PpOcrTextRecognizer::new(
        Arc::new(ShapeRuntime),
        include_bytes!("../../../../models/ppocrv6_tiny_dict.txt"),
        RecognitionConfig::default(),
        &context(),
    )
    .unwrap();
    let bytes = vec![128; 16 * 16 * 3];
    let image = PixelView {
        width: 16,
        height: 16,
        row_stride: 48,
        format: PixelFormat::Bgr8,
        orientation: ImageOrientation::Normal,
        bytes: &bytes,
    };
    let crop = CropDescriptor {
        polygon: [(0.0, 0.0), (15.0, 0.0), (15.0, 15.0), (0.0, 15.0)],
        width: 15,
        height: 15,
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let error =
        block_on(recognizer.recognize(image, std::slice::from_ref(&crop), None, &cancelled))
            .unwrap_err();
    assert_eq!(error.code().as_str(), "cancelled");

    let constrained = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() },
    );
    let error =
        block_on(recognizer.recognize(image, std::slice::from_ref(&crop), None, &constrained))
            .unwrap_err();
    assert_eq!(error.code().as_str(), "resourceLimit");
    assert_eq!(constrained.reserved_memory_bytes(), 0);
}

#[test]
fn character_and_result_memory_stays_charged_until_the_last_owner_drops() {
    let execution = context();
    let recognizer = PpOcrTextRecognizer::new(
        Arc::new(ShapeRuntime),
        include_bytes!("../../../../models/ppocrv6_tiny_dict.txt"),
        RecognitionConfig::default(),
        &execution,
    )
    .unwrap();
    let character_bytes = execution.reserved_memory_bytes();
    assert!(character_bytes > 0);
    let bytes = vec![128; 16 * 16 * 3];
    let image = PixelView {
        width: 16,
        height: 16,
        row_stride: 48,
        format: PixelFormat::Bgr8,
        orientation: ImageOrientation::Normal,
        bytes: &bytes,
    };
    let crop = CropDescriptor {
        polygon: [(0.0, 0.0), (15.0, 0.0), (15.0, 15.0), (0.0, 15.0)],
        width: 15,
        height: 15,
    };

    let first =
        block_on(recognizer.recognize(image, std::slice::from_ref(&crop), Some("en"), &execution))
            .unwrap();
    let one_result = execution.reserved_memory_bytes() - character_bytes;
    assert!(one_result > 0);
    let shared_clone = first.clone();
    assert_eq!(execution.reserved_memory_bytes(), character_bytes + one_result);
    let second =
        block_on(recognizer.recognize(image, std::slice::from_ref(&crop), Some("en"), &execution))
            .unwrap();
    assert_eq!(execution.reserved_memory_bytes(), character_bytes + one_result * 2);
    let boundary = execution.reserve_memory(execution.available_memory_bytes()).unwrap();
    assert_eq!(execution.available_memory_bytes(), 0);
    let error = execution.reserve_memory(1).unwrap_err();
    assert_eq!(error.code().as_str(), "resourceLimit");
    drop(boundary);
    assert_eq!(execution.reserved_memory_bytes(), character_bytes + one_result * 2);
    drop(first);
    assert_eq!(execution.reserved_memory_bytes(), character_bytes + one_result * 2);
    drop(shared_clone);
    assert_eq!(execution.reserved_memory_bytes(), character_bytes + one_result);
    drop(second);
    assert_eq!(execution.reserved_memory_bytes(), character_bytes);
    drop(recognizer);
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

struct PipelineRuntime;

impl TensorRuntime for PipelineRuntime {
    fn id(&self) -> &'static str {
        "test.detect-recognize-orientations"
    }

    fn run<'a>(
        &'a self,
        model_id: &'a str,
        inputs: &'a [Tensor],
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
        Box::pin(async move {
            match model_id {
                "pp-ocrv6-tiny-zh-en" => {
                    let height = inputs[0].shape[2];
                    let width = inputs[0].shape[3];
                    let mut values = vec![0.0; height * width];
                    for y in height / 3..height * 2 / 3 {
                        for x in width / 4..width * 3 / 4 {
                            values[y * width + x] = 0.95;
                        }
                    }
                    Ok(vec![Tensor { shape: vec![1, 1, height, width], values }])
                }
                MODEL_ID => {
                    let batch = inputs[0].shape[0];
                    let mut values = vec![0.0; batch * CLASSES];
                    for item in 0..batch {
                        values[item * CLASSES + 1] = 1.0;
                    }
                    Ok(vec![Tensor { shape: vec![batch, 1, CLASSES], values }])
                }
                _ => panic!("unexpected model {model_id}"),
            }
        })
    }
}

#[test]
fn all_exif_orientations_flow_from_detection_to_raw_source_recognition() {
    let runtime: Arc<dyn TensorRuntime> = Arc::new(PipelineRuntime);
    let detector = PpOcrTextDetector::new(
        Arc::clone(&runtime),
        DetectionConfig {
            max_source_pixels: 48 * 24,
            max_model_pixels: 1_500_000,
            max_contour_events: 10_000,
            max_contour_points: 10_000,
            max_score_pixels: 1_500_000,
            max_score_work: 100_000_000_000,
            max_offset_points: 1_000,
        },
    )
    .unwrap();
    let recognizer = PpOcrTextRecognizer::new(
        runtime,
        include_bytes!("../../../../models/ppocrv6_tiny_dict.txt"),
        RecognitionConfig::default(),
        &context(),
    )
    .unwrap();
    let bytes = (0..48 * 24).map(|index| (index % 251) as u8).collect::<Vec<_>>();
    for orientation in [
        ImageOrientation::Normal,
        ImageOrientation::MirrorHorizontal,
        ImageOrientation::Rotate180,
        ImageOrientation::MirrorVertical,
        ImageOrientation::MirrorHorizontalRotate270,
        ImageOrientation::Rotate90,
        ImageOrientation::MirrorHorizontalRotate90,
        ImageOrientation::Rotate270,
    ] {
        let image = PixelView {
            width: 48,
            height: 24,
            row_stride: 48,
            format: PixelFormat::Gray8,
            orientation,
            bytes: &bytes,
        };
        let detected = block_on(detector.detect_page(7, image, &context())).unwrap();
        assert_eq!(detected.result().regions.len(), 1, "orientation={orientation:?}");
        assert_eq!(detected.page(), 7);
        let result =
            block_on(recognizer.recognize_page(image, &detected, None, &context())).unwrap();
        assert_eq!(result.regions.len(), 1);
        assert_eq!(result.batch_identity.as_ref(), Some(&detected.identity));
        assert_eq!(result.recognizer_model, Some(crate::batch::RECOGNIZER_MODEL_ID));
        let character_context = context();
        let (characters, _lease) = characters(&character_context);
        assert_eq!(result.regions[0].text, characters[0]);
    }
}
