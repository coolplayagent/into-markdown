use crate::budget::LayoutBudget;
use crate::geometry::{major_end, major_start, union};
use crate::model::{Line, RebuiltBlock, block_provenance};
use crate::ordering;
use crate::semantics::take_line_inlines;
use crate::{LayoutConfig, memory};
use into_markdown_core::{Block, BlockNode, Cell, ConversionError, Inline, NodeId, TableRow};
use std::collections::VecDeque;
use std::fmt::Write as _;

pub(crate) fn recover(
    mut lines: Vec<Line>,
    page: u32,
    width: f32,
    height: f32,
    config: &LayoutConfig,
    budget: &mut LayoutBudget<'_>,
) -> Result<(Vec<RebuiltBlock>, Vec<Line>), ConversionError> {
    ordering::by(&mut lines, budget, |left, right| {
        left.bounds
            .y
            .total_cmp(&right.bounds.y)
            .then_with(|| left.bounds.x.total_cmp(&right.bounds.x))
            .then_with(|| left.source_index.cmp(&right.source_index))
    })?;
    let mut pending = VecDeque::new();
    pending.try_reserve_exact(lines.len()).map_err(|_| memory("layout table queue"))?;
    pending.extend(lines);
    let mut tables = Vec::new();
    tables.try_reserve_exact(pending.len() / 2).map_err(|_| memory("layout table output"))?;
    let mut remaining = Vec::new();
    remaining.try_reserve_exact(pending.len()).map_err(|_| memory("layout table remainder"))?;
    let mut sequence = 0_usize;
    let mut total_cells = 0_usize;
    while let Some(first) = pending.pop_front() {
        budget.checkpoint_item()?;
        let first_segments = segments(&first, budget)?;
        if !row_candidate(&first, &first_segments, width, config) {
            remaining.push(first);
            continue;
        }
        let mut run = 1_usize;
        let mut prior = &first;
        for next in &pending {
            let next_segments = segments(next, budget)?;
            if !compatible_rows(prior, &first_segments, next, &next_segments, width, budget)? {
                break;
            }
            run += 1;
            prior = next;
        }
        if run < 2 {
            remaining.push(first);
            continue;
        }
        let mut source_rows = Vec::new();
        source_rows.try_reserve_exact(run).map_err(|_| memory("layout source row evidence"))?;
        source_rows.push(&first);
        source_rows.extend(pending.iter().take(run - 1));
        if !strong_grid(&source_rows, &first_segments, budget)? {
            remaining.push(first);
            continue;
        }
        let columns = first_segments.len();
        let cells = run.checked_mul(columns).ok_or_else(|| memory("layout table cells"))?;
        total_cells = total_cells.checked_add(cells).ok_or_else(|| memory("layout table cells"))?;
        if total_cells > config.limits.max_table_cells {
            return Err(crate::limit(
                "pdfLayoutTableCells",
                format!("{total_cells} > {}", config.limits.max_table_cells),
            ));
        }
        let mut rows = Vec::new();
        rows.try_reserve_exact(run).map_err(|_| memory("layout table rows"))?;
        let header = header_likely(&source_rows);
        let mut owned_rows = Vec::new();
        owned_rows.try_reserve_exact(run).map_err(|_| memory("layout source rows"))?;
        owned_rows.push(first);
        for _ in 1..run {
            owned_rows.push(pending.pop_front().ok_or_else(|| memory("layout table run"))?);
        }
        let mut table_bounds = owned_rows[0].bounds;
        let source_index = owned_rows[0].source_index;
        let mut confidence = None;
        for (row_index, row) in owned_rows.into_iter().enumerate() {
            table_bounds = union(table_bounds, row.bounds);
            confidence = min_confidence(confidence, confidence_of(&row));
            let ranges = segments(&row, budget)?;
            rows.push(materialize_row(
                row,
                &ranges,
                page,
                width,
                height,
                sequence,
                row_index,
                header && row_index == 0,
                budget,
            )?);
        }
        tables.push(RebuiltBlock {
            node: BlockNode {
                id: table_id(page, sequence, None, None)?,
                block: Block::Table { rows, alignments: Vec::new() },
                provenance: block_provenance(page, table_bounds, width, height, confidence),
            },
            bounds: Some(table_bounds),
            orientation: 0,
            source_index,
        });
        sequence += 1;
    }
    Ok((tables, remaining))
}

#[derive(Clone, Copy)]
struct Segment {
    start: usize,
    end: usize,
    x: f32,
    right: f32,
}

fn segments(line: &Line, budget: &mut LayoutBudget<'_>) -> Result<Vec<Segment>, ConversionError> {
    if line.orientation != 0 || line.atoms.is_empty() {
        return Ok(Vec::new());
    }
    let threshold = line.bounds.height.max(1.0) * 1.35;
    let mut output = Vec::new();
    output.try_reserve_exact(line.atoms.len()).map_err(|_| memory("layout table segments"))?;
    let mut start = 0;
    for index in 1..line.atoms.len() {
        budget.compare()?;
        let gap =
            major_start(line.atoms[index].bounds, 0) - major_end(line.atoms[index - 1].bounds, 0);
        if gap > threshold {
            output.push(segment(line, start, index));
            start = index;
        }
    }
    output.push(segment(line, start, line.atoms.len()));
    Ok(output)
}

fn segment(line: &Line, start: usize, end: usize) -> Segment {
    let x = line.atoms[start].bounds.x;
    let right = line.atoms[end - 1].bounds.x + line.atoms[end - 1].bounds.width;
    Segment { start, end, x, right }
}

fn row_candidate(
    line: &Line,
    segments: &[Segment],
    page_width: f32,
    config: &LayoutConfig,
) -> bool {
    segments.len() >= 2
        && segments.len() <= config.limits.max_table_columns
        && segments.iter().all(|segment| {
            segment.right - segment.x <= page_width * 0.42
                && segment.end.saturating_sub(segment.start) <= 256
        })
        && line.bounds.width >= page_width * 0.20
}

/// Require independent two-dimensional evidence before claiming a table.
/// Three or more repeated columns across three rows form a grid. A two-row
/// grid instead needs a typographic header or repeated left and right cell
/// boundaries; coincidentally aligned flowing columns normally lack both.
fn strong_grid(
    rows: &[&Line],
    first_segments: &[Segment],
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    if rows.len() >= 3 && first_segments.len() >= 3 {
        return Ok(true);
    }
    if header_likely(rows) {
        return Ok(true);
    }
    for row in rows.iter().skip(1) {
        let actual = segments(row, budget)?;
        if actual.len() != first_segments.len() {
            return Ok(false);
        }
        let tolerance = row.bounds.height.max(rows[0].bounds.height).max(1.0) * 0.08;
        for (expected, actual) in first_segments.iter().zip(actual) {
            budget.compare()?;
            if (expected.x - actual.x).abs() > tolerance.max(0.5)
                || (expected.right - actual.right).abs() > tolerance.max(0.5)
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn compatible_rows(
    previous: &Line,
    first_segments: &[Segment],
    next: &Line,
    next_segments: &[Segment],
    page_width: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    if next.orientation != 0 || first_segments.len() != next_segments.len() {
        return Ok(false);
    }
    let height = previous.bounds.height.max(next.bounds.height).max(1.0);
    let gap = next.bounds.y - (previous.bounds.y + previous.bounds.height);
    if gap < -height * 0.25 || gap > height * 2.5 {
        return Ok(false);
    }
    for (expected, actual) in first_segments.iter().zip(next_segments) {
        budget.compare()?;
        if (expected.x - actual.x).abs() > height * 1.5
            || actual.right - actual.x > page_width * 0.42
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn materialize_row(
    line: Line,
    ranges: &[Segment],
    page: u32,
    width: f32,
    height: f32,
    table_sequence: usize,
    row_index: usize,
    header: bool,
    budget: &mut LayoutBudget<'_>,
) -> Result<TableRow, ConversionError> {
    let mut cells = Vec::new();
    cells.try_reserve_exact(ranges.len()).map_err(|_| memory("layout table cells"))?;
    let mut indexed = line.atoms.into_iter().enumerate().peekable();
    for (column, range) in ranges.iter().enumerate() {
        budget.checkpoint_item()?;
        let capacity =
            range.end.checked_sub(range.start).ok_or_else(|| memory("layout table range"))?;
        let mut atoms = Vec::new();
        atoms.try_reserve_exact(capacity).map_err(|_| memory("layout table cell atoms"))?;
        while indexed.peek().is_some_and(|(index, _)| *index < range.end) {
            budget.checkpoint_item()?;
            let (index, atom) = indexed.next().ok_or_else(|| memory("layout table atom"))?;
            if index < range.start {
                return Err(memory("layout table atom range"));
            }
            atoms.push(atom);
        }
        if atoms.len() != capacity {
            return Err(memory("layout table atom range"));
        }
        let bounds = atoms
            .iter()
            .map(|atom| atom.bounds)
            .reduce(union)
            .ok_or_else(|| memory("layout empty table cell"))?;
        let font_size = atoms.iter().filter_map(|atom| atom.font_size).reduce(f32::midpoint);
        let source_index = atoms[0].source_index;
        let source_kind = atoms[0].source_kind;
        let cell_line =
            Line { atoms, bounds, font_size, orientation: 0, source_index, source_kind };
        let confidence = confidence_of(&cell_line);
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(1).map_err(|_| memory("layout table cell blocks"))?;
        blocks.push(BlockNode {
            id: table_id(page, table_sequence, Some(row_index), Some(column))?,
            block: Block::Paragraph(take_line_inlines(cell_line)?),
            provenance: block_provenance(page, bounds, width, height, confidence),
        });
        cells.push(Cell { row_span: 1, column_span: 1, header, blocks });
    }
    if indexed.next().is_some() {
        return Err(memory("layout table trailing atom"));
    }
    Ok(TableRow { cells })
}

fn table_id(
    page: u32,
    sequence: usize,
    row: Option<usize>,
    column: Option<usize>,
) -> Result<NodeId, ConversionError> {
    let mut value = String::new();
    value.try_reserve_exact(96).map_err(|_| memory("layout table node id"))?;
    write!(value, "pdf-page-{page}-layout-table-{sequence}")
        .map_err(|_| memory("layout table node id"))?;
    if let (Some(row), Some(column)) = (row, column) {
        write!(value, "-r{row}-c{column}").map_err(|_| memory("layout table cell id"))?;
    }
    Ok(NodeId(value))
}

fn header_likely(rows: &[&Line]) -> bool {
    let Some(first) = rows.first().and_then(|row| row.font_size) else { return false };
    let Some(body) = rows.iter().skip(1).filter_map(|row| row.font_size).reduce(f32::midpoint)
    else {
        return false;
    };
    first >= body * 1.08
}

fn confidence_of(line: &Line) -> Option<f32> {
    line.atoms
        .iter()
        .filter_map(|atom| match &atom.inline {
            Inline::SourceText { provenance, .. } | Inline::OcrText { provenance, .. } => {
                provenance.confidence
            }
            _ => None,
        })
        .reduce(f32::min)
}

fn min_confidence(left: Option<f32>, right: Option<f32>) -> Option<f32> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}
