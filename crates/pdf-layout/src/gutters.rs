use crate::budget::LayoutBudget;
use crate::geometry::{major_end, major_start, union};
use crate::memory;
use crate::model::Line;
use into_markdown_core::ConversionError;

const MIN_GUTTER_RATIO: f32 = 0.12;
const MIN_COMMON_GUTTER_RATIO: f32 = 0.08;
const MIN_COLUMN_SPAN_RATIO: f32 = 0.18;
const GUTTER_CENTER_TOLERANCE_RATIO: f32 = 0.06;
const MIN_ROW_OFFSET_HEIGHTS: f32 = 0.25;
const MAX_ROW_GAP_HEIGHTS: f32 = 4.0;

/// Split page columns only when a gutter is corroborated by multiple
/// vertically-adjacent lines with substantial content on both sides.
pub(crate) fn split(
    lines: Vec<Line>,
    page_width: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Line>, ConversionError> {
    let candidate_capacity = lines.iter().try_fold(0_usize, |total, line| {
        total
            .checked_add(line.atoms.len().saturating_sub(1))
            .ok_or_else(|| memory("layout gutter candidate count"))
    })?;
    let mut candidates = Vec::new();
    candidates
        .try_reserve_exact(candidate_capacity)
        .map_err(|_| memory("layout gutter candidates"))?;
    for (line_index, line) in lines.iter().enumerate() {
        if line.orientation != 0 || line.atoms.len() < 2 {
            continue;
        }
        let line_start = major_start(line.atoms[0].bounds, 0);
        let line_end = major_end(line.atoms[line.atoms.len() - 1].bounds, 0);
        for cut_index in 1..line.atoms.len() {
            budget.compare()?;
            let left = major_end(line.atoms[cut_index - 1].bounds, 0);
            let right = major_start(line.atoms[cut_index].bounds, 0);
            if right - left > page_width * MIN_GUTTER_RATIO
                && left < page_width * 0.55
                && right > page_width * 0.45
            {
                candidates.push(Candidate {
                    line_index,
                    cut_index,
                    left,
                    right,
                    center: f32::midpoint(left, right),
                    left_span: left - line_start,
                    right_span: line_end - right,
                    y: line.bounds.y,
                    height: line.bounds.height.max(1.0),
                    font_size: line.font_size,
                });
            }
        }
    }
    charge_sort(candidates.len(), budget)?;
    candidates.sort_unstable_by(|left, right| {
        left.center
            .total_cmp(&right.center)
            .then_with(|| left.line_index.cmp(&right.line_index))
            .then_with(|| left.cut_index.cmp(&right.cut_index))
    });

    let mut qualified = Vec::new();
    qualified.try_reserve_exact(candidates.len()).map_err(|_| memory("layout gutter decisions"))?;
    qualified.resize(candidates.len(), false);
    let tolerance = page_width * GUTTER_CENTER_TOLERANCE_RATIO;
    let mut start = 0_usize;
    while start < candidates.len() {
        budget.checkpoint_item()?;
        let mut end = start + 1;
        while end < candidates.len()
            && candidates[end].center - candidates[start].center <= tolerance
        {
            budget.compare()?;
            end += 1;
        }
        if group_is_column_flow(&candidates[start..end], &lines, page_width, budget)? {
            qualified[start..end].fill(true);
        }
        start = end;
    }

    let mut cuts_by_line = Vec::new();
    cuts_by_line.try_reserve_exact(lines.len()).map_err(|_| memory("layout gutter line index"))?;
    cuts_by_line.resize_with(lines.len(), Vec::new);
    let mut qualified_count = 0_usize;
    for (candidate, is_qualified) in candidates.into_iter().zip(qualified) {
        budget.checkpoint_item()?;
        if is_qualified {
            cuts_by_line[candidate.line_index]
                .try_reserve(1)
                .map_err(|_| memory("layout gutter cuts"))?;
            cuts_by_line[candidate.line_index].push(candidate.cut_index);
            qualified_count = qualified_count
                .checked_add(1)
                .ok_or_else(|| memory("layout gutter split count"))?;
        }
    }

    let output_capacity = lines
        .len()
        .checked_add(qualified_count)
        .ok_or_else(|| memory("layout gutter output count"))?;
    let mut output = Vec::new();
    output.try_reserve_exact(output_capacity).map_err(|_| memory("layout gutter lines"))?;
    for (line, mut cuts) in lines.into_iter().zip(cuts_by_line) {
        budget.checkpoint_item()?;
        if cuts.is_empty() {
            output.push(line);
            continue;
        }
        charge_sort(cuts.len(), budget)?;
        cuts.sort_unstable();
        cuts.try_reserve(1).map_err(|_| memory("layout gutter terminal cut"))?;
        cuts.push(line.atoms.len());
        materialize_segments(line, cuts, &mut output, budget)?;
    }
    Ok(output)
}

fn group_is_column_flow(
    candidates: &[Candidate],
    lines: &[Line],
    page_width: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    if candidates.len() < 2 {
        return Ok(false);
    }
    let common_left =
        candidates.iter().map(|candidate| candidate.left).fold(f32::NEG_INFINITY, f32::max);
    let common_right =
        candidates.iter().map(|candidate| candidate.right).fold(f32::INFINITY, f32::min);
    let left_span = candidates.iter().map(|candidate| candidate.left_span).fold(0.0, f32::max);
    let right_span = candidates.iter().map(|candidate| candidate.right_span).fold(0.0, f32::max);
    if common_right - common_left < page_width * MIN_COMMON_GUTTER_RATIO {
        return Ok(false);
    }
    let substantial_columns = left_span >= page_width * MIN_COLUMN_SPAN_RATIO
        && right_span >= page_width * MIN_COLUMN_SPAN_RATIO;
    // Broad text on both sides is direct column-flow evidence. Short repeated
    // cells stay eligible for table recovery unless a nearby larger-font
    // region heading supplies independent evidence that the rows are columns.
    if !substantial_columns
        && !has_typographic_region_heading(candidates, lines, page_width, budget)?
    {
        return Ok(false);
    }

    let mut rows = Vec::new();
    rows.try_reserve_exact(candidates.len()).map_err(|_| memory("layout gutter rows"))?;
    rows.extend(0..candidates.len());
    charge_sort(rows.len(), budget)?;
    rows.sort_unstable_by_key(|index| candidates[*index].line_index);
    rows.dedup_by_key(|index| candidates[*index].line_index);
    if rows.len() < 2 {
        return Ok(false);
    }
    charge_sort(rows.len(), budget)?;
    rows.sort_unstable_by(|left, right| {
        candidates[*left]
            .y
            .total_cmp(&candidates[*right].y)
            .then_with(|| candidates[*left].line_index.cmp(&candidates[*right].line_index))
    });
    for pair in rows.windows(2) {
        budget.compare()?;
        let left = &candidates[pair[0]];
        let right = &candidates[pair[1]];
        let height = left.height.max(right.height);
        let offset = right.y - left.y;
        if offset >= height * MIN_ROW_OFFSET_HEIGHTS && offset <= height * MAX_ROW_GAP_HEIGHTS {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_typographic_region_heading(
    candidates: &[Candidate],
    lines: &[Line],
    page_width: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    let Some(body_font) =
        candidates.iter().filter_map(|candidate| candidate.font_size).reduce(f32::max)
    else {
        return Ok(false);
    };
    let top = candidates.iter().map(|candidate| candidate.y).fold(f32::INFINITY, f32::min);
    let body_height = candidates.iter().map(|candidate| candidate.height).fold(1.0, f32::max);
    for line in lines {
        budget.compare()?;
        let bottom = line.bounds.y + line.bounds.height;
        if line.orientation == 0
            && bottom < top
            && top - bottom <= body_height * 8.0
            && line.bounds.x <= page_width * 0.20
            && line.font_size.is_some_and(|font| font >= body_font * 1.4)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn materialize_segments(
    line: Line,
    cuts: Vec<usize>,
    output: &mut Vec<Line>,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let mut atoms = line.atoms.into_iter();
    let mut consumed = 0_usize;
    for end in cuts {
        budget.checkpoint_item()?;
        let take = end.checked_sub(consumed).ok_or_else(|| memory("layout gutter cut order"))?;
        let mut segment_atoms = Vec::new();
        segment_atoms.try_reserve_exact(take).map_err(|_| memory("layout gutter segment"))?;
        segment_atoms.extend(atoms.by_ref().take(take));
        if segment_atoms.len() != take {
            return Err(memory("layout gutter segment length"));
        }
        consumed = end;
        let bounds = segment_atoms
            .iter()
            .map(|atom| atom.bounds)
            .reduce(union)
            .ok_or_else(|| memory("layout empty gutter segment"))?;
        let font_size =
            segment_atoms.iter().filter_map(|atom| atom.font_size).reduce(f32::midpoint);
        let source_index = segment_atoms[0].source_index;
        let source_kind = segment_atoms[0].source_kind;
        budget.consume_line()?;
        output.push(Line {
            atoms: segment_atoms,
            bounds,
            font_size,
            orientation: line.orientation,
            source_index,
            source_kind,
        });
    }
    if atoms.next().is_some() {
        return Err(memory("layout gutter trailing atom"));
    }
    Ok(())
}

fn charge_sort(items: usize, budget: &mut LayoutBudget<'_>) -> Result<(), ConversionError> {
    if items < 2 {
        return Ok(());
    }
    let levels = usize::BITS - (items - 1).leading_zeros();
    let comparisons = items
        .checked_mul(levels as usize)
        .and_then(|value| value.checked_mul(2))
        .and_then(|value| value.checked_add(items))
        .ok_or_else(|| memory("layout gutter sort work"))?;
    for _ in 0..comparisons {
        budget.compare()?;
    }
    Ok(())
}

struct Candidate {
    line_index: usize,
    cut_index: usize,
    left: f32,
    right: f32,
    center: f32,
    left_span: f32,
    right_span: f32,
    y: f32,
    height: f32,
    font_size: Option<f32>,
}
