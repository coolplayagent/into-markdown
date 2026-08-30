use crate::{EngineBuilder, RegistryBuilder};
use into_markdown_core::{
    ArtifactSink, Asset, AssetId, AssetStreamInfo, Block, BlockNode, BoxFuture, ConversionError,
    ConversionOptions, ConversionOutcome, ConversionRequest, Converter, ConverterOutput, Document,
    ErrorPolicy, ExecutionContext, FormatCandidate, FormatDetector, FormatHint, InputFormat,
    InputRef, MarkdownRenderer, NodeId, ProbeOutcome, Provenance, ProvenanceKind, ResolvedInput,
    ResultContent, Services, SourceContentEvidence, SourceLocator, SourceMetadata, SourceResolver,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct BytesResolver;

impl SourceResolver for BytesResolver {
    fn id(&self) -> &'static str {
        "empty-policy.bytes"
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

struct TextDetector;

impl FormatDetector for TextDetector {
    fn id(&self) -> &'static str {
        "empty-policy.text"
    }

    fn detect<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatHint,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async {
            Ok(vec![FormatCandidate::new(InputFormat::Text, 1.0, "empty-result policy test")])
        })
    }
}

#[derive(Clone, Copy)]
enum OutputKind {
    UnknownEmpty,
    CertifiedEmpty,
    ExternalAssetOnly,
}

struct CountingConverter {
    calls: Arc<AtomicUsize>,
    output: OutputKind,
}

impl Converter for CountingConverter {
    fn id(&self) -> &'static str {
        "empty-policy.converter"
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

    fn planned_output_bytes(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(64 * 1024)
    }

    fn convert<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatCandidate,
        _: &'a ConversionOptions,
        _: &'a Services,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        let calls = Arc::clone(&self.calls);
        let output = self.output;
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(match output {
                OutputKind::UnknownEmpty => ConverterOutput::default(),
                OutputKind::CertifiedEmpty => ConverterOutput::default()
                    .with_source_content_evidence(SourceContentEvidence::Empty),
                OutputKind::ExternalAssetOnly => external_asset_only_output(),
            })
        })
    }
}

fn external_asset_only_output() -> ConverterOutput {
    let id = AssetId("external-image".into());
    ConverterOutput::new(
        Document {
            blocks: vec![BlockNode {
                id: NodeId("image".into()),
                block: Block::Image { asset: id.clone(), alt: None },
                provenance: Provenance {
                    kind: ProvenanceKind::NativeParser,
                    provider: "empty-policy.converter".into(),
                    locator: SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        },
        vec![Asset {
            id,
            filename: Some("external.png".into()),
            media_type: "image/png".into(),
            bytes: Vec::new(),
            external_uri: Some("https://example.invalid/external.png".into()),
        }],
        Vec::new(),
    )
}

struct CountingEmptyRenderer(Arc<AtomicUsize>);

impl MarkdownRenderer for CountingEmptyRenderer {
    fn id(&self) -> &'static str {
        "empty-policy.renderer"
    }

    fn planned_markdown_bytes(
        &self,
        _: &Document,
        _: &[Asset],
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(64 * 1024)
    }

    fn render<'a>(
        &'a self,
        _: &'a Document,
        _: &'a [Asset],
        _: &'a ConversionOptions,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<String, ConversionError>> {
        let calls = Arc::clone(&self.0);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(String::new())
        })
    }
}

#[derive(Default)]
struct RecordingSink {
    markdown_writes: usize,
    asset_starts: usize,
}

impl ArtifactSink for RecordingSink {
    fn write_markdown(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        self.markdown_writes += 1;
        Ok(())
    }

    fn begin_asset(&mut self, _: &AssetStreamInfo) -> Result<(), ConversionError> {
        self.asset_starts += 1;
        Ok(())
    }

    fn write_asset(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        Ok(())
    }

    fn end_asset(&mut self) -> Result<(), ConversionError> {
        Ok(())
    }
}

fn engine(
    output: OutputKind,
    converter_calls: &Arc<AtomicUsize>,
    renderer_calls: &Arc<AtomicUsize>,
) -> crate::Engine {
    let mut registry = RegistryBuilder::new();
    registry
        .register_source_resolver(Arc::new(BytesResolver))
        .register_format_detector(Arc::new(TextDetector))
        .register_converter(Arc::new(CountingConverter {
            calls: Arc::clone(converter_calls),
            output,
        }));
    let mut builder =
        EngineBuilder::new().renderer(Arc::new(CountingEmptyRenderer(Arc::clone(renderer_calls))));
    *builder.registry_mut() = registry;
    builder.build().unwrap()
}

fn request(bytes: &'static [u8]) -> ConversionRequest {
    ConversionRequest::new(InputRef::bytes(bytes, Some("input.txt")))
}

fn block_on<F: Future>(future: F) -> F::Output {
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
fn unusable_empty_result_fails_before_sink_and_never_reexecutes() {
    let converter_calls = Arc::new(AtomicUsize::new(0));
    let renderer_calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(OutputKind::UnknownEmpty, &converter_calls, &renderer_calls);
    let mut sink = RecordingSink::default();

    let error = block_on(engine.convert_into(request(b"visible"), &mut sink)).unwrap_err();

    assert_eq!(error.reason_code(), "emptyContent");
    assert_eq!(sink.markdown_writes, 0);
    assert_eq!(sink.asset_starts, 0);
    assert_eq!(converter_calls.load(Ordering::SeqCst), 1);
    assert_eq!(renderer_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn certified_empty_source_is_complete_with_a_stable_reason() {
    let converter_calls = Arc::new(AtomicUsize::new(0));
    let renderer_calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(OutputKind::CertifiedEmpty, &converter_calls, &renderer_calls);
    let mut sink = RecordingSink::default();

    let summary = block_on(engine.convert_into(request(b""), &mut sink)).unwrap();

    assert_eq!(summary.content().unwrap(), ResultContent::EmptySource);
    assert_eq!(summary.outcome, ConversionOutcome::Complete);
    assert_eq!(summary.reason_code(), Some("emptySource"));
    assert_eq!(sink.markdown_writes, 0);
    assert_eq!(sink.asset_starts, 0);
    assert_eq!(converter_calls.load(Ordering::SeqCst), 1);
    assert_eq!(renderer_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn external_asset_only_result_remains_usable_for_structured_consumers() {
    let converter_calls = Arc::new(AtomicUsize::new(0));
    let renderer_calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(OutputKind::ExternalAssetOnly, &converter_calls, &renderer_calls);
    let mut sink = RecordingSink::default();

    let summary = block_on(engine.convert_into(request(b"external"), &mut sink)).unwrap();

    assert_eq!(summary.content().unwrap(), ResultContent::AssetsOnly);
    assert_eq!(summary.outcome, ConversionOutcome::Complete);
    assert_eq!(summary.reason_code(), Some("assetOnly"));
    assert_eq!(summary.assets, 1);
    assert_eq!(sink.markdown_writes, 0);
    assert_eq!(sink.asset_starts, 1);
}

#[test]
fn empty_content_is_failed_in_best_effort_and_strict_modes() {
    for policy in [ErrorPolicy::BestEffort, ErrorPolicy::Strict] {
        let converter_calls = Arc::new(AtomicUsize::new(0));
        let renderer_calls = Arc::new(AtomicUsize::new(0));
        let engine = engine(OutputKind::UnknownEmpty, &converter_calls, &renderer_calls);
        let mut request = request(b"visible");
        request.options.error_policy = policy;

        let error = block_on(engine.convert(request)).unwrap_err();

        assert_eq!(error.reason_code(), "emptyContent");
        assert_eq!(converter_calls.load(Ordering::SeqCst), 1);
        assert_eq!(renderer_calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn recoverable_resume_reuses_conversion_and_reapplies_the_terminal_gate() {
    let converter_calls = Arc::new(AtomicUsize::new(0));
    let renderer_calls = Arc::new(AtomicUsize::new(0));
    let engine = engine(OutputKind::UnknownEmpty, &converter_calls, &renderer_calls);
    let temporary = tempfile::tempdir().unwrap();
    let store = crate::RecoveryStore::open(temporary.path().join("recovery")).unwrap();
    let token = store.create_token().unwrap();

    for _ in 0..2 {
        let error =
            block_on(engine.convert_recoverable(request(b"visible"), &store, &token)).unwrap_err();
        assert_eq!(error.reason_code(), "emptyContent");
    }

    assert_eq!(converter_calls.load(Ordering::SeqCst), 1);
    assert_eq!(renderer_calls.load(Ordering::SeqCst), 2);
}
