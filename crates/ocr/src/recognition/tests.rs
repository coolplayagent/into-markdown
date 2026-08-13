use super::authority::{authority, load_characters};
use super::ctc::decode_output;
use super::preprocess::{CropPlan, raw_bgr};
use super::*;
use crate::PixelFormat;
use futures::executor::block_on;
use into_markdown_core::{ExecutionOptions, ResourceLimits, Tensor};

fn context() -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
}

fn characters() -> Vec<String> {
    load_characters(include_bytes!("../../../../models/ppocrv6_tiny_dict.txt")).unwrap()
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
        include_bytes!("../../../../models/ppocrv6_tiny_dict.txt"),
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
