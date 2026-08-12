//! Deterministic registry and conversion pipeline orchestration.

use into_markdown_core::{
    Block, BlockNode, ConversionError, ConversionRequest, ConversionResult, Converter,
    DetectionRequest, DetectionResult, FormatCandidate, FormatDetector, InputFormat,
    MarkdownRenderer, ProbeOutcome, Services, SourceResolver,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Explicit registry used by built-ins and future process-isolated plugins.
#[derive(Default)]
pub struct RegistryBuilder {
    source_resolvers: Vec<Arc<dyn SourceResolver>>,
    format_detectors: Vec<Arc<dyn FormatDetector>>,
    converters: Vec<Arc<dyn Converter>>,
}

impl RegistryBuilder {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a source resolver.
    pub fn register_source_resolver(&mut self, resolver: Arc<dyn SourceResolver>) -> &mut Self {
        self.source_resolvers.push(resolver);
        self
    }

    /// Register a format detector.
    pub fn register_format_detector(&mut self, detector: Arc<dyn FormatDetector>) -> &mut Self {
        self.format_detectors.push(detector);
        self
    }

    /// Register a format converter.
    pub fn register_converter(&mut self, converter: Arc<dyn Converter>) -> &mut Self {
        self.converters.push(converter);
        self
    }

    fn validate(&self) -> Result<(), ConversionError> {
        validate_unique("source resolver", self.source_resolvers.iter().map(|v| v.id()))?;
        validate_unique("format detector", self.format_detectors.iter().map(|v| v.id()))?;
        validate_unique("converter", self.converters.iter().map(|v| v.id()))
    }
}

fn validate_unique<'a>(
    kind: &str,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), ConversionError> {
    let mut seen = BTreeSet::new();
    for id in ids {
        if id.is_empty() {
            return Err(ConversionError::Internal { detail: format!("{kind} ID is empty") });
        }
        if !seen.insert(id) {
            return Err(ConversionError::Internal { detail: format!("duplicate {kind} ID: {id}") });
        }
    }
    Ok(())
}

/// Builder for a conversion engine and its explicitly registered services.
#[derive(Default)]
pub struct EngineBuilder {
    registry: RegistryBuilder,
    renderer: Option<Arc<dyn MarkdownRenderer>>,
    services: Services,
}

impl EngineBuilder {
    /// Create an empty engine builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Access the explicit component registry.
    pub fn registry_mut(&mut self) -> &mut RegistryBuilder {
        &mut self.registry
    }

    /// Set the single Markdown renderer.
    #[must_use]
    pub fn renderer(mut self, renderer: Arc<dyn MarkdownRenderer>) -> Self {
        self.renderer = Some(renderer);
        self
    }

    /// Set optional OCR, transcription, and AI services.
    #[must_use]
    pub fn services(mut self, services: Services) -> Self {
        self.services = services;
        self
    }

    /// Validate IDs and build an immutable engine.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::Internal`] for empty or duplicate component
    /// IDs.
    pub fn build(mut self) -> Result<Engine, ConversionError> {
        self.registry.validate()?;
        self.registry.format_detectors.sort_by(|left, right| {
            right.priority().cmp(&left.priority()).then_with(|| left.id().cmp(right.id()))
        });
        Ok(Engine {
            source_resolvers: self.registry.source_resolvers,
            format_detectors: self.registry.format_detectors,
            converters: self.registry.converters,
            renderer: self.renderer,
            services: self.services,
        })
    }
}

/// Immutable conversion engine.
pub struct Engine {
    source_resolvers: Vec<Arc<dyn SourceResolver>>,
    format_detectors: Vec<Arc<dyn FormatDetector>>,
    converters: Vec<Arc<dyn Converter>>,
    renderer: Option<Arc<dyn MarkdownRenderer>>,
    services: Services,
}

impl Engine {
    /// Resolve an input and return ordered format hypotheses without converting.
    ///
    /// # Errors
    ///
    /// Returns a typed error from source resolution or format detection.
    pub async fn detect(
        &self,
        request: DetectionRequest,
    ) -> Result<DetectionResult, ConversionError> {
        let input = self.resolve_input(&request.input, &request.options).await?;
        let candidates = self.detect_formats(&input, &request.hint).await?;
        Ok(DetectionResult { source: input.metadata, candidates })
    }

    /// Resolve, detect, select, convert, and render one request.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ConversionError`] from source resolution, detection,
    /// converter selection/conversion, or Markdown rendering.
    pub async fn convert(
        &self,
        request: ConversionRequest,
    ) -> Result<ConversionResult, ConversionError> {
        let input = self.resolve_input(&request.input, &request.options).await?;

        let candidates = self.detect_formats(&input, &request.hint).await?;
        if candidates.is_empty() {
            return Err(ConversionError::Unsupported {
                detail: "format detectors produced no candidates".into(),
            });
        }

        let mut attempts = Vec::new();
        for candidate in &candidates {
            for converter in &self.converters {
                if !converter.supported_formats().contains(&candidate.format) {
                    continue;
                }
                match converter.probe(&input, candidate).await? {
                    ProbeOutcome::NotApplicable => {}
                    ProbeOutcome::Match { confidence } => attempts.push(Attempt {
                        converter: Arc::clone(converter),
                        candidate: candidate.clone(),
                        confidence: candidate.confidence * normalize_confidence(confidence),
                        priority: converter.priority(),
                    }),
                }
            }
        }

        attempts.sort_by(|left, right| {
            right
                .confidence
                .total_cmp(&left.confidence)
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

        // A successful probe makes conversion authoritative. Conversion errors
        // are returned immediately rather than being hidden by another parser.
        let output = attempt
            .converter
            .convert(&input, &attempt.candidate, &request.options, &self.services)
            .await?;
        output.document.validate().map_err(|error| ConversionError::Internal {
            detail: format!(
                "converter {} returned invalid document IR ({} at {}): {}",
                attempt.converter.id(),
                error.code.as_str(),
                error.path,
                error.detail
            ),
        })?;
        let renderer = self.renderer.as_ref().ok_or_else(|| ConversionError::Internal {
            detail: "no Markdown renderer is registered".into(),
        })?;
        let markdown = renderer.render(&output.document, &output.assets, &request.options).await?;
        let mut provenance = Vec::new();
        collect_provenance(&output.document.blocks, &mut provenance);
        Ok(ConversionResult {
            document: output.document,
            markdown,
            assets: output.assets,
            diagnostics: output.diagnostics,
            provenance,
        })
    }

    async fn resolve_input(
        &self,
        input: &into_markdown_core::InputRef,
        options: &into_markdown_core::ConversionOptions,
    ) -> Result<into_markdown_core::ResolvedInput, ConversionError> {
        let resolver =
            self.source_resolvers.iter().find(|resolver| resolver.supports(input)).ok_or_else(
                || ConversionError::Unsupported {
                    detail: "no source resolver accepts the requested input".into(),
                },
            )?;
        resolver.resolve(input, options).await
    }

    async fn detect_formats(
        &self,
        input: &into_markdown_core::ResolvedInput,
        hint: &into_markdown_core::FormatHint,
    ) -> Result<Vec<FormatCandidate>, ConversionError> {
        let mut best: BTreeMap<InputFormat, FormatCandidate> = BTreeMap::new();
        if let Some(format) = hint.format {
            best.insert(format, FormatCandidate::explicit(format));
        }
        for detector in &self.format_detectors {
            for candidate in detector.detect(input, hint).await? {
                let replace = best.get(&candidate.format).is_none_or(|existing| {
                    candidate.explicit && !existing.explicit
                        || candidate.explicit == existing.explicit
                            && candidate.confidence > existing.confidence
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
                .then_with(|| left.format.cmp(&right.format))
        });
        Ok(candidates)
    }
}

struct Attempt {
    converter: Arc<dyn Converter>,
    candidate: FormatCandidate,
    confidence: f32,
    priority: i32,
}

fn normalize_confidence(confidence: f32) -> f32 {
    if confidence.is_finite() { confidence.clamp(0.0, 1.0) } else { 0.0 }
}

fn collect_provenance(nodes: &[BlockNode], output: &mut Vec<into_markdown_core::Provenance>) {
    for node in nodes {
        output.push(node.provenance.clone());
        match &node.block {
            Block::List { items, .. } => {
                for item in items {
                    collect_provenance(&item.blocks, output);
                }
            }
            Block::Table { rows } => {
                for row in rows {
                    for cell in &row.cells {
                        collect_provenance(&cell.blocks, output);
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => collect_provenance(blocks, output),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        Asset, BoxFuture, ConversionOptions, ConverterOutput, Document, FormatHint, InputRef,
        MarkdownRenderer, ResolvedInput, SourceMetadata,
    };

    struct BytesResolver;
    impl SourceResolver for BytesResolver {
        fn id(&self) -> &'static str {
            "test.bytes"
        }
        fn supports(&self, input: &InputRef) -> bool {
            matches!(input, InputRef::Bytes { .. })
        }
        fn resolve<'a>(
            &'a self,
            input: &'a InputRef,
            _: &'a ConversionOptions,
        ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
            Box::pin(async move {
                let InputRef::Bytes { data, name } = input else {
                    return Err(ConversionError::Unsupported { detail: "expected bytes".into() });
                };
                Ok(ResolvedInput {
                    bytes: Arc::clone(data),
                    metadata: SourceMetadata {
                        name: name.clone(),
                        size: data.len() as u64,
                        ..SourceMetadata::default()
                    },
                })
            })
        }
    }

    struct TextDetector;
    impl FormatDetector for TextDetector {
        fn id(&self) -> &'static str {
            "test.text"
        }
        fn detect<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatHint,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            Box::pin(async { Ok(vec![FormatCandidate::new(InputFormat::Text, 0.8, "test")]) })
        }
    }

    struct NotApplicable;
    impl Converter for NotApplicable {
        fn id(&self) -> &'static str {
            "test.not-applicable"
        }
        fn priority(&self) -> i32 {
            100
        }
        fn supported_formats(&self) -> &'static [InputFormat] {
            &[InputFormat::Text]
        }
        fn probe<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async { Ok(ProbeOutcome::NotApplicable) })
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            Box::pin(async { Err(ConversionError::Internal { detail: "must not run".into() }) })
        }
    }

    struct MatchingConverter(&'static str);
    impl Converter for MatchingConverter {
        fn id(&self) -> &'static str {
            self.0
        }
        fn supported_formats(&self) -> &'static [InputFormat] {
            &[InputFormat::Text]
        }
        fn probe<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async { Ok(ProbeOutcome::Match { confidence: 1.0 }) })
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            let id = self.0;
            Box::pin(async move {
                let mut document = Document::default();
                document.metadata.title = Some(id.into());
                if id == "invalid.converter" {
                    document.schema_version += 1;
                }
                Ok(ConverterOutput { document, ..ConverterOutput::default() })
            })
        }
    }

    struct EmptyRenderer;
    impl MarkdownRenderer for EmptyRenderer {
        fn id(&self) -> &'static str {
            "test.renderer"
        }
        fn render<'a>(
            &'a self,
            _: &'a Document,
            _: &'a [Asset],
            _: &'a ConversionOptions,
        ) -> BoxFuture<'a, Result<String, ConversionError>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn not_applicable_probe_produces_no_converter() {
        let mut builder = EngineBuilder::new();
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(NotApplicable));
        let engine = builder.build().unwrap();
        let request = ConversionRequest::new(InputRef::bytes(b"hello".as_slice(), Some("x.txt")));
        let error = block_on(engine.convert(request)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::NoConverter);
    }

    #[test]
    fn duplicate_component_ids_are_rejected() {
        let mut builder = EngineBuilder::new();
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_source_resolver(Arc::new(BytesResolver));
        assert_eq!(builder.build().err().unwrap().code(), into_markdown_core::ErrorCode::Internal);
    }

    #[test]
    fn stable_id_breaks_equal_confidence_and_priority_ties() {
        let mut builder = EngineBuilder::new().renderer(Arc::new(EmptyRenderer));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(MatchingConverter("z.converter")))
            .register_converter(Arc::new(MatchingConverter("a.converter")));
        let engine = builder.build().unwrap();
        let request = ConversionRequest::new(InputRef::bytes(b"hello".as_slice(), Some("x.txt")));
        let result = block_on(engine.convert(request)).unwrap();
        assert_eq!(result.document.metadata.title.as_deref(), Some("a.converter"));
    }

    #[test]
    fn invalid_converter_ir_is_rejected_before_rendering() {
        let mut builder = EngineBuilder::new().renderer(Arc::new(EmptyRenderer));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(TextDetector))
            .register_converter(Arc::new(MatchingConverter("invalid.converter")));
        let engine = builder.build().unwrap();
        let request = ConversionRequest::new(InputRef::bytes(b"hello".as_slice(), Some("x.txt")));
        let error = block_on(engine.convert(request)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Internal);
        assert!(error.to_string().contains("unsupportedSchemaVersion"));
    }
}
