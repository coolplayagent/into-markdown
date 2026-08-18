use super::*;
use crate::{
    AssetId, CancellationToken, Cell, DocumentMetadata, ExecutionOptions, NodeId, Provenance,
    ProvenanceKind, ResourceLimits, SourceLocator, TableRow,
};

fn provenance(bounds: Option<Rect>) -> Provenance {
    Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "layout-quality-test".into(),
        locator: SourceLocator { bounds, page: Some(1), ..SourceLocator::default() },
        confidence: None,
    }
}

fn node(id: &str, block: Block, bounds: Option<Rect>) -> BlockNode {
    BlockNode { id: NodeId(id.into()), block, provenance: provenance(bounds) }
}

fn paragraph(id: &str, text: &str, y: f32) -> BlockNode {
    node(
        id,
        Block::Paragraph(vec![Inline::Text { value: text.into(), marks: Vec::new() }]),
        Some(Rect { x: 10.0, y, width: 100.0, height: 12.0 }),
    )
}

fn table(id: &str) -> BlockNode {
    node(
        id,
        Block::Table {
            rows: vec![TableRow {
                cells: vec![
                    Cell {
                        row_span: 1,
                        column_span: 1,
                        header: true,
                        blocks: vec![paragraph("cell-a", "A", 20.0)],
                    },
                    Cell {
                        row_span: 1,
                        column_span: 2,
                        header: true,
                        blocks: vec![paragraph("cell-b", "B", 20.0)],
                    },
                ],
            }],
            alignments: Vec::new(),
        },
        Some(Rect { x: 10.0, y: 20.0, width: 200.0, height: 40.0 }),
    )
}

fn document(children: Vec<BlockNode>) -> Document {
    Document {
        schema_version: crate::DOCUMENT_SCHEMA_VERSION,
        metadata: DocumentMetadata::default(),
        blocks: vec![node("page-1", Block::Page { number: 1, blocks: children }, None)],
    }
}

fn image_asset(id: &str) -> Asset {
    Asset {
        id: AssetId(id.into()),
        filename: Some(format!("{id}.png")),
        media_type: "image/png".into(),
        bytes: vec![1, 2, 3],
        external_uri: None,
    }
}

fn config() -> LayoutQualityConfig {
    LayoutQualityConfig {
        coordinate_tolerance: 0.01,
        minimum_precision: 0.95,
        minimum_recall: 0.95,
    }
}

#[test]
fn exact_layout_is_deterministic_and_retains_lease() {
    let document = document(vec![paragraph("one", "First", 10.0), table("table")]);
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let first =
        audit_semantic_layout("fixture", &document, &[], &document, &[], config(), &context)
            .unwrap();
    assert!(first.report().passed());
    let json = first.to_json().unwrap();
    assert_eq!(
        json,
        r#"{"fixture":"fixture","metrics":{"precision":1.0,"recall":1.0,"truePositive":5,"falsePositive":0,"falseNegative":0},"minimumPrecision":0.95,"minimumRecall":0.95,"differences":[]}"#
    );
    assert!(context.reserved_memory_bytes() > 0);
    drop(first);
    assert_eq!(context.reserved_memory_bytes(), 0);
    let second =
        audit_semantic_layout("fixture", &document, &[], &document, &[], config(), &context)
            .unwrap();
    assert_eq!(second.to_json().unwrap(), json);
}

#[test]
fn report_locates_order_hierarchy_topology_geometry_and_resources() {
    let image = node(
        "image",
        Block::Image { asset: AssetId("golden-image".into()), alt: Some("figure".into()) },
        Some(Rect { x: 1.0, y: 1.0, width: 10.0, height: 10.0 }),
    );
    let golden = document(vec![paragraph("one", "First", 10.0), table("table"), image]);
    let mut wrong_table = table("table");
    if let Block::Table { rows, .. } = &mut wrong_table.block {
        rows[0].cells[0].column_span = 2;
    }
    let observed = document(vec![
        wrong_table,
        paragraph("one", "Changed", 11.0),
        node(
            "image",
            Block::Image { asset: AssetId("wrong-image".into()), alt: Some("figure".into()) },
            None,
        ),
    ]);
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let audit = audit_semantic_layout(
        "complex",
        &observed,
        &[image_asset("orphan")],
        &golden,
        &[image_asset("golden-image")],
        config(),
        &context,
    )
    .unwrap();
    let kinds = audit
        .report()
        .differences
        .iter()
        .map(|difference| difference.kind)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(kinds.contains(&LayoutDiffKind::OutOfOrder));
    assert!(kinds.contains(&LayoutDiffKind::WrongHierarchy));
    assert!(kinds.contains(&LayoutDiffKind::TableTopology));
    assert!(kinds.contains(&LayoutDiffKind::Geometry));
    assert!(kinds.contains(&LayoutDiffKind::ResourceAssociation));
    assert!(!audit.report().passed());
}

#[test]
fn duplicate_ids_are_reported_instead_of_publishing_partial_success() {
    let golden = document(vec![paragraph("one", "First", 10.0)]);
    let actual = document(vec![paragraph("one", "First", 10.0), paragraph("one", "First", 10.0)]);
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let audit =
        audit_semantic_layout("duplicate", &actual, &[], &golden, &[], config(), &context).unwrap();
    assert!(
        audit
            .report()
            .differences
            .iter()
            .any(|difference| difference.kind == LayoutDiffKind::Duplicate)
    );
}

#[test]
fn tolerance_has_a_passing_boundary_and_a_failing_counterexample() {
    let golden = document(vec![paragraph("one", "First", 10.0)]);
    let mut within = golden.clone();
    let Block::Page { blocks, .. } = &mut within.blocks[0].block else { unreachable!() };
    blocks[0].provenance.locator.bounds.as_mut().unwrap().y += 0.009;
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let passing =
        audit_semantic_layout("within", &within, &[], &golden, &[], config(), &context).unwrap();
    assert!(passing.report().differences.is_empty());
    drop(passing);
    let Block::Page { blocks, .. } = &mut within.blocks[0].block else { unreachable!() };
    blocks[0].provenance.locator.bounds.as_mut().unwrap().y += 0.002;
    let failing =
        audit_semantic_layout("outside", &within, &[], &golden, &[], config(), &context).unwrap();
    assert!(
        failing
            .report()
            .differences
            .iter()
            .any(|difference| difference.kind == LayoutDiffKind::Geometry)
    );
}

#[test]
fn cancellation_and_memory_limits_leave_no_lease() {
    let document = document(vec![paragraph("one", "First", 10.0)]);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = ExecutionContext::new(
        ExecutionOptions { cancellation, timeout: None, progress_listener: None },
        ResourceLimits::default(),
    );
    assert!(matches!(
        audit_semantic_layout("cancelled", &document, &[], &document, &[], config(), &cancelled),
        Err(ConversionError::Cancelled)
    ));
    assert_eq!(cancelled.reserved_memory_bytes(), 0);

    let limits = ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() };
    let limited = ExecutionContext::new(ExecutionOptions::default(), limits);
    assert!(matches!(
        audit_semantic_layout("limited", &document, &[], &document, &[], config(), &limited),
        Err(ConversionError::ResourceLimit { .. })
    ));
    assert_eq!(limited.reserved_memory_bytes(), 0);
}

#[test]
fn invalid_numeric_authority_is_rejected() {
    let document = document(Vec::new());
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let invalid = LayoutQualityConfig { coordinate_tolerance: f32::NAN, ..config() };
    assert!(
        audit_semantic_layout("invalid", &document, &[], &document, &[], invalid, &context)
            .is_err()
    );
}
