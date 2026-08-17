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

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let identity = OcrInputIdentity::try_new(Sha256::digest(request.image).into(), 1, 1, 0);
        Box::pin(async move {
            context.checkpoint()?;
            Ok(OcrRecognition::Bound(BoundOcrResult::try_new_for_input(
                OcrResult {
                    regions: vec![OcrRegion {
                        text: "text from embedded picture".into(),
                        polygon: [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
                        confidence: 0.99,
                    }],
                    provider: "test.api.recognizer".into(),
                },
                vec![0.99],
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
        assert!(result.markdown.contains("text from embedded picture"));
        match mode {
            AssetMode::Extract => {
                assert!(result.markdown.contains("!["));
                assert!(!result.markdown.contains("data:image/png;base64,"));
            }
            AssetMode::Embed => assert!(result.markdown.contains("data:image/png;base64,")),
            AssetMode::Omit => assert!(!result.markdown.contains("![")),
        }
    }
    assert_eq!(ocr.0.load(Ordering::SeqCst), 3);

    let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
    let engine = default_engine_with_services(services).unwrap();
    let mut request = ConversionRequest::new(InputRef::Path(file.path().to_path_buf()));
    request.options.ocr.policy = OcrPolicy::Off;
    request.options.output.asset_mode = AssetMode::Omit;
    let result = block_on(engine.convert(request)).unwrap();
    assert!(!result.markdown.contains("text from embedded picture"));
    assert_eq!(ocr.0.load(Ordering::SeqCst), 3);
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
    assert_dynamic_file_ocr(".zip", &zip_parts(&[("nested/document.docx", &nested_docx)]));
}

#[test]
fn dynamically_created_pptx_and_xlsx_deduplicate_bytes_but_preserve_references() {
    assert_dynamic_file_ocr_references(".pptx", &pptx_with_interleaved_picture_references(), 3);
    assert_dynamic_file_ocr_references(".xlsx", &xlsx_with_two_picture_references(), 2);
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

fn assert_dynamic_file_ocr(suffix: &str, bytes: &[u8]) {
    assert_dynamic_file_ocr_references(suffix, bytes, 1);
}

fn assert_dynamic_file_ocr_references(suffix: &str, bytes: &[u8], references: usize) {
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
    assert_eq!(ocr.0.load(Ordering::SeqCst), 1, "{suffix}");
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

fn pptx_with_interleaved_picture_references() -> Vec<u8> {
    const TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>"#;
    const ROOT: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="root" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;
    const PRESENTATION: &str = r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="slide"/></p:sldIdLst></p:presentation>"#;
    const PRESENTATION_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="slide" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#;
    const SLIDE: &str = r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:cSld><p:spTree><p:pic><p:nvPicPr><p:cNvPr id="2" name="start"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic><p:sp><p:nvSpPr><p:cNvPr id="3" name="before-middle"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>before middle</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:nvPicPr><p:cNvPr id="4" name="middle"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic><p:sp><p:nvSpPr><p:cNvPr id="5" name="before-end"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>before end</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:nvPicPr><p:cNvPr id="6" name="end"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic></p:spTree></p:cSld></p:sld>"#;
    const SLIDE_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="image" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image.png"/></Relationships>"#;
    zip_parts(&[
        ("[Content_Types].xml", TYPES.as_bytes()),
        ("_rels/.rels", ROOT.as_bytes()),
        ("ppt/presentation.xml", PRESENTATION.as_bytes()),
        ("ppt/_rels/presentation.xml.rels", PRESENTATION_RELS.as_bytes()),
        ("ppt/slides/slide1.xml", SLIDE.as_bytes()),
        ("ppt/slides/_rels/slide1.xml.rels", SLIDE_RELS.as_bytes()),
        ("ppt/media/image.png", TEST_PNG),
    ])
}

fn xlsx_with_two_picture_references() -> Vec<u8> {
    const TYPES: &str = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/><Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/></Types>"#;
    const ROOT: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="root" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    const WORKBOOK: &str = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="sheet"/></sheets></workbook>"#;
    const WORKBOOK_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="sheet" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="styles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
    const STYLES: &str = r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font/></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf/></cellStyleXfs><cellXfs count="1"><xf/></cellXfs></styleSheet>"#;
    const SHEET: &str = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><dimension ref="C4"/><sheetData/><drawing r:id="drawing"/></worksheet>"#;
    const SHEET_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="drawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#;
    const DRAWING: &str = r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:oneCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:row>0</xdr:row></xdr:from><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="1" name="first"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="image"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor><xdr:oneCellAnchor><xdr:from><xdr:col>2</xdr:col><xdr:row>3</xdr:row></xdr:from><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="second"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="image"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#;
    const DRAWING_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="image" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image.png"/></Relationships>"#;
    zip_parts(&[
        ("[Content_Types].xml", TYPES.as_bytes()),
        ("_rels/.rels", ROOT.as_bytes()),
        ("xl/workbook.xml", WORKBOOK.as_bytes()),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS.as_bytes()),
        ("xl/styles.xml", STYLES.as_bytes()),
        ("xl/worksheets/sheet1.xml", SHEET.as_bytes()),
        ("xl/worksheets/_rels/sheet1.xml.rels", SHEET_RELS.as_bytes()),
        ("xl/drawings/drawing1.xml", DRAWING.as_bytes()),
        ("xl/drawings/_rels/drawing1.xml.rels", DRAWING_RELS.as_bytes()),
        ("xl/media/image.png", TEST_PNG),
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
    const DOCUMENT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image.png"/></Relationships>"#;

    let mut document = String::from(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body>"#,
    );
    for paragraph in 0..256 {
        use std::fmt::Write as _;
        write!(document, "<w:p><w:r><w:t>normal volume paragraph {paragraph}</w:t></w:r></w:p>")
            .unwrap();
    }
    document.push_str(
        r#"<w:p><w:r><w:drawing><a:blip r:embed="rImage"/></w:drawing></w:r></w:p></w:body></w:document>"#,
    );

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
