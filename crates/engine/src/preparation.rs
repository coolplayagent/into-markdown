//! Resolution, detection, and converter selection for one execution.

use super::{Attempt, Engine, measured_input_bytes, normalize_confidence};
use into_markdown_core::{
    ConversionError, ConversionRequest, ExecutionContext, ExecutionStage, ProbeOutcome,
    ResolvedSource,
};
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct PreparedConversion {
    pub(crate) request: ConversionRequest,
    pub(crate) context: ExecutionContext,
    pub(crate) source: ResolvedSource,
    pub(crate) attempt: Attempt,
    pub(crate) preparation_duration: Duration,
}

pub(crate) async fn prepare(
    engine: &Engine,
    mut request: ConversionRequest,
    context: ExecutionContext,
) -> Result<PreparedConversion, ConversionError> {
    let timer = crate::timing::ProcessingTimer::start();
    if context.resource_limits() != &request.options.limits {
        return Err(ConversionError::Internal {
            detail: "shared execution context limits do not match conversion request".into(),
        });
    }
    if request.options.text.charset.is_none() {
        request.options.text.charset.clone_from(&request.hint.charset);
    }
    context.report(ExecutionStage::Resolving, None, None, None::<String>)?;
    let mut source = engine.resolve_input(&request.input, &request.options, &context).await?;
    measured_input_bytes(source.input(), &request.options)?;
    source.ensure_memory_reservation(&context)?;

    context.report(ExecutionStage::Detecting, None, None, None::<String>)?;
    let candidates = super::source_recovery::detect_for_conversion(
        &engine.format_detectors,
        source.input(),
        &request.hint,
        &request.options,
        &context,
    )
    .await?;
    context.record_detected_format(candidates[0].format);
    context.report(ExecutionStage::Probing, Some(0), None, None::<String>)?;

    let attempt = select_converter(
        &engine.converters,
        source.input(),
        &candidates,
        &request.options,
        &context,
        &[],
    )
    .await?;
    context.record_detected_format(attempt.candidate.format);
    Ok(PreparedConversion {
        request,
        context,
        source,
        attempt,
        preparation_duration: timer.elapsed(),
    })
}

pub(crate) async fn select_converter(
    converters: &[Arc<dyn into_markdown_core::Converter>],
    input: &into_markdown_core::ResolvedInput,
    candidates: &[into_markdown_core::FormatCandidate],
    options: &into_markdown_core::ConversionOptions,
    context: &ExecutionContext,
    excluded: &[&str],
) -> Result<Attempt, ConversionError> {
    if let Some(candidate) =
        candidates.first().filter(|c| c.detector_id == "builtin.source-recovery")
    {
        return Ok(super::unsupported::original_attempt(
            candidate,
            ConversionError::Unsupported { detail: candidate.evidence.clone() },
        ));
    }
    if let Err(error) = super::unsupported::check(input, candidates) {
        if super::source_recovery::recoverable(options, &error)
            && let Some(candidate) = candidates.first()
        {
            return Ok(super::unsupported::original_attempt(candidate, error));
        }
        return Err(error);
    }
    let mut attempts = Vec::new();
    for candidate in candidates {
        for converter in converters {
            if excluded.contains(&converter.id())
                || !converter.supported_formats().contains(&candidate.format)
            {
                continue;
            }
            let (confidence, probe_error) =
                match context.run(converter.probe(input, candidate, context)).await? {
                    Ok(ProbeOutcome::NotApplicable) => continue,
                    Ok(ProbeOutcome::Match { confidence }) => {
                        (candidate.confidence * normalize_confidence(confidence), None)
                    }
                    Err(error) if super::source_recovery::recoverable(options, &error) => {
                        (-1.0, Some(error))
                    }
                    Err(error) => return Err(error),
                };
            attempts.push(Attempt {
                converter: Arc::clone(converter),
                candidate: candidate.clone(),
                explicit: candidate.explicit,
                confidence,
                priority: converter.priority(),
                probe_error,
            });
        }
    }
    attempts.sort_by(|left, right| {
        right
            .explicit
            .cmp(&left.explicit)
            .then_with(|| right.confidence.total_cmp(&left.confidence))
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| left.converter.id().cmp(right.converter.id()))
    });
    if attempts.is_empty() {
        for candidate in candidates {
            let error = ConversionError::Unsupported {
                detail: format!("No converter accepted the detected {} source", candidate.format),
            };
            if super::source_recovery::recoverable(options, &error)
                && let Some(converter) = converters.iter().find(|converter| {
                    !excluded.contains(&converter.id())
                        && converter.supported_formats().contains(&candidate.format)
                })
            {
                attempts.push(Attempt {
                    converter: Arc::clone(converter),
                    candidate: candidate.clone(),
                    explicit: candidate.explicit,
                    confidence: -1.0,
                    priority: converter.priority(),
                    probe_error: Some(error),
                });
                break;
            }
        }
    }
    let Some(attempt) = attempts.into_iter().next() else {
        let formats = candidates
            .iter()
            .map(|candidate| candidate.format.as_str())
            .collect::<Vec<_>>()
            .join(",");
        return Err(ConversionError::NoConverter { format: formats });
    };
    Ok(attempt)
}
