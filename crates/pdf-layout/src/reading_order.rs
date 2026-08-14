use crate::budget::LayoutBudget;
use crate::geometry::reading_cmp;
use crate::memory;
use crate::model::{Line, RebuiltBlock};
use into_markdown_core::{ConversionError, Rect};

const MAX_PARTITION_DEPTH: usize = 24;

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
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<RebuiltBlock>, ConversionError> {
    partition(
        values,
        width,
        height,
        0,
        budget,
        |block| block.bounds,
        |block| (block.orientation, block.bounds.unwrap_or_default(), block.source_index),
    )
}

fn partition<T>(
    mut values: Vec<T>,
    width: f32,
    height: f32,
    depth: usize,
    budget: &mut LayoutBudget<'_>,
    bounds: impl Copy + Fn(&T) -> Option<Rect>,
    key: impl Copy + Fn(&T) -> (u16, Rect, usize),
) -> Result<Vec<T>, ConversionError> {
    if values.len() < 3 || depth >= MAX_PARTITION_DEPTH {
        values.sort_by(|left, right| reading_cmp(key(left), key(right)));
        return Ok(values);
    }
    if let Some((axis, cut)) = best_cut(&values, width, height, budget, bounds)? {
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
            let mut ordered = partition(before, width, height, depth + 1, budget, bounds, key)?;
            let tail = partition(after, width, height, depth + 1, budget, bounds, key)?;
            ordered.try_reserve_exact(tail.len()).map_err(|_| memory("layout partition merge"))?;
            ordered.extend(tail);
            return Ok(ordered);
        }
        values.append(&mut before);
        values.append(&mut after);
    }
    values.sort_by(|left, right| reading_cmp(key(left), key(right)));
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
    budget: &mut LayoutBudget<'_>,
    bounds: impl Copy + Fn(&T) -> Option<Rect>,
) -> Result<Option<(Axis, f32)>, ConversionError> {
    let mut rectangles = Vec::new();
    rectangles.try_reserve_exact(values.len()).map_err(|_| memory("layout cut rectangles"))?;
    rectangles.extend(values.iter().filter_map(bounds));
    if rectangles.len() < 3 {
        return Ok(None);
    }
    let horizontal = gap(&rectangles, Axis::Horizontal, height, budget)?
        .filter(|(_, size)| *size >= median_extent(&rectangles, Axis::Horizontal) * 1.25);
    let vertical = gap(&rectangles, Axis::Vertical, width, budget)?.filter(|(_, size)| {
        *size >= (width * 0.035).max(median_extent(&rectangles, Axis::Horizontal) * 2.0)
    });
    Ok(match (horizontal, vertical) {
        (Some((horizontal_cut, horizontal_gap)), Some((vertical_cut, vertical_gap))) => {
            if vertical_gap / width >= horizontal_gap / height {
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

fn gap(
    rectangles: &[Rect],
    axis: Axis,
    page_extent: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<Option<(f32, f32)>, ConversionError> {
    let mut edges = Vec::new();
    edges.try_reserve_exact(rectangles.len() * 2).map_err(|_| memory("layout gap edges"))?;
    for rect in rectangles {
        let (start, end) = if axis == Axis::Horizontal {
            (rect.y, rect.y + rect.height)
        } else {
            (rect.x, rect.x + rect.width)
        };
        edges.push((start, true));
        edges.push((end, false));
    }
    edges.sort_by(|left, right| left.0.total_cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
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

fn median_extent(rectangles: &[Rect], axis: Axis) -> f32 {
    let mut extents = rectangles
        .iter()
        .map(|rect| if axis == Axis::Horizontal { rect.height } else { rect.width })
        .collect::<Vec<_>>();
    extents.sort_by(f32::total_cmp);
    extents[extents.len() / 2].max(1.0)
}
