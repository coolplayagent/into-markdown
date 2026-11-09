//! Evidence for directory rows is local to their displayed text column.
use crate::budget::LayoutBudget;
use crate::geometry::{major_end, major_start, minor_extent};
use crate::links::Links;
use crate::model::{Atom, Line};
use crate::{lines, memory};
use into_markdown_core::ConversionError;

pub(super) struct Hint {
    pub tail: usize,
    pub leaders: bool,
    pub linked: bool,
    pub indent: f32,
    pub end: f32,
    pub font: f32,
    pub depth: Option<usize>,
}

pub(super) fn describe(
    atoms: &[Atom],
    orientation: u16,
    associations: &Links,
    budget: &mut LayoutBudget<'_>,
) -> Result<Option<Hint>, ConversionError> {
    let Some(last) = atoms.iter().rposition(|a| !text(a).trim().is_empty()) else {
        return Ok(None);
    };
    let font =
        atoms[last].font_size.unwrap_or(minor_extent(atoms[last].bounds, orientation)).max(1.0);
    let mut tail = last;
    while tail > 0 {
        budget.compare()?;
        let prior = &atoms[tail - 1];
        if prior.space_after
            || text(prior).is_empty()
            || !text(prior).chars().all(page_character)
            || major_start(atoms[tail].bounds, orientation) - major_end(prior.bounds, orientation)
                >= font * 0.5
        {
            break;
        }
        tail -= 1;
    }
    let label = join(&atoms[tail..=last], budget)?;
    if tail == 0 || !page_label(&label) {
        return Ok(None);
    }
    let prefix = join(&atoms[..tail], budget)?;
    if !prefix.chars().any(char::is_alphabetic) {
        return Ok(None);
    }
    let Some(prior) = atoms[..tail].iter().rfind(|a| !text(a).trim().is_empty()) else {
        return Ok(None);
    };
    let leaders = prefix
        .trim_end()
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace() || matches!(c, '.' | '·' | '…' | '．'))
        .filter(|c| !c.is_whitespace())
        .count()
        >= 3
        || prefix.trim_end().ends_with('…');
    let gap = major_start(atoms[tail].bounds, orientation) - major_end(prior.bounds, orientation);
    if !leaders && gap < font * 0.5 && !prior.space_after {
        return Ok(None);
    }
    let mut linked = false;
    for atom in atoms {
        budget.checkpoint_item()?;
        linked |= associations.internal_target(&atom.inline, budget)?.is_some();
    }
    Ok(Some(Hint {
        tail,
        leaders,
        linked,
        indent: major_start(atoms[0].bounds, orientation),
        end: major_end(atoms[last].bounds, orientation),
        font,
        depth: number_depth(&prefix),
    }))
}

fn text(atom: &Atom) -> &str {
    lines::inline_text(&atom.inline)
}

fn page_character(value: char) -> bool {
    value.is_ascii_digit()
        || matches!(
            value,
            'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm' | 'I' | 'V' | 'X' | 'L' | 'C' | 'D' | 'M' | '-'
        )
}

fn page_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16
        && (value.chars().all(|c| c.is_ascii_digit())
            || value.chars().all(|c| {
                matches!(
                    c,
                    'i' | 'v'
                        | 'x'
                        | 'l'
                        | 'c'
                        | 'd'
                        | 'm'
                        | 'I'
                        | 'V'
                        | 'X'
                        | 'L'
                        | 'C'
                        | 'D'
                        | 'M'
                )
            })
            || value.split_once('-').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.chars().all(|c| c.is_ascii_digit())
                    && b.chars().all(|c| c.is_ascii_digit())
            }))
}

pub(super) fn number_depth(value: &str) -> Option<usize> {
    let value = value.trim_start();
    let end = value.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(value.len());
    let prefix = &value[..end];
    let prefix = prefix.trim_end_matches('.');
    (!prefix.is_empty() && prefix.split('.').all(|part| !part.is_empty()))
        .then(|| prefix.split('.').count() - 1)
}

fn join(atoms: &[Atom], budget: &mut LayoutBudget<'_>) -> Result<String, ConversionError> {
    let length = atoms
        .iter()
        .try_fold(0_usize, |n, a| n.checked_add(text(a).len() + 1))
        .ok_or_else(|| memory("directory row text"))?;
    let mut bytes = budget.reserve(length)?;
    for atom in atoms {
        budget.checkpoint_item()?;
        bytes.extend_from_slice(text(atom).as_bytes());
        if atom.space_after {
            bytes.push(b' ');
        }
    }
    String::from_utf8(bytes).map_err(|_| memory("directory row encoding"))
}

pub(super) fn split_columns(
    lines: Vec<Line>,
    associations: &Links,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Line>, ConversionError> {
    let mut result = budget.reserve(lines.len())?;
    for mut line in lines {
        while let Some(index) = column_cut(&line, associations, budget)? {
            let mut right = budget.reserve(line.atoms.len() - index)?;
            right.extend(line.atoms.drain(index..));
            let right = from_atoms(right, &line)?;
            line.bounds = bounds(&line.atoms).ok_or_else(|| memory("directory left column"))?;
            budget.push(&mut result, line)?;
            line = right;
        }
        budget.push(&mut result, line)?;
    }
    Ok(result)
}

fn column_cut(
    line: &Line,
    associations: &Links,
    budget: &mut LayoutBudget<'_>,
) -> Result<Option<usize>, ConversionError> {
    let font = line.font_size.unwrap_or(line.bounds.height).max(1.0);
    for index in 1..line.atoms.len() {
        budget.compare()?;
        let gap = major_start(line.atoms[index].bounds, line.orientation)
            - major_end(line.atoms[index - 1].bounds, line.orientation);
        if gap < font * 2.0 {
            continue;
        }
        let left = describe(&line.atoms[..index], line.orientation, associations, budget)?;
        let right = describe(&line.atoms[index..], line.orientation, associations, budget)?;
        if left.is_some_and(|h| h.linked || h.leaders)
            && right.is_some_and(|h| h.linked || h.leaders)
        {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn from_atoms(atoms: Vec<Atom>, original: &Line) -> Result<Line, ConversionError> {
    Ok(Line {
        bounds: bounds(&atoms).ok_or_else(|| memory("directory right column"))?,
        source_index: atoms.iter().map(|a| a.source_index).min().unwrap_or(original.source_index),
        atoms,
        font_size: original.font_size,
        orientation: original.orientation,
        source_kind: original.source_kind,
    })
}

fn bounds(atoms: &[Atom]) -> Option<into_markdown_core::Rect> {
    atoms.iter().map(|a| a.bounds).reduce(crate::geometry::union)
}

pub(super) fn heading(line: &Line, budget: &mut LayoutBudget<'_>) -> Result<bool, ConversionError> {
    let value = lines::fallible_text(line, budget)?;
    let value = value.trim().to_lowercase();
    Ok(matches!(
        value.as_str(),
        "contents"
            | "table of contents"
            | "目录"
            | "目錄"
            | "list of figures"
            | "list of tables"
            | "图目录"
            | "表目录"
    ))
}
