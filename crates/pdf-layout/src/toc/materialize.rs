use super::Row;
use crate::budget::LayoutBudget;
use crate::geometry::union;
use crate::model::{RebuiltBlock, block_provenance};
use crate::{memory, semantics};
use into_markdown_core::{
    Block, BlockNode, ConversionError, Inline, ListItem, ListKind, NodeId, Rect,
};
use std::collections::VecDeque;

pub(super) fn column(
    rows: Vec<Row>,
    page: u32,
    dimensions: (f32, f32),
    sequence: usize,
    budget: &mut LayoutBudget<'_>,
) -> Result<RebuiltBlock, ConversionError> {
    let first = rows.first().ok_or_else(|| memory("empty directory"))?;
    let source_index = first.line.source_index;
    let orientation = first.line.orientation;
    let bounds = rows
        .iter()
        .map(|row| row.line.bounds)
        .reduce(union)
        .ok_or_else(|| memory("directory bounds"))?;
    let mut pending = VecDeque::from(rows);
    let mut serial = sequence * 1_000_000;
    let node = list(&mut pending, page, dimensions, &mut serial, 0, budget)?;
    Ok(RebuiltBlock { node, bounds: Some(bounds), orientation, source_index })
}

fn list(
    pending: &mut VecDeque<Row>,
    page: u32,
    dimensions: (f32, f32),
    serial: &mut usize,
    depth: usize,
    budget: &mut LayoutBudget<'_>,
) -> Result<BlockNode, ConversionError> {
    let first = pending.front().ok_or_else(|| memory("directory list"))?;
    let indent = first.hint.indent;
    let font = first.hint.font;
    let mut parent_indent = indent;
    let mut parent_depth = first.hint.depth;
    let mut bounds = first.line.bounds;
    let mut items: Vec<ListItem> = Vec::new();
    while let Some(next) = pending.front() {
        budget.checkpoint_item()?;
        if depth > 0 && next.hint.indent < indent - font * 0.6 {
            break;
        }
        let nested = next.hint.indent > parent_indent + font * 0.6
            && parent_depth.zip(next.hint.depth).is_none_or(|(a, b)| b > a);
        if nested && !items.is_empty() && depth < 24 {
            let child = list(pending, page, dimensions, serial, depth + 1, budget)?;
            if let Some(r) = child.provenance.locator.bounds {
                bounds = union(bounds, r);
            }
            budget.push(
                &mut items.last_mut().ok_or_else(|| memory("directory parent"))?.blocks,
                child,
            )?;
            continue;
        }
        let row = pending.pop_front().ok_or_else(|| memory("directory row"))?;
        parent_indent = row.hint.indent;
        parent_depth = row.hint.depth;
        let row_bounds = row.line.bounds;
        bounds = union(bounds, row_bounds);
        let content = row_inlines(row, budget)?;
        let node = node(page, dimensions, serial, row_bounds, Block::Paragraph(content), budget)?;
        let mut blocks = budget.reserve(1)?;
        blocks.push(node);
        budget.push(&mut items, ListItem { checked: None, marker_label: None, blocks })?;
    }
    node(
        page,
        dimensions,
        serial,
        bounds,
        Block::List { kind: ListKind::Bullet, start: 1, items },
        budget,
    )
}

fn row_inlines(
    mut row: Row,
    budget: &mut LayoutBudget<'_>,
) -> Result<Vec<Inline>, ConversionError> {
    let mut values = Vec::new();
    for line in row.prefix {
        for inline in semantics::take_line_inlines(line)? {
            budget.push(&mut values, inline)?;
        }
        budget.push(&mut values, Inline::Text { value: " ".into(), marks: Vec::new() })?;
    }
    // Preserve the printed page label even when a font's word-space geometry
    // is empty or the title ends in a CJK character.
    let mut label = budget.reserve(row.line.atoms.len() - row.hint.tail)?;
    label.extend(row.line.atoms.drain(row.hint.tail..));
    let label_line = crate::model::Line {
        bounds: row.line.bounds,
        font_size: row.line.font_size,
        orientation: row.line.orientation,
        source_index: row.line.source_index,
        source_kind: row.line.source_kind,
        atoms: label,
    };
    for inline in semantics::take_line_inlines(row.line)? {
        budget.push(&mut values, inline)?;
    }
    budget.push(&mut values, Inline::Text { value: " ".into(), marks: Vec::new() })?;
    for inline in semantics::take_line_inlines(label_line)? {
        budget.push(&mut values, inline)?;
    }
    Ok(values)
}

fn node(
    page: u32,
    dimensions: (f32, f32),
    serial: &mut usize,
    bounds: Rect,
    block: Block,
    budget: &mut LayoutBudget<'_>,
) -> Result<BlockNode, ConversionError> {
    budget.generated_node()?;
    let id = NodeId(format!("pdf-page-{page}-toc-{serial}"));
    *serial += 1;
    let mut provenance = block_provenance(page, bounds, dimensions.0, dimensions.1, None);
    provenance.locator.part = Some("pdf/toc".into());
    Ok(BlockNode { id, block, provenance })
}
