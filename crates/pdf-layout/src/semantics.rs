use crate::budget::LayoutBudget;
use crate::footnotes;
use crate::geometry::{major_end, major_start, minor_center, minor_extent, union};
use crate::lines;
use crate::memory;
use crate::model::{Atom, Line, RebuiltBlock, block_provenance};
use into_markdown_core::{
    Block, BlockNode, ConversionError, Inline, ListItem, ListKind, NodeId, Rect,
};
use std::collections::VecDeque;

#[allow(
    clippy::too_many_lines,
    reason = "one transactional semantic dispatch over a consumed queue"
)]
pub(crate) fn blocks(
    page: u32,
    lines: Vec<Line>,
    width: f32,
    height: f32,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<RebuiltBlock>, ConversionError> {
    let median_font = median_font_size(&lines);
    let mut pending = VecDeque::new();
    pending.try_reserve_exact(lines.len()).map_err(|_| memory("layout semantic queue"))?;
    pending.extend(lines);
    let mut output = Vec::new();
    output.try_reserve_exact(pending.len()).map_err(|_| memory("layout semantic blocks"))?;
    let mut sequence = 0_usize;
    while let Some(line) = pending.pop_front() {
        budget.checkpoint_item()?;
        if let Some(level) = heading_level(&line, median_font) {
            let bounds = line.bounds;
            let orientation = line.orientation;
            let source_index = line.source_index;
            let confidence = line_confidence(&line);
            output.push(RebuiltBlock {
                node: BlockNode {
                    id: id(page, "heading", sequence),
                    block: Block::Heading { level, content: take_line_inlines(line)? },
                    provenance: block_provenance(page, bounds, width, height, confidence),
                },
                bounds: Some(bounds),
                orientation,
                source_index,
            });
            sequence += 1;
            continue;
        }
        if let Some(marker) = list_marker(&line) {
            let run = list_run_length(&line, marker.kind, &pending);
            if run >= 2 {
                let mut list_lines = Vec::new();
                list_lines.try_reserve_exact(run).map_err(|_| memory("layout list lines"))?;
                list_lines.push((line, marker));
                for _ in 1..run {
                    let next = pending.pop_front().ok_or_else(|| memory("layout list queue"))?;
                    let next_marker =
                        list_marker(&next).ok_or_else(|| memory("layout list marker"))?;
                    list_lines.push((next, next_marker));
                }
                output.push(list_block(
                    page,
                    marker.kind,
                    marker.start,
                    list_lines,
                    width,
                    height,
                    sequence,
                )?);
                sequence += 1;
                continue;
            }
        }
        if let Some((prefix_chars, digits)) = footnote_marker(&line, median_font, height) {
            let bounds = line.bounds;
            let orientation = line.orientation;
            let source_index = line.source_index;
            let confidence = line_confidence(&line);
            let mut content = take_line_inlines(line)?;
            strip_prefix(&mut content, prefix_chars);
            let paragraph = BlockNode {
                id: id(page, "footnote-text", sequence),
                block: Block::Paragraph(content),
                provenance: block_provenance(page, bounds, width, height, confidence),
            };
            output.push(RebuiltBlock {
                node: BlockNode {
                    id: id(page, "footnote", sequence),
                    block: Block::Footnote {
                        label: footnotes::label(page, &digits)?,
                        blocks: vec![paragraph],
                    },
                    provenance: block_provenance(page, bounds, width, height, confidence),
                },
                bounds: Some(bounds),
                orientation,
                source_index,
            });
            sequence += 1;
            continue;
        }

        let source_index = line.source_index;
        let orientation = line.orientation;
        let mut bounds = line.bounds;
        let mut confidence = line_confidence(&line);
        let mut content = take_line_inlines(line)?;
        while pending.front().is_some_and(|next| {
            heading_level(next, median_font).is_none()
                && list_marker(next).is_none()
                && footnote_marker(next, median_font, height).is_none()
                && paragraph_continues(bounds, orientation, next)
        }) {
            let next = pending.pop_front().expect("front checked");
            append_line(&mut content, take_line_inlines(next)?)?;
            confidence = min_confidence(confidence, line_confidence_from_inlines(&content));
            bounds = union(bounds, pending_bounds(&content).unwrap_or(bounds));
        }
        output.push(RebuiltBlock {
            node: BlockNode {
                id: id(page, "paragraph", sequence),
                block: Block::Paragraph(content),
                provenance: block_provenance(page, bounds, width, height, confidence),
            },
            bounds: Some(bounds),
            orientation,
            source_index,
        });
        sequence += 1;
    }
    Ok(output)
}

pub(crate) fn take_line_inlines(line: Line) -> Result<Vec<Inline>, ConversionError> {
    let mut spaced = Vec::new();
    spaced
        .try_reserve_exact(line.atoms.len().saturating_mul(2))
        .map_err(|_| memory("layout inline materialization"))?;
    let mut prior: Option<(Rect, u16, char)> = None;
    for atom in line.atoms {
        let text = lines::inline_text(&atom.inline);
        let first = text.chars().next();
        if let (Some((bounds, orientation, last)), Some(first)) = (prior, first)
            && gap_requires_space(bounds, orientation, last, atom.bounds, first)
        {
            spaced.push(Inline::Text { value: " ".into(), marks: Vec::new() });
        }
        let last = text.chars().next_back();
        if let Some(last) = last {
            prior = Some((atom.bounds, atom.orientation, last));
        }
        spaced.push(atom.inline);
    }
    Ok(spaced)
}

fn gap_requires_space(
    left_bounds: Rect,
    orientation: u16,
    left: char,
    right_bounds: Rect,
    right: char,
) -> bool {
    !left.is_whitespace()
        && !right.is_whitespace()
        && left.is_ascii_alphanumeric()
        && right.is_ascii_alphanumeric()
        && major_start(right_bounds, orientation) - major_end(left_bounds, orientation)
            > if matches!(orientation, 90 | 270) { right_bounds.width } else { right_bounds.height }
                * 0.6
}

fn append_line(output: &mut Vec<Inline>, mut next: Vec<Inline>) -> Result<(), ConversionError> {
    let left = last_text_char(output);
    let right = first_text_char(&next);
    if left.is_some_and(|value| value.is_ascii_alphanumeric())
        && right.is_some_and(|value| value.is_ascii_alphanumeric())
        && left.is_none_or(|value| value != '-')
    {
        output.try_reserve(1).map_err(|_| memory("layout paragraph space"))?;
        output.push(Inline::Text { value: " ".into(), marks: Vec::new() });
    }
    output.try_reserve(next.len()).map_err(|_| memory("layout paragraph append"))?;
    output.append(&mut next);
    Ok(())
}

fn paragraph_continues(bounds: Rect, orientation: u16, next: &Line) -> bool {
    if next.orientation != orientation {
        return false;
    }
    let extent =
        minor_extent(bounds, orientation).min(minor_extent(next.bounds, orientation)).max(1.0);
    let gap = minor_center(next.bounds, orientation)
        - minor_center(bounds, orientation)
        - f32::midpoint(minor_extent(bounds, orientation), minor_extent(next.bounds, orientation));
    gap >= -extent * 0.25
        && gap <= extent * 2.5
        && (major_start(next.bounds, orientation) - major_start(bounds, orientation)).abs()
            <= extent * 1.5
}

fn heading_level(line: &Line, median_font: Option<f32>) -> Option<u8> {
    let font = line.font_size?;
    let median = median_font?;
    let text = lines::text(line);
    let length = text.trim().chars().count();
    if length == 0 || length > 160 || font < median * 1.25 {
        return None;
    }
    Some(if font >= median * 2.0 {
        1
    } else if font >= median * 1.65 {
        2
    } else if font >= median * 1.4 {
        3
    } else {
        4
    })
}

#[derive(Clone, Copy)]
struct Marker {
    kind: ListKind,
    start: u64,
    prefix_chars: usize,
}

fn list_marker(line: &Line) -> Option<Marker> {
    let text = lines::text(line);
    let trimmed = text.trim_start();
    let leading = text.chars().count() - trimmed.chars().count();
    for marker in ['-', '*', '•', '◦', '▪'] {
        if let Some(rest) = trimmed.strip_prefix(marker)
            && rest.starts_with(char::is_whitespace)
        {
            return Some(Marker { kind: ListKind::Bullet, start: 1, prefix_chars: leading + 2 });
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).take(10).collect::<String>();
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    let rest = &trimmed[digits.len()..];
    if (rest.starts_with(". ") || rest.starts_with(") "))
        && let Ok(start) = digits.parse()
    {
        return Some(Marker {
            kind: ListKind::Ordered,
            start,
            prefix_chars: leading + digits.chars().count() + 2,
        });
    }
    None
}

fn list_run_length(first: &Line, kind: ListKind, pending: &VecDeque<Line>) -> usize {
    let mut count = 1;
    let mut previous = first;
    for line in pending {
        let Some(marker) = list_marker(line) else { break };
        let height = previous.bounds.height.max(line.bounds.height).max(1.0);
        if marker.kind != kind
            || (line.bounds.x - first.bounds.x).abs() > height * 1.5
            || line.bounds.y - (previous.bounds.y + previous.bounds.height) > height * 2.5
        {
            break;
        }
        count += 1;
        previous = line;
    }
    count
}

fn list_block(
    page: u32,
    kind: ListKind,
    start: u64,
    lines: Vec<(Line, Marker)>,
    width: f32,
    height: f32,
    sequence: usize,
) -> Result<RebuiltBlock, ConversionError> {
    let mut items = Vec::new();
    items.try_reserve_exact(lines.len()).map_err(|_| memory("layout list items"))?;
    let mut list_bounds = lines[0].0.bounds;
    let source_index = lines[0].0.source_index;
    let mut confidence = None;
    for (item_index, (line, marker)) in lines.into_iter().enumerate() {
        let bounds = line.bounds;
        confidence = min_confidence(confidence, line_confidence(&line));
        list_bounds = union(list_bounds, bounds);
        let mut content = take_line_inlines(line)?;
        strip_prefix(&mut content, marker.prefix_chars);
        items.push(ListItem {
            checked: None,
            marker_label: None,
            blocks: vec![BlockNode {
                id: id(page, "list-item", sequence * 10_000 + item_index),
                block: Block::Paragraph(content),
                provenance: block_provenance(page, bounds, width, height, confidence),
            }],
        });
    }
    Ok(RebuiltBlock {
        node: BlockNode {
            id: id(page, "list", sequence),
            block: Block::List { kind, start, items },
            provenance: block_provenance(page, list_bounds, width, height, confidence),
        },
        bounds: Some(list_bounds),
        orientation: 0,
        source_index,
    })
}

fn footnote_marker(line: &Line, median: Option<f32>, page_height: f32) -> Option<(usize, String)> {
    let font = line.font_size?;
    let median = median?;
    if font > median * 0.82 || line.bounds.y < page_height * 0.72 {
        return None;
    }
    let text = lines::text(line);
    let trimmed = text.trim_start();
    let digits = trimmed.chars().take_while(char::is_ascii_digit).take(4).collect::<String>();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let rest = &trimmed[digits.len()..];
    if !(rest.starts_with(' ') || rest.starts_with('.') || rest.starts_with(')')) {
        return None;
    }
    Some((digits.chars().count() + 1, digits))
}

fn strip_prefix(inlines: &mut Vec<Inline>, mut characters: usize) {
    let mut index = 0;
    while index < inlines.len() && characters > 0 {
        let (Inline::Text { value, .. }
        | Inline::SourceText { value, .. }
        | Inline::OcrText { value, .. }) = &mut inlines[index]
        else {
            index += 1;
            continue;
        };
        let count = value.chars().count();
        if count <= characters {
            characters -= count;
            inlines.remove(index);
        } else {
            let byte = value.char_indices().nth(characters).map_or(value.len(), |(byte, _)| byte);
            value.drain(..byte);
            characters = 0;
        }
    }
}

fn median_font_size(lines: &[Line]) -> Option<f32> {
    let mut fonts = lines.iter().filter_map(|line| line.font_size).collect::<Vec<_>>();
    fonts.sort_by(f32::total_cmp);
    fonts.get(fonts.len() / 2).copied()
}

fn line_confidence(line: &Line) -> Option<f32> {
    line_confidence_from_atoms(&line.atoms)
}

fn line_confidence_from_atoms(atoms: &[Atom]) -> Option<f32> {
    atoms
        .iter()
        .filter_map(|atom| match &atom.inline {
            Inline::SourceText { provenance, .. } | Inline::OcrText { provenance, .. } => {
                provenance.confidence
            }
            _ => None,
        })
        .reduce(f32::min)
}

fn line_confidence_from_inlines(inlines: &[Inline]) -> Option<f32> {
    inlines
        .iter()
        .filter_map(|inline| match inline {
            Inline::SourceText { provenance, .. } | Inline::OcrText { provenance, .. } => {
                provenance.confidence
            }
            _ => None,
        })
        .reduce(f32::min)
}

fn min_confidence(left: Option<f32>, right: Option<f32>) -> Option<f32> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn pending_bounds(inlines: &[Inline]) -> Option<Rect> {
    inlines
        .iter()
        .filter_map(|inline| match inline {
            Inline::SourceText { provenance, .. } | Inline::OcrText { provenance, .. } => {
                provenance.locator.bounds
            }
            _ => None,
        })
        .reduce(union)
}

fn first_text_char(inlines: &[Inline]) -> Option<char> {
    inlines.iter().find_map(|inline| lines::inline_text(inline).chars().next())
}

fn last_text_char(inlines: &[Inline]) -> Option<char> {
    inlines.iter().rev().find_map(|inline| lines::inline_text(inline).chars().next_back())
}

fn id(page: u32, kind: &str, sequence: usize) -> NodeId {
    NodeId(format!("pdf-page-{page}-layout-{kind}-{sequence}"))
}
