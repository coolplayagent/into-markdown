//! Deterministic registry and conversion pipeline orchestration.

use into_markdown_core::{
    Block, BlockNode, ConversionError, ConversionRequest, ConversionResult, Converter,
    DetectionRequest, DetectionResult, ExecutionContext, ExecutionStage, FormatCandidate,
    FormatDetector, InputFormat, MarkdownRenderer, ProbeOutcome, Services, SourceResolver,
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
        let context = ExecutionContext::new(request.execution, request.options.limits.clone());
        context.report(ExecutionStage::Resolving, None, None, None::<String>)?;
        let mut source = self.resolve_input(&request.input, &request.options, &context).await?;
        measured_input_bytes(source.input(), &request.options)?;
        source.ensure_memory_reservation(&context)?;
        context.report(ExecutionStage::Detecting, None, None, None::<String>)?;
        let candidates = self.detect_formats(source.input(), &request.hint, &context).await?;
        context.report(ExecutionStage::Completed, Some(1), Some(1), None::<String>)?;
        let source_metadata = source.input().metadata.clone();
        // Keep the resolver's source-memory lease through the terminal event.
        drop(source);
        Ok(DetectionResult { source: source_metadata, candidates })
    }

    /// Resolve, detect, select, convert, and render one request.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ConversionError`] from source resolution, detection,
    /// converter selection/conversion, or Markdown rendering.
    #[allow(clippy::too_many_lines)]
    pub async fn convert(
        &self,
        request: ConversionRequest,
    ) -> Result<ConversionResult, ConversionError> {
        let context = ExecutionContext::new(request.execution, request.options.limits.clone());
        context.report(ExecutionStage::Resolving, None, None, None::<String>)?;
        let mut source = self.resolve_input(&request.input, &request.options, &context).await?;
        measured_input_bytes(source.input(), &request.options)?;
        source.ensure_memory_reservation(&context)?;

        context.report(ExecutionStage::Detecting, None, None, None::<String>)?;
        let candidates = self.detect_formats(source.input(), &request.hint, &context).await?;
        if candidates.is_empty() {
            return Err(ConversionError::Unsupported {
                detail: "format detectors produced no candidates".into(),
            });
        }

        context.report(ExecutionStage::Probing, Some(0), None, None::<String>)?;
        let mut attempts = Vec::new();
        for candidate in &candidates {
            for converter in &self.converters {
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

        // A successful probe makes conversion authoritative. Conversion errors
        // are returned immediately rather than being hidden by another parser.
        context.report(ExecutionStage::Converting, None, None, Some(attempt.converter.id()))?;
        let output = context
            .run(attempt.converter.convert(
                source.input(),
                &attempt.candidate,
                &request.options,
                &self.services,
                &context,
            ))
            .await??;
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
        let asset_bytes = output.assets.iter().try_fold(0_u64, |total, asset| {
            let size =
                u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                    limit: "max_asset_bytes",
                    detail: "asset size cannot be represented as u64".into(),
                })?;
            total.checked_add(size).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "asset byte count overflowed".into(),
            })
        })?;
        if asset_bytes > request.options.limits.max_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: format!("{asset_bytes} > {}", request.options.limits.max_asset_bytes),
            });
        }
        let _asset_memory = context.reserve_memory(asset_bytes)?;
        context.report(ExecutionStage::Rendering, None, None, Some(renderer.id()))?;
        let markdown = context
            .run(renderer.render(&output.document, &output.assets, &request.options, &context))
            .await??;
        let markdown_bytes =
            u64::try_from(markdown.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "rendered Markdown size cannot be represented as u64".into(),
            })?;
        let _markdown_memory = context.reserve_memory(markdown_bytes)?;
        let mut provenance = Vec::new();
        collect_provenance(&output.document.blocks, &mut provenance);
        let result = ConversionResult {
            document: output.document,
            markdown,
            assets: output.assets,
            diagnostics: output.diagnostics,
            provenance,
        };
        context.report(ExecutionStage::Completed, Some(1), Some(1), None::<String>)?;
        // Keep the resolver's source-memory lease through conversion,
        // rendering, result assembly, and the terminal event.
        drop(source);
        Ok(result)
    }

    async fn resolve_input(
        &self,
        input: &into_markdown_core::InputRef,
        options: &into_markdown_core::ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<into_markdown_core::ResolvedSource, ConversionError> {
        let resolver =
            self.source_resolvers.iter().find(|resolver| resolver.supports(input)).ok_or_else(
                || ConversionError::Unsupported {
                    detail: "no source resolver accepts the requested input".into(),
                },
            )?;
        context.run(resolver.resolve_accounted(input, options, context)).await?
    }

    async fn detect_formats(
        &self,
        input: &into_markdown_core::ResolvedInput,
        hint: &into_markdown_core::FormatHint,
        context: &ExecutionContext,
    ) -> Result<Vec<FormatCandidate>, ConversionError> {
        let mut best: BTreeMap<InputFormat, FormatCandidate> = BTreeMap::new();
        if let Some(format) = hint.format {
            best.insert(format, FormatCandidate::explicit(format));
        }
        for detector in &self.format_detectors {
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
                                        || candidate.detector_priority
                                            == existing.detector_priority
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
}

fn measured_input_bytes(
    input: &into_markdown_core::ResolvedInput,
    options: &into_markdown_core::ConversionOptions,
) -> Result<u64, ConversionError> {
    let size = u64::try_from(input.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_input_bytes",
        detail: "resolved input size cannot be represented as u64".into(),
    })?;
    if size > options.limits.max_input_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: format!("{size} > {}", options.limits.max_input_bytes),
        });
    }
    Ok(size)
}

struct Attempt {
    converter: Arc<dyn Converter>,
    candidate: FormatCandidate,
    explicit: bool,
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
            _: &'a ExecutionContext,
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
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            Box::pin(async { Ok(vec![FormatCandidate::new(InputFormat::Text, 0.8, "test")]) })
        }
    }

    struct FixedDetector {
        id: &'static str,
        priority: i32,
        format: InputFormat,
        confidence: f32,
    }

    struct MaliciousDetector;

    struct PdfMagicDetector;

    impl FormatDetector for PdfMagicDetector {
        fn id(&self) -> &'static str {
            "test.pdf-magic"
        }
        fn detect<'a>(
            &'a self,
            input: &'a ResolvedInput,
            _: &'a FormatHint,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            Box::pin(async move {
                Ok(input
                    .bytes
                    .starts_with(b"%PDF-")
                    .then(|| FormatCandidate::new(InputFormat::Pdf, 0.99, "PDF magic bytes"))
                    .into_iter()
                    .collect())
            })
        }
    }

    impl FormatDetector for MaliciousDetector {
        fn id(&self) -> &'static str {
            "malicious.detector"
        }
        fn priority(&self) -> i32 {
            i32::MAX
        }
        fn detect<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatHint,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            Box::pin(async {
                let mut nan = FormatCandidate::new(InputFormat::Pdf, 0.5, "malicious NaN");
                nan.confidence = f32::NAN;
                nan.explicit = true;
                let mut infinity = FormatCandidate::new(InputFormat::Html, 0.5, "malicious Inf");
                infinity.confidence = f32::INFINITY;
                infinity.explicit = true;
                let mut oversized =
                    FormatCandidate::new(InputFormat::Markdown, 42.0, "malicious oversized");
                oversized.confidence = 42.0;
                oversized.explicit = true;
                Ok(vec![nan, infinity, oversized])
            })
        }
    }

    impl FormatDetector for FixedDetector {
        fn id(&self) -> &'static str {
            self.id
        }
        fn priority(&self) -> i32 {
            self.priority
        }
        fn detect<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatHint,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            Box::pin(async move {
                Ok(vec![FormatCandidate::new(self.format, self.confidence, "fixed")])
            })
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
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async { Ok(ProbeOutcome::NotApplicable) })
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
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
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async { Ok(ProbeOutcome::Match { confidence: 1.0 }) })
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
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

    struct FormatAwareConverter {
        id: &'static str,
        format: InputFormat,
        probe_confidence: f32,
    }

    impl Converter for FormatAwareConverter {
        fn id(&self) -> &'static str {
            self.id
        }
        fn supported_formats(&self) -> &'static [InputFormat] {
            match self.format {
                InputFormat::Text => &[InputFormat::Text],
                InputFormat::Pdf => &[InputFormat::Pdf],
                _ => &[],
            }
        }
        fn probe<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            Box::pin(async move { Ok(ProbeOutcome::Match { confidence: self.probe_confidence }) })
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            let id = self.id;
            Box::pin(async move {
                let mut document = Document::default();
                document.metadata.title = Some(id.into());
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
            _: &'a ExecutionContext,
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

    #[test]
    fn legacy_trait_object_resolver_uses_default_accounting_adapter() {
        let resolver: Arc<dyn SourceResolver> = Arc::new(BytesResolver);
        let mut builder = EngineBuilder::new();
        builder.registry_mut().register_source_resolver(resolver);
        let engine = builder.build().unwrap();
        let mut request =
            DetectionRequest::new(InputRef::bytes(b"data".as_slice(), Some("data.bin")));
        request.options.limits.max_memory_bytes = 3;

        let error = block_on(engine.detect(request)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
    }

    #[test]
    fn detection_candidates_sort_by_confidence_priority_and_stable_id() {
        let mut builder = EngineBuilder::new();
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(FixedDetector {
                id: "z.detector",
                priority: 20,
                format: InputFormat::Json,
                confidence: 0.8,
            }))
            .register_format_detector(Arc::new(FixedDetector {
                id: "b.detector",
                priority: 30,
                format: InputFormat::Html,
                confidence: 0.8,
            }))
            .register_format_detector(Arc::new(FixedDetector {
                id: "a.detector",
                priority: 30,
                format: InputFormat::Markdown,
                confidence: 0.8,
            }))
            .register_format_detector(Arc::new(FixedDetector {
                id: "low-priority-high-confidence",
                priority: -100,
                format: InputFormat::Pdf,
                confidence: 0.9,
            }));
        let engine = builder.build().unwrap();
        let result =
            block_on(engine.detect(DetectionRequest::new(InputRef::bytes(
                b"data".as_slice(),
                Some("data.bin"),
            ))))
            .unwrap();
        assert_eq!(
            result.candidates.iter().map(|candidate| candidate.format).collect::<Vec<_>>(),
            vec![InputFormat::Pdf, InputFormat::Markdown, InputFormat::Html, InputFormat::Json]
        );
        assert_eq!(result.candidates[1].detector_id, "a.detector");
        assert_eq!(result.candidates[1].detector_priority, 30);
    }

    #[test]
    fn stable_detector_id_selects_equal_candidates_for_one_format() {
        let mut builder = EngineBuilder::new();
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(FixedDetector {
                id: "z.detector",
                priority: 10,
                format: InputFormat::Pdf,
                confidence: 0.9,
            }))
            .register_format_detector(Arc::new(FixedDetector {
                id: "a.detector",
                priority: 10,
                format: InputFormat::Pdf,
                confidence: 0.9,
            }));
        let engine = builder.build().unwrap();
        let result =
            block_on(engine.detect(DetectionRequest::new(InputRef::bytes(
                b"data".as_slice(),
                Some("data.bin"),
            ))))
            .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].detector_id, "a.detector");
    }

    #[test]
    fn detector_cannot_forge_explicit_or_non_finite_confidence() {
        let mut builder = EngineBuilder::new();
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(MaliciousDetector));
        let engine = builder.build().unwrap();
        let mut request =
            DetectionRequest::new(InputRef::bytes(b"data".as_slice(), Some("data.bin")));
        request.hint.format = Some(InputFormat::Json);
        let result = block_on(engine.detect(request)).unwrap();
        assert_eq!(result.candidates[0].format, InputFormat::Json);
        assert!(result.candidates[0].explicit);
        for candidate in &result.candidates[1..] {
            assert!(!candidate.explicit);
            assert!((0.0..=1.0).contains(&candidate.confidence));
        }
        assert!(
            result.candidates.iter().any(|candidate| candidate.format == InputFormat::Markdown
                && (candidate.confidence - 1.0).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn explicit_format_attempt_precedes_higher_combined_inferred_attempt() {
        let mut builder = EngineBuilder::new().renderer(Arc::new(EmptyRenderer));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(BytesResolver))
            .register_format_detector(Arc::new(PdfMagicDetector))
            .register_converter(Arc::new(FormatAwareConverter {
                id: "explicit.text",
                format: InputFormat::Text,
                probe_confidence: 0.5,
            }))
            .register_converter(Arc::new(FormatAwareConverter {
                id: "inferred.pdf",
                format: InputFormat::Pdf,
                probe_confidence: 1.0,
            }));
        let engine = builder.build().unwrap();
        let mut request =
            ConversionRequest::new(InputRef::bytes(b"%PDF-1.7".as_slice(), Some("conflict.pdf")));
        request.hint.format = Some(InputFormat::Text);
        let result = block_on(engine.convert(request)).unwrap();
        assert_eq!(result.document.metadata.title.as_deref(), Some("explicit.text"));
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CancelStage {
        Resolve,
        Detect,
        Probe,
        Convert,
        Render,
    }

    struct CancellingPipeline {
        target: CancelStage,
        token: into_markdown_core::CancellationToken,
    }

    impl CancellingPipeline {
        fn cancel<'a, T: Send + 'a>(&self) -> BoxFuture<'a, Result<T, ConversionError>> {
            let token = self.token.clone();
            Box::pin(async move {
                token.cancel();
                std::future::pending::<Result<T, ConversionError>>().await
            })
        }
    }

    impl SourceResolver for CancellingPipeline {
        fn id(&self) -> &'static str {
            "test.cancelling.source"
        }
        fn supports(&self, _: &InputRef) -> bool {
            true
        }
        fn resolve<'a>(
            &'a self,
            input: &'a InputRef,
            _: &'a ConversionOptions,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
            if self.target == CancelStage::Resolve {
                return self.cancel();
            }
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

    impl FormatDetector for CancellingPipeline {
        fn id(&self) -> &'static str {
            "test.cancelling.detector"
        }
        fn detect<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatHint,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            if self.target == CancelStage::Detect {
                return self.cancel();
            }
            Box::pin(async {
                Ok(vec![FormatCandidate::new(InputFormat::Text, 1.0, "cancellation test")])
            })
        }
    }

    impl Converter for CancellingPipeline {
        fn id(&self) -> &'static str {
            "test.cancelling.converter"
        }
        fn supported_formats(&self) -> &'static [InputFormat] {
            &[InputFormat::Text]
        }
        fn probe<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            if self.target == CancelStage::Probe {
                return self.cancel();
            }
            Box::pin(async { Ok(ProbeOutcome::Match { confidence: 1.0 }) })
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            if self.target == CancelStage::Convert {
                return self.cancel();
            }
            Box::pin(async { Ok(ConverterOutput::default()) })
        }
    }

    impl MarkdownRenderer for CancellingPipeline {
        fn id(&self) -> &'static str {
            "test.cancelling.renderer"
        }
        fn render<'a>(
            &'a self,
            _: &'a Document,
            _: &'a [Asset],
            _: &'a ConversionOptions,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<String, ConversionError>> {
            if self.target == CancelStage::Render {
                return self.cancel();
            }
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[test]
    fn cancellation_propagates_through_every_engine_spi_stage() {
        for stage in [
            CancelStage::Resolve,
            CancelStage::Detect,
            CancelStage::Probe,
            CancelStage::Convert,
            CancelStage::Render,
        ] {
            let token = into_markdown_core::CancellationToken::new();
            let pipeline = Arc::new(CancellingPipeline { target: stage, token: token.clone() });
            let mut builder = EngineBuilder::new().renderer(pipeline.clone());
            builder
                .registry_mut()
                .register_source_resolver(pipeline.clone())
                .register_format_detector(pipeline.clone())
                .register_converter(pipeline);
            let engine = builder.build().unwrap();
            let mut request =
                ConversionRequest::new(InputRef::bytes(b"cancel".as_slice(), Some("cancel.txt")));
            request.execution.cancellation = token;
            let error = block_on(engine.convert(request)).unwrap_err();
            assert_eq!(error.code(), into_markdown_core::ErrorCode::Cancelled);
        }
    }

    #[test]
    fn total_deadline_interrupts_a_pending_resolver() {
        let pipeline = Arc::new(CancellingPipeline {
            target: CancelStage::Resolve,
            token: into_markdown_core::CancellationToken::new(),
        });
        let mut builder = EngineBuilder::new().renderer(pipeline.clone());
        builder.registry_mut().register_source_resolver(pipeline);
        let engine = builder.build().unwrap();
        let mut request = ConversionRequest::new(InputRef::bytes(b"wait".as_slice(), Some("x")));
        request.execution.timeout = Some(std::time::Duration::from_millis(20));
        let error = block_on(engine.convert(request)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Timeout);
    }
}
