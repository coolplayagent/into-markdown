//! Completion-summary ownership and accounting.

use crate::spi::OutputMemoryLease;
use crate::{ConversionResult, ConversionSummary};

impl Clone for ConversionSummary {
    fn clone(&self) -> Self {
        Self {
            format: self.format,
            outcome: self.outcome,
            diagnostics: self.diagnostics.clone(),
            markdown_bytes: self.markdown_bytes,
            assets: self.assets,
            content: self.content,
            payload_only_assets: self.payload_only_assets,
            external_only_assets: self.external_only_assets,
            dual_representation_assets: self.dual_representation_assets,
            _memory_lease: OutputMemoryLease::default(),
        }
    }
}

impl PartialEq for ConversionSummary {
    fn eq(&self, other: &Self) -> bool {
        self.format == other.format
            && self.outcome == other.outcome
            && self.diagnostics == other.diagnostics
            && self.markdown_bytes == other.markdown_bytes
            && self.assets == other.assets
            && self.content == other.content
            && self.payload_only_assets == other.payload_only_assets
            && self.external_only_assets == other.external_only_assets
            && self.dual_representation_assets == other.dual_representation_assets
    }
}

impl ConversionResult {
    /// Consume a completed result into bounded streaming completion metadata.
    /// Diagnostic ownership and the authenticated result lease move without a
    /// second allocation; the lease may conservatively retain the former
    /// result charge until the summary is dropped.
    #[doc(hidden)]
    #[must_use]
    pub fn into_summary(self) -> ConversionSummary {
        let outcome = self.outcome();
        let content = self.content().ok();
        let payload_only_assets = self
            .assets
            .iter()
            .filter(|asset| !asset.bytes.is_empty() && asset.external_uri.is_none())
            .count();
        let external_only_assets = self
            .assets
            .iter()
            .filter(|asset| asset.bytes.is_empty() && asset.external_uri.is_some())
            .count();
        let dual_representation_assets = self
            .assets
            .iter()
            .filter(|asset| !asset.bytes.is_empty() && asset.external_uri.is_some())
            .count();
        ConversionSummary {
            format: self.detected_format,
            outcome,
            diagnostics: self.diagnostics,
            markdown_bytes: u64::try_from(self.markdown.len()).unwrap_or(u64::MAX),
            assets: u64::try_from(self.assets.len()).unwrap_or(u64::MAX),
            content,
            payload_only_assets: u64::try_from(payload_only_assets).unwrap_or(u64::MAX),
            external_only_assets: u64::try_from(external_only_assets).unwrap_or(u64::MAX),
            dual_representation_assets: u64::try_from(dual_representation_assets)
                .unwrap_or(u64::MAX),
            _memory_lease: self.memory_lease,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostic, DiagnosticSeverity, Document};

    #[test]
    fn moves_diagnostics_without_cloning_the_inventory() {
        let mut diagnostics = Vec::with_capacity(128);
        diagnostics.push(Diagnostic {
            code: "moved".into(),
            severity: DiagnosticSeverity::Warning,
            message: "owned".into(),
            locator: None,
        });
        let pointer = diagnostics.as_ptr();
        let capacity = diagnostics.capacity();
        let result = ConversionResult::new(
            Document::default(),
            "markdown".into(),
            Vec::new(),
            diagnostics,
            Vec::new(),
        );
        let summary = result.into_summary();
        assert_eq!(summary.diagnostics.as_ptr(), pointer);
        assert_eq!(summary.diagnostics.capacity(), capacity);
    }
}
