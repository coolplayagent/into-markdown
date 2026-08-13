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
    assert_eq!(evidence.chain[0].model.as_deref(), Some("detector-model-sha256"));
    assert_eq!(evidence.chain[1].model.as_deref(), Some("recognizer-model-sha256"));
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

    let malformed = OcrPageInput { page: 0, ..input(&detected, &recognized) };
    let off = merge_document(
        document.clone(),
        &[malformed],
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
    let page_two = OcrPageInput { page: 2, ..input(&detection, &recognition) };
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
            block: Block::Paragraph(Vec::new()),
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
