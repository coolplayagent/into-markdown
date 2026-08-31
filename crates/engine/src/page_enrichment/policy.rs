//! PDF's optional native-page preflight policy, before any image recognition.

use super::*;
use into_markdown_core::{
    AiMode, Block, BlockNode, Diagnostic, DiagnosticSeverity, ErrorPolicy, Inline, OcrPolicy,
    ProvenanceKind,
};

pub(super) fn optional_page(
    output: &ConverterOutput,
    sink: &PageEnrichmentSink<'_>,
    enricher: &dyn OutputEnricher,
) -> Result<bool, ConversionError> {
    if sink.format != InputFormat::Pdf
        || sink.options.error_policy != ErrorPolicy::BestEffort
        || enricher.id() != super::EMBEDDED_OCR
    {
        return Ok(false);
    }
    let effective = match sink.options.ai.vision_ocr {
        AiMode::Only => OcrPolicy::Always,
        AiMode::Fallback | AiMode::Prefer if sink.options.ocr.policy == OcrPolicy::Off => {
            OcrPolicy::Auto
        }
        _ => sink.options.ocr.policy,
    };
    if effective != OcrPolicy::Auto {
        return Ok(false);
    }
    for diagnostic in &output.diagnostics {
        sink.context.checkpoint()?;
        if diagnostic.code == "pdf.scannedPage" {
            return Ok(false);
        }
    }
    native_body(&output.document.blocks, sink)
}

fn native_body(
    nodes: &[BlockNode],
    sink: &PageEnrichmentSink<'_>,
) -> Result<bool, ConversionError> {
    for node in nodes {
        sink.context.checkpoint()?;
        if node.provenance.kind != ProvenanceKind::NativeParser
            || (node.provenance.provider != sink.converter_id
                && node.provenance.provider != "builtin.pdf.layout")
        {
            continue;
        }
        let body = match &node.block {
            Block::Paragraph(inlines) => inlines.iter().any(native_inline),
            Block::Code { text, .. } => printable(text),
            Block::Page { blocks, .. } => native_body(blocks, sink)?,
            _ => false,
        };
        if body {
            return Ok(true);
        }
    }
    Ok(false)
}

fn native_inline(inline: &Inline) -> bool {
    match inline {
        Inline::Text { value, .. } => printable(value),
        Inline::SourceText { value, provenance, .. } => {
            provenance.kind == ProvenanceKind::NativeParser && printable(value)
        }
        Inline::Link { content, .. } => content.iter().any(native_inline),
        _ => false,
    }
}

fn printable(text: &str) -> bool {
    text.chars().any(|c| !c.is_whitespace() && !c.is_control())
}

pub(super) fn push_skipped(
    output: &mut ConverterOutput,
    error: &ConversionError,
) -> Result<(), ConversionError> {
    output.diagnostics.try_reserve(1).map_err(|allocation| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("cannot reserve optional OCR diagnostic: {allocation}"),
    })?;
    output.diagnostics.push(Diagnostic {
        code: "presentation.optionalOcrSkipped".into(),
        severity: DiagnosticSeverity::Warning,
        message: format!("optional embedded OCR was skipped: {error}"),
        locator: output.document.blocks.first().map(|node| node.provenance.locator.clone()),
    });
    Ok(())
}
