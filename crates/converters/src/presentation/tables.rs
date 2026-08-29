use super::error::limit;
use super::model::{ParseState, PresentationCell};
use into_markdown_core::{
    Block, Cell, ConversionError, ConversionOptions, MAX_TABLE_COLUMNS, Rect, TableAlignment,
    TableRow,
};

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn table_block(
    rows: Vec<Vec<PresentationCell>>,
    part: &str,
    slide: u32,
    bounds: Option<Rect>,
    z_order: usize,
    languages: &[String],
    options: &ConversionOptions,
    state: &mut ParseState,
) -> Result<Block, ConversionError> {
    let width = rows.iter().try_fold(0_usize, |maximum, row| {
        let row_width = row.iter().try_fold(0_usize, |width, cell| {
            width
                .checked_add(usize::try_from(cell.column_span).unwrap_or(usize::MAX))
                .ok_or_else(|| limit("max_table_columns", "logical table width overflow"))
        })?;
        Ok::<_, ConversionError>(maximum.max(row_width))
    })?;
    if width > MAX_TABLE_COLUMNS
        || u64::try_from(width).unwrap_or(u64::MAX) > options.limits.max_table_columns
    {
        return Err(limit("max_table_columns", width.to_string()));
    }
    if u64::try_from(rows.len()).unwrap_or(u64::MAX) > options.limits.max_table_rows {
        return Err(limit("max_table_rows", rows.len().to_string()));
    }
    let cell_count = rows.iter().try_fold(0_u64, |count, row| {
        count
            .checked_add(u64::try_from(row.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("max_table_cells", "table cell count overflow"))
    })?;
    if cell_count > options.limits.max_table_cells {
        return Err(limit("max_table_cells", cell_count.to_string()));
    }
    let mut output = Vec::new();
    output.try_reserve(rows.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve table rows: {error}"))
    })?;
    let mut occupancy = vec![0_u32; width];
    for (row_index, row) in rows.into_iter().enumerate() {
        let mut cells = Vec::new();
        cells.try_reserve(row.len()).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve table cells: {error}"))
        })?;
        for cell in row {
            if cell.horizontal_continuation || cell.vertical_continuation {
                continue;
            }
            state.add_inlines(cell.inlines.len())?;
            let paragraph = state.node(
                Block::Paragraph(cell.inlines),
                part,
                slide,
                bounds,
                Some(z_order),
                Some(languages),
            )?;
            let mut blocks = Vec::new();
            blocks.try_reserve_exact(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve table cell block: {error}"))
            })?;
            blocks.push(paragraph);
            cells.push(Cell {
                row_span: cell.row_span,
                column_span: cell.column_span,
                header: row_index == 0,
                blocks,
            });
        }
        let occupied_from_prior_rows = occupancy.iter().filter(|remaining| **remaining > 0).count();
        let emitted_columns = cells.iter().try_fold(occupied_from_prior_rows, |count, cell| {
            count
                .checked_add(usize::try_from(cell.column_span).unwrap_or(usize::MAX))
                .ok_or_else(|| limit("max_table_columns", "logical table width overflow"))
        })?;
        if emitted_columns > width {
            return Err(limit("max_table_columns", "merged table row exceeds its logical width"));
        }
        for _ in emitted_columns..width {
            let paragraph = state.node(
                Block::Paragraph(Vec::new()),
                part,
                slide,
                bounds,
                Some(z_order),
                Some(languages),
            )?;
            cells.try_reserve(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve normalized table cell: {error}"))
            })?;
            cells.push(Cell {
                row_span: 1,
                column_span: 1,
                header: row_index == 0,
                blocks: vec![paragraph],
            });
        }
        let mut column = 0_usize;
        for cell in &cells {
            while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                column += 1;
            }
            let end = column
                .checked_add(usize::try_from(cell.column_span).unwrap_or(usize::MAX))
                .ok_or_else(|| limit("max_table_columns", "logical table width overflow"))?;
            if end > occupancy.len() {
                return Err(limit(
                    "max_table_columns",
                    "normalized table cell exceeds its logical width",
                ));
            }
            occupancy[column..end].fill(cell.row_span);
            column = end;
        }
        output.push(TableRow { cells });
        for remaining in &mut occupancy {
            *remaining = remaining.saturating_sub(1);
        }
    }
    let mut alignments = Vec::new();
    alignments.try_reserve_exact(width).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve table alignments: {error}"))
    })?;
    alignments.resize(width, TableAlignment::None);
    Ok(Block::Table { rows: output, alignments })
}
