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
    let golden_refs = refs(golden_nodes);
    let actual_refs = refs(actual_nodes);
    let golden_published = published(golden_assets);
    let actual_published = published(actual_assets);
    let all = golden_refs
        .union(&golden_published)
        .copied()
        .chain(actual_refs.union(&actual_published).copied())
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
