use super::*;
use into_markdown_core::{Inline, OcrPolicy};

#[test]
fn nfc_and_whitespace_equivalent_native_text_is_not_duplicated() {
    let bounds = Rect { x: 20.0, y: 20.0, width: 100.0, height: 16.0 };
    let native = "Cafe\u{301}"
        .chars()
        .map(|character| Inline::SourceText {
            value: character.to_string(),
            marks: vec![],
            provenance: Box::new(native_provenance(Some(bounds))),
        })
        .collect();
    let detection = detection(&[(polygon(20.0, 20.0, 100.0, 16.0), 0.99)]);
    let recognition = recognition(&[(0, "Ca fé", 0.98)]);
    let context = context();
    let output = merge_document(
        page_document(native),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context,
    )
    .unwrap();
    assert!(merged_text(&output.document).is_empty());
    assert_eq!(output.diagnostics[0].code, "ocr.nativeDuplicateSuppressed");
    assert_eq!(context.resource_usage().ocr_recognized_regions, 0);
    assert_eq!(context.resource_usage().ocr_recognized_chars, 0);
}

#[test]
fn overlapping_and_crossing_equivalent_boxes_keep_only_best_confidence() {
    let detection = detection(&[
        (polygon(20.0, 20.0, 100.0, 18.0), 0.92),
        ([(18.0, 22.0), (118.0, 18.0), (120.0, 36.0), (20.0, 40.0)], 0.98),
    ]);
    let recognition = recognition(&[(0, "duplicate", 0.97), (1, "duplicate", 0.98)]);
    let context = context();
    let output = merge_document(
        page_document(Vec::new()),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context,
    )
    .unwrap();
    assert_eq!(merged_text(&output.document), "duplicate");
    assert_eq!(
        output
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "ocr.duplicateSuppressed")
            .count(),
        1
    );
    assert_eq!(context.resource_usage().ocr_recognized_regions, 1);
    assert_eq!(context.resource_usage().ocr_recognized_chars, 9);
}

#[test]
fn overlapping_distinct_text_is_preserved() {
    let detection = detection(&[
        (polygon(20.0, 20.0, 100.0, 18.0), 0.98),
        (polygon(70.0, 20.0, 100.0, 18.0), 0.98),
    ]);
    let recognition = recognition(&[(0, "left", 0.98), (1, "right", 0.98)]);
    let output = merge_document(
        page_document(Vec::new()),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    assert_eq!(merged_text(&output.document), "leftright");
    assert!(output.diagnostics.is_empty());
}

#[test]
fn explicit_inline_page_overrides_node_page_for_native_dedup() {
    let bounds = Rect { x: 20.0, y: 20.0, width: 100.0, height: 16.0 };
    let inlines = "same"
        .chars()
        .map(|character| Inline::SourceText {
            value: character.to_string(),
            marks: vec![],
            provenance: Box::new(Provenance {
                locator: SourceLocator {
                    page: Some(2),
                    bounds: Some(bounds),
                    page_width: Some(600.0),
                    page_height: Some(800.0),
                    ..SourceLocator::default()
                },
                ..native_provenance(None)
            }),
        })
        .collect();
    let document = Document {
        blocks: vec![BlockNode {
            id: NodeId("page-conflict".into()),
            block: Block::Paragraph(inlines),
            provenance: native_provenance(Some(bounds)),
        }],
        ..Document::default()
    };
    let detected = detection(&[(polygon(20.0, 20.0, 100.0, 16.0), 0.99)]);
    let recognized = recognition(&[(0, "same", 0.99)]);
    let output = merge_document(
        document,
        &[input(&detected, &recognized)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    assert_eq!(merged_text(&output.document), "same");
    assert!(output.diagnostics.is_empty());
}
