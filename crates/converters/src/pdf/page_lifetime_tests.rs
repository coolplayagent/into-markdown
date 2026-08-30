use super::{assemble_pdf, stream_object};
use crate::{EmbeddedVisualOcrEnricher, HintFormatDetector, MemorySourceResolver, PdfConverter};
use futures::executor::block_on;
use into_markdown_core::{
    ArtifactSink, AssetStreamInfo, Block, BoundOcrResult, BoxFuture, CancellationToken,
    ConversionError, ConversionOptions, ConversionRequest, ConverterOutput, EnrichmentPlan,
    ExecutionContext, ExecutionOptions, FormatHint, InputFormat, InputRef, OcrEngine,
    OcrEvidenceStage, OcrEvidenceStep, OcrInputIdentity, OcrOutputPlan, OcrRecognition, OcrRegion,
    OcrRequest, OcrResult, OutputEnricher, Services,
};
use into_markdown_engine::{Engine, EngineBuilder};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recognizer(AtomicUsize);

impl OcrEngine for Recognizer {
    fn id(&self) -> &'static str {
        "test.page-ocr"
    }
    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("bound OCR path is required") })
    }
    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new(16 * 1024, 1, 128)
    }
    fn planned_normalized_png_output(
        &self,
        _: u32,
        _: u32,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new(16 * 1024, 1, 128)
    }
    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            let mut yielded = false;
            std::future::poll_fn(|task| {
                if std::mem::replace(&mut yielded, true) {
                    std::task::Poll::Ready(())
                } else {
                    task.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            let ordinal = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            let image = image::load_from_memory(request.image).unwrap();
            let identity = OcrInputIdentity::try_new(
                Sha256::digest(request.image).into(),
                image.width(),
                image.height(),
                0,
            )?;
            Ok(OcrRecognition::Bound(BoundOcrResult::try_new_for_input(
                OcrResult {
                    provider: self.id().into(),
                    regions: vec![OcrRegion {
                        text: format!("recognized body {ordinal}"),
                        polygon: [(0.0, 0.0), (0.8, 0.0), (0.8, 0.8), (0.0, 0.8)],
                        confidence: 0.99,
                    }],
                },
                vec![0.99],
                vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: self.id().into(),
                        model: Some("test-detector".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: self.id().into(),
                        model: Some("test-recognizer".into()),
                    },
                ],
                identity,
            )?))
        })
    }
}

#[derive(Default)]
struct ObservedPages {
    // page number, live source pixels, all converter-credit bytes at entry
    entries: Mutex<Vec<(u32, usize, u64)>>,
    stop_at: Option<u32>,
    cancel: Option<CancellationToken>,
}

impl OutputEnricher for ObservedPages {
    fn id(&self) -> &'static str {
        "builtin.enricher.embedded-visual-ocr"
    }
    fn planned_enrichment_bytes(
        &self,
        output: &ConverterOutput,
        converter: &str,
        format: InputFormat,
        options: &ConversionOptions,
        services: &Services,
        context: &ExecutionContext,
    ) -> Result<EnrichmentPlan, ConversionError> {
        EmbeddedVisualOcrEnricher
            .planned_enrichment_bytes(output, converter, format, options, services, context)
    }
    fn enrich<'a>(
        &'a self,
        output: ConverterOutput,
        converter: &'a str,
        format: InputFormat,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            assert_eq!(
                output.document.blocks.len(),
                1,
                "must consume one page before extracting another"
            );
            let Block::Page { number, .. } = output.document.blocks[0].block else {
                panic!("page")
            };
            if let Block::Page { blocks, .. } = &output.document.blocks[0].block {
                for node in blocks.iter().filter(|node| node.id.0.ends_with("ocr-render")) {
                    let locator = &node.provenance.locator;
                    let bounds = locator.bounds.unwrap();
                    assert_eq!(locator.rotation_degrees, Some(0.0));
                    assert_eq!((bounds.x, bounds.y), (0.0, 0.0));
                    assert_eq!(Some(bounds.width), locator.page_width);
                    assert_eq!(Some(bounds.height), locator.page_height);
                }
            }
            self.entries.lock().unwrap().push((
                number,
                output.assets.iter().map(|asset| asset.bytes.len()).sum(),
                context.reserved_memory_bytes(),
            ));
            if self.stop_at == Some(number) {
                if let Some(cancel) = &self.cancel {
                    cancel.cancel();
                    context.checkpoint()?;
                }
                return Err(ConversionError::Malformed {
                    part: Some("page OCR".into()),
                    detail: "test page failure".into(),
                });
            }
            EmbeddedVisualOcrEnricher
                .enrich(output, converter, format, options, services, context)
                .await
        })
    }
}

fn engine(pages: Arc<ObservedPages>, ocr: Arc<Recognizer>) -> Engine {
    let mut builder = EngineBuilder::new()
        .services(Services { ocr: Some(ocr), ..Services::default() })
        .enricher(pages)
        .renderer(Arc::new(into_markdown_render_markdown::GfmRenderer));
    builder.registry_mut().register_source_resolver(Arc::new(MemorySourceResolver));
    builder.registry_mut().register_format_detector(Arc::new(HintFormatDetector));
    builder.registry_mut().register_converter(Arc::new(PdfConverter::with_runtime_path(
        std::env::var_os("PDFIUM_LIBRARY").expect("PDFIUM_LIBRARY"),
    )));
    builder.build().unwrap()
}

fn request(page_count: u32) -> ConversionRequest {
    let mut options = ConversionOptions::default();
    options.output.asset_mode = into_markdown_core::AssetMode::Omit;
    options.limits.max_total_asset_bytes = 400_000;
    options.limits.max_memory_bytes = 128 * 1024 * 1024;
    ConversionRequest {
        input: InputRef::Bytes {
            data: Arc::from(scanned_pages(page_count)),
            name: Some("pages.pdf".into()),
        },
        hint: FormatHint { format: Some(InputFormat::Pdf), ..FormatHint::default() },
        options,
        execution: ExecutionOptions::default(),
    }
}

fn scanned_pages(count: u32) -> Vec<u8> {
    scanned_page_variants(count, false)
}

fn scanned_page_variants(count: u32, repeated: bool) -> Vec<u8> {
    let kids =
        (0..count).map(|index| format!("{} 0 R", 3 + index * 3)).collect::<Vec<_>>().join(" ");
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {count} >>").into_bytes(),
    ];
    for index in 0..count {
        let page = 3 + index * 3;
        let variant = if repeated { 0 } else { index };
        let rotation = if variant % 2 == 0 { 0 } else { 90 };
        objects.push(format!("<< /Type /Page /Parent 2 0 R /Rotate {rotation} /MediaBox [0 0 100 200] /Resources << /XObject << /Im1 {} 0 R >> >> /Contents {} 0 R >>", page + 2, page + 1).into_bytes());
        objects.push(stream_object("", b"q 100 0 0 200 0 0 cm /Im1 Do Q\n"));
        objects.push(stream_object("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[u8::try_from(variant + 1).unwrap(), 0, 0]));
    }
    assemble_pdf(&objects)
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn collecting_consumes_ocr_pixels_per_page_and_releases_leases() {
    for count in [2, 8] {
        let pages = Arc::new(ObservedPages::default());
        let ocr = Arc::new(Recognizer::default());
        let engine = engine(pages.clone(), ocr.clone());
        let request = request(count);
        let context =
            ExecutionContext::new(ExecutionOptions::default(), request.options.limits.clone());
        super::IMAGE_BITMAP_MATERIALIZATIONS.set(0);
        let result = block_on(engine.convert_with_context(request, context.clone())).unwrap();
        assert_eq!(
            super::IMAGE_BITMAP_MATERIALIZATIONS.get(),
            0,
            "whole-page OCR must not decode embedded images too"
        );
        assert_eq!(result.document.blocks.len(), usize::try_from(count).unwrap());
        for (index, node) in result.document.blocks.iter().enumerate() {
            assert!(
                matches!(node.block, Block::Page { number, .. } if usize::try_from(number).unwrap() == index + 1)
            );
            assert!(
                serde_json::to_string(node).unwrap().contains("recognized body"),
                "each page needs recognized body"
            );
        }
        assert!(result.assets.is_empty());
        assert_eq!(
            result.markdown.matches("recognized body").count(),
            usize::try_from(count).unwrap()
        );
        assert!(
            result.markdown.contains("recognized body"),
            "must retain OCR body, not just page headings"
        );
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "pdf.scannedPage")
                .count(),
            usize::try_from(count).unwrap()
        );
        let entries = pages.entries.lock().unwrap();
        assert_eq!(entries.len(), usize::try_from(count).unwrap());
        assert!(entries.iter().all(|(_, pixels, _)| *pixels < 400_000));
        assert!(entries.iter().map(|(_, pixels, _)| pixels).sum::<usize>() > 400_000);
        assert!(
            entries.last().unwrap().2 < entries[0].2 + 512 * 1024,
            "pixel leases must not accumulate: {entries:?}"
        );
        assert_eq!(ocr.0.load(Ordering::SeqCst), usize::try_from(count).unwrap());
        drop(result);
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }
}

#[derive(Default)]
struct MarkdownSink(Vec<u8>);
impl ArtifactSink for MarkdownSink {
    fn write_markdown(&mut self, bytes: &[u8]) -> Result<(), ConversionError> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn begin_asset(&mut self, _: &AssetStreamInfo) -> Result<(), ConversionError> {
        panic!("OCR-only assets must be released")
    }
    fn write_asset(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        panic!("OCR-only assets must be released")
    }
    fn end_asset(&mut self) -> Result<(), ConversionError> {
        panic!("OCR-only assets must be released")
    }
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn prepared_page_failure_and_cancellation_do_not_publish_partial_output() {
    for cancel in [false, true] {
        let token = CancellationToken::new();
        let pages = Arc::new(ObservedPages {
            stop_at: Some(2),
            cancel: cancel.then(|| token.clone()),
            ..Default::default()
        });
        let engine = engine(pages.clone(), Arc::new(Recognizer::default()));
        let request = request(3);
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            request.options.limits.clone(),
        );
        let mut sink = MarkdownSink::default();
        let prepared = block_on(engine.prepare_into_with_context(
            request,
            context.clone(),
            sink.capabilities(),
        ))
        .unwrap();
        assert!(pages.entries.lock().unwrap().is_empty());
        let error = block_on(engine.execute_prepared_into(prepared, &mut sink)).unwrap_err();
        assert!(if cancel {
            matches!(error, ConversionError::Cancelled)
        } else {
            matches!(error, ConversionError::Malformed { .. })
        });
        assert_eq!(pages.entries.lock().unwrap().len(), 2);
        assert!(sink.0.is_empty());
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn prepared_path_is_single_execution_and_keeps_body_in_page_order() {
    let pages = Arc::new(ObservedPages::default());
    let engine = engine(pages.clone(), Arc::new(Recognizer::default()));
    let request = request(3);
    let context =
        ExecutionContext::new(ExecutionOptions::default(), request.options.limits.clone());
    let mut sink = MarkdownSink::default();
    let prepared =
        block_on(engine.prepare_into_with_context(request, context.clone(), sink.capabilities()))
            .unwrap();
    assert!(pages.entries.lock().unwrap().is_empty());
    let summary = block_on(engine.execute_prepared_into(prepared, &mut sink)).unwrap();
    assert_eq!(
        pages.entries.lock().unwrap().iter().map(|entry| entry.0).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(String::from_utf8(sink.0).unwrap().contains("recognized body"));
    assert_eq!(summary.assets, 0);
    drop(summary);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn permanent_assets_still_obey_document_total_budget() {
    for mode in [into_markdown_core::AssetMode::Extract, into_markdown_core::AssetMode::Embed] {
        let mut request = request(3);
        request.options.output.asset_mode = mode;
        let pages = Arc::new(ObservedPages::default());
        let engine = engine(pages.clone(), Arc::new(Recognizer::default()));
        let context =
            ExecutionContext::new(ExecutionOptions::default(), request.options.limits.clone());
        assert!(matches!(
            block_on(engine.convert_with_context(request, context.clone())),
            Err(ConversionError::ResourceLimit { limit: "max_total_asset_bytes", .. })
        ));
        assert!(pages.entries.lock().unwrap().is_empty());
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn mixed_pdf_modes_yield_runtime_admission_on_one_executor() {
    for mode in [into_markdown_core::AssetMode::Omit, into_markdown_core::AssetMode::Extract] {
        let pages = Arc::new(ObservedPages::default());
        let ocr = Arc::new(Recognizer::default());
        let engine = engine(pages, ocr.clone());
        let scanning = request(1);
        let mut aggregate = request(1);
        aggregate.options.ocr.policy = into_markdown_core::OcrPolicy::Off;
        aggregate.options.output.asset_mode = mode;
        let scanning_context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::from_secs(5)),
                ..ExecutionOptions::default()
            },
            scanning.options.limits.clone(),
        );
        let aggregate_context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::from_secs(5)),
                ..ExecutionOptions::default()
            },
            aggregate.options.limits.clone(),
        );
        // Recognizer yields while owning PDFium. A synchronous permit wait in
        // the aggregate future would prevent the first future from resuming.
        let (scan, plain) = block_on(async {
            futures::join!(
                engine.convert_with_context(scanning, scanning_context.clone()),
                engine.convert_with_context(aggregate, aggregate_context.clone()),
            )
        });
        let scan = scan.unwrap();
        let plain = plain.unwrap();
        assert!(scan.markdown.contains("recognized body"));
        assert_eq!(plain.document.blocks.len(), 1);
        assert_eq!(ocr.0.load(Ordering::SeqCst), 1);
        drop((scan, plain));
        assert_eq!(scanning_context.reserved_memory_bytes(), 0);
        assert_eq!(aggregate_context.reserved_memory_bytes(), 0);
    }
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn identical_scanned_pages_reuse_recognition_without_retaining_pixels() {
    let pages = Arc::new(ObservedPages::default());
    let ocr = Arc::new(Recognizer::default());
    let engine = engine(pages.clone(), ocr.clone());
    let mut request = request(4);
    request.input = InputRef::Bytes {
        data: Arc::from(scanned_page_variants(4, true)),
        name: Some("repeated.pdf".into()),
    };
    let context =
        ExecutionContext::new(ExecutionOptions::default(), request.options.limits.clone());
    let result = block_on(engine.convert_with_context(request, context.clone())).unwrap();
    assert_eq!(ocr.0.load(Ordering::SeqCst), 1);
    assert_eq!(result.markdown.matches("recognized body 1").count(), 4);
    assert_eq!(context.resource_usage().ocr_recognized_regions, 1);
    assert_eq!(context.resource_usage().ocr_recognized_chars, 17);
    assert_eq!(result.document.blocks.len(), 4);
    assert!(result.assets.is_empty());
    for (index, node) in result.document.blocks.iter().enumerate() {
        assert!(
            matches!(node.block, Block::Page { number, .. } if usize::try_from(number).unwrap() == index + 1)
        );
        assert!(serde_json::to_string(node).unwrap().contains("recognized body 1"));
    }
    let entries = pages.entries.lock().unwrap();
    assert_eq!(entries.len(), 4);
    assert!(
        entries.last().unwrap().2 < entries[0].2 + 512 * 1024,
        "cached contributions must not retain source pixels: {entries:?}"
    );
    drop(result);
    assert_eq!(context.reserved_memory_bytes(), 0);
    assert_eq!(context.reserved_temporary_bytes(), 0);
}
