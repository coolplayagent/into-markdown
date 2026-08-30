//! Format-independent completion semantics for rendered conversion results.

use crate::{
    Asset, Block, ConversionError, ConversionOutcome, ConversionResult, Diagnostic,
    DiagnosticSeverity, Document, Inline,
};

/// Stable diagnostic emitted when a converter proves that the source has no
/// visible document content.
pub const EMPTY_SOURCE_REASON_CODE: &str = "emptySource";

/// Stable diagnostic emitted for useful asset-backed results whose Markdown
/// representation is empty.
pub const ASSET_ONLY_REASON_CODE: &str = "assetOnly";

/// Converter-owned evidence used by the shared empty-result gate.
///
/// The default is fail-closed. `Empty` is valid only after the converter has
/// scanned every visible-content domain it supports. `AssetsOnly` additionally
/// requires retained IR references and a retained asset inventory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceContentEvidence {
    /// The converter cannot prove why a rendered result is empty.
    #[default]
    Unknown,
    /// The converter proved that the source has no visible document content.
    Empty,
    /// The source contains only useful asset-backed document content.
    AssetsOnly,
}

/// Usable content class of a completed result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultContent {
    /// Markdown contains a non-whitespace scalar.
    Markdown,
    /// The source was explicitly certified as empty.
    EmptySource,
    /// Structured IR and assets remain useful despite empty Markdown.
    AssetsOnly,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DocumentContent {
    #[default]
    Empty,
    AssetsOnly,
    Visible,
}

impl DocumentContent {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Visible, _) | (_, Self::Visible) => Self::Visible,
            (Self::AssetsOnly, _) | (_, Self::AssetsOnly) => Self::AssetsOnly,
            _ => Self::Empty,
        }
    }
}

/// Classify the semantic content retained by a document independently of its
/// format-specific container structure.
#[must_use]
pub(crate) fn document_content(document: &Document) -> DocumentContent {
    document
        .blocks
        .iter()
        .map(|node| block_content(&node.block))
        .fold(DocumentContent::Empty, DocumentContent::merge)
}

/// Whether a converter result has no visible semantic body content.
#[doc(hidden)]
#[must_use]
pub fn document_is_empty(document: &Document) -> bool {
    document_content(document) == DocumentContent::Empty
}

/// Whether every visible semantic node is an asset reference.
#[doc(hidden)]
#[must_use]
pub fn document_is_asset_only(document: &Document) -> bool {
    document_content(document) == DocumentContent::AssetsOnly
}

/// Whether Markdown contains a usable non-whitespace scalar. A leading BOM is
/// transport metadata and is not content by itself.
#[must_use]
pub fn markdown_has_visible_content(markdown: &str) -> bool {
    markdown.chars().any(|value| !value.is_whitespace() && value != '\u{feff}')
}

/// Derive the stable success outcome without treating informational audit
/// diagnostics as content loss.
#[must_use]
pub fn conversion_outcome(diagnostics: &[Diagnostic]) -> ConversionOutcome {
    if diagnostics.iter().any(|value| value.severity != DiagnosticSeverity::Info) {
        ConversionOutcome::Degraded
    } else {
        ConversionOutcome::Complete
    }
}

/// Validate and classify a completed result.
///
/// # Errors
///
/// Returns `emptyContent` for an unusable empty result and `internal` for
/// contradictory engine-reserved evidence.
pub fn classify_result(
    document: &Document,
    markdown: &str,
    assets: &[Asset],
    diagnostics: &[Diagnostic],
) -> Result<ResultContent, ConversionError> {
    let empty_source = has_reason(diagnostics, EMPTY_SOURCE_REASON_CODE);
    let asset_only = has_reason(diagnostics, ASSET_ONLY_REASON_CODE);
    if empty_source && asset_only {
        return Err(ConversionError::Internal {
            detail: "result carries conflicting empty-source and asset-only evidence".into(),
        });
    }
    let content = document_content(document);
    if empty_source {
        if content != DocumentContent::Empty || !assets.is_empty() {
            return Err(ConversionError::Internal {
                detail: "empty-source evidence conflicts with retained document content".into(),
            });
        }
        if markdown_has_visible_content(markdown) {
            return Ok(ResultContent::Markdown);
        }
        return Ok(ResultContent::EmptySource);
    }
    if asset_only {
        if content != DocumentContent::AssetsOnly || assets.is_empty() {
            return Err(ConversionError::Internal {
                detail: "asset-only evidence requires asset-only IR and retained assets".into(),
            });
        }
        if markdown_has_visible_content(markdown) {
            return Ok(ResultContent::Markdown);
        }
        return Ok(ResultContent::AssetsOnly);
    }
    if markdown_has_visible_content(markdown) {
        Ok(ResultContent::Markdown)
    } else {
        Err(ConversionError::EmptyContent)
    }
}

impl ConversionResult {
    /// Validate and return this result's usable content class.
    ///
    /// # Errors
    ///
    /// Returns the shared empty-result failure when the result is unusable.
    pub fn content(&self) -> Result<ResultContent, ConversionError> {
        classify_result(&self.document, &self.markdown, &self.assets, &self.diagnostics)
    }

    /// Stable success outcome derived from diagnostic severity.
    #[must_use]
    pub fn outcome(&self) -> ConversionOutcome {
        conversion_outcome(&self.diagnostics)
    }

    /// Stable success reason for exceptional usable results.
    #[must_use]
    pub fn reason_code(&self) -> Option<&str> {
        if let Some(diagnostic) =
            self.diagnostics.iter().find(|value| value.severity != DiagnosticSeverity::Info)
        {
            Some(diagnostic.code.as_str())
        } else if has_reason(&self.diagnostics, EMPTY_SOURCE_REASON_CODE) {
            Some(EMPTY_SOURCE_REASON_CODE)
        } else if has_reason(&self.diagnostics, ASSET_ONLY_REASON_CODE) {
            Some(ASSET_ONLY_REASON_CODE)
        } else {
            None
        }
    }
}

impl crate::ConversionSummary {
    /// Return the usable content class validated before streaming emission.
    ///
    /// # Errors
    ///
    /// Returns `emptyContent` only for a summary derived from an externally
    /// constructed result that never passed the Engine terminal gate.
    pub fn content(&self) -> Result<ResultContent, ConversionError> {
        self.content.ok_or(ConversionError::EmptyContent)
    }

    /// Stable reason for an exceptional successful completion.
    #[must_use]
    pub fn reason_code(&self) -> Option<&str> {
        if let Some(diagnostic) =
            self.diagnostics.iter().find(|value| value.severity != DiagnosticSeverity::Info)
        {
            Some(diagnostic.code.as_str())
        } else if has_reason(&self.diagnostics, EMPTY_SOURCE_REASON_CODE) {
            Some(EMPTY_SOURCE_REASON_CODE)
        } else if has_reason(&self.diagnostics, ASSET_ONLY_REASON_CODE) {
            Some(ASSET_ONLY_REASON_CODE)
        } else {
            None
        }
    }
}

fn has_reason(diagnostics: &[Diagnostic], code: &str) -> bool {
    diagnostics.iter().any(|value| value.code == code)
}

fn block_content(block: &Block) -> DocumentContent {
    match block {
        Block::Paragraph(values) => inline_content(values),
        Block::Heading { content, .. } | Block::TimedSegment { content, .. } => {
            inline_content(content)
        }
        Block::List { items, .. } => items
            .iter()
            .flat_map(|item| &item.blocks)
            .map(|node| block_content(&node.block))
            .fold(DocumentContent::Empty, DocumentContent::merge),
        Block::Table { rows, .. } => {
            if rows.is_empty() {
                DocumentContent::Empty
            } else {
                DocumentContent::Visible
            }
        }
        Block::Code { text, .. } | Block::Formula(text) => {
            if text.is_empty() {
                DocumentContent::Empty
            } else {
                DocumentContent::Visible
            }
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Sheet { blocks, .. } => blocks
            .iter()
            .map(|node| block_content(&node.block))
            .fold(DocumentContent::Empty, DocumentContent::merge),
        Block::Slide { title, blocks, .. } => {
            let title = title.as_deref().is_some_and(|value| !value.trim().is_empty());
            if title {
                DocumentContent::Visible
            } else {
                blocks
                    .iter()
                    .map(|node| block_content(&node.block))
                    .fold(DocumentContent::Empty, DocumentContent::merge)
            }
        }
        Block::Image { .. } => DocumentContent::AssetsOnly,
        Block::Rule => DocumentContent::Visible,
    }
}

fn inline_content(values: &[Inline]) -> DocumentContent {
    if values.iter().any(inline_is_visible) {
        DocumentContent::Visible
    } else {
        DocumentContent::Empty
    }
}

fn inline_is_visible(value: &Inline) -> bool {
    match value {
        Inline::Text { value, .. }
        | Inline::SourceText { value, .. }
        | Inline::OcrText { value, .. }
        | Inline::Code(value)
        | Inline::Formula(value) => markdown_has_visible_content(value),
        Inline::Link { content, .. } => content.iter().any(inline_is_visible),
        Inline::FootnoteReference(_) => true,
        Inline::LineBreak => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagnostic, Document};

    fn diagnostic(code: &str, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic { code: code.into(), severity, message: code.into(), locator: None }
    }

    #[test]
    fn empty_results_require_explicit_evidence() {
        let document = Document::default();
        assert!(matches!(
            classify_result(&document, "", &[], &[]),
            Err(ConversionError::EmptyContent)
        ));
        assert_eq!(
            classify_result(
                &document,
                "",
                &[],
                &[diagnostic(EMPTY_SOURCE_REASON_CODE, DiagnosticSeverity::Info)]
            )
            .unwrap(),
            ResultContent::EmptySource
        );
    }

    #[test]
    fn informational_evidence_does_not_degrade_success() {
        assert_eq!(
            conversion_outcome(&[diagnostic(EMPTY_SOURCE_REASON_CODE, DiagnosticSeverity::Info)]),
            ConversionOutcome::Complete
        );
        assert_eq!(
            conversion_outcome(&[diagnostic("omitted", DiagnosticSeverity::Warning)]),
            ConversionOutcome::Degraded
        );
    }

    #[test]
    fn visible_recovery_is_degraded_and_uses_the_first_loss_reason() {
        let result = ConversionResult::new(
            Document::default(),
            "[Unsupported media]".into(),
            Vec::new(),
            vec![diagnostic("media.omitted", DiagnosticSeverity::Warning)],
            Vec::new(),
        );
        assert_eq!(result.content().unwrap(), ResultContent::Markdown);
        assert_eq!(result.outcome(), ConversionOutcome::Degraded);
        assert_eq!(result.reason_code(), Some("media.omitted"));
    }

    #[test]
    fn asset_only_evidence_does_not_override_visible_markdown() {
        let id = crate::AssetId("asset".into());
        let document = Document {
            blocks: vec![crate::BlockNode {
                id: crate::NodeId("image".into()),
                block: Block::Image { asset: id.clone(), alt: None },
                provenance: crate::Provenance {
                    kind: crate::ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: crate::SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        };
        let assets = vec![Asset {
            id,
            filename: None,
            media_type: "image/png".into(),
            bytes: vec![1],
            external_uri: None,
        }];
        let diagnostics = vec![diagnostic(ASSET_ONLY_REASON_CODE, DiagnosticSeverity::Info)];
        assert_eq!(
            classify_result(&document, "![image](asset.png)", &assets, &diagnostics).unwrap(),
            ResultContent::Markdown
        );
        assert_eq!(
            classify_result(&document, "", &assets, &diagnostics).unwrap(),
            ResultContent::AssetsOnly
        );
    }
}
