use super::super::materialize_nodes;
use super::*;
use into_markdown_core::{
    Block, ConversionOptions, ExecutionContext, ExecutionOptions, Inline, OcrEvidenceStage,
    OcrEvidenceStep, OcrRegion, OcrResult,
};

fn chain() -> Vec<OcrEvidenceStep> {
    vec![
        OcrEvidenceStep {
            stage: OcrEvidenceStage::Detection,
            provider: "test.detector".into(),
            model: Some("detector".into()),
        },
        OcrEvidenceStep {
            stage: OcrEvidenceStage::Recognition,
            provider: "test.recognizer".into(),
            model: Some("recognizer".into()),
        },
        OcrEvidenceStep {
            stage: OcrEvidenceStage::Merge,
            provider: "test.merge".into(),
            model: None,
        },
    ]
}

fn region(text: &str, polygon: [(f32, f32); 4]) -> OcrRegion {
    OcrRegion { text: text.into(), polygon, confidence: 0.95 }
}

const VALID: [(f32, f32); 4] = [(1.0, 1.0), (10.0, 1.0), (10.0, 10.0), (1.0, 10.0)];
// Captured from ddb216474d903742140efad6, slide 5, 137x91 image,
// region 11: empty text, recognition confidence 0, detector confidence .42518458.
// Twice-area is 2, but the first three vertices are collinear.
const COLLINEAR_NOISE: [(f32, f32); 4] = [(19.0, 74.0), (19.0, 75.0), (19.0, 76.0), (18.0, 75.0)];

fn captured_noise() -> OcrRegion {
    OcrRegion { text: String::new(), polygon: COLLINEAR_NOISE, confidence: 0.0 }
}

#[test]
fn captured_empty_collinear_noise_is_diagnosed_before_publication_shape_validation() {
    let options = ConversionOptions::default();
    let execution = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let chain = chain();
    let materialize = MaterializeContext {
        chain: &chain,
        page: 1,
        width: 137,
        height: 91,
        options: &options,
        engine_id: "test.recognizer",
        execution: &execution,
    };
    for regions in [
        vec![region("first", VALID), captured_noise(), region("last", VALID)],
        vec![captured_noise()],
    ] {
        let result = OcrResult { regions, provider: "test.recognizer".into() };
        let count = result.regions.len();
        let (document, diagnostics, accepted, _) =
            materialize_nodes(result, vec![0.96; count], &materialize).unwrap();
        document.validate().unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "ocr.lowConfidence");
        assert_eq!(accepted, u64::try_from(count - 1).unwrap());
        let text: Vec<_> = document
            .blocks
            .iter()
            .map(|node| {
                let Block::Paragraph(inlines) = &node.block else { panic!("paragraph") };
                let Inline::OcrText { value, evidence, .. } = &inlines[0] else {
                    panic!("OCR text")
                };
                (value.as_str(), evidence.regions[0].source_index)
            })
            .collect();
        if count == 3 {
            assert_eq!(text, [("first", 0), ("last", 2)]);
        } else {
            assert!(text.is_empty());
        }
    }
    let usable_text_with_invalid_shape = OcrResult {
        regions: vec![region("must not silently disappear", COLLINEAR_NOISE)],
        provider: "test.recognizer".into(),
    };
    assert!(matches!(
        materialize_nodes(usable_text_with_invalid_shape, vec![0.96], &materialize),
        Err(ConversionError::Ocr { .. })
    ));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn nonfinite_or_out_of_image_coordinates_remain_hard_errors() {
    let options = ConversionOptions::default();
    let execution = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let chain = chain();
    let materialize = MaterializeContext {
        chain: &chain,
        page: 1,
        width: 32,
        height: 32,
        options: &options,
        engine_id: "test.recognizer",
        execution: &execution,
    };
    for x in [f32::NAN, f32::INFINITY, -1.0, 33.0] {
        let mut polygon = VALID;
        polygon[0].0 = x;
        assert!(matches!(
            validate_region_bounds(&polygon, &materialize),
            Err(ConversionError::Ocr { .. })
        ));
    }
}
