use crate::budget::LayoutBudget;
use crate::geometry::{major_end, major_start, minor_center};
use crate::model::Atom;
use crate::{memory, ordering};
use into_markdown_core::{ConversionError, Inline};

pub(crate) fn source(atom: &Atom) -> Option<f32> {
    match &atom.inline {
        Inline::SourceText { provenance, .. } => {
            provenance.locator.text_baseline.filter(|v| v.is_finite())
        }
        _ => None,
    }
}

pub(crate) fn coordinate(atom: &Atom) -> f32 {
    source(atom).unwrap_or_else(|| minor_center(atom.bounds, atom.orientation))
}

/// Preserve two-dimensional numeric notation and drop capitals before
/// grouping native text into scalar baselines.
pub(crate) fn validate(
    atoms: &[Atom],
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let mut numbers = Vec::new();
    numbers.try_reserve_exact(atoms.len()).map_err(|_| memory("layout numeric origins"))?;
    let mut fonts = Vec::new();
    fonts.try_reserve_exact(atoms.len()).map_err(|_| memory("layout numeric font scale"))?;
    for (index, atom) in atoms.iter().enumerate() {
        budget.checkpoint_item()?;
        if let Some(font) = atom.font_size.filter(|font| *font > 0.0 && source(atom).is_some()) {
            fonts.push(font);
        }
        if source(atom).is_some()
            && crate::lines::inline_text(&atom.inline).chars().any(|c| c.is_ascii_digit())
        {
            numbers.push(index);
        }
    }
    ordering::by(&mut fonts, budget, f32::total_cmp)?;
    let body_font = fonts.get(fonts.len() / 2).copied().unwrap_or(0.0);
    check_drop_caps(atoms, body_font, budget)?;
    if numbers.len() < 2 {
        return Ok(());
    }
    ordering::by(&mut numbers, budget, |left, right| {
        let (left, right) = (&atoms[*left], &atoms[*right]);
        left.orientation
            .cmp(&right.orientation)
            .then_with(|| {
                major_start(left.bounds, left.orientation)
                    .total_cmp(&major_start(right.bounds, right.orientation))
            })
            .then_with(|| coordinate(left).total_cmp(&coordinate(right)))
    })?;
    for (position, index) in numbers.iter().enumerate() {
        let left = &atoms[*index];
        for index in &numbers[position + 1..] {
            budget.compare()?;
            let right = &atoms[*index];
            if right.orientation != left.orientation
                || major_start(right.bounds, right.orientation)
                    >= major_end(left.bounds, left.orientation)
            {
                break;
            }
            let font = left
                .font_size
                .unwrap_or(left.bounds.height)
                .max(right.font_size.unwrap_or(right.bounds.height));
            let distance = (minor_center(left.bounds, left.orientation)
                - minor_center(right.bounds, right.orientation))
            .abs();
            let spacing = if font < body_font * 0.85 { body_font * 1.5 } else { font * 0.75 };
            if distance < spacing && crate::lines::collides_across_rows(left, right) {
                return Err(ConversionError::Malformed {
                    part: Some("pdf-layout-visual".into()),
                    detail: "stacked numeric notation requires a page image to preserve its two-dimensional relationship".into(),
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn reference(atoms: &[Atom]) -> Option<(f32, f32)> {
    atoms
        .iter()
        .filter_map(|atom| Some((atom.font_size?, source(atom)?)))
        .max_by(|left, right| left.0.total_cmp(&right.0))
}

/// Native superscripts can overhang the following glyph. Within that overlap,
/// the source character sequence preserves the marker's place in the text.
pub(crate) fn restore_overlapping_scripts(
    atoms: &mut [Atom],
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    for index in 1..atoms.len() {
        let mut cursor = index;
        while cursor > 0 {
            budget.compare()?;
            let (left, right) = (&atoms[cursor - 1], &atoms[cursor]);
            let (Some(left_base), Some(right_base), Some(left_font), Some(right_font)) =
                (source(left), source(right), left.font_size, right.font_size)
            else {
                break;
            };
            let font = left_font.max(right_font);
            if left.source_index <= right.source_index
                || left.orientation != right.orientation
                || left_font.min(right_font) >= font * 0.85
                || (left_base - right_base).abs() < font * 0.15
                || major_end(left.bounds, left.orientation)
                    <= major_start(right.bounds, right.orientation)
                || major_end(right.bounds, right.orientation)
                    <= major_start(left.bounds, left.orientation)
            {
                break;
            }
            atoms.swap(cursor - 1, cursor);
            cursor -= 1;
        }
    }
    Ok(())
}

pub(crate) fn mark_script(
    atom: &mut Atom,
    reference: Option<(f32, f32)>,
) -> Result<(), ConversionError> {
    use into_markdown_core::InlineMark;
    let (Some((font, baseline)), Some(atom_font), Some(atom_baseline)) =
        (reference, atom.font_size, source(atom))
    else {
        return Ok(());
    };
    let offset = atom_baseline - baseline;
    if atom_font > font * 0.85 || offset.abs() < font * 0.15 {
        return Ok(());
    }
    if let Inline::SourceText { marks, .. } = &mut atom.inline {
        if !marks.contains(&InlineMark::Superscript) && !marks.contains(&InlineMark::Subscript) {
            marks.try_reserve(1).map_err(|_| memory("layout script mark"))?;
            marks.push(if offset < 0.0 { InlineMark::Superscript } else { InlineMark::Subscript });
        }
    }
    Ok(())
}

fn check_drop_caps(
    atoms: &[Atom],
    font: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    if font <= 0.0 {
        return Ok(());
    }
    for cap in atoms {
        budget.checkpoint_item()?;
        let Some(baseline) = source(cap) else { continue };
        let text = crate::lines::inline_text(&cap.inline);
        if cap.orientation != 0
            || cap.font_size.unwrap_or(0.0) < font * 1.4
            || cap.bounds.height < font * 1.25
            || text.chars().count() != 1
            || !text.chars().all(char::is_uppercase)
        {
            continue;
        }
        let edge = cap.bounds.x + cap.bounds.width;
        for next in atoms {
            budget.compare()?;
            if next.orientation == 0
                && next.font_size.is_some_and(|size| size <= font * 1.1)
                && next.bounds.x >= edge - font * 0.2
                && next.bounds.x <= edge + font * 2.0
                && (next.bounds.y - cap.bounds.y).abs() <= font * 0.5
                && source(next).is_some_and(|next_baseline| baseline - next_baseline > font * 0.5)
            {
                return Err(ConversionError::Malformed {
                    part: Some("pdf-layout-visual".into()),
                    detail: "drop capital spanning text rows requires its source page image".into(),
                });
            }
        }
    }
    Ok(())
}

/// A small isolated native script must retain its visual relationship to the
/// adjacent expression rather than becoming a separate paragraph.
pub(crate) fn validate_detached_scripts(
    lines: &[crate::model::Line],
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let mut references = Vec::new();
    references.try_reserve_exact(lines.len()).map_err(|_| memory("layout script references"))?;
    for line in lines {
        budget.checkpoint_item()?;
        references.push(reference(&line.atoms));
    }
    for (index, script) in lines.iter().enumerate() {
        budget.checkpoint_item()?;
        if script.atoms.is_empty() {
            continue;
        }
        let Some((font, baseline)) = references[index] else { continue };
        for (body_index, body) in lines.iter().enumerate() {
            budget.compare()?;
            if body.orientation != script.orientation || body.atoms.len() < 8 {
                continue;
            }
            let Some((body_font, body_baseline)) = references[body_index] else { continue };
            if font >= body_font * 0.85
                || (baseline - body_baseline).abs() >= body_font * 0.9
                || major_end(script.bounds, script.orientation)
                    < major_start(body.bounds, body.orientation) - body_font
                || major_start(script.bounds, script.orientation)
                    > major_end(body.bounds, body.orientation) + body_font
            {
                continue;
            }
            return Err(ConversionError::Malformed {
                part: Some("pdf-layout-visual".into()),
                detail: "detached native script requires its source page image to preserve expression relationships".into(),
            });
        }
    }
    Ok(())
}
