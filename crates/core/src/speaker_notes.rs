//! Presentation notes identity, carried by node IDs through ZIP and OCR remapping.
//!
//! Suffixes identify generated headings and their consecutive body blocks while
//! preserving the existing IR wire shape and original source locators.

use crate::{AssetMode, Block, BlockNode, ConversionError, Inline};

const HEADING: &str = "::speaker-notes-heading";
const BODY: &str = "::speaker-note";

/// Mark a generated notes heading while preserving its unique node identity.
///
/// # Errors
/// Returns a resource limit error if the identity allocation fails.
pub fn mark_heading(node: &mut BlockNode) -> Result<(), ConversionError> {
    mark(node, HEADING)
}

/// Mark one top-level notes body block, including its later OCR contributions.
///
/// # Errors
/// Returns a resource limit error if the identity allocation fails.
pub fn mark_body(node: &mut BlockNode) -> Result<(), ConversionError> {
    mark(node, BODY)
}

fn mark(node: &mut BlockNode, suffix: &str) -> Result<(), ConversionError> {
    node.id.0.try_reserve(suffix.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "cannot reserve speaker notes identity".into(),
    })?;
    node.id.0.push_str(suffix);
    Ok(())
}

/// Whether a node is an explicitly generated notes heading.
#[must_use]
pub fn is_heading(node: &BlockNode) -> bool {
    node.id.0.ends_with(HEADING) && matches!(node.block, Block::Heading { level: 3, .. })
}

/// Whether a node belongs to the body of the preceding notes heading.
#[must_use]
pub fn is_body(node: &BlockNode) -> bool {
    node.id.0.split("::ocr::").next().is_some_and(|id| id.ends_with(BODY))
}

/// Test visible content after optional enrichment and the chosen asset policy.
/// Callers validate the ordinary IR depth and node budgets before traversing it.
#[must_use]
pub fn has_visible_content(nodes: &[BlockNode], asset_mode: AssetMode) -> bool {
    nodes.iter().any(|node| match &node.block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => visible_inlines(inlines),
        Block::Image { alt, .. } => {
            asset_mode != AssetMode::Omit
                || alt.as_ref().is_some_and(|value| !value.trim().is_empty())
        }
        Block::List { items, .. } => items
            .iter()
            .any(|item| item.checked.is_some() || has_visible_content(&item.blocks, asset_mode)),
        Block::Table { rows, .. } => rows
            .iter()
            .any(|row| row.cells.iter().any(|cell| has_visible_content(&cell.blocks, asset_mode))),
        Block::Code { text, .. } | Block::Formula(text) => !text.trim().is_empty(),
        Block::Footnote { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Sheet { blocks, .. } => has_visible_content(blocks, asset_mode),
        Block::Rule => true,
    })
}

fn visible_inlines(inlines: &[Inline]) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::Text { value, .. }
        | Inline::SourceText { value, .. }
        | Inline::OcrText { value, .. }
        | Inline::Code(value)
        | Inline::Formula(value)
        | Inline::FootnoteReference(value) => !value.trim().is_empty(),
        Inline::Link { content, .. } => visible_inlines(content),
        Inline::LineBreak => false,
    })
}
