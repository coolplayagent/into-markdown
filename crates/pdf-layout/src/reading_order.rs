use crate::budget::LayoutBudget;
use crate::geometry::reading_cmp;
use crate::memory;
use crate::model::{Line, RebuiltBlock};
use crate::ordering;
use into_markdown_core::{ConversionError, Rect};

const MAX_PARTITION_DEPTH: usize = 24;

pub(crate) fn line_height(
    lines: &[Line],
    budget: &mut LayoutBudget<'_>,
) -> Result<Option<f32>, ConversionError> {
    if lines.is_empty() {
        return Ok(None);
    }
    let mut rectangles = Vec::new();
    rectangles.try_reserve_exact(lines.len()).map_err(|_| memory("layout text extents"))?;
    rectangles.extend(lines.iter().map(|line| line.bounds));
    median_extent(&rectangles, Axis::Horizontal, budget).map(Some)
}

pub(crate) fn lines(
    values: Vec<Line>,
    width: f32,
    height: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Line>, ConversionError> {
    partition(
        values,
        width,
        height,
        None,
        0,
        budget,
        |line| Some(line.bounds),
        |line| (line.orientation, line.bounds, line.source_index),
    )
}

pub(crate) fn blocks(
    values: Vec<RebuiltBlock>,
    width: f32,
    height: f32,
    line_height: Option<f32>,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<RebuiltBlock>, ConversionError> {
    partition(
        values,
        width,
        height,
        line_height,
        0,
        budget,
        |block| block.bounds,
        |block| {
            let mut start = block.bounds.unwrap_or_default();
            if block.orientation == 0 {
                start.height = 0.0;
            }
            (block.orientation, start, block.source_index)
        },
    )
}

fn partition<T>(
    mut values: Vec<T>,
    width: f32,
    height: f32,
    line_height: Option<f32>,
    depth: usize,
    budget: &mut LayoutBudget<'_>,
    bounds: impl Copy + Fn(&T) -> Option<Rect>,
    key: impl Copy + Fn(&T) -> (u16, Rect, usize),
) -> Result<Vec<T>, ConversionError> {
    // Marginal vertical labels have their own reading direction. Partition
    // horizontal body text independently so these labels cannot bridge its
    // column gutters or disable column ordering for the entire page.
    if values.iter().any(|value| key(value).0 == 0) && values.iter().any(|value| key(value).0 != 0)
    {
        let mut horizontal = Vec::new();
        let mut rotated = Vec::new();
        horizontal.try_reserve_exact(values.len()).map_err(|_| memory("layout body text"))?;
        rotated.try_reserve_exact(values.len()).map_err(|_| memory("layout rotated text"))?;
        for value in values {
            budget.checkpoint_item()?;
            if key(&value).0 == 0 { horizontal.push(value) } else { rotated.push(value) }
        }
        let mut ordered =
            partition(horizontal, width, height, line_height, depth, budget, bounds, key)?;
        ordering::by(&mut rotated, budget, |left, right| reading_cmp(key(left), key(right)))?;
        ordered.try_reserve_exact(rotated.len()).map_err(|_| memory("layout rotated merge"))?;
        ordered.extend(rotated);
        return Ok(ordered);
    }
    if values.len() < 2
        || depth >= MAX_PARTITION_DEPTH
        || values.iter().any(|value| key(value).0 != 0)
    {
        ordering::by(&mut values, budget, |left, right| reading_cmp(key(left), key(right)))?;
        return Ok(values);
    }
    if let Some((axis, cut)) = best_cut(&values, width, height, line_height, budget, bounds)? {
        let mut before = Vec::new();
        let mut after = Vec::new();
        before.try_reserve_exact(values.len()).map_err(|_| memory("layout partition"))?;
        after.try_reserve_exact(values.len()).map_err(|_| memory("layout partition"))?;
        for value in std::mem::take(&mut values) {
            let Some(rect) = bounds(&value) else {
                after.push(value);
                continue;
            };
            let center = if axis == Axis::Horizontal {
                rect.y + rect.height / 2.0
            } else {
                rect.x + rect.width / 2.0
            };
            if center < cut { before.push(value) } else { after.push(value) }
        }
        if !before.is_empty() && !after.is_empty() {
            let mut ordered =
                partition(before, width, height, line_height, depth + 1, budget, bounds, key)?;
            let tail =
                partition(after, width, height, line_height, depth + 1, budget, bounds, key)?;
            ordered.try_reserve_exact(tail.len()).map_err(|_| memory("layout partition merge"))?;
            ordered.extend(tail);
            return Ok(ordered);
        }
        values.append(&mut before);
        values.append(&mut after);
    }
    ordering::by(&mut values, budget, |left, right| reading_cmp(key(left), key(right)))?;
    Ok(values)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

fn best_cut<T>(
    values: &[T],
    width: f32,
    height: f32,
    line_height: Option<f32>,
    budget: &mut LayoutBudget<'_>,
    bounds: impl Copy + Fn(&T) -> Option<Rect>,
) -> Result<Option<(Axis, f32)>, ConversionError> {
    let mut rectangles = Vec::new();
    rectangles.try_reserve_exact(values.len()).map_err(|_| memory("layout cut rectangles"))?;
    rectangles.extend(values.iter().filter_map(bounds));
    if rectangles.len() < 2 {
        return Ok(None);
    }
    // Paragraph and image heights do not describe the line spacing between
    // a page header and its columns. Keep the page's observed text scale.
    let median_height = match line_height {
        Some(value) => value,
        None => median_extent(&rectangles, Axis::Horizontal, budget)?,
    };
    let horizontal = gap(&rectangles, Axis::Horizontal, height, median_height, budget)?;
    let vertical = gap(&rectangles, Axis::Vertical, width, median_height * 0.05, budget)?;
    Ok(match (horizontal, vertical) {
        // A gutter extending through the region establishes independent
        // columns. Paragraph spacing within either column must not splice
        // its continuation into the neighboring column.
        (Some((horizontal_cut, _)), Some((vertical_cut, size))) => {
            if size >= width * 0.015
                || !separates_marginal_band(&rectangles, horizontal_cut, median_height, budget)?
            {
                Some((Axis::Vertical, vertical_cut))
            } else {
                Some((Axis::Horizontal, horizontal_cut))
            }
        }
        (Some((cut, _)), None) => Some((Axis::Horizontal, cut)),
        (None, Some((cut, _))) => Some((Axis::Vertical, cut)),
        (None, None) => None,
    })
}

fn separates_marginal_band(
    rectangles: &[Rect],
    cut: f32,
    line_height: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    let mut before: Option<Rect> = None;
    let mut after: Option<Rect> = None;
    for rect in rectangles {
        budget.compare()?;
        let band = if rect.y + rect.height / 2.0 < cut { &mut before } else { &mut after };
        *band = Some(band.map_or(*rect, |prior| crate::geometry::union(prior, *rect)));
    }
    Ok([before, after]
        .iter()
        .flatten()
        .any(|bounds| bounds.height <= line_height * 2.5 && bounds.width <= line_height * 4.0))
}

fn gap(
    rectangles: &[Rect],
    axis: Axis,
    page_extent: f32,
    minimum: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<Option<(f32, f32)>, ConversionError> {
    let mut edges = Vec::new();
    let edge_count =
        rectangles.len().checked_mul(2).ok_or_else(|| memory("layout gap edge count"))?;
    edges.try_reserve_exact(edge_count).map_err(|_| memory("layout gap edges"))?;
    for rect in rectangles {
        let (start, end) = if axis == Axis::Horizontal {
            (rect.y, rect.y + rect.height)
        } else {
            (rect.x, rect.x + rect.width)
        };
        edges.push((start, true));
        edges.push((end, false));
    }
    ordering::by(&mut edges, budget, |left, right| {
        left.0.total_cmp(&right.0).then_with(|| left.1.cmp(&right.1))
    })?;
    let first_edge = edges.first().map_or(0.0, |edge| edge.0);
    let last_edge = edges.last().map_or(page_extent, |edge| edge.0);
    let mut active = 0_i64;
    let mut previous_end = 0.0_f32;
    let mut best = None;
    for (position, start) in edges {
        budget.compare()?;
        if start {
            if active == 0 {
                let size = position - previous_end;
                let cut = previous_end + size / 2.0;
                if previous_end > 0.0
                    && position < page_extent
                    && size > 0.0
                    && (size >= minimum
                        || (axis == Axis::Horizontal
                            && size >= minimum * 0.05
                            && (previous_end - first_edge).min(last_edge - position)
                                <= minimum * 2.5))
                    && best.is_none_or(|(_, current)| size > current)
                {
                    best = Some((cut, size));
                }
            }
            active += 1;
        } else {
            active -= 1;
            if active == 0 {
                previous_end = position;
            }
        }
    }
    Ok(best)
}

fn median_extent(
    rectangles: &[Rect],
    axis: Axis,
    budget: &mut LayoutBudget<'_>,
) -> Result<f32, ConversionError> {
    let mut extents = Vec::new();
    extents.try_reserve_exact(rectangles.len()).map_err(|_| memory("layout median extents"))?;
    extents.extend(
        rectangles.iter().map(
            |rect| {
                if axis == Axis::Horizontal { rect.height } else { rect.width }
            },
        ),
    );
    ordering::by(&mut extents, budget, f32::total_cmp)?;
    Ok(extents[extents.len() / 2].max(1.0))
}
