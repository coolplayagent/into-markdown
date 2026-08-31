use super::*;
use into_markdown_core::ErrorPolicy;

struct MixedOcr {
    failure: ConversionError,
    refused: AtomicUsize,
    success: SourceBoundOcr,
}

impl OcrEngine for MixedOcr {
    fn id(&self) -> &'static str {
        "test.ocr.mixed"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("bound recognition required") })
    }

    fn planned_bound_output(
        &self,
        request: OcrRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        self.success.planned_bound_output(request, options, context)
    }

    fn planned_normalized_png_output(
        &self,
        width: u32,
        height: u32,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        self.success.planned_normalized_png_output(width, height, options, context)
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            if image::load_from_memory(request.image).unwrap().width() == 3 {
                self.refused.fetch_add(1, Ordering::SeqCst);
                let _memory = context.reserve_memory(256)?;
                let mut temporary = context.temporary_file("ocr-refusal")?;
                temporary.write_all_checked(b"transient")?;
                return Err(self.failure.clone());
            }
            self.success.recognize_bound(request, context).await
        })
    }
}

fn engine(failure: ConversionError) -> Arc<MixedOcr> {
    Arc::new(MixedOcr {
        failure,
        refused: AtomicUsize::new(0),
        success: SourceBoundOcr {
            calls: AtomicUsize::new(0),
            plans: AtomicUsize::new(0),
            planned_bytes: 16 * 1024,
            planned_working_bytes: 0,
            corrupt_identity: false,
        },
    })
}

fn refusal() -> ConversionError {
    ConversionError::OcrRecognitionMemory {
        provider: "test.ocr.mixed".into(),
        detail: "private 1 MiB quota".into(),
    }
}

fn mixed_output(native: bool) -> ConverterOutput {
    let mut source = output();
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 2, Rgba([10, 20, 30, 255])))
        .write_to(&mut encoded, ImageFormat::Png)
        .unwrap();
    source.assets.push(Asset {
        id: AssetId("asset-c".into()),
        filename: Some("c.png".into()),
        media_type: "image/png".into(),
        bytes: encoded.into_inner(),
        external_uri: None,
    });
    let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else { unreachable!() };
    blocks.push(image_node("image-a-again", "asset-a", "word/media/a.png"));
    blocks.push(image_node("image-c", "asset-c", "word/media/c.png"));
    if native {
        blocks.insert(
            0,
            BlockNode {
                id: NodeId("body".into()),
                block: Block::Paragraph(vec![Inline::Text {
                    value: "native body survives".into(),
                    marks: vec![],
                }]),
                provenance: provenance("word/document.xml"),
            },
        );
    }
    source
}

#[test]
fn optional_refusal_preserves_body_assets_other_ocr_and_every_reference_diagnostic() {
    let engine = engine(refusal());
    let services = Services { ocr: Some(engine.clone()), ..Default::default() };
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let source = mixed_output(true);
    let assets = source.assets.clone();
    let result =
        block_on(enrich(source, InputFormat::Docx, &options, &services, &context)).unwrap();
    assert_eq!(result.assets, assets);
    let json = serde_json::to_string(&result.document).unwrap();
    assert!(json.contains("native body survives"));
    assert!(json.contains("embedded words"));
    assert_eq!(engine.refused.load(Ordering::SeqCst), 1);
    assert_eq!(engine.success.calls.load(Ordering::SeqCst), 1);
    let omitted: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.code == "ocr.optionalRecognitionMemorySkipped")
        .collect();
    assert_eq!(omitted.len(), 3);
    assert!(omitted.iter().all(|d| d.locator.as_ref().is_some_and(|l| l.page == Some(2))));
    drop(result);
    assert_eq!(context.reserved_memory_bytes(), 0);
    assert_eq!(context.reserved_temporary_bytes(), 0);
    // The cache is request-local: another document retries the same byte identity once.
    drop(
        block_on(enrich(mixed_output(true), InputFormat::Docx, &options, &services, &context))
            .unwrap(),
    );
    assert_eq!(engine.refused.load(Ordering::SeqCst), 2);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn strict_forced_and_required_body_refusals_remain_terminal() {
    for (policy, strict, native) in [
        (OcrPolicy::Auto, true, true),
        (OcrPolicy::Always, false, true),
        (OcrPolicy::Auto, false, false),
    ] {
        let engine = engine(refusal());
        let services = Services { ocr: Some(engine), ..Default::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = policy;
        if strict {
            options.error_policy = ErrorPolicy::Strict;
        }
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(
            block_on(enrich(
                mixed_output(native),
                InputFormat::Docx,
                &options,
                &services,
                &context
            ))
            .is_err()
        );
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }
}

#[test]
fn shared_limits_protocol_cancellation_and_timeout_are_never_optional() {
    for failure in [
        ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: "shared".into() },
        ConversionError::ResourceLimit { limit: "provider", detail: "legacy worker".into() },
        ConversionError::ResourceLimit { limit: "providerFrameBytes", detail: "frame".into() },
        ConversionError::ResourceLimit { limit: "recognitionWidth", detail: "width".into() },
        ConversionError::Internal { detail: "protocol".into() },
        ConversionError::Cancelled,
        ConversionError::Timeout,
    ] {
        let expected = failure.code();
        let services = Services { ocr: Some(engine(failure)), ..Default::default() };
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let error =
            block_on(enrich(mixed_output(true), InputFormat::Docx, &options, &services, &context))
                .unwrap_err();
        assert_eq!(error.code(), expected);
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }
}

#[test]
fn text_on_another_page_does_not_authorize_shared_asset_omission() {
    let mut source = mixed_output(true);
    let mut second = output().document.blocks.remove(0);
    second.id = NodeId("another-page".into());
    source.document.blocks.push(second);
    let services = Services { ocr: Some(engine(refusal())), ..Default::default() };
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(block_on(enrich(source, InputFormat::Docx, &options, &services, &context)).is_err());
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn scanned_pdf_page_with_native_footer_still_requires_ocr() {
    let mut source = mixed_output(true);
    let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else { unreachable!() };
    blocks[0].block = Block::Paragraph(vec![Inline::Text { value: "2".into(), marks: vec![] }]);
    source.diagnostics.push(Diagnostic {
        code: "pdf.scannedPage".into(),
        severity: DiagnosticSeverity::Info,
        message: "parser classified image coverage and native text".into(),
        locator: Some(SourceLocator { page: Some(2), ..Default::default() }),
    });
    let services = Services { ocr: Some(engine(refusal())), ..Default::default() };
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let error =
        block_on(enrich(source, InputFormat::Pdf, &options, &services, &context)).unwrap_err();
    assert!(matches!(error, ConversionError::OcrRecognitionMemory { .. }));
    assert_eq!(context.reserved_memory_bytes(), 0);
    assert_eq!(context.reserved_temporary_bytes(), 0);
}
