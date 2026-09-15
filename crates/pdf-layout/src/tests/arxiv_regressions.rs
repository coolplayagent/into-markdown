use super::*;

fn lower_page_lines(values: &[&str]) -> Document {
    let mut input = Vec::new();
    for row in 0..8 {
        input.extend(source_text(
            "Ordinary body text",
            Rect { x: 30.0, y: 60.0 + row as f32 * 25.0, width: 170.0, height: 12.0 },
            12.0,
        ));
    }
    for (row, text) in values.iter().enumerate() {
        input.extend(source_text(
            text,
            Rect { x: 30.0, y: 660.0 + row as f32 * 23.0, width: 230.0, height: 8.0 },
            8.0,
        ));
    }
    document(input)
}

#[test]
fn decimal_table_rows_keep_their_full_numbers() {
    let output =
        rebuild(lower_page_lines(&["1.1 Instruction", "1.2 Task Prompt", "1.3 Reasoning"]));
    output.validate().unwrap();
    assert!(page_blocks(&output).iter().all(|n| !matches!(n.block, Block::Footnote { .. })));
    let json = output.to_json().unwrap();
    for word in ["Instruction", "Reasoning"] {
        // Source-addressed characters remain independently represented.
        let actual = page_blocks(&output).iter().map(|n| block_text(&n.block)).collect::<String>();
        assert!(actual.contains(word), "{json}");
    }
}

#[test]
fn ambiguous_same_page_footnotes_keep_both_texts_and_stable_relayout() {
    let output = rebuild(lower_page_lines(&["1 First definition", "1 Second definition"]));
    output.validate().unwrap();
    assert!(page_blocks(&output).iter().all(|n| !matches!(n.block, Block::Footnote { .. })));
    let text = page_blocks(&output).iter().map(|n| block_text(&n.block)).collect::<String>();
    assert!(text.contains("First definition") && text.contains("Second definition"));
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn zero_area_native_word_spaces_survive_geometric_reconstruction() {
    let mut input =
        source_text("Hello", Rect { x: 30.0, y: 60.0, width: 25.0, height: 12.0 }, 12.0);
    input.push(source(" ", Rect { x: 55.0, y: 60.0, width: 0.0, height: 0.0 }, 12.0));
    input.extend(source_text("world", Rect { x: 57.0, y: 60.0, width: 25.0, height: 12.0 }, 12.0));
    let output = rebuild(document(input));
    assert_eq!(block_text(&page_blocks(&output)[0].block), "Hello world");
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn dense_overlapping_glyphs_request_visual_recovery() {
    let input = document(
        (0..80)
            .map(|index| {
                source(
                    "x",
                    Rect { x: 30.0 + index as f32 * 2.0, y: 60.0, width: 10.0, height: 12.0 },
                    12.0,
                )
            })
            .collect(),
    );
    let error = reconstruct_document(input, &LayoutConfig::default(), &context()).err().unwrap();
    assert!(
        matches!(error, ConversionError::Malformed { part, .. } if part.as_deref() == Some("pdf-layout-visual"))
    );
}

#[test]
fn sentence_boundaries_keep_spaces_across_wrapped_lines() {
    let mut input =
        source_text("First sentence.", Rect { x: 30.0, y: 60.0, width: 150.0, height: 12.0 }, 12.0);
    input.extend(source_text(
        "Next sentence.",
        Rect { x: 30.0, y: 75.0, width: 150.0, height: 12.0 },
        12.0,
    ));
    let output = rebuild(document(input));
    assert_eq!(block_text(&page_blocks(&output)[0].block), "First sentence. Next sentence.");
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn unresolved_dense_numeric_rows_request_visual_recovery() {
    let mut input = Vec::new();
    for row in 0..32 {
        let y = 50.0 + row as f32 * 20.0;
        input.extend(source_text("Subject", Rect { x: 30.0, y, width: 42.0, height: 10.0 }, 10.0));
        input.extend(source_text(
            "56.46 68.90",
            Rect { x: 200.0 + (row % 3) as f32 * 28.0, y, width: 66.0, height: 10.0 },
            10.0,
        ));
    }
    let error = reconstruct_document(document(input), &LayoutConfig::default(), &context())
        .err()
        .expect("uncorroborated numeric grid needs its original visual alignment");
    assert!(matches!(error, ConversionError::Malformed { part, .. }
        if part.as_deref() == Some("pdf-layout-visual")));
}

#[test]
fn nearby_rows_do_not_move_the_clustering_baseline() {
    let mut input = Vec::new();
    for row in 0..8 {
        input.extend(source_text(
            &format!("Row {row} has its own baseline"),
            Rect { x: 30.0, y: 60.0 + row as f32 * 14.0, width: 200.0, height: 12.0 },
            12.0,
        ));
    }
    let output = rebuild(document(input));
    let actual = page_blocks(&output).iter().map(|n| block_text(&n.block)).collect::<String>();
    for row in 0..8 {
        assert!(actual.contains(&format!("Row {row} has its own baseline")), "{actual}");
    }
}

#[test]
fn text_tables_with_unresolved_columns_keep_a_visual_recovery_boundary() {
    let mut input = source_text(
        "Table 2. Toolkit comparison",
        Rect { x: 30.0, y: 40.0, width: 180.0, height: 10.0 },
        10.0,
    );
    for row in 0..8 {
        let y = 70.0 + row as f32 * 23.0;
        input.extend(source_text("Toolkit", Rect { x: 30.0, y, width: 50.0, height: 10.0 }, 10.0));
        input.extend(source_text(
            "Analysis utilities",
            Rect { x: 230.0 + (row % 3) as f32 * 35.0, y, width: 100.0, height: 10.0 },
            10.0,
        ));
    }
    for rotated in [false, true] {
        let mut selected = input.clone();
        let mut bounds = vec![
            Rect { x: 20.0, y: 62.0, width: 450.0, height: 0.5 },
            Rect { x: 20.0, y: 255.0, width: 450.0, height: 0.5 },
        ];
        let rotate = |r: &mut Rect| {
            *r = Rect { x: r.y, y: r.x, width: r.height, height: r.width };
        };
        if rotated {
            for inline in &mut selected {
                if let Inline::SourceText { provenance, .. } = inline {
                    rotate(provenance.locator.bounds.as_mut().unwrap());
                    provenance.locator.rotation_degrees = Some(90.0);
                }
            }
            for bound in &mut bounds {
                rotate(bound);
            }
        }
        let evidence = [PagePathEvidence { page: 1, bounds }];
        let error = reconstruct_document_with_path_evidence(
            document(selected),
            &LayoutConfig::default(),
            &evidence,
            &context(),
        )
        .err()
        .expect("unresolved cell relationships require visual preservation");
        assert!(matches!(error, ConversionError::Malformed { part, .. }
            if part.as_deref() == Some("pdf-layout-visual")));
    }
}

#[test]
fn narrow_columns_keep_wrapped_sentences_together() {
    let mut input = Vec::new();
    for (row, (left, right)) in [
        ("Left biography starts", "Right biography starts"),
        ("and continues here", "with separate research"),
        ("through its final line.", "through its final line."),
    ]
    .into_iter()
    .enumerate()
    {
        let y = 100.0 + row as f32 * 14.0;
        input.extend(source_text(left, Rect { x: 40.0, y, width: 250.0, height: 10.0 }, 10.0));
        input.extend(source_text(right, Rect { x: 307.0, y, width: 250.0, height: 10.0 }, 10.0));
    }
    let output = rebuild(document(input));
    let text = page_blocks(&output)
        .iter()
        .map(|node| block_text(&node.block))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("Left biography starts and continues here through its final line."),
        "{text}"
    );
    assert!(
        text.contains("Right biography starts with separate research through its final line."),
        "{text}"
    );
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn staggered_columns_wrapping_portraits_keep_biographies_separate() {
    let mut input =
        source_text("Author profiles", Rect { x: 40.0, y: 70.0, width: 350.0, height: 10.0 }, 12.0);
    for row in 0..8 {
        let y = 100.0 + row as f32 * 17.0;
        let (left_x, right_x, width) =
            if row < 5 { (140.0, 400.0, 150.0) } else { (40.0, 304.0, 250.0) };
        input.extend(source_text(
            &format!("Alice biography line {row}"),
            Rect { x: left_x, y, width, height: 10.0 },
            12.0,
        ));
        input.extend(source_text(
            &format!("Bob biography line {row}"),
            Rect { x: right_x, y: y + 12.5, width, height: 10.0 },
            12.0,
        ));
    }
    let mut original = document(input);
    let Block::Page { blocks, .. } = &mut original.blocks[0].block else { unreachable!() };
    for (index, x) in [40.0, 304.0].into_iter().enumerate() {
        blocks.push(BlockNode {
            id: NodeId(format!("portrait-{index}")),
            block: Block::Image { asset: AssetId(format!("portrait-{index}")), alt: None },
            provenance: provenance(
                1,
                Some(Rect { x, y: 100.0, width: 70.0, height: 80.0 }),
                12.0,
                0.0,
            ),
        });
    }
    let output = rebuild(original);
    let text = page_blocks(&output)
        .iter()
        .map(|node| block_text(&node.block))
        .collect::<Vec<_>>()
        .join(" ");
    let left_end = text.find("Alice biography line 7").expect("left last line");
    let right_start = text.find("Bob biography line 0").expect("right first line");
    assert!(left_end < right_start, "{text}");
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn centered_footer_preserves_columns_using_observed_glyph_height() {
    let mut input = Vec::new();
    for row in 0..6 {
        let y = 600.0 + row as f32 * 12.0;
        input.extend(source_text(
            &format!("Left column line {row}"),
            Rect { x: 40.0, y, width: 250.0, height: 9.0 },
            10.0,
        ));
        input.extend(source_text(
            &format!("Right column line {row}"),
            Rect { x: 307.0, y, width: 250.0, height: 9.0 },
            10.0,
        ));
    }
    input.extend(source_text("24", Rect { x: 295.0, y: 678.1, width: 10.0, height: 7.0 }, 10.0));
    let output = rebuild(document(input));
    let text = page_blocks(&output)
        .iter()
        .map(|node| block_text(&node.block))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.find("Left column line 5").unwrap() < text.find("Right column line 0").unwrap(),
        "{text}"
    );
    assert!(text.ends_with("24"), "{text}");
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn staggered_columns_keep_adjacent_glyph_rows_separate() {
    let atoms = super::staggered_glyphs::GLYPHS
        .iter()
        .enumerate()
        .map(|(index, &(x, y, width, height, row))| crate::model::Atom {
            inline: Inline::Text { value: row.to_string(), marks: vec![] },
            bounds: Rect { x, y, width, height },
            font_size: Some(9.5),
            orientation: 0,
            source_index: index,
            source_kind: crate::model::SourceKind::Native,
            space_after: false,
        })
        .collect();
    let execution = context();
    let mut budget = budget::LayoutBudget::preflight(
        &Document::default(),
        &[],
        &LayoutConfig::default(),
        &execution,
    )
    .unwrap();
    let lines = crate::lines::cluster(atoms, &mut budget).unwrap();
    let mut rows = std::collections::BTreeMap::new();
    for line in lines {
        let labels = line
            .atoms
            .iter()
            .filter(|atom| atom.bounds.x < 300.0)
            .map(|atom| crate::lines::inline_text(&atom.inline))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(labels.len() <= 1, "neighboring left-column rows merged: {labels:?}");
        for label in labels {
            *rows.entry(label.to_owned()).or_insert(0) += 1;
        }
    }
    assert_eq!(rows.values().copied().collect::<Vec<_>>(), vec![1, 1, 1, 1]);
}

#[test]
fn ambiguous_native_rows_request_visual_recovery() {
    let input = super::mixed_rows_glyphs::GLYPHS
        .iter()
        .map(|&(x, y, width, height)| source("x", Rect { x, y, width, height }, 10.0))
        .collect();
    let error = reconstruct_document(document(input), &LayoutConfig::default(), &context())
        .err()
        .expect("native rows with colliding horizontal glyph positions require recovery");
    assert!(matches!(error, ConversionError::Malformed { part, .. }
        if part.as_deref() == Some("pdf-layout-visual")));
}

#[test]
fn vertical_margin_label_preserves_horizontal_columns() {
    let mut input = source_text(
        "Full width article heading",
        Rect { x: 40.0, y: 60.0, width: 517.0, height: 15.0 },
        18.0,
    );
    for row in 0..8 {
        let y = 150.0 + row as f32 * 16.0;
        for (label, x) in [("Left", 40.0), ("Right", 307.0)] {
            input.extend(source_text(
                &format!("{label} column line {row}"),
                Rect { x, y, width: 250.0, height: 9.0 },
                10.0,
            ));
        }
    }
    let mut label = source_text(
        "arXiv marginal identifier",
        Rect { x: 15.0, y: 100.0, width: 180.0, height: 9.0 },
        10.0,
    );
    for atom in &mut label {
        if let Inline::SourceText { provenance, .. } = atom {
            let r = provenance.locator.bounds.as_mut().unwrap();
            *r = Rect { x: 15.0, y: r.x + 100.0, width: r.height, height: r.width };
            provenance.locator.rotation_degrees = Some(90.0);
        }
    }
    input.extend(label);
    let output = rebuild(document(input));
    let text = page_blocks(&output)
        .iter()
        .map(|node| block_text(&node.block))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.find("Left column line 7").unwrap() < text.find("Right column line 0").unwrap(),
        "{text}"
    );
    assert!(text.contains("arXiv marginal identifier"), "{text}");
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn overhanging_native_line_preserves_established_column_flow() {
    let mut input = Vec::new();
    for (label, x) in [("Left", 40.0), ("Right", 307.0)] {
        for row in 0..8 {
            input.extend(source_text(
                &format!("{label} column line {row}"),
                Rect {
                    x,
                    y: 150.0 + row as f32 * 16.0 + if row >= 4 { 20.0 } else { 0.0 },
                    width: if label == "Left" && row == 4 { 266.0 } else { 250.0 },
                    height: 9.0,
                },
                10.0,
            ));
        }
    }
    let output = rebuild(document(input));
    let text = page_blocks(&output)
        .iter()
        .map(|node| block_text(&node.block))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.find("Left column line 7").unwrap() < text.find("Right column line 0").unwrap(),
        "{text}"
    );
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn partially_rebuilt_table_preserves_unresolved_numeric_rows_visually() {
    let mut input = source_text(
        "Table 4. Comparison results",
        Rect { x: 30.0, y: 30.0, width: 180.0, height: 10.0 },
        10.0,
    );
    for row in 0..7 {
        let y = 60.0 + row as f32 * 25.0;
        input.extend(source_text("Subject", Rect { x: 30.0, y, width: 42.0, height: 10.0 }, 10.0));
        input.extend(source_text(
            "56.46 68.90",
            Rect {
                x: 200.0 + if row < 3 { 0.0 } else { (row % 3) as f32 * 28.0 },
                y,
                width: 66.0,
                height: 10.0,
            },
            10.0,
        ));
    }
    let error = reconstruct_document(document(input), &LayoutConfig::default(), &context())
        .err()
        .expect("remaining numeric rows retain their label-value associations through recovery");
    assert!(matches!(error, ConversionError::Malformed { part, .. }
        if part.as_deref() == Some("pdf-layout-visual")));
}

#[test]
fn stacked_numeric_uncertainty_preserves_visual_notation() {
    let input = [
        source_text("Distance", Rect { x: 30.0, y: 100.0, width: 50.0, height: 10.0 }, 10.0),
        source_text("40", Rect { x: 100.0, y: 100.0, width: 12.0, height: 10.0 }, 10.0),
        source_text("8", Rect { x: 115.0, y: 96.0, width: 4.0, height: 5.0 }, 7.0),
        source_text("14", Rect { x: 115.0, y: 103.0, width: 8.0, height: 5.0 }, 7.0),
    ]
    .concat();
    let error = reconstruct_document(document(input), &LayoutConfig::default(), &context())
        .err()
        .expect("stacked numeric bounds must retain their two-dimensional relationship");
    assert!(matches!(error, ConversionError::Malformed { part, .. }
        if part.as_deref() == Some("pdf-layout-visual")));
}

#[test]
fn native_origins_keep_descenders_with_their_source_rows() {
    let atoms = super::native_baselines::GLYPHS
        .iter()
        .enumerate()
        .map(|(index, &(x, y, width, height, baseline, label))| {
            let bounds = Rect { x, y, width, height };
            let mut inline = source(&label.to_string(), bounds, 9.5);
            if let Inline::SourceText { provenance, .. } = &mut inline {
                provenance.locator.text_baseline = Some(baseline);
            }
            crate::model::Atom {
                inline,
                bounds,
                font_size: Some(9.5),
                orientation: 0,
                source_index: index,
                source_kind: crate::model::SourceKind::Native,
                space_after: false,
            }
        })
        .collect();
    let execution = context();
    let mut budget = budget::LayoutBudget::preflight(
        &Document::default(),
        &[],
        &LayoutConfig::default(),
        &execution,
    )
    .unwrap();
    let lines = crate::lines::cluster(atoms, &mut budget).unwrap();
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines.iter().map(|line| line.atoms.len()).sum::<usize>(),
        super::native_baselines::GLYPHS.len()
    );
    for line in lines {
        let labels = line
            .atoms
            .iter()
            .map(|a| crate::lines::inline_text(&a.inline))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(labels.len(), 1, "descenders separated from their native baseline: {labels:?}");
    }
}

#[test]
fn source_baselines_preserve_stacked_numeric_relationships() {
    let mut input = [
        source_text("Distance", Rect { x: 30.0, y: 100.0, width: 50.0, height: 10.0 }, 10.0),
        source_text("40", Rect { x: 100.0, y: 100.0, width: 12.0, height: 10.0 }, 10.0),
        source_text("8", Rect { x: 115.0, y: 96.0, width: 4.0, height: 5.0 }, 7.0),
        source_text("14", Rect { x: 115.0, y: 103.0, width: 8.0, height: 5.0 }, 7.0),
    ]
    .concat();
    for inline in &mut input {
        if let Inline::SourceText { provenance, .. } = inline {
            let r = provenance.locator.bounds.unwrap();
            provenance.locator.text_baseline = Some(r.y + r.height);
        }
    }
    let error = reconstruct_document(document(input), &LayoutConfig::default(), &context())
        .err()
        .expect("numeric bounds survive as a visual with native origins enabled");
    assert!(matches!(error, ConversionError::Malformed { part, .. }
        if part.as_deref() == Some("pdf-layout-visual")));
}

#[test]
fn native_baselines_retain_superscript_semantics() {
    let mut input = [
        source_text("10", Rect { x: 30.0, y: 100.0, width: 12.0, height: 10.0 }, 10.0),
        source_text("4", Rect { x: 43.0, y: 101.0, width: 4.0, height: 5.0 }, 7.0),
    ]
    .concat();
    for inline in &mut input {
        if let Inline::SourceText { provenance, .. } = inline {
            let r = provenance.locator.bounds.unwrap();
            provenance.locator.text_baseline = Some(r.y + r.height);
        }
    }
    let output = rebuild(document(input));
    let Block::Paragraph(inlines) = &page_blocks(&output)[0].block else {
        panic!("numeric expression")
    };
    assert!(inlines.iter().any(|inline| matches!(inline,
        Inline::SourceText { value, marks, .. } if value == "4" && marks.contains(&into_markdown_core::InlineMark::Superscript))));
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn overlapping_block_regions_preserve_paragraph_start_order() {
    let mut input =
        source_text("Page heading", Rect { x: 40.0, y: 30.0, width: 517.0, height: 8.0 }, 10.0);
    for (label, x, count) in [("Left", 40.0, 20), ("Right", 307.0, 8)] {
        for row in 0..count {
            input.extend(source_text(
                &format!("{label} paragraph line {row}"),
                Rect { x, y: 42.0 + row as f32 * 12.0, width: 250.0, height: 9.0 },
                10.0,
            ));
        }
    }
    let output = rebuild(document(input));
    let text =
        page_blocks(&output).iter().map(|n| block_text(&n.block)).collect::<Vec<_>>().join(" ");
    assert!(
        text.find("Left paragraph line 19").unwrap() < text.find("Right paragraph line 0").unwrap(),
        "{text}"
    );
}

#[test]
fn narrow_typeset_gutters_follow_font_scale_and_keep_full_columns() {
    let mut input = Vec::new();
    for row in 0..8 {
        for (label, x, width) in [("Left", 40.0, 264.0), ("Right", 311.0, 249.0)] {
            input.extend(source_text(
                &format!("{label} paragraph line {row}"),
                Rect { x, y: 100.0 + row as f32 * 12.0, width, height: 6.0 },
                8.0,
            ));
        }
    }
    let output = rebuild(document(input));
    let text =
        page_blocks(&output).iter().map(|n| block_text(&n.block)).collect::<Vec<_>>().join(" ");
    assert!(
        text.find("Left paragraph line 7").unwrap() < text.find("Right paragraph line 0").unwrap(),
        "{text}"
    );
    assert_eq!(output, rebuild(output.clone()));
}

#[test]
fn ordinary_numeric_rows_with_native_origins_remain_structured() {
    let mut input = Vec::new();
    for row in 0..10 {
        let y = 100.0 + row as f32 * 12.0;
        input.extend(source_text("Value", Rect { x: 30.0, y, width: 35.0, height: 7.0 }, 10.0));
        input.extend(source_text("12.34", Rect { x: 120.0, y, width: 35.0, height: 7.0 }, 10.0));
    }
    for inline in &mut input {
        if let Inline::SourceText { provenance, .. } = inline {
            let r = provenance.locator.bounds.unwrap();
            provenance.locator.text_baseline = Some(r.y + r.height);
        }
    }
    let output = rebuild(document(input));
    let tables =
        page_blocks(&output)
            .iter()
            .filter_map(|n| {
                if let Block::Table { rows, .. } = &n.block { Some(rows.len()) } else { None }
            })
            .sum::<usize>();
    assert_eq!(tables, 10);
}

#[test]
fn drop_cap_spanning_two_native_baselines_keeps_source_visual() {
    let mut input = [
        source_text("L", Rect { x: 30.0, y: 100.0, width: 10.0, height: 18.0 }, 18.0),
        source_text(
            "ANGUAGE begins here",
            Rect { x: 41.0, y: 100.0, width: 180.0, height: 8.0 },
            10.0,
        ),
        source_text("Next body line", Rect { x: 41.0, y: 110.0, width: 150.0, height: 8.0 }, 10.0),
    ]
    .concat();
    for inline in &mut input {
        if let Inline::SourceText { provenance, .. } = inline {
            let r = provenance.locator.bounds.unwrap();
            provenance.locator.text_baseline = Some(r.y + r.height);
        }
    }
    let error = reconstruct_document(document(input), &LayoutConfig::default(), &context())
        .err()
        .expect("drop cap must not attach to the following line");
    assert!(matches!(error, ConversionError::Malformed { part, .. }
        if part.as_deref() == Some("pdf-layout-visual")));
}
