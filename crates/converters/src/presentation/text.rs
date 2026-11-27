use super::error::{limit, malformed};
use super::model::{ParseState, Shape, TextParagraph};
use into_markdown_core::{Block, BlockNode, ConversionError, Inline, ListItem, ListKind, Rect};
use std::fmt::Write as _;

pub(super) fn shape_plain_text(shape: &Shape) -> Result<String, ConversionError> {
    let mut title = String::new();
    for paragraph in &shape.paragraphs {
        let value = plain_text(&paragraph.text)?;
        if value.is_empty() {
            continue;
        }
        let separator = usize::from(!title.is_empty());
        let additional = value
            .len()
            .checked_add(separator)
            .ok_or_else(|| limit("max_memory_bytes", "title capacity overflow"))?;
        title.try_reserve(additional).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve slide title: {error}"))
        })?;
        if !title.is_empty() {
            title.push(' ');
        }
        title.push_str(&value);
    }
    Ok(title)
}

pub(super) fn plain_text(inlines: &[Inline]) -> Result<String, ConversionError> {
    let capacity = inlines.iter().try_fold(0_usize, |total, inline| {
        let length = match inline {
            Inline::Text { value, .. } | Inline::Code(value) => value.len(),
            Inline::LineBreak => 1,
            _ => 0,
        };
        total
            .checked_add(length)
            .ok_or_else(|| limit("max_memory_bytes", "plain-text capacity overflow"))
    })?;
    let mut value = String::new();
    value.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve plain text: {error}"))
    })?;
    for inline in inlines {
        match inline {
            Inline::Text { value: text, .. } | Inline::Code(text) => value.push_str(text),
            Inline::LineBreak => value.push(' '),
            _ => {}
        }
    }
    let leading = value.len().saturating_sub(value.trim_start().len());
    if leading != 0 {
        value.drain(..leading);
    }
    value.truncate(value.trim_end().len());
    Ok(value)
}

pub(super) struct ParagraphSource<'a> {
    pub(super) part: &'a str,
    pub(super) slide: u32,
    pub(super) bounds: Option<Rect>,
    pub(super) z_order: usize,
    pub(super) languages: &'a [String],
}

impl ParagraphSource<'_> {
    pub(super) fn append_blocks(
        &self,
        paragraphs: &mut [TextParagraph],
        title: bool,
        output: &mut Vec<BlockNode>,
        state: &mut ParseState,
    ) -> Result<(), ConversionError> {
        let mut index = 0;
        while index < paragraphs.len() {
            if !title && paragraphs[index].bullet.is_some() {
                let level = paragraphs[index].level;
                output.push(self.build_list_level(paragraphs, &mut index, level, state)?);
            } else {
                let text = std::mem::take(&mut paragraphs[index].text);
                state.add_inlines(text.len())?;
                let block = if title {
                    Block::Heading { level: 3, content: text }
                } else {
                    Block::Paragraph(text)
                };
                output.push(self.node(state, block)?);
                index += 1;
            }
        }
        Ok(())
    }

    pub(super) fn node(
        &self,
        state: &mut ParseState,
        block: Block,
    ) -> Result<BlockNode, ConversionError> {
        state.node(
            block,
            self.part,
            self.slide,
            self.bounds,
            Some(self.z_order),
            Some(self.languages),
        )
    }

    fn build_list_level(
        &self,
        paragraphs: &mut [TextParagraph],
        index: &mut usize,
        level: u8,
        state: &mut ParseState,
    ) -> Result<BlockNode, ConversionError> {
        let kind = paragraphs[*index].bullet.ok_or_else(|| {
            malformed(Some(self.part), "list builder started at a plain paragraph")
        })?;
        let start = paragraphs[*index].start;
        let mut items = Vec::<ListItem>::new();
        while *index < paragraphs.len() {
            let paragraph_level = paragraphs[*index].level;
            let Some(paragraph_kind) = paragraphs[*index].bullet else { break };
            if paragraph_level < level || (paragraph_level == level && paragraph_kind != kind) {
                break;
            }
            if paragraph_level > level {
                let last = items
                    .last_mut()
                    .ok_or_else(|| malformed(Some(self.part), "nested list lacks a parent item"))?;
                let nested = self.build_list_level(paragraphs, index, paragraph_level, state)?;
                last.blocks.try_reserve(1).map_err(|error| {
                    limit("max_memory_bytes", format!("cannot reserve nested list: {error}"))
                })?;
                last.blocks.push(nested);
                continue;
            }
            state.add_inlines(paragraphs[*index].text.len())?;
            let text = std::mem::take(&mut paragraphs[*index].text);
            let numbering = paragraphs[*index].numbering.take();
            let paragraph = self.node(state, Block::Paragraph(text))?;
            let mut blocks = Vec::new();
            blocks.try_reserve_exact(2).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve list item blocks: {error}"))
            })?;
            blocks.push(paragraph);
            items.try_reserve(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve list item: {error}"))
            })?;
            items.push(ListItem {
                checked: None,
                marker_label: Some(list_marker(level, kind, numbering.as_deref())?),
                blocks,
            });
            *index += 1;
        }
        self.node(state, Block::List { kind, start, items })
    }
}

fn list_marker(
    level: u8,
    kind: ListKind,
    numbering: Option<&str>,
) -> Result<String, ConversionError> {
    let capacity = 8_usize
        .checked_add(numbering.map_or(0, |value| value.len().saturating_add(8)))
        .ok_or_else(|| limit("max_memory_bytes", "list marker capacity overflow"))?;
    let mut marker = String::new();
    marker.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve list marker: {error}"))
    })?;
    write!(marker, "level:{level}")
        .map_err(|_| malformed(None, "cannot format list level marker"))?;
    if let Some(numbering) = numbering {
        marker.push_str(match kind {
            ListKind::Bullet => ";character:",
            ListKind::Ordered => ";scheme:",
            ListKind::Task => ";marker:",
        });
        marker.push_str(numbering);
    }
    Ok(marker)
}
