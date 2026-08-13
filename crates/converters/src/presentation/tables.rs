use super::error::limit;
use super::model::ParseState;
use into_markdown_core::{
    Block, Cell, ConversionError, ConversionOptions, Inline, MAX_TABLE_COLUMNS, Rect,
    TableAlignment, TableRow,
};

#[allow(clippy::too_many_arguments)]
pub(super) fn table_block(
    rows: Vec<Vec<Vec<Inline>>>,
    part: &str,
    slide: u32,
    bounds: Option<Rect>,
    z_order: usize,
    languages: &[String],
    options: &ConversionOptions,
    state: &mut ParseState,
) -> Result<Block, ConversionError> {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
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
    for (row_index, row) in rows.into_iter().enumerate() {
        let mut cells = Vec::new();
        cells.try_reserve(row.len()).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve table cells: {error}"))
        })?;
        for cell in row {
            state.add_inlines(cell.len())?;
            let paragraph = state.node(
                Block::Paragraph(cell),
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
            cells.push(Cell { row_span: 1, column_span: 1, header: row_index == 0, blocks });
        }
        output.push(TableRow { cells });
    }
    let mut alignments = Vec::new();
    alignments.try_reserve_exact(width).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve table alignments: {error}"))
    })?;
    alignments.resize(width, TableAlignment::None);
    Ok(Block::Table { rows: output, alignments })
}
