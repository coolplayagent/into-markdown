use super::*;

pub(super) fn native_character<'a>(blocks: &'a [BlockNode], wanted: &str) -> &'a Provenance {
    fn find<'a>(inlines: &'a [Inline], wanted: &str) -> Option<&'a Provenance> {
        inlines.iter().find_map(|inline| match inline {
            Inline::SourceText { value, provenance, .. } if value == wanted => Some(&**provenance),
            Inline::Link { content, .. } => find(content, wanted),
            _ => None,
        })
    }
    blocks
        .iter()
        .find_map(|node| match &node.block {
            Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
                find(inlines, wanted)
            }
            _ => None,
        })
        .expect("native character retains provenance inside inline links")
}

pub(super) fn assert_rotated_links(blocks: &[BlockNode], external: Rect, internal: Rect) {
    let mut found = [false; 2];
    for node in blocks {
        let (Block::Paragraph(inlines) | Block::Heading { content: inlines, .. }) = &node.block
        else {
            continue;
        };
        for inline in inlines {
            let Inline::Link { target, content } = inline else { continue };
            let (index, expected) = match target.as_str() {
                "https://example.test/rotated" => (0, external),
                "#pdf-page-2" => (1, internal),
                _ => continue,
            };
            found[index] = true;
            let mut source = false;
            for inline in content {
                if let Inline::SourceText { provenance, .. } = inline {
                    source = true;
                    let r = provenance.locator.bounds.unwrap();
                    assert!(
                        (expected.x..=expected.x + expected.width).contains(&(r.x + r.width / 2.0))
                    );
                    assert!(
                        (expected.y..=expected.y + expected.height)
                            .contains(&(r.y + r.height / 2.0))
                    );
                }
            }
            if !source {
                assert_eq!(node.provenance.locator.bounds, Some(expected));
            }
        }
    }
    assert_eq!(found, [true, true]);
}

fn linked_document() -> Vec<u8> {
    let directory = b"BT /F1 16 Tf 20 270 Td (Contents) Tj ET\n\
        BT /F1 10 Tf 20 240 Td (1 Guide) Tj 170 0 Td (. . . .) Tj 60 0 Td (1) Tj ET\n\
        BT /F1 10 Tf 32 220 Td (1.1 Details) Tj 158 0 Td (. . . .) Tj 60 0 Td (1) Tj ET\n\
        BT /F1 10 Tf 20 200 Td (2 Examples) Tj 170 0 Td (. . . .) Tj 60 0 Td (1) Tj ET";
    let body = b"BT /F1 10 Tf 20 240 Td (Read the product guide for details.) Tj ET\n\
        BT /F1 10 Tf 20 220 Td (See the next page for examples.) Tj ET\n\
        BT /F1 10 Tf 20 200 Td (https://example.com/guide) Tj ET\n\
        BT /F1 10 Tf 20 180 Td (https://example.com/guide) Tj ET";
    assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 7 0 R >> >> /Contents 5 0 R /Annots [8 0 R 9 0 R 10 0 R] >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R /Annots [11 0 R 12 0 R 13 0 R 14 0 R] >>".to_vec(),
        stream_object("",directory), stream_object("",body),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [20 237 62 250] /Dest [4 0 R /Fit] >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [32 217 98 230] /Dest [4 0 R /Fit] >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [20 197 80 210] /Dest [4 0 R /Fit] >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [74 237 152 250] /A << /S /URI /URI (https://example.com/guide) >> >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [68 217 122 230] /Dest [4 0 R /Fit] >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [20 197 170 210] /A << /S /URI /URI (https://example.com/guide) >> >>".to_vec(),
        b"<< /Type /Annot /Subtype /Link /Rect [200 30 250 50] /A << /S /URI /URI (https://example.com/unmatched) >> >>".to_vec(),
    ])
}

#[test]
#[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
fn native_issue378_directory_and_body_links_keep_labels_positions_and_fallback() {
    let runtime = PathBuf::from(std::env::var_os("PDFIUM_LIBRARY").expect("PDFIUM_LIBRARY"));
    let input =
        ResolvedInput { bytes: Arc::from(linked_document()), metadata: SourceMetadata::default() };
    for policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
        let mut options = ConversionOptions { error_policy: policy, ..Default::default() };
        options.ocr.policy = OcrPolicy::Off;
        options.output.asset_mode = AssetMode::Omit;
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            options.limits.clone(),
        );
        let result = convert_pdf(&runtime, &input, &options, &context).unwrap();
        let markdown =
            into_markdown_render_markdown::render(&result.document, &result.assets, &options)
                .unwrap();
        assert!(
            markdown.contains("Read the [product guide](<https://example.com/guide>) for details."),
            "{markdown}"
        );
        assert!(
            markdown.contains("See the [next page](<#pdf-page-2>) for examples."),
            "{markdown}"
        );
        assert!(markdown.contains("- [1 Guide](<#pdf-page-2>)"), "{markdown}");
        assert!(markdown.contains("    - [1.1 Details](<#pdf-page-2>)"), "{markdown}");
        assert!(markdown.contains("- [2 Examples](<#pdf-page-2>)"), "{markdown}");
        assert_eq!(markdown.matches("](<https://example.com/guide>)").count(), 3, "{markdown}");
        assert_eq!(markdown.matches("](<https://example.com/unmatched>)").count(), 1);
        assert!(!markdown.contains("[#pdf-page-2](<#pdf-page-2>)"), "{markdown}");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        result.document.validate().unwrap();
        drop(result);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}
