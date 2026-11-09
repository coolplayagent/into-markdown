use super::*;
use into_markdown_core::{ExecutionOptions, ResourceLimits};

fn events(markdown: &str) -> Vec<Event<'_>> {
    Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES,
    )
    .collect()
}

#[test]
fn ordinary_punctuation_stays_readable_and_literal() {
    let text = "1.2、A-B、file_name 和 A | B! C++ {item} #tag a>b";
    assert_eq!(output(&document(vec![paragraph("p", text)])), format!("{text}\n"));
}

#[test]
fn literal_block_markers_keep_paragraph_structure_on_every_line() {
    for text in [
        "# 标题",
        "##",
        "> quote",
        "+ item",
        "- item",
        "1. item",
        "9) item",
        "---",
        "- - -",
        "===",
        "```code",
        "~~~",
        "    indented",
        "\tindented",
        "| A | B |\n| --- | --- |",
        "A | B\n---|---",
        "[ref]: target",
        "<div>literal</div>",
    ] {
        for prefix in ["", "before\n", "before  \n", "before\n\n"] {
            let source = format!("{prefix}{text}");
            let markdown = output(&document(vec![paragraph("p", &source)]));
            let parsed = events(&markdown);
            let visible: String = parsed
                .iter()
                .filter_map(|event| match event {
                    Event::Text(value) => Some(value.as_ref()),
                    _ => None,
                })
                .collect();
            let compact =
                |value: &str| value.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            assert_eq!(compact(&visible), compact(&source), "{markdown:?}");
            assert!(
                parsed.iter().all(|event| matches!(
                    event,
                    Event::Start(Tag::Paragraph)
                        | Event::End(pulldown_cmark::TagEnd::Paragraph)
                        | Event::Text(_)
                        | Event::SoftBreak
                )),
                "{source:?} => {markdown:?}: {parsed:?}"
            );
        }
    }
}

#[test]
fn source_breaks_preserve_soft_hard_and_paragraph_boundaries() {
    let doc = document(vec![
        node(
            "p",
            Block::Paragraph(vec![
                Inline::Text { value: "one\r\ntwo  \rthree\n\nfour".into(), marks: vec![] },
                Inline::LineBreak,
                Inline::Text { value: "# literal".into(), marks: vec![] },
            ]),
        ),
        paragraph("end", "last"),
    ]);
    let markdown = output(&doc);
    let parsed = events(&markdown);
    assert_eq!(parsed.iter().filter(|e| matches!(e, Event::SoftBreak)).count(), 2);
    assert_eq!(parsed.iter().filter(|e| matches!(e, Event::HardBreak)).count(), 1);
    assert_eq!(parsed.iter().filter(|e| matches!(e, Event::Start(Tag::Paragraph))).count(), 3);
    assert!(markdown.contains("four  \n\\# literal"));
}

#[test]
fn table_text_breaks_and_pipes_preserve_cells_and_styles() {
    for marks in [vec![], vec![InlineMark::Bold], vec![InlineMark::Underline]] {
        let doc = document(vec![node(
            "t",
            Block::Table {
                alignments: vec![],
                rows: vec![TableRow {
                    cells: vec![Cell {
                        row_span: 1,
                        column_span: 1,
                        header: false,
                        blocks: vec![node(
                            "cell",
                            Block::Paragraph(vec![Inline::Text {
                                value: "\r\n第一行|a\r第二行\n\n第三行\n".into(),
                                marks,
                            }]),
                        )],
                    }],
                }],
            },
        )]);
        let markdown = output(&doc);
        assert!(markdown.contains("第一行\\|a<br>第二行"), "{markdown}");
        assert!(markdown.contains("<br><br>"));
        assert_eq!(
            events(&markdown).iter().filter(|e| matches!(e, Event::Start(Tag::TableCell))).count(),
            2
        );
        assert!(!markdown.contains('\r'));
    }
}

#[test]
fn multiline_marks_headings_and_links_keep_their_extent() {
    let text = |value: &str| Inline::Text { value: value.into(), marks: vec![InlineMark::Bold] };
    let markdown =
        output(&document(vec![node("p", Block::Paragraph(vec![text("one\ntwo\n\nthree")]))]));
    assert_eq!(markdown, "**one\ntwo**\n\n**three**\n");
    let spaced = output(&document(vec![node("p", Block::Paragraph(vec![text("one\n \ntwo")]))]));
    assert_eq!(spaced, "**one**\n \n**two**\n");
    let heading = output(&document(vec![node(
        "h",
        Block::Heading {
            level: 2,
            content: vec![
                text("one\ntwo"),
                Inline::LineBreak,
                Inline::Text { value: "end #".into(), marks: vec![] },
            ],
        },
    )]));
    assert_eq!(heading, "## **one<br>two**<br>end \\#\n");
    let link = output(&document(vec![node(
        "p",
        Block::Paragraph(vec![Inline::Link {
            target: "https://example.test".into(),
            content: vec![text("a\nb\n\nc")],
        }]),
    )]));
    assert_eq!(
        events(&link).iter().filter(|e| matches!(e, Event::Start(Tag::Link { .. }))).count(),
        1
    );
}

#[test]
fn fragmented_text_and_nested_lists_share_line_context() {
    let source = "1.2 file_name\n# literal\n1) item";
    let parts =
        source.chars().map(|c| Inline::Text { value: c.to_string(), marks: vec![] }).collect();
    let block = node("p", Block::Paragraph(parts));
    assert_eq!(
        output(&document(vec![block.clone()])),
        output(&document(vec![paragraph("p", source)]))
    );
    let doc = document(vec![node(
        "list",
        Block::List {
            kind: ListKind::Bullet,
            start: 1,
            items: vec![ListItem { checked: None, marker_label: None, blocks: vec![block] }],
        },
    )]);
    let markdown = output(&doc);
    let parsed = events(&markdown);
    assert_eq!(parsed.iter().filter(|e| matches!(e, Event::Start(Tag::Item))).count(), 1);
    assert!(!parsed.iter().any(|e| matches!(e, Event::Start(Tag::Heading { .. }))));
}

#[test]
fn edge_and_consecutive_explicit_breaks_remain_visible_inside_links_and_paragraphs() {
    let content = vec![
        Inline::LineBreak,
        Inline::Text { value: "a".into(), marks: vec![] },
        Inline::LineBreak,
        Inline::LineBreak,
        Inline::Text { value: "b".into(), marks: vec![] },
        Inline::LineBreak,
    ];
    for inlines in
        [content.clone(), vec![Inline::Link { target: "https://example.test".into(), content }]]
    {
        let markdown = output(&document(vec![node("p", Block::Paragraph(inlines))]));
        assert_eq!(markdown.matches("<br>").count(), 4);
        assert_eq!(
            events(&markdown).iter().filter(|e| matches!(e, Event::Start(Tag::Paragraph))).count(),
            1
        );
        if markdown.starts_with('[') {
            assert_eq!(
                events(&markdown)
                    .iter()
                    .filter(|e| matches!(e, Event::Start(Tag::Link { .. })))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn multiline_marked_runs_fit_admitted_memory_without_changing_ir() {
    let doc = document(vec![node(
        "p",
        Block::Paragraph(vec![Inline::Text {
            value: "\t# heading\rone & two\r\rthree | four\r\n".repeat(1024),
            marks: vec![
                InlineMark::Bold,
                InlineMark::Italic,
                InlineMark::Underline,
                InlineMark::Strikethrough,
                InlineMark::Superscript,
            ],
        }]),
    )]);
    let original = doc.clone();
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let plan = planned_render_peak(&doc, &[], &options, &context).unwrap();
    let low = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: plan - 1, ..Default::default() },
    );
    assert!(low.reserve_memory(plan).is_err());
    let exact = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: plan, ..Default::default() },
    );
    let permit = exact.reserve_memory(plan).unwrap();
    let markdown = output(&doc);
    assert!(markdown.capacity() as u64 <= plan);
    assert_eq!(doc, original);
    drop(permit);
    assert_eq!(exact.reserved_memory_bytes(), 0);
}
