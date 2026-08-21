use super::*;
use into_markdown_core::{
    AssetId, CancellationToken, Cell, ConversionError, ExecutionOptions, Inline, ListItem,
    ListKind, NodeId, Provenance, ProvenanceKind, Rect, ResourceLimits, SourceLocator, TableRow,
};
use std::time::Duration;

#[test]
fn complete_mixed_document_passes_and_report_owns_memory_until_drop() {
    let (document, assets) = mixed_document();
    let context = context(ResourceLimits::default(), ExecutionOptions::default());
    let authority = authority(&document, &assets, "stable gfm", &context);
    assert_eq!(context.reserved_memory_bytes(), 0);
    let report = audit(&authority, &document, &assets, "stable gfm", &context).unwrap();
    assert!(report.passed, "{:?}", report.diffs);
    assert_eq!(report.metrics.precision_basis_points, 10_000);
    assert_eq!(report.metrics.recall_basis_points, 10_000);
    assert!(report.retained_memory_is_accounted());
    assert!(context.reserved_memory_bytes() > 0);
    drop(report);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn deterministic_diff_covers_every_structural_category() {
    let (document, assets) = mixed_document();
    let context = context(ResourceLimits::default(), ExecutionOptions::default());
    let mut authority = authority(&document, &assets, "stable gfm", &context);
    let mut actual = authority.snapshot.clone();

    actual.nodes[1].text.push_str(" changed");
    actual.nodes[1].order += 1;
    actual.nodes[1].parent_id = None;
    actual.nodes[1].boundary.page = Some(9);
    actual.nodes[1].bounds.as_mut().unwrap().x_milli += 2;
    actual.nodes[1].source_chain[0].provider = "wrong-provider".into();
    let table = actual.nodes.iter_mut().find(|node| node.kind == "table").unwrap();
    table.table.as_mut().unwrap().cells[0].column_span = 9;
    let image = actual.nodes.iter_mut().find(|node| node.kind == "image").unwrap();
    image.references[0].target = "missing-asset".into();
    let removed = actual.nodes.remove(2);
    let mut unexpected = removed.clone();
    unexpected.id = "unexpected".into();
    actual.nodes.push(unexpected);
    actual.nodes.push(actual.nodes[0].clone());

    let (_, diffs) = quality_metrics::compare(&authority, &actual, &context).unwrap();
    let kinds = diffs.iter().map(|diff| diff.kind).collect::<std::collections::BTreeSet<_>>();
    for expected in [
        DiffKind::Missing,
        DiffKind::Duplicate,
        DiffKind::Unexpected,
        DiffKind::Order,
        DiffKind::Content,
        DiffKind::Hierarchy,
        DiffKind::Boundary,
        DiffKind::Geometry,
        DiffKind::TableTopology,
        DiffKind::ResourceAssociation,
        DiffKind::SourceChain,
    ] {
        assert!(kinds.contains(&expected), "missing {expected:?}: {diffs:#?}");
    }

    authority.snapshot = actual;
    assert_ne!(
        authority.snapshot.nodes,
        project(&document, &assets, &context).unwrap().snapshot.nodes
    );
}

#[test]
fn geometry_tolerance_accepts_declared_drift_and_rejects_counterexample() {
    let (document, assets) = mixed_document();
    let context = context(ResourceLimits::default(), ExecutionOptions::default());
    let mut authority = authority(&document, &assets, "stable gfm", &context);
    authority.geometry_tolerance_milli = 2;
    authority.snapshot.nodes[1].bounds.as_mut().unwrap().x_milli += 2;
    let report = audit(&authority, &document, &assets, "stable gfm", &context).unwrap();
    assert!(report.passed, "{:?}", report.diffs);
    drop(report);

    authority.snapshot.nodes[1].bounds.as_mut().unwrap().x_milli += 1;
    let report = audit(&authority, &document, &assets, "stable gfm", &context).unwrap();
    assert!(!report.passed);
    assert!(report.diffs.iter().any(|diff| diff.kind == DiffKind::Geometry));

    assert!(!geometry::within_tolerance(
        Some(NormalizedBounds { x_milli: i64::MIN, y_milli: 0, width_milli: 0, height_milli: 0 }),
        Some(NormalizedBounds { x_milli: i64::MAX, y_milli: 0, width_milli: 0, height_milli: 0 }),
        u32::MAX,
    ));
}

#[test]
fn ir_gfm_and_threshold_regressions_are_explicit() {
    let (document, assets) = mixed_document();
    let context = context(ResourceLimits::default(), ExecutionOptions::default());
    let mut authority = authority(&document, &assets, "stable gfm", &context);
    authority.ir_sha256 = "0".repeat(64);
    authority.gfm_sha256 = "1".repeat(64);
    authority.snapshot.nodes.truncate(1);
    let report = audit(&authority, &document, &assets, "different gfm", &context).unwrap();
    for kind in [DiffKind::IrGolden, DiffKind::GfmGolden, DiffKind::Threshold] {
        assert!(report.diffs.iter().any(|diff| diff.kind == kind), "{:?}", report.diffs);
    }
}

#[test]
fn cancellation_timeout_depth_work_and_memory_fail_without_a_report_or_lease() {
    let (document, assets) = mixed_document();

    let token = CancellationToken::new();
    token.cancel();
    let options = ExecutionOptions { cancellation: token, ..ExecutionOptions::default() };
    assert_failed_without_lease(&document, &assets, options, ResourceLimits::default(), |error| {
        matches!(error, ConversionError::Cancelled)
    });

    let options = ExecutionOptions { timeout: Some(Duration::ZERO), ..ExecutionOptions::default() };
    assert_failed_without_lease(&document, &assets, options, ResourceLimits::default(), |error| {
        matches!(error, ConversionError::Timeout)
    });

    let limits = ResourceLimits { max_nesting_depth: 1, ..ResourceLimits::default() };
    assert_failed_without_lease(&document, &assets, ExecutionOptions::default(), limits, |error| {
        matches!(error, ConversionError::ResourceLimit { limit: "max_nesting_depth", .. })
    });

    let limits = ResourceLimits { max_table_cells: 2, ..ResourceLimits::default() };
    assert_failed_without_lease(&document, &assets, ExecutionOptions::default(), limits, |error| {
        matches!(error, ConversionError::ResourceLimit { limit: "semantic_layout_work", .. })
    });

    let mut inline_document = Document::default();
    inline_document.blocks.push(node(
        "many-inlines",
        Block::Paragraph(vec![Inline::LineBreak, Inline::LineBreak, Inline::LineBreak]),
        SourceLocator::default(),
    ));
    let limits = ResourceLimits { max_table_cells: 3, ..ResourceLimits::default() };
    let inline_context = context(limits, ExecutionOptions::default());
    let error = project(&inline_document, &[], &inline_context).unwrap_err();
    assert!(matches!(error, ConversionError::ResourceLimit { limit: "semantic_layout_work", .. }));
    assert_eq!(inline_context.reserved_memory_bytes(), 0);

    let mut authority_document = Document::default();
    authority_document.blocks.push(node(
        "authority-work",
        Block::Paragraph(text("bounded")),
        SourceLocator::default(),
    ));
    let authority_context = context(ResourceLimits::default(), ExecutionOptions::default());
    let mut oversized_authority = authority(&authority_document, &[], "gfm", &authority_context);
    oversized_authority.snapshot.nodes.extend([
        oversized_authority.snapshot.nodes[0].clone(),
        oversized_authority.snapshot.nodes[0].clone(),
    ]);
    let limits = ResourceLimits { max_table_cells: 4, ..ResourceLimits::default() };
    let comparison_context = context(limits, ExecutionOptions::default());
    let error = audit(&oversized_authority, &authority_document, &[], "gfm", &comparison_context)
        .unwrap_err();
    assert!(matches!(error, ConversionError::ResourceLimit { limit: "semantic_layout_work", .. }));
    assert_eq!(comparison_context.reserved_memory_bytes(), 0);

    let limits = ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() };
    assert_failed_without_lease(&document, &assets, ExecutionOptions::default(), limits, |error| {
        matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    });
}

#[test]
fn malformed_authority_is_rejected_before_reservation() {
    let (document, assets) = mixed_document();
    let context = context(ResourceLimits::default(), ExecutionOptions::default());
    let mut authority = authority(&document, &assets, "gfm", &context);
    authority.schema_version += 1;
    let error = audit(&authority, &document, &assets, "gfm", &context).unwrap_err();
    assert!(matches!(error, ConversionError::Malformed { .. }));
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn malformed_geometry_and_table_spans_fail_without_partial_projection() {
    let mut geometry_document = Document::default();
    geometry_document.blocks.push(node(
        "negative-geometry",
        Block::Paragraph(text("invalid")),
        bounds(0.0, 0.0, -1.0, 1.0),
    ));
    let geometry_context = context(ResourceLimits::default(), ExecutionOptions::default());
    let error = project(&geometry_document, &[], &geometry_context).unwrap_err();
    assert!(matches!(error, ConversionError::Malformed { .. }));
    assert_eq!(geometry_context.reserved_memory_bytes(), 0);

    let mut table_document = Document::default();
    table_document.blocks.push(node(
        "out-of-grid-table",
        Block::Table {
            rows: vec![TableRow {
                cells: vec![Cell { row_span: 2, column_span: 1, header: false, blocks: vec![] }],
            }],
            alignments: vec![],
        },
        SourceLocator::default(),
    ));
    let table_context = context(ResourceLimits::default(), ExecutionOptions::default());
    let error = project(&table_document, &[], &table_context).unwrap_err();
    assert!(matches!(error, ConversionError::Malformed { .. }));
    assert_eq!(table_context.reserved_memory_bytes(), 0);
}

#[test]
fn table_topology_places_spans_and_nested_blocks_on_the_logical_grid() {
    let (document, assets) = mixed_document();
    let context = context(ResourceLimits::default(), ExecutionOptions::default());
    let projection = project(&document, &assets, &context).unwrap();
    let snapshot = projection.snapshot();
    let topology = snapshot
        .nodes
        .iter()
        .find(|node| node.kind == "table")
        .and_then(|node| node.table.as_ref())
        .unwrap();
    assert_eq!((topology.rows, topology.columns), (2, 2));
    assert_eq!(topology.cells[0].column_span, 2);
    assert_eq!(topology.cells[0].block_ids, ["cell-title"]);
    assert_eq!((topology.cells[2].row, topology.cells[2].column), (1, 1));
}

#[test]
fn projection_retains_memory_and_unique_attachment_labels_bind_exactly() {
    let mut document = Document::default();
    document.blocks.push(node(
        "attachment-label",
        Block::Paragraph(text("notes.txt")),
        SourceLocator::default(),
    ));
    let assets = vec![Asset {
        id: AssetId("attachment-1".into()),
        filename: Some("notes.txt".into()),
        media_type: "text/plain".into(),
        bytes: b"notes".to_vec(),
        external_uri: None,
    }];
    let context = context(ResourceLimits::default(), ExecutionOptions::default());
    let projection = project(&document, &assets, &context).unwrap();
    assert!(projection.retained_memory_is_accounted());
    assert!(context.reserved_memory_bytes() > 0);
    assert_eq!(projection.snapshot().nodes[0].references[0].kind, "attachment");
    assert!(projection.snapshot().assets[0].referenced);
    drop(projection);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn ambiguous_attachment_filenames_remain_unbound_counterexamples() {
    let mut document = Document::default();
    document.blocks.push(node(
        "attachment-label",
        Block::Paragraph(text("same.txt")),
        SourceLocator::default(),
    ));
    let assets = ["attachment-1", "attachment-2"]
        .into_iter()
        .map(|id| Asset {
            id: AssetId(id.into()),
            filename: Some("same.txt".into()),
            media_type: "text/plain".into(),
            bytes: vec![],
            external_uri: None,
        })
        .collect::<Vec<_>>();
    let context = context(ResourceLimits::default(), ExecutionOptions::default());
    let projection = project(&document, &assets, &context).unwrap();
    assert!(projection.snapshot().nodes[0].references.is_empty());
    assert!(projection.snapshot().assets.iter().all(|asset| !asset.referenced));
}

fn assert_failed_without_lease(
    document: &Document,
    assets: &[Asset],
    options: ExecutionOptions,
    limits: ResourceLimits,
    predicate: impl FnOnce(&ConversionError) -> bool,
) {
    let authority_context = context(ResourceLimits::default(), ExecutionOptions::default());
    let authority = authority(document, assets, "gfm", &authority_context);
    let context = context(limits, options);
    let error = audit(&authority, document, assets, "gfm", &context).unwrap_err();
    assert!(predicate(&error), "unexpected error: {error:?}");
    assert_eq!(context.reserved_memory_bytes(), 0);
}

fn authority(
    document: &Document,
    assets: &[Asset],
    gfm: &str,
    context: &ExecutionContext,
) -> FixtureAuthority {
    FixtureAuthority {
        schema_version: AUTHORITY_SCHEMA_VERSION,
        fixture_id: "mixed-layout".into(),
        format: "synthetic-cross-format-counterexample".into(),
        cohort: QualityCohort::Modern,
        geometry_tolerance_milli: 0,
        snapshot: project(document, assets, context).unwrap().into_authority_snapshot(),
        ir_sha256: hash_ir(document, context).unwrap(),
        gfm_sha256: hex(Sha256::digest(gfm.as_bytes())),
    }
}

fn context(limits: ResourceLimits, options: ExecutionOptions) -> ExecutionContext {
    ExecutionContext::new(options, limits)
}

#[allow(
    clippy::too_many_lines,
    reason = "one readable fixture keeps every semantic relationship visible to the tests"
)]
fn mixed_document() -> (Document, Vec<Asset>) {
    let heading = node(
        "heading",
        Block::Heading { level: 1, content: text("Title") },
        bounds(10.0, 10.0, 100.0, 20.0),
    );
    let paragraph = node(
        "paragraph",
        Block::Paragraph(vec![
            Inline::Text { value: "Body ".into(), marks: vec![] },
            Inline::FootnoteReference("note-1".into()),
        ]),
        bounds(10.0, 40.0, 200.0, 20.0),
    );
    let list = node(
        "list",
        Block::List {
            kind: ListKind::Ordered,
            start: 3,
            items: vec![ListItem {
                checked: None,
                marker_label: Some("3.".into()),
                blocks: vec![node(
                    "list-item",
                    Block::Paragraph(text("Nested item")),
                    SourceLocator::default(),
                )],
            }],
        },
        SourceLocator::default(),
    );
    let table = node(
        "table",
        Block::Table {
            rows: vec![
                TableRow {
                    cells: vec![Cell {
                        row_span: 1,
                        column_span: 2,
                        header: true,
                        blocks: vec![node(
                            "cell-title",
                            Block::Paragraph(text("Merged heading")),
                            SourceLocator::default(),
                        )],
                    }],
                },
                TableRow {
                    cells: vec![
                        Cell {
                            row_span: 1,
                            column_span: 1,
                            header: false,
                            blocks: vec![node(
                                "cell-a",
                                Block::Paragraph(text("A")),
                                SourceLocator::default(),
                            )],
                        },
                        Cell {
                            row_span: 1,
                            column_span: 1,
                            header: false,
                            blocks: vec![node(
                                "cell-b",
                                Block::Paragraph(text("B")),
                                SourceLocator::default(),
                            )],
                        },
                    ],
                },
            ],
            alignments: vec![],
        },
        SourceLocator::default(),
    );
    let image = node(
        "image",
        Block::Image { asset: AssetId("figure-1".into()), alt: Some("Architecture".into()) },
        SourceLocator::default(),
    );
    let footnote = node(
        "footnote",
        Block::Footnote {
            label: "note-1".into(),
            blocks: vec![node(
                "footnote-body",
                Block::Paragraph(text("Source note")),
                SourceLocator::default(),
            )],
        },
        SourceLocator::default(),
    );
    let page = node(
        "page-1",
        Block::Page { number: 1, blocks: vec![heading, paragraph, list, table, image, footnote] },
        SourceLocator {
            page: Some(1),
            part: Some("word/document.xml".into()),
            ..default_locator()
        },
    );
    let mut document = Document::default();
    document.blocks.push(page);
    let assets = vec![Asset {
        id: AssetId("figure-1".into()),
        filename: Some("figure.png".into()),
        media_type: "image/png".into(),
        bytes: vec![1, 2, 3],
        external_uri: None,
    }];
    (document, assets)
}

fn text(value: &str) -> Vec<Inline> {
    vec![Inline::Text { value: value.into(), marks: vec![] }]
}

fn node(id: &str, block: Block, locator: SourceLocator) -> BlockNode {
    BlockNode {
        id: NodeId(id.into()),
        block,
        provenance: Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "test-parser".into(),
            locator,
            confidence: Some(1.0),
        },
    }
}

fn bounds(x: f32, y: f32, width: f32, height: f32) -> SourceLocator {
    SourceLocator {
        page: Some(1),
        bounds: Some(Rect { x, y, width, height }),
        page_width: Some(600.0),
        page_height: Some(800.0),
        ..default_locator()
    }
}

fn default_locator() -> SourceLocator {
    SourceLocator::default()
}
