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
    fn merge_non_visible(&mut self, other: Self) -> bool {
        match other {
            Self::Visible => true,
            Self::AssetsOnly => {
                *self = Self::AssetsOnly;
                false
            }
            Self::Empty => false,
        }
    }
}

/// Classify the semantic content retained by a document independently of its
/// format-specific container structure.
#[must_use]
pub(crate) fn document_content(document: &Document) -> DocumentContent {
    nodes_content(&document.blocks)
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
    if markdown_has_visible_content(markdown) {
        return Ok(ResultContent::Markdown);
    }
    let (empty_source, asset_only) = evidence_reasons(diagnostics);
    if empty_source && asset_only {
        return Err(ConversionError::Internal {
            detail: "result carries conflicting empty-source and asset-only evidence".into(),
        });
    }
    if empty_source {
        if !assets.is_empty() || document_content(document) != DocumentContent::Empty {
            return Err(ConversionError::Internal {
                detail: "empty-source evidence conflicts with retained document content".into(),
            });
        }
        return Ok(ResultContent::EmptySource);
    }
    if assets.is_empty() {
        return Err(ConversionError::EmptyContent);
    }
    let content = document_content(document);
    if asset_only {
        if content != DocumentContent::AssetsOnly || assets.is_empty() {
            return Err(ConversionError::Internal {
                detail: "asset-only evidence requires asset-only IR and retained assets".into(),
            });
        }
        return Ok(ResultContent::AssetsOnly);
    }
    if content == DocumentContent::AssetsOnly {
        Ok(ResultContent::AssetsOnly)
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
            match self.content().ok()? {
                ResultContent::AssetsOnly => Some(ASSET_ONLY_REASON_CODE),
                ResultContent::Markdown | ResultContent::EmptySource => None,
            }
        }
    }

    /// Whether every retained asset can be represented by a destination that
    /// accepts local payloads and/or external URI metadata.
    #[must_use]
    pub fn assets_are_deliverable(&self, payloads: bool, external_references: bool) -> bool {
        assets_are_deliverable(&self.assets, payloads, external_references)
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
            match self.content? {
                ResultContent::AssetsOnly => Some(ASSET_ONLY_REASON_CODE),
                ResultContent::Markdown | ResultContent::EmptySource => None,
            }
        }
    }

    /// Whether every retained asset can be represented by a destination that
    /// accepts local payloads and/or external URI metadata.
    #[must_use]
    pub const fn assets_are_deliverable(&self, payloads: bool, external_references: bool) -> bool {
        (payloads || self.payload_only_assets == 0)
            && (external_references || self.external_only_assets == 0)
            && (payloads || external_references || self.dual_representation_assets == 0)
    }
}

fn evidence_reasons(diagnostics: &[Diagnostic]) -> (bool, bool) {
    let mut empty_source = false;
    let mut asset_only = false;
    for diagnostic in diagnostics {
        match diagnostic.code.as_str() {
            EMPTY_SOURCE_REASON_CODE => empty_source = true,
            ASSET_ONLY_REASON_CODE => asset_only = true,
            _ => {}
        }
        if empty_source && asset_only {
            break;
        }
    }
    (empty_source, asset_only)
}

fn assets_are_deliverable(assets: &[Asset], payloads: bool, external_references: bool) -> bool {
    assets.iter().all(|asset| {
        !asset.bytes.is_empty() && payloads || asset.external_uri.is_some() && external_references
    })
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
        Block::List { items, .. } => {
            let mut content = DocumentContent::Empty;
            for item in items {
                if content.merge_non_visible(nodes_content(&item.blocks)) {
                    return DocumentContent::Visible;
                }
            }
            content
        }
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
        | Block::Sheet { blocks, .. } => nodes_content(blocks),
        Block::Slide { title, blocks, .. } => {
            let title = title.as_deref().is_some_and(|value| !value.trim().is_empty());
            if title { DocumentContent::Visible } else { nodes_content(blocks) }
        }
        Block::Image { .. } => DocumentContent::AssetsOnly,
        Block::Rule => DocumentContent::Visible,
    }
}

fn nodes_content(nodes: &[crate::BlockNode]) -> DocumentContent {
    let mut content = DocumentContent::Empty;
    for node in nodes {
        if content.merge_non_visible(block_content(&node.block)) {
            return DocumentContent::Visible;
        }
    }
    content
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

    #[test]
    fn visible_markdown_short_circuits_a_large_ir_inventory() {
        let prototype = crate::BlockNode {
            id: crate::NodeId("empty".into()),
            block: Block::Paragraph(Vec::new()),
            provenance: crate::Provenance {
                kind: crate::ProvenanceKind::NativeParser,
                provider: "test".into(),
                locator: crate::SourceLocator::default(),
                confidence: None,
            },
        };
        let document = Document { blocks: vec![prototype; 100_000], ..Document::default() };

        assert_eq!(
            classify_result(&document, "visible", &[], &[]).unwrap(),
            ResultContent::Markdown
        );
    }
}
