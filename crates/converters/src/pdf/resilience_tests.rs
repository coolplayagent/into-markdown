use super::*;

fn links_pdf(rectangles: &[&str]) -> Vec<u8> {
    links_pdf_on_page(rectangles, "0 0 100 200", false)
}

fn links_pdf_on_page(rectangles: &[&str], media_box: &str, prefix_note: bool) -> Vec<u8> {
    let annotations =
        (0..rectangles.len()).map(|i| format!("{} 0 R", i + 6)).collect::<Vec<_>>().join(" ");
    let annotations = if prefix_note {
        format!("{} 0 R {annotations}", rectangles.len() + 6)
    } else {
        annotations
    };
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [{media_box}] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R /Annots [{annotations}] >>").into_bytes(),
        stream_object("", b"BT /F1 8 Tf 10 100 Td (Retained body) Tj ET"),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for (i, rect) in rectangles.iter().enumerate() {
        objects.push(format!("<< /Type /Annot /Subtype /Link /Rect [{rect}] /A << /S /URI /URI (https://example.test/link-{i}) >> >>").into_bytes());
    }
    if prefix_note {
        objects
            .push(b"<< /Type /Annot /Subtype /Text /Rect [1 1 10 10] /Contents (Note) >>".to_vec());
    }
    assemble_pdf(&objects)
}

fn long_pdf(pages: u32, objects_per_page: usize) -> Vec<u8> {
    let kids = (0..pages).map(|i| format!("{} 0 R", i + 5)).collect::<Vec<_>>().join(" ");
    let content = b"BT /F1 8 Tf 10 100 Td (x) Tj ET\n".repeat(objects_per_page);
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>").into_bytes(),
        stream_object("", &content),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for _ in 0..pages {
        objects.push(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 3 0 R >>".to_vec());
    }
    assemble_pdf(&objects)
}

fn convert(
    bytes: Vec<u8>,
    options: &ConversionOptions,
) -> Result<ConverterOutput, ConversionError> {
    let path = PathBuf::from(std::env::var_os("PDFIUM_LIBRARY").expect("PDFIUM_LIBRARY"));
    let input = ResolvedInput { bytes: Arc::from(bytes), metadata: SourceMetadata::default() };
    let context = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        options.limits.clone(),
    );
    let result = convert_pdf(&path, &input, options, &context);
    if result.is_err() {
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
    result
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue334_reversed_links_preserve_body_and_targets() {
    for policy in [ErrorPolicy::BestEffort, ErrorPolicy::Strict] {
        let options = ConversionOptions { error_policy: policy, ..ConversionOptions::default() };
        let output = convert(links_pdf(&["10 160 40 180", "10 180 40 160"]), &options).unwrap();
        let markdown =
            into_markdown_render_markdown::render(&output.document, &output.assets, &options)
                .unwrap();
        assert!(markdown.contains("Retained body"));
        assert!(markdown.contains("https://example.test/link-0"));
        assert!(markdown.contains("https://example.test/link-1"));
    }
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue334_long_document_has_independent_object_budget() {
    let mut options = ConversionOptions::default();
    options.ocr.policy = OcrPolicy::Off;
    options.output.asset_mode = AssetMode::Omit;
    let output = convert(long_pdf(501, 200), &options).unwrap();
    assert_eq!(output.document.blocks.len(), 501);
    assert!(output.document.validate().is_ok());
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue334_mixed_links_have_local_diagnostics_in_all_ocr_modes() {
    for ocr in [OcrPolicy::Off, OcrPolicy::Auto, OcrPolicy::Always] {
        let mut options = ConversionOptions::default();
        options.ocr.policy = ocr;
        let bytes = links_pdf(&[
            "10 160 40 180",
            "10 20 10 40",
            "120 20 130 40",
            "-10 30 20 50",
            "10 180 40 160",
        ]);
        let output = convert(bytes.clone(), &options).unwrap();
        let markdown =
            into_markdown_render_markdown::render(&output.document, &output.assets, &options)
                .unwrap();
        assert!(markdown.contains("Retained body"));
        for index in [0, 3, 4] {
            assert!(markdown.contains(&format!("https://example.test/link-{index}")));
        }
        for index in [1, 2] {
            assert!(!markdown.contains(&format!("https://example.test/link-{index}")));
        }
        let diagnostics =
            output.diagnostics.iter().filter(|d| d.code == "pdf.linkOmitted").collect::<Vec<_>>();
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics[0].message.contains("annotation[1]: zero-area"));
        assert!(diagnostics[1].message.contains("annotation[2]: rectangle is outside"));
        assert!(diagnostics.iter().all(|d| d.locator.as_ref().unwrap().page == Some(1)));
        options.error_policy = ErrorPolicy::Strict;
        assert!(
            matches!(convert(bytes, &options), Err(ConversionError::Malformed { detail, .. }) if detail.contains("page 1") && detail.contains("annotation[1]"))
        );
    }
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue334_page_total_and_ir_limits_remain_independent() {
    let mut options = ConversionOptions::default();
    options.ocr.policy = OcrPolicy::Off;
    let bytes = long_pdf(3, 4);
    options.limits.max_pdf_page_objects = 3;
    assert!(
        matches!(convert(bytes.clone(), &options), Err(ConversionError::ResourceLimit { limit: "max_pdf_page_objects", detail }) if detail.contains("page 1: 4 > 3"))
    );
    options.limits.max_pdf_page_objects = 4;
    options.limits.max_pdf_total_objects = 11;
    assert!(
        matches!(convert(bytes.clone(), &options), Err(ConversionError::ResourceLimit { limit: "max_pdf_total_objects", detail }) if detail.contains("page 3: 12 > 11"))
    );
    options.limits.max_pdf_total_objects = 12;
    assert_eq!(convert(bytes.clone(), &options).unwrap().document.blocks.len(), 3);
    options.limits.max_pages = 2;
    assert!(matches!(
        convert(bytes.clone(), &options),
        Err(ConversionError::ResourceLimit { limit: "max_pages", .. })
    ));
    options.limits.max_pages = 3;
    options.limits.max_memory_bytes = 1;
    assert!(matches!(
        convert(bytes, &options),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
}

#[test]
fn request_link_checkpoints_preserve_cancellation_and_timeout() {
    use into_markdown_core::ExecutionOptions;
    for timeout in [false, true] {
        let mut execution = ExecutionOptions::default();
        if timeout {
            execution.timeout = Some(std::time::Duration::ZERO);
        } else {
            execution.cancellation.cancel();
        }
        let context =
            ExecutionContext::new(execution, into_markdown_core::ResourceLimits::default());
        let result = request_path_scan(&context, |check| {
            assert!(!check());
            Err::<(), _>(PdfiumError::InvalidResult {
                operation: "links_checkpoint",
                detail: "interrupted".into(),
            })
        });
        if timeout {
            assert!(matches!(result, Err(ConversionError::Timeout)));
        } else {
            assert!(matches!(result, Err(ConversionError::Cancelled)));
        }
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue334_explicit_layout_budget_is_enforced() {
    let mut options = ConversionOptions::default();
    options.ocr.policy = OcrPolicy::Off;
    options.limits.max_pdf_layout_comparisons = 1;
    let result = convert(
        one_page_fixture(
            b"BT /F1 8 Tf 10 160 Td (First line) Tj 0 -15 Td (Second line) Tj ET",
            false,
        ),
        &options,
    );
    assert!(
        matches!(
            &result,
            Err(ConversionError::ResourceLimit { limit: "pdfLayoutComparisons", .. })
        ),
        "actual output: {:?}",
        result.as_ref().map(|o| &o.document)
    );
    options.limits.max_pdf_layout_comparisons = 12_000_000;
    assert!(
        convert(
            one_page_fixture(
                b"BT /F1 8 Tf 10 160 Td (First line) Tj 0 -15 Td (Second line) Tj ET",
                false
            ),
            &options
        )
        .is_ok()
    );
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue334_link_clipping_accounts_for_nonzero_page_origin() {
    let bytes = links_pdf_on_page(&["90 190 150 350"], "100 200 200 400", false);
    let output = convert(bytes, &ConversionOptions::default()).unwrap();
    let Block::Page { blocks, .. } = &output.document.blocks[0].block else { panic!("page") };
    let link = blocks.iter().find(|node| node.id.0.contains("-link-")).unwrap();
    assert_eq!(
        link.provenance.locator.bounds,
        Some(Rect { x: 0.0, y: 50.0, width: 50.0, height: 150.0 })
    );
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue334_diagnostics_keep_original_annotation_array_index() {
    let bytes = links_pdf_on_page(&["10 20 10 40", "10 40 20 60"], "0 0 100 200", true);
    let output = convert(bytes, &ConversionOptions::default()).unwrap();
    assert!(
        output
            .diagnostics
            .iter()
            .any(|d| d.code == "pdf.linkOmitted" && d.message.contains("annotation[1]"))
    );
}
