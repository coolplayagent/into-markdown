use super::budget::LegacyBudget;
use super::builder::{OutputBuilder, locator};
use into_markdown_core::{Block, Cell, ConversionError, TableRow};

pub(super) fn rectangularize(
    rows: &mut [TableRow],
    builder: &mut OutputBuilder,
    budget: &LegacyBudget<'_>,
    part: &str,
) -> Result<(), ConversionError> {
    let width = logical_width(rows, budget)?;
    budget.table_shape(rows.len(), width)?;
    let mut occupancy = vec![0u32; width];
    for row in rows {
        let mut column = 0usize;
        for cell in &row.cells {
            while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                column += 1;
            }
            let end = column.saturating_add(cell.column_span as usize);
            occupancy[column..end].fill(cell.row_span);
            column = end;
        }
        while column < width {
            if occupancy[column] > 0 {
                column += 1;
                continue;
            }
            row.cells.push(Cell {
                row_span: 1,
                column_span: 1,
                header: false,
                blocks: vec![builder.node(Block::Paragraph(Vec::new()), locator(part))],
            });
            occupancy[column] = 1;
            column += 1;
        }
        for remaining in &mut occupancy {
            *remaining = remaining.saturating_sub(1);
        }
    }
    Ok(())
}

fn logical_width(rows: &[TableRow], budget: &LegacyBudget<'_>) -> Result<usize, ConversionError> {
    let mut width = 0usize;
    let mut occupancy = Vec::<u32>::new();
    for row in rows {
        let mut column = 0usize;
        for cell in &row.cells {
            while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                column += 1;
            }
            let end = column.saturating_add(cell.column_span as usize);
            budget.table_shape(rows.len(), end)?;
            if occupancy.len() < end {
                occupancy.resize(end, 0);
            }
            occupancy[column..end].fill(cell.row_span);
            column = end;
            width = width.max(end);
        }
        for remaining in &mut occupancy {
            *remaining = remaining.saturating_sub(1);
        }
    }
    Ok(width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ConversionOptions, ExecutionContext, ExecutionOptions};

    #[test]
    fn pads_short_rows_without_discarding_empty_positions_or_spans() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let budget = LegacyBudget::new(1, &options, &context).unwrap();
        let mut builder = OutputBuilder::new("test");
        let cell =
            |span| Cell { row_span: 1, column_span: span, header: false, blocks: Vec::new() };
        let mut rows =
            vec![TableRow { cells: vec![cell(1), cell(2)] }, TableRow { cells: vec![cell(1)] }];
        rectangularize(&mut rows, &mut builder, &budget, "table").unwrap();
        assert_eq!(rows[0].cells.iter().map(|cell| cell.column_span).sum::<u32>(), 3);
        assert_eq!(rows[1].cells.len(), 3);
    }

    #[test]
    fn width_limit_precedes_occupancy_allocation() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let budget = LegacyBudget::new(1, &options, &context).unwrap();
        let mut builder = OutputBuilder::new("test");
        let mut rows = [TableRow {
            cells: vec![Cell {
                row_span: 1,
                column_span: u32::MAX,
                header: false,
                blocks: Vec::new(),
            }],
        }];
        assert!(matches!(
            rectangularize(&mut rows, &mut builder, &budget, "table"),
            Err(ConversionError::ResourceLimit { .. })
        ));
    }
}
