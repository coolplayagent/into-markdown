use crate::budget::LayoutBudget;
use crate::geometry::{major_end, major_start, union};
use crate::memory;
use crate::model::Line;
use crate::ordering;
use into_markdown_core::ConversionError;

const MIN_GUTTER_RATIO: f32 = 0.12;
const MIN_COMMON_GUTTER_RATIO: f32 = 0.08;
const GUTTER_CENTER_TOLERANCE_RATIO: f32 = 0.06;
const MIN_ROW_OFFSET_HEIGHTS: f32 = 0.25;
const MAX_ROW_GAP_HEIGHTS: f32 = 4.0;

/// Split the remaining text flow only when a gutter is corroborated by
/// multiple vertically-adjacent lines. Strong table regions have already
/// been removed by the caller, so headings are intentionally not evidence
/// for (or against) a column decision.
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
        for cut_index in 1..line.atoms.len() {
            budget.compare()?;
            let left = major_end(line.atoms[cut_index - 1].bounds, 0);
            let right = major_start(line.atoms[cut_index].bounds, 0);
            let gap = right - left;
            let narrow_gutter = gap > line.font_size.unwrap_or(line.bounds.height) * 0.8;
            if (gap > page_width * MIN_GUTTER_RATIO || narrow_gutter)
                && left < page_width * 0.55
                && right > page_width * 0.45
            {
                candidates.push(Candidate {
                    line_index,
                    cut_index,
                    left,
                    right,
                    center: f32::midpoint(left, right),
                    y: line.bounds.y,
                    height: line.bounds.height.max(1.0),
                });
            }
        }
    }
    ordering::by(&mut candidates, budget, |left, right| {
        left.center
            .total_cmp(&right.center)
            .then_with(|| left.line_index.cmp(&right.line_index))
            .then_with(|| left.cut_index.cmp(&right.cut_index))
    })?;

    let mut qualified = qualify_candidates(&candidates, page_width, budget)?;
    qualify_separate_column_rows(&lines, &candidates, &mut qualified, page_width, budget)?;
    complete_native_boundaries(&lines, &mut candidates, &mut qualified, budget)?;

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
        ordering::by(&mut cuts, budget, Ord::cmp)?;
        cuts.try_reserve(1).map_err(|_| memory("layout gutter terminal cut"))?;
        cuts.push(line.atoms.len());
        materialize_segments(line, cuts, &mut output, budget)?;
    }
    Ok(output)
}

fn complete_native_boundaries(
    lines: &[Line],
    candidates: &mut Vec<Candidate>,
    qualified: &mut Vec<bool>,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let mut centers = Vec::new();
    centers.try_reserve_exact(candidates.len()).map_err(|_| memory("layout gutter centers"))?;
    let mut first_y = f32::INFINITY;
    let mut last_y = f32::NEG_INFINITY;
    for (candidate, keep) in candidates.iter().zip(qualified.iter()) {
        if *keep {
            centers.push(candidate.center);
            first_y = first_y.min(candidate.y);
            last_y = last_y.max(candidate.y + candidate.height);
        }
    }
    if centers.len() < 3 {
        return Ok(());
    }
    ordering::by(&mut centers, budget, f32::total_cmp)?;
    let center = centers[centers.len() / 2];
    // A slightly overhanging line can narrow an established gutter. Native
    // source discontinuity distinguishes that column boundary from adjacent
    // letters in a spanning heading.
    for (line_index, line) in lines.iter().enumerate() {
        if line.orientation != 0 || line.bounds.y < first_y || line.bounds.y > last_y {
            continue;
        }
        for (index, pair) in line.atoms.windows(2).enumerate() {
            budget.compare()?;
            let left = major_end(pair[0].bounds, 0);
            let right = major_start(pair[1].bounds, 0);
            if pair.iter().all(|atom| atom.source_kind == crate::model::SourceKind::Native)
                && pair[0].source_index.abs_diff(pair[1].source_index) > 8
                && right >= left
                && right - left <= line.font_size.unwrap_or(line.bounds.height) * 0.8
                && (f32::midpoint(left, right) - center).abs() <= line.bounds.height
            {
                candidates.try_reserve(1).map_err(|_| memory("layout native gutter"))?;
                qualified.try_reserve(1).map_err(|_| memory("layout native gutter decision"))?;
                candidates.push(Candidate {
                    line_index,
                    cut_index: index + 1,
                    left,
                    right,
                    center: f32::midpoint(left, right),
                    y: line.bounds.y,
                    height: line.bounds.height,
                });
                qualified.push(true);
            }
        }
    }
    Ok(())
}

fn qualify_candidates(
    candidates: &[Candidate],
    page_width: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<bool>, ConversionError> {
    let mut qualified = Vec::new();
    qualified.try_reserve_exact(candidates.len()).map_err(|_| memory("layout gutter decisions"))?;
    qualified.resize(candidates.len(), false);
    // Text wrapping around pictures changes each gap's center while the
    // column boundary stays fixed. Corroborate that shared boundary before
    // grouping gaps by their individual centers.
    let middle = page_width / 2.0;
    let mut central = Vec::new();
    central.try_reserve_exact(candidates.len()).map_err(|_| memory("layout central gutter"))?;
    central.extend(
        candidates
            .iter()
            .filter(|candidate| candidate.left < middle && candidate.right > middle)
            .cloned(),
    );
    if central.len() >= 3 && group_is_column_flow(&central, page_width, budget)? {
        for (candidate, decision) in candidates.iter().zip(&mut qualified) {
            budget.checkpoint_item()?;
            *decision = candidate.left < middle && candidate.right > middle;
        }
    }
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
        if group_is_column_flow(&candidates[start..end], page_width, budget)? {
            qualified[start..end].fill(true);
        }
        start = end;
    }

    Ok(qualified)
}

fn group_is_column_flow(
    candidates: &[Candidate],
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
    // Three aligned rows can corroborate a normal typeset column gutter,
    // which is often much narrower than the whitespace in sparse layouts.
    let minimum = if candidates.len() >= 3 {
        candidates.iter().map(|candidate| candidate.height).fold(f32::INFINITY, f32::min) * 0.5
    } else {
        page_width * MIN_COMMON_GUTTER_RATIO
    };
    if common_right - common_left < minimum {
        return Ok(false);
    }

    let mut rows = Vec::new();
    rows.try_reserve_exact(candidates.len()).map_err(|_| memory("layout gutter rows"))?;
    rows.extend(0..candidates.len());
    ordering::by(&mut rows, budget, |left, right| {
        candidates[*left].line_index.cmp(&candidates[*right].line_index)
    })?;
    rows.dedup_by_key(|index| candidates[*index].line_index);
    if rows.len() < 2 {
        return Ok(false);
    }
    ordering::by(&mut rows, budget, |left, right| {
        candidates[*left]
            .y
            .total_cmp(&candidates[*right].y)
            .then_with(|| candidates[*left].line_index.cmp(&candidates[*right].line_index))
    })?;
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

#[derive(Clone)]
struct Candidate {
    line_index: usize,
    cut_index: usize,
    left: f32,
    right: f32,
    center: f32,
    y: f32,
    height: f32,
}

// Staggered baselines can keep the body columns on independent rows while a
// short first row bridges both. Corroborate its gap with separate column rows.
fn qualify_separate_column_rows(
    lines: &[Line],
    candidates: &[Candidate],
    qualified: &mut [bool],
    page_width: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    for (candidate, keep) in candidates.iter().zip(qualified) {
        if *keep || candidate.right - candidate.left < page_width * MIN_GUTTER_RATIO {
            continue;
        }
        let line = &lines[candidate.line_index];
        let pair = &line.atoms[candidate.cut_index - 1..=candidate.cut_index];
        if pair.iter().any(|atom| atom.source_kind != crate::model::SourceKind::Native)
            || pair[0].source_index.abs_diff(pair[1].source_index) <= 8
        {
            continue;
        }
        let mut left_rows = 0;
        let mut right_rows = 0;
        let mut left_edge = candidate.left;
        let mut right_edge = candidate.right;
        for other in lines {
            budget.compare()?;
            if other.orientation != 0
                || other.bounds.width < page_width * 0.2
                || (other.bounds.y - candidate.y).abs() > candidate.height * 20.0
            {
                continue;
            }
            let start = major_start(other.bounds, 0);
            let end = major_end(other.bounds, 0);
            if start < candidate.left && end < candidate.right {
                left_rows += 1;
                left_edge = left_edge.max(end);
            } else if start > candidate.left && end > candidate.right {
                right_rows += 1;
                right_edge = right_edge.min(start);
            }
        }
        *keep =
            left_rows >= 3 && right_rows >= 3 && right_edge - left_edge >= candidate.height * 0.5;
    }
    Ok(())
}
