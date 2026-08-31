use super::*;
use into_markdown_core::{
    Block, BlockNode, BoxFuture, Diagnostic, DiagnosticSeverity, ErrorPolicy, ExecutionOptions,
    Inline, IrErrorCode, MAX_DOCUMENT_NODES, NodeId, OcrPolicy, Provenance, ProvenanceKind,
    ResourceLimits, SourceLocator,
};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    plan: Result<EnrichmentPlan, ConversionError>,
    runtime_error: Option<ConversionError>,
    calls: AtomicUsize,
}

impl OutputEnricher for Fixture {
    fn id(&self) -> &'static str {
        EMBEDDED_OCR
    }

    fn planned_enrichment_bytes(
        &self,
        _: &ConverterOutput,
        _: &str,
        _: InputFormat,
        _: &ConversionOptions,
        _: &Services,
        _: &ExecutionContext,
    ) -> Result<EnrichmentPlan, ConversionError> {
        self.plan.clone()
    }

    fn enrich<'a>(
        &'a self,
        output: ConverterOutput,
        _: &'a str,
        _: InputFormat,
        _: &'a ConversionOptions,
        _: &'a Services,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { self.runtime_error.clone().map_or(Ok(output), Err) })
    }
}

fn fixture(plan: Result<EnrichmentPlan, ConversionError>) -> Fixture {
    Fixture { plan, runtime_error: None, calls: AtomicUsize::new(0) }
}

fn resource(limit: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: "preflight fixture".into() }
}

fn options() -> ConversionOptions {
    let mut options =
        ConversionOptions { error_policy: ErrorPolicy::BestEffort, ..ConversionOptions::default() };
    options.ocr.policy = OcrPolicy::Auto;
    options
}

fn node(id: String, block: Block) -> BlockNode {
    BlockNode {
        id: NodeId(id),
        block,
        provenance: Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "builtin.converter.pdfium".into(),
            locator: SourceLocator::default(),
            confidence: None,
        },
    }
}

fn source() -> ConverterOutput {
    ConverterOutput::new(
        Document {
            blocks: vec![node(
                "native".into(),
                Block::Paragraph(vec![Inline::Text { value: "native body".into(), marks: vec![] }]),
            )],
            ..Document::default()
        },
        vec![],
        vec![],
    )
}

fn enrich(
    fixture: &Fixture,
    options: &ConversionOptions,
    context: &ExecutionContext,
    output: ConverterOutput,
) -> Result<ConverterOutput, ConversionError> {
    let services = Services::default();
    let mut destination = crate::collecting::CollectingArtifactSink::new(context);
    let mut sink = PageEnrichmentSink {
        destination: &mut destination,
        enricher: Some(fixture),
        converter_id: "builtin.converter.pdfium",
        format: InputFormat::Pdf,
        options,
        services: &services,
        context,
    };
    let mut future = std::pin::pin!(sink.enrich_page(output));
    let mut task = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        match future.as_mut().poll(&mut task) {
            std::task::Poll::Ready(result) => return result,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

#[test]
fn auto_preflight_resource_failures_preserve_native_page_without_running_ocr() {
    let options = options();
    for plan in [
        Err(resource("documentNodes")),
        Err(resource("max_memory_bytes")),
        Ok(EnrichmentPlan::Reserve(4097)),
    ] {
        let fixture = fixture(plan);
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 4096, ..ResourceLimits::default() },
        );
        let mut input = source();
        input.document.blocks[0].provenance.provider = "builtin.pdf.layout".into();
        let expected = input.document.clone();
        let output = enrich(&fixture, &options, &context, input).unwrap();
        assert_eq!(output.document, expected);
        assert_eq!(output.diagnostics.len(), 1);
        assert_eq!(output.diagnostics[0].code, "presentation.optionalOcrSkipped");
        assert_eq!(output.diagnostics[0].severity, DiagnosticSeverity::Warning);
        assert!(output.diagnostics[0].message.contains("resource limit exceeded"));
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn scanned_or_bodyless_pages_cannot_skip_required_ocr_preflight() {
    let mut scanned = source();
    scanned.diagnostics.push(Diagnostic {
        code: "pdf.scannedPage".into(),
        severity: DiagnosticSeverity::Warning,
        message: "image coverage requires OCR despite a native page number".into(),
        locator: Some(SourceLocator { page: Some(1), ..SourceLocator::default() }),
    });
    let mut bodyless = source();
    bodyless.document.blocks[0].block =
        Block::Paragraph(vec![Inline::Text { value: "\u{8}\n ".into(), marks: vec![] }]);
    let mut recognized_only = source();
    recognized_only.document.blocks[0].provenance.kind = ProvenanceKind::LocalOcr;
    for output in [scanned, bodyless, recognized_only] {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let fixture = fixture(Err(resource("max_memory_bytes")));
        assert!(matches!(
            enrich(&fixture, &options(), &context, output),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn aggregate_ocr_preflight_keeps_global_and_structure_failures_terminal() {
    for format in [
        InputFormat::Html,
        InputFormat::Docx,
        InputFormat::Pptx,
        InputFormat::Xlsx,
        InputFormat::Odt,
        InputFormat::Ipynb,
        InputFormat::Pdf,
    ] {
        for plan in [
            Err(resource("documentNodes")),
            Err(resource("max_memory_bytes")),
            Ok(EnrichmentPlan::Reserve(4097)),
        ] {
            let fixture = Arc::new(fixture(plan));
            let enrichers: Vec<Arc<dyn OutputEnricher>> = vec![fixture.clone()];
            let context = ExecutionContext::new(
                ExecutionOptions::default(),
                ResourceLimits { max_memory_bytes: 4096, ..ResourceLimits::default() },
            );
            let options = options();
            let services = Services::default();
            let mut future = std::pin::pin!(crate::invoke_enrichers(
                &enrichers,
                source(),
                "native.converter",
                format,
                &options,
                &services,
                &context
            ));
            let mut task = std::task::Context::from_waker(std::task::Waker::noop());
            let result = loop {
                if let std::task::Poll::Ready(result) = future.as_mut().poll(&mut task) {
                    break result;
                }
            };
            assert!(matches!(result, Err(ConversionError::ResourceLimit { .. })), "{format:?}");
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
            assert_eq!(context.reserved_memory_bytes(), 0);
        }
    }
}

#[test]
fn strict_or_required_ocr_keeps_preflight_resource_errors() {
    for (policy, ocr) in
        [(ErrorPolicy::Strict, OcrPolicy::Auto), (ErrorPolicy::BestEffort, OcrPolicy::Always)]
    {
        let mut options = options();
        options.error_policy = policy;
        options.ocr.policy = ocr;
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        for plan in [Err(resource("documentNodes")), Ok(EnrichmentPlan::Reserve(u64::MAX))] {
            let fixture = fixture(plan);
            assert!(matches!(
                enrich(&fixture, &options, &context, source()),
                Err(ConversionError::ResourceLimit { .. })
            ));
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn non_resource_preflight_and_all_runtime_errors_remain_terminal() {
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let options = options();
    for error in [
        ConversionError::Cancelled,
        ConversionError::Timeout,
        ConversionError::Malformed { part: None, detail: "fixture".into() },
        ConversionError::ComponentUnavailable { component: "ocr".into(), detail: "fixture".into() },
    ] {
        let fixture = fixture(Err(error.clone()));
        let actual = enrich(&fixture, &options, &context, source()).unwrap_err();
        assert_eq!(actual.to_string(), error.to_string());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }
    for error in [resource("documentNodes"), ConversionError::Cancelled, ConversionError::Timeout] {
        let mut fixture = fixture(Ok(EnrichmentPlan::Reserve(1)));
        fixture.runtime_error = Some(error.clone());
        let actual = enrich(&fixture, &options, &context, source()).unwrap_err();
        assert_eq!(actual.to_string(), error.to_string());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn skipped_or_empty_ocr_contribution_keeps_the_original_page() {
    let options = options();
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    for (plan, calls) in [(EnrichmentPlan::Skip, 0), (EnrichmentPlan::Reserve(1), 1)] {
        let fixture = fixture(Ok(plan));
        let input = source();
        let expected = input.document.clone();
        let output = enrich(&fixture, &options, &context, input).unwrap();
        assert_eq!(output.document, expected);
        assert!(output.diagnostics.is_empty());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), calls);
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn final_validation_budget_counts_nodes_across_enriched_pages() {
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let options = options();
    let fixture = fixture(Ok(EnrichmentPlan::Skip));
    let mut combined = Document::default();
    for number in 1..=2 {
        let blocks = (0..MAX_DOCUMENT_NODES / 2 - 1)
            .map(|index| node(format!("page-{number}-rule-{index}"), Block::Rule))
            .collect();
        let page = ConverterOutput::new(
            Document {
                blocks: vec![node(format!("page-{number}"), Block::Page { number, blocks })],
                ..Document::default()
            },
            vec![],
            vec![],
        );
        let mut page = enrich(&fixture, &options, &context, page).unwrap();
        estimate_validation_working_set(&page.document, &[], &[]).unwrap();
        combined.blocks.append(&mut page.document.blocks);
    }
    // invoke_native applies this same guard to the complete output, not once
    // per page. The two page containers count toward the exact 100,000 nodes.
    estimate_validation_working_set(&combined, &[], &[]).unwrap();
    combined.validate().unwrap();
    combined.blocks.push(node("one-too-many".into(), Block::Rule));
    assert!(matches!(
        estimate_validation_working_set(&combined, &[], &[]),
        Err(ConversionError::ResourceLimit { limit: "documentNodes", .. })
    ));
    assert_eq!(combined.validate().unwrap_err().code, IrErrorCode::ResourceLimit);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(context.reserved_memory_bytes(), 0);
}
