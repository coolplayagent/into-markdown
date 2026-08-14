use crate::budget::LayoutBudget;
use crate::memory;
use into_markdown_core::{Block, BlockNode, ConversionError, Inline};
use std::fmt::Write as _;

pub(crate) fn label(page: u32, digits: &str) -> Result<String, ConversionError> {
    let capacity =
        digits.len().checked_add(32).ok_or_else(|| memory("layout footnote label length"))?;
    let mut label = String::new();
    label.try_reserve_exact(capacity).map_err(|_| memory("layout footnote label"))?;
    write!(label, "pdf-page-{page}-{digits}").map_err(|_| memory("layout footnote label"))?;
    Ok(label)
}

pub(crate) fn namespace_existing(
    nodes: &mut [BlockNode],
    page: u32,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let mut blocks = Vec::new();
    blocks.try_reserve_exact(nodes.len()).map_err(|_| memory("layout footnote block stack"))?;
    blocks.extend(nodes.iter_mut());
    while let Some(node) = blocks.pop() {
        budget.checkpoint_item()?;
        match &mut node.block {
            Block::Paragraph(inlines)
            | Block::Heading { content: inlines, .. }
            | Block::TimedSegment { content: inlines, .. } => {
                namespace_inlines(inlines, page, budget)?;
            }
            Block::List { items, .. } => {
                let count = items.iter().try_fold(0_usize, |total, item| {
                    total
                        .checked_add(item.blocks.len())
                        .ok_or_else(|| memory("layout footnote list count"))
                })?;
                blocks.try_reserve(count).map_err(|_| memory("layout footnote list stack"))?;
                blocks.extend(items.iter_mut().flat_map(|item| item.blocks.iter_mut()));
            }
            Block::Table { rows, .. } => {
                let count =
                    rows.iter().flat_map(|row| &row.cells).try_fold(0_usize, |total, cell| {
                        total
                            .checked_add(cell.blocks.len())
                            .ok_or_else(|| memory("layout footnote table count"))
                    })?;
                blocks.try_reserve(count).map_err(|_| memory("layout footnote table stack"))?;
                blocks.extend(
                    rows.iter_mut()
                        .flat_map(|row| &mut row.cells)
                        .flat_map(|cell| cell.blocks.iter_mut()),
                );
            }
            Block::Footnote { label: existing, blocks: children } => {
                rewrite_legacy(existing, page)?;
                blocks
                    .try_reserve(children.len())
                    .map_err(|_| memory("layout footnote definition stack"))?;
                blocks.extend(children.iter_mut());
            }
            Block::Page { blocks: children, .. }
            | Block::Slide { blocks: children, .. }
            | Block::Sheet { blocks: children, .. } => {
                blocks
                    .try_reserve(children.len())
                    .map_err(|_| memory("layout footnote container stack"))?;
                blocks.extend(children.iter_mut());
            }
            _ => {}
        }
    }
    Ok(())
}

fn namespace_inlines(
    inlines: &mut [Inline],
    page: u32,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    let mut stack = Vec::new();
    stack.try_reserve_exact(inlines.len()).map_err(|_| memory("layout footnote inline stack"))?;
    stack.extend(inlines.iter_mut());
    while let Some(inline) = stack.pop() {
        budget.checkpoint_item()?;
        match inline {
            Inline::FootnoteReference(existing) => rewrite_legacy(existing, page)?,
            Inline::Link { content, .. } => {
                stack
                    .try_reserve(content.len())
                    .map_err(|_| memory("layout footnote link stack"))?;
                stack.extend(content.iter_mut());
            }
            _ => {}
        }
    }
    Ok(())
}

fn rewrite_legacy(existing: &mut String, page: u32) -> Result<(), ConversionError> {
    let Some(digits) = existing.strip_prefix("pdf-") else { return Ok(()) };
    if digits.is_empty() || !digits.bytes().all(|value| value.is_ascii_digit()) {
        return Ok(());
    }
    *existing = label(page, digits)?;
    Ok(())
}
