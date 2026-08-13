use super::page_scope::PageScope;
use super::{MergeConfig, ocr};
use into_markdown_core::{Block, ConversionError, ExecutionContext, Inline};

pub(crate) fn validate(config: &MergeConfig) -> Result<(), ConversionError> {
    if !config.minimum_confidence.is_finite()
        || !(0.0..=1.0).contains(&config.minimum_confidence)
        || config.auto_min_native_characters == 0
        || config.limits.max_pages == 0
        || config.limits.max_regions == 0
        || config.limits.max_text_bytes == 0
        || config.limits.max_identity_bytes == 0
        || config.limits.max_comparisons == 0
    {
        return Err(ocr("invalidMergeConfig"));
    }
    Ok(())
}

pub(crate) fn has_sufficient_native_text(
    blocks: &[into_markdown_core::BlockNode],
    page: u32,
    explicitly_scoped: bool,
    minimum: usize,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let mut block_stack = Vec::new();
    block_stack.try_reserve_exact(1).map_err(|_| super::memory())?;
    block_stack.push((blocks, PageScope::root(explicitly_scoped)));
    let mut inline_stack = Vec::new();
    let mut printable = 0_usize;
    let mut visited = 0_usize;
    while let Some((values, parent_scope)) = block_stack.pop() {
        for node in values {
            visited += 1;
            super::traversal_checkpoint(context, visited)?;
            let scope = parent_scope.for_node(&node.provenance, page);
            if scope == PageScope::Excluded {
                continue;
            }
            match &node.block {
                Block::Paragraph(values)
                | Block::Heading { content: values, .. }
                | Block::TimedSegment { content: values, .. } => {
                    inline_stack.try_reserve(1).map_err(|_| super::memory())?;
                    inline_stack.push((values.as_slice(), scope));
                }
                Block::List { items, .. } => {
                    for item in items {
                        block_stack.try_reserve(1).map_err(|_| super::memory())?;
                        block_stack.push((item.blocks.as_slice(), scope));
                    }
                }
                Block::Table { rows, .. } => {
                    for cell in rows.iter().flat_map(|row| &row.cells) {
                        block_stack.try_reserve(1).map_err(|_| super::memory())?;
                        block_stack.push((cell.blocks.as_slice(), scope));
                    }
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => {
                    block_stack.try_reserve(1).map_err(|_| super::memory())?;
                    block_stack.push((blocks.as_slice(), scope));
                }
                _ => {}
            }
        }
    }
    while let Some((values, scope)) = inline_stack.pop() {
        for inline in values {
            visited += 1;
            super::traversal_checkpoint(context, visited)?;
            match inline {
                Inline::SourceText { value, .. } | Inline::OcrText { value, .. }
                    if scope.includes_sourced_inline(inline, page) =>
                {
                    for character in value.chars() {
                        if !character.is_control() && !character.is_whitespace() {
                            printable += 1;
                            if printable >= minimum {
                                return Ok(true);
                            }
                        }
                    }
                }
                Inline::Text { value, .. }
                | Inline::Code(value)
                | Inline::Formula(value)
                | Inline::FootnoteReference(value)
                    if scope.includes_plain_text() =>
                {
                    for character in value.chars() {
                        if !character.is_control() && !character.is_whitespace() {
                            printable += 1;
                            if printable >= minimum {
                                return Ok(true);
                            }
                        }
                    }
                }
                Inline::Link { content, .. } => {
                    inline_stack.try_reserve(1).map_err(|_| super::memory())?;
                    inline_stack.push((content.as_slice(), scope));
                }
                _ => {}
            }
        }
    }
    Ok(false)
}
