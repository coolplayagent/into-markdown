use super::*;

#[test]
fn actual_short_column_edge_geometry_keeps_columns_contiguous() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/pdf-layout-column-edge-geometry.json"
    ))
    .unwrap();
    let width = fixture["width"].as_f64().unwrap() as f32;
    let height = fixture["height"].as_f64().unwrap() as f32;
    let mut input = Vec::new();
    for row in fixture["glyphs"].as_array().unwrap() {
        let value = |i: usize| row[i].as_f64().unwrap() as f32;
        let mut atom = source(
            row[6].as_str().unwrap(),
            Rect { x: value(0), y: value(1), width: value(2), height: value(3) },
            value(5),
        );
        if let Inline::SourceText { provenance, .. } = &mut atom {
            provenance.locator.text_baseline = Some(value(4));
            provenance.locator.page_width = Some(width);
            provenance.locator.page_height = Some(height);
        }
        input.push(atom);
    }
    let mut doc = document(input);
    doc.blocks[0].provenance.locator.page_width = Some(width);
    doc.blocks[0].provenance.locator.page_height = Some(height);
    let output = rebuild(doc);
    let text = page_blocks(&output).iter().map(|n| block_text(&n.block)).collect::<String>();
    assert!(text.rfind('A').unwrap() < text.find('B').unwrap(), "{text}");
}
