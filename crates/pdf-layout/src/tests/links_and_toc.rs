use super::*;

fn spatial(doc: &mut Document, bounds: Rect, target: &str) {
    let Block::Page { blocks, .. } = &mut doc.blocks[0].block else { panic!("page") };
    let mut source = provenance(1, Some(bounds), 12.0, 0.0);
    source.locator.part = Some(LINK_SOURCE_PART.into());
    blocks.push(BlockNode {
        id: NodeId(format!("test-link-{}", blocks.len())),
        block: Block::Paragraph(vec![Inline::Link {
            target: target.into(),
            content: vec![Inline::Text { value: target.into(), marks: vec![] }],
        }]),
        provenance: source,
    });
}

fn labels(nodes: &[BlockNode]) -> Vec<(String, String)> {
    fn inlines(values: &[Inline], out: &mut Vec<(String, String)>) {
        for inline in values {
            if let Inline::Link { target, content } = inline {
                out.push((target.clone(), inline_text(content)));
            }
        }
    }
    let mut found = Vec::new();
    for node in nodes {
        match &node.block {
            Block::Paragraph(v) | Block::Heading { content: v, .. } => inlines(v, &mut found),
            Block::List { items, .. } => {
                for item in items {
                    found.extend(labels(&item.blocks));
                }
            }
            Block::Page { blocks, .. } => found.extend(labels(blocks)),
            Block::Table { rows, .. } => {
                for cell in rows.iter().flat_map(|r| &r.cells) {
                    found.extend(labels(&cell.blocks));
                }
            }
            _ => {}
        }
    }
    found
}

#[test]
fn body_links_use_source_labels_and_relayout_preserves_provenance() {
    let rect = Rect { x: 40.0, y: 80.0, width: 340.0, height: 12.0 };
    let mut chars = source_text("Read the product guide for details", rect, 12.0);
    for value in &mut chars[9..16] {
        let Inline::SourceText { marks, .. } = value else { panic!("source") };
        marks.push(into_markdown_core::InlineMark::Bold);
    }
    let mut input = document(chars);
    spatial(&mut input, Rect { x: 130.0, width: 130.0, ..rect }, "https://example.com/guide");
    let actual = rebuild(input);
    assert_eq!(
        labels(&actual.blocks),
        vec![("https://example.com/guide".into(), "product guide".into())]
    );
    assert_eq!(page_blocks(&actual).len(), 1);
    assert_eq!(rebuild(actual.clone()), actual);
    let Block::Paragraph(content) = &page_blocks(&actual)[0].block else { panic!("paragraph") };
    let linked = content
        .iter()
        .find_map(|i| if let Inline::Link { content, .. } = i { Some(content) } else { None })
        .unwrap();
    assert!(linked.iter().all(|i| matches!(i, Inline::SourceText { .. })));
    assert!(linked[..7].iter().all(|i| matches!(i, Inline::SourceText { marks, .. }
        if marks == &[into_markdown_core::InlineMark::Bold])));
}

#[test]
fn dedup_is_position_scoped_and_conflicting_targets_fall_back() {
    let rect = Rect { x: 40.0, y: 80.0, width: 50.0, height: 12.0 };
    let other = Rect { y: 120.0, ..rect };
    let mut chars = source_text("Guide", rect, 12.0);
    chars.extend(source_text("Guide", other, 12.0));
    let mut input = document(chars);
    for bounds in [rect, rect, other] {
        spatial(&mut input, bounds, "https://example.com");
    }
    let actual = rebuild(input.clone());
    assert_eq!(labels(&actual.blocks), vec![("https://example.com".into(), "Guide".into()); 2]);
    spatial(&mut input, rect, "https://different.example");
    let actual = rebuild(input);
    let found = labels(&actual.blocks);
    assert!(found.contains(&("https://example.com".into(), "Guide".into())));
    assert!(found.contains(&("https://example.com".into(), "https://example.com".into())));
    assert!(
        found.contains(&("https://different.example".into(), "https://different.example".into()))
    );
}

#[test]
fn unmatched_links_and_plain_existing_links_are_retained() {
    let mut input =
        document(source_text("Text", Rect { x: 40.0, y: 80.0, width: 40.0, height: 12.0 }, 12.0));
    spatial(&mut input, Rect { x: 200.0, y: 200.0, width: 80.0, height: 12.0 }, "#pdf-page-2");
    assert_eq!(labels(&rebuild(input).blocks), vec![("#pdf-page-2".into(), "#pdf-page-2".into())]);
}

fn directory(rows: &[(&str, &str, f32, f32)], leaders: bool) -> Document {
    let mut chars = Vec::new();
    for (label, page, x, y) in rows {
        chars.extend(source_text(
            label,
            Rect { x: *x, y: *y, width: label.chars().count() as f32 * 6.0, height: 12.0 },
            12.0,
        ));
        if leaders {
            chars.extend(source_text(
                ". . . . .",
                Rect { x: 300.0, y: *y, width: 80.0, height: 12.0 },
                12.0,
            ));
        }
        chars.extend(source_text(
            page,
            Rect {
                x: 420.0 - page.len() as f32 * 6.0,
                y: *y,
                width: page.len() as f32 * 6.0,
                height: 12.0,
            },
            12.0,
        ));
    }
    let mut doc = document(chars);
    for (index, (label, _, x, y)) in rows.iter().enumerate() {
        spatial(
            &mut doc,
            Rect { x: *x, y: *y, width: label.chars().count() as f32 * 6.0, height: 12.0 },
            &format!("#pdf-page-{}", index + 15),
        );
    }
    doc
}

fn entries(nodes: &[BlockNode]) -> Vec<(usize, String)> {
    fn walk(nodes: &[BlockNode], depth: usize, out: &mut Vec<(usize, String)>) {
        for n in nodes {
            if let Block::List { items, .. } = &n.block {
                for item in items {
                    let text = item
                        .blocks
                        .iter()
                        .filter_map(|n| match &n.block {
                            Block::Paragraph(values) => Some(
                                values
                                    .iter()
                                    .map(|v| match v {
                                        Inline::Link { content, .. } => inline_text(content),
                                        _ => inline_text(std::slice::from_ref(v)),
                                    })
                                    .collect::<String>(),
                            ),
                            _ => None,
                        })
                        .collect::<String>();
                    out.push((depth, text));
                    walk(&item.blocks, depth + 1, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, 0, &mut out);
    out
}

#[test]
fn directory_rows_keep_hierarchy_leaders_and_printed_page_labels() {
    for leaders in [false, true] {
        let input = directory(
            &[
                ("1 Introduction", "i", 40.0, 80.0),
                ("1.1 Background", "1", 58.0, 100.0),
                ("1.1.1 Details", "2", 76.0, 120.0),
                ("2 方法", "12", 40.0, 150.0),
            ],
            leaders,
        );
        let actual = rebuild(input);
        let rows = entries(page_blocks(&actual));
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), [0, 1, 2, 0]);
        assert_eq!(rows.len(), 4);
        for (row, page) in rows.iter().zip(["i", "1", "2", "12"]) {
            assert!(row.1.ends_with(page), "{rows:?}");
        }
        assert_eq!(labels(&actual.blocks)[0], ("#pdf-page-15".into(), "1 Introduction".into()));
        assert_eq!(actual, rebuild(actual.clone()));
        assert!(!page_blocks(&actual).iter().any(|n| matches!(n.block, Block::Table { .. })));
    }
}

#[test]
fn directory_page_and_whole_row_link_ranges_remain_distinct() {
    let mut input = directory(
        &[
            ("1 Introduction", "1", 40.0, 80.0),
            ("2 Methods", "5", 40.0, 100.0),
            ("3 Results", "8", 40.0, 120.0),
        ],
        true,
    );
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    blocks.truncate(1);
    spatial(&mut input, Rect { x: 414.0, y: 80.0, width: 6.0, height: 12.0 }, "#pdf-page-15");
    spatial(&mut input, Rect { x: 40.0, y: 100.0, width: 380.0, height: 12.0 }, "#pdf-page-19");
    spatial(&mut input, Rect { x: 40.0, y: 120.0, width: 54.0, height: 12.0 }, "#pdf-page-22");
    spatial(&mut input, Rect { x: 414.0, y: 120.0, width: 6.0, height: 12.0 }, "#pdf-page-22");
    let actual = rebuild(input);
    let found = labels(&actual.blocks);
    assert_eq!(found[0], ("#pdf-page-15".into(), "1".into()));
    assert!(found[1].1.starts_with("2 Methods") && found[1].1.ends_with('5'));
    assert_eq!(
        found[2..],
        [("#pdf-page-22".into(), "3 Results".into()), ("#pdf-page-22".into(), "8".into())]
    );
    assert_eq!(entries(page_blocks(&actual)).len(), 3);
}

#[test]
fn continued_directory_page_keeps_entries_after_a_shallower_heading() {
    let input = directory(
        &[
            ("2.4.1 Continuation", "19", 76.0, 80.0),
            ("2.4.2 More", "20", 76.0, 100.0),
            ("2.5 Following", "21", 58.0, 120.0),
            ("3 Next chapter", "22", 40.0, 150.0),
            ("3.1 Child", "23", 58.0, 170.0),
        ],
        true,
    );
    let actual = rebuild(input);
    let rows = entries(page_blocks(&actual));
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), [0, 0, 0, 0, 1]);
    assert_eq!(labels(&actual.blocks).len(), 5);
    assert_eq!(actual, rebuild(actual.clone()));
}

#[test]
fn ordinary_numbered_paragraphs_and_numeric_tables_are_not_directories() {
    let mut input = directory(
        &[
            ("1 Introduction", "2024", 40.0, 80.0),
            ("2 Methods", "2025", 40.0, 100.0),
            ("3 Results", "2026", 40.0, 120.0),
        ],
        false,
    );
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    blocks.truncate(1);
    let actual = rebuild(input);
    assert!(
        page_blocks(&actual)
            .iter()
            .all(|n| n.provenance.locator.part.as_deref() != Some("pdf/toc"))
    );
}

#[test]
fn bibliography_with_linked_titles_and_years_keeps_reference_structure() {
    let mut input = directory(
        &[("[1] Author. Paper", "2020", 40.0, 80.0), ("[2] Another. Study", "2021", 40.0, 100.0)],
        false,
    );
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    blocks.truncate(1);
    for y in [80.0, 100.0] {
        spatial(
            &mut input,
            Rect { x: 40.0, y, width: 120.0, height: 12.0 },
            "https://example.com/paper",
        );
    }
    let actual = rebuild(input);
    assert!(
        page_blocks(&actual)
            .iter()
            .all(|n| n.provenance.locator.part.as_deref() != Some("pdf/toc"))
    );
    assert_eq!(labels(&actual.blocks).len(), 2);
}

#[test]
fn wrapped_directory_title_keeps_one_entry() {
    let mut input = directory(
        &[
            ("continued title", "1", 40.0, 100.0),
            ("2 Methods", "2", 40.0, 120.0),
            ("3 Results", "3", 40.0, 140.0),
        ],
        true,
    );
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    let Block::Paragraph(chars) = &mut blocks[0].block else { panic!("text") };
    chars.extend(source_text(
        "1 A long heading",
        Rect { x: 40.0, y: 84.0, width: 96.0, height: 12.0 },
        12.0,
    ));
    spatial(&mut input, Rect { x: 40.0, y: 84.0, width: 96.0, height: 12.0 }, "#pdf-page-15");
    let actual = rebuild(input);
    let rows = entries(page_blocks(&actual));
    assert_eq!(rows.len(), 3);
    assert!(rows[0].1.contains("1 A long heading continued title"), "{rows:?}");
}

#[test]
fn a_three_line_nested_title_keeps_its_number_and_hierarchy() {
    let mut input =
        directory(&[("1 Chapter", "1", 40.0, 60.0), ("last part", "2", 58.0, 120.0)], true);
    for (label, y) in [("1.1 A long heading", 88.0), ("middle part", 104.0)] {
        let rect = Rect { x: 58.0, y, width: label.len() as f32 * 6.0, height: 12.0 };
        let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
        let Block::Paragraph(chars) = &mut blocks[0].block else { panic!("text") };
        chars.extend(source_text(label, rect, 12.0));
        spatial(&mut input, rect, "#pdf-page-16");
    }
    let actual = rebuild(input);
    let rows = entries(page_blocks(&actual));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].0, 1);
    assert!(rows[1].1.starts_with("1.1 A long heading middle part last part"), "{rows:?}");
}

#[test]
fn link_comparison_and_memory_failures_release_all_reservations() {
    let input =
        directory(&[("1 Introduction", "1", 40.0, 80.0), ("2 Methods", "2", 40.0, 100.0)], true);
    for limits in
        [ResourceLimits { max_memory_bytes: 1, ..Default::default() }, ResourceLimits::default()]
    {
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let config =
            LayoutConfig { limits: LayoutLimits { max_comparisons: 1, ..Default::default() } };
        assert!(reconstruct_document(input.clone(), &config, &context).is_err());
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn link_association_observes_cancellation_and_timeout_during_comparisons() {
    for timed in [false, true] {
        let cancel = CancellationToken::new();
        let execution = ExecutionContext::new(
            ExecutionOptions {
                cancellation: cancel.clone(),
                timeout: timed.then_some(Duration::from_millis(100)),
                ..Default::default()
            },
            ResourceLimits::default(),
        );
        let rect = Rect { x: 40.0, y: 80.0, width: 500.0, height: 12.0 };
        let mut input = document(source_text(&"a".repeat(100), rect, 12.0));
        for _ in 0..20 {
            spatial(&mut input, rect, "https://example.com");
        }
        let mut budget =
            budget::LayoutBudget::preflight(&input, &[], &LayoutConfig::default(), &execution)
                .unwrap();
        let Block::Page { mut blocks, .. } = input.blocks.remove(0).block else { panic!("page") };
        let mut links =
            crate::links::Links::extract(1, 600.0, 800.0, &mut blocks, &mut budget).unwrap();
        let content = collect::page_content(1, 600.0, 800.0, blocks, &mut budget).unwrap();
        let lines = lines::cluster(content.atoms, &mut budget).unwrap();
        budget::set_checkpoint_hook(Some(Box::new(move || {
            if timed {
                std::thread::sleep(Duration::from_millis(120));
            } else {
                cancel.cancel();
            }
        })));
        let result = links.associate(&lines, &mut budget);
        budget::set_checkpoint_hook(None);
        if timed {
            assert!(matches!(result, Err(ConversionError::Timeout)));
        } else {
            assert!(matches!(result, Err(ConversionError::Cancelled)));
        }
        drop(budget);
        assert_eq!(execution.reserved_memory_bytes(), 0);
    }
}

#[test]
fn multi_column_directory_keeps_each_page_number_with_its_title() {
    let left = directory(
        &[
            ("1 Alpha", "1", 40.0, 80.0),
            ("2 Beta", "2", 40.0, 100.0),
            ("3 Gamma", "3", 40.0, 120.0),
        ],
        true,
    );
    let mut right = directory(
        &[
            ("4 Delta", "4", 40.0, 80.0),
            ("5 Epsilon", "5", 40.0, 100.0),
            ("6 Zeta", "6", 40.0, 120.0),
        ],
        true,
    );
    fn shrink_shift(node: &mut BlockNode, shift: f32) {
        if let Some(r) = &mut node.provenance.locator.bounds {
            r.x = r.x * 0.5 + shift;
            r.width *= 0.5;
        }
        if let Block::Paragraph(values) = &mut node.block {
            for v in values {
                if let Inline::SourceText { provenance, .. } = v {
                    let r = provenance.locator.bounds.as_mut().unwrap();
                    r.x = r.x * 0.5 + shift;
                    r.width *= 0.5;
                }
            }
        }
    }
    let mut input = left;
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    for node in blocks.iter_mut() {
        shrink_shift(node, 0.0);
    }
    let Block::Page { blocks: other, .. } = &mut right.blocks[0].block else { panic!("page") };
    for node in other.iter_mut() {
        shrink_shift(node, 280.0);
        node.id.0.push_str("-right");
    }
    blocks.append(other);
    let actual = rebuild(input);
    let rows = entries(page_blocks(&actual));
    assert_eq!(rows.len(), 6, "{rows:?}");
    for (row, letter) in rows.iter().zip(["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta"]) {
        assert!(row.1.contains(letter), "{rows:?}");
    }
}

#[test]
fn rotated_directory_keeps_entries_and_source_link_ranges() {
    let mut input = directory(
        &[
            ("1 Alpha", "i", 40.0, 80.0),
            ("1.1 Beta", "1", 58.0, 100.0),
            ("2 Gamma", "2", 40.0, 120.0),
        ],
        true,
    );
    let rotate = |r: &mut Rect| {
        *r = Rect { x: 600.0 - r.y - r.height, y: r.x, width: r.height, height: r.width };
    };
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    for node in blocks {
        if let Some(r) = &mut node.provenance.locator.bounds {
            rotate(r);
        }
        if let Block::Paragraph(values) = &mut node.block {
            for v in values {
                if let Inline::SourceText { provenance, .. } = v {
                    rotate(provenance.locator.bounds.as_mut().unwrap());
                    provenance.locator.rotation_degrees = Some(90.0);
                }
            }
        }
    }
    let actual = rebuild(input);
    assert_eq!(entries(page_blocks(&actual)).len(), 3);
    assert_eq!(labels(&actual.blocks)[1], ("#pdf-page-16".into(), "1.1 Beta".into()));
}

#[test]
fn directory_with_only_geometry_and_heading_keeps_roman_page_labels() {
    let mut input = directory(
        &[
            ("Preface", "iii", 40.0, 80.0),
            ("Introduction", "v", 40.0, 100.0),
            ("Methods", "12", 40.0, 120.0),
        ],
        false,
    );
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    blocks.truncate(1);
    let Block::Paragraph(values) = &mut blocks[0].block else { panic!("text") };
    values.extend(source_text("目录", Rect { x: 40.0, y: 40.0, width: 30.0, height: 18.0 }, 18.0));
    let actual = rebuild(input);
    assert_eq!(entries(page_blocks(&actual)).len(), 3);
    assert!(labels(&actual.blocks).is_empty());
}

#[test]
fn ocr_relayout_keeps_directory_entries_and_avoids_duplicate_body_links() {
    let mut input = directory(&[("1 Alpha", "1", 40.0, 80.0), ("2 Beta", "2", 40.0, 100.0)], true);
    let first = rebuild(input.clone());
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    blocks.push(BlockNode {
        id: NodeId("ocr-body".into()),
        block: Block::Paragraph(vec![ocr(
            "Supplement",
            Rect { x: 40.0, y: 200.0, width: 100.0, height: 12.0 },
        )]),
        provenance: provenance(1, None, 12.0, 0.0),
    });
    let expected = rebuild(input);
    let mut merged = first;
    let Block::Page { blocks, .. } = &mut merged.blocks[0].block else { panic!("page") };
    blocks.push(BlockNode {
        id: NodeId("ocr-body".into()),
        block: Block::Paragraph(vec![ocr(
            "Supplement",
            Rect { x: 40.0, y: 200.0, width: 100.0, height: 12.0 },
        )]),
        provenance: provenance(1, None, 12.0, 0.0),
    });
    let actual = rebuild(merged);
    assert_eq!(entries(page_blocks(&actual)), entries(page_blocks(&expected)));
    assert_eq!(labels(&actual.blocks), labels(&expected.blocks));
    assert_eq!(actual, rebuild(actual.clone()));
}

#[test]
fn overlapping_annotation_and_wrapped_web_rectangles_emit_each_source_once() {
    let first = Rect { x: 40.0, y: 80.0, width: 100.0, height: 12.0 };
    let second = Rect { y: 100.0, ..first };
    let whole = Rect { height: 32.0, ..first };
    for rectangles in [[whole, first, second], [first, second, whole]] {
        let mut chars = source_text("first part", first, 12.0);
        chars.extend(source_text("final part", second, 12.0));
        let mut input = document(chars);
        for rect in rectangles {
            spatial(&mut input, rect, "https://example.com/wrapped");
        }
        let actual = rebuild(input);
        assert_eq!(
            labels(&actual.blocks),
            [("https://example.com/wrapped".into(), "first part final part".into())]
        );
    }
}

#[test]
fn a_long_appendix_title_keeps_its_nearby_page_label() {
    let label = format!("C {}", "x".repeat(58));
    let input = directory(
        &[
            ("A First appendix", "40", 40.0, 80.0),
            ("B Second appendix", "45", 40.0, 100.0),
            (&label, "48", 40.0, 120.0),
        ],
        false,
    );
    let actual = rebuild(input);
    assert_eq!(entries(page_blocks(&actual)).len(), 3);
    assert_eq!(labels(&actual.blocks)[2].1, label);
}

#[test]
fn directory_page_containers_and_same_target_entries_survive_relayout() {
    let mut input =
        directory(&[("1 First", "i", 40.0, 80.0), ("1.1 Next", "1", 58.0, 100.0)], true);
    let mut continued =
        directory(&[("1.2 Continued", "2", 58.0, 80.0), ("2 Chapter", "3", 40.0, 100.0)], true);
    let page = &mut continued.blocks[0];
    page.id = NodeId("pdf-page-2".into());
    page.provenance.locator.page = Some(2);
    let Block::Page { number, blocks } = &mut page.block else { panic!("page") };
    *number = 2;
    for node in blocks {
        node.id.0.push_str("-page2");
        node.provenance.locator.page = Some(2);
        if let Block::Paragraph(values) = &mut node.block {
            for value in values {
                if let Inline::SourceText { provenance, .. } = value {
                    provenance.locator.page = Some(2);
                }
            }
        }
    }
    input.blocks.extend(continued.blocks);
    let actual = rebuild(input);
    assert_eq!(actual.blocks.len(), 2);
    for page in &actual.blocks {
        let Block::Page { number, blocks } = &page.block else { panic!("page") };
        assert_eq!(entries(blocks).len(), 2);
        assert_eq!(page.provenance.locator.page, Some(*number));
    }
    assert_eq!(actual, rebuild(actual.clone()));
}
