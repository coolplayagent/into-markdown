use super::*;
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
