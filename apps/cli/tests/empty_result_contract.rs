//! Real-process contracts for exceptional empty conversion results.

use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::process::Command;
use zip::write::SimpleFileOptions;

fn binary() -> PathBuf {
    option_env!("CARGO_BIN_EXE_into-md")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("INTO_MD_BIN").map(PathBuf::from))
        .expect("Cargo or Bazel must provide the into-md binary")
}

#[test]
fn genuine_empty_sources_commit_existing_targets_with_complete_reports() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("input");
    let output = temporary.path().join("output");
    let report = temporary.path().join("report.json");
    std::fs::create_dir(&input).unwrap();
    let text = input.join("empty.txt");
    let markdown = input.join("empty.md");
    let docx = input.join("empty.docx");
    let xlsx = input.join("empty.xlsx");
    std::fs::write(&text, b" \r\n").unwrap();
    std::fs::write(&markdown, b"\xef\xbb\xbf\n").unwrap();
    std::fs::write(&docx, empty_docx()).unwrap();
    std::fs::write(&xlsx, empty_xlsx()).unwrap();

    let result = Command::new(binary())
        .args(["--no-config", "--output-dir"])
        .arg(&output)
        .args(["--report"])
        .arg(&report)
        .args([&text, &markdown, &docx, &xlsx])
        .output()
        .unwrap();

    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(result.stdout.is_empty());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(report["succeeded"], 4);
    assert_eq!(report["failed"], 0);
    for item in report["items"].as_array().unwrap() {
        assert_eq!(item["status"], "success");
        assert_eq!(item["outcome"], "complete");
        assert_eq!(item["reasonCode"], "emptySource");
        let target = PathBuf::from(item["output"].as_str().unwrap());
        assert!(target.is_file(), "missing committed target {target:?}");
    }
}

#[test]
fn empty_stdout_is_explicit_in_reports_and_structured_output() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("empty.txt");
    let report = temporary.path().join("stdout-report.json");
    std::fs::write(&input, b"\n").unwrap();

    let markdown = Command::new(binary())
        .args(["--no-config", "--report"])
        .arg(&report)
        .arg(&input)
        .output()
        .unwrap();
    assert!(markdown.status.success());
    assert!(markdown.stdout.is_empty());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(report["items"][0]["outcome"], "complete");
    assert_eq!(report["items"][0]["reasonCode"], "emptySource");

    let structured = Command::new(binary())
        .args(["--no-config", "--emit", "result-json"])
        .arg(&input)
        .output()
        .unwrap();
    assert!(structured.status.success());
    let result: serde_json::Value = serde_json::from_slice(&structured.stdout).unwrap();
    assert_eq!(result["markdown"], "");
    assert!(
        result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "emptySource")
    );
}

#[test]
fn empty_content_fails_batch_without_committing_a_false_success_target() {
    let temporary = tempfile::tempdir().unwrap();
    let good = temporary.path().join("good.txt");
    let omitted = temporary.path().join("omitted.docx");
    let output = temporary.path().join("output");
    let report = temporary.path().join("report.json");
    std::fs::write(&good, b"usable").unwrap();
    std::fs::write(&omitted, alt_chunk_only_docx()).unwrap();

    let result = Command::new(binary())
        .args(["--no-config", "--output-dir"])
        .arg(&output)
        .args(["--report"])
        .arg(&report)
        .args([&good, &omitted])
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(10), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(output.join("good.md").is_file());
    assert!(!std::fs::read(output.join("good.md")).unwrap().is_empty());
    assert!(!output.join("omitted.md").exists());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(report["succeeded"], 1);
    assert_eq!(report["failed"], 1);
    let failed =
        report["items"].as_array().unwrap().iter().find(|item| item["status"] == "failed").unwrap();
    assert_eq!(failed["outcome"], "failed");
    assert_eq!(failed["errorCode"], "malformed");
    assert_eq!(failed["reasonCode"], "emptyContent");
    assert!(failed["output"].as_str().unwrap().ends_with("omitted.md"));
}

#[test]
fn recoverable_omission_commits_nonempty_degraded_output() {
    let temporary = tempfile::tempdir().unwrap();
    let input = temporary.path().join("recovered.rtf");
    let output = temporary.path().join("output");
    let report = temporary.path().join("report.json");
    std::fs::write(&input, br"{\rtf1\ansi visible \unknowncontrol42 text}").unwrap();

    let result = Command::new(binary())
        .args(["--no-config", "--output-dir"])
        .arg(&output)
        .args(["--report"])
        .arg(&report)
        .arg(&input)
        .output()
        .unwrap();

    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(!std::fs::read(output.join("recovered.md")).unwrap().is_empty());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(report["succeeded"], 1);
    assert_eq!(report["failed"], 0);
    assert_eq!(report["items"][0]["status"], "success");
    assert_eq!(report["items"][0]["outcome"], "degraded");
    assert_eq!(report["items"][0]["reasonCode"], "rtf.unknownControlIgnored");
}

fn empty_docx() -> Vec<u8> {
    docx(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body></w:body></w:document>"#,
    )
}

fn alt_chunk_only_docx() -> Vec<u8> {
    docx(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:altChunk/></w:body></w:document>"#,
    )
}

fn docx(document: &str) -> Vec<u8> {
    zip(&[
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rDocument" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#,
        ),
        ("word/document.xml", document),
    ])
}

fn empty_xlsx() -> Vec<u8> {
    zip(&[
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Empty" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        ),
        (
            "xl/styles.xml",
            r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="0"/><fonts count="1"><font/></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0"/></cellStyleXfs><cellXfs count="1"><xf numFmtId="0"/></cellXfs></styleSheet>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData/></worksheet>"#,
        ),
    ])
}

fn zip(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    {
        let mut archive = zip::ZipWriter::new(&mut output);
        for (name, bytes) in parts {
            archive.start_file(*name, SimpleFileOptions::default()).unwrap();
            archive.write_all(bytes.as_bytes()).unwrap();
        }
        archive.finish().unwrap();
    }
    output.into_inner()
}
