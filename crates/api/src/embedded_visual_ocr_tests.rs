use super::*;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::Write as _;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

const TEST_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

struct SourceBoundOcr(AtomicUsize);

struct PdfGeometryOcr(AtomicUsize);

struct RemoteEmbeddedOcr(AtomicUsize);

struct DanglingDocxConverter;

impl Converter for DanglingDocxConverter {
    fn id(&self) -> &'static str {
        "test.api.dangling-docx"
    }

    fn priority(&self) -> i32 {
        i32::MAX
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        &[InputFormat::Docx]
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
        Ok(96 * 1024)
    }

    fn convert<'a>(
        &'a self,
        _: &'a ResolvedInput,
        _: &'a FormatCandidate,
        _: &'a ConversionOptions,
        _: &'a Services,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async {
            let provenance = || Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: "test.api.dangling-docx".into(),
                locator: SourceLocator::default(),
                confidence: Some(1.0),
            };
            let blocks = vec![
                BlockNode {
                    id: NodeId("image-first".into()),
                    block: Block::Image { asset: AssetId("z-missing".into()), alt: None },
                    provenance: provenance(),
                },
                BlockNode {
                    id: NodeId("image-second".into()),
                    block: Block::Image {
                        asset: AssetId(format!("a-missing-{}", "x".repeat(64 * 1024))),
                        alt: None,
                    },
                    provenance: provenance(),
                },
            ];
            Ok(ConverterOutput::new(
                Document { blocks, ..Document::default() },
                Vec::new(),
                Vec::new(),
            ))
        })
    }
}

impl OcrEngine for RemoteEmbeddedOcr {
    fn id(&self) -> &'static str {
        "provider.fixture.vision-ocr"
    }

    fn provenance_kind(&self) -> ProvenanceKind {
        ProvenanceKind::AiProvider
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async {
            Ok(OcrResult {
                regions: vec![OcrRegion {
                    text: "remote embedded text".into(),
                    polygon: [(0.0, 0.0); 4],
                    confidence: 0.0,
                }],
                provider: "provider.fixture.vision-ocr".into(),
            })
        })
    }

    fn planned_bound_output(
        &self,
        _: OcrRequest<'_>,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new_with_working(16 * 1024, 16 * 1024, 1, 128)
    }

    fn planned_normalized_png_output(
        &self,
        _: u32,
        _: u32,
        _: &ConversionOptions,
        _: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        OcrOutputPlan::try_new_with_working(16 * 1024, 16 * 1024, 1, 128)
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { self.recognize(request, context).await.map(OcrRecognition::Remote) })
    }
}

#[test]
fn best_effort_auto_keeps_low_memory_dangling_reference_as_a_hard_error() {
    let mut builder = default_engine_builder();
    builder.registry_mut().register_converter(Arc::new(DanglingDocxConverter));
    let engine = builder.build().unwrap();
    let mut request =
        ConversionRequest::new(InputRef::bytes(b"fixture".to_vec(), Some("dangling.docx")));
    request.hint.format = Some(InputFormat::Docx);
    request.options.error_policy = ErrorPolicy::BestEffort;
    request.options.ocr.policy = OcrPolicy::Auto;
    request.options.output.asset_mode = AssetMode::Omit;
    request.options.limits.max_memory_bytes = 100 * 1024;

    let error = block_on(engine.convert(request)).unwrap_err();
    assert!(
        matches!(
            &error,
            ConversionError::Internal { detail }
                if detail == "image node references missing asset z-missing"
        ),
        "unexpected engine error: {error:?}"
    );
}

impl OcrEngine for PdfGeometryOcr {
    fn id(&self) -> &'static str {
        "test.api.pdf-geometry-ocr"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("embedded visuals require bound OCR") })
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
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let dimensions = normalized_png_dimensions(request.image);
        let digest = Sha256::digest(request.image).into();
        Box::pin(async move {
            context.checkpoint()?;
            let (width, height) = dimensions?;
            let is_page_render = width > 10 && height > 10;
            let regions = (!is_page_render)
                .then(|| OcrRegion {
                    text: "embedded second".into(),
                    polygon: [
                        (0.0, 0.0),
                        (width as f32, 0.0),
                        (width as f32, height as f32),
                        (0.0, height as f32),
                    ],
                    confidence: 0.99,
                })
                .into_iter()
                .collect();
            let confidences = if is_page_render { Vec::new() } else { vec![0.99] };
            let identity = OcrInputIdentity::try_new(digest, width, height, 0);
            Ok(OcrRecognition::Bound(BoundOcrResult::try_new_for_input(
                OcrResult { regions, provider: "test.api.pdf-geometry-ocr".into() },
                confidences,
                vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: "test.api.pdf-geometry-ocr".into(),
                        model: Some("source-bound-detector".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: "test.api.pdf-geometry-ocr".into(),
                        model: Some("source-bound-recognizer".into()),
                    },
                ],
                identity?,
            )?))
        })
    }
}

fn normalized_png_dimensions(bytes: &[u8]) -> Result<(u32, u32), ConversionError> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(ConversionError::Ocr {
            provider: "test.api.pdf-geometry-ocr".into(),
            detail: "test provider expected normalized PNG input".into(),
        });
    }
    Ok((
        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
    ))
}

impl OcrEngine for SourceBoundOcr {
    fn id(&self) -> &'static str {
        "test.api.embedded-source-bound-ocr"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async { unreachable!("embedded visuals require bound OCR") })
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
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let no_text = image::load_from_memory(request.image)
            .is_ok_and(|image| image.to_rgba8().pixels().all(|pixel| pixel.0[..3] == [255, 0, 0]));
        let identity = OcrInputIdentity::try_new(Sha256::digest(request.image).into(), 1, 1, 0);
        Box::pin(async move {
            context.checkpoint()?;
            Ok(OcrRecognition::Bound(BoundOcrResult::try_new_for_input(
                OcrResult {
                    regions: if no_text {
                        Vec::new()
                    } else {
                        vec![OcrRegion {
                            text: "text from embedded picture".into(),
                            polygon: [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
                            confidence: 0.99,
                        }]
                    },
                    provider: "test.api.recognizer".into(),
                },
                if no_text { Vec::new() } else { vec![0.99] },
                vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: "test.api.detector".into(),
                        model: Some("detector-sha256".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: "test.api.recognizer".into(),
                        model: Some("recognizer-sha256".into()),
                    },
                ],
                identity?,
            )?))
        })
    }
}

#[test]
fn dynamically_created_docx_obeys_embedded_visual_ocr_and_asset_modes() {
    let mut file = tempfile::Builder::new().suffix(".docx").tempfile().unwrap();
    file.write_all(&docx_with_png()).unwrap();
    file.flush().unwrap();

    let ocr = Arc::new(SourceBoundOcr(AtomicUsize::new(0)));
    for mode in [AssetMode::Extract, AssetMode::Embed, AssetMode::Omit] {
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let engine = default_engine_with_services(services).unwrap();
        let mut request = ConversionRequest::new(InputRef::Path(file.path().to_path_buf()));
        request.options.ocr.policy = OcrPolicy::Always;
        request.options.output.asset_mode = mode;
        let result = block_on(engine.convert(request)).unwrap();
        assert_eq!(result.markdown.matches("text from embedded picture").count(), 3);
        match mode {
            AssetMode::Extract => {
                assert!(result.markdown.contains("!["));
                assert!(!result.markdown.contains("data:image/png;base64,"));
            }
            AssetMode::Embed => assert!(result.markdown.contains("data:image/png;base64,")),
            AssetMode::Omit => assert!(!result.markdown.contains("![")),
        }
    }
    assert_eq!(ocr.0.load(Ordering::SeqCst), 6);

    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    let engine = default_engine_with_services(services).unwrap();
    let mut request = ConversionRequest::new(InputRef::Path(file.path().to_path_buf()));
    request.options.ocr.policy = OcrPolicy::Off;
    request.options.output.asset_mode = AssetMode::Omit;
    let result = block_on(engine.convert(request)).unwrap();
    assert!(!result.markdown.contains("text from embedded picture"));
    assert_eq!(ocr.0.load(Ordering::SeqCst), 6);
}

#[test]
fn remote_vision_ocr_enriches_container_images_when_local_ocr_is_off() {
    let mut file = tempfile::Builder::new().suffix(".docx").tempfile().unwrap();
    file.write_all(&docx_with_png()).unwrap();
    file.flush().unwrap();
    let ocr = Arc::new(RemoteEmbeddedOcr(AtomicUsize::new(0)));
    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    let engine = default_engine_with_services(services).unwrap();
    let mut request = ConversionRequest::new(InputRef::Path(file.path().to_path_buf()));
    request.options.ocr.policy = OcrPolicy::Off;
    request.options.ai.vision_ocr = AiMode::Only;
    request.options.output.asset_mode = AssetMode::Omit;
    let result = block_on(engine.convert(request)).unwrap();
    assert_eq!(result.markdown.matches("remote embedded text").count(), 4);
    assert!(result.provenance.iter().any(|item| {
        item.kind == ProvenanceKind::AiProvider && item.provider == "provider.fixture.vision-ocr"
    }));
    assert_eq!(ocr.0.load(Ordering::SeqCst), 2);
}

#[test]
fn dynamically_created_rtf_and_notebook_images_use_embedded_ocr() {
    let hex_png =
        TEST_PNG.iter().fold(String::with_capacity(TEST_PNG.len() * 2), |mut output, byte| {
            use std::fmt::Write as _;
            write!(output, "{byte:02x}").unwrap();
            output
        });
    let rtf = format!("{{\\rtf1\\ansi before\\par{{\\pict\\pngblip {hex_png}}}after\\par}}");
    assert_dynamic_file_ocr(".rtf", rtf.as_bytes());

    let notebook = serde_json::json!({
        "cells": [{
            "id": "embedded-image",
            "cell_type": "raw",
            "metadata": {},
            "source": ["before and after"],
            "attachments": {
                "embedded.png": {
                    "image/png": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
                }
            }
        }],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    });
    assert_dynamic_file_ocr(".ipynb", notebook.to_string().as_bytes());

    assert_dynamic_file_ocr(".odt", &odf_with_png("text"));
    assert_dynamic_file_ocr(".ods", &odf_with_png("spreadsheet"));
    assert_dynamic_file_ocr(".odp", &odf_with_png("presentation"));
    assert_dynamic_file_ocr(".epub", &epub_with_png());

    let nested_docx = docx_with_png();
    assert_dynamic_file_ocr_references(
        ".zip",
        &zip_parts(&[("nested/document.docx", &nested_docx)]),
        3,
        2,
    );
}

#[test]
fn dynamically_created_pptx_and_xlsx_deduplicate_bytes_but_preserve_references() {
    assert_dynamic_file_ocr_references(".pptx", &pptx_with_interleaved_picture_references(), 4, 2);
    assert_dynamic_file_ocr_references(".xlsx", &xlsx_with_two_picture_references(), 4, 2);
}

#[test]
fn dynamically_created_html_remote_image_is_not_fetched_or_ocr_guessed() {
    let mut file = tempfile::Builder::new().suffix(".html").tempfile().unwrap();
    file.write_all(
        br#"<!doctype html><p>safe</p><img src="https://example.invalid/secret.png" alt="remote">"#,
    )
    .unwrap();
    file.flush().unwrap();
    let ocr = Arc::new(SourceBoundOcr(AtomicUsize::new(0)));
    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    let engine = default_engine_with_services(services).unwrap();
    let mut request = ConversionRequest::new(InputRef::Path(file.path().to_path_buf()));
    request.options.ocr.policy = OcrPolicy::Always;
    let result = block_on(engine.convert(request)).unwrap();
    assert!(result.markdown.contains("remote"));
    assert!(!result.markdown.contains("text from embedded picture"));
    assert_eq!(ocr.0.load(Ordering::SeqCst), 0);

    assert_dynamic_file_ocr(
        ".html",
        br#"<!doctype html><p>before</p><img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=" alt="local"><p>after</p>"#,
    );
}

#[test]
fn dynamically_created_epub_external_and_traversal_images_never_enter_embedded_ocr() {
    let ocr = Arc::new(SourceBoundOcr(AtomicUsize::new(0)));
    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    let engine = default_engine_with_services(services).unwrap();

    let mut request = ConversionRequest::new(InputRef::bytes(
        epub_with_unsafe_image("https://example.invalid/secret.png", None),
        Some("external-image.epub"),
    ));
    request.options.ocr.policy = OcrPolicy::Always;
    let result = block_on(engine.convert(request)).unwrap();
    assert!(result.markdown.contains("remote image"));
    assert!(!result.markdown.contains("text from embedded picture"));
    assert_eq!(ocr.0.load(Ordering::SeqCst), 0, "external EPUB image must not reach OCR");
    assert!(result.assets.iter().all(|asset| asset.bytes.is_empty()));

    let mut request = ConversionRequest::new(InputRef::bytes(
        epub_with_unsafe_image("../../../escape.png", Some("../../../escape.png")),
        Some("traversal-image.epub"),
    ));
    request.options.ocr.policy = OcrPolicy::Always;
    assert!(matches!(block_on(engine.convert(request)), Err(ConversionError::Malformed { .. })));
    assert_eq!(
        ocr.0.load(Ordering::SeqCst),
        0,
        "path-confined EPUB rejection must happen before embedded OCR"
    );
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn dynamically_created_pdf_merges_native_and_embedded_ocr_geometry_and_deduplicates() {
    assert!(std::env::var_os("PDFIUM_LIBRARY").is_some());
    let ocr = Arc::new(PdfGeometryOcr(AtomicUsize::new(0)));
    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    let engine = default_engine_with_services(services).unwrap();
    let mut request = ConversionRequest::new(InputRef::bytes(
        pdf_with_native_text_and_two_images(),
        Some("dynamic-embedded.pdf"),
    ));
    request.options.ocr.policy = OcrPolicy::Always;
    let result = block_on(engine.convert(request)).unwrap();
    // One request is the PDF page OCR pass. The embedded stage makes one request
    // for red and one for green; the second green drawing reuses that result.
    assert_eq!(ocr.0.load(Ordering::SeqCst), 3);
    assert_eq!(result.markdown.matches("native words").count(), 1);
    assert_eq!(result.markdown.matches("embedded second").count(), 3);
    assert!(
        result.markdown.find("native words").unwrap()
            < result.markdown.find("embedded second").unwrap()
    );
}

fn pdf_with_native_text_and_two_images() -> Vec<u8> {
    let content = pdf_stream(
        "",
        b"BT /F1 12 Tf 10 60 Td (native words) Tj ET\nq 20 0 0 12 10 55 cm /Im1 Do Q\nq 20 0 0 12 10 30 cm /Im2 Do Q\nq 20 0 0 12 10 10 cm /Im3 Do Q\n",
    );
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 5 0 R >> /XObject << /Im1 6 0 R /Im2 7 0 R /Im3 7 0 R >> >> /Contents 4 0 R >>".to_vec(),
        content,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        pdf_stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[255, 0, 0]),
        pdf_stream("/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8", &[0, 255, 0]),
    ];
    assemble_pdf(&objects)
}

fn pdf_stream(dictionary: &str, bytes: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {dictionary} /Length {} >>\nstream\n", bytes.len()).into_bytes();
    object.extend_from_slice(bytes);
    object.extend_from_slice(b"\nendstream");
    object
}

fn assemble_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n%\x80\x80\x80\x80\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn assert_dynamic_file_ocr(suffix: &str, bytes: &[u8]) {
    assert_dynamic_file_ocr_references(suffix, bytes, 1, 1);
}

fn assert_dynamic_file_ocr_references(
    suffix: &str,
    bytes: &[u8],
    references: usize,
    recognized_assets: usize,
) {
    let mut file = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    let ocr = Arc::new(SourceBoundOcr(AtomicUsize::new(0)));
    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    let engine = default_engine_with_services(services).unwrap();
    let mut request = ConversionRequest::new(InputRef::Path(file.path().to_path_buf()));
    request.options.ocr.policy = OcrPolicy::Always;
    request.options.output.asset_mode = AssetMode::Extract;
    let result =
        block_on(engine.convert(request)).unwrap_or_else(|error| panic!("{suffix}: {error}"));
    assert_eq!(
        result.markdown.matches("text from embedded picture").count(),
        references,
        "{suffix}"
    );
    if suffix == ".pptx" {
        assert!(result.markdown.contains("table cell"), "PPTX table content must survive");
        assert!(result.markdown.contains("chart series"), "PPTX chart cache must survive");
        let ocr_positions = result
            .markdown
            .match_indices("text from embedded picture")
            .map(|(offset, _)| offset)
            .collect::<Vec<_>>();
        let middle = result.markdown.find("before middle").unwrap();
        let end = result.markdown.find("before end").unwrap();
        assert!(
            ocr_positions[0] < middle
                && middle < ocr_positions[1]
                && ocr_positions[1] < end
                && end < ocr_positions[2],
            "PPTX OCR must retain start/middle/end text-image reading order"
        );
    }
    assert_eq!(ocr.0.load(Ordering::SeqCst), recognized_assets, "{suffix}");
    let mut ocr_nodes = Vec::new();
    collect_ocr_nodes(&result.document.blocks, &mut ocr_nodes);
    assert_eq!(ocr_nodes.len(), references, "{suffix}: OCR IR reference count");
    let mut xlsx_locators = Vec::new();
    for node in ocr_nodes {
        assert_eq!(node.provenance.kind, ProvenanceKind::LocalOcr, "{suffix}");
        assert_eq!(node.provenance.provider, "test.api.recognizer", "{suffix}");
        assert!(node.provenance.confidence.is_some(), "{suffix}");
        assert!(node.id.0.contains("::ocr::"), "{suffix}");
        if suffix == ".pptx" {
            assert_eq!(node.provenance.locator.slide, Some(1));
            assert_eq!(node.provenance.locator.part.as_deref(), Some("ppt/slides/slide1.xml"));
        } else if suffix == ".xlsx" {
            xlsx_locators.push((
                node.provenance.locator.sheet.as_deref().unwrap().to_owned(),
                node.provenance.locator.part.as_deref().unwrap().to_owned(),
            ));
        }
    }
    if suffix == ".xlsx" {
        assert_eq!(
            xlsx_locators,
            [
                ("Start".into(), "xl/drawings/drawing1.xml".into()),
                ("Start".into(), "xl/drawings/drawing1.xml".into()),
                ("Middle".into(), "xl/drawings/drawing2.xml".into()),
                ("End".into(), "xl/drawings/drawing3.xml".into()),
            ],
            "XLSX OCR locators must retain workbook sheet order"
        );
    }
}

fn collect_ocr_nodes<'a>(nodes: &'a [BlockNode], output: &mut Vec<&'a BlockNode>) {
    for node in nodes {
        if node.provenance.kind == ProvenanceKind::LocalOcr {
            output.push(node);
        }
        match &node.block {
            Block::List { items, .. } => {
                for item in items {
                    collect_ocr_nodes(&item.blocks, output);
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        collect_ocr_nodes(&cell.blocks, output);
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => collect_ocr_nodes(blocks, output),
            _ => {}
        }
    }
}

fn zip_parts(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in parts {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn zip_owned(parts: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in parts {
        zip.start_file(name, options).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn blank_png() -> Vec<u8> {
    let pixels = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels).write_to(&mut cursor, image::ImageFormat::Png).unwrap();
    cursor.into_inner()
}

fn odf_with_png(family: &str) -> Vec<u8> {
    let (media_type, body) = match family {
        "text" => (
            "application/vnd.oasis.opendocument.text",
            "<office:text><text:p>before</text:p><draw:frame><draw:image xlink:type='simple' xlink:href='Pictures/a.png'/></draw:frame><text:p>after</text:p></office:text>",
        ),
        "spreadsheet" => (
            "application/vnd.oasis.opendocument.spreadsheet",
            "<office:spreadsheet><table:table table:name='Data'><table:table-row><table:table-cell><text:p>before</text:p><draw:frame><draw:image xlink:type='simple' xlink:href='Pictures/a.png'/></draw:frame><text:p>after</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet>",
        ),
        "presentation" => (
            "application/vnd.oasis.opendocument.presentation",
            "<office:presentation><draw:page draw:name='Slide 1'><draw:frame><draw:text-box><text:p>before</text:p></draw:text-box></draw:frame><draw:frame><draw:image xlink:type='simple' xlink:href='Pictures/a.png'/></draw:frame><draw:frame><draw:text-box><text:p>after</text:p></draw:text-box></draw:frame></draw:page></office:presentation>",
        ),
        _ => panic!("unsupported ODF family"),
    };
    let content = format!(
        r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:xlink="http://www.w3.org/1999/xlink"><office:body>{body}</office:body></office:document-content>"#
    );
    let manifest = format!(
        r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:media-type="{media_type}"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="Pictures/a.png" manifest:media-type="image/png"/></manifest:manifest>"#
    );
    zip_parts(&[
        ("mimetype", media_type.as_bytes()),
        ("content.xml", content.as_bytes()),
        ("META-INF/manifest.xml", manifest.as_bytes()),
        ("Pictures/a.png", TEST_PNG),
    ])
}

fn epub_with_png() -> Vec<u8> {
    const CONTAINER: &str = r#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="EPUB/package.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    const PACKAGE: &str = r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="book">urn:test:embedded-ocr</dc:identifier><dc:title>Embedded OCR</dc:title><dc:language>en</dc:language><meta property="dcterms:modified">2026-08-17T00:00:00Z</meta></metadata><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="image" href="image.png" media-type="image/png"/></manifest><spine><itemref idref="chapter"/></spine></package>"#;
    const NAV: &str = r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head><body><nav epub:type="toc"><ol><li><a href="chapter.xhtml">Chapter</a></li></ol></nav></body></html>"#;
    const CHAPTER: &str = r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter</title></head><body><p>before</p><img src="image.png" alt="embedded"/><p>after</p></body></html>"#;
    zip_parts(&[
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", CONTAINER.as_bytes()),
        ("EPUB/package.opf", PACKAGE.as_bytes()),
        ("EPUB/nav.xhtml", NAV.as_bytes()),
        ("EPUB/chapter.xhtml", CHAPTER.as_bytes()),
        ("EPUB/image.png", TEST_PNG),
    ])
}

fn epub_with_unsafe_image(source: &str, manifest_image: Option<&str>) -> Vec<u8> {
    const CONTAINER: &str = r#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="EPUB/package.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    const NAV: &str = r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head><body><nav epub:type="toc"><ol><li><a href="chapter.xhtml">Chapter</a></li></ol></nav></body></html>"#;
    let image_item = manifest_image.map_or_else(String::new, |href| {
        format!(r#"<item id="image" href="{href}" media-type="image/png"/>"#)
    });
    let package = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="book">urn:test:unsafe-embedded-ocr</dc:identifier><dc:title>Unsafe Embedded OCR</dc:title><dc:language>en</dc:language><meta property="dcterms:modified">2026-08-17T00:00:00Z</meta></metadata><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>{image_item}</manifest><spine><itemref idref="chapter"/></spine></package>"#
    );
    let chapter = format!(
        r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter</title></head><body><p>safe text</p><img src="{source}" alt="remote image"/></body></html>"#
    );
    zip_owned(vec![
        ("mimetype".into(), b"application/epub+zip".to_vec()),
        ("META-INF/container.xml".into(), CONTAINER.as_bytes().to_vec()),
        ("EPUB/package.opf".into(), package.into_bytes()),
        ("EPUB/nav.xhtml".into(), NAV.as_bytes().to_vec()),
        ("EPUB/chapter.xhtml".into(), chapter.into_bytes()),
    ])
}

fn pptx_with_interleaved_picture_references() -> Vec<u8> {
    const ROOT: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="root" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;
    const SLIDE: &str = r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:cSld><p:spTree><p:pic><p:nvPicPr><p:cNvPr id="2" name="start"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic><p:sp><p:nvSpPr><p:cNvPr id="3" name="before-middle"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>before middle</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:nvPicPr><p:cNvPr id="4" name="middle"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic><p:pic><p:nvPicPr><p:cNvPr id="7" name="blank-no-text"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="blank"/></p:blipFill><p:spPr/></p:pic><p:sp><p:nvSpPr><p:cNvPr id="5" name="before-end"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>before end</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:nvPicPr><p:cNvPr id="6" name="end"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic><p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="8" name="Table"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>table cell</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame><p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="9" name="Chart"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><a:graphic><a:graphicData><c:chart r:id="chart"/></a:graphicData></a:graphic></p:graphicFrame><p:grpSp><p:nvGrpSpPr><p:cNvPr id="10" name="Nested group"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/><a:chOff x="0" y="0"/><a:chExt cx="914400" cy="914400"/></a:xfrm></p:grpSpPr><p:pic><p:nvPicPr><p:cNvPr id="11" name="nested image"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic></p:grpSp></p:spTree></p:cSld></p:sld>"#;
    const SLIDE_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="image" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image.png"/><Relationship Id="blank" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/blank.png"/><Relationship Id="chart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#;
    const CHART: &str = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:barChart><c:ser><c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>chart series</c:v></c:pt></c:strCache></c:strRef></c:tx><c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>42</c:v></c:pt></c:numCache></c:numRef></c:val></c:ser></c:barChart></c:plotArea></c:chart></c:chartSpace>"#;
    let mut overrides = String::new();
    let mut slide_ids = String::new();
    let mut relationships = String::new();
    let mut parts = vec![
        ("_rels/.rels".into(), ROOT.as_bytes().to_vec()),
        ("ppt/media/image.png".into(), TEST_PNG.to_vec()),
        ("ppt/media/blank.png".into(), blank_png()),
        ("ppt/charts/chart1.xml".into(), CHART.as_bytes().to_vec()),
    ];
    for slide in 1..=6 {
        use std::fmt::Write as _;
        write!(overrides, "<Override PartName=\"/ppt/slides/slide{slide}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>").unwrap();
        write!(slide_ids, "<p:sldId id=\"{}\" r:id=\"slide{slide}\"/>", 255 + slide).unwrap();
        write!(relationships, "<Relationship Id=\"slide{slide}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide{slide}.xml\"/>").unwrap();
        let body = if slide == 1 {
            SLIDE.to_owned()
        } else {
            format!(
                r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="normal-{slide}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>normal volume slide {slide}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            )
        };
        parts.push((format!("ppt/slides/slide{slide}.xml"), body.into_bytes()));
        if slide == 1 {
            parts.push(("ppt/slides/_rels/slide1.xml.rels".into(), SLIDE_RELS.as_bytes().to_vec()));
        }
    }
    let types = format!(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>{overrides}</Types>"#
    );
    let presentation = format!(
        r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst>{slide_ids}</p:sldIdLst></p:presentation>"#
    );
    let presentation_rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{relationships}</Relationships>"#
    );
    parts.push(("[Content_Types].xml".into(), types.into_bytes()));
    parts.push(("ppt/presentation.xml".into(), presentation.into_bytes()));
    parts.push(("ppt/_rels/presentation.xml.rels".into(), presentation_rels.into_bytes()));
    zip_owned(parts)
}

fn xlsx_with_two_picture_references() -> Vec<u8> {
    const TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet3.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/><Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/><Override PartName="/xl/drawings/drawing2.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/><Override PartName="/xl/drawings/drawing3.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/></Types>"#;
    const ROOT: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="root" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    const WORKBOOK: &str = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Start" sheetId="1" r:id="sheet1"/><sheet name="Middle" sheetId="2" r:id="sheet2"/><sheet name="End" sheetId="3" r:id="sheet3"/></sheets></workbook>"#;
    const WORKBOOK_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="sheet1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="sheet2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/><Relationship Id="sheet3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet3.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
    const STYLES: &str = r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font/></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf/></cellStyleXfs><cellXfs count="1"><xf/></cellXfs></styleSheet>"#;
    const SHEET: &str = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><dimension ref="C4"/><sheetData/><drawing r:id="drawing"/></worksheet>"#;
    const VOLUME_SHEET: &str = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><dimension ref="A1"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>normal volume sheet</t></is></c></row></sheetData><drawing r:id="drawing"/></worksheet>"#;
    const DRAWING: &str = r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:oneCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:row>0</xdr:row></xdr:from><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="1" name="first"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="image"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor><xdr:oneCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>2</xdr:row></xdr:from><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="3" name="blank-no-text"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="blank"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor><xdr:oneCellAnchor><xdr:from><xdr:col>2</xdr:col><xdr:row>3</xdr:row></xdr:from><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="second"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="image"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#;
    const DRAWING_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="image" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image.png"/><Relationship Id="blank" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/blank.png"/></Relationships>"#;
    const DRAWING_SINGLE: &str = r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:oneCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:row>0</xdr:row></xdr:from><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="1" name="cross-sheet image"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="image"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#;
    const DRAWING_SINGLE_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="image" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image.png"/></Relationships>"#;
    zip_parts(&[
        ("[Content_Types].xml", TYPES.as_bytes()),
        ("_rels/.rels", ROOT.as_bytes()),
        ("xl/workbook.xml", WORKBOOK.as_bytes()),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS.as_bytes()),
        ("xl/styles.xml", STYLES.as_bytes()),
        ("xl/worksheets/sheet1.xml", SHEET.as_bytes()),
        ("xl/worksheets/sheet2.xml", VOLUME_SHEET.as_bytes()),
        ("xl/worksheets/sheet3.xml", VOLUME_SHEET.as_bytes()),
        ("xl/worksheets/_rels/sheet1.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="drawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#),
        ("xl/worksheets/_rels/sheet2.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="drawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing2.xml"/></Relationships>"#),
        ("xl/worksheets/_rels/sheet3.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="drawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing3.xml"/></Relationships>"#),
        ("xl/drawings/drawing1.xml", DRAWING.as_bytes()),
        ("xl/drawings/_rels/drawing1.xml.rels", DRAWING_RELS.as_bytes()),
        ("xl/drawings/drawing2.xml", DRAWING_SINGLE.as_bytes()),
        ("xl/drawings/_rels/drawing2.xml.rels", DRAWING_SINGLE_RELS.as_bytes()),
        ("xl/drawings/drawing3.xml", DRAWING_SINGLE.as_bytes()),
        ("xl/drawings/_rels/drawing3.xml.rels", DRAWING_SINGLE_RELS.as_bytes()),
        ("xl/media/image.png", TEST_PNG),
        ("xl/media/blank.png", blank_png().as_slice()),
    ])
}

fn docx_with_png() -> Vec<u8> {
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    const TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    const ROOT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rDocument" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    const DOCUMENT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image.png"/><Relationship Id="rBlank" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/blank.png"/></Relationships>"#;
    const IMAGE: &str =
        r#"<w:p><w:r><w:drawing><a:blip r:embed="rImage"/></w:drawing></w:r></w:p>"#;
    const BLANK: &str =
        r#"<w:p><w:r><w:drawing><a:blip r:embed="rBlank"/></w:drawing></w:r></w:p>"#;

    let mut document = String::from(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body>"#,
    );
    document.push_str(IMAGE);
    for paragraph in 0..256 {
        use std::fmt::Write as _;
        write!(document, "<w:p><w:r><w:t>normal volume paragraph {paragraph}</w:t></w:r></w:p>")
            .unwrap();
        if paragraph == 127 {
            document.push_str(IMAGE);
            document.push_str(BLANK);
        }
    }
    document.push_str(IMAGE);
    document.push_str("</w:body></w:document>");

    let blank = blank_png();

    let cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in [
        ("[Content_Types].xml", TYPES.as_bytes()),
        ("_rels/.rels", ROOT_RELS.as_bytes()),
        ("word/document.xml", document.as_bytes()),
        ("word/_rels/document.xml.rels", DOCUMENT_RELS.as_bytes()),
        ("word/media/image.png", PNG),
        ("word/media/blank.png", blank.as_slice()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}
