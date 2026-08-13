use super::*;
use into_markdown_core::{CellRef, OcrPolicy, SourceLocator, TimeRange};

#[test]
fn existing_exact_source_locator_struct_literal_remains_source_compatible() {
    let locator = SourceLocator {
        byte_start: Some(1),
        byte_end: Some(2),
        page: Some(1),
        slide: None,
        sheet: Some("Sheet".into()),
        cell: Some(CellRef { row: 0, column: 0 }),
        bounds: Some(Rect { x: 1.0, y: 2.0, width: 3.0, height: 4.0 }),
        character_index: Some(0),
        font_name: Some("Font".into()),
        font_size: Some(12.0),
        rotation_degrees: Some(0.0),
        page_width: Some(600.0),
        page_height: Some(800.0),
        time: Some(TimeRange { start_ms: 1, end_ms: 2 }),
        part: Some("part.xml".into()),
    };
    assert_eq!(locator.page, Some(1));
}

#[test]
fn merged_document_round_trips_through_schema_one_json() {
    let detection = detection(&[(polygon(10.0, 20.0, 100.0, 20.0), 0.97)]);
    let recognition = recognition(&[(0, "wire text", 0.96)]);
    let output = merge_document(
        Document::default(),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    let json = output.document.to_json().unwrap();
    assert!(json.contains("\"schemaVersion\":1"));
    assert!(json.contains("\"type\":\"text\""));
    assert!(json.contains("\"ocrEvidence\":"));
    assert!(json.contains("\"polygon\":"));
    assert!(json.contains("\"stage\":\"detection\""));
    assert_eq!(Document::from_json(&json).unwrap(), output.document);
}

#[test]
fn legacy_documents_emit_identical_json_when_no_ocr_evidence_exists() {
    let document = Document::default();
    assert_eq!(
        document.to_json().unwrap(),
        r#"{"schemaVersion":1,"metadata":{"title":null,"authors":[],"properties":{}},"blocks":[]}"#
    );
}
