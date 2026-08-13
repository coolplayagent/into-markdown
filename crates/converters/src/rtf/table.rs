//! Paragraph, list, and table IR construction.

use super::budget::{limit, malformed, parameter_i32, reserve_vec};
use super::parser::{CellMerge, ListKey, Paragraph, Parser};
use into_markdown_core::{
    Block, BlockNode, Cell, ConversionError, Inline, ListItem, ListKind, MAX_TABLE_COLUMNS,
    TableRow,
};

impl Parser<'_> {
    pub(super) fn finish_paragraph(&mut self, end: usize) -> Result<(), ConversionError> {
        if self.paragraph.inlines.is_empty() && self.pending_list_marker.is_none() {
            return Ok(());
        }
        let in_table = self.table.active || self.state().in_table;
        let has_list = self.state().list_id.is_some() || self.pending_list_marker.is_some();
        if in_table && has_list {
            return Err(malformed(
                "RTF lists nested in table cells cannot be represented by this bounded state",
            ));
        }
        let list_plan = if has_list {
            let marker = self.pending_list_marker.as_deref().ok_or_else(|| {
                malformed("RTF list identity lacks a source marker and cannot be represented")
            })?;
            let (kind, list_start) = list_marker(marker)?;
            let level = self.state().list_level.unwrap_or(0);
            if level != 0 {
                return Err(malformed(
                    "nested RTF list levels cannot be represented without a list table",
                ));
            }
            let key = ListKey { id: self.state().list_id, level, kind };
            let aggregate = self.last_list_key == Some(key)
                && self.blocks.last().is_some_and(
                    |node| matches!(&node.block, Block::List { kind: current, .. } if *current == kind),
                );
            if aggregate && kind == ListKind::Ordered {
                let Some(BlockNode { block: Block::List { start, items, .. }, .. }) =
                    self.blocks.last()
                else {
                    return Err(malformed("RTF list aggregation state is inconsistent"));
                };
                let expected = start
                    .checked_add(u64::try_from(items.len()).unwrap_or(u64::MAX))
                    .ok_or_else(|| limit("rtf_list_start", "ordered-list marker overflowed u64"))?;
                if list_start != expected {
                    return Err(malformed(
                        "adjacent ordered-list markers are not a consecutive sequence",
                    ));
                }
            }
            Some((key, list_start, aggregate))
        } else {
            None
        };
        let required_nodes =
            1 + list_plan.map_or(0, |(_, _, aggregate)| if aggregate { 1 } else { 2 });
        self.ensure_document_nodes(required_nodes)?;
        if self.paragraph.inlines.is_empty() && list_plan.is_none() {
            return Ok(());
        }
        let start = self.paragraph.start.unwrap_or(end);
        let inlines = std::mem::take(&mut self.paragraph.inlines);
        let block = self.node(Block::Paragraph(inlines), start, self.paragraph.end.max(end))?;
        self.paragraph = Paragraph::default();
        if in_table {
            reserve_vec(&mut self.table.cell_blocks, 1, &mut self.memory)?;
            self.table.cell_blocks.push(block);
        } else if let Some((key, list_start, aggregate)) = list_plan {
            let marker = self
                .pending_list_marker
                .take()
                .ok_or_else(|| malformed("RTF validated list marker disappeared"))?;
            let kind = key.kind;
            let mut item_blocks = Vec::new();
            reserve_vec(&mut item_blocks, 1, &mut self.memory)?;
            item_blocks.push(block);
            self.consume_document_node()?;
            let item = ListItem { checked: None, marker_label: Some(marker), blocks: item_blocks };
            if aggregate
                && let Some(BlockNode { block: Block::List { items, .. }, .. }) =
                    self.blocks.last_mut()
            {
                reserve_vec(items, 1, &mut self.memory)?;
                items.push(item);
                return Ok(());
            }
            let mut items = Vec::new();
            reserve_vec(&mut items, 1, &mut self.memory)?;
            items.push(item);
            let list = self.node(Block::List { kind, start: list_start, items }, start, end)?;
            self.push_block(list)?;
            self.last_list_key = Some(key);
        } else {
            self.push_block(block)?;
        }
        Ok(())
    }

    pub(super) fn finish_field_result(&mut self) -> Result<(), ConversionError> {
        let field =
            self.field.as_mut().ok_or_else(|| malformed("field result has no enclosing field"))?;
        let start = field
            .inline_start
            .take()
            .ok_or_else(|| malformed("field result state is incomplete"))?;
        let Some(target) = field.link.take() else {
            return Ok(());
        };
        if start > self.paragraph.inlines.len() {
            return Err(ConversionError::Internal {
                detail: "RTF field inline boundary exceeds paragraph".into(),
            });
        }
        let count = self.paragraph.inlines.len() - start;
        if count == 0 {
            return Ok(());
        }
        let mut content = Vec::new();
        reserve_vec(&mut content, count, &mut self.memory)?;
        content.extend(self.paragraph.inlines.drain(start..));
        reserve_vec(&mut self.paragraph.inlines, 1, &mut self.memory)?;
        self.paragraph.inlines.push(Inline::Link { target, content });
        Ok(())
    }

    pub(super) fn start_table_row(&mut self, end: usize) -> Result<(), ConversionError> {
        if self.state().list_id.is_some() || self.pending_list_marker.is_some() {
            return Err(malformed(
                "RTF tables nested in list paragraphs require an explicit paragraph reset",
            ));
        }
        if !self.paragraph.inlines.is_empty()
            || !self.table.cell_blocks.is_empty()
            || !self.table.cells.is_empty()
        {
            self.finish_row(end)?;
        }
        if u64::try_from(self.table.rows.len()).unwrap_or(u64::MAX)
            >= self.options.limits.max_table_rows
        {
            return Err(limit(
                "max_table_rows",
                format!(">= {}", self.options.limits.max_table_rows),
            ));
        }
        if self.table.active {
            self.ensure_document_nodes(1)?;
        } else {
            self.ensure_document_nodes(2)?;
            self.consume_document_node()?;
            self.table.node_reserved = true;
        }
        self.consume_document_node()?;
        self.table.active = true;
        self.table.cell_definitions.clear();
        self.table.cell_definition_index = 0;
        self.table.pending_cell_merge = CellMerge::Normal;
        self.table.last_cell_boundary = None;
        self.table.row_width = 0;
        self.state_mut().in_table = true;
        Ok(())
    }

    pub(super) fn set_cell_merge(&mut self, merge: CellMerge) -> Result<(), ConversionError> {
        if !self.table.active {
            return Err(malformed("RTF cell merge property appears outside a table row"));
        }
        if self.table.pending_cell_merge != CellMerge::Normal {
            return Err(malformed("RTF cell has conflicting horizontal merge properties"));
        }
        self.table.pending_cell_merge = merge;
        Ok(())
    }

    pub(super) fn add_cell_definition(
        &mut self,
        parameter: Option<i64>,
    ) -> Result<(), ConversionError> {
        if !self.table.active {
            return Err(malformed("cellx appears outside a table row"));
        }
        let boundary = parameter_i32(parameter, "cell boundary")?;
        if boundary <= 0 {
            return Err(malformed("RTF cell boundary must be positive"));
        }
        if self.table.last_cell_boundary.is_some_and(|previous| boundary <= previous) {
            return Err(malformed("RTF cell boundaries must be strictly increasing"));
        }
        let next = self
            .table
            .cell_definitions
            .len()
            .checked_add(1)
            .ok_or_else(|| limit("max_table_columns", "RTF cell definition count overflow"))?;
        let maximum = self.effective_table_width();
        if u64::try_from(next).unwrap_or(u64::MAX) > maximum {
            return Err(limit("max_table_columns", format!("{next} > {maximum}")));
        }
        if self.table.pending_cell_merge == CellMerge::Continue {
            let previous = self.table.cell_definitions.last().copied();
            if !matches!(previous, Some(CellMerge::Start | CellMerge::Continue)) {
                return Err(malformed(
                    "horizontal merge continuation must immediately follow its origin chain",
                ));
            }
            let mut span = 1_usize;
            for merge in self.table.cell_definitions.iter().rev() {
                span = span
                    .checked_add(1)
                    .ok_or_else(|| limit("max_table_columns", "RTF cell span overflow"))?;
                if *merge == CellMerge::Start {
                    break;
                }
            }
            if u64::try_from(span).unwrap_or(u64::MAX) > maximum {
                return Err(limit("max_table_columns", format!("{span} > {maximum}")));
            }
        }
        reserve_vec(&mut self.table.cell_definitions, 1, &mut self.memory)?;
        self.table.cell_definitions.push(self.table.pending_cell_merge);
        self.table.pending_cell_merge = CellMerge::Normal;
        self.table.last_cell_boundary = Some(boundary);
        Ok(())
    }

    pub(super) fn finish_cell(&mut self, end: usize) -> Result<(), ConversionError> {
        if !self.table.active {
            return Err(malformed("RTF cell appears outside a table row"));
        }
        if self.table.pending_cell_merge != CellMerge::Normal {
            return Err(malformed("RTF cell merge property requires a cellx boundary"));
        }
        let next_width = self
            .table
            .row_width
            .checked_add(1)
            .ok_or_else(|| limit("max_table_columns", "RTF table width overflow"))?;
        let maximum = self.effective_table_width();
        if next_width > maximum {
            return Err(limit("max_table_columns", format!("{next_width} > {maximum}")));
        }
        let next_cells = self
            .table_cells
            .checked_add(1)
            .ok_or_else(|| limit("max_table_cells", "RTF table cell count overflow"))?;
        if next_cells > self.options.limits.max_table_cells {
            return Err(limit(
                "max_table_cells",
                format!("{next_cells} > {}", self.options.limits.max_table_cells),
            ));
        }
        let merge = self
            .table
            .cell_definitions
            .get(self.table.cell_definition_index)
            .copied()
            .unwrap_or(CellMerge::Normal);
        if merge == CellMerge::Continue {
            let span = self
                .table
                .cells
                .last()
                .ok_or_else(|| malformed("horizontal merge continuation has no origin cell"))?
                .column_span
                .checked_add(1)
                .ok_or_else(|| limit("max_table_columns", "RTF cell span overflow"))?;
            if u64::from(span) > maximum {
                return Err(limit("max_table_columns", format!("{span} > {maximum}")));
            }
            if !self.paragraph.inlines.is_empty()
                || self
                    .table
                    .cell_blocks
                    .iter()
                    .any(|node| !matches!(&node.block, Block::Paragraph(value) if value.is_empty()))
            {
                return Err(malformed(
                    "horizontal merge continuation contains displayable content",
                ));
            }
        }
        let paragraph_nodes = usize::from(!self.paragraph.inlines.is_empty());
        let empty_paragraph_nodes = usize::from(
            merge != CellMerge::Continue
                && self.table.cell_blocks.is_empty()
                && paragraph_nodes == 0,
        );
        let cell_nodes = usize::from(merge != CellMerge::Continue);
        self.ensure_document_nodes(paragraph_nodes + empty_paragraph_nodes + cell_nodes)?;
        if merge != CellMerge::Continue {
            self.consume_document_node()?;
        }
        self.finish_paragraph(end)?;
        if self.table.cell_blocks.is_empty() && merge != CellMerge::Continue {
            let empty = self.node(Block::Paragraph(Vec::new()), end, end)?;
            reserve_vec(&mut self.table.cell_blocks, 1, &mut self.memory)?;
            self.table.cell_blocks.push(empty);
        }
        self.table_cells = next_cells;
        self.table.row_width = next_width;
        self.table.cell_definition_index = self.table.cell_definition_index.saturating_add(1);
        let blocks = std::mem::take(&mut self.table.cell_blocks);
        if merge == CellMerge::Continue {
            let previous = self
                .table
                .cells
                .last_mut()
                .filter(|cell| cell.column_span >= 1)
                .ok_or_else(|| malformed("horizontal merge continuation has no origin cell"))?;
            previous.column_span = previous
                .column_span
                .checked_add(1)
                .ok_or_else(|| limit("max_table_columns", "RTF cell span overflow"))?;
            drop(blocks);
        } else {
            reserve_vec(&mut self.table.cells, 1, &mut self.memory)?;
            self.table.cells.push(Cell { row_span: 1, column_span: 1, header: false, blocks });
        }
        Ok(())
    }

    pub(super) fn finish_row(&mut self, end: usize) -> Result<(), ConversionError> {
        if !self.table.active {
            return Err(malformed("RTF row appears outside a table definition"));
        }
        if !self.paragraph.inlines.is_empty() || !self.table.cell_blocks.is_empty() {
            self.finish_cell(end)?;
        }
        if self.table.cells.is_empty() {
            return Err(malformed("RTF table row contains no cells"));
        }
        if !self.table.cell_definitions.is_empty()
            && self.table.row_width
                != u64::try_from(self.table.cell_definitions.len()).unwrap_or(u64::MAX)
        {
            return Err(malformed("RTF table cell definitions do not match the logical row width"));
        }
        let maximum = self.effective_table_width();
        if self.table.row_width == 0 || self.table.row_width > maximum {
            return Err(limit(
                "max_table_columns",
                format!("{} > {maximum}", self.table.row_width),
            ));
        }
        if let Some(width) = self.table.table_width {
            if width != self.table.row_width {
                return Err(malformed("RTF table rows have inconsistent logical widths"));
            }
        } else {
            self.table.table_width = Some(self.table.row_width);
        }
        if u64::try_from(self.table.rows.len()).unwrap_or(u64::MAX)
            >= self.options.limits.max_table_rows
        {
            return Err(limit(
                "max_table_rows",
                format!(">= {}", self.options.limits.max_table_rows),
            ));
        }
        reserve_vec(&mut self.table.rows, 1, &mut self.memory)?;
        self.table.rows.push(TableRow { cells: std::mem::take(&mut self.table.cells) });
        self.table.cell_definitions.clear();
        self.table.cell_definition_index = 0;
        self.table.last_cell_boundary = None;
        self.table.row_width = 0;
        Ok(())
    }

    pub(super) fn finish_table(&mut self, end: usize) -> Result<(), ConversionError> {
        if !self.paragraph.inlines.is_empty()
            || !self.table.cell_blocks.is_empty()
            || !self.table.cells.is_empty()
        {
            self.finish_row(end)?;
        }
        if !self.table.rows.is_empty() {
            let start = self
                .table
                .rows
                .first()
                .and_then(|row| row.cells.first())
                .and_then(|cell| cell.blocks.first())
                .and_then(|node| node.provenance.locator.byte_start)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(end);
            let rows = std::mem::take(&mut self.table.rows);
            if !self.table.node_reserved {
                return Err(ConversionError::Internal {
                    detail: "RTF table node was not reserved before row parsing".into(),
                });
            }
            let table =
                self.node_reserved(Block::Table { rows, alignments: Vec::new() }, start, end)?;
            self.push_block(table)?;
        }
        self.table.active = false;
        self.table.table_width = None;
        self.table.node_reserved = false;
        Ok(())
    }

    pub(super) fn finish_table_or_paragraph(&mut self, end: usize) -> Result<(), ConversionError> {
        if self.table.active { self.finish_table(end) } else { self.finish_paragraph(end) }
    }

    fn effective_table_width(&self) -> u64 {
        self.options.limits.max_table_columns.min(MAX_TABLE_COLUMNS as u64)
    }
}

fn list_marker(marker: &str) -> Result<(ListKind, u64), ConversionError> {
    let marker = marker.trim();
    if matches!(marker, "•" | "·") {
        return Ok((ListKind::Bullet, 1));
    }
    let digit_end = marker.bytes().take_while(u8::is_ascii_digit).count();
    let digits = marker
        .get(..digit_end)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed("RTF list marker kind cannot be determined"))?;
    let suffix = marker.get(digit_end..).unwrap_or_default().trim();
    if !matches!(suffix, "." | ")") {
        return Err(malformed("RTF ordered-list marker punctuation is ambiguous"));
    }
    let start = digits
        .parse::<u64>()
        .map_err(|_| limit("rtf_list_start", "ordered-list marker overflows u64"))?;
    if start == 0 {
        return Err(malformed("RTF ordered-list marker must start above zero"));
    }
    Ok((ListKind::Ordered, start))
}
