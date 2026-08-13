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
    let output = merge_document(
        page_document(native),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    assert!(merged_text(&output.document).is_empty());
    assert_eq!(output.diagnostics[0].code, "ocr.nativeDuplicateSuppressed");
}

#[test]
fn overlapping_and_crossing_equivalent_boxes_keep_only_best_confidence() {
    let detection = detection(&[
        (polygon(20.0, 20.0, 100.0, 18.0), 0.92),
        ([(18.0, 22.0), (118.0, 18.0), (120.0, 36.0), (20.0, 40.0)], 0.98),
    ]);
    let recognition = recognition(&[(0, "duplicate", 0.97), (1, "duplicate", 0.98)]);
    let output = merge_document(
        page_document(Vec::new()),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
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
