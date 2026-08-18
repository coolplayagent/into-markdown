use super::{LayoutDiff, LayoutDiffKind, SemanticNode, by_id};
use crate::Rect;

pub(super) fn compare(
    golden: &[SemanticNode],
    actual: &[SemanticNode],
    tolerance: f32,
    differences: &mut Vec<LayoutDiff>,
) {
    let actual = by_id(actual);
    for expected in golden {
        let Some(observed) = actual.get(expected.id.as_str()) else { continue };
        if !same(expected.bounds, observed.bounds, tolerance) {
            differences.push(LayoutDiff {
                kind: LayoutDiffKind::Geometry,
                node: Some(expected.id.clone()),
                boundary: observed.boundary.clone(),
                expected: canonical(expected.bounds),
                actual: canonical(observed.bounds),
            });
        }
    }
}

fn same(expected: Option<Rect>, actual: Option<Rect>, tolerance: f32) -> bool {
    match (expected, actual) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            finite(left)
                && finite(right)
                && (left.x - right.x).abs() <= tolerance
                && (left.y - right.y).abs() <= tolerance
                && (left.width - right.width).abs() <= tolerance
                && (left.height - right.height).abs() <= tolerance
        }
        _ => false,
    }
}

fn finite(rect: Rect) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && rect.width >= 0.0
        && rect.height >= 0.0
}

fn canonical(rect: Option<Rect>) -> String {
    rect.map_or_else(
        || "none".into(),
        |value| format!("{:.6},{:.6},{:.6},{:.6}", value.x, value.y, value.width, value.height),
    )
}
