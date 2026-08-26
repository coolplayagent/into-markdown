//! Explicit pinned-PDFium semantic layout quality gate.

#![allow(unexpected_cfgs)]

#[cfg(not(pdf_layout_quality))]
#[test]
#[ignore = "run the explicit Bazel pdf_layout_quality target with pinned PDFium"]
fn explicit_quality_target_requires_pinned_pdfium() {}

#[cfg(pdf_layout_quality)]
#[allow(
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    reason = "small hash-bound fixture metrics and one auditable quality transaction"
)]
mod quality {
    use into_markdown_converters::PdfConverter;
    use into_markdown_core::{
        Block, ConversionOptions, Converter, ExecutionContext, ExecutionOptions, FormatCandidate,
        Inline, InputFormat, ResolvedInput, ResourceLimits, Services, SourceMetadata,
    };
    use sha2::{Digest, Sha256};
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use unicode_normalization::UnicodeNormalization;

    #[test]
    fn pinned_pdfium_meets_hash_bound_semantic_precision_recall_and_goldens() {
        let runfiles = PathBuf::from(std::env::var_os("TEST_SRCDIR").expect("Bazel runfiles"));
        let authority_bytes =
            std::fs::read(runfile(&runfiles, "_main/fixtures/pdf-layout-quality-authority.json"))
                .unwrap();
        let authority: serde_json::Value = serde_json::from_slice(&authority_bytes).unwrap();
        let manifest_bytes =
            std::fs::read(runfile(&runfiles, "_main/fixtures/manifest.json")).unwrap();
        assert_eq!(hex(&manifest_bytes), authority["fixture_manifest_sha256"]);
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
        let pdfium_manifest =
            std::fs::read(runfile(&runfiles, "_main/third_party/pdfium/manifest.json")).unwrap();
        assert_eq!(hex(&pdfium_manifest), authority["pdfium_manifest_sha256"]);
        let runtime = runtime_path(&runfiles);
        let mut true_positive = 0_usize;
        let mut false_positive = 0_usize;
        let mut false_negative = 0_usize;
        for golden in authority["fixtures"].as_array().unwrap() {
            let id = golden["fixture_id"].as_str().unwrap();
            let record = manifest["fixtures"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["id"] == id)
                .unwrap();
            let bytes = std::fs::read(runfile(
                &runfiles,
                &format!("_main/fixtures/{}", record["path"].as_str().unwrap()),
            ))
            .unwrap();
            assert_eq!(hex(&bytes), golden["fixture_sha256"]);
            let first = convert(&runtime, id, &bytes)
                .unwrap_or_else(|error| panic!("{id} conversion failed: {error:?}"));
            let second = convert(&runtime, id, &bytes)
                .unwrap_or_else(|error| panic!("{id} repeated conversion failed: {error:?}"));
            assert_eq!(
                first.document.to_json().unwrap(),
                second.document.to_json().unwrap(),
                "{id} must be deterministic"
            );
            validate_source_geometry(&first.document);
            let actual = semantic_sequence(&first.document);
            let expected = golden["expected_sequence"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            for node in &actual {
                if expected.contains(node) {
                    true_positive += 1;
                } else {
                    false_positive += 1;
                }
            }
            false_negative += expected.iter().filter(|node| !actual.contains(node)).count();
            assert_eq!(actual, expected, "{id} layout golden drift");
        }
        let precision = true_positive as f64 / (true_positive + false_positive) as f64;
        let recall = true_positive as f64 / (true_positive + false_negative) as f64;
        assert!(precision >= authority["minimum_semantic_precision"].as_f64().unwrap());
        assert!(recall >= authority["minimum_semantic_recall"].as_f64().unwrap());
    }

    fn convert(
        runtime: &Path,
        id: &str,
        bytes: &[u8],
    ) -> Result<into_markdown_core::ConverterOutput, into_markdown_core::ConversionError> {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let input = ResolvedInput {
            bytes: Arc::from(bytes),
            metadata: SourceMetadata {
                name: Some(id.into()),
                media_type: Some("application/pdf".into()),
                uri: None,
                size: u64::try_from(bytes.len()).unwrap(),
            },
        };
        block_on(PdfConverter::with_runtime_path(runtime).convert(
            &input,
            &FormatCandidate::explicit(InputFormat::Pdf),
            &options,
            &Services::default(),
            &context,
        ))
    }

    fn semantic_sequence(document: &into_markdown_core::Document) -> Vec<String> {
        let Block::Page { blocks, .. } = &document.blocks[0].block else { panic!("page") };
        blocks
            .iter()
            .filter_map(|node| match &node.block {
                Block::Heading { content, .. } => Some(format!("heading:{}", text(content))),
                Block::Paragraph(content) => Some(format!("paragraph:{}", text(content))),
                Block::List { items, .. } => Some(format!(
                    "list:{}",
                    items
                        .iter()
                        .map(|item| block_text(&item.blocks[0].block))
                        .collect::<Vec<_>>()
                        .join("|")
                )),
                Block::Table { rows, .. } => Some(format!(
                    "table:{}",
                    rows.iter()
                        .map(|row| row
                            .cells
                            .iter()
                            .map(|cell| block_text(&cell.blocks[0].block))
                            .collect::<Vec<_>>()
                            .join("|"))
                        .collect::<Vec<_>>()
                        .join(";")
                )),
                Block::Footnote { blocks, .. } => {
                    Some(format!("footnote:{}", block_text(&blocks[0].block)))
                }
                Block::Image { .. } => None,
                other => panic!("unexpected quality block {other:?}"),
            })
            .collect()
    }

    fn block_text(block: &Block) -> String {
        match block {
            Block::Paragraph(content) | Block::Heading { content, .. } => text(content),
            other => panic!("unexpected text block {other:?}"),
        }
    }

    fn text(inlines: &[Inline]) -> String {
        let raw = inlines
            .iter()
            .filter_map(|inline| match inline {
                Inline::Text { value, .. }
                | Inline::SourceText { value, .. }
                | Inline::OcrText { value, .. } => Some(value.as_str()),
                _ => None,
            })
            .collect::<String>();
        raw.nfc().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn validate_source_geometry(document: &into_markdown_core::Document) {
        let json = document.to_json().unwrap();
        assert!(!json.contains("NaN") && !json.contains("Infinity"));
        let Block::Page { blocks, .. } = &document.blocks[0].block else { panic!("page") };
        for node in blocks {
            if let Some(bounds) = node.provenance.locator.bounds {
                let width = node.provenance.locator.page_width.unwrap();
                let height = node.provenance.locator.page_height.unwrap();
                assert!(bounds.x >= 0.0 && bounds.y >= 0.0);
                assert!(bounds.x + bounds.width <= width + 0.5);
                assert!(bounds.y + bounds.height <= height + 0.5);
            }
        }
    }

    fn runtime_path(runfiles: &Path) -> PathBuf {
        let repository = env!("PDF_LAYOUT_PDFIUM_REPOSITORY");
        assert_ne!(repository, "unsupported", "quality target requires an audited platform");
        runfile(runfiles, &format!("{repository}/{}", env!("PDF_LAYOUT_PDFIUM_LIBRARY")))
            .canonicalize()
            .unwrap()
    }

    fn runfile(runfiles: &Path, logical: &str) -> PathBuf {
        if let Some(manifest) = std::env::var_os("RUNFILES_MANIFEST_FILE") {
            let manifest = PathBuf::from(manifest);
            let metadata = std::fs::metadata(&manifest).expect("Bazel runfiles manifest metadata");
            assert!(metadata.len() <= 64 * 1024 * 1024, "Bazel runfiles manifest is too large");
            let contents =
                std::fs::read_to_string(manifest).expect("Bazel runfiles manifest contents");
            if let Some(path) = contents.lines().find_map(|line| {
                let (name, path) = line.split_once(' ')?;
                (name == logical).then(|| PathBuf::from(path))
            }) {
                return path;
            }
            panic!("Bazel runfile is missing from the manifest: {logical}");
        }
        runfiles.join(logical)
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
}
