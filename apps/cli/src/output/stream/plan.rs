//! Immutable representation selection for one CLI artifact.

use crate::args::{AssetModeArg, EmitKind};
use into_markdown::ArtifactSinkCapabilities;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkdownRepresentation {
    None,
    Raw,
    Escaped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SemanticRepresentation {
    None,
    Ir,
    IrWithInventories,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RepresentationPlan {
    pub(super) emit: EmitKind,
    markdown: MarkdownRepresentation,
    semantic: SemanticRepresentation,
    pub(super) assets: bool,
}

impl RepresentationPlan {
    pub(super) const fn new(emit: EmitKind, asset_mode: AssetModeArg) -> Self {
        let assets = matches!(emit, EmitKind::ResultJson | EmitKind::Bundle)
            || matches!(asset_mode, AssetModeArg::Extract);
        match emit {
            EmitKind::Markdown => Self {
                emit,
                markdown: MarkdownRepresentation::Raw,
                semantic: SemanticRepresentation::None,
                assets,
            },
            EmitKind::IrJson => Self {
                emit,
                markdown: MarkdownRepresentation::None,
                semantic: SemanticRepresentation::Ir,
                assets,
            },
            EmitKind::ResultJson => Self {
                emit,
                markdown: MarkdownRepresentation::Escaped,
                semantic: SemanticRepresentation::IrWithInventories,
                assets,
            },
            EmitKind::Bundle => Self {
                emit,
                markdown: MarkdownRepresentation::Raw,
                semantic: SemanticRepresentation::IrWithInventories,
                assets,
            },
        }
    }

    pub(super) const fn raw_markdown(self) -> bool {
        matches!(self.markdown, MarkdownRepresentation::Raw)
    }

    pub(super) const fn escaped_markdown(self) -> bool {
        matches!(self.markdown, MarkdownRepresentation::Escaped)
    }

    pub(super) const fn semantic_ir(self) -> bool {
        !matches!(self.semantic, SemanticRepresentation::None)
    }

    pub(super) const fn inventories(self) -> bool {
        matches!(self.semantic, SemanticRepresentation::IrWithInventories)
    }

    pub(super) const fn capabilities(self) -> ArtifactSinkCapabilities {
        ArtifactSinkCapabilities {
            markdown: !matches!(self.markdown, MarkdownRepresentation::None),
            semantic_events: self.semantic_ir(),
            assets: self.assets,
        }
    }
}
