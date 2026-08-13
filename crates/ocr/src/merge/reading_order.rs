use super::budget::MergeBudget;
use super::geometry::union_rect;
use into_markdown_core::{Block, BlockNode, ConversionError, Inline, Rect, SourceLocator};

#[derive(Clone, Copy)]
struct ReadingKey {
    page: Option<u32>,
    bounds: Option<Rect>,
}

#[derive(Clone, Copy)]
enum PageEvidence {
    Missing,
    One(u32),
    Conflicting,
}

impl PageEvidence {
    fn observe(&mut self, page: u32) {
        *self = match *self {
            Self::Missing => Self::One(page),
            Self::One(current) if current == page => Self::One(current),
            Self::One(_) | Self::Conflicting => Self::Conflicting,
        };
    }

    const fn page(self) -> Option<u32> {
        match self {
            Self::One(page) => Some(page),
            Self::Missing | Self::Conflicting => None,
        }
    }
}

struct ReadingWalk<'budget, 'context> {
    budget: &'budget mut MergeBudget<'context>,
    visited: usize,
}

impl ReadingWalk<'_, '_> {
    fn visit(&mut self) -> Result<(), ConversionError> {
        self.visited = self.visited.checked_add(1).ok_or_else(super::memory)?;
        self.budget.consume(1)?;
        super::traversal_checkpoint(self.budget.context(), self.visited)
    }
}

enum Frame<'a> {
    Blocks(&'a [BlockNode]),
    Inlines(&'a [Inline]),
}

/// Sort a flat document or one page container with the same fallible key.
/// Existing order is the stable fallback for equal or missing geometry.
pub(crate) fn sort_blocks(
    blocks: &mut Vec<BlockNode>,
    fallback_page: Option<u32>,
    budget: &mut MergeBudget<'_>,
) -> Result<(), ConversionError> {
    let mut keyed = Vec::new();
    keyed.try_reserve_exact(blocks.len()).map_err(|_| super::memory())?;
    let mut walk = ReadingWalk { budget, visited: 0 };
    for node in blocks.drain(..) {
        keyed.push((reading_key(&node, fallback_page, &mut walk)?, node));
    }
    keyed.sort_by(|(left, _), (right, _)| compare_keys(*left, *right));
    blocks.extend(keyed.into_iter().map(|(_, node)| node));
    Ok(())
}

fn reading_key(
    node: &BlockNode,
    fallback_page: Option<u32>,
    walk: &mut ReadingWalk<'_, '_>,
) -> Result<ReadingKey, ConversionError> {
    let page = if let Some(page) = node.provenance.locator.page {
        Some(page)
    } else {
        descendant_page(node, walk)?.or(fallback_page)
    };
    let bounds = if let Some(bounds) = node.provenance.locator.bounds {
        Some(bounds)
    } else {
        descendant_bounds(node, page, walk)?
    };
    Ok(ReadingKey { page, bounds })
}

fn descendant_page(
    node: &BlockNode,
    walk: &mut ReadingWalk<'_, '_>,
) -> Result<Option<u32>, ConversionError> {
    let mut evidence = PageEvidence::Missing;
    visit_source_locators(node, walk, |locator| {
        if let Some(page) = locator.page {
            evidence.observe(page);
        }
    })?;
    Ok(evidence.page())
}

fn descendant_bounds(
    node: &BlockNode,
    page: Option<u32>,
    walk: &mut ReadingWalk<'_, '_>,
) -> Result<Option<Rect>, ConversionError> {
    let mut bounds = None;
    visit_source_locators(node, walk, |locator| {
        let belongs = match (page, locator.page) {
            (Some(expected), Some(actual)) => expected == actual,
            (Some(_) | None, None) => true,
            (None, Some(_)) => false,
        };
        if belongs && let Some(value) = locator.bounds {
            bounds = Some(bounds.map_or(value, |current| union_rect(current, value)));
        }
    })?;
    Ok(bounds)
}

fn visit_source_locators(
    node: &BlockNode,
    walk: &mut ReadingWalk<'_, '_>,
    mut visit: impl FnMut(&SourceLocator),
) -> Result<(), ConversionError> {
    let mut stack = Vec::new();
    stack.try_reserve_exact(1).map_err(|_| super::memory())?;
    push_block_content(&node.block, &mut stack)?;
    walk.visit()?;
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Blocks(blocks) => {
                for child in blocks {
                    walk.visit()?;
                    push_block_content(&child.block, &mut stack)?;
                }
            }
            Frame::Inlines(inlines) => {
                for inline in inlines {
                    walk.visit()?;
                    match inline {
                        Inline::SourceText { provenance, .. }
                        | Inline::OcrText { provenance, .. } => visit(&provenance.locator),
                        Inline::Link { content, .. } => {
                            stack.try_reserve(1).map_err(|_| super::memory())?;
                            stack.push(Frame::Inlines(content));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn push_block_content<'a>(
    block: &'a Block,
    stack: &mut Vec<Frame<'a>>,
) -> Result<(), ConversionError> {
    match block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => {
            stack.try_reserve(1).map_err(|_| super::memory())?;
            stack.push(Frame::Inlines(inlines));
        }
        Block::List { items, .. } => {
            stack.try_reserve(items.len()).map_err(|_| super::memory())?;
            stack.extend(items.iter().map(|item| Frame::Blocks(item.blocks.as_slice())));
        }
        Block::Table { rows, .. } => {
            let cells = rows.iter().try_fold(0_usize, |total, row| {
                total.checked_add(row.cells.len()).ok_or_else(super::memory)
            })?;
            stack.try_reserve(cells).map_err(|_| super::memory())?;
            stack.extend(
                rows.iter()
                    .flat_map(|row| &row.cells)
                    .map(|cell| Frame::Blocks(cell.blocks.as_slice())),
            );
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => {
            stack.try_reserve(1).map_err(|_| super::memory())?;
            stack.push(Frame::Blocks(blocks));
        }
        _ => {}
    }
    Ok(())
}

fn compare_keys(left: ReadingKey, right: ReadingKey) -> std::cmp::Ordering {
    compare_optional(left.page, right.page).then_with(|| match (left.bounds, right.bounds) {
        (Some(left), Some(right)) => left
            .y
            .total_cmp(&right.y)
            .then_with(|| left.x.total_cmp(&right.x))
            .then_with(|| left.height.total_cmp(&right.height))
            .then_with(|| left.width.total_cmp(&right.width)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    })
}

fn compare_optional<T: Ord>(left: Option<T>, right: Option<T>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}
