fn validate_table_limits(
    rows: &[TableRow],
    part: &str,
    options: &ConversionOptions,
) -> Result<bool, ConversionError> {
    if rows.is_empty() {
        return Err(malformed(Some(part), "table has no rows"));
    }
    let row_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    if row_count > options.limits.max_table_rows {
        return Err(limit(
            "max_table_rows",
            format!("{row_count} > {}", options.limits.max_table_rows),
        ));
    }
    let mut cells = 0_u64;
    let mut expected_width = None;
    let mut ragged = false;
    for row in rows {
        if row.cells.is_empty() {
            return Err(malformed(Some(part), "table row has no cells"));
        }
        cells = cells
            .checked_add(u64::try_from(row.cells.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("max_table_cells", "DOCX table cell count overflow"))?;
        let width = row.cells.iter().try_fold(0_u64, |total, cell| {
            total
                .checked_add(u64::from(cell.column_span))
                .ok_or_else(|| limit("max_table_columns", "DOCX table width overflow"))
        })?;
        if width > options.limits.max_table_columns || width > MAX_TABLE_COLUMNS as u64 {
            return Err(limit(
                "max_table_columns",
                format!(
                    "{width} > {}",
                    options.limits.max_table_columns.min(MAX_TABLE_COLUMNS as u64)
                ),
            ));
        }
        if expected_width.replace(width).is_some_and(|expected| expected != width) {
            if options.error_policy == into_markdown_core::ErrorPolicy::Strict {
                return Err(malformed(Some(part), "table rows have inconsistent widths"));
            }
            ragged = true;
        }
    }
    if cells > options.limits.max_table_cells {
        return Err(limit(
            "max_table_cells",
            format!("{cells} > {}", options.limits.max_table_cells),
        ));
    }
    Ok(ragged)
}

fn parse_vertical_merge(value: Option<&str>) -> VerticalMerge {
    match value {
        Some("restart") => VerticalMerge::Restart,
        None | Some("" | "continue") => VerticalMerge::Continue,
        Some(_) => VerticalMerge::Invalid,
    }
}

fn inlines_have_visible_content(inlines: &[Inline]) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::Text { value, .. }
        | Inline::SourceText { value, .. }
        | Inline::OcrText { value, .. }
        | Inline::Code(value)
        | Inline::Formula(value) => !value.trim().is_empty(),
        Inline::Link { content, .. } => inlines_have_visible_content(content),
        Inline::LineBreak => false,
        _ => true,
    })
}

fn blocks_have_visible_content(blocks: &[BlockNode]) -> bool {
    blocks.iter().any(|node| match &node.block {
        Block::Paragraph(inlines) => inlines_have_visible_content(inlines),
        Block::Heading { content, .. } | Block::TimedSegment { content, .. } => {
            inlines_have_visible_content(content)
        }
        Block::List { items, .. } => {
            items.iter().any(|item| blocks_have_visible_content(&item.blocks))
        }
        Block::Code { text, .. } | Block::Formula(text) => !text.trim().is_empty(),
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Sheet { blocks, .. } => blocks_have_visible_content(blocks),
        Block::Slide { title, blocks, .. } => {
            title.as_deref().is_some_and(|value| !value.trim().is_empty())
                || blocks_have_visible_content(blocks)
        }
        _ => true,
    })
}

#[derive(Clone, Copy)]
struct ActiveVerticalMerge {
    width: u32,
    origin_row: usize,
    origin_cell: usize,
}

fn normalize_vertical_merges(
    rows: &mut [TableRow],
    declarations: &[Vec<VerticalMerge>],
    part: &str,
    options: &ConversionOptions,
    state: &mut ParseState,
) -> Result<(), ConversionError> {
    if options.error_policy == into_markdown_core::ErrorPolicy::Strict
        && declarations.iter().flatten().any(|merge| *merge != VerticalMerge::None)
    {
        return Err(malformed(Some(part), "vertical table merges are unsupported"));
    }
    let mut active = BTreeMap::<u64, ActiveVerticalMerge>::new();
    for row_index in 0..rows.len() {
        let mut source_cells = std::mem::take(&mut rows[row_index].cells);
        let row_declarations = declarations.get(row_index).map(Vec::as_slice).unwrap_or_default();
        let mut output_cells = Vec::new();
        output_cells.try_reserve_exact(source_cells.len()).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve normalized DOCX row: {error}"))
        })?;
        let mut next_active = BTreeMap::<u64, ActiveVerticalMerge>::new();
        let mut column = 0_u64;
        for (cell_index, cell) in source_cells.drain(..).enumerate() {
            let width = u64::from(cell.column_span);
            let end = column
                .checked_add(width)
                .ok_or_else(|| limit("max_table_columns", "DOCX vertical merge overflow"))?;
            match row_declarations.get(cell_index).copied().unwrap_or_default() {
                VerticalMerge::None => output_cells.push(cell),
                VerticalMerge::Invalid => {
                    state.warning(
                        "word.tableNormalized",
                        "invalid vertical merge was kept as an ordinary cell",
                        part,
                    );
                    output_cells.push(cell);
                }
                VerticalMerge::Restart => {
                    let origin_cell = output_cells.len();
                    output_cells.push(cell);
                    next_active.insert(
                        column,
                        ActiveVerticalMerge {
                            width: u32::try_from(width).unwrap_or(u32::MAX),
                            origin_row: row_index,
                            origin_cell,
                        },
                    );
                    state.info(
                        "word.tableNormalized",
                        "vertical merge geometry was preserved with rowspan",
                        part,
                    );
                }
                VerticalMerge::Continue => {
                    if blocks_have_visible_content(&cell.blocks) {
                        state.warning(
                            "word.tableNormalized",
                            "vertical merge continuation with visible content was kept in document order",
                            part,
                        );
                        output_cells.push(cell);
                        column = end;
                        continue;
                    }
                    let Some(origin) = active
                        .get(&column)
                        .copied()
                        .filter(|merge| u64::from(merge.width) == width)
                    else {
                        state.warning(
                            "word.tableNormalized",
                            "unmatched vertical merge was kept as an ordinary cell",
                            part,
                        );
                        output_cells.push(cell);
                        column = end;
                        continue;
                    };
                    let origin_cell = rows
                        .get_mut(origin.origin_row)
                        .and_then(|row| row.cells.get_mut(origin.origin_cell))
                        .ok_or_else(|| malformed(Some(part), "vertical merge origin is invalid"))?;
                    origin_cell.row_span = origin_cell
                        .row_span
                        .checked_add(1)
                        .ok_or_else(|| limit("max_table_rows", "vertical merge span overflow"))?;
                    state.info(
                        "word.tableNormalized",
                        "vertical merge geometry was preserved with rowspan",
                        part,
                    );
                    next_active.insert(column, origin);
                }
            }
            column = end;
        }
        rows[row_index].cells = output_cells;
        active = next_active;
    }
    Ok(())
}

fn normalize_ragged_table(
    rows: &mut [TableRow],
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let width = rows.iter().try_fold(0_u64, |maximum, row| {
        let width = row.cells.iter().try_fold(0_u64, |total, cell| {
            total
                .checked_add(u64::from(cell.column_span))
                .ok_or_else(|| limit("max_table_columns", "DOCX table width overflow"))
        })?;
        Ok::<_, ConversionError>(maximum.max(width))
    })?;
    let mut added = 0_u64;
    for row in rows.iter_mut() {
        let row_width = row.cells.iter().map(|cell| u64::from(cell.column_span)).sum::<u64>();
        let missing = width.saturating_sub(row_width);
        added = added
            .checked_add(missing)
            .ok_or_else(|| limit("max_table_cells", "normalized DOCX cell count overflow"))?;
        let missing = usize::try_from(missing)
            .map_err(|_| limit("max_table_cells", "normalized cell count is not representable"))?;
        row.cells.try_reserve(missing).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve normalized DOCX cells: {error}"))
        })?;
        for _ in 0..missing {
            row.cells.push(Cell { row_span: 1, column_span: 1, header: false, blocks: Vec::new() });
        }
    }
    let total = rows.iter().try_fold(0_u64, |count, row| {
        count
            .checked_add(u64::try_from(row.cells.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("max_table_cells", "normalized DOCX cell count overflow"))
    })?;
    if total > options.limits.max_table_cells {
        return Err(limit(
            "max_table_cells",
            format!(
                "{total} > {} after adding {added} normalized cells",
                options.limits.max_table_cells
            ),
        ));
    }
    Ok(())
}
