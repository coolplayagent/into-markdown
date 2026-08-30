use super::*;
use into_markdown_core::{
    Document, ExecutionOptions, OcrEvidence, OcrEvidenceStage, OcrEvidenceStep, OcrSourceRegion,
    ProvenanceKind, Rect, ResourceLimits, SourcePoint,
};

fn image_locator() -> SourceLocator {
    SourceLocator {
        page: Some(1),
        page_width: Some(1366.0),
        page_height: Some(109.0),
        bounds: Some(Rect { x: 10.0, y: 2.0, width: 1200.0, height: 100.0 }),
        ..SourceLocator::default()
    }
}

fn container_locator() -> SourceLocator {
    SourceLocator {
        slide: Some(4),
        part: Some("Pictures/source.png".into()),
        bounds: Some(Rect { x: 20.0, y: 311.811, width: 500.0, height: 40.0 }),
        byte_start: Some(7),
        byte_end: Some(12),
        ..SourceLocator::default()
    }
}

fn assert_image_local(source: &SourceLocator) {
    let image = image_locator();
    assert!(coordinate_frame(source, &image).is_none());
    let actual = remapped_ocr_locator(source, &image);
    assert_eq!(actual.page_width, image.page_width);
    assert_eq!(actual.page_height, image.page_height);
    assert_eq!(actual.bounds, image.bounds);
    assert_eq!(actual.rotation_degrees, image.rotation_degrees);
    assert_eq!(actual.slide, source.slide);
    assert_eq!(actual.part, source.part);
    assert_eq!(actual.page, Some(4));
    assert_eq!(actual.byte_start, None);
    assert_eq!(actual.byte_end, None);
}

#[test]
fn incomplete_canvas_never_mixes_container_points_with_image_pixels() {
    let mut source = container_locator();
    assert_image_local(&source);
    source.page_width = Some(800.0);
    assert_image_local(&source);
    source.page_width = None;
    source.page_height = Some(600.0);
    assert_image_local(&source);
}

#[test]
fn off_canvas_and_unknown_rotation_keep_image_local_evidence() {
    let mut source = container_locator();
    source.page_width = Some(800.0);
    source.page_height = Some(600.0);
    for bounds in [
        Rect { x: -2653.0, y: -8214.0, width: 100_000.0, height: 20_000.0 },
        Rect { x: -0.0001, y: 20.0, width: 100.0, height: 30.0 },
        Rect { x: 790.0, y: 20.0, width: 100.0, height: 30.0 },
        Rect { x: 20.0, y: 590.0, width: 100.0, height: 30.0 },
    ] {
        source.bounds = Some(bounds);
        assert_image_local(&source);
    }
    source.bounds = container_locator().bounds;
    for rotation in [90.0, 180.0, 0.01, f32::NAN] {
        source.rotation_degrees = Some(rotation);
        assert_image_local(&source);
    }
}

#[test]
fn complete_axis_aligned_canvas_maps_pixels_into_source_units() {
    let source = SourceLocator {
        page_width: Some(800.0),
        page_height: Some(600.0),
        bounds: Some(Rect { x: 72.0, y: 144.0, width: 72.0, height: 36.0 }),
        rotation_degrees: Some(0.0),
        ..container_locator()
    };
    let image = SourceLocator {
        page_width: Some(96.0),
        page_height: Some(48.0),
        bounds: Some(Rect { x: 8.0, y: 4.0, width: 80.0, height: 40.0 }),
        ..image_locator()
    };
    let actual = remapped_ocr_locator(&source, &image);
    assert_eq!(actual.page_width, Some(800.0));
    assert_eq!(actual.page_height, Some(600.0));
    assert_eq!(actual.bounds, Some(Rect { x: 78.0, y: 147.0, width: 60.0, height: 30.0 }));
    assert_eq!(actual.slide, source.slide);
    assert_eq!(actual.part, source.part);
}

#[test]
fn fallback_preserves_polygon_bytes_and_passes_the_unchanged_ir_validator() {
    let polygon = [
        SourcePoint { x: 10.0, y: 2.0 },
        SourcePoint { x: 1210.0, y: 2.0 },
        SourcePoint { x: 1210.0, y: 102.0 },
        SourcePoint { x: 10.0, y: 102.0 },
    ];
    let provenance = Provenance {
        kind: ProvenanceKind::LocalOcr,
        provider: "test.recognizer".into(),
        locator: image_locator(),
        confidence: Some(0.9),
    };
    let evidence = OcrEvidence {
        page: 1,
        regions: vec![OcrSourceRegion {
            source_index: 7,
            polygon,
            detection_confidence: 0.9,
            recognition_confidence: 0.9,
        }],
        chain: vec![
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
        ],
    };
    let node = BlockNode {
        id: NodeId("original".into()),
        provenance: provenance.clone(),
        block: Block::Paragraph(vec![Inline::OcrText {
            value: "retained words".into(),
            marks: Vec::new(),
            provenance: Box::new(provenance),
            evidence: Box::new(evidence),
        }]),
    };
    let source = Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "test.container".into(),
        locator: container_locator(),
        confidence: None,
    };
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let mapped = remap_ocr_node(node, NodeId("mapped".into()), &source, &context).unwrap();
    let Block::Paragraph(inlines) = &mapped.block else { panic!("paragraph") };
    let Inline::OcrText { value, provenance, evidence, .. } = &inlines[0] else {
        panic!("OCR text")
    };
    assert_eq!(value, "retained words");
    assert_eq!(evidence.page, 4);
    assert_eq!(evidence.regions[0].source_index, 7);
    assert_eq!(evidence.regions[0].polygon, polygon);
    assert_eq!(provenance.locator.page_width, Some(1366.0));
    assert_eq!(provenance.locator.page_height, Some(109.0));
    assert_eq!(provenance.locator.slide, Some(4));
    assert_eq!(provenance.locator.part, source.locator.part);
    Document { blocks: vec![mapped], ..Document::default() }.validate().unwrap();
    assert_eq!(context.reserved_memory_bytes(), 0);
}
