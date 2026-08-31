//! PDF-side orchestration of the generic OCR merge and final page layout.

use into_markdown_core::{ConversionError, ConversionOptions, ConverterOutput, ExecutionContext};
use into_markdown_ocr::{MergeConfig, OcrPageInput};
use into_markdown_pdf_layout::{LayoutConfig, reconstruct_document};

/// Merge recognizer-bound OCR pages into a consumed PDF converter output, then
/// reconstruct native and OCR text through one page-layout path.
///
/// This is intentionally owned by the PDF orchestration layer. The generic OCR
/// crate does not depend on, name, or invoke PDF layout. Any failure consumes
/// and drops the unpublished input and all request-owned leases.
///
/// # Errors
///
/// Returns the stable OCR, layout, validation, cancellation, deadline, or
/// resource error produced before a unified output can be published.
pub fn merge_pdf_ocr(
    mut source: ConverterOutput,
    pages: &[OcrPageInput],
    merge_config: &MergeConfig,
    layout_config: &LayoutConfig,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let document = std::mem::take(&mut source.document);
    let mut merged = into_markdown_ocr::merge_document(document, pages, merge_config, context)?;
    source.diagnostics.try_reserve_exact(merged.diagnostics.len()).map_err(|_| {
        ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "PDF OCR diagnostic inventory allocation failed".into(),
        }
    })?;
    source.diagnostics.append(&mut merged.diagnostics);
    let document = std::mem::take(&mut merged.document);
    source.absorb_memory_lease(&mut merged, context)?;
    drop(merged);
    let layout = reconstruct_document(document, layout_config, context)?;
    let (document, reservation) = layout.into_parts();
    source.document = document;
    if let Some(reservation) = reservation {
        source.attach_memory_reservation(context, reservation)?;
    }
    source.account_retained(context)
}

/// Reconstruct an already-enriched PDF output through the same native/OCR
/// coordinate merge used by page OCR. This keeps embedded-image OCR from
/// becoming a second, order-dependent text stream.
pub(crate) fn reconstruct_enriched_pdf(
    mut source: ConverterOutput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let document = std::mem::take(&mut source.document);
    let layout_config = LayoutConfig {
        limits: into_markdown_pdf_layout::LayoutLimits {
            max_comparisons: options.limits.max_pdf_layout_comparisons,
            max_table_columns: usize::try_from(options.limits.max_table_columns)
                .unwrap_or(usize::MAX)
                .min(into_markdown_core::MAX_TABLE_COLUMNS),
            max_table_cells: usize::try_from(options.limits.max_table_cells)
                .unwrap_or(usize::MAX)
                .min(into_markdown_core::MAX_DOCUMENT_NODES),
            ..into_markdown_pdf_layout::LayoutLimits::default()
        },
    };
    let layout = reconstruct_document(document, &layout_config, context)?;
    let (document, reservation) = layout.into_parts();
    source.document = document;
    if let Some(reservation) = reservation {
        source.attach_memory_reservation(context, reservation)?;
    }
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        Asset, AssetId, Block, BlockNode, Diagnostic, DiagnosticSeverity, Document,
        ExecutionOptions, Inline, NodeId, OcrPolicy, Provenance, ProvenanceKind, Rect,
        ResourceLimits, SourceLocator,
    };

    #[test]
    fn orchestration_preserves_assets_and_diagnostics_then_reflows_native_pdf_ir() {
        let bounds = Rect { x: 20.0, y: 30.0, width: 8.0, height: 12.0 };
        let provenance = |bounds| Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "builtin.converter.pdfium".into(),
            locator: SourceLocator {
                page: Some(1),
                bounds,
                page_width: Some(600.0),
                page_height: Some(800.0),
                font_size: Some(12.0),
                rotation_degrees: Some(0.0),
                ..SourceLocator::default()
            },
            confidence: None,
        };
        let document = Document {
            blocks: vec![BlockNode {
                id: NodeId("pdf-page-1".into()),
                block: Block::Page {
                    number: 1,
                    blocks: vec![BlockNode {
                        id: NodeId("pdf-page-1-native-text".into()),
                        block: Block::Paragraph(vec![Inline::SourceText {
                            value: "A".into(),
                            marks: Vec::new(),
                            provenance: Box::new(provenance(Some(bounds))),
                        }]),
                        provenance: provenance(None),
                    }],
                },
                provenance: provenance(None),
            }],
            ..Document::default()
        };
        let source = ConverterOutput::new(
            document,
            vec![Asset {
                id: AssetId("image".into()),
                filename: Some("image.bmp".into()),
                media_type: "image/bmp".into(),
                bytes: vec![0; 4],
                external_uri: None,
            }],
            vec![Diagnostic {
                code: "pdf.test".into(),
                severity: DiagnosticSeverity::Info,
                message: "retained".into(),
                locator: None,
            }],
        );
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let output = merge_pdf_ocr(
            source,
            &[],
            &MergeConfig { policy: OcrPolicy::Off, ..MergeConfig::default() },
            &LayoutConfig::default(),
            &context,
        )
        .unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.diagnostics[0].code, "pdf.test");
        let Block::Page { blocks, .. } = &output.document.blocks[0].block else { panic!("page") };
        assert!(blocks[0].id.0.contains("layout-paragraph"));
        assert!(output.leased_memory_for(&context) > 0);
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn enriched_relayout_preserves_request_table_limits() {
        for configure in [
            (|options: &mut ConversionOptions| options.limits.max_table_columns = 0)
                as fn(&mut ConversionOptions),
            (|options: &mut ConversionOptions| options.limits.max_table_cells = 0)
                as fn(&mut ConversionOptions),
        ] {
            let mut options = ConversionOptions::default();
            configure(&mut options);
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let source = ConverterOutput::new(Document::default(), Vec::new(), Vec::new());
            assert!(matches!(
                reconstruct_enriched_pdf(source, &options, &context),
                Err(ConversionError::ResourceLimit { limit: "pdfLayoutConfig", .. })
            ));
        }
    }
}
