use into_markdown_core::{
    BoxFuture, ConversionError, Converter, ConverterOutput, ExecutionContext, FormatCandidate,
    FormatDetector, InputFormat, NestedConversionRequest, NestedConversionService, ProbeOutcome,
    Services,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

pub(crate) struct NestedDispatcher {
    detectors: Vec<Arc<dyn FormatDetector>>,
    converters: Vec<Arc<dyn Converter>>,
    base_services: Services,
    self_weak: Weak<Self>,
}

impl NestedDispatcher {
    pub(crate) fn new(
        detectors: Vec<Arc<dyn FormatDetector>>,
        converters: Vec<Arc<dyn Converter>>,
        mut base_services: Services,
    ) -> Arc<Self> {
        base_services.nested = None;
        Arc::new_cyclic(|self_weak| Self {
            detectors,
            converters,
            base_services,
            self_weak: self_weak.clone(),
        })
    }

    fn services(&self) -> Result<Services, ConversionError> {
        let nested = self.self_weak.upgrade().ok_or_else(|| ConversionError::Internal {
            detail: "nested conversion dispatcher is unavailable".into(),
        })?;
        let mut services = self.base_services.clone();
        // This restores only the dispatcher. Every other provider is the
        // caller-configured instance, and the borrowed request options remain
        // the sole network/AI authority for the nested converter.
        services.nested = Some(nested);
        Ok(services)
    }
}

impl NestedConversionService for NestedDispatcher {
    fn convert<'a>(
        &'a self,
        request: NestedConversionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            let candidates =
                detect_formats(&self.detectors, request.input, request.hint, context).await?;
            if candidates.is_empty() {
                return Err(ConversionError::Unsupported {
                    detail: "format detectors produced no candidates for container member".into(),
                });
            }
            let mut selected = None;
            for candidate in &candidates {
                for converter in &self.converters {
                    if request.excluded_converter_ids.contains(&converter.id())
                        || !converter.supported_formats().contains(&candidate.format)
                    {
                        continue;
                    }
                    match context.run(converter.probe(request.input, candidate, context)).await?? {
                        ProbeOutcome::NotApplicable => {}
                        ProbeOutcome::Match { confidence } => {
                            let attempt = Attempt {
                                converter: Arc::clone(converter),
                                candidate,
                                confidence: candidate.confidence * normalize_confidence(confidence),
                            };
                            if selected.as_ref().is_none_or(|current| attempt.precedes(current)) {
                                selected = Some(attempt);
                            }
                        }
                    }
                }
            }
            let Some(attempt) = selected else {
                return Err(ConversionError::NoConverter {
                    format: candidates
                        .iter()
                        .map(|candidate| candidate.format.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                });
            };
            let services = self.services()?;
            let plan = attempt.converter.planned_output_bytes(
                request.input,
                attempt.candidate,
                request.options,
                context,
            )?;
            if plan > context.available_memory_bytes() {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: format!(
                        "nested converter {} planned {plan} bytes but only {} remain",
                        attempt.converter.id(),
                        context.available_memory_bytes()
                    ),
                });
            }
            // The containing converter already runs inside the engine's
            // globally charged preflight credit. Reusing that exact context is
            // required: minting a credit from a child credit is forbidden.
            let output = context
                .run(attempt.converter.convert(
                    request.input,
                    attempt.candidate,
                    request.options,
                    &services,
                    context,
                ))
                .await??;
            let validation_bytes = into_markdown_core::estimate_validation_working_set(
                &output.document,
                &output.assets,
                &output.diagnostics,
            )?;
            let validation_memory = context.reserve_memory(validation_bytes)?;
            output.document.validate().map_err(|error| ConversionError::Internal {
                detail: format!(
                    "nested converter {} returned invalid document IR ({} at {}): {}",
                    attempt.converter.id(),
                    error.code.as_str(),
                    error.path,
                    error.detail
                ),
            })?;
            drop(validation_memory);
            output.account_retained(context)
        })
    }
}

struct Attempt<'a> {
    converter: Arc<dyn Converter>,
    candidate: &'a FormatCandidate,
    confidence: f32,
}

impl Attempt<'_> {
    fn precedes(&self, other: &Self) -> bool {
        self.candidate
            .explicit
            .cmp(&other.candidate.explicit)
            .then_with(|| self.confidence.total_cmp(&other.confidence))
            .then_with(|| self.converter.priority().cmp(&other.converter.priority()))
            .then_with(|| other.converter.id().cmp(self.converter.id()))
            .is_gt()
    }
}

pub(crate) async fn detect_formats(
    detectors: &[Arc<dyn FormatDetector>],
    input: &into_markdown_core::ResolvedInput,
    hint: &into_markdown_core::FormatHint,
    context: &ExecutionContext,
) -> Result<Vec<FormatCandidate>, ConversionError> {
    let mut best: BTreeMap<InputFormat, FormatCandidate> = BTreeMap::new();
    if let Some(format) = hint.format {
        best.insert(format, FormatCandidate::explicit(format));
    }
    for detector in detectors {
        for mut candidate in context.run(detector.detect(input, hint, context)).await?? {
            candidate.explicit = false;
            candidate.confidence = normalize_confidence(candidate.confidence);
            candidate.detector_id = detector.id().into();
            candidate.detector_priority = detector.priority();
            let replace = best.get(&candidate.format).is_none_or(|existing| {
                candidate.explicit && !existing.explicit
                    || candidate.explicit == existing.explicit
                        && (candidate.confidence > existing.confidence
                            || candidate.confidence.total_cmp(&existing.confidence)
                                == std::cmp::Ordering::Equal
                                && (candidate.detector_priority > existing.detector_priority
                                    || candidate.detector_priority == existing.detector_priority
                                        && candidate.detector_id < existing.detector_id))
            });
            if replace {
                best.insert(candidate.format, candidate);
            }
        }
    }
    let mut candidates = best.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .explicit
            .cmp(&left.explicit)
            .then_with(|| right.confidence.total_cmp(&left.confidence))
            .then_with(|| right.detector_priority.cmp(&left.detector_priority))
            .then_with(|| left.detector_id.cmp(&right.detector_id))
            .then_with(|| left.format.cmp(&right.format))
    });
    Ok(candidates)
}

fn normalize_confidence(confidence: f32) -> f32 {
    if confidence.is_finite() { confidence.clamp(0.0, 1.0) } else { 0.0 }
}
