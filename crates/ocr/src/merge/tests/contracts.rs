use super::*;
use into_markdown_core::{Inline, NodeId, OcrEvidenceStage, OcrPolicy};

#[test]
fn stable_source_indexes_form_structured_page_ir_and_chain() {
    let detection = detection(&[
        (polygon(20.0, 20.0, 80.0, 16.0), 0.98),
        (polygon(104.0, 20.0, 100.0, 16.0), 0.97),
        (polygon(20.0, 60.0, 140.0, 16.0), 0.96),
    ]);
    let recognition =
        recognition(&[(2, "second line", 0.94), (0, "hello", 0.95), (1, "world", 0.93)]);
    let config = MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() };
    let auto_context = context();
    let output = merge_document(
        page_document(Vec::new()),
        &[input(&detection, &recognition)],
        &config,
        &auto_context,
    )
    .unwrap();
    assert_eq!(merged_text(&output.document), "hello world\nsecond line");
    let Block::Page { blocks, .. } = &output.document.blocks[0].block else { panic!("page") };
    let Block::Paragraph(inlines) = &blocks[0].block else { panic!("paragraph") };
    let Inline::OcrText { evidence, provenance, .. } = &inlines[0] else { panic!("OCR") };
    assert_eq!(
        evidence.regions.iter().map(|region| region.source_index).collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(
        evidence.chain.iter().map(|step| step.stage).collect::<Vec<_>>(),
        [OcrEvidenceStage::Detection, OcrEvidenceStage::Recognition, OcrEvidenceStage::Merge]
    );
    assert_eq!(evidence.chain[0].model.as_deref(), Some(crate::batch::DETECTOR_MODEL_ID));
    assert_eq!(evidence.chain[1].model.as_deref(), Some(crate::batch::RECOGNIZER_MODEL_ID));
    assert_eq!(provenance.locator.page, Some(1));
    assert!(provenance.locator.bounds.is_some());
    output.document.validate().unwrap();
}

#[test]
fn policy_off_is_a_true_no_op_and_auto_respects_native_text() {
    let native = Inline::SourceText {
        value: "n".into(),
        marks: vec![],
        provenance: Box::new(native_provenance(Some(Rect {
            x: 20.0,
            y: 20.0,
            width: 10.0,
            height: 16.0,
        }))),
    };
    let document = page_document(vec![native; 8]);
    let detected = detection(&[(polygon(20.0, 20.0, 80.0, 16.0), 0.99)]);
    let recognized = recognition(&[(0, "ignored", 0.99)]);
    let auto_context = context();
    let auto = merge_document(
        document.clone(),
        &[input(&detected, &recognized)],
        &MergeConfig::default(),
        &auto_context,
    )
    .unwrap();
    assert_eq!(auto.document, document);

    let oversized_detection = detection(&[
        (polygon(20.0, 20.0, 80.0, 16.0), 0.99),
        (polygon(20.0, 60.0, 80.0, 16.0), 0.99),
    ]);
    let malformed_recognition = recognition(&[]);
    let ignored = merge_document(
        document.clone(),
        &[input(&oversized_detection, &malformed_recognition)],
        &MergeConfig {
            limits: MergeLimits { max_regions: 1, ..MergeLimits::default() },
            ..MergeConfig::default()
        },
        &context(),
    )
    .unwrap();
    assert_eq!(ignored.document, document);

    let off = merge_document(
        document.clone(),
        &[],
        &MergeConfig { policy: OcrPolicy::Off, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    assert_eq!(off.document, document);
}

#[test]
fn auto_does_not_use_native_text_from_a_different_page() {
    let bounds = Some(Rect { x: 20.0, y: 20.0, width: 180.0, height: 16.0 });
    let native = "native text on page one"
        .chars()
        .map(|character| Inline::SourceText {
            value: character.to_string(),
            marks: vec![],
            provenance: Box::new(native_provenance(bounds)),
        })
        .collect();
    let document = page_document(native);
    let detection = detection(&[(polygon(20.0, 20.0, 80.0, 16.0), 0.99)]);
    let recognition = recognition(&[(0, "page two", 0.99)]);
    let page_two = input_for_page(2, &detection, &recognition);
    let output =
        merge_document(document, &[page_two], &MergeConfig::default(), &context()).unwrap();
    assert_eq!(merged_text(&output.document), "page two");
    assert!(
        output
            .document
            .blocks
            .iter()
            .any(|node| matches!(&node.block, Block::Page { number: 2, .. }))
    );
}

#[test]
fn flat_ir_auto_uses_only_explicit_matching_page_locators() {
    let matching = |page| Inline::SourceText {
        value: "n".into(),
        marks: vec![],
        provenance: Box::new(Provenance {
            locator: SourceLocator {
                page,
                bounds: Some(Rect { x: 20.0, y: 20.0, width: 10.0, height: 10.0 }),
                page_width: Some(600.0),
                page_height: Some(800.0),
                ..SourceLocator::default()
            },
            ..native_provenance(None)
        }),
    };
    let flat_node = |values| BlockNode {
        id: NodeId("mixed-page-wrapper".into()),
        block: Block::Paragraph(values),
        provenance: Provenance {
            locator: SourceLocator {
                page: None,
                bounds: Some(Rect { x: 20.0, y: 20.0, width: 10.0, height: 10.0 }),
                page_width: Some(600.0),
                page_height: Some(800.0),
                ..SourceLocator::default()
            },
            ..native_provenance(None)
        },
    };
    let mut unrelated = vec![matching(None); 8];
    unrelated.extend(vec![matching(Some(2)); 8]);
    let flat = Document { blocks: vec![flat_node(unrelated)], ..Document::default() };
    let detection = detection(&[(polygon(20.0, 20.0, 80.0, 16.0), 0.99)]);
    let recognition = recognition(&[(0, "page one", 0.99)]);
    let output = merge_document(
        flat,
        &[input(&detection, &recognition)],
        &MergeConfig::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(merged_text(&output.document), "page one");

    let matching_page =
        Document { blocks: vec![flat_node(vec![matching(Some(1)); 8])], ..Document::default() };
    let suppressed = merge_document(
        matching_page.clone(),
        &[input(&detection, &recognition)],
        &MergeConfig::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(suppressed.document, matching_page);

    let mut conflicting_node = flat_node(vec![matching(Some(2)); 8]);
    conflicting_node.provenance.locator.page = Some(1);
    let conflicting = Document { blocks: vec![conflicting_node], ..Document::default() };
    let retained = merge_document(
        conflicting,
        &[input(&detection, &recognition)],
        &MergeConfig::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(merged_text(&retained.document), "page one");
}

#[test]
fn flat_legacy_ir_merges_native_and_ocr_in_page_geometry_order() {
    let native = |id: &str, y: Option<f32>| BlockNode {
        id: NodeId(id.into()),
        block: Block::Paragraph(vec![Inline::Text { value: id.into(), marks: vec![] }]),
        provenance: Provenance {
            locator: SourceLocator {
                page: Some(1),
                bounds: y.map(|y| Rect { x: 20.0, y, width: 80.0, height: 16.0 }),
                page_width: Some(600.0),
                page_height: Some(800.0),
                ..SourceLocator::default()
            },
            ..native_provenance(None)
        },
    };
    let document = Document {
        blocks: vec![
            native("middle", Some(100.0)),
            native("end", Some(300.0)),
            native("unknown", None),
        ],
        ..Document::default()
    };
    let detection = detection(&[
        (polygon(20.0, 20.0, 80.0, 16.0), 0.98),
        (polygon(20.0, 200.0, 80.0, 16.0), 0.98),
    ]);
    let recognition = recognition(&[(0, "top", 0.98), (1, "lower", 0.98)]);
    let output = merge_document(
        document,
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    assert!(!output.document.blocks.iter().any(|node| matches!(node.block, Block::Page { .. })));
    assert_eq!(
        output.document.blocks.iter().map(|node| node.id.0.as_str()).collect::<Vec<_>>(),
        ["ocr-page-1-paragraph-1", "middle", "ocr-page-1-paragraph-2", "end", "unknown",]
    );
}

#[test]
fn confidence_filter_emits_stable_page_diagnostic() {
    let detection = detection(&[(polygon(10.0, 10.0, 60.0, 15.0), 0.95)]);
    let recognition = recognition(&[(0, "uncertain", 0.69)]);
    let context = context();
    let output = merge_document(
        page_document(Vec::new()),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context,
    )
    .unwrap();
    assert_eq!(output.diagnostics.len(), 1);
    assert_eq!(output.diagnostics[0].code, "ocr.lowConfidence");
    assert_eq!(output.diagnostics[0].locator.as_ref().and_then(|value| value.page), Some(1));
    assert!(merged_text(&output.document).is_empty());
}

#[test]
fn invalid_or_duplicate_recognition_indexes_fail_stably() {
    let detection =
        detection(&[(polygon(0.0, 0.0, 20.0, 10.0), 0.9), (polygon(30.0, 0.0, 20.0, 10.0), 0.9)]);
    let recognition = recognition(&[(0, "a", 0.9), (0, "b", 0.9)]);
    let error = merge_document(
        Document::default(),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap_err();
    assert_eq!(error.code().as_str(), "ocr");
    assert!(error.to_string().contains("invalidRecognitionSourceIndex"));
}

#[test]
fn polygon_outside_declared_page_fails_before_publication() {
    let detection = detection(&[(polygon(590.0, 20.0, 20.0, 10.0), 0.9)]);
    let recognition = recognition(&[(0, "outside", 0.9)]);
    let error = merge_document(
        page_document(Vec::new()),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap_err();
    assert_eq!(error.code().as_str(), "ocr");
    assert!(error.to_string().contains("detectionPolygonOutsidePage"));
}

#[test]
fn generated_page_id_does_not_collide_with_existing_document_nodes() {
    let document = Document {
        blocks: vec![BlockNode {
            id: NodeId("ocr-page-1".into()),
            block: Block::Page { number: 2, blocks: Vec::new() },
            provenance: native_provenance(None),
        }],
        ..Document::default()
    };
    let detection = detection(&[(polygon(20.0, 20.0, 80.0, 16.0), 0.98)]);
    let recognition = recognition(&[(0, "text", 0.98)]);
    let output = merge_document(
        document,
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    assert_eq!(output.document.blocks[1].id.0, "ocr-page-1-1");
    output.document.validate().unwrap();
}

#[test]
fn batch_identity_rejects_page_region_and_model_mismatches() {
    let detection = detection(&[(polygon(20.0, 20.0, 80.0, 16.0), 0.98)]);
    let recognition = recognition(&[(0, "bound", 0.98)]);
    let valid = input(&detection, &recognition);
    assert_eq!(valid.page(), 1);

    let page_two = PageDetection::from_result(
        2,
        600.0,
        800.0,
        crate::batch::DETECTOR_MODEL_ID,
        detection.clone(),
    )
    .unwrap();
    let page_one = PageDetection::from_result(
        1,
        600.0,
        800.0,
        crate::batch::DETECTOR_MODEL_ID,
        detection.clone(),
    )
    .unwrap();
    let wrong_page =
        crate::BoundRecognition::new(recognition.clone(), page_two.identity.clone(), &context())
            .unwrap();
    assert!(OcrPageInput::new(page_one, wrong_page).is_err());

    let detected = PageDetection::from_result(
        1,
        600.0,
        800.0,
        crate::batch::DETECTOR_MODEL_ID,
        detection.clone(),
    )
    .unwrap();
    let mut wrong_model =
        crate::BoundRecognition::new(recognition.clone(), detected.identity.clone(), &context())
            .unwrap();
    wrong_model.tamper_model("wrong-model");
    assert!(OcrPageInput::new(detected, wrong_model).is_err());

    let mut mutated = detection.clone();
    let bound = PageDetection::from_result(
        1,
        600.0,
        800.0,
        crate::batch::DETECTOR_MODEL_ID,
        detection.clone(),
    )
    .unwrap();
    mutated.regions[0].crop.width += 1;
    let tampered = PageDetection { result: mutated, identity: bound.identity };
    let recognized =
        crate::BoundRecognition::new(recognition.clone(), tampered.identity.clone(), &context())
            .unwrap();
    assert!(OcrPageInput::new(tampered, recognized).is_err());

    let mut changed_provider = detection.clone();
    let bound = PageDetection::from_result(
        1,
        600.0,
        800.0,
        crate::batch::DETECTOR_MODEL_ID,
        changed_provider.clone(),
    )
    .unwrap();
    changed_provider.provider = "substituted.detector".into();
    let tampered = PageDetection { result: changed_provider, identity: bound.identity };
    assert!(tampered.identity.validate(&tampered.result).is_err());

    let detection = PageDetection::from_result(
        1,
        600.0,
        800.0,
        crate::batch::DETECTOR_MODEL_ID,
        detection.clone(),
    )
    .unwrap();
    let mut changed_text =
        crate::BoundRecognition::new(recognition.clone(), detection.identity.clone(), &context())
            .unwrap();
    changed_text.tamper_result(|result| {
        Arc::make_mut(&mut result.regions)[0].text = "substituted".into();
    });
    let tampered_input = OcrPageInput::new(detection.clone(), changed_text).unwrap();
    assert!(
        merge_document(
            Document::default(),
            &[tampered_input],
            &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
            &context(),
        )
        .is_err()
    );

    let mut changed_index =
        crate::BoundRecognition::new(recognition.clone(), detection.identity.clone(), &context())
            .unwrap();
    changed_index.tamper_result(|result| {
        Arc::make_mut(&mut result.regions)[0].source_index = 1;
    });
    assert!(changed_index.validate_payload(&context()).is_err());

    let mut changed_confidence =
        crate::BoundRecognition::new(recognition.clone(), detection.identity.clone(), &context())
            .unwrap();
    changed_confidence.tamper_result(|result| {
        Arc::make_mut(&mut result.regions)[0].confidence = 0.5;
    });
    assert!(changed_confidence.validate_payload(&context()).is_err());

    let mut changed_provider =
        crate::BoundRecognition::new(recognition.clone(), detection.identity.clone(), &context())
            .unwrap();
    changed_provider.tamper_result(|result| result.provider = "substituted.recognizer".into());
    assert!(changed_provider.validate_payload(&context()).is_err());

    let mut changed_language =
        crate::BoundRecognition::new(recognition, detection.identity.clone(), &context()).unwrap();
    changed_language.tamper_result(|result| result.language_hint = Some("zh-Hant".into()));
    assert!(changed_language.validate_payload(&context()).is_err());
}

#[test]
fn native_and_ocr_blocks_share_stable_page_reading_order() {
    fn native(id: &str, y: Option<f32>) -> BlockNode {
        BlockNode {
            id: NodeId(id.into()),
            block: Block::Paragraph(vec![Inline::Text { value: id.into(), marks: vec![] }]),
            provenance: native_provenance(y.map(|y| Rect {
                x: 20.0,
                y,
                width: 80.0,
                height: 16.0,
            })),
        }
    }
    let document = Document {
        blocks: vec![BlockNode {
            id: NodeId("page-1".into()),
            block: Block::Page {
                number: 1,
                blocks: vec![
                    native("middle", Some(100.0)),
                    native("end", Some(300.0)),
                    native("unknown", None),
                ],
            },
            provenance: native_provenance(None),
        }],
        ..Document::default()
    };
    let detection = detection(&[
        (polygon(20.0, 20.0, 80.0, 16.0), 0.98),
        (polygon(20.0, 200.0, 80.0, 16.0), 0.98),
    ]);
    let recognition = recognition(&[(0, "top", 0.98), (1, "lower", 0.98)]);
    let output = merge_document(
        document,
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    let Block::Page { blocks, .. } = &output.document.blocks[0].block else { panic!("page") };
    let ids = blocks.iter().map(|block| block.id.0.as_str()).collect::<Vec<_>>();
    assert!(ids[0].starts_with("ocr-page-1-paragraph"));
    assert_eq!(ids[1], "middle");
    assert!(ids[2].starts_with("ocr-page-1-paragraph"));
    assert_eq!(ids[3..], ["end", "unknown"]);
}
