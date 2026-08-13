//! Paragraph, list, and table IR construction.

use super::budget::{limit, malformed, reserve_vec};
use super::parser::{CellMerge, Paragraph, Parser};
use into_markdown_core::{
    Block, Cell, ConversionError, Inline, ListItem, ListKind, MAX_TABLE_COLUMNS, TableRow,
};

impl Parser<'_> {
    pub(super) fn finish_paragraph(&mut self, end: usize) -> Result<(), ConversionError> {
        if self.paragraph.inlines.is_empty() {
            return Ok(());
        }
        let start = self.paragraph.start.unwrap_or(end);
        let inlines = std::mem::take(&mut self.paragraph.inlines);
        let block = self.node(Block::Paragraph(inlines), start, self.paragraph.end.max(end))?;
        self.paragraph = Paragraph::default();
        if self.table.active || self.state().in_table {
            reserve_vec(&mut self.table.cell_blocks, 1, &mut self.memory)?;
            self.table.cell_blocks.push(block);
        } else if self.state().list_id.is_some() || self.pending_list_marker.is_some() {
            let marker = self.pending_list_marker.take();
            let kind = if marker
                .as_deref()
                .is_some_and(|value| value.contains('•') || value.contains('·'))
            {
                ListKind::Bullet
            } else {
                ListKind::Ordered
            };
            let mut item_blocks = Vec::new();
            reserve_vec(&mut item_blocks, 1, &mut self.memory)?;
            item_blocks.push(block);
            let item = ListItem { checked: None, marker_label: marker, blocks: item_blocks };
            let mut items = Vec::new();
            reserve_vec(&mut items, 1, &mut self.memory)?;
            items.push(item);
            let list = self.node(Block::List { kind, start: 1, items }, start, end)?;
            self.push_block(list)?;
        } else {
            self.push_block(block)?;
        }
        Ok(())
    }

    pub(super) fn finish_field_result(&mut self) -> Result<(), ConversionError> {
        let Some(start) = self.field_inline_start.take() else {
            return Ok(());
        };
        let Some(target) = self.active_link.take() else {
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
        if !self.table.cells.is_empty() {
            self.finish_row(end)?;
        }
        self.table.active = true;
        self.table.cell_definitions.clear();
        self.table.cell_definition_index = 0;
        self.table.pending_cell_merge = CellMerge::Normal;
        self.state_mut().in_table = true;
        Ok(())
    }

    pub(super) fn finish_cell(&mut self, end: usize) -> Result<(), ConversionError> {
        self.finish_paragraph(end)?;
        if self.table.cell_blocks.is_empty() {
            let empty = self.node(Block::Paragraph(Vec::new()), end, end)?;
            reserve_vec(&mut self.table.cell_blocks, 1, &mut self.memory)?;
            self.table.cell_blocks.push(empty);
        }
        self.table_cells = self
            .table_cells
            .checked_add(1)
            .ok_or_else(|| limit("max_table_cells", "RTF table cell count overflow"))?;
        if self.table_cells > self.options.limits.max_table_cells {
            return Err(limit(
                "max_table_cells",
                format!("{} > {}", self.table_cells, self.options.limits.max_table_cells),
            ));
        }
        let merge = self
            .table
            .cell_definitions
            .get(self.table.cell_definition_index)
            .copied()
            .unwrap_or(CellMerge::Normal);
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
            if blocks
                .iter()
                .any(|node| !matches!(&node.block, Block::Paragraph(value) if value.is_empty()))
            {
                return Err(malformed(
                    "horizontal merge continuation contains displayable content",
                ));
            }
        } else {
            reserve_vec(&mut self.table.cells, 1, &mut self.memory)?;
            self.table.cells.push(Cell { row_span: 1, column_span: 1, header: false, blocks });
        }
        Ok(())
    }

    pub(super) fn finish_row(&mut self, end: usize) -> Result<(), ConversionError> {
        if !self.paragraph.inlines.is_empty() || !self.table.cell_blocks.is_empty() {
            self.finish_cell(end)?;
        }
        if self.table.cells.is_empty() {
            return Err(malformed("RTF table row contains no cells"));
        }
        if self.table.cells.len() > MAX_TABLE_COLUMNS {
            return Err(limit(
                "max_table_columns",
                format!("{} > {MAX_TABLE_COLUMNS}", self.table.cells.len()),
            ));
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
            let table = self.node(Block::Table { rows, alignments: Vec::new() }, start, end)?;
            self.push_block(table)?;
        }
        self.table.active = false;
        Ok(())
    }

    pub(super) fn finish_table_or_paragraph(&mut self, end: usize) -> Result<(), ConversionError> {
        if self.table.active { self.finish_table(end) } else { self.finish_paragraph(end) }
    }
}
