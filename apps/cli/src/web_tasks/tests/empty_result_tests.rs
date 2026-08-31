use super::*;
use into_markdown::{
    Asset, AssetId, Block, BlockNode, ConversionResult, Document, NodeId, Provenance,
    ProvenanceKind, SourceLocator,
};
use std::io::Cursor;
use zip::write::SimpleFileOptions;

#[test]
fn empty_source_and_empty_content_share_the_web_terminal_contract() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();

    let mut empty_request =
        WebTaskRequest { format: Some(InputFormat::Text), ..WebTaskRequest::default() };
    empty_request.options.error_policy = into_markdown::ErrorPolicy::BestEffort;
    let mut upload = backend.begin_upload_configured("empty.txt", Some(3), empty_request).unwrap();
    upload.write_chunk(b" \n").unwrap();
    let empty = upload.finish().unwrap();
    let empty = wait_terminal(&backend, &empty.id);
    assert_eq!(empty.status, TaskStatus::Succeeded);
    let markdown =
        empty.artifacts.iter().find(|artifact| artifact.kind == ArtifactKind::Markdown).unwrap();
    assert_eq!(markdown.byte_len, 0);
    let diagnostics =
        empty.artifacts.iter().find(|artifact| artifact.kind == ArtifactKind::Diagnostics).unwrap();
    let (mut diagnostics_file, _) = backend.artifact(&empty.id, &diagnostics.storage_key).unwrap();
    let mut diagnostics_json = String::new();
    diagnostics_file.read_to_string(&mut diagnostics_json).unwrap();
    assert!(diagnostics_json.contains("emptySource"), "{diagnostics_json}");
    assert!(backend.web_record(empty).unwrap().failure.is_none());

    let docx = alt_chunk_only_docx();
    let request = WebTaskRequest { format: Some(InputFormat::Docx), ..WebTaskRequest::default() };
    let mut upload = backend
        .begin_upload_configured("omitted.docx", Some(u64::try_from(docx.len()).unwrap()), request)
        .unwrap();
    upload.write_chunk(&docx).unwrap();
    let omitted = upload.finish().unwrap();
    let omitted = wait_terminal(&backend, &omitted.id);
    assert_eq!(omitted.status, TaskStatus::Failed);
    assert!(omitted.artifacts.is_empty());
    let failure = backend.web_record(omitted).unwrap().failure.unwrap();
    assert_eq!(failure.code, "malformed");
    assert_eq!(failure.reason_code.as_deref(), Some("emptyContent"));
}

#[test]
fn web_rejects_external_uri_only_asset_results_it_cannot_publish() {
    let id = AssetId("external".into());
    let result = ConversionResult::new(
        Document {
            blocks: vec![BlockNode {
                id: NodeId("image".into()),
                block: Block::Image { asset: id.clone(), alt: None },
                provenance: Provenance {
                    kind: ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..Document::default()
        },
        String::new(),
        vec![Asset {
            id,
            filename: Some("external.png".into()),
            media_type: "image/png".into(),
            bytes: Vec::new(),
            external_uri: Some("https://example.invalid/external.png".into()),
        }],
        Vec::new(),
        Vec::new(),
    );

    let error = validate_web_result_delivery(&result).unwrap_err();
    assert!(matches!(
        error,
        WebTaskError::Conversion {
            ref code,
            reason_code: Some(ref reason_code),
            ref stage,
        } if code == "malformed" && reason_code == "emptyContent" && stage == "resultDelivery"
    ));
}

fn alt_chunk_only_docx() -> Vec<u8> {
    let parts = [
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rDocument" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#,
        ),
        (
            "word/document.xml",
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:altChunk/></w:body></w:document>"#,
        ),
    ];
    let mut output = Cursor::new(Vec::new());
    {
        let mut archive = zip::ZipWriter::new(&mut output);
        for (name, bytes) in parts {
            archive.start_file(name, SimpleFileOptions::default()).unwrap();
            archive.write_all(bytes.as_bytes()).unwrap();
        }
        archive.finish().unwrap();
    }
    output.into_inner()
}

#[test]
fn rar_failure_reason_survives_web_task_persistence() {
    let temporary = tempfile::tempdir().unwrap();
    let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
    let mut upload =
        backend.begin_upload_configured("renamed.txt", Some(8), WebTaskRequest::default()).unwrap();
    upload.write_chunk(b"Rar!\x1a\x07\x01\x00").unwrap();
    let record = upload.finish().unwrap();
    let record = wait_terminal(&backend, &record.id);
    assert_eq!(record.status, TaskStatus::Failed);
    assert!(record.artifacts.is_empty());
    let failure = backend.web_record(record).unwrap().failure.unwrap();
    assert_eq!(failure.code, "unsupported");
    assert_eq!(failure.reason_code.as_deref(), Some("archiveExtractionRequired"));
    assert!(!failure.retryable);
}
