//! Deterministic, bounded PDF page-layout reconstruction over unified Document IR.
//!
//! The crate deliberately knows nothing about `PDFium` or OCR runtimes. It consumes
//! source-addressed `SourceText` and `OcrText` from PDF page containers and emits
//! the same stable IR shapes for native-only and native-plus-OCR documents.

#![forbid(unsafe_code)]

mod budget;
mod collect;
mod dedup;
mod footnotes;
mod geometry;
mod gutters;
mod ids;
mod lines;
mod materialize;
mod model;
mod ordering;
mod reading_order;
mod running_matter;
mod semantics;
mod tables;

#[cfg(test)]
mod quality_authority_tests;
#[cfg(test)]
mod tests;

use budget::LayoutBudget;
use into_markdown_core::{
    Block, ConversionError, Document, ExecutionContext, Rect, ResourceReservation,
};

/// Stable provider attached to blocks reconstructed from PDF geometry.
pub const LAYOUT_PROVIDER: &str = "builtin.pdf.layout";

/// Hard local limits for one PDF-layout operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutLimits {
    /// Maximum source-addressed text atoms across the document.
    pub max_atoms: usize,
    /// Maximum geometric lines constructed across the document.
    pub max_lines: usize,
    /// Maximum candidate comparisons across clustering and table recovery.
    pub max_comparisons: u64,
    /// Maximum columns accepted in a recovered table.
    pub max_table_columns: usize,
    /// Maximum cells accepted across recovered tables.
    pub max_table_cells: usize,
}

impl Default for LayoutLimits {
    fn default() -> Self {
        Self {
            max_atoms: into_markdown_core::MAX_DOCUMENT_INLINES,
            max_lines: into_markdown_core::MAX_DOCUMENT_NODES,
            max_comparisons: 12_000_000,
            max_table_columns: 16_384,
            max_table_cells: 1_000_000,
        }
    }
}

/// Deterministic PDF-layout policy and resource bounds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutConfig {
    /// Local resource and work limits.
    pub limits: LayoutLimits,
}

/// Page-scoped bounds of PDF PATH objects supplied out of band by the PDF
/// converter. They are transient structural evidence and never enter wire IR.
#[derive(Debug, Clone, PartialEq)]
pub struct PagePathEvidence {
    /// One-based PDF page number.
    pub page: u32,
    /// Finite displayed-coordinate bounds, in PDF points.
    pub bounds: Vec<Rect>,
}

/// Consumed document plus the request-scoped reservation that covers rebuilt IR.
pub struct LayoutOutput {
    document: Document,
    reservation: Option<ResourceReservation>,
}

impl LayoutOutput {
    /// Transfer the rebuilt document and its live reservation to the caller.
    #[must_use]
    pub fn into_parts(self) -> (Document, Option<ResourceReservation>) {
        (self.document, self.reservation)
    }
}

/// Reconstruct every PDFium-backed page in one consumed document.
///
/// Non-PDF documents are returned unchanged and acquire no reservation. All
/// preflight and reconstruction completes before the returned document can be
/// published; cancellation, deadline, malformed geometry, and limit failures
/// drop the consumed local document and reservation together.
///
/// # Errors
///
/// Returns a stable conversion error for invalid geometry, exhausted work or
/// memory limits, cancellation, deadline, or invalid reconstructed IR.
pub fn reconstruct_document(
    document: Document,
    config: &LayoutConfig,
    context: &ExecutionContext,
) -> Result<LayoutOutput, ConversionError> {
    reconstruct_document_with_path_evidence(document, config, &[], context)
}

/// Reconstruct PDF pages with bounded, page-scoped native PATH evidence.
///
/// PATH bounds may corroborate a text table but are never published into the
/// output document. Evidence pages must be strictly increasing and must refer
/// to PDF page containers in the consumed document.
///
/// # Errors
///
/// Returns a stable conversion error for invalid or unmatched path evidence,
/// malformed geometry, exhausted resources, cancellation, or deadline.
pub fn reconstruct_document_with_path_evidence(
    mut document: Document,
    config: &LayoutConfig,
    path_evidence: &[PagePathEvidence],
    context: &ExecutionContext,
) -> Result<LayoutOutput, ConversionError> {
    validate_config(config)?;
    if !has_pdf_pages(&document) {
        if !path_evidence.is_empty() {
            return Err(malformed("pdfLayoutPathEvidenceWithoutPdfPage"));
        }
        return Ok(LayoutOutput { document, reservation: None });
    }
    validate_path_evidence(path_evidence)?;
    let mut budget = LayoutBudget::preflight(&document, path_evidence, config, context)?;
    let mut rebuilt = Vec::new();
    rebuilt.try_reserve_exact(document.blocks.len()).map_err(|_| memory("root blocks"))?;
    let mut evidence_index = 0_usize;
    for node in document.blocks.drain(..) {
        budget.checkpoint_item()?;
        let into_markdown_core::BlockNode { id, block, provenance } = node;
        match block {
            Block::Page { number, blocks } if is_pdf_provenance(&provenance.provider) => {
                let page_paths = if path_evidence
                    .get(evidence_index)
                    .is_some_and(|evidence| evidence.page == number)
                {
                    let bounds = path_evidence[evidence_index].bounds.as_slice();
                    evidence_index += 1;
                    bounds
                } else {
                    &[]
                };
                let blocks = materialize::reconstruct_page(
                    number,
                    blocks,
                    &provenance,
                    page_paths,
                    config,
                    &mut budget,
                )?;
                rebuilt.push(into_markdown_core::BlockNode {
                    id,
                    block: Block::Page { number, blocks },
                    provenance,
                });
            }
            block => rebuilt.push(into_markdown_core::BlockNode { id, block, provenance }),
        }
    }
    if evidence_index != path_evidence.len() {
        return Err(malformed("pdfLayoutPathEvidencePageMismatch"));
    }
    document.blocks = rebuilt;
    running_matter::annotate(&mut document.blocks, &mut budget)?;
    document.validate().map_err(|error| ConversionError::Malformed {
        part: Some("pdf-layout".into()),
        detail: format!("layoutInvalidIr:{}:{}", error.code.as_str(), error.path),
    })?;
    let reservation = budget.finish()?;
    Ok(LayoutOutput { document, reservation: Some(reservation) })
}

fn validate_path_evidence(path_evidence: &[PagePathEvidence]) -> Result<(), ConversionError> {
    let mut previous = 0_u32;
    for evidence in path_evidence {
        if evidence.page == 0 || evidence.page <= previous {
            return Err(malformed("pdfLayoutPathEvidencePageOrder"));
        }
        previous = evidence.page;
        if evidence.bounds.iter().any(|bounds| {
            !bounds.x.is_finite()
                || !bounds.y.is_finite()
                || !bounds.width.is_finite()
                || !bounds.height.is_finite()
                || bounds.width < 0.0
                || bounds.height < 0.0
        }) {
            return Err(malformed("pdfLayoutInvalidPathBounds"));
        }
    }
    Ok(())
}

fn validate_config(config: &LayoutConfig) -> Result<(), ConversionError> {
    let limits = &config.limits;
    if limits.max_atoms == 0
        || limits.max_lines == 0
        || limits.max_comparisons == 0
        || limits.max_table_columns == 0
        || limits.max_table_cells == 0
    {
        return Err(limit("pdfLayoutConfig", "layout limits must be positive"));
    }
    Ok(())
}

fn has_pdf_pages(document: &Document) -> bool {
    document.blocks.iter().any(|node| {
        matches!(node.block, Block::Page { .. }) && is_pdf_provenance(&node.provenance.provider)
    })
}

fn is_pdf_provenance(provider: &str) -> bool {
    provider == "builtin.converter.pdfium" || provider == LAYOUT_PROVIDER
}

fn memory(detail: &'static str) -> ConversionError {
    limit("max_memory_bytes", detail)
}

fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("pdf-layout".into()), detail: detail.into() }
}
