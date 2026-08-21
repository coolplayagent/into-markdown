use crate::{TableCellTopology, TableTopology};
use into_markdown_core::{Cell, ConversionError, ExecutionContext, TableRow};
use std::collections::BTreeSet;

pub(crate) fn topology(
    rows: &[TableRow],
    context: &ExecutionContext,
) -> Result<TableTopology, ConversionError> {
    let row_count = u64::try_from(rows.len()).map_err(|_| work_limit("table row count"))?;
    if row_count > context.resource_limits().max_table_rows {
        return Err(ConversionError::ResourceLimit {
            limit: "max_table_rows",
            detail: format!("{row_count} > {}", context.resource_limits().max_table_rows),
        });
    }
    let mut cells = Vec::new();
    let mut occupied = BTreeSet::new();
    let mut columns = 0_u64;
    for (row_index, row) in rows.iter().enumerate() {
        context.checkpoint()?;
        let row_index = u64::try_from(row_index).map_err(|_| work_limit("table row index"))?;
        let mut column = 0_u64;
        for cell in &row.cells {
            context.checkpoint()?;
            while occupied.contains(&(row_index, column)) {
                column = column.checked_add(1).ok_or_else(|| work_limit("table column"))?;
            }
            let row_span = u64::from(cell.row_span);
            let column_span = u64::from(cell.column_span);
            if row_span == 0 || column_span == 0 {
                return Err(ConversionError::Malformed {
                    part: None,
                    detail: "table origin cell has a zero span".into(),
                });
            }
            let row_end =
                row_index.checked_add(row_span).ok_or_else(|| work_limit("table row span"))?;
            if row_end > row_count {
                return Err(ConversionError::Malformed {
                    part: None,
                    detail: "table row span leaves the logical row grid".into(),
                });
            }
            let column_end =
                column.checked_add(column_span).ok_or_else(|| work_limit("table column span"))?;
            if column_end > context.resource_limits().max_table_columns {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_table_columns",
                    detail: format!(
                        "{column_end} > {}",
                        context.resource_limits().max_table_columns
                    ),
                });
            }
            for occupied_row in row_index..row_end {
                for occupied_column in column..column_end {
                    context.checkpoint()?;
                    if !occupied.insert((occupied_row, occupied_column)) {
                        return Err(ConversionError::Malformed {
                            part: None,
                            detail: "table origin-cell spans overlap".into(),
                        });
                    }
                    if u64::try_from(occupied.len()).unwrap_or(u64::MAX)
                        > context.resource_limits().max_table_cells
                    {
                        return Err(ConversionError::ResourceLimit {
                            limit: "max_table_cells",
                            detail: "semantic layout table topology exceeds the configured grid"
                                .into(),
                        });
                    }
                }
            }
            cells.push(cell_topology(row_index, column, cell));
            column = column_end;
        }
        columns = columns.max(column);
    }
    Ok(TableTopology { rows: row_count, columns, cells })
}

fn cell_topology(row: u64, column: u64, cell: &Cell) -> TableCellTopology {
    TableCellTopology {
        row,
        column,
        row_span: cell.row_span,
        column_span: cell.column_span,
        header: cell.header,
        block_ids: cell.blocks.iter().map(|block| block.id.0.clone()).collect(),
    }
}

fn work_limit(detail: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "semantic_layout_work", detail: detail.into() }
}
