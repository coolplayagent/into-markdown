#![allow(clippy::cast_precision_loss, reason = "small bounded fixture coordinates")]

use super::*;
use into_markdown_core::{
    AssetId, Block, BlockNode, CancellationToken, ExecutionOptions, Inline, NodeId, OcrEvidence,
    OcrEvidenceStage, OcrEvidenceStep, OcrSourceRegion, Provenance, ProvenanceKind, Rect,
    ResourceLimits, SourceLocator, SourcePoint,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
use std::time::Duration;

fn context() -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
}

fn provenance(page: u32, bounds: Option<Rect>, font_size: f32, rotation: f32) -> Provenance {
    Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "builtin.converter.pdfium".into(),
        locator: SourceLocator {
            page: Some(page),
            bounds,
            font_size: Some(font_size),
            rotation_degrees: Some(rotation),
            page_width: Some(600.0),
            page_height: Some(800.0),
            ..SourceLocator::default()
        },
        confidence: None,
    }
}

fn source(value: &str, bounds: Rect, font_size: f32) -> Inline {
    assert_eq!(value.chars().count(), 1);
    Inline::SourceText {
        value: value.into(),
        marks: Vec::new(),
        provenance: Box::new(provenance(1, Some(bounds), font_size, 0.0)),
    }
}

fn source_text(value: &str, bounds: Rect, font_size: f32) -> Vec<Inline> {
    let count = value.chars().count();
    let width = bounds.width / count as f32;
    value
        .chars()
        .enumerate()
        .map(|(index, character)| {
            source(
                &character.to_string(),
                Rect { x: bounds.x + index as f32 * width, width, ..bounds },
                font_size,
            )
        })
        .collect()
}

fn without_font(mut inlines: Vec<Inline>) -> Vec<Inline> {
    for inline in &mut inlines {
        let Inline::SourceText { provenance, .. } = inline else { unreachable!() };
        provenance.locator.font_size = None;
    }
    inlines
}

fn ocr(value: &str, bounds: Rect) -> Inline {
    let polygon = [
        SourcePoint { x: bounds.x, y: bounds.y },
        SourcePoint { x: bounds.x + bounds.width, y: bounds.y },
        SourcePoint { x: bounds.x + bounds.width, y: bounds.y + bounds.height },
        SourcePoint { x: bounds.x, y: bounds.y + bounds.height },
    ];
    Inline::OcrText {
        value: value.into(),
        marks: Vec::new(),
        provenance: Box::new(Provenance {
            kind: ProvenanceKind::LocalOcr,
            provider: "test.recognizer".into(),
            locator: SourceLocator {
                page: Some(1),
                bounds: Some(bounds),
                page_width: Some(600.0),
                page_height: Some(800.0),
                ..SourceLocator::default()
            },
            confidence: Some(0.98),
        }),
        evidence: Box::new(OcrEvidence {
            page: 1,
            regions: vec![OcrSourceRegion {
                source_index: 0,
                polygon,
                detection_confidence: 0.99,
                recognition_confidence: 0.98,
            }],
            chain: vec![
                OcrEvidenceStep {
                    stage: OcrEvidenceStage::Detection,
                    provider: "test.detector".into(),
                    model: Some("detector".into()),
                },
                OcrEvidenceStep {
                    stage: OcrEvidenceStage::Recognition,
                    provider: "test.recognizer".into(),
                    model: Some("recognizer".into()),
                },
                OcrEvidenceStep {
                    stage: OcrEvidenceStage::Merge,
                    provider: "builtin.ocr.ir-merge".into(),
                    model: None,
                },
            ],
        }),
    }
}

fn document(inlines: Vec<Inline>) -> Document {
    Document { blocks: vec![page_node(1, inlines)], ..Document::default() }
}

fn page_node(page: u32, mut inlines: Vec<Inline>) -> BlockNode {
    for inline in &mut inlines {
        match inline {
            Inline::SourceText { provenance, .. } => provenance.locator.page = Some(page),
            Inline::OcrText { provenance, evidence, .. } => {
                provenance.locator.page = Some(page);
                evidence.page = page;
            }
            _ => {}
        }
    }
    BlockNode {
        id: NodeId(format!("pdf-page-{page}")),
        block: Block::Page {
            number: page,
            blocks: vec![BlockNode {
                id: NodeId(format!("pdf-page-{page}-native-text")),
                block: Block::Paragraph(inlines),
                provenance: provenance(page, None, 12.0, 0.0),
            }],
        },
        provenance: provenance(page, None, 12.0, 0.0),
    }
}

fn rebuild(document: Document) -> Document {
    let output = reconstruct_document(document, &LayoutConfig::default(), &context()).unwrap();
    let (document, reservation) = output.into_parts();
    drop(reservation);
    document
}

fn page_blocks(document: &Document) -> &[BlockNode] {
    let Block::Page { blocks, .. } = &document.blocks[0].block else { panic!("page") };
    blocks
}

fn inline_text(inlines: &[Inline]) -> String {
    inlines
        .iter()
        .filter_map(|inline| match inline {
            Inline::Text { value, .. }
            | Inline::SourceText { value, .. }
            | Inline::OcrText { value, .. } => Some(value.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn zero_area_native_whitespace_is_ignored_but_ocr_geometry_is_required() {
    let mut inlines =
        source_text("Alpha", Rect { x: 40.0, y: 80.0, width: 50.0, height: 12.0 }, 12.0);
    inlines.push(source("\n", Rect { x: 90.0, y: 80.0, width: 0.0, height: 0.0 }, 0.0));
    inlines.extend(source_text(
        "Beta",
        Rect { x: 40.0, y: 110.0, width: 40.0, height: 12.0 },
        12.0,
    ));
    let rebuilt = rebuild(document(inlines));
    assert_eq!(page_blocks(&rebuilt).len(), 1);
    let Block::Paragraph(content) = &page_blocks(&rebuilt)[0].block else { panic!("paragraph") };
    assert_eq!(inline_text(content), "Alpha Beta");

    let invalid_ocr = document(vec![ocr(" ", Rect { x: 40.0, y: 80.0, width: 0.0, height: 0.0 })]);
    let error =
        reconstruct_document(invalid_ocr, &LayoutConfig::default(), &context()).err().unwrap();
    assert!(matches!(
        error,
        ConversionError::Malformed { detail, .. }
            if detail == "pdfLayoutOcrEvidenceMissingGeometry"
    ));
}

fn block_text(block: &Block) -> String {
    match block {
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => inline_text(inlines),
        _ => String::new(),
    }
}

#[test]
fn non_pdf_documents_are_unchanged_without_a_layout_lease() {
    let document = Document::default();
    let output =
        reconstruct_document(document.clone(), &LayoutConfig::default(), &context()).unwrap();
    let (actual, reservation) = output.into_parts();
    assert_eq!(actual, document);
    assert!(reservation.is_none());
}

#[test]
fn dependency_dag_keeps_generic_ocr_free_of_pdf_layout() {
    let ocr_cargo = include_str!("../../ocr/Cargo.toml");
    let ocr_bazel = include_str!("../../ocr/BUILD.bazel");
    let layout_cargo = include_str!("../Cargo.toml");
    assert!(!ocr_cargo.contains("pdf-layout"));
    assert!(!ocr_bazel.contains("pdf-layout"));
    for forbidden in ["into-markdown-ocr", "into-markdown-converters", "into-markdown-pdfium"] {
        assert!(!layout_cargo.contains(forbidden));
    }
}

#[test]
fn spanning_heading_and_two_columns_have_stable_geometric_order() {
    let input = document(
        [
            source_text(
                "Layout title",
                Rect { x: 60.0, y: 20.0, width: 420.0, height: 24.0 },
                24.0,
            ),
            source_text("Left one", Rect { x: 40.0, y: 90.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Right one", Rect { x: 350.0, y: 90.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Left two", Rect { x: 40.0, y: 120.0, width: 196.0, height: 12.0 }, 12.0),
            source_text("Right two", Rect { x: 350.0, y: 120.0, width: 194.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert!(matches!(blocks[0].block, Block::Heading { .. }));
    assert_eq!(block_text(&blocks[0].block), "Layout title");
    assert_eq!(block_text(&blocks[1].block), "Left one Left two");
    assert_eq!(block_text(&blocks[2].block), "Right one Right two");
    assert!(!blocks.iter().any(|node| matches!(node.block, Block::Table { .. })));
    let second = rebuild(actual.clone());
    assert_eq!(actual, second, "relayout after OCR merge must be idempotent");
}

#[test]
fn headings_lists_and_two_by_two_tables_use_conservative_geometry() {
    let input = document(
        [
            source_text("Section", Rect { x: 40.0, y: 20.0, width: 140.0, height: 24.0 }, 24.0),
            source_text("- Alpha", Rect { x: 50.0, y: 80.0, width: 90.0, height: 12.0 }, 12.0),
            source_text("- Beta", Rect { x: 50.0, y: 105.0, width: 80.0, height: 12.0 }, 12.0),
            source_text("Name", Rect { x: 50.0, y: 180.0, width: 50.0, height: 13.0 }, 13.0),
            source_text("Value", Rect { x: 150.0, y: 180.0, width: 50.0, height: 13.0 }, 13.0),
            source_text("A", Rect { x: 50.0, y: 205.0, width: 20.0, height: 12.0 }, 12.0),
            source_text("1", Rect { x: 150.0, y: 205.0, width: 20.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert!(blocks.iter().any(|node| matches!(node.block, Block::Heading { .. })));
    assert!(blocks.iter().any(|node| matches!(node.block, Block::List { .. })));
    let table = blocks.iter().find(|node| matches!(node.block, Block::Table { .. })).unwrap();
    let Block::Table { rows, .. } = &table.block else { unreachable!() };
    assert_eq!((rows.len(), rows[0].cells.len()), (2, 2));
    assert!(rows[0].cells.iter().all(|cell| cell.header));
}

#[test]
fn repeated_compact_cells_across_a_wide_gap_remain_a_table() {
    let input = document(
        [
            source_text("Key", Rect { x: 40.0, y: 180.0, width: 40.0, height: 13.0 }, 13.0),
            source_text("Value", Rect { x: 360.0, y: 180.0, width: 40.0, height: 13.0 }, 13.0),
            source_text("A", Rect { x: 40.0, y: 205.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("1", Rect { x: 360.0, y: 205.0, width: 40.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let table = page_blocks(&actual)
        .iter()
        .find_map(|node| match &node.block {
            Block::Table { rows, .. } => Some(rows),
            _ => None,
        })
        .expect("wide-gap table");
    assert_eq!((table.len(), table[0].cells.len()), (2, 2));
    assert_eq!(
        table
            .iter()
            .flat_map(|row| &row.cells)
            .map(|cell| block_text(&cell.blocks[0].block))
            .collect::<Vec<_>>(),
        ["Key", "Value", "A", "1"]
    );
}

#[test]
fn repeated_long_cell_boundaries_lock_a_wide_two_by_two_table() {
    let input = document(
        [
            source_text(
                "Long left cell",
                Rect { x: 40.0, y: 180.0, width: 130.0, height: 12.0 },
                12.0,
            ),
            source_text(
                "Long right cell",
                Rect { x: 360.0, y: 180.0, width: 130.0, height: 12.0 },
                12.0,
            ),
            source_text(
                "Another value",
                Rect { x: 40.0, y: 205.0, width: 130.0, height: 12.0 },
                12.0,
            ),
            source_text(
                "Second value",
                Rect { x: 360.0, y: 205.0, width: 130.0, height: 12.0 },
                12.0,
            ),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let tables = page_blocks(&actual)
        .iter()
        .filter(|node| matches!(node.block, Block::Table { .. }))
        .count();
    assert_eq!(tables, 1);
}

#[test]
fn asymmetric_two_by_two_repeated_boundaries_allow_a_wide_description_cell() {
    let input = document(
        [
            source_text(
                "Wide description",
                Rect { x: 40.0, y: 180.0, width: 270.0, height: 12.0 },
                12.0,
            ),
            source_text("Value", Rect { x: 400.0, y: 180.0, width: 100.0, height: 12.0 }, 12.0),
            source_text(
                "Another description",
                Rect { x: 40.0, y: 205.0, width: 270.0, height: 12.0 },
                12.0,
            ),
            source_text("Other", Rect { x: 400.0, y: 205.0, width: 100.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let Block::Table { rows, .. } = &page_blocks(&actual)[0].block else {
        panic!("asymmetric table")
    };
    assert_eq!((rows.len(), rows[0].cells.len()), (2, 2));
}

#[test]
fn noncompact_header_absorbs_multiple_compact_rows_despite_font_oscillation_and_absence() {
    let input = document(
        [
            source_text(
                "Long left header",
                Rect { x: 40.0, y: 180.0, width: 130.0, height: 14.0 },
                14.0,
            ),
            source_text(
                "Long right header",
                Rect { x: 360.0, y: 180.0, width: 130.0, height: 14.0 },
                14.0,
            ),
            source_text("A", Rect { x: 40.0, y: 205.0, width: 40.0, height: 12.0 }, 10.0),
            source_text("1", Rect { x: 360.0, y: 205.0, width: 40.0, height: 12.0 }, 10.0),
            source_text("B", Rect { x: 40.0, y: 230.0, width: 40.0, height: 12.0 }, 11.0),
            source_text("2", Rect { x: 360.0, y: 230.0, width: 40.0, height: 12.0 }, 11.0),
            without_font(source_text(
                "C",
                Rect { x: 40.0, y: 255.0, width: 40.0, height: 12.0 },
                9.0,
            )),
            without_font(source_text(
                "3",
                Rect { x: 360.0, y: 255.0, width: 40.0, height: 12.0 },
                9.0,
            )),
            source_text("D", Rect { x: 40.0, y: 280.0, width: 40.0, height: 12.0 }, 10.0),
            source_text("4", Rect { x: 360.0, y: 280.0, width: 40.0, height: 12.0 }, 10.0),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let Block::Table { rows, .. } = &page_blocks(&actual)[0].block else {
        panic!("header and body table")
    };
    assert_eq!((rows.len(), rows[0].cells.len()), (5, 2));
    assert!(rows[0].cells.iter().all(|cell| cell.header));
    assert_eq!(
        rows.iter()
            .flat_map(|row| &row.cells)
            .map(|cell| block_text(&cell.blocks[0].block))
            .collect::<Vec<_>>(),
        ["Long left header", "Long right header", "A", "1", "B", "2", "C", "3", "D", "4"]
    );
}

#[test]
fn three_equal_broad_dual_columns_are_not_a_table() {
    let input = document(
        [
            source_text("Left one", Rect { x: 40.0, y: 90.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Right one", Rect { x: 350.0, y: 90.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Left two", Rect { x: 40.0, y: 120.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Right two", Rect { x: 350.0, y: 120.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Left three", Rect { x: 40.0, y: 150.0, width: 210.0, height: 12.0 }, 12.0),
            source_text(
                "Right three",
                Rect { x: 350.0, y: 150.0, width: 210.0, height: 12.0 },
                12.0,
            ),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert!(!blocks.iter().any(|node| matches!(node.block, Block::Table { .. })));
    assert_eq!(block_text(&blocks[0].block), "Left one Left two Left three");
    assert_eq!(block_text(&blocks[1].block), "Right one Right two Right three");
}

#[test]
fn larger_first_row_does_not_turn_broad_dual_columns_into_a_table() {
    let input = document(
        [
            source_text("Left one", Rect { x: 40.0, y: 90.0, width: 210.0, height: 14.0 }, 14.0),
            source_text("Right one", Rect { x: 350.0, y: 90.0, width: 210.0, height: 14.0 }, 14.0),
            source_text("Left two", Rect { x: 40.0, y: 120.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Right two", Rect { x: 350.0, y: 120.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Left three", Rect { x: 40.0, y: 150.0, width: 210.0, height: 12.0 }, 12.0),
            source_text(
                "Right three",
                Rect { x: 350.0, y: 150.0, width: 210.0, height: 12.0 },
                12.0,
            ),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert!(!blocks.iter().any(|node| matches!(node.block, Block::Table { .. })));
    assert_eq!(block_text(&blocks[0].block), "Left one Left two Left three");
    assert_eq!(block_text(&blocks[1].block), "Right one Right two Right three");
}

#[test]
fn two_compact_column_rows_need_repeated_edges_or_a_confirmed_header() {
    let input = document(
        [
            source_text("L1", Rect { x: 40.0, y: 90.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("R1", Rect { x: 350.0, y: 90.0, width: 50.0, height: 12.0 }, 12.0),
            source_text("L2", Rect { x: 40.0, y: 120.0, width: 45.0, height: 12.0 }, 12.0),
            source_text("R2", Rect { x: 350.0, y: 120.0, width: 55.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert!(!blocks.iter().any(|node| matches!(node.block, Block::Table { .. })));
    assert_eq!(block_text(&blocks[0].block), "L1 L2");
    assert_eq!(block_text(&blocks[1].block), "R1 R2");
}

#[test]
fn broad_column_prefix_does_not_poison_following_same_profile_header_table() {
    let input = document(
        [
            source_text("Left one", Rect { x: 40.0, y: 90.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Right one", Rect { x: 350.0, y: 90.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Left two", Rect { x: 40.0, y: 120.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Right two", Rect { x: 350.0, y: 120.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Left three", Rect { x: 40.0, y: 150.0, width: 210.0, height: 12.0 }, 12.0),
            source_text(
                "Right three",
                Rect { x: 350.0, y: 150.0, width: 210.0, height: 12.0 },
                12.0,
            ),
            source_text("Table key", Rect { x: 40.0, y: 180.0, width: 210.0, height: 14.0 }, 14.0),
            source_text(
                "Table value",
                Rect { x: 350.0, y: 180.0, width: 210.0, height: 14.0 },
                14.0,
            ),
            source_text("A", Rect { x: 40.0, y: 210.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("1", Rect { x: 350.0, y: 210.0, width: 40.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert_eq!(block_text(&blocks[0].block), "Left one Left two Left three");
    assert_eq!(block_text(&blocks[1].block), "Right one Right two Right three");
    let Block::Table { rows, .. } = &blocks[2].block else { panic!("following table") };
    assert_eq!((rows.len(), rows[0].cells.len()), (2, 2));
    assert!(rows[0].cells.iter().all(|cell| cell.header));
}

#[test]
fn three_by_three_long_body_is_ambiguous_and_remains_text() {
    let mut inlines = Vec::new();
    inlines.try_reserve_exact(9).unwrap();
    for row in 0..3 {
        for column in 0..3 {
            inlines.extend(source_text(
                ["Long left body", "Long middle body", "Long right body"][column],
                Rect {
                    x: 40.0 + column as f32 * 195.0,
                    y: 180.0 + row as f32 * 25.0,
                    width: 130.0,
                    height: 12.0,
                },
                12.0,
            ));
        }
    }
    let actual = rebuild(document(inlines));
    assert!(!page_blocks(&actual).iter().any(|node| matches!(node.block, Block::Table { .. })));
}

#[test]
fn table_window_stops_before_following_three_row_column_flow() {
    let input = document(
        [
            source_text("Key", Rect { x: 40.0, y: 180.0, width: 40.0, height: 13.0 }, 13.0),
            source_text("Value", Rect { x: 160.0, y: 180.0, width: 40.0, height: 13.0 }, 13.0),
            source_text("A", Rect { x: 40.0, y: 205.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("1", Rect { x: 160.0, y: 205.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("Left one", Rect { x: 40.0, y: 240.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Right one", Rect { x: 350.0, y: 240.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Left two", Rect { x: 40.0, y: 270.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Right two", Rect { x: 350.0, y: 270.0, width: 210.0, height: 12.0 }, 12.0),
            source_text("Left three", Rect { x: 40.0, y: 300.0, width: 210.0, height: 12.0 }, 12.0),
            source_text(
                "Right three",
                Rect { x: 350.0, y: 300.0, width: 210.0, height: 12.0 },
                12.0,
            ),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert_eq!(blocks.iter().filter(|node| matches!(node.block, Block::Table { .. })).count(), 1);
    assert_eq!(block_text(&blocks[1].block), "Left one Left two Left three");
    assert_eq!(block_text(&blocks[2].block), "Right one Right two Right three");
}

#[test]
fn repeated_three_by_three_grid_survives_wide_gutters() {
    let mut inlines = Vec::new();
    inlines.try_reserve_exact(9).unwrap();
    for row in 0..3 {
        for column in 0..3 {
            inlines.extend(source_text(
                ["A", "B", "C"][column],
                Rect {
                    x: 40.0 + column as f32 * 210.0,
                    y: 180.0 + row as f32 * 25.0,
                    width: 40.0,
                    height: 12.0,
                },
                12.0,
            ));
        }
    }
    let actual = rebuild(document(inlines));
    let table = page_blocks(&actual)
        .iter()
        .find_map(|node| match &node.block {
            Block::Table { rows, .. } => Some(rows),
            _ => None,
        })
        .expect("three-column grid");
    assert_eq!((table.len(), table[0].cells.len()), (3, 3));
}

#[test]
fn table_title_remains_a_heading_and_does_not_split_its_table() {
    let input = document(
        [
            source_text(
                "Table title",
                Rect { x: 40.0, y: 100.0, width: 180.0, height: 20.0 },
                20.0,
            ),
            source_text("Key", Rect { x: 40.0, y: 180.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("Value", Rect { x: 360.0, y: 180.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("A", Rect { x: 40.0, y: 205.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("1", Rect { x: 360.0, y: 205.0, width: 40.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert!(matches!(&blocks[0].block, Block::Heading { .. }));
    assert_eq!(block_text(&blocks[0].block), "Table title");
    assert!(matches!(&blocks[1].block, Block::Table { rows, .. } if rows.len() == 2));
}

#[test]
fn one_row_and_non_aligned_wide_gaps_do_not_invent_tables() {
    let one_row = document(
        [
            source_text("Left", Rect { x: 40.0, y: 180.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("Right", Rect { x: 360.0, y: 180.0, width: 40.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    assert!(
        !page_blocks(&rebuild(one_row))
            .iter()
            .any(|node| matches!(node.block, Block::Table { .. }))
    );

    let non_aligned = document(
        [
            source_text("A", Rect { x: 40.0, y: 180.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("1", Rect { x: 360.0, y: 180.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("B", Rect { x: 40.0, y: 205.0, width: 40.0, height: 12.0 }, 12.0),
            source_text("2", Rect { x: 460.0, y: 205.0, width: 40.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    assert!(
        !page_blocks(&rebuild(non_aligned))
            .iter()
            .any(|node| matches!(node.block, Block::Table { .. }))
    );
}

#[test]
fn recovered_tables_obey_the_document_wide_cell_limit_without_a_lease() {
    let input = document(
        [
            source_text("Name", Rect { x: 50.0, y: 180.0, width: 50.0, height: 13.0 }, 13.0),
            source_text("Value", Rect { x: 150.0, y: 180.0, width: 50.0, height: 13.0 }, 13.0),
            source_text("A", Rect { x: 50.0, y: 205.0, width: 20.0, height: 12.0 }, 12.0),
            source_text("1", Rect { x: 150.0, y: 205.0, width: 20.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let mut config = LayoutConfig::default();
    config.limits.max_table_cells = 3;
    let execution = context();
    assert!(matches!(
        reconstruct_document(input, &config, &execution),
        Err(ConversionError::ResourceLimit { limit: "pdfLayoutTableCells", .. })
    ));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

fn wide_table_document(columns: usize) -> Document {
    let page_width = 40.0 + columns as f32 * 3.0;
    let mut inlines = Vec::with_capacity(columns * 2);
    for row in 0..2 {
        for column in 0..columns {
            inlines.push(source(
                "x",
                Rect {
                    x: 20.0 + column as f32 * 3.0,
                    y: 100.0 + row as f32 * 3.0,
                    width: 0.5,
                    height: 1.0,
                },
                1.0,
            ));
        }
    }
    let mut document = document(inlines);
    let page = &mut document.blocks[0];
    page.provenance.locator.page_width = Some(page_width);
    let Block::Page { blocks, .. } = &mut page.block else { unreachable!() };
    for block in blocks {
        block.provenance.locator.page_width = Some(page_width);
        let Block::Paragraph(inlines) = &mut block.block else { unreachable!() };
        for inline in inlines {
            let Inline::SourceText { provenance, .. } = inline else { unreachable!() };
            provenance.locator.page_width = Some(page_width);
        }
    }
    document
}

#[test]
fn table_comparison_limit_has_an_exact_adjacent_boundary() {
    let input = wide_table_document(128);
    let succeeds = |limit| {
        let mut config = LayoutConfig::default();
        config.limits.max_comparisons = limit;
        reconstruct_document(input.clone(), &config, &context()).is_ok()
    };
    let mut low = 1_u64;
    let mut high = LayoutConfig::default().limits.max_comparisons;
    while low < high {
        let middle = low + (high - low) / 2;
        if succeeds(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    assert!(low > 1 && succeeds(low));
    let mut below = LayoutConfig::default();
    below.limits.max_comparisons = low - 1;
    let execution = context();
    assert!(matches!(
        reconstruct_document(input, &below, &execution),
        Err(ConversionError::ResourceLimit { limit: "pdfLayoutComparisons", .. })
    ));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn budgeted_sort_has_an_exact_comparison_boundary() {
    let required = ordering::comparison_charge(32).unwrap() as u64;
    let mut config = LayoutConfig::default();
    config.limits.max_comparisons = required;
    let execution = context();
    let mut budget =
        budget::LayoutBudget::preflight(&Document::default(), &config, &execution).unwrap();
    let mut values = (0..32).rev().collect::<Vec<_>>();
    ordering::by(&mut values, &mut budget, Ord::cmp).unwrap();
    assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
    drop(budget);
    assert_eq!(execution.reserved_memory_bytes(), 0);

    config.limits.max_comparisons = required - 1;
    let execution = context();
    let mut budget =
        budget::LayoutBudget::preflight(&Document::default(), &config, &execution).unwrap();
    let mut values = (0..32).rev().collect::<Vec<_>>();
    assert!(matches!(
        ordering::by(&mut values, &mut budget, Ord::cmp),
        Err(ConversionError::ResourceLimit { limit: "pdfLayoutComparisons", .. })
    ));
    drop(budget);
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn cancellation_before_sort_prevents_comparisons_and_releases_lease() {
    let cancellation = CancellationToken::new();
    let execution = ExecutionContext::new(
        ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let mut budget =
        budget::LayoutBudget::preflight(&Document::default(), &LayoutConfig::default(), &execution)
            .unwrap();
    let compared = Arc::new(AtomicBool::new(false));
    ordering::set_before_sort_hook(Some(Box::new(move || cancellation.cancel())));
    let mut values = (0..1_024).rev().collect::<Vec<_>>();
    let compared_in_sort = Arc::clone(&compared);
    let result = ordering::by(&mut values, &mut budget, move |left, right| {
        compared_in_sort.store(true, AtomicOrdering::Relaxed);
        left.cmp(right)
    });
    ordering::set_before_sort_hook(None);
    assert!(matches!(result, Err(ConversionError::Cancelled)));
    assert!(!compared.load(AtomicOrdering::Relaxed));
    drop(budget);
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn timeout_before_sort_prevents_comparisons_and_releases_lease() {
    let execution = ExecutionContext::new(
        ExecutionOptions { timeout: Some(Duration::from_millis(5)), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let mut budget =
        budget::LayoutBudget::preflight(&Document::default(), &LayoutConfig::default(), &execution)
            .unwrap();
    let compared = Arc::new(AtomicBool::new(false));
    ordering::set_before_sort_hook(Some(Box::new(|| {
        std::thread::sleep(Duration::from_millis(10));
    })));
    let mut values = (0..1_024).rev().collect::<Vec<_>>();
    let compared_in_sort = Arc::clone(&compared);
    let result = ordering::by(&mut values, &mut budget, move |left, right| {
        compared_in_sort.store(true, AtomicOrdering::Relaxed);
        left.cmp(right)
    });
    ordering::set_before_sort_hook(None);
    assert!(matches!(result, Err(ConversionError::Timeout)));
    assert!(!compared.load(AtomicOrdering::Relaxed));
    drop(budget);
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn large_table_materialization_observes_in_flight_cancel_and_releases_lease() {
    let cancellation = CancellationToken::new();
    let execution = ExecutionContext::new(
        ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let mut checkpoints = 0_usize;
    budget::set_checkpoint_hook(Some(Box::new(move || {
        checkpoints += 1;
        if checkpoints == 170 {
            cancellation.cancel();
        }
    })));
    let result =
        reconstruct_document(wide_table_document(3_000), &LayoutConfig::default(), &execution);
    budget::set_checkpoint_hook(None);
    assert!(matches!(result, Err(ConversionError::Cancelled)));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn large_table_materialization_observes_timeout_and_releases_lease() {
    let execution = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(Duration::from_millis(250)),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let mut checkpoints = 0_usize;
    budget::set_checkpoint_hook(Some(Box::new(move || {
        checkpoints += 1;
        if checkpoints == 170 {
            std::thread::sleep(Duration::from_millis(300));
        }
    })));
    let result =
        reconstruct_document(wide_table_document(3_000), &LayoutConfig::default(), &execution);
    budget::set_checkpoint_hook(None);
    assert!(matches!(result, Err(ConversionError::Timeout)));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn native_and_ocr_overlap_is_deduplicated_without_losing_evidence_source() {
    let bounds = Rect { x: 40.0, y: 80.0, width: 100.0, height: 16.0 };
    let mut inlines = source_text("same", bounds, 12.0);
    inlines.push(ocr("same", bounds));
    let actual = rebuild(document(inlines));
    let blocks = page_blocks(&actual);
    assert_eq!(blocks.len(), 1);
    let Block::Paragraph(inlines) = &blocks[0].block else { panic!("paragraph") };
    assert_eq!(inline_text(inlines), "same");
    assert!(matches!(inlines[0], Inline::SourceText { .. }));
}

#[test]
fn page_wide_spatial_dedup_finds_old_overlap_and_preserves_distant_equal_text() {
    let top = Rect { x: 40.0, y: 20.0, width: 40.0, height: 6.0 };
    let mut inlines = source_text("same", top, 12.0);
    for index in 0..80 {
        inlines.extend(source_text(
            &format!("row{index}"),
            Rect { x: 40.0, y: 35.0 + index as f32 * 8.0, width: 45.0, height: 6.0 },
            12.0,
        ));
    }
    inlines.push(ocr("same", top));
    let actual = rebuild(document(inlines));
    let text = page_blocks(&actual)
        .iter()
        .map(|node| block_text(&node.block))
        .collect::<Vec<_>>()
        .join("|");
    assert_eq!(text.matches("same").count(), 1);

    let mut distant = source_text("same", top, 12.0);
    distant.push(ocr("same", Rect { x: 450.0, y: 700.0, width: 40.0, height: 6.0 }));
    let actual = rebuild(document(distant));
    let text = page_blocks(&actual)
        .iter()
        .map(|node| block_text(&node.block))
        .collect::<Vec<_>>()
        .join("|");
    assert_eq!(text.matches("same").count(), 2);

    let across_pages = rebuild(Document {
        blocks: vec![
            page_node(1, source_text("same", top, 12.0)),
            page_node(2, vec![ocr("same", top)]),
        ],
        ..Document::default()
    });
    assert_eq!(
        across_pages
            .blocks
            .iter()
            .map(|page| {
                let Block::Page { blocks, .. } = &page.block else { unreachable!() };
                blocks.iter().map(|node| block_text(&node.block)).collect::<String>()
            })
            .collect::<Vec<_>>(),
        ["same", "same"],
        "deduplication never crosses the page scope"
    );
}

#[test]
fn vertical_orientation_orders_baselines_without_language_inference() {
    let vertical = |value: &str, x: f32| {
        value
            .chars()
            .enumerate()
            .map(|(index, character)| {
                source(
                    &character.to_string(),
                    Rect { x, y: 80.0 + index as f32 * 14.0, width: 12.0, height: 12.0 },
                    12.0,
                )
            })
            .collect::<Vec<_>>()
    };
    let mut right = vertical("right", 450.0);
    let mut left = vertical("left", 400.0);
    for inline in right.iter_mut().chain(&mut left) {
        let Inline::SourceText { provenance, .. } = inline else { unreachable!() };
        provenance.locator.rotation_degrees = Some(90.0);
    }
    left.extend(right);
    let actual = rebuild(document(left));
    let text = page_blocks(&actual).iter().map(|node| block_text(&node.block)).collect::<Vec<_>>();
    assert_eq!(text, ["right", "left"]);
}

#[test]
fn nearby_vertical_lines_form_one_paragraph_in_oriented_reading_order() {
    let vertical = |value: &str, x: f32| {
        value
            .chars()
            .enumerate()
            .map(|(index, character)| {
                let mut inline = source(
                    &character.to_string(),
                    Rect { x, y: 80.0 + index as f32 * 14.0, width: 12.0, height: 12.0 },
                    12.0,
                );
                let Inline::SourceText { provenance, .. } = &mut inline else { unreachable!() };
                provenance.locator.rotation_degrees = Some(90.0);
                inline
            })
            .collect::<Vec<_>>()
    };
    let mut inlines = vertical("first", 450.0);
    inlines.extend(vertical("second", 430.0));
    let actual = rebuild(document(inlines));
    assert_eq!(page_blocks(&actual).len(), 1);
    assert_eq!(block_text(&page_blocks(&actual)[0].block), "first second");
}

#[test]
fn images_share_page_reading_order_with_reconstructed_text() {
    let mut input = document(
        [
            source_text("Before", Rect { x: 40.0, y: 80.0, width: 60.0, height: 12.0 }, 12.0),
            source_text("After", Rect { x: 40.0, y: 240.0, width: 50.0, height: 12.0 }, 12.0),
        ]
        .concat(),
    );
    let Block::Page { blocks, .. } = &mut input.blocks[0].block else { unreachable!() };
    blocks.push(BlockNode {
        id: NodeId("pdf-page-1-image".into()),
        block: Block::Image { asset: AssetId("image".into()), alt: None },
        provenance: provenance(
            1,
            Some(Rect { x: 40.0, y: 150.0, width: 80.0, height: 50.0 }),
            12.0,
            0.0,
        ),
    });
    let actual = rebuild(input);
    let blocks = page_blocks(&actual);
    assert_eq!(blocks.len(), 3);
    assert_eq!(block_text(&blocks[0].block), "Before");
    assert!(matches!(blocks[1].block, Block::Image { .. }));
    assert_eq!(block_text(&blocks[2].block), "After");
}

#[test]
fn repeated_page_edge_matter_is_annotated_without_dropping_source_text() {
    let page = |number, body: &str| {
        page_node(
            number,
            [
                source_text("Report", Rect { x: 40.0, y: 20.0, width: 60.0, height: 12.0 }, 12.0),
                source_text(body, Rect { x: 40.0, y: 200.0, width: 80.0, height: 12.0 }, 12.0),
                source_text(
                    "Confidential",
                    Rect { x: 40.0, y: 760.0, width: 100.0, height: 12.0 },
                    12.0,
                ),
            ]
            .concat(),
        )
    };
    let input = Document { blocks: vec![page(1, "One"), page(2, "Two")], ..Document::default() };
    let actual = rebuild(input);
    for page in &actual.blocks {
        let Block::Page { blocks, .. } = &page.block else { unreachable!() };
        let header = blocks.iter().find(|node| block_text(&node.block) == "Report").unwrap();
        let footer = blocks.iter().find(|node| block_text(&node.block) == "Confidential").unwrap();
        let body = blocks
            .iter()
            .find(|node| matches!(block_text(&node.block).as_str(), "One" | "Two"))
            .unwrap();
        assert_eq!(header.provenance.locator.part.as_deref(), Some("pdf/running-header"));
        assert_eq!(footer.provenance.locator.part.as_deref(), Some("pdf/running-footer"));
        assert_eq!(body.provenance.locator.part, None);
    }
}

fn page_with_legacy_reference_and_footnote(page: u32, note: &str) -> BlockNode {
    let mut inlines = source_text(
        &format!("Body {page}"),
        Rect { x: 40.0, y: 150.0, width: 80.0, height: 12.0 },
        12.0,
    );
    inlines.extend(source_text(
        &format!("1 {note}"),
        Rect { x: 40.0, y: 740.0, width: 120.0, height: 9.0 },
        9.0,
    ));
    let mut output = page_node(page, inlines);
    let Block::Page { blocks, .. } = &mut output.block else { unreachable!() };
    blocks.push(BlockNode {
        id: NodeId(format!("pdf-page-{page}-reference")),
        block: Block::Paragraph(vec![
            Inline::Text { value: "See".into(), marks: Vec::new() },
            Inline::FootnoteReference("pdf-1".into()),
        ]),
        provenance: provenance(
            page,
            Some(Rect { x: 40.0, y: 100.0, width: 30.0, height: 12.0 }),
            12.0,
            0.0,
        ),
    });
    output
}

#[test]
fn repeated_footnote_numbers_are_page_scoped_and_references_remain_valid() {
    let input = Document {
        blocks: vec![
            page_with_legacy_reference_and_footnote(1, "First note"),
            page_with_legacy_reference_and_footnote(2, "Second note"),
        ],
        ..Document::default()
    };
    let actual = rebuild(input);
    let mut labels = Vec::new();
    let mut references = Vec::new();
    let mut definitions = Vec::new();
    for page in &actual.blocks {
        let Block::Page { blocks, .. } = &page.block else { unreachable!() };
        for node in blocks {
            match &node.block {
                Block::Footnote { label, blocks } => {
                    labels.push(label.clone());
                    definitions.push(block_text(&blocks[0].block));
                }
                Block::Paragraph(inlines) => {
                    references.extend(inlines.iter().filter_map(|inline| match inline {
                        Inline::FootnoteReference(label) => Some(label.clone()),
                        _ => None,
                    }));
                }
                _ => {}
            }
        }
    }
    assert_eq!(labels, ["pdf-page-1-1", "pdf-page-2-1"]);
    assert_eq!(references, labels);
    assert_eq!(definitions, ["First note", "Second note"]);
    assert_eq!(rebuild(actual.clone()), actual, "single-page labels stay stable on relayout");
}

#[test]
fn a_single_page_footnote_label_is_exact_and_stable() {
    let actual = rebuild(Document {
        blocks: vec![page_with_legacy_reference_and_footnote(1, "Only note")],
        ..Document::default()
    });
    let blocks = page_blocks(&actual);
    let definition = blocks
        .iter()
        .find_map(|node| match &node.block {
            Block::Footnote { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .unwrap();
    let reference = blocks
        .iter()
        .filter_map(|node| match &node.block {
            Block::Paragraph(inlines) => Some(inlines),
            _ => None,
        })
        .flatten()
        .find_map(|inline| match inline {
            Inline::FootnoteReference(label) => Some(label.as_str()),
            _ => None,
        })
        .unwrap();
    assert_eq!(definition, "pdf-page-1-1");
    assert_eq!(reference, definition);
    assert_eq!(rebuild(actual.clone()), actual);
}

#[test]
fn layout_preflight_has_an_exact_memory_boundary_and_bounded_work_failures() {
    let input =
        document(source_text("memory", Rect { x: 40.0, y: 80.0, width: 60.0, height: 12.0 }, 12.0));
    let measuring = context();
    let measured =
        reconstruct_document(input.clone(), &LayoutConfig::default(), &measuring).unwrap();
    let required = measuring.reserved_memory_bytes();
    assert!(required > 0);
    drop(measured);
    assert_eq!(measuring.reserved_memory_bytes(), 0);

    let low = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: required - 1, ..ResourceLimits::default() },
    );
    assert!(matches!(
        reconstruct_document(input.clone(), &LayoutConfig::default(), &low),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(low.reserved_memory_bytes(), 0);

    let exact = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: required, ..ResourceLimits::default() },
    );
    let output = reconstruct_document(input, &LayoutConfig::default(), &exact).unwrap();
    assert_eq!(exact.reserved_memory_bytes(), required);
    drop(output);
    assert_eq!(exact.reserved_memory_bytes(), 0);

    let mut comparison_config = LayoutConfig::default();
    comparison_config.limits.max_comparisons = 1;
    let comparison_input = document(source_text(
        "comparison",
        Rect { x: 40.0, y: 80.0, width: 110.0, height: 12.0 },
        12.0,
    ));
    let execution = context();
    assert!(matches!(
        reconstruct_document(comparison_input, &comparison_config, &execution),
        Err(ConversionError::ResourceLimit { limit: "pdfLayoutComparisons", .. })
    ));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn outside_page_geometry_fails_without_a_publishable_document_or_lease() {
    let input = document(source_text(
        "outside",
        Rect { x: 590.0, y: 20.0, width: 20.0, height: 12.0 },
        12.0,
    ));
    let execution = context();
    assert!(matches!(
        reconstruct_document(input, &LayoutConfig::default(), &execution),
        Err(ConversionError::Malformed { .. })
    ));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn cancellation_during_large_traversal_releases_the_layout_lease() {
    let cancellation = CancellationToken::new();
    let execution = ExecutionContext::new(
        ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let inlines = (0..600)
        .map(|index| {
            source(
                "x",
                Rect {
                    x: (index % 50) as f32 * 10.0,
                    y: (index / 50) as f32 * 15.0,
                    width: 5.0,
                    height: 10.0,
                },
                12.0,
            )
        })
        .collect();
    let mut fired = false;
    budget::set_checkpoint_hook(Some(Box::new(move || {
        if !fired {
            fired = true;
            cancellation.cancel();
        }
    })));
    let result = reconstruct_document(document(inlines), &LayoutConfig::default(), &execution);
    budget::set_checkpoint_hook(None);
    assert!(matches!(result, Err(ConversionError::Cancelled)));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn timeout_during_large_traversal_releases_the_layout_lease() {
    let execution = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(Duration::from_millis(25)),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let inlines = (0..600)
        .map(|index| {
            source(
                "x",
                Rect {
                    x: (index % 50) as f32 * 10.0,
                    y: (index / 50) as f32 * 15.0,
                    width: 5.0,
                    height: 10.0,
                },
                12.0,
            )
        })
        .collect();
    let mut fired = false;
    budget::set_checkpoint_hook(Some(Box::new(move || {
        if !fired {
            fired = true;
            std::thread::sleep(Duration::from_millis(35));
        }
    })));
    let result = reconstruct_document(document(inlines), &LayoutConfig::default(), &execution);
    budget::set_checkpoint_hook(None);
    assert!(matches!(result, Err(ConversionError::Timeout)));
    assert_eq!(execution.reserved_memory_bytes(), 0);
}
