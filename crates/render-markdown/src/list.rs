//! List boundaries and marker widths follow `CommonMark` limits.
use super::{
    Block, ConversionError, InlineContext, ListItem, ListKind, RenderContext, indent_all,
    indent_continuation, render_error,
};

impl RenderContext<'_> {
    pub(super) fn render_list(
        &self,
        kind: ListKind,
        start: u64,
        items: &[ListItem],
        context: InlineContext,
    ) -> Result<String, ConversionError> {
        if items.is_empty() {
            return Ok(String::new());
        }
        let html = kind == ListKind::Ordered
            && start.saturating_add(items.len().saturating_sub(1) as u64) > 999_999_999;
        let mut lines = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let marker = match kind {
                ListKind::Bullet => "-".to_owned(),
                ListKind::Task => {
                    if item.checked == Some(true) {
                        "- [x]".to_owned()
                    } else {
                        "- [ ]".to_owned()
                    }
                }
                ListKind::Ordered => {
                    let number = start
                        .checked_add(index as u64)
                        .ok_or_else(|| render_error("ordered-list marker overflowed u64"))?;
                    format!("{number}.")
                }
            };
            let body = self.render_blocks_in(&item.blocks, context)?;
            if html {
                lines.push(format!("<li>\n\n{body}\n\n</li>"));
                continue;
            }
            let indentation = if kind == ListKind::Ordered { (marker.len() + 1).max(4) } else { 4 };
            let paragraph_first =
                item.blocks.first().is_some_and(|node| matches!(node.block, Block::Paragraph(_)));
            if body.is_empty() {
                lines.push(marker);
            } else if paragraph_first {
                lines.push(format!("{marker} {}", indent_continuation(&body, indentation)));
            } else {
                lines.push(format!("{marker}\n{}", indent_all(&body, indentation)));
            }
        }
        let body = lines.join("\n");
        Ok(if html { format!("<ol start=\"{start}\">\n\n{body}\n\n</ol>") } else { body })
    }
}
