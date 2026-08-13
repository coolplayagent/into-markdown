use into_markdown_core::{Inline, Provenance};

/// Whether text is already proven to belong to the requested page or still
/// needs an inline-level source locator. An inline's explicit page always
/// overrides its containing node; an absent inline page inherits the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageScope {
    Container,
    Node,
    InlineFallback,
    Excluded,
}

impl PageScope {
    pub(crate) fn root(explicitly_scoped: bool) -> Self {
        if explicitly_scoped { Self::Container } else { Self::InlineFallback }
    }

    pub(crate) fn for_node(self, provenance: &Provenance, page: u32) -> Self {
        if matches!(self, Self::Container | Self::Excluded) {
            return self;
        }
        match provenance.locator.page {
            Some(value) if value == page => Self::Node,
            Some(_) => Self::Excluded,
            None => self,
        }
    }

    pub(crate) fn includes_plain_text(self) -> bool {
        matches!(self, Self::Container | Self::Node)
    }

    pub(crate) fn includes_sourced_inline(self, inline: &Inline, page: u32) -> bool {
        match inline {
            Inline::SourceText { provenance, .. } | Inline::OcrText { provenance, .. } => {
                provenance
                    .locator
                    .page
                    .map_or_else(|| self.includes_plain_text(), |value| value == page)
            }
            _ => false,
        }
    }
}
