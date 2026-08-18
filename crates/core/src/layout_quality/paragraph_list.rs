use super::{LayoutDiff, LayoutDiffKind, SemanticNode, by_id, working_overflow};
use crate::{ConversionError, ExecutionContext};

pub(super) fn compare(
    golden: &[SemanticNode],
    actual: &[SemanticNode],
    differences: &mut Vec<LayoutDiff>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let units = golden
        .len()
        .checked_add(actual.len())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(working_overflow)?;
    context.consume_work(units)?;
    let actual = by_id(actual);
    for expected in golden {
        let Some(observed) = actual.get(expected.id.as_str()) else { continue };
        if expected.parent != observed.parent {
            differences.push(LayoutDiff {
                kind: LayoutDiffKind::WrongHierarchy,
                node: Some(expected.id.clone()),
                boundary: observed.boundary.clone(),
                expected: expected.parent.clone().unwrap_or_else(|| "root".into()),
                actual: observed.parent.clone().unwrap_or_else(|| "root".into()),
            });
        }
        if expected.boundary != observed.boundary {
            differences.push(LayoutDiff {
                kind: LayoutDiffKind::Boundary,
                node: Some(expected.id.clone()),
                boundary: observed.boundary.clone(),
                expected: format!("{:?}", expected.boundary),
                actual: format!("{:?}", observed.boundary),
            });
        }
        if expected.kind != observed.kind || expected.text != observed.text {
            differences.push(LayoutDiff {
                kind: LayoutDiffKind::WrongHierarchy,
                node: Some(expected.id.clone()),
                boundary: observed.boundary.clone(),
                expected: format!("{}:{}", expected.kind, expected.text),
                actual: format!("{}:{}", observed.kind, observed.text),
            });
        }
    }
    Ok(())
}
