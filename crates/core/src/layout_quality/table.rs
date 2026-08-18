use super::{LayoutDiff, LayoutDiffKind, SemanticNode, by_id, working_overflow};
use crate::{Block, ConversionError, ExecutionContext};
use serde::{Deserialize, Serialize};

/// Exact logical table grid and origin-cell spans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableTopology {
    /// Logical row count.
    pub rows: u32,
    /// Maximum logical column count.
    pub columns: u32,
    /// Origin cells as `(row, column, row_span, column_span)`.
    pub cells: Vec<(u32, u32, u32, u32)>,
}

pub(super) fn topology(
    block: &Block,
    context: &ExecutionContext,
) -> Result<Option<TableTopology>, ConversionError> {
    let Block::Table { rows, .. } = block else { return Ok(None) };
    let mut occupied = Vec::<u32>::new();
    let mut cells = Vec::new();
    let mut width = 0_u32;
    for (row_index, row) in rows.iter().enumerate() {
        context.consume_work(1)?;
        let mut column = 0_usize;
        for cell in &row.cells {
            context.consume_work(1)?;
            while occupied.get(column).is_some_and(|remaining| *remaining > 0) {
                column += 1;
            }
            let end = column.saturating_add(cell.column_span as usize);
            occupied.resize(occupied.len().max(end), 0);
            occupied[column..end].fill(cell.row_span);
            cells.push((
                u32::try_from(row_index).unwrap_or(u32::MAX),
                u32::try_from(column).unwrap_or(u32::MAX),
                cell.row_span,
                cell.column_span,
            ));
            width = width.max(u32::try_from(end).unwrap_or(u32::MAX));
            column = end;
        }
        for remaining in &mut occupied {
            *remaining = remaining.saturating_sub(1);
        }
    }
    Ok(Some(TableTopology {
        rows: u32::try_from(rows.len()).unwrap_or(u32::MAX),
        columns: width,
        cells,
    }))
}

pub(super) fn compare(
    golden: &[SemanticNode],
    actual: &[SemanticNode],
    differences: &mut Vec<LayoutDiff>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let units = golden
        .len()
        .checked_mul(2)
        .and_then(|value| value.checked_add(actual.len()))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(working_overflow)?;
    context.consume_work(units)?;
    let actual = by_id(actual);
    for expected in golden.iter().filter(|node| node.table.is_some()) {
        let Some(observed) = actual.get(expected.id.as_str()) else { continue };
        if expected.table != observed.table {
            differences.push(LayoutDiff {
                kind: LayoutDiffKind::TableTopology,
                node: Some(expected.id.clone()),
                boundary: observed.boundary.clone(),
                expected: format!("{:?}", expected.table),
                actual: format!("{:?}", observed.table),
            });
        }
    }
    Ok(())
}
