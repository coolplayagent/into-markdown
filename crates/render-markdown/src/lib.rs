//! Central GitHub-Flavored Markdown renderer boundary.
//!
//! The production serializer is intentionally not implemented in the
//! scaffold. The placeholder accepts an empty document so registry plumbing
//! can be exercised and returns a typed error for material content.

use into_markdown_core::{
    Asset, BoxFuture, ConversionError, ConversionOptions, Document, MarkdownRenderer,
};

/// Scaffold implementation occupying the single GFM renderer slot.
#[derive(Debug, Default)]
pub struct GfmRenderer;

impl MarkdownRenderer for GfmRenderer {
    fn id(&self) -> &'static str {
        "builtin.gfm"
    }

    fn render<'a>(
        &'a self,
        document: &'a Document,
        _: &'a [Asset],
        _: &'a ConversionOptions,
    ) -> BoxFuture<'a, Result<String, ConversionError>> {
        Box::pin(async move {
            if document.blocks.is_empty() {
                Ok(String::new())
            } else {
                Err(ConversionError::ComponentUnavailable {
                    component: "builtin.gfm".into(),
                    detail: "the Markdown serializer is not implemented".into(),
                })
            }
        })
    }
}
