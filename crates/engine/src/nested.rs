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
            let mut attempts = Vec::new();
            for candidate in &candidates {
                for converter in &self.converters {
                    if request.excluded_converter_ids.contains(&converter.id())
                        || !converter.supported_formats().contains(&candidate.format)
                    {
                        continue;
                    }
                    match context.run(converter.probe(request.input, candidate, context)).await?? {
                        ProbeOutcome::NotApplicable => {}
                        ProbeOutcome::Match { confidence } => attempts.push(Attempt {
                            converter: Arc::clone(converter),
                            candidate: candidate.clone(),
                            confidence: candidate.confidence * normalize_confidence(confidence),
                        }),
                    }
                }
            }
            attempts.sort_by(|left, right| {
                right
                    .candidate
                    .explicit
                    .cmp(&left.candidate.explicit)
                    .then_with(|| right.confidence.total_cmp(&left.confidence))
                    .then_with(|| right.converter.priority().cmp(&left.converter.priority()))
                    .then_with(|| left.converter.id().cmp(right.converter.id()))
            });
            let Some(attempt) = attempts.into_iter().next() else {
                return Err(ConversionError::NoConverter {
                    format: candidates
                        .iter()
                        .map(|candidate| candidate.format.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                });
            };
            let services = self.services()?;
            let output = context
                .run(attempt.converter.convert(
                    request.input,
                    &attempt.candidate,
                    request.options,
                    &services,
                    context,
                ))
                .await??;
            output.document.validate().map_err(|error| ConversionError::Internal {
                detail: format!(
                    "nested converter {} returned invalid document IR ({} at {}): {}",
                    attempt.converter.id(),
                    error.code.as_str(),
                    error.path,
                    error.detail
                ),
            })?;
            Ok(output)
        })
    }
}

struct Attempt {
    converter: Arc<dyn Converter>,
    candidate: FormatCandidate,
    confidence: f32,
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
