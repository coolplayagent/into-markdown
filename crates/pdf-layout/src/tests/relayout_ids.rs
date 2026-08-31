use super::*;

#[test]
fn new_ocr_table_preserves_existing_table_and_uses_distinct_nested_ids() {
    let mut native = Vec::new();
    for row in 0..3 {
        for column in 0..3 {
            native.extend(source_text(
                ["A", "B", "C"][column],
                Rect {
                    x: 40.0 + column as f32 * 210.0,
                    y: 80.0 + row as f32 * 25.0,
                    width: 40.0,
                    height: 12.0,
                },
                12.0,
            ));
        }
    }
    let mut input = rebuild(document(native));
    let original = page_blocks(&input)[0].clone();
    assert!(matches!(original.block, Block::Table { .. }));
    let mut additions = Vec::new();
    for row in 0..3 {
        for column in 0..3 {
            additions.push(ocr(
                ["D", "E", "F"][column],
                Rect {
                    x: 40.0 + column as f32 * 210.0,
                    y: 380.0 + row as f32 * 25.0,
                    width: 40.0,
                    height: 12.0,
                },
            ));
        }
    }
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    blocks.push(BlockNode {
        id: NodeId("new-ocr".into()),
        block: Block::Paragraph(additions),
        provenance: provenance(1, None, 12.0, 0.0),
    });
    let actual = rebuild(input.clone());
    actual.validate().unwrap();
    let blocks = page_blocks(&actual);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0], original);
    let Block::Table { rows, .. } = &blocks[1].block else { panic!("new table") };
    let text = rows
        .iter()
        .flat_map(|row| &row.cells)
        .flat_map(|cell| &cell.blocks)
        .map(|node| block_text(&node.block))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(text, "D E F D E F D E F");
    assert_eq!(actual, rebuild(input));
    assert_eq!(actual, rebuild(actual.clone()));
}

#[test]
fn retained_link_paragraph_does_not_collide_with_new_paragraph() {
    let mut input =
        document(source_text("new", Rect { x: 40.0, y: 80.0, width: 30.0, height: 12.0 }, 12.0));
    let retained = BlockNode {
        id: NodeId("pdf-page-1-layout-paragraph-0".into()),
        block: Block::Paragraph(vec![Inline::Link {
            target: "https://example.com".into(),
            content: vec![Inline::Text { value: "original link".into(), marks: Vec::new() }],
        }]),
        provenance: provenance(
            1,
            Some(Rect { x: 40.0, y: 380.0, width: 100.0, height: 12.0 }),
            12.0,
            0.0,
        ),
    };
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { panic!("page") };
    blocks.push(retained.clone());
    let actual = rebuild(input);
    actual.validate().unwrap();
    assert!(page_blocks(&actual).iter().any(|node| node == &retained));
    assert_eq!(
        page_blocks(&actual).iter().filter(|node| block_text(&node.block) == "new").count(),
        1
    );
}
