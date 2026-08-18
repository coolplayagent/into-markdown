use super::{LayoutDiff, LayoutDiffKind, SemanticNode};
use crate::Asset;
use std::collections::BTreeSet;

pub(super) fn compare(
    golden_nodes: &[SemanticNode],
    golden_assets: &[Asset],
    actual_nodes: &[SemanticNode],
    actual_assets: &[Asset],
    differences: &mut Vec<LayoutDiff>,
) {
    compare_published(
        golden_nodes,
        &published(golden_assets),
        actual_nodes,
        &published(actual_assets),
        differences,
    );
    duplicate_assets(actual_assets, differences);
}

pub(super) fn compare_golden(
    golden_nodes: &[SemanticNode],
    golden_assets: &[String],
    actual_nodes: &[SemanticNode],
    actual_assets: &[Asset],
    differences: &mut Vec<LayoutDiff>,
) {
    let golden_published = golden_assets.iter().map(String::as_str).collect();
    let actual_published = published(actual_assets);
    compare_published(
        golden_nodes,
        &golden_published,
        actual_nodes,
        &actual_published,
        differences,
    );
    duplicate_assets(actual_assets, differences);
}

fn compare_published(
    golden_nodes: &[SemanticNode],
    golden_published: &BTreeSet<&str>,
    actual_nodes: &[SemanticNode],
    actual_published: &BTreeSet<&str>,
    differences: &mut Vec<LayoutDiff>,
) {
    let golden_refs = refs(golden_nodes);
    let actual_refs = refs(actual_nodes);
    let all = golden_refs
        .union(golden_published)
        .copied()
        .chain(actual_refs.union(actual_published).copied())
        .collect::<BTreeSet<_>>();
    for id in all {
        let expected = (golden_refs.contains(id), golden_published.contains(id));
        let actual = (actual_refs.contains(id), actual_published.contains(id));
        if expected != actual {
            differences.push(diff(
                id,
                &format!("referenced={},published={}", expected.0, expected.1),
                &format!("referenced={},published={}", actual.0, actual.1),
            ));
        }
    }
}

fn duplicate_assets(actual_assets: &[Asset], differences: &mut Vec<LayoutDiff>) {
    let mut seen = BTreeSet::new();
    for asset in actual_assets {
        if !seen.insert(asset.id.0.as_str()) {
            differences.push(diff(&asset.id.0, "one published asset", "duplicate published asset"));
        }
    }
}

fn refs(nodes: &[SemanticNode]) -> BTreeSet<&str> {
    nodes.iter().filter_map(|node| node.asset.as_deref()).collect()
}

fn published(assets: &[Asset]) -> BTreeSet<&str> {
    assets.iter().map(|asset| asset.id.0.as_str()).collect()
}

fn diff(id: &str, expected: &str, actual: &str) -> LayoutDiff {
    LayoutDiff {
        kind: LayoutDiffKind::ResourceAssociation,
        node: Some(id.to_owned()),
        boundary: None,
        expected: expected.into(),
        actual: actual.into(),
    }
}
