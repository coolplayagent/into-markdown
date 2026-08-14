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

#[derive(Default)]
struct NativeNested {
    requests: Mutex<Vec<InputFormat>>,
}

impl into_markdown_core::NestedConversionService for NativeNested {
    fn convert<'a>(
        &'a self,
        request: NestedConversionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            let format = request.hint.format.ok_or_else(|| ConversionError::Malformed {
                part: None,
                detail: "manual nested conversion lacks format".into(),
            })?;
            self.requests.lock().unwrap().push(format);
            let candidate = FormatCandidate::explicit(format);
            match format {
                InputFormat::Docx => {
                    crate::DocxConverter
                        .convert(
                            request.input,
                            &candidate,
                            request.options,
                            &Services::default(),
                            context,
                        )
                        .await
                }
                InputFormat::Pptx => {
                    crate::PresentationConverter
                        .convert(
                            request.input,
                            &candidate,
                            request.options,
                            &Services::default(),
                            context,
                        )
                        .await
                }
                InputFormat::Xlsx => {
                    crate::WorkbookConverter
                        .convert(
                            request.input,
                            &candidate,
                            request.options,
                            &Services::default(),
                            context,
                        )
                        .await
                }
                _ => Err(ConversionError::Unsupported {
                    detail: "manual nested conversion received wrong family".into(),
                }),
            }
        })
    }
}

#[test]
fn missing_packaged_runtime_uses_catalog_component_and_install_hint() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let services =
        Services { nested: Some(Arc::new(NativeNested::default())), ..Services::default() };
    let error = futures::executor::block_on(LegacyOfficeConverter::default().convert(
        &input(),
        &FormatCandidate::explicit(InputFormat::Doc),
        &options,
        &services,
        &context,
    ))
    .unwrap_err();
    match error {
        ConversionError::ComponentUnavailable { component, detail } => {
            assert_eq!(component, crate::core_catalog::LEGACY_OFFICE.component);
            assert!(detail.contains(crate::core_catalog::LEGACY_OFFICE.install_hint));
            assert!(detail.contains("cause:"));
        }
        error => panic!("expected stable runtime error, got {error}"),
    }
}

#[test]
#[ignore = "requires an explicitly audited local LibreOffice runtime and DOC/PPT/XLS fixtures"]
fn manual_native_three_families_enter_real_nested_converters() {
    let path = |name: &str| {
        std::path::PathBuf::from(std::env::var_os(name).expect(name)).canonicalize().unwrap()
    };
    let root = path("INTO_MD_LEGACY_OFFICE_ROOT");
    let runtime = LegacyOfficeRuntime::new(into_markdown_legacy_office::RuntimeConfig::new(
        path("INTO_MD_LEGACY_OFFICE_AUTHORITY"),
        root,
        path("INTO_MD_LEGACY_OFFICE_WORKER"),
    ));
    let nested = Arc::new(NativeNested::default());
    for (variable, format) in [
        ("INTO_MD_LEGACY_OFFICE_DOC_FIXTURE", InputFormat::Doc),
        ("INTO_MD_LEGACY_OFFICE_PPT_FIXTURE", InputFormat::Ppt),
        ("INTO_MD_LEGACY_OFFICE_XLS_FIXTURE", InputFormat::Xls),
    ] {
        let bytes = std::fs::read(path(variable)).unwrap();
        let input = ResolvedInput { bytes: Arc::from(bytes), metadata: SourceMetadata::default() };
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::from_mins(1)),
                ..ExecutionOptions::default()
            },
            options.limits.clone(),
        );
        let services = Services { nested: Some(nested.clone()), ..Services::default() };
        futures::executor::block_on(LegacyOfficeConverter::with_runtime(runtime.clone()).convert(
            &input,
            &FormatCandidate::explicit(format),
            &options,
            &services,
            &context,
        ))
        .unwrap();
    }
    assert_eq!(
        *nested.requests.lock().unwrap(),
        [InputFormat::Docx, InputFormat::Pptx, InputFormat::Xlsx]
    );
}
