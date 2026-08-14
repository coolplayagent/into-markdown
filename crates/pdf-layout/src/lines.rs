use crate::budget::LayoutBudget;
use crate::geometry::{major_end, major_start, minor_center, minor_extent, union};
use crate::memory;
use crate::model::{Atom, Line, SourceKind};
use into_markdown_core::{ConversionError, Inline};

pub(crate) fn cluster(
    mut atoms: Vec<Atom>,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Line>, ConversionError> {
    atoms.sort_by(|left, right| {
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
    });
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
        line.atoms.sort_by(|left, right| {
            major_start(left.bounds, left.orientation)
                .total_cmp(&major_start(right.bounds, right.orientation))
                .then_with(|| left.source_index.cmp(&right.source_index))
        });
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

pub(crate) fn split_page_gutters(
    lines: Vec<Line>,
    page_width: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Line>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve_exact(lines.len()).map_err(|_| memory("layout gutter lines"))?;
    for line in lines {
        if line.orientation != 0 || line.atoms.len() < 2 {
            output.push(line);
            continue;
        }
        let mut cuts = Vec::new();
        cuts.try_reserve_exact(line.atoms.len()).map_err(|_| memory("layout gutter cuts"))?;
        for index in 1..line.atoms.len() {
            budget.compare()?;
            let left = major_end(line.atoms[index - 1].bounds, 0);
            let right = major_start(line.atoms[index].bounds, 0);
            let gap = right - left;
            if gap > page_width * 0.12 && left < page_width * 0.55 && right > page_width * 0.45 {
                cuts.push(index);
            }
        }
        if cuts.is_empty() {
            output.push(line);
            continue;
        }
        cuts.push(line.atoms.len());
        let mut atoms = line.atoms.into_iter();
        let mut consumed = 0_usize;
        for end in cuts {
            let take = end - consumed;
            let mut segment_atoms = Vec::new();
            segment_atoms.try_reserve_exact(take).map_err(|_| memory("layout gutter segment"))?;
            segment_atoms.extend(atoms.by_ref().take(take));
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
    }
    Ok(output)
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
