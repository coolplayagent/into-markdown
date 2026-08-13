use super::{MergeConfig, ocr};
use into_markdown_core::{Block, ConversionError, Inline};

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
    minimum: usize,
) -> Result<bool, ConversionError> {
    let mut block_stack = Vec::new();
    block_stack.try_reserve_exact(1).map_err(|_| super::memory())?;
    block_stack.push(blocks);
    let mut inline_stack = Vec::new();
    let mut printable = 0_usize;
    while let Some(values) = block_stack.pop() {
        for node in values {
            match &node.block {
                Block::Paragraph(values)
                | Block::Heading { content: values, .. }
                | Block::TimedSegment { content: values, .. } => {
                    inline_stack.try_reserve(1).map_err(|_| super::memory())?;
                    inline_stack.push(values.as_slice());
                }
                Block::List { items, .. } => {
                    for item in items {
                        block_stack.try_reserve(1).map_err(|_| super::memory())?;
                        block_stack.push(item.blocks.as_slice());
                    }
                }
                Block::Table { rows, .. } => {
                    for cell in rows.iter().flat_map(|row| &row.cells) {
                        block_stack.try_reserve(1).map_err(|_| super::memory())?;
                        block_stack.push(cell.blocks.as_slice());
                    }
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => {
                    block_stack.try_reserve(1).map_err(|_| super::memory())?;
                    block_stack.push(blocks.as_slice());
                }
                _ => {}
            }
        }
    }
    while let Some(values) = inline_stack.pop() {
        for inline in values {
            match inline {
                Inline::Text { value, .. }
                | Inline::SourceText { value, .. }
                | Inline::OcrText { value, .. }
                | Inline::Code(value)
                | Inline::Formula(value)
                | Inline::FootnoteReference(value) => {
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
                    inline_stack.push(content.as_slice());
                }
                _ => {}
            }
        }
    }
    Ok(false)
}
