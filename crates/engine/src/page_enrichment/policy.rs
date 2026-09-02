//! Recovery policy for one transactional native PDF page.

use super::{ConversionError, ConverterOutput, InputFormat, OutputEnricher, PageEnrichmentSink};
use into_markdown_core::{
    Diagnostic, DiagnosticSeverity, ResourceFailureScope, ResourceLimitSource,
    ResourceRecoveryAction, ResourceRecoveryBoundary, ResourceUnitKind, classify_resource_recovery,
    recovery_diagnostic,
};

pub(super) fn recovery(
    output: &ConverterOutput,
    sink: &PageEnrichmentSink<'_>,
    enricher: &dyn OutputEnricher,
    error: &ConversionError,
) -> ResourceRecoveryAction {
    if sink.format != InputFormat::Pdf || enricher.id() != super::EMBEDDED_OCR {
        return ResourceRecoveryAction::Fail;
    }
    let locator = page_locator(output);
    classify_resource_recovery(
        sink.options.error_policy,
        error,
        ResourceRecoveryBoundary {
            scope: ResourceFailureScope::VisualRecognition,
            unit: ResourceUnitKind::Page,
            locator: locator.as_ref(),
            rollback_complete: true,
            // The native page and its visual assets are still owned by this
            // sink. Bodyless scan pages therefore have a useful visual fallback.
            fallback_retained: true,
            committed_units: 0,
            omitted_units: 1,
            limit_source: ResourceLimitSource::Explicit,
            precise_required: None,
            raised_limit: None,
        },
    )
}

pub(super) fn push_omitted(
    output: &mut ConverterOutput,
    error: &ConversionError,
) -> Result<(), ConversionError> {
    let locator = page_locator(output);
    let facts = ResourceRecoveryBoundary {
        scope: ResourceFailureScope::VisualRecognition,
        unit: ResourceUnitKind::Page,
        locator: locator.as_ref(),
        rollback_complete: true,
        fallback_retained: true,
        committed_units: 0,
        omitted_units: 1,
        limit_source: ResourceLimitSource::Explicit,
        precise_required: None,
        raised_limit: None,
    };
    let generic = recovery_diagnostic(error, ResourceRecoveryAction::OmitUnit, facts, None)
        .ok_or_else(|| ConversionError::Internal {
            detail: "page recovery selected an error without a resource diagnostic".into(),
        })?;
    output.diagnostics.try_reserve(2).map_err(|allocation| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("cannot reserve page OCR omission diagnostics: {allocation}"),
    })?;
    output.diagnostics.push(Diagnostic {
        code: legacy_ocr_code(error).into(),
        severity: DiagnosticSeverity::Warning,
        message: format!("page OCR was omitted and the original visual was retained: {error}"),
        locator: locator.clone(),
    });
    output.diagnostics.push(generic);
    Ok(())
}

fn legacy_ocr_code(error: &ConversionError) -> &'static str {
    match error {
        ConversionError::OcrRecognitionMemory { .. }
        | ConversionError::ResourceLimit {
            limit:
                "max_memory_bytes"
                | "ocrRecognitionMemory"
                | "recognitionMemory"
                | "recognitionCropMemory"
                | "recognitionOutputMemory",
            ..
        } => "ocr.optionalRecognitionMemorySkipped",
        _ => "ocr.optionalRecognitionResourceSkipped",
    }
}

fn page_locator(output: &ConverterOutput) -> Option<into_markdown_core::SourceLocator> {
    output
        .document
        .blocks
        .first()
        .map(|node| node.provenance.locator.clone())
        .or_else(|| output.diagnostics.iter().find_map(|item| item.locator.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        Block, BlockNode, Document, NodeId, Provenance, ProvenanceKind, SourceLocator,
    };

    #[test]
    fn omission_preserves_legacy_and_generic_diagnostics_at_the_page() {
        let locator = SourceLocator { page: Some(7), ..Default::default() };
        let mut output = ConverterOutput::new(
            Document {
                blocks: vec![BlockNode {
                    id: NodeId("page".into()),
                    block: Block::Page { number: 7, blocks: vec![] },
                    provenance: Provenance {
                        kind: ProvenanceKind::NativeParser,
                        provider: "fixture".into(),
                        locator: locator.clone(),
                        confidence: None,
                    },
                }],
                ..Default::default()
            },
            vec![],
            vec![],
        );
        push_omitted(
            &mut output,
            &ConversionError::ResourceLimit { limit: "ocrWidthLimit", detail: "fixture".into() },
        )
        .unwrap();
        assert_eq!(output.diagnostics[0].code, "ocr.optionalRecognitionResourceSkipped");
        assert_eq!(output.diagnostics[1].code, "resource.ocrWidthLimit.unitOmitted");
        assert!(output.diagnostics.iter().all(|item| item.locator == Some(locator.clone())));
    }
}
