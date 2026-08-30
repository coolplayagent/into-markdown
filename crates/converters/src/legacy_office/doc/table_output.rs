use super::{emit_story, field_inlines, tables::Row, visible_field_text};
use crate::legacy_office::budget::{LegacyBudget, malformed};
use crate::legacy_office::builder::{OutputBuilder, locator};
use crate::legacy_office::tables::rectangularize;
use into_markdown_core::{Block, Cell, ConversionError, TableRow};
use std::collections::BTreeMap;

pub(super) fn emit(
    text: &str,
    rows: &[Row],
    builder: &mut OutputBuilder,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let mut positions = rows
        .iter()
        .flat_map(|row| [row.start, row.end])
        .map(|cp| (cp, None))
        .collect::<BTreeMap<_, _>>();
    let mut cp = 0;
    for (byte, character) in text.char_indices() {
        if let Some(position) = positions.get_mut(&cp) {
            *position = Some(byte);
        }
        cp += character.len_utf16();
    }
    if let Some(position) = positions.get_mut(&cp) {
        *position = Some(text.len());
    }
    let rows = rows.iter().filter(|row| row.end <= cp).collect::<Vec<_>>();
    let mut cursor = 0;
    let mut index = 0;
    while index < rows.len() {
        let first = index;
        let start = positions[&rows[index].start]
            .ok_or_else(|| malformed("WordDocument/PAPX", "row starts inside a character"))?;
        if start < cursor {
            index += 1;
            continue;
        }
        emit_story(&text[cursor..start], builder, budget)?;
        index += 1;
        while index < rows.len() && rows[index].start == rows[index - 1].end {
            index += 1;
        }
        let end = positions[&rows[index - 1].end]
            .ok_or_else(|| malformed("WordDocument/PAPX", "row ends inside a character"))?;
        if let Some(mut table) = make_rows(text, &rows[first..index], &positions, builder, budget)?
        {
            rectangularize(&mut table, builder, budget, "WordDocument/table")?;
            builder.push(
                Block::Table { rows: table, alignments: Vec::new() },
                locator("WordDocument"),
            );
        } else {
            builder.warning(
                "legacyOffice.doc.tableGeometryOmitted",
                "cell marks disagree with direct row properties; text and empty cells were retained without guessing merges",
                Some(locator("WordDocument/table")),
            );
            emit_story(&text[start..end], builder, budget)?;
        }
        cursor = end;
    }
    emit_story(&text[cursor..], builder, budget)
}

fn make_rows(
    text: &str,
    rows: &[&Row],
    positions: &BTreeMap<usize, Option<usize>>,
    builder: &mut OutputBuilder,
    budget: &LegacyBudget<'_>,
) -> Result<Option<Vec<TableRow>>, ConversionError> {
    let mut grid = rows.iter().flat_map(|row| row.edges.iter().copied()).collect::<Vec<_>>();
    grid.sort_unstable();
    grid.dedup();
    budget.table_shape(rows.len(), grid.len().saturating_sub(1))?;
    let mut output = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let start = positions[&row.start]
            .ok_or_else(|| malformed("WordDocument/table", "invalid row start"))?;
        let end = positions[&row.end]
            .ok_or_else(|| malformed("WordDocument/table", "invalid row end"))?;
        let raw = visible_field_text(&text[start..end]);
        let raw = raw.strip_suffix('\u{7}').unwrap_or(&raw);
        let values = raw.split_terminator('\u{7}').collect::<Vec<_>>();
        if values.len() != row.flags.len() {
            return Ok(None);
        }
        let mut cells = Vec::new();
        for _ in 0..grid.partition_point(|edge| *edge < row.edges[0]) {
            cells.push(Cell { row_span: 1, column_span: 1, header: false, blocks: Vec::new() });
        }
        let mut index = 0;
        while index < values.len() {
            let first = index;
            let flags = row.flags[index];
            index += 1;
            if flags & 3 >= 2 {
                while index < values.len() && row.flags[index] & 3 == 1 {
                    index += 1;
                }
            }
            let left = row.edges[first];
            let right = row.edges[index];
            let column_span = grid.partition_point(|edge| *edge < right)
                - grid.partition_point(|edge| *edge < left);
            let mut row_span = 1;
            if (flags >> 5) & 3 == 3 {
                for next in &rows[row_index + 1..] {
                    if !vertical_continuation(next, left, right) {
                        break;
                    }
                    row_span += 1;
                }
            }
            if (flags >> 5) & 3 == 1
                && row_index > 0
                && vertical_start_above(&rows[..row_index], left, right)
            {
                if values[first..index].iter().any(|value| !value.trim().is_empty()) {
                    return Ok(None);
                }
                continue;
            }
            let contents = values[first..index].join("\n").replace('\r', "\n");
            cells.push(Cell {
                row_span,
                column_span: u32::try_from(column_span).unwrap_or(u32::MAX),
                header: false,
                blocks: vec![
                    builder
                        .node(Block::Paragraph(field_inlines(&contents)), locator("WordDocument")),
                ],
            });
        }
        output.push(TableRow { cells });
    }
    Ok(Some(output))
}

fn vertical_continuation(row: &Row, left: i16, right: i16) -> bool {
    vertical_flag(row, left, right) == Some(1)
}

fn vertical_start_above(rows: &[&Row], left: i16, right: i16) -> bool {
    for row in rows.iter().rev() {
        match vertical_flag(row, left, right) {
            Some(3) => return true,
            Some(1) => {}
            _ => return false,
        }
    }
    false
}

fn vertical_flag(row: &Row, left: i16, right: i16) -> Option<u16> {
    let mut index = 0;
    while index < row.flags.len() {
        let first = index;
        index += 1;
        if row.flags[first] & 3 >= 2 {
            while index < row.flags.len() && row.flags[index] & 3 == 1 {
                index += 1;
            }
        }
        if row.edges[first] == left && row.edges[index] == right {
            return Some((row.flags[first] >> 5) & 3);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        ConversionOptions, ConverterOutput, ExecutionContext, ExecutionOptions,
    };

    fn convert(text: &str, rows: &[Row]) -> ConverterOutput {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut budget = LegacyBudget::new(text.len(), &options, &context).unwrap();
        let mut builder = OutputBuilder::new("doc");
        emit(text, rows, &mut builder, &mut budget).unwrap();
        let output = builder.finish();
        output.document.validate().unwrap();
        output
    }

    #[test]
    fn source_grid_keeps_empty_cells_cross_columns_and_cell_paragraphs() {
        let text = "before\rA\x07B\x07\x07C\x07\x07D\rE\x07\x07after\r";
        let rows = [
            Row { start: 7, end: 12, edges: vec![0, 20, 30], flags: vec![0, 0] },
            Row { start: 12, end: 20, edges: vec![0, 10, 20, 30], flags: vec![0, 0, 0] },
        ];
        let output = convert(text, &rows);
        assert_eq!(output.document.blocks.len(), 3);
        let Block::Table { rows, .. } = &output.document.blocks[1].block else { panic!("table") };
        assert_eq!(rows[0].cells[0].column_span, 2);
        assert_eq!(rows[1].cells.len(), 3);
        assert_eq!(rows[1].cells[1].blocks[0].block, Block::Paragraph(Vec::new()));
        assert_eq!(rows[1].cells[2].blocks[0].block, Block::Paragraph(field_inlines("D\nE")));
    }

    #[test]
    fn vertical_merge_uses_explicit_flags_not_blank_text() {
        let text = "A\x07B\x07\x07\x07C\x07\x07";
        let rows = [
            Row { start: 0, end: 5, edges: vec![0, 10, 20], flags: vec![0x60, 0] },
            Row { start: 5, end: 9, edges: vec![0, 10, 20], flags: vec![0x20, 0] },
        ];
        let output = convert(text, &rows);
        let Block::Table { rows, .. } = &output.document.blocks[0].block else { panic!("table") };
        assert_eq!(rows[0].cells[0].row_span, 2);
        assert_eq!(rows[1].cells.len(), 1);
    }

    #[test]
    fn horizontal_and_vertical_merges_share_the_same_source_interval() {
        let text = "A\x07\x07B\x07\x07\x07\x07C\x07\x07";
        let rows = [
            Row { start: 0, end: 6, edges: vec![0, 10, 20, 30], flags: vec![0x62, 1, 0] },
            Row { start: 6, end: 11, edges: vec![0, 10, 20, 30], flags: vec![0x22, 1, 0] },
        ];
        let output = convert(text, &rows);
        let Block::Table { rows, .. } = &output.document.blocks[0].block else { panic!("table") };
        assert_eq!((rows[0].cells[0].row_span, rows[0].cells[0].column_span), (2, 2));
        assert_eq!(rows[1].cells.len(), 1);
    }

    #[test]
    fn inconsistent_properties_keep_the_table_with_a_diagnostic() {
        let text = "A\x07\x07B\x07\x07";
        let rows = [Row { start: 0, end: 6, edges: vec![0, 10], flags: vec![0] }];
        let output = convert(text, &rows);
        assert!(matches!(output.document.blocks[0].block, Block::Table { .. }));
        assert!(
            output
                .diagnostics
                .iter()
                .any(|item| item.code == "legacyOffice.doc.tableGeometryOmitted")
        );
        let encoded = serde_json::to_string(&output.document).unwrap();
        assert!(encoded.contains("\"value\":\"A\""));
        assert!(encoded.contains("\"value\":\"B\""));
    }
}
