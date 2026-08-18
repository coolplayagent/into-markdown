use super::{LayoutDiff, LayoutDiffKind, SemanticNode, by_id};
use crate::Block;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TableTopology {
    rows: u32,
    columns: u32,
    cells: Vec<(u32, u32, u32, u32)>,
}

pub(super) fn topology(block: &Block) -> Option<TableTopology> {
    let Block::Table { rows, .. } = block else { return None };
    let mut occupied = Vec::<u32>::new();
    let mut cells = Vec::new();
    let mut width = 0_u32;
    for (row_index, row) in rows.iter().enumerate() {
        let mut column = 0_usize;
        for cell in &row.cells {
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
    Some(TableTopology {
        rows: u32::try_from(rows.len()).unwrap_or(u32::MAX),
        columns: width,
        cells,
    })
}

pub(super) fn compare(
    golden: &[SemanticNode],
    actual: &[SemanticNode],
    differences: &mut Vec<LayoutDiff>,
) {
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
}
