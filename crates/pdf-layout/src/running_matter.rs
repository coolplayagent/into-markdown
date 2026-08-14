use crate::budget::LayoutBudget;
use crate::lines;
use crate::{LAYOUT_PROVIDER, memory};
use into_markdown_core::{Block, BlockNode, ConversionError};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

const EDGE_FRACTION: f32 = 0.12;
const HEADER_PART: &str = "pdf/running-header";
const FOOTER_PART: &str = "pdf/running-footer";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Edge {
    Header,
    Footer,
}

/// Mark exact repeated page-edge matter without deleting source content or
/// inventing a format-specific public IR node.
pub(crate) fn annotate(
    roots: &mut [BlockNode],
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let mut pages_by_key: BTreeMap<(Edge, String), BTreeSet<u32>> = BTreeMap::new();
    for root in roots.iter() {
        budget.checkpoint_item()?;
        let Block::Page { number, blocks } = &root.block else { continue };
        let Some(height) = root
            .provenance
            .locator
            .page_height
            .filter(|height| height.is_finite() && *height > 0.0)
        else {
            continue;
        };
        for node in blocks {
            budget.checkpoint_item()?;
            let Some(edge) = edge(node, height) else { continue };
            let Some(text) = canonical_text(node, budget)? else { continue };
            pages_by_key.entry((edge, text)).or_default().insert(*number);
        }
    }

    let repeated = pages_by_key
        .into_iter()
        .filter_map(|(key, pages)| (pages.len() >= 2).then_some(key))
        .collect::<BTreeSet<_>>();
    if repeated.is_empty() {
        return Ok(());
    }
    for root in roots {
        budget.checkpoint_item()?;
        let Block::Page { blocks, .. } = &mut root.block else { continue };
        let Some(height) = root.provenance.locator.page_height else { continue };
        for node in blocks {
            budget.checkpoint_item()?;
            let Some(edge) = edge(node, height) else { continue };
            let Some(text) = canonical_text(node, budget)? else { continue };
            if repeated.contains(&(edge, text)) {
                node.provenance.locator.part = Some(part(edge)?);
            }
        }
    }
    Ok(())
}

fn edge(node: &BlockNode, page_height: f32) -> Option<Edge> {
    if node.provenance.provider != LAYOUT_PROVIDER
        || !matches!(node.block, Block::Paragraph(_) | Block::Heading { .. })
    {
        return None;
    }
    let bounds = node.provenance.locator.bounds?;
    if bounds.y + bounds.height <= page_height * EDGE_FRACTION {
        Some(Edge::Header)
    } else if bounds.y >= page_height * (1.0 - EDGE_FRACTION) {
        Some(Edge::Footer)
    } else {
        None
    }
}

fn canonical_text(
    node: &BlockNode,
    budget: &mut LayoutBudget<'_>,
) -> Result<Option<String>, ConversionError> {
    let (Block::Paragraph(inlines) | Block::Heading { content: inlines, .. }) = &node.block else {
        return Ok(None);
    };
    for inline in inlines {
        budget.checkpoint_bytes(lines::inline_text(inline).len())?;
    }
    let normalized = || inlines.iter().flat_map(|inline| lines::inline_text(inline).chars()).nfc();
    let mut capacity = 0_usize;
    let mut pending_space = false;
    for character in normalized() {
        if character.is_whitespace() {
            pending_space = capacity > 0;
        } else {
            if pending_space {
                capacity = capacity.checked_add(1).ok_or_else(|| memory("running matter text"))?;
                pending_space = false;
            }
            capacity = capacity
                .checked_add(character.len_utf8())
                .ok_or_else(|| memory("running matter text"))?;
        }
    }
    if capacity == 0 {
        return Ok(None);
    }
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|_| memory("running matter text"))?;
    pending_space = false;
    for character in normalized() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(character);
        }
    }
    Ok(Some(output))
}

fn part(edge: Edge) -> Result<String, ConversionError> {
    let value = match edge {
        Edge::Header => HEADER_PART,
        Edge::Footer => FOOTER_PART,
    };
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| memory("running matter part"))?;
    output.push_str(value);
    Ok(output)
}
