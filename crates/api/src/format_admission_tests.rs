use super::tests::block_on;
use crate::default_engine;
use into_markdown_core::*;
use std::sync::Arc;

fn request(bytes: &[u8], name: Option<&str>) -> ConversionRequest {
    let mut request =
        ConversionRequest::new(InputRef::bytes(bytes.to_vec(), name.map(str::to_owned)));
    request.options.ocr.policy = OcrPolicy::Off;
    request
}

#[test]
fn unsupported_suffixes_cannot_become_text_or_structured_documents() {
    let engine = default_engine().unwrap();
    for extension in ["js", "mjs", "cjs", "py", "css", "sh", "yaml", "toml", "svg", "bin"] {
        for content in
            ["", "print('hello')", "{\"a\":1}", "<root/>", "<html><body>visible</body></html>"]
        {
            for policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
                let mut input = request(content.as_bytes(), Some(&format!("source.{extension}")));
                input.options.error_policy = policy;
                input.hint.charset = Some("utf-8".into());
                input.hint.media_type = Some("text/plain".into());
                let error = block_on(engine.convert(input)).unwrap_err();
                assert_eq!(error.code(), ErrorCode::Unsupported, "{extension}: {error}");
                assert!(error.to_string().contains(extension));
            }
        }
    }
}

#[test]
fn explicit_format_and_registered_content_detectors_remain_authoritative() {
    let engine = default_engine().unwrap();
    let mut input = request(b"source text", Some("source.py"));
    input.hint.format = Some(InputFormat::Text);
    assert_eq!(block_on(engine.convert(input)).unwrap().markdown, "source text\n");
    let mut input = request(b"{\"a\":", Some("source.js"));
    input.hint.format = Some(InputFormat::Json);
    assert_eq!(block_on(engine.convert(input)).unwrap_err().code(), ErrorCode::Malformed);

    struct CustomDetector;
    impl FormatDetector for CustomDetector {
        fn id(&self) -> &'static str {
            "test.custom.source"
        }
        fn detect<'a>(
            &'a self,
            _: &'a ResolvedInput,
            _: &'a FormatHint,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
            Box::pin(async {
                Ok(vec![FormatCandidate::new(InputFormat::Text, 1.0, "custom source contract")])
            })
        }
    }
    let mut builder = into_markdown_engine::EngineBuilder::new()
        .renderer(Arc::new(into_markdown_render_markdown::GfmRenderer));
    into_markdown_converters::register_core_components(builder.registry_mut()).unwrap();
    builder.registry_mut().register_format_detector(Arc::new(CustomDetector));
    let custom = builder.build().unwrap();
    assert_eq!(
        block_on(custom.convert(request(b"custom text", Some("source.custom")))).unwrap().markdown,
        "custom text\n"
    );
}

#[test]
fn mime_admission_and_nameless_inputs_follow_the_same_boundary() {
    let engine = default_engine().unwrap();
    for mime in ["image/svg+xml", "image/unknown", "audio/unknown", "application/javascript"] {
        let mut input = request(b"ordinary text", None);
        input.hint.media_type = Some(mime.into());
        assert_eq!(block_on(engine.convert(input)).unwrap_err().code(), ErrorCode::Unsupported);
    }
    for mime in [None, Some("APPLICATION/OCTET-STREAM; x=y"), Some("Text/Plain; charset=utf-8")] {
        let mut input = request(b"ordinary text", None);
        input.hint.media_type = mime.map(str::to_owned);
        assert_eq!(block_on(engine.convert(input)).unwrap().markdown, "ordinary text\n");
    }
    let mut input = request(b"ordinary text", Some("source.TXT"));
    input.hint.media_type = Some("application/javascript".into());
    let detected = block_on(engine.detect(DetectionRequest {
        input: input.input.clone(),
        hint: input.hint.clone(),
        options: input.options.clone(),
        execution: input.execution.clone(),
    }))
    .unwrap();
    assert!(detected.candidates.iter().any(|candidate| !candidate.diagnostics.is_empty()));
    let output = block_on(engine.convert(input)).unwrap();
    assert_eq!(output.markdown, "ordinary text\n");
}

#[test]
fn real_binary_documents_override_wrong_supported_and_unsupported_suffixes() {
    let root = crate::test_fixture_root().parent().unwrap().to_path_buf();
    let engine = default_engine().unwrap();
    for (path, format) in [
        ("tools/macos-release/fixtures/normal.ppt", InputFormat::Ppt),
        ("tools/macos-release/fixtures/normal.doc", InputFormat::Doc),
        ("tools/macos-release/fixtures/normal.xls", InputFormat::Xls),
        ("fixtures/small/pptx/normal.pptx", InputFormat::Pptx),
        ("fixtures/small/docx/normal.docx", InputFormat::Docx),
        ("fixtures/small/xlsx/normal.xlsx", InputFormat::Xlsx),
        ("fixtures/small/ocr/ocr-english-clear-1.png", InputFormat::Image),
    ] {
        let bytes = std::fs::read(root.join(path)).unwrap();
        let original = block_on(engine.convert(request(&bytes, Some(path)))).unwrap();
        for name in [Some("wrong.js"), Some("wrong.md"), Some("wrong.csv"), Some("wrong.bin"), None]
        {
            let mut input = request(&bytes, name);
            input.hint.media_type = Some("application/javascript".into());
            let detected = block_on(engine.detect(DetectionRequest {
                input: input.input.clone(),
                hint: input.hint.clone(),
                options: input.options.clone(),
                execution: input.execution.clone(),
            }))
            .unwrap();
            assert_eq!(detected.candidates[0].format, format, "{path} as {name:?}");
            assert!(detected.candidates.iter().all(|candidate| !matches!(
                candidate.format,
                InputFormat::Markdown | InputFormat::Csv | InputFormat::Text
            )));
            let context =
                ExecutionContext::new(input.execution.clone(), input.options.limits.clone());
            let output = block_on(engine.convert_with_context(input, context.clone())).unwrap();
            assert_eq!(context.detected_format(), Some(format));
            output.document.validate().unwrap();
            assert_eq!(output.markdown, original.markdown, "{path} as {name:?}");
            assert_eq!(output.assets.len(), original.assets.len());
        }
    }
}

#[test]
fn txt_partial_prefixes_and_full_structures_keep_their_existing_routes() {
    let engine = default_engine().unwrap();
    for bytes in
        [b"{\"a\":".as_slice(), b"<root>text", b"\xef\xbb\xbf{\"a\":", b"\xff\xfe{\0\"\0a\0\"\0:\0"]
    {
        let output = block_on(engine.convert(request(bytes, Some("partial.txt")))).unwrap();
        output.document.validate().unwrap();
        assert!(!output.markdown.starts_with("# JSON"));
        assert!(!output.markdown.starts_with("# XML"));
    }
    for (bytes, format) in [
        (b"{\"a\":1}".as_slice(), InputFormat::Json),
        (b"<root/>", InputFormat::Xml),
        (b"name,age\nAlice,42\nBob,30\n", InputFormat::Csv),
    ] {
        let input = request(bytes, Some("structured.txt"));
        let detected = block_on(engine.detect(DetectionRequest {
            input: input.input,
            hint: input.hint,
            options: input.options,
            execution: input.execution,
        }))
        .unwrap();
        assert_eq!(detected.candidates[0].format, format);
    }
}

#[test]
fn feed_html_keeps_nested_styles_and_blank_link_labels_under_both_policies() {
    let source = br#"<rss version="2.0"><channel><title>Feed</title><item><guid isPermaLink="false">one</guid><description><![CDATA[<p><b><strong><a href=" ">feed-visible</a></strong></b></p>]]></description></item></channel></rss>"#;
    for policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
        let mut input = request(source, Some("input.rss"));
        input.options.error_policy = policy;
        let output = block_on(default_engine().unwrap().convert(input)).unwrap();
        output.document.validate().unwrap();
        assert!(output.markdown.replace("\\-", "-").contains("feed-visible"));
    }
}

#[test]
fn admission_and_html_keep_cancellation_timeout_and_memory_limits() {
    for (bytes, name) in [
        (b"print('x')".as_slice(), "input.py"),
        (b"<main><p><b><b>text</b></b></p></main>", "input.html"),
    ] {
        let engine = default_engine().unwrap();
        let input = request(bytes, Some(name));
        input.execution.cancellation.cancel();
        assert_eq!(block_on(engine.convert(input)).unwrap_err().code(), ErrorCode::Cancelled);
        let mut timed = request(bytes, Some(name));
        timed.execution.timeout = Some(std::time::Duration::ZERO);
        assert_eq!(block_on(engine.convert(timed)).unwrap_err().code(), ErrorCode::Timeout);
        let mut input = request(bytes, Some(name));
        input.options.limits.max_memory_bytes = 1;
        assert_eq!(block_on(engine.convert(input)).unwrap_err().code(), ErrorCode::ResourceLimit);
    }
}

#[test]
fn utf16_incomplete_xml_declarations_keep_safe_txt_route() {
    for source in ["<?xml version='1.0'?>", "<?xml version='1.0'?>unfinished"] {
        for little_endian in [true, false] {
            let mut bytes = if little_endian { vec![0xff, 0xfe] } else { vec![0xfe, 0xff] };
            for word in source.encode_utf16() {
                bytes.extend(if little_endian { word.to_le_bytes() } else { word.to_be_bytes() });
            }
            let input = request(&bytes, Some("partial.txt"));
            let context =
                ExecutionContext::new(input.execution.clone(), input.options.limits.clone());
            let output =
                block_on(default_engine().unwrap().convert_with_context(input, context.clone()))
                    .unwrap();
            assert_eq!(context.detected_format(), Some(InputFormat::Text));
            assert!(output.markdown.contains("xml"));
        }
    }
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn renamed_pdf_preserves_native_page_structure_and_body() {
    assert!(std::env::var_os("PDFIUM_LIBRARY").is_some());
    let bytes = std::fs::read(crate::test_fixture_root().join("small/pdf/structures.pdf")).unwrap();
    let engine = default_engine().unwrap();
    let original = block_on(engine.convert(request(&bytes, Some("original.pdf")))).unwrap();
    for name in ["wrong.js", "wrong.md", "wrong.csv", "wrong.bin", "wrong"] {
        let input = request(&bytes, Some(name));
        let context = ExecutionContext::new(input.execution.clone(), input.options.limits.clone());
        let output = block_on(engine.convert_with_context(input, context.clone())).unwrap();
        assert_eq!(context.detected_format(), Some(InputFormat::Pdf));
        assert_eq!(output.markdown, original.markdown);
        assert_eq!(output.document.blocks.len(), original.document.blocks.len());
        assert_eq!(output.assets.len(), original.assets.len());
        output.document.validate().unwrap();
    }
}
