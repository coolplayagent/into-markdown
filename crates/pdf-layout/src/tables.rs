use crate::budget::LayoutBudget;
use crate::geometry::{major_end, major_start, union};
use crate::model::{Line, RebuiltBlock, block_provenance};
use crate::ordering;
use crate::semantics::take_line_inlines;
use crate::{LayoutConfig, memory};
use into_markdown_core::{Block, BlockNode, Cell, ConversionError, Inline, NodeId, TableRow};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::ops::Range;

const REPEATED_START_TOLERANCE_HEIGHTS: f32 = 0.16;

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
        let run = candidate_run(first, &mut pending, config, budget)?;
        let decisions = evidence_windows(&run, budget)?;
        let mut cursor = 0_usize;
        let mut owned = run.into_iter();
        for decision in decisions {
            let window = decision.range;
            while cursor < window.start {
                remaining
                    .push(owned.next().ok_or_else(|| memory("layout table window prefix"))?.line);
                cursor += 1;
            }
            if decision.evidence.is_none() {
                while cursor < window.end {
                    remaining.push(
                        owned.next().ok_or_else(|| memory("layout rejected table window"))?.line,
                    );
                    cursor += 1;
                }
                continue;
            }
            let row_count = window.end - window.start;
            tables.push(materialize_window(
                &mut owned,
                row_count,
                page,
                width,
                height,
                sequence,
                decision.header,
                config,
                &mut total_cells,
                budget,
            )?);
            cursor += row_count;
            sequence += 1;
        }
        for evidence in owned {
            remaining.push(evidence.line);
        }
    }
    Ok((tables, remaining))
}

/// Gather the maximal run that shares a stable column-start profile. Table
/// evidence is deliberately not considered here: a run can contain ambiguous
/// column text followed by a locally provable table with the same starts.
fn candidate_run(
    first: Line,
    pending: &mut VecDeque<Line>,
    config: &LayoutConfig,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<RowEvidence>, ConversionError> {
    let first_segments = segments(&first, budget)?;
    let mut run = Vec::new();
    run.try_reserve_exact(1).map_err(|_| memory("layout table run"))?;
    if !row_candidate(&first_segments, config) {
        run.push(RowEvidence::new(first, first_segments));
        return Ok(run);
    }
    run.push(RowEvidence::new(first, first_segments));
    while let Some(next) = pending.front() {
        let next_segments = segments(next, budget)?;
        if !row_candidate(&next_segments, config)
            || !compatible_rows(
                run.last().ok_or_else(|| memory("layout table run"))?,
                next,
                &next_segments,
                budget,
            )?
        {
            break;
        }
        let next = pending.pop_front().ok_or_else(|| memory("layout table run"))?;
        run.try_reserve(1).map_err(|_| memory("layout table run"))?;
        run.push(RowEvidence::new(next, next_segments));
    }
    Ok(run)
}

#[derive(Clone, Copy)]
struct Segment {
    start: usize,
    end: usize,
    x: f32,
    right: f32,
}

struct RowEvidence {
    line: Line,
    segments: Vec<Segment>,
    compact: bool,
}

impl RowEvidence {
    fn new(line: Line, segments: Vec<Segment>) -> Self {
        let compact = segments
            .iter()
            .all(|segment| segment.right - segment.x <= line.bounds.height.max(1.0) * 8.0);
        Self { line, segments, compact }
    }
}

struct WindowDecision {
    range: Range<usize>,
    evidence: Option<GridEvidence>,
    header: bool,
}

/// Find geometry-backed table windows without using font metadata to create
/// or split candidates. A run of compact rows with repeated column starts is
/// independently provable. It may absorb at most one immediately preceding
/// non-compact row after font or height confirms that row as its header.
/// Ambiguous text before or after a proven window remains one conservative
/// paragraph window and is never reconsidered as overlapping two-row slices.
fn evidence_windows(
    rows: &[RowEvidence],
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<WindowDecision>, ConversionError> {
    let mut windows = Vec::new();
    windows.try_reserve_exact(rows.len()).map_err(|_| memory("layout evidence windows"))?;
    let mut plain_start = 0_usize;
    let mut index = 0_usize;
    while index < rows.len() {
        budget.checkpoint_item()?;
        if !rows[index].compact {
            index += 1;
            continue;
        }
        let body_start = index;
        index += 1;
        while index < rows.len()
            && rows[index].compact
            && repeated_starts(&rows[index - 1], &rows[index], budget)?
        {
            index += 1;
        }
        let body_end = index;
        let absorb_header = body_start > plain_start
            && !rows[body_start - 1].compact
            && repeated_starts(&rows[body_start - 1], &rows[body_start], budget)?
            && confirms_header(&rows[body_start - 1], &rows[body_start]);
        let body_evidence = compact_body_evidence(&rows[body_start..body_end], budget)?;
        if body_evidence.is_none() && !absorb_header {
            continue;
        }
        let table_start = if absorb_header { body_start - 1 } else { body_start };
        push_plain_window(&mut windows, plain_start..table_start)?;
        let table_range = table_start..body_end;
        windows.try_reserve(1).map_err(|_| memory("layout evidence windows"))?;
        windows.push(WindowDecision {
            header: absorb_header || header_likely(&rows[table_range.clone()]),
            range: table_range,
            evidence: body_evidence.or(Some(GridEvidence::AlignedCompactBody)),
        });
        plain_start = body_end;
    }
    if plain_start < rows.len() {
        let range = plain_start..rows.len();
        let evidence = if range.len() == 2
            && repeated_boundaries(&rows[range.start], &rows[range.start + 1], budget)?
        {
            Some(GridEvidence::RepeatedBoundaries)
        } else {
            None
        };
        windows.try_reserve(1).map_err(|_| memory("layout evidence windows"))?;
        windows.push(WindowDecision {
            header: evidence.is_some() && header_likely(&rows[range.clone()]),
            range,
            evidence,
        });
    }
    Ok(windows)
}

fn push_plain_window(
    windows: &mut Vec<WindowDecision>,
    range: Range<usize>,
) -> Result<(), ConversionError> {
    if range.is_empty() {
        return Ok(());
    }
    windows.try_reserve(1).map_err(|_| memory("layout evidence windows"))?;
    windows.push(WindowDecision { range, evidence: None, header: false });
    Ok(())
}

fn confirms_header(previous: &RowEvidence, next: &RowEvidence) -> bool {
    let previous_font = previous.line.font_size.unwrap_or(previous.line.bounds.height.max(1.0));
    let next_font = next.line.font_size.unwrap_or(next.line.bounds.height.max(1.0));
    previous_font >= next_font * 1.08
}

fn compact_body_evidence(
    rows: &[RowEvidence],
    budget: &mut LayoutBudget<'_>,
) -> Result<Option<GridEvidence>, ConversionError> {
    if rows.len() >= 3 {
        return Ok(Some(GridEvidence::AlignedCompactBody));
    }
    if rows.len() == 2 && (header_likely(rows) || repeated_boundaries(&rows[0], &rows[1], budget)?)
    {
        return Ok(Some(GridEvidence::AlignedCompactBody));
    }
    Ok(None)
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

fn row_candidate(segments: &[Segment], config: &LayoutConfig) -> bool {
    segments.len() >= 2
        && segments.len() <= config.limits.max_table_columns
        && segments.iter().all(|segment| segment.end.saturating_sub(segment.start) <= 256)
}

/// Auditable geometry that can establish a table before style metadata is
/// consulted. Identical broad columns longer than two rows have neither signal
/// and deliberately remain paragraphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GridEvidence {
    RepeatedBoundaries,
    AlignedCompactBody,
}

fn compatible_rows(
    previous: &RowEvidence,
    next: &Line,
    next_segments: &[Segment],
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    if next.orientation != 0 || previous.segments.len() != next_segments.len() {
        return Ok(false);
    }
    let height = previous.line.bounds.height.max(next.bounds.height).max(1.0);
    let gap = next.bounds.y - (previous.line.bounds.y + previous.line.bounds.height);
    if gap < -height * 0.25 || gap > height * 2.5 {
        return Ok(false);
    }
    for (expected, actual) in previous.segments.iter().zip(next_segments) {
        budget.compare()?;
        if (expected.x - actual.x).abs() > height * 1.5 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn repeated_boundaries(
    first: &RowEvidence,
    next: &RowEvidence,
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    if first.segments.len() != next.segments.len() {
        return Ok(false);
    }
    let tolerance = first.line.bounds.height.max(next.line.bounds.height).max(1.0) * 0.08;
    for (expected, actual) in first.segments.iter().zip(&next.segments) {
        budget.compare()?;
        if (expected.x - actual.x).abs() > tolerance.max(0.5)
            || (expected.right - actual.right).abs() > tolerance.max(0.5)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn repeated_starts(
    first: &RowEvidence,
    next: &RowEvidence,
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    if first.segments.len() != next.segments.len() {
        return Ok(false);
    }
    // PDFium reports glyph ink bounds rather than text origins, so different
    // leading glyphs in the same column can shift by a small fraction of the
    // line height. This remains far tighter than the candidate-run tolerance.
    let tolerance = first.line.bounds.height.max(next.line.bounds.height).max(1.0)
        * REPEATED_START_TOLERANCE_HEIGHTS;
    for (expected, actual) in first.segments.iter().zip(&next.segments) {
        budget.compare()?;
        if (expected.x - actual.x).abs() > tolerance.max(0.5) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn materialize_window(
    owned: &mut std::vec::IntoIter<RowEvidence>,
    row_count: usize,
    page: u32,
    width: f32,
    height: f32,
    sequence: usize,
    header: bool,
    config: &LayoutConfig,
    total_cells: &mut usize,
    budget: &mut LayoutBudget<'_>,
) -> Result<RebuiltBlock, ConversionError> {
    let mut first = Some(owned.next().ok_or_else(|| memory("layout table window"))?);
    let first_row = first.as_ref().ok_or_else(|| memory("layout table window"))?;
    let columns = first_row.segments.len();
    let cells = row_count.checked_mul(columns).ok_or_else(|| memory("layout table cells"))?;
    *total_cells = total_cells.checked_add(cells).ok_or_else(|| memory("layout table cells"))?;
    if *total_cells > config.limits.max_table_cells {
        return Err(crate::limit(
            "pdfLayoutTableCells",
            format!("{total_cells} > {}", config.limits.max_table_cells),
        ));
    }
    let mut rows = Vec::new();
    rows.try_reserve_exact(row_count).map_err(|_| memory("layout table rows"))?;
    let mut table_bounds = first_row.line.bounds;
    let source_index = first_row.line.source_index;
    let mut confidence = None;
    for row_index in 0..row_count {
        budget.checkpoint_item()?;
        let evidence =
            first.take().or_else(|| owned.next()).ok_or_else(|| memory("layout table run"))?;
        table_bounds = union(table_bounds, evidence.line.bounds);
        confidence = min_confidence(confidence, confidence_of(&evidence.line));
        rows.push(materialize_row(
            evidence.line,
            &evidence.segments,
            page,
            width,
            height,
            sequence,
            row_index,
            header && row_index == 0,
            budget,
        )?);
    }
    Ok(RebuiltBlock {
        node: BlockNode {
            id: table_id(page, sequence, None, None)?,
            block: Block::Table { rows, alignments: Vec::new() },
            provenance: block_provenance(page, table_bounds, width, height, confidence),
        },
        bounds: Some(table_bounds),
        orientation: 0,
        source_index,
    })
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

fn header_likely(rows: &[RowEvidence]) -> bool {
    let Some(first) = rows.first().and_then(|row| row.line.font_size) else { return false };
    let Some(body) = rows.iter().skip(1).filter_map(|row| row.line.font_size).reduce(f32::midpoint)
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
