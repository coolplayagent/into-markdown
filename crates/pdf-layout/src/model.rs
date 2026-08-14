use into_markdown_core::{BlockNode, Inline, Provenance, Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Native,
    Ocr,
}

pub(crate) struct Atom {
    pub(crate) inline: Inline,
    pub(crate) bounds: Rect,
    pub(crate) font_size: Option<f32>,
    pub(crate) orientation: u16,
    pub(crate) source_index: usize,
    pub(crate) source_kind: SourceKind,
}

pub(crate) struct Line {
    pub(crate) atoms: Vec<Atom>,
    pub(crate) bounds: Rect,
    pub(crate) font_size: Option<f32>,
    pub(crate) orientation: u16,
    pub(crate) source_index: usize,
    pub(crate) source_kind: SourceKind,
}

pub(crate) struct PageContent {
    pub(crate) atoms: Vec<Atom>,
    pub(crate) passthrough: Vec<BlockNode>,
}

pub(crate) struct RebuiltBlock {
    pub(crate) node: BlockNode,
    pub(crate) bounds: Option<Rect>,
    pub(crate) orientation: u16,
    pub(crate) source_index: usize,
}

pub(crate) fn block_provenance(
    page: u32,
    bounds: Rect,
    width: f32,
    height: f32,
    confidence: Option<f32>,
) -> Provenance {
    Provenance {
        kind: into_markdown_core::ProvenanceKind::Postprocessor,
        provider: crate::LAYOUT_PROVIDER.into(),
        locator: into_markdown_core::SourceLocator {
            page: Some(page),
            bounds: Some(bounds),
            page_width: Some(width),
            page_height: Some(height),
            ..into_markdown_core::SourceLocator::default()
        },
        confidence,
    }
}
