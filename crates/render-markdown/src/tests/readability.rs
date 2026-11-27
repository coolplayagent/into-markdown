use super::*;

#[test]
fn markdown_prose_keeps_bare_ampersands_and_protects_literal_entities() {
    let text = "R&D & QA; &amp; &#38; &#x26; &copy;";
    let markdown = output(&document(vec![paragraph("p", text)]));
    assert!(markdown.starts_with("R&D & QA; "));
    let parsed: String = Parser::new(&markdown)
        .filter_map(|event| match event {
            Event::Text(value) => Some(value.into_string()),
            _ => None,
        })
        .collect();
    assert_eq!(parsed, text);
}

#[test]
fn markdown_header_cells_apply_bold_once_and_preserve_other_marks() {
    let row = TableRow {
        cells: vec![
            ("a", vec![marked("Heading", &[InlineMark::Bold])]),
            (
                "b",
                vec![
                    marked("Other", &[InlineMark::Bold]),
                    Inline::LineBreak,
                    marked("detail", &[]),
                ],
            ),
            ("c", vec![marked("Nested", &[InlineMark::Bold, InlineMark::Italic])]),
        ]
        .into_iter()
        .map(|(id, inlines)| Cell {
            header: true,
            row_span: 1,
            column_span: 1,
            blocks: vec![node(id, Block::Paragraph(inlines))],
        })
        .collect(),
    };
    let markdown = output(&document(vec![node(
        "table",
        Block::Table { rows: vec![row], alignments: vec![] },
    )]));
    assert!(markdown.starts_with("| **Heading** | "), "{markdown}");
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, Parser::new_ext(&markdown, Options::ENABLE_TABLES));
    assert_eq!(html.matches("<strong>").count(), 3, "{html}");
    assert!(html.contains("<em>Nested</em>"), "{html}");
    assert!(html.contains("Other<br>detail"), "{html}");
}

#[test]
fn entity_text_split_across_inherited_header_marks_stays_literal() {
    let row = TableRow {
        cells: vec![Cell {
            header: true,
            row_span: 1,
            column_span: 1,
            blocks: vec![node(
                "p",
                Block::Paragraph(vec![
                    marked("&co", &[]),
                    marked("", &[InlineMark::Italic]),
                    marked("py;", &[InlineMark::Bold]),
                ]),
            )],
        }],
    };
    let markdown =
        output(&document(vec![node("t", Block::Table { rows: vec![row], alignments: vec![] })]));
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, Parser::new_ext(&markdown, Options::ENABLE_TABLES));
    assert!(html.contains("<strong>&amp;copy;</strong>"), "{html}");
}

#[test]
fn merged_html_header_keeps_nested_marks_and_escapes_source_text_once() {
    let rows = vec![TableRow {
        cells: vec![Cell {
            header: true,
            column_span: 2,
            row_span: 1,
            blocks: vec![node(
                "p",
                Block::Paragraph(vec![
                    marked("<R&D>", &[InlineMark::Bold, InlineMark::Italic]),
                    marked(" ** literal", &[]),
                ]),
            )],
        }],
    }];
    let markdown =
        output(&xlsx_document(vec![node("t", Block::Table { rows, alignments: vec![] })]));
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, Parser::new_ext(&markdown, Options::ENABLE_TABLES));
    assert!(
        html.contains(
            "<th colspan=\"2\"><strong><em>&lt;R&amp;D&gt;</em></strong> ** literal</th>"
        ),
        "{html}"
    );
}

fn marked(value: &str, marks: &[InlineMark]) -> Inline {
    Inline::Text { value: value.into(), marks: marks.to_vec() }
}

fn mask(marks: &[InlineMark]) -> u8 {
    marks.iter().fold(0, |bits, mark| {
        bits | match mark {
            InlineMark::Bold => 1,
            InlineMark::Italic => 2,
            InlineMark::Strikethrough => 4,
            InlineMark::Underline => 8,
            InlineMark::Superscript => 16,
            InlineMark::Subscript => 32,
        }
    })
}

fn semantic_text(html: &str) -> Vec<(char, u8)> {
    let mut output = Vec::new();
    let mut bits = 0;
    let mut rest = html;
    while !rest.is_empty() {
        if let Some(tag) = rest.strip_prefix('<') {
            let (tag, tail) = tag.split_once('>').unwrap();
            let bit = match tag.trim_start_matches('/') {
                "strong" => 1,
                "em" => 2,
                "del" => 4,
                "u" => 8,
                "sup" => 16,
                "sub" => 32,
                _ => 0,
            };
            if tag.starts_with('/') {
                bits &= !bit;
            } else {
                bits |= bit;
            }
            rest = tail;
        } else if rest.starts_with('&') {
            let (entity, tail) = rest.split_once(';').unwrap();
            let character = match entity {
                "&amp" => '&',
                "&lt" => '<',
                "&gt" => '>',
                "&quot" => '"',
                "&#39" => '\'',
                "&#32" => ' ',
                "&#9" => '\t',
                _ => panic!("unexpected {entity}"),
            };
            if !character.is_whitespace() {
                output.push((character, bits));
            }
            rest = tail;
        } else {
            let character = rest.chars().next().unwrap();
            if !character.is_whitespace() {
                output.push((character, bits));
            }
            rest = &rest[character.len_utf8()..];
        }
    }
    output
}

#[test]
fn native_marks_preserve_text_and_styles_across_word_and_punctuation_boundaries() {
    let mark_sets = [
        vec![InlineMark::Bold],
        vec![InlineMark::Italic],
        vec![InlineMark::Strikethrough],
        vec![InlineMark::Bold, InlineMark::Italic],
        vec![InlineMark::Bold, InlineMark::Strikethrough],
        vec![InlineMark::Bold, InlineMark::Underline],
        vec![InlineMark::Bold, InlineMark::Italic, InlineMark::Strikethrough],
    ];
    for marks in mark_sets {
        for text in [
            "word",
            "中文",
            "!",
            "“中文”",
            "a!",
            "*x*",
            "x_y",
            " x ",
            "    word",
            "\tword",
            " \tword",
            "e\u{301}",
            "<>&",
        ] {
            for (before, after) in [("", ""), ("a", "b"), (" ", " "), ("中", "文"), ("(", ")")] {
                let inlines = vec![marked(before, &[]), marked(text, &marks), marked(after, &[])];
                let markdown = output(&document(vec![node("p", Block::Paragraph(inlines))]));
                let mut html = String::new();
                pulldown_cmark::html::push_html(
                    &mut html,
                    Parser::new_ext(&markdown, Options::ENABLE_STRIKETHROUGH),
                );
                let expected = [(before, 0), (text, mask(&marks)), (after, 0)]
                    .into_iter()
                    .flat_map(|(text, mask)| {
                        text.chars().filter(|c| !c.is_whitespace()).map(move |c| (c, mask))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(semantic_text(&html), expected, "{markdown:?} => {html:?}");
            }
        }
    }
}

#[test]
fn adjacent_equal_marks_merge_and_different_marks_keep_their_extent() {
    let inlines = vec![
        marked("one", &[InlineMark::Bold]),
        marked("two", &[InlineMark::Bold]),
        marked("three", &[InlineMark::Italic]),
        marked("four", &[InlineMark::Bold]),
    ];
    let markdown = output(&document(vec![node("p", Block::Paragraph(inlines))]));
    assert!(markdown.starts_with("**onetwo**"));
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, Parser::new(&markdown));
    let expected = [("onetwo", 1), ("three", 2), ("four", 1)]
        .into_iter()
        .flat_map(|(s, mark)| s.chars().map(move |c| (c, mark)))
        .collect::<Vec<_>>();
    assert_eq!(semantic_text(&html), expected, "{markdown:?}");
}

#[test]
fn an_initial_line_break_does_not_turn_later_marks_into_literal_text() {
    for indent in ["", "    ", "\t"] {
        let doc = document(vec![node(
            "p",
            Block::Paragraph(vec![
                Inline::LineBreak,
                marked(&format!("\n{indent}"), &[]),
                marked("bold", &[InlineMark::Bold]),
                marked("!", &[]),
                Inline::LineBreak,
                marked("end", &[]),
            ]),
        )]);
        let markdown = output(&doc);
        let events = Parser::new(&markdown).collect::<Vec<_>>();
        assert_eq!(events.iter().filter(|e| matches!(e, Event::SoftBreak)).count(), 1);
        assert_eq!(events.iter().filter(|e| matches!(e, Event::HardBreak)).count(), 1);
        let mut html = String::new();
        pulldown_cmark::html::push_html(&mut html, events.into_iter());
        assert!(html.contains(&format!("\n{indent}<strong>bold</strong>!")), "{html:?}");
        assert_eq!(html.matches("<br>").count(), 1, "{html}");
    }
}

#[test]
fn standalone_style_tag_preserves_nested_marks_and_literal_stars_after_a_blank_line() {
    let runs: &[(&str, &[InlineMark])] = &[
        ("before\n\n", &[]),
        ("\n!word!", &[InlineMark::Underline]),
        ("suffix ", &[]),
        ("nested", &[InlineMark::Bold, InlineMark::Italic]),
        (" literal **", &[]),
    ];
    let inlines = runs.iter().map(|(text, marks)| marked(text, marks)).collect();
    let markdown = output(&document(vec![node("p", Block::Paragraph(inlines))]));
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, Parser::new(&markdown));
    let expected = runs
        .iter()
        .flat_map(|(text, marks)| {
            text.chars().filter(|character| !character.is_whitespace()).map(|ch| (ch, mask(marks)))
        })
        .collect::<Vec<_>>();
    assert_eq!(semantic_text(&html), expected, "{markdown:?} => {html:?}");
}

#[test]
fn unicode_uri_and_preencoded_octets_survive_rendering_without_double_encoding() {
    let destination = "目录/图 (1)%20%23%3F%25.png?a=中&b=%26";
    let markdown = output(&document(vec![node(
        "p",
        Block::Paragraph(vec![Inline::Link {
            target: destination.into(),
            content: vec![marked("image", &[])],
        }]),
    )]));
    let hrefs = Parser::new(&markdown)
        .filter_map(|event| match event {
            Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(hrefs, [destination.replace(' ', "%20")]);
    assert!(markdown.contains("目录/图"));
    assert!(!markdown.contains("%2520"));
}

#[test]
fn notes_heading_tracks_final_visible_content_and_preserves_ordinary_headings() {
    use into_markdown_core::speaker_notes;
    let mut heading =
        node("notes", Block::Heading { level: 3, content: vec![marked("Speaker notes", &[])] });
    speaker_notes::mark_heading(&mut heading).unwrap();
    let mut image = node("picture", Block::Image { asset: AssetId("image".into()), alt: None });
    speaker_notes::mark_body(&mut image).unwrap();
    let asset = Asset {
        id: AssetId("image".into()),
        filename: Some("image.png".into()),
        media_type: "image/png".into(),
        bytes: vec![1],
        external_uri: None,
    };
    let mut doc = document(vec![heading.clone(), image.clone()]);
    let mut options = ConversionOptions::default();
    for mode in [AssetMode::Extract, AssetMode::Embed, AssetMode::Omit] {
        options.output.asset_mode = mode;
        let markdown = render(&doc, std::slice::from_ref(&asset), &options).unwrap();
        assert_eq!(markdown.contains("Speaker notes"), mode != AssetMode::Omit);
    }
    doc.blocks.push(paragraph(&format!("{}::ocr::1", image.id.0), "Recognized note"));
    assert!(
        render(&doc, std::slice::from_ref(&asset), &options).unwrap().contains("Speaker notes")
    );
    doc.blocks = vec![heading, image];
    if let Block::Image { alt, .. } = &mut doc.blocks[1].block {
        *alt = Some("Alternative text".into());
    }
    assert!(
        render(&doc, std::slice::from_ref(&asset), &options).unwrap().contains("Speaker notes")
    );
    doc.blocks = vec![node(
        "ordinary",
        Block::Heading { level: 3, content: vec![marked("Speaker notes", &[])] },
    )];
    assert_eq!(render(&doc, &[], &options).unwrap(), "### Speaker notes\n");
}

#[test]
fn custom_numbering_restarts_tasks_and_empty_items_preserve_structure_without_source_comments() {
    let list = |id, start| {
        node(
            id,
            Block::List {
                kind: ListKind::Ordered,
                start,
                items: vec![
                    ListItem {
                        checked: None,
                        marker_label: Some("第壹章".into()),
                        blocks: vec![paragraph(&format!("{id}-p"), "item")],
                    },
                    ListItem {
                        checked: None, marker_label: Some("第贰章".into()), blocks: vec![]
                    },
                ],
            },
        )
    };
    let doc = document(vec![
        list("a", 7),
        list("b", 3),
        node(
            "tasks",
            Block::List {
                kind: ListKind::Task,
                start: 1,
                items: vec![ListItem {
                    checked: Some(true),
                    marker_label: None,
                    blocks: vec![paragraph("t", "done")],
                }],
            },
        ),
    ]);
    let original = doc.clone();
    let markdown = output(&doc);
    assert!(!markdown.contains("source-marker"));
    assert!(!markdown.contains("第壹章"));
    let events = Parser::new_ext(&markdown, Options::ENABLE_TASKLISTS).collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                Event::Start(Tag::List(start)) => Some(*start),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [Some(7), Some(3), None]
    );
    assert_eq!(events.iter().filter(|event| matches!(event, Event::Start(Tag::Item))).count(), 5);
    assert!(events.contains(&Event::TaskListMarker(true)));
    assert_eq!(doc, original);
}

#[test]
fn wide_ordered_markers_keep_children_and_html_preserves_non_markdown_starts() {
    for start in [100, 100_000_000, 1_000_000_000] {
        let doc = document(vec![node(
            "wide",
            Block::List {
                kind: ListKind::Ordered,
                start,
                items: vec![ListItem {
                    checked: None,
                    marker_label: Some("源编号".into()),
                    blocks: vec![
                        paragraph("a", "first"),
                        node("b", Block::Paragraph(vec![marked("second", &[InlineMark::Bold])])),
                    ],
                }],
            },
        )]);
        let before = doc.clone();
        let markdown = output(&doc);
        let events = Parser::new(&markdown).collect::<Vec<_>>();
        assert!(!events.iter().any(|event| matches!(event, Event::Start(Tag::CodeBlock(_)))));
        if start <= 999_999_999 {
            assert_eq!(events.first(), Some(&Event::Start(Tag::List(Some(start)))));
            assert_eq!(events.last(), Some(&Event::End(pulldown_cmark::TagEnd::List(true))));
        } else {
            assert!(markdown.starts_with("<ol start=\"1000000000\">"));
        }
        assert!(events.contains(&Event::Start(Tag::Strong)));
        assert_eq!(doc, before);
    }
}

#[test]
fn native_marks_beside_links_code_and_table_breaks_keep_semantics() {
    let content = vec![
        marked("bold", &[InlineMark::Bold]),
        Inline::Link {
            target: "https://example.test/中文".into(),
            content: vec![marked("link", &[InlineMark::Italic])],
        },
        Inline::Code("*literal*".into()),
        Inline::LineBreak,
        marked("deleted", &[InlineMark::Strikethrough]),
    ];
    let markdown = output(&document(vec![node("p", Block::Paragraph(content.clone()))]));
    let events = Parser::new_ext(&markdown, Options::ENABLE_STRIKETHROUGH).collect::<Vec<_>>();
    assert!(events.contains(&Event::Start(Tag::Strong)));
    assert!(events.contains(&Event::Start(Tag::Emphasis)));
    assert!(events.contains(&Event::Start(Tag::Strikethrough)));
    assert!(events.contains(&Event::Code("*literal*".into())));
    assert!(events.contains(&Event::HardBreak));
    let table = output(&document(vec![node(
        "table",
        Block::Table {
            alignments: vec![],
            rows: vec![TableRow {
                cells: vec![Cell {
                    blocks: vec![node("cell", Block::Paragraph(content))],
                    row_span: 1,
                    column_span: 1,
                    header: false,
                }],
            }],
        },
    )]));
    assert!(table.contains("<br>"));
    assert!(
        Parser::new_ext(&table, Options::ENABLE_TABLES)
            .any(|event| matches!(event, Event::Start(Tag::Table(_))))
    );
}

#[test]
fn hidden_source_markers_keep_ir_memory_without_rendered_overhead() {
    use into_markdown_core::{ExecutionOptions, ResourceLimits};

    let make = |label| {
        document(vec![node(
            "list",
            Block::List {
                kind: ListKind::Ordered,
                start: 3,
                items: vec![ListItem {
                    checked: None,
                    marker_label: label,
                    blocks: vec![paragraph("p", "body")],
                }],
            },
        )])
    };
    let plain = make(None);
    let label = "源编号".repeat(4096);
    let label_bytes = u64::try_from(label.len()).unwrap();
    let labeled = make(Some(label));
    let retained =
        |doc| into_markdown_core::estimate_retained_output(doc, &vec![], &vec![]).unwrap();
    assert!(retained(&labeled) >= retained(&plain) + label_bytes);
    assert_eq!(output(&plain), output(&labeled));
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    assert_eq!(
        planned_render_peak(&plain, &[], &options, &context).unwrap(),
        planned_render_peak(&labeled, &[], &options, &context).unwrap()
    );
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn retained_non_browser_images_have_visible_download_labels() {
    let asset = Asset {
        id: AssetId("drawing".into()),
        filename: Some("drawing.svm".into()),
        media_type: "application/x-openoffice-gdimetafile".into(),
        bytes: vec![1, 2, 3],
        external_uri: None,
    };
    for alt in [None, Some("".into()), Some("   ".into())] {
        let doc = document(vec![node("picture", Block::Image { asset: asset.id.clone(), alt })]);
        for mode in [AssetMode::Extract, AssetMode::Embed] {
            let mut options = ConversionOptions::default();
            options.output.asset_mode = mode;
            let output = render(&doc, std::slice::from_ref(&asset), &options).unwrap();
            assert!(output.starts_with("[Original image](<"));
            assert!(!output.starts_with("!"));
        }
    }
}
