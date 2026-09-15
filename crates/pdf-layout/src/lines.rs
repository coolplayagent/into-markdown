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
    crate::baseline::validate(&atoms, budget)?;
    ordering::by(&mut atoms, budget, |left, right| {
        left.orientation
            .cmp(&right.orientation)
            .then_with(|| source_rank(left.source_kind).cmp(&source_rank(right.source_kind)))
            .then_with(|| {
                crate::baseline::coordinate(left).total_cmp(&crate::baseline::coordinate(right))
            })
            .then_with(|| {
                major_start(left.bounds, left.orientation)
                    .total_cmp(&major_start(right.bounds, right.orientation))
            })
            .then_with(|| left.source_index.cmp(&right.source_index))
    })?;
    let mut lines: Vec<Line> = Vec::new();
    lines.try_reserve_exact(atoms.len()).map_err(|_| memory("layout line allocation"))?;
    let mut glyph_heights: Vec<f32> = Vec::new();
    glyph_heights
        .try_reserve_exact(atoms.len())
        .map_err(|_| memory("layout baseline allocation"))?;
    let mut baselines: Vec<Option<f32>> = Vec::new();
    baselines.try_reserve_exact(atoms.len()).map_err(|_| memory("layout source baselines"))?;
    for atom in atoms {
        budget.checkpoint_item()?;
        let mut selected = None;
        for (index, line) in lines.iter().enumerate().rev().take(32) {
            budget.compare()?;
            if line.orientation != atom.orientation || line.source_kind != atom.source_kind {
                continue;
            }
            // The union of glyph boxes can grow toward the following row.
            // Keep the tolerance at the observed glyph scale.
            let (distance, tolerance) = if let (Some(line_baseline), Some(atom_baseline)) =
                (baselines[index], crate::baseline::source(&atom))
            {
                let font = line
                    .font_size
                    .unwrap_or(glyph_heights[index])
                    .max(atom.font_size.unwrap_or(minor_extent(atom.bounds, atom.orientation)));
                ((line_baseline - atom_baseline).abs(), font * 0.45)
            } else {
                let distance = (minor_center(line.bounds, line.orientation)
                    - minor_center(atom.bounds, atom.orientation))
                .abs();
                (
                    distance,
                    glyph_heights[index].max(minor_extent(atom.bounds, atom.orientation)) * 0.65,
                )
            };
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
            let extent = minor_extent(atom.bounds, atom.orientation);
            glyph_heights[index] = glyph_heights[index].max(extent);
            line.bounds = union(line.bounds, atom.bounds);
            line.font_size = combine_font(line.font_size, atom.font_size);
            line.source_index = line.source_index.min(atom.source_index);
            line.atoms.try_reserve(1).map_err(|_| memory("layout line atom allocation"))?;
            line.atoms.push(atom);
        } else {
            budget.consume_line()?;
            glyph_heights.push(minor_extent(atom.bounds, atom.orientation));
            baselines.push(crate::baseline::source(&atom));
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
        crate::baseline::restore_overlapping_scripts(&mut line.atoms, budget)?;
    }
    crate::baseline::validate_detached_scripts(&lines, budget)?;
    validate_lines(&lines, budget)?;
    Ok(lines)
}

fn validate_lines(lines: &[Line], budget: &mut LayoutBudget<'_>) -> Result<(), ConversionError> {
    for line in lines {
        if line.source_kind != SourceKind::Native || line.atoms.len() < 8 {
            continue;
        }
        let mut overlaps = 0;
        let mut row_collisions = 0;
        let mut stacked_numbers = 0;
        for pair in line.atoms.windows(2) {
            budget.compare()?;
            if collides_across_rows(&pair[0], &pair[1]) {
                row_collisions += 1;
                if pair
                    .iter()
                    .all(|atom| inline_text(&atom.inline).chars().any(|c| c.is_ascii_digit()))
                {
                    stacked_numbers += 1;
                }
            }
            if crate::geometry::overlap_ratio(pair[0].bounds, pair[1].bounds) > 0.60 {
                overlaps += 1;
            }
        }
        let glyph_height = line
            .atoms
            .iter()
            .map(|atom| minor_extent(atom.bounds, atom.orientation))
            .fold(0.0_f32, f32::max);
        let merged_rows =
            glyph_height > 0.0 && minor_extent(line.bounds, line.orientation) > glyph_height * 2.5;
        if (overlaps >= 16 && overlaps * 3 > line.atoms.len())
            || (row_collisions >= 4 && row_collisions * 5 > line.atoms.len())
            || merged_rows
            || stacked_numbers > 0
        {
            return Err(ConversionError::Malformed {
                part: Some("pdf-layout-visual".into()),
                detail: "overlapping native rows or stacked numeric notation require a page image"
                    .into(),
            });
        }
    }
    Ok(())
}

// Repeated glyph positions across separate baselines indicate interleaved rows.
// A few isolated overlaps can belong to superscripts or combining marks.
pub(super) fn collides_across_rows(left: &Atom, right: &Atom) -> bool {
    let orientation = left.orientation;
    let overlap = major_end(left.bounds, orientation).min(major_end(right.bounds, orientation))
        - major_start(left.bounds, orientation).max(major_start(right.bounds, orientation));
    let width = crate::geometry::major_extent(left.bounds, orientation)
        .min(crate::geometry::major_extent(right.bounds, orientation));
    let height =
        minor_extent(left.bounds, orientation).min(minor_extent(right.bounds, orientation));
    let distance =
        (minor_center(left.bounds, orientation) - minor_center(right.bounds, orientation)).abs();
    width > 0.0 && height > 0.0 && overlap > width * 0.6 && distance > height * 0.5
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
        if atom.space_after {
            output.push(' ');
        }
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
