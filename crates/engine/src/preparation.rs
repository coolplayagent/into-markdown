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
    let candidates = engine.detect_formats(source.input(), &request.hint, &context).await?;
    if candidates.is_empty() {
        return Err(ConversionError::Unsupported {
            detail: "format detectors produced no candidates".into(),
        });
    }
    context.record_detected_format(candidates[0].format);
    context.report(ExecutionStage::Probing, Some(0), None, None::<String>)?;

    let mut attempts = Vec::new();
    for candidate in &candidates {
        for converter in &engine.converters {
            if !converter.supported_formats().contains(&candidate.format) {
                continue;
            }
            match context.run(converter.probe(source.input(), candidate, &context)).await?? {
                ProbeOutcome::NotApplicable => {}
                ProbeOutcome::Match { confidence } => attempts.push(Attempt {
                    converter: Arc::clone(converter),
                    candidate: candidate.clone(),
                    explicit: candidate.explicit,
                    confidence: candidate.confidence * normalize_confidence(confidence),
                    priority: converter.priority(),
                }),
            }
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
    let Some(attempt) = attempts.into_iter().next() else {
        let formats = candidates
            .iter()
            .map(|candidate| candidate.format.as_str())
            .collect::<Vec<_>>()
            .join(",");
        return Err(ConversionError::NoConverter { format: formats });
    };
    context.record_detected_format(attempt.candidate.format);
    Ok(PreparedConversion {
        request,
        context,
        source,
        attempt,
        preparation_duration: timer.elapsed(),
    })
}
