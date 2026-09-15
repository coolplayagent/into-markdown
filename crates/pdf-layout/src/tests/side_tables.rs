use super::*;

#[test]
fn score_column_beside_long_prose_requires_preserved_cell_associations() {
    let mut input = Vec::new();
    for row in 0..3 {
        let y = 100.0 + row as f32 * 14.0;
        input.extend(source_text(
            "Adjacent prose continues beside a compact table whose scores belong to their own labels",
            Rect { x: 30.0, y, width: 260.0, height: 10.0 },
            10.0,
        ));
        input.extend(source_text(
            "Model label",
            Rect { x: 310.0, y, width: 120.0 + row as f32 * 10.0, height: 10.0 },
            10.0,
        ));
        input.extend(source_text(
            "6.58 ± 0.05",
            Rect { x: 454.0, y, width: 60.0, height: 10.0 },
            10.0,
        ));
    }
    input.extend(source_text(
        "The paragraph continues next to Table 4: Model scores",
        Rect { x: 30.0, y: 150.0, width: 450.0, height: 10.0 },
        10.0,
    ));
    let result = reconstruct_document(document(input), &LayoutConfig::default(), &context());
    assert!(matches!(result, Err(ConversionError::Malformed { part: Some(part), .. })
        if part == "pdf-layout-visual"));
}
