//! Engine-side source evidence attachment and terminal result validation.

use into_markdown_core::{
    ASSET_ONLY_REASON_CODE, ConversionError, ConverterOutput, Diagnostic, DiagnosticSeverity,
    EMPTY_SOURCE_REASON_CODE, ExecutionContext, SourceContentEvidence, estimate_retained_output,
};

const EVIDENCE_DIAGNOSTIC_PEAK_BYTES: u64 = 1_280;

pub(crate) fn attach_evidence(
    mut output: ConverterOutput,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    if output.diagnostics.iter().any(|diagnostic| {
        matches!(diagnostic.code.as_str(), EMPTY_SOURCE_REASON_CODE | ASSET_ONLY_REASON_CODE)
    }) {
        return Err(ConversionError::Internal {
            detail: "converter emitted an engine-reserved result diagnostic".into(),
        });
    }
    let evidence = output.source_content_evidence();
    let (code, message) = match evidence {
        SourceContentEvidence::Unknown => return Ok(output),
        SourceContentEvidence::Empty => {
            (EMPTY_SOURCE_REASON_CODE, "source was fully scanned and contains no visible content")
        }
        SourceContentEvidence::AssetsOnly => {
            (ASSET_ONLY_REASON_CODE, "source contains only asset-backed structured content")
        }
    };

    let retained_before =
        estimate_retained_output(&output.document, &output.assets, &output.diagnostics)?;
    let mut allocation_guard = context.reserve_memory(EVIDENCE_DIAGNOSTIC_PEAK_BYTES)?;
    output.diagnostics.try_reserve(1).map_err(|error| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("cannot reserve source-content diagnostic: {error}"),
    })?;
    output.diagnostics.push(Diagnostic {
        code: code.into(),
        severity: DiagnosticSeverity::Info,
        message: message.into(),
        locator: None,
    });
    let retained_after =
        estimate_retained_output(&output.document, &output.assets, &output.diagnostics)?;
    let retained_delta = retained_after.saturating_sub(retained_before);
    if retained_delta > EVIDENCE_DIAGNOSTIC_PEAK_BYTES {
        return Err(ConversionError::Internal {
            detail: format!(
                "source-content diagnostic retained {retained_delta} bytes beyond its {EVIDENCE_DIAGNOSTIC_PEAK_BYTES}-byte allocation plan"
            ),
        });
    }
    allocation_guard.shrink(EVIDENCE_DIAGNOSTIC_PEAK_BYTES - retained_delta)?;
    output.attach_memory_reservation(context, allocation_guard)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    #[test]
    fn certified_empty_source_gets_complete_audit_evidence() {
        let output = attach_evidence(
            ConverterOutput::default().with_source_content_evidence(SourceContentEvidence::Empty),
            &context(),
        )
        .unwrap();
        assert_eq!(output.diagnostics[0].code, EMPTY_SOURCE_REASON_CODE);
        assert_eq!(output.diagnostics[0].severity, DiagnosticSeverity::Info);
    }

    #[test]
    fn evidence_peak_plan_succeeds_at_the_exact_limit_without_double_charging() {
        let limits = ResourceLimits {
            max_memory_bytes: EVIDENCE_DIAGNOSTIC_PEAK_BYTES,
            ..ResourceLimits::default()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let output = attach_evidence(
            ConverterOutput::default().with_source_content_evidence(SourceContentEvidence::Empty),
            &context,
        )
        .unwrap();

        assert!(context.reserved_memory_bytes() > 0);
        assert!(context.reserved_memory_bytes() < EVIDENCE_DIAGNOSTIC_PEAK_BYTES);
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn evidence_peak_plan_fails_one_byte_below_the_exact_limit() {
        let limits = ResourceLimits {
            max_memory_bytes: EVIDENCE_DIAGNOSTIC_PEAK_BYTES - 1,
            ..ResourceLimits::default()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let error = attach_evidence(
            ConverterOutput::default().with_source_content_evidence(SourceContentEvidence::Empty),
            &context,
        )
        .unwrap_err();

        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[cfg(test)]
#[path = "result_policy/integration_tests.rs"]
mod integration_tests;
