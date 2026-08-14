use crate::budget::LayoutBudget;
use crate::geometry::{major_end, major_start, minor_center, minor_extent, union};
use crate::memory;
use crate::model::{Atom, Line, SourceKind};
use crate::ordering;
use into_markdown_core::{ConversionError, Inline};

pub(crate) fn cluster(
    mut atoms: Vec<Atom>,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Line>, ConversionError> {
    ordering::by(&mut atoms, budget, |left, right| {
        left.orientation
            .cmp(&right.orientation)
            .then_with(|| source_rank(left.source_kind).cmp(&source_rank(right.source_kind)))
            .then_with(|| {
                minor_center(left.bounds, left.orientation)
                    .total_cmp(&minor_center(right.bounds, right.orientation))
            })
            .then_with(|| {
                major_start(left.bounds, left.orientation)
                    .total_cmp(&major_start(right.bounds, right.orientation))
            })
            .then_with(|| left.source_index.cmp(&right.source_index))
    })?;
    let mut lines: Vec<Line> = Vec::new();
    lines.try_reserve_exact(atoms.len()).map_err(|_| memory("layout line allocation"))?;
    for atom in atoms {
        budget.checkpoint_item()?;
        let mut selected = None;
        for (index, line) in lines.iter().enumerate().rev().take(32) {
            budget.compare()?;
            if line.orientation != atom.orientation || line.source_kind != atom.source_kind {
                continue;
            }
            let distance = (minor_center(line.bounds, line.orientation)
                - minor_center(atom.bounds, atom.orientation))
            .abs();
            let tolerance = minor_extent(line.bounds, line.orientation)
                .max(minor_extent(atom.bounds, atom.orientation))
                * 0.65;
            if distance <= tolerance {
                selected = Some(index);
                break;
            }
            if distance > tolerance * 3.0 {
                break;
            }
        }
        if let Some(index) = selected {
            let line = &mut lines[index];
            line.bounds = union(line.bounds, atom.bounds);
            line.font_size = combine_font(line.font_size, atom.font_size);
            line.source_index = line.source_index.min(atom.source_index);
            line.atoms.try_reserve(1).map_err(|_| memory("layout line atom allocation"))?;
            line.atoms.push(atom);
        } else {
            budget.consume_line()?;
            lines.push(Line {
                bounds: atom.bounds,
                font_size: atom.font_size,
                orientation: atom.orientation,
                source_index: atom.source_index,
                source_kind: atom.source_kind,
                atoms: vec![atom],
            });
        }
    }
    for line in &mut lines {
        ordering::by(&mut line.atoms, budget, |left, right| {
            major_start(left.bounds, left.orientation)
                .total_cmp(&major_start(right.bounds, right.orientation))
                .then_with(|| left.source_index.cmp(&right.source_index))
        })?;
    }
    Ok(lines)
}

pub(crate) fn text(line: &Line) -> String {
    let mut output = String::new();
    append_text(line, &mut output);
    output
}

pub(crate) fn fallible_text(
    line: &Line,
    budget: &mut LayoutBudget<'_>,
) -> Result<String, ConversionError> {
    let capacity = line.atoms.iter().try_fold(line.atoms.len(), |total, atom| {
        total
            .checked_add(inline_text(&atom.inline).len())
            .ok_or_else(|| memory("layout line text length"))
    })?;
    budget.checkpoint_bytes(capacity)?;
    for _ in &line.atoms {
        budget.checkpoint_item()?;
    }
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|_| memory("layout line text allocation"))?;
    append_text(line, &mut output);
    Ok(output)
}

fn append_text(line: &Line, output: &mut String) {
    let mut previous = None;
    for atom in &line.atoms {
        let value = inline_text(&atom.inline);
        if should_insert_space(output, value, previous, atom) {
            output.push(' ');
        }
        output.push_str(value);
        previous = Some(atom);
    }
}

pub(crate) fn inline_text(inline: &Inline) -> &str {
    match inline {
        Inline::Text { value, .. }
        | Inline::SourceText { value, .. }
        | Inline::OcrText { value, .. }
        | Inline::Code(value)
        | Inline::Formula(value)
        | Inline::FootnoteReference(value) => value,
        _ => "",
    }
}

fn should_insert_space(output: &str, value: &str, previous: Option<&Atom>, atom: &Atom) -> bool {
    let Some(left) = output.chars().next_back() else { return false };
    let Some(right) = value.chars().next() else { return false };
    if left.is_whitespace() || right.is_whitespace() {
        return false;
    }
    let Some(previous) = previous else {
        return false;
    };
    let gap = major_start(atom.bounds, atom.orientation)
        - major_end(previous.bounds, previous.orientation);
    gap > minor_extent(atom.bounds, atom.orientation) * 0.6
        && left.is_ascii_alphanumeric()
        && right.is_ascii_alphanumeric()
}

const fn source_rank(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::Native => 0,
        SourceKind::Ocr => 1,
    }
}

fn combine_font(left: Option<f32>, right: Option<f32>) -> Option<f32> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.midpoint(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}
