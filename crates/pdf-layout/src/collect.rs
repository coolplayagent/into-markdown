use crate::budget::LayoutBudget;
use crate::model::{Atom, PageContent, SourceKind};
use crate::{malformed, memory};
use into_markdown_core::{Block, BlockNode, ConversionError, Inline, OcrEvidence, Rect};

pub(crate) fn page_content(
    page: u32,
    width: f32,
    height: f32,
    nodes: Vec<BlockNode>,
    budget: &mut LayoutBudget<'_>,
) -> Result<PageContent, ConversionError> {
    let mut content = PageContent { atoms: Vec::new(), passthrough: Vec::new() };
    content
        .atoms
        .try_reserve_exact(count_source_inlines(&nodes))
        .map_err(|_| memory("layout atom allocation"))?;
    content
        .passthrough
        .try_reserve_exact(nodes.len())
        .map_err(|_| memory("layout passthrough allocation"))?;
    let mut source_index = 0_usize;
    for node in nodes {
        budget.checkpoint_item()?;
        if block_is_collectable(&node, page, width, height)? {
            collect_node(node, page, width, height, &mut source_index, &mut content.atoms, budget)?;
        } else {
            content.passthrough.push(node);
        }
    }
    Ok(content)
}

fn block_is_collectable(
    node: &BlockNode,
    page: u32,
    width: f32,
    height: f32,
) -> Result<bool, ConversionError> {
    if matches!(node.block, Block::Footnote { .. }) {
        return Ok(false);
    }
    let mut saw_source = false;
    let mut valid = true;
    let contains_footnote = inspect_block(&node.block, &mut |inline| {
        let (provenance, native_separator, ocr_evidence) = match inline {
            Inline::SourceText { value, provenance, .. } => {
                saw_source = true;
                (provenance, is_native_separator(value), false)
            }
            Inline::OcrText { provenance, .. } => {
                saw_source = true;
                (provenance, false, true)
            }
            Inline::Link { .. } | Inline::FootnoteReference(_) => {
                valid = false;
                return Ok(());
            }
            _ => return Ok(()),
        };
        let locator = &provenance.locator;
        if locator.page.is_some_and(|actual| actual != page) {
            return Err(malformed("pdfLayoutInlinePageMismatch"));
        }
        let Some(bounds) = locator.bounds else {
            valid = false;
            return Ok(());
        };
        if !usable_rect(bounds, width, height)? {
            if native_separator {
                return Ok(());
            }
            if ocr_evidence {
                return Err(malformed("pdfLayoutOcrEvidenceMissingGeometry"));
            }
            valid = false;
        }
        Ok(())
    })?;
    Ok(saw_source && valid && !contains_footnote)
}

fn collect_node(
    node: BlockNode,
    page: u32,
    width: f32,
    height: f32,
    source_index: &mut usize,
    atoms: &mut Vec<Atom>,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    match node.block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => {
            collect_inlines(inlines, page, width, height, source_index, atoms, budget)?;
        }
        Block::List { items, .. } => {
            for item in items {
                for child in item.blocks {
                    collect_node(child, page, width, height, source_index, atoms, budget)?;
                }
            }
        }
        Block::Table { rows, .. } => {
            for cell in rows.into_iter().flat_map(|row| row.cells) {
                for child in cell.blocks {
                    collect_node(child, page, width, height, source_index, atoms, budget)?;
                }
            }
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => {
            for child in blocks {
                collect_node(child, page, width, height, source_index, atoms, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_inlines(
    inlines: Vec<Inline>,
    page: u32,
    width: f32,
    height: f32,
    source_index: &mut usize,
    atoms: &mut Vec<Atom>,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    for inline in inlines {
        budget.checkpoint_item()?;
        match inline {
            Inline::SourceText { value, marks, provenance } => {
                let bounds =
                    provenance.locator.bounds.ok_or_else(|| malformed("pdfLayoutMissingBounds"))?;
                if !usable_rect(bounds, width, height)? {
                    if is_native_separator(&value) {
                        continue;
                    }
                    return Err(malformed("pdfLayoutVisibleTextMissingGeometry"));
                }
                budget.consume_atom(value.len())?;
                let font_size = valid_font_size(provenance.locator.font_size);
                let orientation = orientation(provenance.locator.rotation_degrees, None);
                atoms.push(Atom {
                    inline: Inline::SourceText { value, marks, provenance },
                    bounds,
                    font_size,
                    orientation,
                    source_index: *source_index,
                    source_kind: SourceKind::Native,
                });
                *source_index =
                    source_index.checked_add(1).ok_or_else(|| memory("source index"))?;
            }
            Inline::OcrText { value, marks, provenance, evidence } => {
                if evidence.page != page {
                    return Err(malformed("pdfLayoutOcrEvidencePageMismatch"));
                }
                let bounds =
                    provenance.locator.bounds.ok_or_else(|| malformed("pdfLayoutMissingBounds"))?;
                if !usable_rect(bounds, width, height)? {
                    return Err(malformed("pdfLayoutOcrEvidenceMissingGeometry"));
                }
                budget.consume_atom(value.len())?;
                let angle = evidence_angle(&evidence);
                let font_size = valid_font_size(provenance.locator.font_size);
                let orientation = orientation(provenance.locator.rotation_degrees, angle);
                atoms.push(Atom {
                    inline: Inline::OcrText { value, marks, provenance, evidence },
                    bounds,
                    font_size,
                    orientation,
                    source_index: *source_index,
                    source_kind: SourceKind::Ocr,
                });
                *source_index =
                    source_index.checked_add(1).ok_or_else(|| memory("source index"))?;
            }
            Inline::Link { content, .. } => {
                collect_inlines(content, page, width, height, source_index, atoms, budget)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn inspect_block(
    block: &Block,
    visit: &mut impl FnMut(&Inline) -> Result<(), ConversionError>,
) -> Result<bool, ConversionError> {
    let mut block_stack = Vec::new();
    let mut inline_stack = Vec::new();
    let mut contains_footnote = false;
    block_stack.try_reserve_exact(1).map_err(|_| memory("layout inspect stack"))?;
    block_stack.push(block);
    while let Some(block) = block_stack.pop() {
        match block {
            Block::Paragraph(inlines)
            | Block::Heading { content: inlines, .. }
            | Block::TimedSegment { content: inlines, .. } => inline_stack.push(inlines.as_slice()),
            Block::List { items, .. } => block_stack
                .extend(items.iter().flat_map(|item| &item.blocks).map(|node| &node.block)),
            Block::Table { rows, .. } => block_stack.extend(
                rows.iter()
                    .flat_map(|row| &row.cells)
                    .flat_map(|cell| &cell.blocks)
                    .map(|node| &node.block),
            ),
            Block::Footnote { blocks, .. } => {
                contains_footnote = true;
                block_stack.extend(blocks.iter().map(|node| &node.block));
            }
            Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                block_stack.extend(blocks.iter().map(|node| &node.block));
            }
            _ => {}
        }
    }
    while let Some(inlines) = inline_stack.pop() {
        for inline in inlines {
            visit(inline)?;
            if let Inline::Link { content, .. } = inline {
                inline_stack.push(content);
            }
        }
    }
    Ok(contains_footnote)
}

fn count_source_inlines(nodes: &[BlockNode]) -> usize {
    fn count(block: &Block) -> usize {
        match block {
            Block::Paragraph(inlines)
            | Block::Heading { content: inlines, .. }
            | Block::TimedSegment { content: inlines, .. } => {
                inlines.iter().map(count_inline).sum()
            }
            Block::List { items, .. } => {
                items.iter().flat_map(|item| &item.blocks).map(|node| count(&node.block)).sum()
            }
            Block::Table { rows, .. } => rows
                .iter()
                .flat_map(|row| &row.cells)
                .flat_map(|cell| &cell.blocks)
                .map(|node| count(&node.block))
                .sum(),
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => blocks.iter().map(|node| count(&node.block)).sum(),
            _ => 0,
        }
    }
    fn count_inline(inline: &Inline) -> usize {
        match inline {
            Inline::SourceText { .. } | Inline::OcrText { .. } => 1,
            Inline::Link { content, .. } => content.iter().map(count_inline).sum(),
            _ => 0,
        }
    }
    nodes.iter().map(|node| count(&node.block)).sum()
}

fn usable_rect(rect: Rect, width: f32, height: f32) -> Result<bool, ConversionError> {
    let right = rect.x + rect.width;
    let bottom = rect.y + rect.height;
    if [rect.x, rect.y, rect.width, rect.height, right, bottom]
        .iter()
        .any(|value| !value.is_finite())
        || rect.width < 0.0
        || rect.height < 0.0
        || rect.x < -0.5
        || rect.y < -0.5
        || right > width + 0.5
        || bottom > height + 0.5
    {
        return Err(malformed("pdfLayoutBoundsOutsidePage"));
    }
    Ok(rect.width > 0.0 && rect.height > 0.0)
}

fn is_native_separator(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| character.is_whitespace() || character.is_control())
}

fn valid_font_size(value: Option<f32>) -> Option<f32> {
    value.filter(|value| value.is_finite() && *value > 0.0)
}

fn evidence_angle(evidence: &OcrEvidence) -> Option<f32> {
    let region = evidence.regions.first()?;
    let left = region.polygon[0];
    let right = region.polygon[1];
    Some((right.y - left.y).atan2(right.x - left.x).to_degrees())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn orientation(explicit: Option<f32>, evidence: Option<f32>) -> u16 {
    let angle = explicit.or(evidence).unwrap_or(0.0);
    if !angle.is_finite() {
        return 0;
    }
    let normalized = angle.rem_euclid(360.0);
    (((normalized / 90.0).round() as u16) % 4) * 90
}
