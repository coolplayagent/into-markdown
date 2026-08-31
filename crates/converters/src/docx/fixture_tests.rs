#[cfg(test)]
mod issue_270_fixture_tests {
    use super::*;
    use into_markdown_core::{
        ConversionOptions, DiagnosticSeverity, ErrorPolicy, ExecutionOptions, ResourceLimits,
    };
    use into_markdown_render_markdown::render;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn convert(bytes: &[u8]) -> Result<(ConverterOutput, String), ConversionError> {
        let options = ConversionOptions::default();
        let output = convert_docx(bytes, &options, &context())?;
        let markdown = render(&output.document, &output.assets, &options)?;
        Ok((output, markdown))
    }

    fn assert_order(markdown: &str, expected: &[&str]) {
        let markdown = markdown.replace("\\-", "-").replace("\\[", "[").replace("\\]", "]");
        let mut cursor = 0;
        for value in expected {
            let relative = markdown[cursor..]
                .find(value)
                .unwrap_or_else(|| panic!("missing {value:?} after byte {cursor}: {markdown}"));
            cursor += relative + value.len();
        }
    }

    #[test]
    fn local_alt_chunks_convert_in_document_order() {
        for (bytes, expected) in [
            (
                include_bytes!("../../tests/fixtures/docx/issue-270/html.docx").as_slice(),
                "HTML visible",
            ),
            (
                include_bytes!("../../tests/fixtures/docx/issue-270/xhtml.docx").as_slice(),
                "XHTML visible",
            ),
            (
                include_bytes!("../../tests/fixtures/docx/issue-270/mhtml.docx").as_slice(),
                "MHTML visible",
            ),
            (
                include_bytes!("../../tests/fixtures/docx/issue-270/rtf.docx").as_slice(),
                "RTF visible",
            ),
        ] {
            let (output, markdown) = convert(bytes).unwrap();
            assert_order(&markdown, &["before", expected, "after"]);
            assert!(!markdown.contains("script-hidden"));
            assert!(output.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "word.altChunkConverted"
                    && diagnostic.severity == DiagnosticSeverity::Info
            }));
        }
    }

    #[test]
    fn alt_chunk_repeated_marks_and_empty_links_preserve_content_in_both_policies() {
        use std::io::{Cursor, Read, Write};
        let original = include_bytes!("../../tests/fixtures/docx/issue-270/html.docx");
        let mut archive = zip::ZipArchive::new(Cursor::new(original)).unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let mut replaced = false;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if entry.name().ends_with(".html") {
                bytes = b"<main><p><b><strong><a href=' '>chunk-visible</a></strong></b></p></main>".to_vec();
                replaced = true;
            }
            writer.start_file(entry.name(), zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        assert!(replaced);
        let bytes = writer.finish().unwrap().into_inner();
        for policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
            let mut options = ConversionOptions::default();
            options.error_policy = policy;
            let output = convert_docx(&bytes, &options, &context()).unwrap();
            output.document.validate().unwrap();
            let markdown = render(&output.document, &output.assets, &options).unwrap();
            assert_order(&markdown, &["before", "chunk-visible", "after"]);
            assert!(output.diagnostics.iter().any(|item| item.code == "html.linkUriRejected" && item.locator.as_ref().and_then(|loc| loc.part.as_deref()).is_some()));
            assert!(!output.diagnostics.iter().any(|item| item.code == "word.altChunkOmitted"));
        }
    }

    #[test]
    fn strict_ooxml_qnames_and_relationships_are_supported() {
        let (_, markdown) = convert(include_bytes!(
            "../../tests/fixtures/docx/issue-270/strict.docx"
        ))
        .unwrap();
        assert_order(&markdown, &["strict-before", "strict chunk", "strict-after"]);
    }

    #[test]
    fn external_and_recursive_alt_chunks_never_escape_the_package() {
        let (external, external_markdown) = convert(include_bytes!(
            "../../tests/fixtures/docx/issue-270/external.docx"
        ))
        .unwrap();
        assert_order(
            &external_markdown,
            &["before", "[Embedded Word content omitted]", "after"],
        );
        assert!(
            external
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "office.relationshipOmitted")
        );

        let (cycle, cycle_markdown) = convert(include_bytes!(
            "../../tests/fixtures/docx/issue-270/cycle.docx"
        ))
        .unwrap();
        assert_eq!(
            cycle_markdown.trim().replace("\\[", "[").replace("\\]", "]"),
            "[Embedded Word content omitted]"
        );
        assert!(
            cycle
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "word.unsupportedWrapperOmitted")
        );
    }

    #[test]
    fn embedded_entities_fail_closed_and_true_empty_source_is_certified() {
        assert!(matches!(
            convert(include_bytes!("../../tests/fixtures/docx/issue-270/entity.docx")),
            Err(ConversionError::Malformed { .. })
        ));
        let (empty, markdown) = convert(include_bytes!(
            "../../tests/fixtures/docx/issue-270/empty.docx"
        ))
        .unwrap();
        assert!(markdown.is_empty());
        assert_eq!(empty.source_content_evidence(), SourceContentEvidence::Empty);
    }

    #[test]
    fn repeated_content_assets_and_nested_table_order_are_preserved() {
        let (duplicate, markdown) = convert(include_bytes!(
            "../../tests/fixtures/docx/issue-270/duplicate-content-assets.docx"
        ))
        .unwrap();
        assert_eq!(markdown.matches("repeat").count(), 2);
        assert_eq!(markdown.matches("same nested text").count(), 2);
        assert_eq!(duplicate.assets.len(), 1);
        assert_eq!(
            duplicate
                .document
                .blocks
                .iter()
                .filter(|node| matches!(node.block, Block::Image { .. }))
                .count(),
            2
        );

        let (_, ordered) = convert(include_bytes!(
            "../../tests/fixtures/docx/issue-270/ordered-nested-merged.docx"
        ))
        .unwrap();
        assert_order(
            &ordered,
            &[
                "body-before",
                "linked",
                "outer-a",
                "nested",
                "outer-b",
                "merged",
                "outer-c",
                "chunk-middle",
                "chunk-middle",
                "body-after",
                "footnote-after-ref",
            ],
        );
    }

    #[test]
    fn known_word_wrappers_preserve_visible_content_by_qname() {
        let (output, markdown) = convert(include_bytes!(
            "../../tests/fixtures/docx/issue-270/wrappers.docx"
        ))
        .unwrap();
        assert_order(
            &markdown,
            &[
                "wrapper-before",
                "content-control",
                "custom-xml-wrapper",
                "compatibility-fallback",
                "field-result",
                "textbox-visible",
                "wrapper-after",
                "header-visible",
                "footer-visible",
            ],
        );
        assert!(!markdown.contains("choice-hidden"));
        assert!(output.diagnostics.iter().all(|diagnostic| {
            diagnostic.code != "word.unsupportedWrapperOmitted"
                || diagnostic.message.contains("nested paragraph wrapper")
        }));
    }
}
