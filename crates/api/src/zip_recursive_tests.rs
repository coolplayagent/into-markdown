use crate::{
    Block, BlockNode, CancellationToken, ConversionError, ConversionOptions, ConversionOutcome,
    ConversionRequest, ErrorCode, ExecutionOptions, FormatHint, Inline, InputFormat, InputRef,
    ResultContent, default_engine,
};
use std::future::Future;
use std::io::{Cursor, Write as _};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use zip::write::SimpleFileOptions;

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut task = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut task) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn archive(entries: &[(&str, &[u8])], deflated: bool) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let method =
        if deflated { zip::CompressionMethod::Deflated } else { zip::CompressionMethod::Stored };
    let options = SimpleFileOptions::default().compression_method(method).unix_permissions(0o644);
    for (name, bytes) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn convert(
    bytes: Vec<u8>,
    options: ConversionOptions,
    execution: ExecutionOptions,
) -> Result<crate::ConversionResult, ConversionError> {
    let mut request = ConversionRequest::new(InputRef::bytes(bytes, Some("input.zip")));
    request.hint = FormatHint { format: Some(InputFormat::Zip), ..FormatHint::default() };
    request.options = options;
    request.execution = execution;
    block_on(default_engine().unwrap().convert(request))
}

#[test]
fn mixed_entries_are_sorted_and_nested_archives_share_the_pipeline() {
    let inner = archive(&[("c.txt", b"inner")], false);
    let outer = archive(&[("z.txt", b"last"), ("nested.zip", &inner), ("a.md", b"# first")], false);
    let first =
        convert(outer.clone(), ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    let second = convert(outer, ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    assert_eq!(first.markdown, second.markdown);
    let a = first.markdown.find("a\\.md").unwrap();
    let nested = first.markdown.find("nested\\.zip/c\\.txt").unwrap();
    let z = first.markdown.find("z\\.txt").unwrap();
    assert!(a < nested && nested < z);
    assert!(first.markdown.contains("inner"));
    assert!(first.markdown.contains("last"));
}

#[test]
fn one_crc_failure_is_diagnostic_and_preserves_other_members() {
    let mut bytes = archive(&[("bad.txt", b"broken"), ("good.txt", b"kept")], false);
    corrupt_crc(&mut bytes, "bad.txt");
    let result = convert(bytes, ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    assert!(result.markdown.contains("kept"));
    assert!(!result.markdown.contains("broken"));
    assert!(result.diagnostics.iter().any(|item| {
        item.code == "zip.entry.failed"
            && item.locator.as_ref().and_then(|locator| locator.part.as_deref()) == Some("bad.txt")
    }));
}

#[test]
fn traversal_depth_ratio_and_cancellation_are_hard_boundaries() {
    let traversal = archive(&[("../escape.txt", b"no")], false);
    assert_eq!(
        convert(traversal, ConversionOptions::default(), ExecutionOptions::default())
            .unwrap_err()
            .code(),
        ErrorCode::Malformed
    );

    let inner = archive(&[("text.txt", b"deep")], false);
    let nested = archive(&[("inner.zip", &inner)], false);
    let mut depth_options = ConversionOptions::default();
    depth_options.limits.max_archive_depth = 1;
    assert!(matches!(
        convert(nested.clone(), depth_options, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_archive_depth", .. })
    ));

    let mut tree_entries = ConversionOptions::default();
    tree_entries.limits.max_archive_entries = 1;
    assert!(matches!(
        convert(nested.clone(), tree_entries, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
    ));
    let mut total_bytes = ConversionOptions::default();
    total_bytes.limits.max_decompressed_bytes = u64::try_from(inner.len()).unwrap();
    assert!(matches!(
        convert(nested, total_bytes, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_decompressed_bytes", .. })
    ));

    let bomb = vec![b'x'; 64 * 1024];
    let ratio = archive(&[("ratio.txt", &bomb)], true);
    let mut ratio_options = ConversionOptions::default();
    ratio_options.limits.max_archive_compression_ratio = 2;
    assert!(matches!(
        convert(ratio, ratio_options, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_archive_compression_ratio", .. })
    ));

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = archive(&[("text.txt", b"cancel")], false);
    assert_eq!(
        convert(
            cancelled,
            ConversionOptions::default(),
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        )
        .unwrap_err()
        .code(),
        ErrorCode::Cancelled
    );
}

#[test]
fn conversion_needs_no_temporary_file_authority() {
    let bytes = archive(&[("text.txt", b"memory only")], false);
    let mut options = ConversionOptions::default();
    options.limits.max_temporary_bytes = 0;
    let result = convert(bytes, options, ExecutionOptions::default()).unwrap();
    assert!(result.markdown.contains("memory only"));
}

#[test]
fn empty_is_valid_but_all_failed_members_are_not_pseudo_success() {
    let empty = archive(&[], false);
    let result = convert(empty, ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    assert!(result.document.blocks.is_empty());
    assert_eq!(result.content().unwrap(), ResultContent::EmptySource);
    assert_eq!(result.outcome(), ConversionOutcome::Complete);
    assert_eq!(result.reason_code(), Some("emptySource"));
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].code, "emptySource");

    let unsupported = archive(&[("unknown.bin", b"\0\x01\x02")], false);
    assert!(matches!(
        convert(unsupported, ConversionOptions::default(), ExecutionOptions::default()),
        Err(ConversionError::Unsupported { .. } | ConversionError::NoConverter { .. })
    ));
}

#[test]
fn empty_zip_bytes_named_as_xlsx_never_fall_back_to_generic_zip_success() {
    let invalid_xlsx = archive(&[], false);
    for policy in [crate::ErrorPolicy::BestEffort, crate::ErrorPolicy::Strict] {
        let mut request =
            ConversionRequest::new(InputRef::bytes(invalid_xlsx.clone(), Some("invalid.xlsx")));
        request.options.error_policy = policy;
        let error = block_on(default_engine().unwrap().convert(request)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
    }

    let nested = archive(&[("invalid.xlsx", &invalid_xlsx)], false);
    let error =
        convert(nested, ConversionOptions::default(), ExecutionOptions::default()).unwrap_err();
    assert!(matches!(
        error,
        ConversionError::Malformed { .. }
            | ConversionError::Unsupported { .. }
            | ConversionError::NoConverter { .. }
    ));
}

#[test]
fn empty_zip_bytes_with_six_package_extensions_never_become_empty_zip_successes() {
    let bytes = archive(&[], false);
    for name in [
        "invalid.docx",
        "invalid.pptx",
        "invalid.xlsx",
        "invalid.odt",
        "invalid.ods",
        "invalid.odp",
    ] {
        let request = ConversionRequest::new(InputRef::bytes(bytes.clone(), Some(name)));
        let error = block_on(default_engine().unwrap().convert(request)).unwrap_err();
        assert_ne!(error.reason_code(), "emptySource", "{name} was washed into an empty ZIP");
    }
}

#[test]
fn empty_text_members_empty_nested_zip_and_failed_siblings_have_stable_outcomes() {
    let empty_text = archive(&[("empty.txt", b" \n"), ("empty.md", b"\xef\xbb\xbf\n")], false);
    let result =
        convert(empty_text, ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    assert_eq!(result.content().unwrap(), ResultContent::Markdown);
    assert!(result.markdown.contains("empty\\.txt"));
    assert!(result.markdown.contains("empty\\.md"));
    assert_eq!(result.outcome(), ConversionOutcome::Complete);

    let empty_nested = archive(&[("nested.zip", &archive(&[], false))], false);
    let result =
        convert(empty_nested, ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    assert_eq!(result.content().unwrap(), ResultContent::EmptySource);
    assert_eq!(result.outcome(), ConversionOutcome::Complete);

    let mixed = archive(&[("empty.txt", b"\n"), ("failed.bin", b"\0\x01\x02")], false);
    let result = convert(mixed, ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    assert_eq!(result.content().unwrap(), ResultContent::Markdown);
    assert!(result.markdown.contains("empty\\.txt"));
    assert_eq!(result.outcome(), ConversionOutcome::Degraded);
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "zip.entry.failed"
            && diagnostic.locator.as_ref().and_then(|locator| locator.part.as_deref())
                == Some("failed.bin")
    }));
}

#[test]
fn recursive_identity_footnotes_and_character_provenance_are_namespaced() {
    let footnotes = archive(
        &[
            ("a.md", b"A[^same]\n\n[^same]: alpha\n"),
            ("b.md", b"B[^same]\n\n[^same]: beta\n"),
            ("c.txt", b"source"),
        ],
        false,
    );
    let result =
        convert(footnotes, ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    result.document.validate().unwrap();

    let mut labels = Vec::new();
    let mut references = Vec::new();
    let mut source_parts = Vec::new();
    let mut node_parts = Vec::new();
    inspect_blocks(
        &result.document.blocks,
        &mut labels,
        &mut references,
        &mut source_parts,
        &mut node_parts,
    );
    labels.sort();
    references.sort();
    assert_eq!(labels, vec!["zip-1-footnote-same", "zip-2-footnote-same"]);
    assert_eq!(references, labels);
    assert!(node_parts.iter().any(|part| part.as_deref() == Some("c.txt")));
    assert!(source_parts.iter().all(|part| {
        part.as_deref().is_some_and(|part| {
            part.starts_with("a.md") || part.starts_with("b.md") || part.starts_with("c.txt")
        })
    }));
}

fn inspect_blocks(
    blocks: &[BlockNode],
    labels: &mut Vec<String>,
    references: &mut Vec<String>,
    source_parts: &mut Vec<Option<String>>,
    node_parts: &mut Vec<Option<String>>,
) {
    for node in blocks {
        node_parts.push(node.provenance.locator.part.clone());
        match &node.block {
            Block::Paragraph(inlines)
            | Block::Heading { content: inlines, .. }
            | Block::TimedSegment { content: inlines, .. } => {
                inspect_inlines(inlines, references, source_parts);
            }
            Block::List { items, .. } => {
                for item in items {
                    inspect_blocks(&item.blocks, labels, references, source_parts, node_parts);
                }
            }
            Block::Table { rows, .. } => {
                for cell in rows.iter().flat_map(|row| &row.cells) {
                    inspect_blocks(&cell.blocks, labels, references, source_parts, node_parts);
                }
            }
            Block::Footnote { label, blocks } => {
                labels.push(label.clone());
                inspect_blocks(blocks, labels, references, source_parts, node_parts);
            }
            Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                inspect_blocks(blocks, labels, references, source_parts, node_parts);
            }
            _ => {}
        }
    }
}

fn inspect_inlines(
    inlines: &[Inline],
    references: &mut Vec<String>,
    source_parts: &mut Vec<Option<String>>,
) {
    for inline in inlines {
        match inline {
            Inline::SourceText { provenance, .. } => {
                source_parts.push(provenance.locator.part.clone());
            }
            Inline::Link { content, .. } => inspect_inlines(content, references, source_parts),
            Inline::FootnoteReference(label) => references.push(label.clone()),
            _ => {}
        }
    }
}

fn corrupt_crc(bytes: &mut [u8], target: &str) {
    let mut cursor = 0;
    while cursor + 46 <= bytes.len() {
        let Some(relative) = bytes[cursor..].windows(4).position(|window| window == b"PK\x01\x02")
        else {
            break;
        };
        let central = cursor + relative;
        let name_len = usize::from(u16::from_le_bytes([bytes[central + 28], bytes[central + 29]]));
        let name = &bytes[central + 46..central + 46 + name_len];
        if name == target.as_bytes() {
            let local = usize::try_from(u32::from_le_bytes(
                bytes[central + 42..central + 46].try_into().unwrap(),
            ))
            .unwrap();
            bytes[local + 14..local + 18].copy_from_slice(&0_u32.to_le_bytes());
            bytes[central + 16..central + 20].copy_from_slice(&0_u32.to_le_bytes());
            return;
        }
        cursor = central + 46 + name_len;
    }
    panic!("target central entry not found");
}

#[test]
fn unsupported_members_are_reported_and_renamed_documents_keep_their_identity() {
    let pptx = std::fs::read(crate::test_fixture_root().join("small/pptx/normal.pptx")).unwrap();
    let bytes = archive(
        &[
            ("source.js", b"const value = '<article><p>wrong</p></article>';"),
            ("slides.md", pptx.as_slice()),
            ("kept.txt", b"kept text"),
        ],
        false,
    );
    let result =
        convert(bytes.clone(), ConversionOptions::default(), ExecutionOptions::default()).unwrap();
    result.document.validate().unwrap();
    assert!(result.markdown.contains("Corpus"));
    assert!(result.markdown.contains("kept text"));
    assert!(!result.markdown.contains("wrong"));
    assert!(result.diagnostics.iter().any(|item| item.code == "zip.entry.failed"
        && item.locator.as_ref().and_then(|loc| loc.part.as_deref()) == Some("source.js")));
    let mut options = ConversionOptions::default();
    options.error_policy = crate::ErrorPolicy::Strict;
    let strict = convert(bytes, options, ExecutionOptions::default()).unwrap();
    assert!(strict.markdown.contains("Corpus"));
    assert!(strict.diagnostics.iter().any(|item| item.code == "zip.entry.failed"
        && item.locator.as_ref().and_then(|loc| loc.part.as_deref()) == Some("source.js")));
}

#[test]
fn rar_signatures_are_terminal_and_nested_failures_keep_good_members() {
    for signature in [b"Rar!\x1a\x07\x00".as_slice(), b"Rar!\x1a\x07\x01\x00".as_slice()] {
        for name in [
            "input.rar",
            "input.txt",
            "input.zip",
            "input.bin",
            "input.drawio",
            "input.md",
            "input.js",
        ] {
            let request = ConversionRequest::new(InputRef::bytes(signature.to_vec(), Some(name)));
            let error = block_on(default_engine().unwrap().convert(request)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported, "{name}: {error}");
            assert!(error.to_string().contains("extract"));
            assert_eq!(error.reason_code(), "archiveExtractionRequired");
            let result = convert(
                archive(&[(name, signature), ("报告（保留）.txt", "正文完整".as_bytes())], false),
                ConversionOptions::default(),
                ExecutionOptions::default(),
            )
            .unwrap();
            assert_eq!(result.outcome(), ConversionOutcome::Degraded);
            assert!(result.markdown.contains("正文完整"));
            assert!(result.diagnostics.iter().any(|d| d.message.contains("extract")
                && d.locator.as_ref().and_then(|l| l.part.as_deref()) == Some(name)));
        }
    }
    for signature in [b"Rar!\x1a".as_slice(), b"Rar!\x1a\x07\x01".as_slice()] {
        let request =
            ConversionRequest::new(InputRef::bytes(signature.to_vec(), Some("truncated.rar")));
        let error = block_on(default_engine().unwrap().convert(request)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Malformed);
        assert!(error.to_string().contains("truncated"));
    }
    let mut plain =
        ConversionRequest::new(InputRef::bytes(b"ordinary Rar! text".to_vec(), Some("plain.rar")));
    assert_eq!(
        block_on(default_engine().unwrap().convert(plain.clone())).unwrap_err().code(),
        ErrorCode::Unsupported
    );
    plain.hint.format = Some(InputFormat::Text);
    assert!(
        block_on(default_engine().unwrap().convert(plain))
            .unwrap()
            .markdown
            .contains("ordinary Rar")
    );
    let request =
        ConversionRequest::new(InputRef::bytes(b"ordinary Rar! text".to_vec(), Some("plain.txt")));
    assert!(
        block_on(default_engine().unwrap().convert(request))
            .unwrap()
            .markdown
            .contains("ordinary Rar")
    );
    let mut forced =
        ConversionRequest::new(InputRef::bytes(b"Rar!\x1a\x07\x00".to_vec(), Some("actual.rar")));
    forced.hint.format = Some(InputFormat::Json);
    let error = block_on(default_engine().unwrap().convert(forced)).unwrap_err();
    assert_ne!(error.reason_code(), "archiveExtractionRequired");
    assert_eq!(error.code(), ErrorCode::Malformed);
    let only = archive(&[("only.rar", b"Rar!\x1a\x07\x00")], false);
    assert_eq!(
        convert(only, ConversionOptions::default(), ExecutionOptions::default())
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
}

#[test]
fn unicode_archive_names_survive_provenance_and_unsafe_aliases_fail() {
    for name in ["目录/报告（最终）.txt", "café.txt", "e\u{301}.txt", "straße.txt", "😀.txt"]
    {
        let result = convert(
            archive(&[(name, b"kept body")], false),
            ConversionOptions::default(),
            ExecutionOptions::default(),
        )
        .unwrap();
        assert!(result.markdown.contains("kept body"));
        assert!(
            result
                .document
                .blocks
                .iter()
                .any(|node| node.provenance.locator.part.as_deref() == Some(name))
        );
    }
    for (a, b) in [
        ("café.txt", "cafe\u{301}.txt"),
        ("straße.txt", "STRASSE.txt"),
        ("x", "x/y.txt"),
        ("A/x.txt", "a/y.txt"),
    ] {
        for entries in [
            [(a, b"a".as_slice()), (b, b"b".as_slice())],
            [(b, b"b".as_slice()), (a, b"a".as_slice())],
        ] {
            assert_eq!(
                convert(
                    archive(&entries, false),
                    ConversionOptions::default(),
                    ExecutionOptions::default()
                )
                .unwrap_err()
                .code(),
                ErrorCode::Malformed
            );
        }
    }
}

#[test]
fn recoverable_rar_retains_the_same_terminal_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::RecoveryStore::open(directory.path().join("recovery")).unwrap();
    let token = store.create_token().unwrap();
    let engine = default_engine().unwrap();
    for _ in 0..2 {
        let request = ConversionRequest::new(InputRef::bytes(
            b"Rar!\x1a\x07\x01\x00".to_vec(),
            Some("renamed.zip"),
        ));
        let error = block_on(engine.convert_recoverable(request, &store, &token)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unsupported);
        assert_eq!(error.reason_code(), "archiveExtractionRequired");
    }
}

#[test]
fn generic_zip_identity_overrides_document_package_suffixes() {
    let bytes = archive(&[("body.txt", b"generic ZIP body")], false);
    for name in [
        "renamed.docx",
        "renamed.pptx",
        "renamed.xlsx",
        "renamed.epub",
        "renamed.odt",
        "renamed.rar",
        "renamed.py",
        "renamed",
    ] {
        let mut request = ConversionRequest::new(InputRef::bytes(bytes.clone(), Some(name)));
        request.options.ocr.policy = crate::OcrPolicy::Off;
        let context =
            crate::ExecutionContext::new(request.execution.clone(), request.options.limits.clone());
        let result =
            block_on(default_engine().unwrap().convert_with_context(request, context.clone()))
                .unwrap();
        assert_eq!(context.detected_format(), Some(InputFormat::Zip), "{name}");
        assert!(result.markdown.contains("generic ZIP body"));
        result.document.validate().unwrap();
    }
}
