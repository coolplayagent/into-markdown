//! Shared diagnostics for recognized, unsupported containers.
use into_markdown_core::{
    ConversionError, FormatCandidate, InputFormat, RarSignature, ResolvedInput,
};

pub(crate) fn check(
    input: &ResolvedInput,
    candidates: &[FormatCandidate],
) -> Result<(), ConversionError> {
    if candidates.is_empty() {
        return Err(ConversionError::Unsupported {
            detail: "format detectors produced no candidates".into(),
        });
    }
    if candidates
        .first()
        .is_some_and(|candidate| candidate.explicit && candidate.format != InputFormat::Rar)
    {
        return Ok(());
    }
    match RarSignature::detect(&input.bytes) {
        Some(RarSignature::Rar4 | RarSignature::Rar5) => Err(ConversionError::ArchiveExtractionRequired {
            format: "RAR".into(),
        }),
        Some(RarSignature::Damaged) => Err(ConversionError::Malformed {
            part: input.metadata.name.clone(),
            detail: "RAR signature is truncated or invalid; obtain a complete archive, then extract it before conversion".into(),
        }),
        None if candidates.first().is_some_and(|candidate| candidate.format == InputFormat::Rar) => Err(ConversionError::Malformed {
            part: input.metadata.name.clone(),
            detail: "input is labelled RAR but has no complete RAR4/5 signature; check the file contents".into(),
        }),
        None => Ok(()),
    }
}

/// Registry identity for containers delivered through original-source recovery.
pub(crate) struct OriginalContainer;

impl into_markdown_core::Converter for OriginalContainer {
    fn id(&self) -> &'static str {
        "builtin.source-recovery"
    }
    fn supported_formats(&self) -> &'static [InputFormat] {
        &[InputFormat::Rar]
    }
    fn probe<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatCandidate,
        _: &'a into_markdown_core::ExecutionContext,
    ) -> into_markdown_core::BoxFuture<'a, Result<into_markdown_core::ProbeOutcome, ConversionError>>
    {
        Box::pin(async { Ok(into_markdown_core::ProbeOutcome::NotApplicable) })
    }
    fn convert<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatCandidate,
        _: &'a into_markdown_core::ConversionOptions,
        _: &'a into_markdown_core::Services,
        _: &'a into_markdown_core::ExecutionContext,
    ) -> into_markdown_core::BoxFuture<
        'a,
        Result<into_markdown_core::ConverterOutput, ConversionError>,
    > {
        Box::pin(async { Err(ConversionError::ArchiveExtractionRequired { format: "RAR".into() }) })
    }
}

pub(crate) fn original_attempt(
    candidate: &FormatCandidate,
    error: ConversionError,
) -> crate::Attempt {
    crate::Attempt {
        converter: std::sync::Arc::new(OriginalContainer),
        candidate: candidate.clone(),
        explicit: candidate.explicit,
        confidence: -1.0,
        priority: 0,
        probe_error: Some(error),
    }
}
