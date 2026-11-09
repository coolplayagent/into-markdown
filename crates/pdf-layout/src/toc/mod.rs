//! Recover directory entries before prose merging and table inference.
mod materialize;
mod rows;

use crate::budget::LayoutBudget;
use crate::geometry::{minor_center, union};
use crate::links::Links;
use crate::model::{Line, RebuiltBlock};
use crate::{memory, ordering};
use into_markdown_core::ConversionError;
use rows::Hint;

struct Row {
    line: Line,
    hint: Hint,
    prefix: Vec<Line>,
}

struct Recovery<'a> {
    page: u32,
    dimensions: (f32, f32),
    heading: bool,
    associations: &'a Links,
}

pub(crate) fn recover(
    lines: Vec<Line>,
    associations: &Links,
    page: u32,
    dimensions: (f32, f32),
    budget: &mut LayoutBudget<'_>,
) -> Result<(Vec<RebuiltBlock>, Vec<Line>), ConversionError> {
    let lines = rows::split_columns(lines, associations, budget)?;
    let mut has_heading = false;
    let mut candidates = Vec::new();
    let mut remaining = Vec::new();
    for line in lines {
        budget.checkpoint_item()?;
        has_heading |= rows::heading(&line, budget)?;
        if let Some(hint) = rows::describe(&line.atoms, line.orientation, associations, budget)? {
            budget.push(&mut candidates, Row { line, hint, prefix: Vec::new() })?;
        } else {
            budget.push(&mut remaining, line)?;
        }
    }
    ordering::by(&mut candidates, budget, |a, b| {
        a.line
            .orientation
            .cmp(&b.line.orientation)
            .then_with(|| a.hint.end.total_cmp(&b.hint.end))
            .then_with(|| {
                minor_center(a.line.bounds, a.line.orientation)
                    .total_cmp(&minor_center(b.line.bounds, b.line.orientation))
            })
    })?;
    let mut columns: Vec<Vec<Row>> = Vec::new();
    for row in candidates {
        let belongs = columns.last().and_then(|column| column.first()).is_some_and(|first| {
            first.line.orientation == row.line.orientation
                && (first.hint.end - row.hint.end).abs() <= first.hint.font.max(row.hint.font)
        });
        if !belongs {
            budget.push(&mut columns, Vec::new())?;
        }
        budget.push(columns.last_mut().ok_or_else(|| memory("directory column"))?, row)?;
    }
    let mut output = Vec::new();
    let recovery = Recovery { page, dimensions, heading: has_heading, associations };
    for mut column in columns {
        ordering::by(&mut column, budget, |a, b| {
            minor_center(a.line.bounds, a.line.orientation)
                .total_cmp(&minor_center(b.line.bounds, b.line.orientation))
        })?;
        let mut run = Vec::new();
        for row in column {
            if run.last().is_some_and(|prior: &Row| {
                minor_center(row.line.bounds, row.line.orientation)
                    - minor_center(prior.line.bounds, prior.line.orientation)
                    > row.hint.font.max(prior.hint.font) * 5.0
            }) {
                finish_run(
                    std::mem::take(&mut run),
                    &recovery,
                    &mut output,
                    &mut remaining,
                    budget,
                )?;
            }
            budget.push(&mut run, row)?;
        }
        finish_run(run, &recovery, &mut output, &mut remaining, budget)?;
    }
    Ok((output, remaining))
}

fn finish_run(
    mut run: Vec<Row>,
    recovery: &Recovery<'_>,
    output: &mut Vec<RebuiltBlock>,
    remaining: &mut Vec<Line>,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let evidence = run.iter().filter(|row| row.hint.linked || row.hint.leaders).count();
    if run.len() >= 2 && (evidence >= 2 || recovery.heading) {
        for row in &mut run {
            take_wrapped_prefix(row, remaining, recovery.associations, budget)?;
        }
        let block =
            materialize::column(run, recovery.page, recovery.dimensions, output.len(), budget)?;
        budget.push(output, block)?;
    } else {
        for row in run {
            budget.push(remaining, row.line)?;
        }
    }
    Ok(())
}

fn take_wrapped_prefix(
    row: &mut Row,
    remaining: &mut Vec<Line>,
    associations: &Links,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    // A continuation has no new section number. Its preceding title line is
    // geometrically adjacent and aligned within the same directory column.
    while row.hint.depth.is_none() {
        let next = row.prefix.last().unwrap_or(&row.line);
        let Some(index) =
            wrapped_prefix_index(next, remaining, row.hint.font, associations, budget)?
        else {
            break;
        };
        let prior = remaining.remove(index);
        row.hint.indent = crate::geometry::major_start(prior.bounds, prior.orientation);
        row.hint.depth = rows::number_depth(&crate::lines::fallible_text(&prior, budget)?);
        budget.push(&mut row.prefix, prior)?;
    }
    row.prefix.reverse();
    for prior in &row.prefix {
        row.line.bounds = union(row.line.bounds, prior.bounds);
    }
    Ok(())
}

fn wrapped_prefix_index(
    next: &Line,
    remaining: &[Line],
    font: f32,
    associations: &Links,
    budget: &mut LayoutBudget<'_>,
) -> Result<Option<usize>, ConversionError> {
    let mut best = None;
    let mut best_distance = f32::MAX;
    for (index, prior) in remaining.iter().enumerate() {
        budget.compare()?;
        let distance = minor_center(next.bounds, next.orientation)
            - minor_center(prior.bounds, prior.orientation);
        if prior.orientation == next.orientation
            && distance > 0.0
            && distance <= font * 1.8
            && distance < best_distance
            && (crate::geometry::major_start(prior.bounds, prior.orientation)
                - crate::geometry::major_start(next.bounds, next.orientation))
            .abs()
                < font * 3.0
            && !rows::heading(prior, budget)?
            && continuation_evidence(prior, next, associations, budget)?
        {
            best = Some(index);
            best_distance = distance;
        }
    }
    Ok(best)
}

fn continuation_evidence(
    prior: &Line,
    next: &Line,
    associations: &Links,
    budget: &mut LayoutBudget<'_>,
) -> Result<bool, ConversionError> {
    if rows::number_depth(&crate::lines::fallible_text(prior, budget)?).is_some() {
        return Ok(true);
    }
    for atom in &prior.atoms {
        let Some(target) = associations.internal_target(&atom.inline, budget)? else { continue };
        for following in &next.atoms {
            budget.compare()?;
            if associations.internal_target(&following.inline, budget)? == Some(target) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
