use super::{LayoutDiff, LayoutDiffKind, SemanticNode, by_id, duplicate_ids};

pub(super) fn compare(
    golden: &[SemanticNode],
    actual: &[SemanticNode],
    differences: &mut Vec<LayoutDiff>,
) {
    let golden_by_id = by_id(golden);
    let actual_by_id = by_id(actual);
    for id in duplicate_ids(actual) {
        let node = actual_by_id[id];
        differences.push(LayoutDiff {
            kind: LayoutDiffKind::Duplicate,
            node: Some(id.to_owned()),
            boundary: node.boundary.clone(),
            expected: "one occurrence".into(),
            actual: "multiple occurrences".into(),
        });
    }
    for node in golden {
        if !actual_by_id.contains_key(node.id.as_str()) {
            differences.push(LayoutDiff {
                kind: LayoutDiffKind::Missing,
                node: Some(node.id.clone()),
                boundary: node.boundary.clone(),
                expected: format!("{}:{}", node.kind, node.text),
                actual: "absent".into(),
            });
        }
    }
    for node in actual {
        if !golden_by_id.contains_key(node.id.as_str()) {
            differences.push(LayoutDiff {
                kind: LayoutDiffKind::Unexpected,
                node: Some(node.id.clone()),
                boundary: node.boundary.clone(),
                expected: "absent".into(),
                actual: format!("{}:{}", node.kind, node.text),
            });
        }
    }
    let expected = golden
        .iter()
        .filter(|node| actual_by_id.contains_key(node.id.as_str()))
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    let observed = actual
        .iter()
        .filter(|node| golden_by_id.contains_key(node.id.as_str()))
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    if expected != observed {
        let first = expected
            .iter()
            .zip(&observed)
            .position(|(left, right)| left != right)
            .unwrap_or(expected.len().min(observed.len()));
        differences.push(LayoutDiff {
            kind: LayoutDiffKind::OutOfOrder,
            node: observed.get(first).map(|value| (*value).to_owned()),
            boundary: observed
                .get(first)
                .and_then(|id| actual_by_id.get(*id))
                .and_then(|node| node.boundary.clone()),
            expected: expected.join(","),
            actual: observed.join(","),
        });
    }
}
