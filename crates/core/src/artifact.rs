//! Caller-owned artifact stream capabilities and semantic document events.

use crate::{BlockNode, DocumentMetadata};

/// Representations accepted by one artifact sink for an entire execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactSinkCapabilities {
    /// Receive rendered Markdown chunks.
    pub markdown: bool,
    /// Receive semantic document events followed by document finalization.
    pub semantic_events: bool,
    /// Receive asset metadata and payload chunks.
    pub assets: bool,
}

impl Default for ArtifactSinkCapabilities {
    fn default() -> Self {
        Self { markdown: true, semantic_events: false, assets: true }
    }
}

impl ArtifactSinkCapabilities {
    /// Whether this sink consumes at least one conversion representation.
    #[must_use]
    pub const fn has_output(self) -> bool {
        self.markdown || self.semantic_events || self.assets
    }
}

/// Borrowed, ordered semantic document event.
#[derive(Debug, Clone, Copy)]
pub enum DocumentStreamEvent<'a> {
    /// Document metadata, emitted exactly once and before body blocks.
    Metadata(&'a DocumentMetadata),
    /// One top-level body block in source reading order.
    RootBlock(&'a BlockNode),
}
