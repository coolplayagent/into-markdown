//! Preserve retained identities when another layout pass produces new blocks.

use crate::{budget::LayoutBudget, model::RebuiltBlock};
use into_markdown_core::{Block, BlockNode, ConversionError};
use std::collections::BTreeSet;

pub(crate) fn avoid_retained_collisions(
    rebuilt: &mut [RebuiltBlock],
    retained: &[BlockNode],
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    if retained.is_empty() {
        return Ok(());
    }
    let mut occupied = BTreeSet::new();
    for node in retained {
        collect(node, &mut occupied, budget)?;
    }
    for block in rebuilt {
        assign(&mut block.node, &mut occupied, budget)?;
    }
    Ok(())
}

fn collect(
    node: &BlockNode,
    occupied: &mut BTreeSet<String>,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    budget.checkpoint_item()?;
    occupied.insert(node.id.0.clone());
    match &node.block {
        Block::List { items, .. } => {
            for child in items.iter().flat_map(|item| &item.blocks) {
                collect(child, occupied, budget)?;
            }
        }
        Block::Table { rows, .. } => {
            for child in rows.iter().flat_map(|row| &row.cells).flat_map(|cell| &cell.blocks) {
                collect(child, occupied, budget)?;
            }
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => {
            for child in blocks {
                collect(child, occupied, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn assign(
    node: &mut BlockNode,
    occupied: &mut BTreeSet<String>,
    budget: &mut LayoutBudget<'_>,
) -> Result<(), ConversionError> {
    budget.checkpoint_item()?;
    if !occupied.insert(node.id.0.clone()) {
        let mut suffix = 1_usize;
        loop {
            budget.checkpoint_item()?;
            let candidate = format!("{}-reflow-{suffix}", node.id.0);
            if occupied.insert(candidate.clone()) {
                node.id.0 = candidate;
                break;
            }
            suffix += 1;
        }
    }
    match &mut node.block {
        Block::List { items, .. } => {
            for child in items.iter_mut().flat_map(|item| &mut item.blocks) {
                assign(child, occupied, budget)?;
            }
        }
        Block::Table { rows, .. } => {
            for child in
                rows.iter_mut().flat_map(|row| &mut row.cells).flat_map(|cell| &mut cell.blocks)
            {
                assign(child, occupied, budget)?;
            }
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => {
            for child in blocks {
                assign(child, occupied, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}
