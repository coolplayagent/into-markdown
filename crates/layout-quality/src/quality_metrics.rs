use crate::geometry;
use crate::{
    AssetSnapshot, DiffKind, FixtureAuthority, QualityDiff, QualityMetrics, SemanticNode,
    SemanticSnapshot,
};
use into_markdown_core::{ConversionError, ExecutionContext};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn compare(
    authority: &FixtureAuthority,
    actual: &SemanticSnapshot,
    context: &ExecutionContext,
) -> Result<(QualityMetrics, Vec<QualityDiff>), ConversionError> {
    let fixture = authority.fixture_id.as_str();
    let mut diffs = Vec::new();
    duplicate_nodes(fixture, &authority.snapshot.nodes, "authority", &mut diffs, context)?;
    duplicate_nodes(fixture, &actual.nodes, "actual", &mut diffs, context)?;
    duplicate_assets(fixture, &authority.snapshot.assets, "authority", &mut diffs, context)?;
    duplicate_assets(fixture, &actual.assets, "actual", &mut diffs, context)?;

    let expected_by_id = first_nodes(&authority.snapshot.nodes, context)?;
    let actual_by_id = first_nodes(&actual.nodes, context)?;
    for expected in &authority.snapshot.nodes {
        context.checkpoint()?;
        let Some(actual) = actual_by_id.get(expected.id.as_str()).copied() else {
            diffs.push(diff(
                DiffKind::Missing,
                fixture,
                Some(&expected.id),
                location(expected),
                Some(expected.kind.clone()),
                None,
            ));
            continue;
        };
        compare_node(authority, expected, actual, &mut diffs);
    }
    for actual in &actual.nodes {
        context.checkpoint()?;
        if !expected_by_id.contains_key(actual.id.as_str()) {
            diffs.push(diff(
                DiffKind::Unexpected,
                fixture,
                Some(&actual.id),
                location(actual),
                None,
                Some(actual.kind.clone()),
            ));
        }
    }
    compare_assets(authority, actual, &mut diffs, context)?;
    validate_actual_references(fixture, actual, &mut diffs, context)?;

    diffs.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.location.cmp(&right.location))
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    Ok((metrics(&authority.snapshot.nodes, &actual.nodes, context)?, diffs))
}

fn compare_node(
    authority: &FixtureAuthority,
    expected: &SemanticNode,
    actual: &SemanticNode,
    diffs: &mut Vec<QualityDiff>,
) {
    let fixture = authority.fixture_id.as_str();
    if expected.kind != actual.kind || expected.text != actual.text {
        diffs.push(diff(
            DiffKind::Content,
            fixture,
            Some(&expected.id),
            location(actual),
            Some(format!("{}:{}", expected.kind, compact(&expected.text))),
            Some(format!("{}:{}", actual.kind, compact(&actual.text))),
        ));
    }
    if expected.order != actual.order {
        diffs.push(diff(
            DiffKind::Order,
            fixture,
            Some(&expected.id),
            location(actual),
            Some(expected.order.to_string()),
            Some(actual.order.to_string()),
        ));
    }
    if expected.parent_id != actual.parent_id
        || expected.sibling_order != actual.sibling_order
        || expected.depth != actual.depth
    {
        diffs.push(diff(
            DiffKind::Hierarchy,
            fixture,
            Some(&expected.id),
            location(actual),
            Some(format!(
                "parent={:?},sibling={},depth={}",
                expected.parent_id, expected.sibling_order, expected.depth
            )),
            Some(format!(
                "parent={:?},sibling={},depth={}",
                actual.parent_id, actual.sibling_order, actual.depth
            )),
        ));
    }
    if expected.boundary != actual.boundary {
        diffs.push(diff(
            DiffKind::Boundary,
            fixture,
            Some(&expected.id),
            location(actual),
            Some(boundary_summary(&expected.boundary)),
            Some(boundary_summary(&actual.boundary)),
        ));
    }
    if !geometry::within_tolerance(
        expected.bounds,
        actual.bounds,
        authority.geometry_tolerance_milli,
    ) {
        diffs.push(diff(
            DiffKind::Geometry,
            fixture,
            Some(&expected.id),
            location(actual),
            Some(format!("{:?}", expected.bounds)),
            Some(format!("{:?}", actual.bounds)),
        ));
    }
    if expected.table != actual.table {
        diffs.push(diff(
            DiffKind::TableTopology,
            fixture,
            Some(&expected.id),
            location(actual),
            Some(table_summary(expected.table.as_ref())),
            Some(table_summary(actual.table.as_ref())),
        ));
    }
    if expected.references != actual.references {
        diffs.push(diff(
            DiffKind::ResourceAssociation,
            fixture,
            Some(&expected.id),
            location(actual),
            Some(reference_summary(&expected.references)),
            Some(reference_summary(&actual.references)),
        ));
    }
    if expected.source_chain != actual.source_chain {
        diffs.push(diff(
            DiffKind::SourceChain,
            fixture,
            Some(&expected.id),
            location(actual),
            Some(chain_summary(&expected.source_chain)),
            Some(chain_summary(&actual.source_chain)),
        ));
    }
}

fn compare_assets(
    authority: &FixtureAuthority,
    actual: &SemanticSnapshot,
    diffs: &mut Vec<QualityDiff>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let expected = authority
        .snapshot
        .assets
        .iter()
        .map(|asset| (asset.id.as_str(), asset))
        .collect::<BTreeMap<_, _>>();
    let actual_by_id =
        actual.assets.iter().map(|asset| (asset.id.as_str(), asset)).collect::<BTreeMap<_, _>>();
    for (id, expected) in expected {
        context.checkpoint()?;
        match actual_by_id.get(id) {
            None => diffs.push(diff(
                DiffKind::Missing,
                &authority.fixture_id,
                Some(id),
                format!("asset:{id}"),
                Some(asset_summary(expected)),
                None,
            )),
            Some(actual) if *actual != expected => diffs.push(diff(
                DiffKind::ResourceAssociation,
                &authority.fixture_id,
                Some(id),
                format!("asset:{id}"),
                Some(asset_summary(expected)),
                Some(asset_summary(actual)),
            )),
            Some(_) => {}
        }
    }
    let expected_ids =
        authority.snapshot.assets.iter().map(|asset| asset.id.as_str()).collect::<BTreeSet<_>>();
    for asset in &actual.assets {
        context.checkpoint()?;
        if !expected_ids.contains(asset.id.as_str()) {
            diffs.push(diff(
                DiffKind::Unexpected,
                &authority.fixture_id,
                Some(&asset.id),
                format!("asset:{}", asset.id),
                None,
                Some(asset_summary(asset)),
            ));
        }
    }
    Ok(())
}

fn validate_actual_references(
    fixture: &str,
    actual: &SemanticSnapshot,
    diffs: &mut Vec<QualityDiff>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let assets = actual.assets.iter().map(|asset| asset.id.as_str()).collect::<BTreeSet<_>>();
    let footnotes = actual
        .nodes
        .iter()
        .filter_map(|node| node.kind.strip_prefix("footnote:"))
        .collect::<BTreeSet<_>>();
    for node in &actual.nodes {
        context.checkpoint()?;
        for reference in &node.references {
            context.checkpoint()?;
            let valid = match reference.kind.as_str() {
                "asset" | "attachment" => assets.contains(reference.target.as_str()),
                "footnote" => footnotes.contains(reference.target.as_str()),
                _ => true,
            };
            if !valid {
                diffs.push(diff(
                    DiffKind::ResourceAssociation,
                    fixture,
                    Some(&node.id),
                    location(node),
                    Some(format!("existing {} target", reference.kind)),
                    Some(reference.target.clone()),
                ));
            }
        }
    }
    for asset in &actual.assets {
        context.checkpoint()?;
        if !asset.referenced {
            diffs.push(diff(
                DiffKind::ResourceAssociation,
                fixture,
                Some(&asset.id),
                format!("asset:{}", asset.id),
                Some("body reference".into()),
                Some("orphan asset".into()),
            ));
        }
    }
    Ok(())
}

fn metrics(
    expected: &[SemanticNode],
    actual: &[SemanticNode],
    context: &ExecutionContext,
) -> Result<QualityMetrics, ConversionError> {
    let expected = multiset(expected, context)?;
    let actual = multiset(actual, context)?;
    let mut true_positive = 0_u64;
    for (fingerprint, expected_count) in &expected {
        context.checkpoint()?;
        true_positive = true_positive
            .saturating_add((*expected_count).min(actual.get(fingerprint).copied().unwrap_or(0)));
    }
    let expected_total = expected.values().copied().fold(0_u64, u64::saturating_add);
    let actual_total = actual.values().copied().fold(0_u64, u64::saturating_add);
    let false_positive = actual_total.saturating_sub(true_positive);
    let false_negative = expected_total.saturating_sub(true_positive);
    Ok(QualityMetrics {
        true_positive,
        false_positive,
        false_negative,
        precision_basis_points: ratio(true_positive, actual_total),
        recall_basis_points: ratio(true_positive, expected_total),
    })
}

fn multiset(
    nodes: &[SemanticNode],
    context: &ExecutionContext,
) -> Result<BTreeMap<(String, String, String), u64>, ConversionError> {
    let mut values = BTreeMap::new();
    for node in nodes {
        context.checkpoint()?;
        *values.entry((node.id.clone(), node.kind.clone(), node.text.clone())).or_default() += 1;
    }
    Ok(values)
}

fn ratio(numerator: u64, denominator: u64) -> u16 {
    if denominator == 0 {
        return 10_000;
    }
    u16::try_from(u128::from(numerator) * 10_000 / u128::from(denominator)).unwrap_or(10_000)
}

fn duplicate_nodes(
    fixture: &str,
    nodes: &[SemanticNode],
    side: &str,
    diffs: &mut Vec<QualityDiff>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut counts = BTreeMap::new();
    for node in nodes {
        context.checkpoint()?;
        *counts.entry(node.id.as_str()).or_insert(0_u64) += 1;
    }
    for (id, count) in counts {
        context.checkpoint()?;
        if count > 1 {
            diffs.push(diff(
                DiffKind::Duplicate,
                fixture,
                Some(id),
                format!("{side}:node:{id}"),
                Some("one occurrence".into()),
                Some(format!("{count} occurrences")),
            ));
        }
    }
    Ok(())
}

fn duplicate_assets(
    fixture: &str,
    assets: &[AssetSnapshot],
    side: &str,
    diffs: &mut Vec<QualityDiff>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut counts = BTreeMap::new();
    for asset in assets {
        context.checkpoint()?;
        *counts.entry(asset.id.as_str()).or_insert(0_u64) += 1;
    }
    for (id, count) in counts {
        context.checkpoint()?;
        if count > 1 {
            diffs.push(diff(
                DiffKind::Duplicate,
                fixture,
                Some(id),
                format!("{side}:asset:{id}"),
                Some("one occurrence".into()),
                Some(format!("{count} occurrences")),
            ));
        }
    }
    Ok(())
}

fn first_nodes<'a>(
    nodes: &'a [SemanticNode],
    context: &ExecutionContext,
) -> Result<BTreeMap<&'a str, &'a SemanticNode>, ConversionError> {
    let mut values = BTreeMap::new();
    for node in nodes {
        context.checkpoint()?;
        values.entry(node.id.as_str()).or_insert(node);
    }
    Ok(values)
}

fn location(node: &SemanticNode) -> String {
    let source = node
        .boundary
        .page
        .map(|page| format!("page:{page}"))
        .or_else(|| node.boundary.slide.map(|slide| format!("slide:{slide}")))
        .or_else(|| node.boundary.sheet.as_ref().map(|sheet| format!("sheet:{sheet}")))
        .unwrap_or_else(|| "document".into());
    format!("{source}/node:{}/order:{}", node.id, node.order)
}

fn boundary_summary(boundary: &crate::SourceBoundary) -> String {
    format!(
        "page={:?},slide={:?},sheet={:?},cell={:?},part={:?},bytes={:?}..{:?}",
        boundary.page,
        boundary.slide,
        boundary.sheet.as_deref().map(compact),
        boundary.cell.as_deref().map(compact),
        boundary.part.as_deref().map(compact),
        boundary.byte_start,
        boundary.byte_end
    )
}

fn table_summary(table: Option<&crate::TableTopology>) -> String {
    table.map_or_else(
        || "none".into(),
        |table| {
            format!(
                "rows={},columns={},originCells={}",
                table.rows,
                table.columns,
                table.cells.len()
            )
        },
    )
}

fn reference_summary(references: &[crate::SemanticReference]) -> String {
    let targets = references
        .iter()
        .take(8)
        .map(|reference| format!("{}:{}", reference.kind, compact(&reference.target)))
        .collect::<Vec<_>>()
        .join(",");
    format!("count={},targets=[{targets}]", references.len())
}

fn chain_summary(chain: &[crate::SourceStep]) -> String {
    let providers =
        chain.iter().take(8).map(|step| compact(&step.provider)).collect::<Vec<_>>().join(",");
    format!("count={},providers=[{providers}]", chain.len())
}

fn asset_summary(asset: &AssetSnapshot) -> String {
    format!(
        "mediaType={},filename={:?},externalUri={:?},bytes={},sha256={},referenced={}",
        compact(&asset.media_type),
        asset.filename.as_deref().map(compact),
        asset.external_uri.as_deref().map(compact),
        asset.bytes,
        asset.sha256,
        asset.referenced
    )
}

fn compact(value: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut output = value.chars().take(MAX_CHARS).collect::<String>();
    if value.chars().nth(MAX_CHARS).is_some() {
        output.push('…');
    }
    output
}

fn diff(
    kind: DiffKind,
    fixture_id: &str,
    node_id: Option<&str>,
    location: String,
    expected: Option<String>,
    actual: Option<String>,
) -> QualityDiff {
    QualityDiff {
        kind,
        fixture_id: fixture_id.into(),
        node_id: node_id.map(str::to_owned),
        location,
        expected,
        actual,
    }
}
