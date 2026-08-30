use crate::{Engine, EngineBuilder};
use into_markdown_core::{
    ArtifactSink, Asset, AssetId, AssetStreamInfo, Block, BlockNode, BoxFuture, ConversionError,
    ConversionOptions, ConversionRequest, Converter, ConverterEventSink, ConverterOutput,
    ConverterStream, ConverterStreamCompletion, ConverterStreamMode, Diagnostic,
    DiagnosticSeverity, Document, ExecutionContext, ExecutionOptions, ExecutionStage,
    FormatCandidate, FormatDetector, FormatHint, InputFormat, InputRef, LocalBoxFuture,
    MarkdownRenderer, NodeId, ProbeOutcome, ProgressEvent, ProgressListener, Provenance,
    ProvenanceKind, ResolvedInput, Services, SourceLocator, SourceMetadata, SourceResolver,
    stream_converter_output,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

struct BytesResolver;

impl SourceResolver for BytesResolver {
    fn id(&self) -> &'static str {
        "test.collecting.bytes"
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
                    size: u64::try_from(data.len()).unwrap_or(u64::MAX),
                    ..SourceMetadata::default()
                },
            })
        })
    }
}

struct Detector;

impl FormatDetector for Detector {
    fn id(&self) -> &'static str {
        "test.collecting.detector"
    }

    fn detect<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatHint,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async {
            Ok(vec![FormatCandidate::new(InputFormat::Text, 1.0, "collecting test")])
        })
    }
}

struct Renderer;

impl MarkdownRenderer for Renderer {
    fn id(&self) -> &'static str {
        "test.collecting.renderer"
    }

    fn planned_markdown_bytes(
        &self,
        _: &Document,
        _: &[Asset],
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(8 * 1024 * 1024)
    }

    fn render<'a>(
        &'a self,
        document: &'a Document,
        _: &'a [Asset],
        _: &'a ConversionOptions,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<String, ConversionError>> {
        Box::pin(async move {
            if document.blocks.len() == 1 {
                Ok("native\n".into())
            } else {
                Ok("x".repeat(4 * 1024 * 1024))
            }
        })
    }
}

struct CountingNative {
    aggregate_calls: Arc<AtomicUsize>,
    native_calls: Arc<AtomicUsize>,
    plan: u64,
    schema_version: u32,
    block_count: usize,
}

impl Converter for CountingNative {
    fn id(&self) -> &'static str {
        "test.collecting.native"
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        &[InputFormat::Text]
    }

    fn stream_support(&self) -> Option<&dyn ConverterStream> {
        Some(self)
    }

    fn probe<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatCandidate,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async { Ok(ProbeOutcome::Match { confidence: 1.0 }) })
    }

    fn planned_output_bytes(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(self.plan)
    }

    fn convert<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatCandidate,
        _: &'a ConversionOptions,
        _: &'a Services,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        self.aggregate_calls.fetch_add(1, Ordering::SeqCst);
        let schema_version = self.schema_version;
        let block_count = self.block_count;
        Box::pin(async move { Ok(output(schema_version, block_count)) })
    }
}

impl ConverterStream for CountingNative {
    fn stream_mode(&self) -> ConverterStreamMode {
        ConverterStreamMode::Native
    }

    fn convert_stream<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatCandidate,
        _: &'a ConversionOptions,
        _: &'a Services,
        _: &'a ExecutionContext,
        sink: &'a mut dyn ConverterEventSink,
    ) -> LocalBoxFuture<'a, Result<ConverterStreamCompletion, ConversionError>> {
        self.native_calls.fetch_add(1, Ordering::SeqCst);
        let schema_version = self.schema_version;
        let block_count = self.block_count;
        Box::pin(async move { stream_converter_output(output(schema_version, block_count), sink) })
    }
}

fn output(schema_version: u32, block_count: usize) -> ConverterOutput {
    ConverterOutput::new(
        Document {
            schema_version,
            blocks: (0..block_count)
                .map(|index| BlockNode {
                    id: NodeId(format!("native-{index}")),
                    block: Block::Rule,
                    provenance: Provenance {
                        kind: ProvenanceKind::NativeParser,
                        provider: "test.collecting.native".into(),
                        locator: SourceLocator::default(),
                        confidence: None,
                    },
                })
                .collect(),
            ..Document::default()
        },
        vec![Asset {
            id: AssetId("asset".into()),
            filename: Some("asset.bin".into()),
            media_type: "application/octet-stream".into(),
            bytes: vec![0, 1, 2, 255],
            external_uri: None,
        }],
        vec![Diagnostic {
            code: "native.diagnostic".into(),
            severity: DiagnosticSeverity::Warning,
            message: "preserved".into(),
            locator: None,
        }],
    )
}

fn engine(converter: CountingNative) -> Engine {
    let mut builder = EngineBuilder::new().renderer(Arc::new(Renderer));
    builder
        .registry_mut()
        .register_source_resolver(Arc::new(BytesResolver))
        .register_format_detector(Arc::new(Detector))
        .register_converter(Arc::new(converter));
    builder.build().unwrap()
}

fn request() -> ConversionRequest {
    ConversionRequest::new(InputRef::bytes(b"native".as_slice(), Some("native.txt")))
}

#[derive(Default)]
struct RecordingSink {
    markdown: Vec<u8>,
    assets: Vec<(AssetStreamInfo, Vec<u8>)>,
    current: Option<(AssetStreamInfo, Vec<u8>)>,
}

impl ArtifactSink for RecordingSink {
    fn write_markdown(&mut self, chunk: &[u8]) -> Result<(), ConversionError> {
        self.markdown.extend_from_slice(chunk);
        Ok(())
    }

    fn begin_asset(&mut self, asset: &AssetStreamInfo) -> Result<(), ConversionError> {
        self.current = Some((asset.clone(), Vec::new()));
        Ok(())
    }

    fn write_asset(&mut self, chunk: &[u8]) -> Result<(), ConversionError> {
        self.current.as_mut().unwrap().1.extend_from_slice(chunk);
        Ok(())
    }

    fn end_asset(&mut self) -> Result<(), ConversionError> {
        self.assets.push(self.current.take().unwrap());
        Ok(())
    }
}

#[test]
fn public_convert_executes_native_once_without_aggregate_fallback() {
    let aggregate_calls = Arc::new(AtomicUsize::new(0));
    let native_calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(CountingNative {
        aggregate_calls: Arc::clone(&aggregate_calls),
        native_calls: Arc::clone(&native_calls),
        plan: 1024 * 1024,
        schema_version: into_markdown_core::DOCUMENT_SCHEMA_VERSION,
        block_count: 1,
    });

    let result = block_on(engine.convert(request())).unwrap();

    assert_eq!(aggregate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(native_calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.markdown.as_bytes(), b"native\n");
    let expected = output(into_markdown_core::DOCUMENT_SCHEMA_VERSION, 1);
    assert_eq!(result.document.blocks, expected.document.blocks);
    assert_eq!(result.assets, expected.assets);
    assert_eq!(result.diagnostics, expected.diagnostics);
}

#[test]
fn production_collecting_uses_one_exact_inventory_for_many_blocks_and_large_markdown() {
    const BLOCKS: usize = 32_768;
    let aggregate_calls = Arc::new(AtomicUsize::new(0));
    let native_calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(CountingNative {
        aggregate_calls: Arc::clone(&aggregate_calls),
        native_calls: Arc::clone(&native_calls),
        plan: 256 * 1024 * 1024,
        schema_version: into_markdown_core::DOCUMENT_SCHEMA_VERSION,
        block_count: BLOCKS,
    });
    let started = std::time::Instant::now();
    let result = block_on(engine.convert(request())).unwrap();
    println!(
        "production collecting: blocks={BLOCKS} markdown={} elapsed_us={}",
        result.markdown.len(),
        started.elapsed().as_micros()
    );
    assert_eq!(aggregate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(native_calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.document.blocks.len(), BLOCKS);
    assert_eq!(result.document.blocks.capacity(), BLOCKS);
    assert_eq!(result.markdown.len(), 4 * 1024 * 1024);
}

#[test]
fn native_admission_has_stable_exact_minus_one_and_releases_on_drop() {
    let expected = output(into_markdown_core::DOCUMENT_SCHEMA_VERSION, 8);
    let retained = into_markdown_core::estimate_retained_output(
        &expected.document,
        &expected.assets,
        &expected.diagnostics,
    )
    .unwrap();
    let validation = into_markdown_core::estimate_validation_working_set(
        &expected.document,
        &expected.assets,
        &expected.diagnostics,
    )
    .unwrap();
    let plan = retained + validation;
    let calls = Arc::new(AtomicUsize::new(0));
    let converter = |calls: Arc<AtomicUsize>| CountingNative {
        aggregate_calls: Arc::new(AtomicUsize::new(0)),
        native_calls: calls,
        plan,
        schema_version: into_markdown_core::DOCUMENT_SCHEMA_VERSION,
        block_count: 8,
    };
    let input = ResolvedInput {
        bytes: Arc::from(&b"native"[..]),
        metadata: SourceMetadata { size: 6, ..SourceMetadata::default() },
    };
    let candidate = FormatCandidate::new(InputFormat::Text, 1.0, "exact boundary");
    let options = ConversionOptions {
        limits: into_markdown_core::ResourceLimits {
            max_memory_bytes: plan - 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let low = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        block_on(crate::stream_execution::invoke_native_collecting(
            &converter(Arc::clone(&calls)),
            &input,
            &candidate,
            &options,
            &Services::default(),
            &low,
        )),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(low.available_memory_bytes(), plan - 1);

    let options = ConversionOptions {
        limits: into_markdown_core::ResourceLimits { max_memory_bytes: plan, ..Default::default() },
        ..Default::default()
    };
    let exact = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let output = block_on(crate::stream_execution::invoke_native_collecting(
        &converter(Arc::clone(&calls)),
        &input,
        &candidate,
        &options,
        &Services::default(),
        &exact,
    ))
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(output.document.blocks.len(), 8);
    drop(output);
    assert_eq!(exact.available_memory_bytes(), plan);
}

#[test]
fn native_collecting_preserves_and_rejects_invalid_document_schema() {
    let aggregate_calls = Arc::new(AtomicUsize::new(0));
    let native_calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(CountingNative {
        aggregate_calls: Arc::clone(&aggregate_calls),
        native_calls: Arc::clone(&native_calls),
        plan: 1024 * 1024,
        schema_version: 999,
        block_count: 1,
    });
    let error = block_on(engine.convert(request())).unwrap_err();
    assert!(matches!(error, ConversionError::Internal { .. }));
    assert!(error.to_string().contains("schemaVersion"));
    assert_eq!(aggregate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(native_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn aggregate_and_streamed_public_bytes_assets_and_diagnostics_match() {
    let aggregate_calls = Arc::new(AtomicUsize::new(0));
    let native_calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(CountingNative {
        aggregate_calls,
        native_calls,
        plan: 1024 * 1024,
        schema_version: into_markdown_core::DOCUMENT_SCHEMA_VERSION,
        block_count: 1,
    });
    let aggregate = block_on(engine.convert(request())).unwrap();
    let mut streamed = RecordingSink::default();
    let summary = block_on(engine.convert_into(request(), &mut streamed)).unwrap();

    assert_eq!(streamed.markdown, aggregate.markdown.as_bytes());
    assert_eq!(streamed.assets.len(), aggregate.assets.len());
    assert_eq!(streamed.assets[0].0.id, aggregate.assets[0].id);
    assert_eq!(streamed.assets[0].1, aggregate.assets[0].bytes);
    assert_eq!(summary.diagnostics, aggregate.diagnostics);
}

struct StageListener(Arc<Mutex<Vec<ExecutionStage>>>);

impl ProgressListener for StageListener {
    fn on_progress(&self, event: ProgressEvent) {
        self.0.lock().unwrap().push(event.stage);
    }
}

#[test]
fn collecting_capacity_failure_never_reports_completed() {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let engine = engine(CountingNative {
        aggregate_calls: Arc::new(AtomicUsize::new(0)),
        native_calls: Arc::new(AtomicUsize::new(0)),
        plan: 0,
        schema_version: into_markdown_core::DOCUMENT_SCHEMA_VERSION,
        block_count: 1,
    });
    let mut request = request();
    request.execution.progress_listener = Some(Arc::new(StageListener(Arc::clone(&stages))));

    assert!(matches!(
        block_on(engine.convert(request)),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    std::thread::sleep(std::time::Duration::from_millis(25));
    assert!(!stages.lock().unwrap().contains(&ExecutionStage::Completed));
}

#[derive(Clone, Copy)]
enum ChunkFailure {
    Markdown,
    BeginAsset,
    AssetBytes,
    EndAsset,
}

struct FailingChunkSink(ChunkFailure);

impl ArtifactSink for FailingChunkSink {
    fn write_markdown(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        if matches!(self.0, ChunkFailure::Markdown) {
            Err(ConversionError::Internal { detail: "Markdown sink failure".into() })
        } else {
            Ok(())
        }
    }

    fn begin_asset(&mut self, _: &AssetStreamInfo) -> Result<(), ConversionError> {
        if matches!(self.0, ChunkFailure::BeginAsset) {
            Err(ConversionError::Internal { detail: "asset begin failure".into() })
        } else {
            Ok(())
        }
    }

    fn write_asset(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        if matches!(self.0, ChunkFailure::AssetBytes) {
            Err(ConversionError::Internal { detail: "asset byte failure".into() })
        } else {
            Ok(())
        }
    }

    fn end_asset(&mut self) -> Result<(), ConversionError> {
        if matches!(self.0, ChunkFailure::EndAsset) {
            Err(ConversionError::Internal { detail: "asset end failure".into() })
        } else {
            Ok(())
        }
    }
}

#[test]
fn every_chunk_sink_terminal_failure_precedes_completed() {
    for failure in [
        ChunkFailure::Markdown,
        ChunkFailure::BeginAsset,
        ChunkFailure::AssetBytes,
        ChunkFailure::EndAsset,
    ] {
        let stages = Arc::new(Mutex::new(Vec::new()));
        let engine = engine(CountingNative {
            aggregate_calls: Arc::new(AtomicUsize::new(0)),
            native_calls: Arc::new(AtomicUsize::new(0)),
            plan: 1024 * 1024,
            schema_version: into_markdown_core::DOCUMENT_SCHEMA_VERSION,
            block_count: 1,
        });
        let mut request = request();
        request.execution.progress_listener = Some(Arc::new(StageListener(Arc::clone(&stages))));
        assert!(matches!(
            block_on(engine.convert_into(request, &mut FailingChunkSink(failure))),
            Err(ConversionError::Internal { .. })
        ));
        std::thread::sleep(std::time::Duration::from_millis(25));
        assert!(!stages.lock().unwrap().contains(&ExecutionStage::Completed));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CancelCallback {
    Markdown,
    BeginAsset,
    AssetBytes,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CallbackCounts {
    markdown: usize,
    begin_asset: usize,
    asset_bytes: usize,
    end_asset: usize,
}

struct CancellingSink {
    at: CancelCallback,
    cancellation: into_markdown_core::CancellationToken,
    counts: CallbackCounts,
}

impl ArtifactSink for CancellingSink {
    fn write_markdown(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        self.counts.markdown += 1;
        if self.at == CancelCallback::Markdown {
            self.cancellation.cancel();
        }
        Ok(())
    }

    fn begin_asset(&mut self, _: &AssetStreamInfo) -> Result<(), ConversionError> {
        self.counts.begin_asset += 1;
        if self.at == CancelCallback::BeginAsset {
            self.cancellation.cancel();
        }
        Ok(())
    }

    fn write_asset(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        self.counts.asset_bytes += 1;
        if self.at == CancelCallback::AssetBytes {
            self.cancellation.cancel();
        }
        Ok(())
    }

    fn end_asset(&mut self) -> Result<(), ConversionError> {
        self.counts.end_asset += 1;
        Ok(())
    }
}

fn assert_callback_cancellation(at: CancelCallback, expected: CallbackCounts) {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let engine = engine(CountingNative {
        aggregate_calls: Arc::new(AtomicUsize::new(0)),
        native_calls: Arc::new(AtomicUsize::new(0)),
        plan: 8 * 1024 * 1024,
        schema_version: into_markdown_core::DOCUMENT_SCHEMA_VERSION,
        block_count: if at == CancelCallback::Markdown { 2 } else { 1 },
    });
    let mut request = request();
    let cancellation = request.execution.cancellation.clone();
    request.execution.progress_listener = Some(Arc::new(StageListener(Arc::clone(&stages))));
    let mut sink = CancellingSink { at, cancellation, counts: CallbackCounts::default() };

    assert!(matches!(
        block_on(engine.convert_into(request, &mut sink)),
        Err(ConversionError::Cancelled)
    ));
    assert_eq!(sink.counts, expected);
    std::thread::sleep(std::time::Duration::from_millis(25));
    assert!(!stages.lock().unwrap().contains(&ExecutionStage::Completed));
}

#[test]
fn cancellation_inside_first_markdown_callback_stops_all_following_writes() {
    assert_callback_cancellation(
        CancelCallback::Markdown,
        CallbackCounts { markdown: 1, ..CallbackCounts::default() },
    );
}

#[test]
fn cancellation_inside_begin_asset_stops_payload_and_finalization() {
    assert_callback_cancellation(
        CancelCallback::BeginAsset,
        CallbackCounts { markdown: 1, begin_asset: 1, ..CallbackCounts::default() },
    );
}

#[test]
fn cancellation_inside_asset_bytes_stops_end_asset() {
    assert_callback_cancellation(
        CancelCallback::AssetBytes,
        CallbackCounts { markdown: 1, begin_asset: 1, asset_bytes: 1, ..CallbackCounts::default() },
    );
}
