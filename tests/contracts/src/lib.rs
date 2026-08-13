//! Black-box contracts shared by Cargo and Bazel.

#[cfg(test)]
mod tests {
    use into_markdown::*;
    use std::collections::BTreeSet;
    use std::future::Future;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn resolved(input: &InputRef) -> Result<ResolvedInput, ConversionError> {
        let InputRef::Bytes { data, name } = input else {
            return Err(ConversionError::Unsupported { detail: "bytes required".into() });
        };
        Ok(ResolvedInput {
            bytes: Arc::clone(data),
            metadata: SourceMetadata {
                name: name.clone(),
                size: data.len() as u64,
                ..SourceMetadata::default()
            },
        })
    }

    #[test]
    fn default_engine_converts_text_and_reports_builtin_text_tables() {
        let engine = default_engine().unwrap();
        let mut request = ConversionRequest::new(InputRef::bytes(
            Arc::<[u8]>::from([0xff, 0xfe, b'A', 0, 0x3d, 0xd8, 0x00, 0xde]),
            Some("unicode.txt"),
        ));
        request.hint.charset = Some("UTF_16LE".into());
        let result = block_on(engine.convert(request)).unwrap();
        assert_eq!(result.markdown, "A😀\n");
        assert_eq!(result.provenance[0].locator.byte_start, Some(2));
        assert_eq!(result.provenance[0].locator.byte_end, Some(8));

        let available = planned_formats()
            .iter()
            .filter(|descriptor| descriptor.status == FormatStatus::Available)
            .map(|descriptor| descriptor.format)
            .collect::<Vec<_>>();
        assert_eq!(
            available,
            vec![
                InputFormat::Pdf,
                InputFormat::Docx,
                InputFormat::Rtf,
                InputFormat::Epub,
                InputFormat::Text,
                InputFormat::Markdown,
                InputFormat::Html,
                InputFormat::Csv,
                InputFormat::Tsv,
                InputFormat::Json,
                InputFormat::Xml,
                InputFormat::Feed,
                InputFormat::Ipynb,
                InputFormat::Zip,
                InputFormat::OutlookMsg,
            ]
        );

        let notebook = ConversionRequest::new(InputRef::bytes(
            br#"{"nbformat":4,"nbformat_minor":5,"metadata":{"language_info":{"name":"python"}},"cells":[{"id":"code","cell_type":"code","metadata":{},"execution_count":1,"source":"NEVER_EXECUTE()","outputs":[]}]}"#
                .as_slice(),
            Some("safe.ipynb"),
        ));
        let notebook_result = block_on(engine.convert(notebook)).unwrap();
        assert!(notebook_result.markdown.contains("```python\nNEVER_EXECUTE()\n```"));

        let markdown = ConversionRequest::new(InputRef::bytes(
            Arc::<[u8]>::from(b"Heading\n=======\n\n- [x] done\n".as_slice()),
            Some("notes.md"),
        ));
        let result = block_on(engine.convert(markdown)).unwrap();
        assert_eq!(result.markdown, "# Heading\n\n- [x] done\n");
        assert!(result.assets.is_empty());
        assert_eq!(result.provenance[0].locator.byte_start, Some(0));

        let external_image = ConversionRequest::new(InputRef::bytes(
            Arc::<[u8]>::from(b"![diagram](https://cdn.example.com/diagram.png)\n".as_slice()),
            Some("diagram.md"),
        ));
        let image_result = block_on(engine.convert(external_image)).unwrap();
        assert_eq!(image_result.markdown, "![diagram](<https://cdn.example.com/diagram.png>)\n");
        assert_eq!(image_result.assets.len(), 1);
        assert!(image_result.assets[0].bytes.is_empty());
        assert_eq!(
            image_result.assets[0].external_uri.as_deref(),
            Some("https://cdn.example.com/diagram.png")
        );
        let json = ResultDto::json_from_result(&image_result, DtoJsonStyle::Compact).unwrap();
        assert!(json.contains("https://cdn.example.com/diagram.png"));
    }

    #[test]
    fn default_engine_bom_probe_is_safe_but_truncation_remains_authoritative() {
        let engine = default_engine().unwrap();
        let truncated = ConversionRequest::new(InputRef::bytes(
            Arc::<[u8]>::from([0xff, 0xfe, b'A']),
            Some("truncated.txt"),
        ));
        assert_eq!(block_on(engine.convert(truncated)).unwrap_err().code(), ErrorCode::Malformed);

        let disguised = ConversionRequest::new(InputRef::bytes(
            Arc::<[u8]>::from([0xff, 0xfe, 0, 0, 1, 0, 2, 0]),
            Some("disguised.txt"),
        ));
        assert_eq!(block_on(engine.convert(disguised)).unwrap_err().code(), ErrorCode::NoConverter);

        let mut sparse_control = vec![0xef, 0xbb, 0xbf];
        sparse_control.extend(std::iter::repeat_n(b'A', 70 * 1024));
        sparse_control.push(0x01);
        let sparse_control = ConversionRequest::new(InputRef::bytes(
            Arc::<[u8]>::from(sparse_control),
            Some("sparse-control.txt"),
        ));
        assert_eq!(
            block_on(engine.convert(sparse_control)).unwrap_err().code(),
            ErrorCode::NoConverter
        );
    }

    #[derive(Default)]
    struct Resolver;
    impl SourceResolver for Resolver {
        fn id(&self) -> &'static str {
            "contract.resolver"
        }
        fn supports(&self, input: &InputRef) -> bool {
            matches!(input, InputRef::Bytes { .. })
        }
        fn resolve<'a>(
            &'a self,
            input: &'a InputRef,
            _: &'a ConversionOptions,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
            Box::pin(async move {
                context.checkpoint()?;
                resolved(input)
            })
        }
    }

    struct Detector {
        id: &'static str,
        priority: i32,
        format: InputFormat,
        confidence: f32,
    }
    impl FormatDetector for Detector {
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
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            Box::pin(async move {
                context.checkpoint()?;
                Ok(vec![FormatCandidate::new(self.format, self.confidence, "contract")])
            })
        }
    }

    #[derive(Clone, Copy)]
    enum Probe {
        No,
        Match(f32),
        Error,
    }

    struct TestConverter {
        id: &'static str,
        priority: i32,
        probe: Probe,
        format: InputFormat,
        convert_error: bool,
        invalid_ir: bool,
        calls: Arc<AtomicUsize>,
    }
    impl Converter for TestConverter {
        fn id(&self) -> &'static str {
            self.id
        }
        fn priority(&self) -> i32 {
            self.priority
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
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                context.checkpoint()?;
                match self.probe {
                    Probe::No => Ok(ProbeOutcome::NotApplicable),
                    Probe::Match(confidence) => Ok(ProbeOutcome::Match { confidence }),
                    Probe::Error => Err(ConversionError::Malformed {
                        part: Some("probe".into()),
                        detail: "authoritative".into(),
                    }),
                }
            })
        }
        fn planned_output_bytes(
            &self,
            _: &ResolvedInput,
            _: &FormatCandidate,
            _: &ConversionOptions,
            context: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(context.available_memory_bytes())
        }
        fn convert<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatCandidate,
            _: &'a ConversionOptions,
            _: &'a Services,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                context.checkpoint()?;
                if self.convert_error {
                    return Err(ConversionError::Encrypted);
                }
                let mut document = Document::default();
                document.metadata.title = Some(self.id.into());
                if self.invalid_ir {
                    document.schema_version = u32::MAX;
                }
                Ok(ConverterOutput::new(document, Vec::new(), Vec::new()))
            })
        }
    }

    struct Renderer {
        calls: Arc<AtomicUsize>,
    }
    impl MarkdownRenderer for Renderer {
        fn id(&self) -> &'static str {
            "contract.renderer"
        }
        fn planned_markdown_bytes(
            &self,
            _: &Document,
            _: &[Asset],
            _: &ConversionOptions,
            context: &ExecutionContext,
        ) -> Result<u64, ConversionError> {
            Ok(context.available_memory_bytes())
        }
        fn render<'a>(
            &'a self,
            document: &'a Document,
            _: &'a [Asset],
            _: &'a ConversionOptions,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<String, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                context.checkpoint()?;
                Ok(document.metadata.title.clone().unwrap_or_default())
            })
        }
    }

    fn engine(converters: Vec<Arc<dyn Converter>>, renderer_calls: Arc<AtomicUsize>) -> Engine {
        let mut builder =
            EngineBuilder::new().renderer(Arc::new(Renderer { calls: renderer_calls }));
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(Resolver))
            .register_format_detector(Arc::new(Detector {
                id: "contract.detector",
                priority: 0,
                format: InputFormat::Text,
                confidence: 1.0,
            }));
        for converter in converters {
            builder.registry_mut().register_converter(converter);
        }
        builder.build().unwrap()
    }

    fn converter(
        id: &'static str,
        priority: i32,
        probe: Probe,
        convert_error: bool,
        invalid_ir: bool,
    ) -> Arc<TestConverter> {
        Arc::new(TestConverter {
            id,
            priority,
            probe,
            format: InputFormat::Text,
            convert_error,
            invalid_ir,
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn request() -> ConversionRequest {
        ConversionRequest::new(InputRef::bytes(b"contract".as_slice(), Some("input.txt")))
    }

    fn require_send_sync<T: ?Sized + Send + Sync>() {}

    #[test]
    fn every_public_spi_is_an_object_safe_send_sync_trait() {
        require_send_sync::<dyn SourceResolver>();
        require_send_sync::<dyn FormatDetector>();
        require_send_sync::<dyn Converter>();
        require_send_sync::<dyn MarkdownRenderer>();
        require_send_sync::<dyn OcrEngine>();
        require_send_sync::<dyn Transcriber>();
        require_send_sync::<dyn TensorRuntime>();
        require_send_sync::<dyn AiProvider>();
        let _: Option<&dyn SourceResolver> = None;
        let _: Option<&dyn FormatDetector> = None;
        let _: Option<&dyn Converter> = None;
        let _: Option<&dyn MarkdownRenderer> = None;
        let _: Option<&dyn OcrEngine> = None;
        let _: Option<&dyn Transcriber> = None;
        let _: Option<&dyn TensorRuntime> = None;
        let _: Option<&dyn AiProvider> = None;
    }

    #[test]
    fn legacy_resolver_default_accounting_is_polled_and_budgeted() {
        let resolver: Arc<dyn SourceResolver> = Arc::new(Resolver);
        let input = InputRef::bytes(b"four".as_slice(), None::<String>);
        let limits = ResourceLimits { max_memory_bytes: 3, ..ResourceLimits::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let options = ConversionOptions::default();
        let mut output =
            block_on(context.run(resolver.resolve_accounted(&input, &options, &context)))
                .unwrap()
                .unwrap();
        assert_eq!(
            output.ensure_memory_reservation(&context).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn duplicate_ids_are_rejected_for_each_registry_kind() {
        let mut sources = EngineBuilder::new();
        sources
            .registry_mut()
            .register_source_resolver(Arc::new(Resolver))
            .register_source_resolver(Arc::new(Resolver));
        assert_eq!(sources.build().err().unwrap().code(), ErrorCode::Internal);

        let mut detectors = EngineBuilder::new();
        for _ in 0..2 {
            detectors.registry_mut().register_format_detector(Arc::new(Detector {
                id: "same",
                priority: 0,
                format: InputFormat::Text,
                confidence: 1.0,
            }));
        }
        assert_eq!(detectors.build().err().unwrap().code(), ErrorCode::Internal);

        let mut converters = EngineBuilder::new();
        converters.registry_mut().register_converter(converter("same", 0, Probe::No, false, false));
        converters.registry_mut().register_converter(converter("same", 0, Probe::No, false, false));
        assert_eq!(converters.build().err().unwrap().code(), ErrorCode::Internal);
    }

    #[test]
    fn detection_order_is_confidence_priority_id_and_explicit_hint_first() {
        let mut builder = EngineBuilder::new();
        builder.registry_mut().register_source_resolver(Arc::new(Resolver));
        for detector in [
            Detector { id: "z", priority: 3, format: InputFormat::Json, confidence: 0.8 },
            Detector { id: "b", priority: 4, format: InputFormat::Html, confidence: 0.8 },
            Detector { id: "a", priority: 4, format: InputFormat::Markdown, confidence: 0.8 },
            Detector { id: "high", priority: -1, format: InputFormat::Pdf, confidence: 0.9 },
        ] {
            builder.registry_mut().register_format_detector(Arc::new(detector));
        }
        let engine = builder.build().unwrap();
        let mut request = DetectionRequest::new(InputRef::bytes(b"x".as_slice(), Some("x.bin")));
        request.hint.format = Some(InputFormat::Text);
        let result = block_on(engine.detect(request)).unwrap();
        let formats = result.candidates.iter().map(|value| value.format).collect::<Vec<_>>();
        assert_eq!(
            formats,
            [
                InputFormat::Text,
                InputFormat::Pdf,
                InputFormat::Markdown,
                InputFormat::Html,
                InputFormat::Json
            ]
        );
        assert!(result.candidates[0].explicit);
        assert_eq!(result.candidates[2].detector_id, "a");
    }

    #[test]
    fn converter_order_is_confidence_priority_then_stable_id() {
        let a = converter("a", 0, Probe::Match(0.8), false, false);
        let z = converter("z", 0, Probe::Match(0.8), false, false);
        let result =
            block_on(engine(vec![z, a], Arc::new(AtomicUsize::new(0))).convert(request())).unwrap();
        assert_eq!(result.document.metadata.title.as_deref(), Some("a"));

        let high = converter("priority", 10, Probe::Match(0.8), false, false);
        let confidence = converter("confidence", -10, Probe::Match(0.9), false, false);
        let result = block_on(
            engine(vec![high, confidence], Arc::new(AtomicUsize::new(0))).convert(request()),
        )
        .unwrap();
        assert_eq!(result.document.metadata.title.as_deref(), Some("confidence"));
    }

    #[test]
    fn only_not_applicable_falls_through_and_other_errors_keep_their_code() {
        let no = converter("a.no", 100, Probe::No, false, false);
        let yes = converter("b.yes", 0, Probe::Match(1.0), false, false);
        let result =
            block_on(engine(vec![no, yes], Arc::new(AtomicUsize::new(0))).convert(request()))
                .unwrap();
        assert_eq!(result.document.metadata.title.as_deref(), Some("b.yes"));

        let probe_error = converter("a.error", 0, Probe::Error, false, false);
        let untouched = converter("b.untouched", 0, Probe::Match(1.0), false, false);
        let error = block_on(
            engine(vec![probe_error, untouched.clone()], Arc::new(AtomicUsize::new(0)))
                .convert(request()),
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
        assert_eq!(untouched.calls.load(Ordering::SeqCst), 0);

        let convert_error = converter("a.convert-error", 100, Probe::Match(1.0), true, false);
        let fallback = converter("b.fallback", 0, Probe::Match(0.5), false, false);
        let error = block_on(
            engine(vec![convert_error, fallback.clone()], Arc::new(AtomicUsize::new(0)))
                .convert(request()),
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Encrypted);
        assert_eq!(
            fallback.calls.load(Ordering::SeqCst),
            1,
            "fallback may be probed but never converted"
        );
    }

    #[test]
    fn invalid_converter_ir_never_reaches_renderer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let bad = converter("bad", 0, Probe::Match(1.0), false, true);
        let error = block_on(engine(vec![bad], calls.clone()).convert(request())).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    struct ContextSpi;
    impl OcrEngine for ContextSpi {
        fn id(&self) -> &'static str {
            "contract.ocr"
        }
        fn recognize<'a>(
            &'a self,
            _: OcrRequest<'a>,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
            Box::pin(async move {
                context.checkpoint()?;
                context.report(ExecutionStage::Ocr, Some(1), Some(1), Some("contract.ocr"))?;
                let _memory = context.reserve_memory(1)?;
                Ok(OcrResult::default())
            })
        }
    }
    impl Transcriber for ContextSpi {
        fn id(&self) -> &'static str {
            "contract.transcriber"
        }
        fn transcribe<'a>(
            &'a self,
            _: TranscriptionRequest<'a>,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
            Box::pin(async move {
                context.checkpoint()?;
                context.report(
                    ExecutionStage::Ai,
                    Some(1),
                    Some(1),
                    Some("contract.transcriber"),
                )?;
                let _memory = context.reserve_memory(1)?;
                Ok(TranscriptionResult::default())
            })
        }
    }
    impl TensorRuntime for ContextSpi {
        fn id(&self) -> &'static str {
            "contract.tensor"
        }
        fn run<'a>(
            &'a self,
            _: &'a str,
            _: &'a [Tensor],
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
            Box::pin(async move {
                context.checkpoint()?;
                context.report(ExecutionStage::Ocr, Some(1), Some(1), Some("contract.tensor"))?;
                let _memory = context.reserve_memory(1)?;
                Ok(vec![])
            })
        }
    }
    impl AiProvider for ContextSpi {
        fn id(&self) -> &'static str {
            "contract.ai"
        }
        fn capabilities(&self) -> BTreeSet<AiCapability> {
            BTreeSet::new()
        }
        fn execute<'a>(
            &'a self,
            _: AiRequest<'a>,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
            Box::pin(async move {
                context.checkpoint()?;
                context.report(ExecutionStage::Ai, Some(1), Some(1), Some("contract.ai"))?;
                let _memory = context.reserve_memory(1)?;
                Ok(AiOutput::default())
            })
        }
    }

    #[test]
    fn optional_spis_poll_the_supplied_cancelled_context_to_terminal_errors() {
        let token = CancellationToken::new();
        token.cancel();
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let spi = ContextSpi;
        let ocr = block_on(spi.recognize(
            OcrRequest { image: b"", media_type: "image/png", languages: &[] },
            &context,
        ))
        .unwrap_err();
        let speech = block_on(spi.transcribe(
            TranscriptionRequest { media: b"", media_type: "audio/wav", language: None },
            &context,
        ))
        .unwrap_err();
        let tensor = block_on(spi.run("model", &[], &context)).unwrap_err();
        let document = Document::default();
        let ai = block_on(spi.execute(
            AiRequest {
                capability: AiCapability::LayoutRepair,
                input: AiInput::Document(&document),
                prompt: None,
            },
            &context,
        ))
        .unwrap_err();
        assert!([ocr, speech, tensor, ai].iter().all(|error| error.code() == ErrorCode::Cancelled));
    }

    #[test]
    fn optional_spis_enforce_the_supplied_deadline_and_memory_budget() {
        let spi = ContextSpi;
        let document = Document::default();
        let timeout_context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::ZERO),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let timeout = block_on(spi.execute(
            AiRequest {
                capability: AiCapability::LayoutRepair,
                input: AiInput::Document(&document),
                prompt: None,
            },
            &timeout_context,
        ))
        .unwrap_err();
        assert_eq!(timeout.code(), ErrorCode::Timeout);

        let no_memory = ResourceLimits { max_memory_bytes: 0, ..ResourceLimits::default() };
        let ocr_context = ExecutionContext::new(ExecutionOptions::default(), no_memory.clone());
        let speech_context = ExecutionContext::new(ExecutionOptions::default(), no_memory.clone());
        let tensor_context = ExecutionContext::new(ExecutionOptions::default(), no_memory.clone());
        let ai_context = ExecutionContext::new(ExecutionOptions::default(), no_memory);
        let errors = [
            block_on(spi.recognize(
                OcrRequest { image: b"", media_type: "image/png", languages: &[] },
                &ocr_context,
            ))
            .unwrap_err(),
            block_on(spi.transcribe(
                TranscriptionRequest { media: b"", media_type: "audio/wav", language: None },
                &speech_context,
            ))
            .unwrap_err(),
            block_on(spi.run("model", &[], &tensor_context)).unwrap_err(),
            block_on(spi.execute(
                AiRequest {
                    capability: AiCapability::LayoutRepair,
                    input: AiInput::Document(&document),
                    prompt: None,
                },
                &ai_context,
            ))
            .unwrap_err(),
        ];
        assert!(errors.iter().all(|error| error.code() == ErrorCode::ResourceLimit));
    }

    #[derive(Default)]
    struct EventState {
        inner: Mutex<EventStateInner>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct EventStateInner {
        events: Vec<ProgressEvent>,
        terminal_seen: bool,
        listener_dropped: bool,
    }

    struct Events {
        state: Arc<EventState>,
    }

    impl ProgressListener for Events {
        fn on_progress(&self, event: ProgressEvent) {
            let mut inner = lock_unpoisoned(&self.state.inner);
            inner.terminal_seen |= event.stage == ExecutionStage::Completed;
            inner.events.push(event);
            drop(inner);
            self.state.changed.notify_all();
        }
    }

    impl Drop for Events {
        fn drop(&mut self) {
            let mut inner = lock_unpoisoned(&self.state.inner);
            inner.listener_dropped = true;
            drop(inner);
            self.state.changed.notify_all();
        }
    }

    fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_for_terminal_and_release(
        state: &EventState,
        timeout: Duration,
    ) -> Result<Vec<ProgressEvent>, String> {
        let deadline = Instant::now().checked_add(timeout).unwrap_or(Instant::now());
        let mut inner = lock_unpoisoned(&state.inner);
        let remaining = deadline.saturating_duration_since(Instant::now());
        (inner, _) = state
            .changed
            .wait_timeout_while(inner, remaining, |inner| {
                !inner.terminal_seen && !inner.listener_dropped
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !inner.terminal_seen {
            return Err(format!(
                "progress listener ended without Completed; dropped={}, events={:?}",
                inner.listener_dropped, inner.events
            ));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        (inner, _) = state
            .changed
            .wait_timeout_while(inner, remaining, |inner| !inner.listener_dropped)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !inner.listener_dropped {
            return Err(format!(
                "progress listener was not released after Completed; events={:?}",
                inner.events
            ));
        }
        Ok(inner.events.clone())
    }

    #[test]
    fn successful_pipeline_emits_one_terminal_completed_event() {
        let state = Arc::new(EventState::default());
        let listener = Arc::new(Events { state: Arc::clone(&state) });
        let mut request = request();
        request.execution.progress_listener = Some(listener);
        let yes = converter("success", 0, Probe::Match(1.0), false, false);
        block_on(engine(vec![yes], Arc::new(AtomicUsize::new(0))).convert(request)).unwrap();
        let events = wait_for_terminal_and_release(&state, Duration::from_secs(5))
            .unwrap_or_else(|detail| panic!("{detail}"));
        assert_eq!(
            events.iter().filter(|event| event.stage == ExecutionStage::Completed).count(),
            1
        );
        assert_eq!(events.last().map(|event| event.stage), Some(ExecutionStage::Completed));
        assert_eq!(events.last().map(|event| event.basis_points), Some(10_000));
        assert!(events.windows(2).all(|pair| pair[0].basis_points <= pair[1].basis_points));
    }

    #[test]
    fn defaults_are_offline_ai_off_and_ocr_auto_without_io() {
        let options = ConversionOptions::default();
        assert!(!options.network.enabled);
        assert!(options.network.deny_private_networks);
        assert_eq!(options.ocr.policy, OcrPolicy::Auto);
        assert!(options.ocr.model_bundle.is_none());
        assert_eq!(options.ai.vision_ocr, AiMode::Off);
        assert_eq!(options.ai.image_description, AiMode::Off);
        assert_eq!(options.ai.layout_repair, AiMode::Off);
        assert_eq!(options.ai.table_repair, AiMode::Off);
        assert_eq!(options.ai.formula_repair, AiMode::Off);
        assert_eq!(options.ai.audio_transcription, AiMode::Off);
        assert_eq!(options.ai.markdown_postprocess, AiMode::Off);

        let error = block_on(
            default_engine()
                .unwrap()
                .detect(DetectionRequest::new(InputRef::Uri("https://example.invalid/x".into()))),
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Network);
    }

    #[test]
    fn all_conversion_errors_expose_the_stable_code_table() {
        let cases = [
            (ConversionError::Unsupported { detail: "x".into() }, "unsupported"),
            (ConversionError::NoConverter { format: "x".into() }, "noConverter"),
            (ConversionError::Malformed { part: None, detail: "x".into() }, "malformed"),
            (ConversionError::Encrypted, "encrypted"),
            (ConversionError::ResourceLimit { limit: "x", detail: "x".into() }, "resourceLimit"),
            (ConversionError::Ocr { provider: "x".into(), detail: "x".into() }, "ocr"),
            (ConversionError::Ai { provider: "x".into(), detail: "x".into() }, "ai"),
            (ConversionError::Network { detail: "x".into() }, "network"),
            (ConversionError::Io { detail: "x".into() }, "io"),
            (
                ConversionError::ComponentUnavailable { component: "x".into(), detail: "x".into() },
                "componentUnavailable",
            ),
            (ConversionError::Cancelled, "cancelled"),
            (ConversionError::Timeout, "timeout"),
            (ConversionError::Internal { detail: "x".into() }, "internal"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.code().as_str(), expected);
        }
    }

    #[test]
    fn dto_boundary_rejects_over_budget_adversarial_input_without_panicking() {
        let fixture = include_str!("../fixtures/adversarial-dto.json");
        let limits =
            DtoLimits { max_depth: 4, max_json_bytes: fixture.len(), ..DtoLimits::default() };
        let outcome =
            catch_unwind(AssertUnwindSafe(|| ResultDto::from_json_with_limits(fixture, &limits)));
        let error = outcome.expect("controlled DTO decoding must not panic").unwrap_err();
        assert_eq!(error.code, DtoErrorCode::ResourceLimit);
    }

    #[test]
    fn adversarial_public_calls_are_polled_to_ready_without_panicking() {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            block_on(default_engine().unwrap().detect(DetectionRequest::new(InputRef::bytes(
                [0_u8, 0xff, 0, 0x80].as_slice(),
                Some("bad.bin"),
            ))))
        }));
        assert!(outcome.is_ok());
    }
}
