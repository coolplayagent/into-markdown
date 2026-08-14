use super::*;
use into_markdown_core::{
    Block, BlockNode, Document, ExecutionOptions, Inline, NodeId, Provenance, ProvenanceKind,
    SourceLocator,
};
use std::sync::Mutex;

struct FakeAdapter;

impl CompatibilityAdapter for FakeAdapter {
    fn normalize(
        &self,
        _: &[u8],
        source: InputFormat,
        _: u64,
        context: &ExecutionContext,
    ) -> Result<AdapterOutput, ConversionError> {
        let bytes = b"PK\x03\x04normalized-package".to_vec().into_boxed_slice();
        let memory = context.reserve_memory(u64::try_from(bytes.len()).unwrap())?;
        Ok(AdapterOutput {
            bytes,
            format: expected_output(source)?,
            version: "26.2.4.2".into(),
            artifact_sha256: "a".repeat(64),
            target: "aarch64-apple-darwin".into(),
            memory,
        })
    }
}

#[derive(Default)]
struct FakeNested {
    requests: Mutex<Vec<InputFormat>>,
}

impl into_markdown_core::NestedConversionService for FakeNested {
    fn convert<'a>(
        &'a self,
        request: NestedConversionRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            assert_eq!(request.excluded_converter_ids, [PROVIDER_ID]);
            assert!(request.input.bytes.starts_with(b"PK"));
            assert!(!request.options.network.enabled);
            let format = request.hint.format.unwrap();
            self.requests.lock().unwrap().push(format);
            let locator = SourceLocator {
                byte_start: Some(7),
                byte_end: Some(12),
                page: (format == InputFormat::Docx).then_some(1),
                slide: (format == InputFormat::Pptx).then_some(2),
                sheet: (format == InputFormat::Xlsx).then(|| "Sheet 1".into()),
                part: Some("word/document.xml".into()),
                ..SourceLocator::default()
            };
            let provenance = Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: "fixture.normalized".into(),
                locator,
                confidence: Some(1.0),
            };
            Ok(ConverterOutput::new(
                Document {
                    blocks: vec![BlockNode {
                        id: NodeId("normalized-1".into()),
                        block: Block::Paragraph(vec![Inline::SourceText {
                            value: "body".into(),
                            marks: Vec::new(),
                            provenance: Box::new(provenance.clone()),
                        }]),
                        provenance,
                    }],
                    ..Document::default()
                },
                Vec::new(),
                Vec::new(),
            ))
        })
    }
}

fn converter() -> LegacyOfficeConverter {
    LegacyOfficeConverter { adapter: Arc::new(FakeAdapter) }
}

fn input() -> ResolvedInput {
    let mut bytes = CFBF_MAGIC.to_vec();
    bytes.extend_from_slice(b"fixture");
    ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata {
            name: Some("source.bin".into()),
            size: 15,
            ..Default::default()
        },
    }
}

#[test]
fn identity_probe_and_non_ole_rejection_are_stable() {
    let converter = converter();
    assert_eq!(converter.id(), PROVIDER_ID);
    assert_eq!(converter.supported_formats(), FORMATS);
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        futures::executor::block_on(converter.probe(
            &input(),
            &FormatCandidate::explicit(InputFormat::Doc),
            &context,
        ))
        .unwrap(),
        ProbeOutcome::Match { .. }
    ));
    let plain = ResolvedInput {
        bytes: Arc::from(b"ordinary text".as_slice()),
        metadata: SourceMetadata::default(),
    };
    assert_eq!(
        futures::executor::block_on(converter.probe(
            &plain,
            &FormatCandidate::explicit(InputFormat::Doc),
            &context,
        ))
        .unwrap(),
        ProbeOutcome::NotApplicable
    );
}

#[test]
fn all_legacy_families_use_same_context_nested_dispatch_and_conservative_provenance() {
    let nested = Arc::new(FakeNested::default());
    for source in FORMATS {
        let converter = converter();
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let services = Services { nested: Some(nested.clone()), ..Services::default() };
        let output = futures::executor::block_on(converter.convert(
            &input(),
            &FormatCandidate::explicit(*source),
            &options,
            &services,
            &context,
        ))
        .unwrap();
        let provenance = &output.document.blocks[0].provenance;
        assert_eq!(provenance.locator.byte_start, None);
        assert_eq!(provenance.locator.byte_end, None);
        assert!(
            provenance
                .locator
                .part
                .as_deref()
                .unwrap()
                .starts_with(&format!("legacy-office/{}/", source.as_str()))
        );
        assert_eq!(output.document.metadata.properties["legacyOffice.runtime.version"], "26.2.4.2");
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
    assert_eq!(
        *nested.requests.lock().unwrap(),
        vec![InputFormat::Docx, InputFormat::Pptx, InputFormat::Xlsx]
    );
}

#[test]
fn nested_service_is_required_before_worker_invocation() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let error = futures::executor::block_on(converter().convert(
        &input(),
        &FormatCandidate::explicit(InputFormat::Doc),
        &options,
        &Services::default(),
        &context,
    ))
    .unwrap_err();
    assert!(matches!(error, ConversionError::ComponentUnavailable { .. }));
    assert_eq!(context.reserved_memory_bytes(), 0);
}
