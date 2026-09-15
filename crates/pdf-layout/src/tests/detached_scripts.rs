use super::*;

fn expression_with_script_text(text: &str, script_font: f32, script_y: f32) -> Document {
    let mut input = source_text(
        "P(k) = P(k) + g(k)",
        Rect { x: 30.0, y: 100.0, width: 150.0, height: 10.0 },
        10.0,
    );
    input.extend(source_text(
        text,
        Rect { x: 60.0, y: script_y, width: 70.0, height: 7.0 },
        script_font,
    ));
    for inline in &mut input {
        if let Inline::SourceText { provenance, .. } = inline {
            let r = provenance.locator.bounds.unwrap();
            provenance.locator.text_baseline = Some(r.y + r.height);
        }
    }
    document(input)
}

fn expression_with_script(script_font: f32, script_y: f32) -> Document {
    expression_with_script_text("ζζζ", script_font, script_y)
}

#[test]
fn detached_long_model_subscripts_preserve_the_complete_label() {
    let error = reconstruct_document(
        expression_with_script_text("large-instruct", 7.0, 108.0),
        &LayoutConfig::default(),
        &context(),
    )
    .err()
    .expect("long detached model labels require visual recovery");
    assert!(matches!(error, ConversionError::Malformed { part: Some(part), .. }
        if part == "pdf-layout-visual"));
}

#[test]
fn detached_small_scripts_preserve_the_source_expression_visually() {
    let error = reconstruct_document(
        expression_with_script(7.0, 108.0),
        &LayoutConfig::default(),
        &context(),
    )
    .err()
    .expect("detached scripts require visual recovery");
    assert!(matches!(error, ConversionError::Malformed { part: Some(part), .. }
        if part == "pdf-layout-visual"));
}

#[test]
fn separated_small_lines_remain_independent_text() {
    let output = rebuild(expression_with_script(7.0, 140.0));
    output.validate().unwrap();
}

#[test]
fn overlapping_superscript_markers_stay_before_the_affiliation() {
    let mut input = Vec::new();
    for (text, x, width, font, baseline) in [
        ("∗", 307.94, 2.84, 6.97, 426.0),
        ("∗", 312.02, 2.84, 6.97, 426.0),
        ("M", 311.80, 9.33, 9.96, 429.62),
        ("icrosoft", 320.90, 32.0, 9.96, 429.62),
    ] {
        for mut atom in source_text(text, Rect { x, y: baseline - font, width, height: font }, font)
        {
            if let Inline::SourceText { provenance, .. } = &mut atom {
                provenance.locator.text_baseline = Some(baseline);
            }
            input.push(atom);
        }
    }
    let output = rebuild(document(input));
    let text = page_blocks(&output).iter().map(|n| block_text(&n.block)).collect::<String>();
    assert!(text.contains("∗∗Microsoft"), "{text}");
}
