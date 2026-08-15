//! Platform-neutral post-install smoke contract.

mod catalog;
mod cli_cases;
mod manifest;
mod path_policy;
mod platform;
mod process;
mod process_tree;
mod report;
mod request;
mod rust_consumer;

pub use platform::{HostPlatform, PlatformAdapter};
pub use report::{CapabilityResult, CaseResult, CaseStatus, CleanupResult, SmokeReport};
pub use request::SmokeRequest;

use crate::process::RealExecutor;
use std::fs;
use std::time::Instant;

/// Execute the complete installed-artifact contract and always attempt cleanup.
///
/// # Errors
///
/// Returns a stable validation or I/O error after writing the report when possible.
pub fn run(request: SmokeRequest) -> Result<SmokeReport, String> {
    run_with_platform(request, &HostPlatform)
}

/// Execute with an explicit platform adapter, allowing platform packages to
/// supply only the small process environment they own.
///
/// # Errors
///
/// Returns a stable validation or I/O error after writing the report when possible.
pub fn run_with_platform(
    request: SmokeRequest,
    platform: &dyn PlatformAdapter,
) -> Result<SmokeReport, String> {
    let executor = RealExecutor::new(platform.process_environment());
    run_with_executor(request, platform, &executor)
}

fn run_with_executor(
    request: SmokeRequest,
    platform: &dyn PlatformAdapter,
    executor: &dyn process::Executor,
) -> Result<SmokeReport, String> {
    let started = Instant::now();
    let validated = request.validate()?;
    let run_root = validated.create_run_root()?;
    let mut cases = Vec::new();
    let mut capabilities = Vec::new();

    let result: Result<(), String> = (|| {
        let projection = manifest::verify_install(&validated)?;
        if projection.target != platform.target() {
            return Err("archive manifest target differs from the executing platform".into());
        }
        let authority = catalog::load_authority(&validated, &projection)?;
        cli_cases::run(
            &validated,
            &run_root,
            &authority,
            &projection,
            executor,
            &mut cases,
            &mut capabilities,
        )?;
        rust_consumer::run(&validated, &run_root, executor, &mut cases);
        Ok(())
    })();

    if let Err(error) = result {
        cases.push(CaseResult::failed("runner", "runnerFailed", &error));
    }
    let cleanup = cleanup_run_root(&run_root);
    let report = SmokeReport::new(
        platform.platform(),
        platform.architecture(),
        validated.archive_sha256.clone(),
        cases,
        capabilities,
        cleanup,
        started.elapsed(),
    );
    report.write(&validated.report)?;
    if report.passed { Ok(report) } else { Err("installed smoke failed; inspect report".into()) }
}

fn cleanup_run_root(run_root: &std::path::Path) -> CleanupResult {
    match fs::remove_dir_all(run_root) {
        Ok(()) if !run_root.exists() => CleanupResult::clean(),
        Ok(()) => CleanupResult::failed("temporary run directory remains"),
        Err(error) => {
            CleanupResult::failed(&format!("cannot remove temporary run directory: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{CommandOutput, CommandSpec, Executor};
    use license_check::schema::{ArchiveFile, ArchiveFileKind, ArchiveProjection};
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeMap, VecDeque};
    use std::num::NonZeroU64;
    use std::sync::Mutex;

    #[test]
    fn cleanup_failure_is_a_failing_report_signal() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("not-a-directory");
        fs::write(&file, b"owned").unwrap();
        let cleanup = cleanup_run_root(&file);
        assert!(!cleanup.clean);
        assert!(file.exists());
    }

    struct TestPlatform;

    impl PlatformAdapter for TestPlatform {
        fn platform(&self) -> &'static str {
            "macos"
        }

        fn architecture(&self) -> &'static str {
            "aarch64"
        }

        fn target(&self) -> &'static str {
            "aarch64-apple-darwin"
        }

        fn process_environment(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
    }

    struct FakeExecutor(Mutex<VecDeque<CommandOutput>>);

    impl Executor for FakeExecutor {
        fn execute(&self, _: CommandSpec<'_>) -> Result<CommandOutput, String> {
            self.0.lock().unwrap().pop_front().ok_or_else(|| "unexpected command".into())
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn complete_success_contract_writes_report_and_cleans_everything() {
        let temporary = tempfile::tempdir().unwrap();
        let install = temporary.path().join("install");
        let fixtures = install.join("share/into-markdown/smoke/fixtures");
        let rust = install.join("lib/into-markdown-rust");
        fs::create_dir_all(install.join("bin")).unwrap();
        fs::create_dir_all(rust.join("vendor/example")).unwrap();
        for fixture in [
            "text/normal.txt",
            "docx/normal.docx",
            "docx/corrupt.docx",
            "epub/normal.epub",
            "msg/normal.msg",
            "rtf/normal.rtf",
            "pdf/structures.pdf",
            "ocr/ocr-english-clear-1.png",
            "pptx/normal.pptx",
            "xlsx/normal.xlsx",
            "xlsb/normal.xlsb",
            "odt/normal.odt",
            "ods/normal.ods",
            "odp/normal.odp",
            "legacy/normal.doc",
            "legacy/normal.ppt",
            "legacy/normal.xls",
        ] {
            let path = fixtures.join(fixture);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"fixture").unwrap();
        }
        let binary = install.join("bin/into-md");
        fs::write(&binary, b"installed binary").unwrap();
        fs::write(rust.join("Cargo.toml"), b"[package]\nname='into-markdown'\nversion='0.0.0'\n")
            .unwrap();
        fs::write(rust.join("Cargo.lock"), b"version = 4\n").unwrap();
        fs::write(rust.join("vendor/example/checksum"), b"vendor").unwrap();
        let authority = into_markdown_converters::core_catalog_authority().unwrap();
        fs::write(
            install.join("core-catalog.json"),
            serde_json::to_vec_pretty(&authority).unwrap(),
        )
        .unwrap();
        let mut files = Vec::new();
        collect_files(&install, &install, &mut files);
        let manifest = install.join("archive-manifest.json");
        let projection = ArchiveProjection {
            schema_version: 1,
            target: "aarch64-apple-darwin".into(),
            components: vec![],
            files,
            license_materials: vec![],
            ffmpeg_evidence: None,
        };
        fs::write(&manifest, serde_json::to_vec(&projection).unwrap()).unwrap();
        let temp_root = temporary.path().join("empty-temp");
        let report = temporary.path().join("report.json");
        fs::create_dir(&temp_root).unwrap();
        let request = SmokeRequest {
            install_root: install,
            into_md: binary.clone(),
            rust_library: rust.clone(),
            manifest,
            fixtures,
            temp_root: temp_root.clone(),
            report: report.clone(),
            archive_sha256: "a".repeat(64),
            cargo: binary.clone(),
            rustc: binary.clone(),
            pdfium_library: None,
            timeout_seconds: NonZeroU64::new(1).unwrap(),
            cancel_file: None,
        };
        let missing_library_request = request.clone();
        let formats = serde_json::to_vec(&authority.entries).unwrap();
        let doctor = br#"[{"id":"runtime.pdfium","status":"missing","detail":"install PDFium"},{"id":"runtime.ocr","status":"missing","detail":"install OCR"},{"id":"runtime.legacy-office","status":"missing","detail":"install legacy Office"}]"#.to_vec();
        let markdown = [
            b"Alpha \xe4\xb8\xad\xe6\x96\x87 line  \nSecond line\n".to_vec(),
            b"Corpus Alpha \xe4\xb8\xad\xe6\x96\x87\n".to_vec(),
            b"# Contents\n\n1. [Corpus chapter](<EPUB/chapter.xhtml#corpus>)\n\n# Corpus chapter\n\n# Corpus chapter\n\nAlpha EPUB text\\.\n".to_vec(),
            b"# Repository MSG\n\n<strong>From: </strong>Alice \\<alice@example\\.test\\>\n\n<strong>To: </strong>Bob \\<bob@example\\.test\\>\n\n<strong>Date: </strong>1970\\-01\\-01T00:00:00Z\n\n---\n\nPlain fixture body\n\n## Transport headers\n\n```rfc822\nMessage-ID: <repository@example.test>\nX-Offline: true\n```\n".to_vec(),
            b"Corpus <strong>Alpha</strong> \xe4\xb8\xad\xe6\x96\x87\n".to_vec(),
            b"## Slide 1: Corpus \xe4\xbd\xa0\xe5\xa5\xbd \xe2\x80\x93 \xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82\n\n<em>English fran\xc3\xa7ais</em>\n\n### Speaker notes\n\nNota \xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e\n\n## Slide 2: Second layout\n\n<em>\xd9\x85\xd8\xb1\xd8\xad\xd8\xa8\xd8\xa7</em>\n".to_vec(),
            b"## Sheet: Corpus\n\n|  |  |  |\n| --- | --- | --- |\n| Corpus | true | 42\\.5 |\n| 2024\\-01\\-01 00:00:00 | `=SUM(1,2) [cached: 3]` | `=cmd` |\n|  |  |  |\n\n![corpus pixel](<data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=>)\n\n![Corpus again](<data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=>)\n".to_vec(),
            b"## Sheet: Binary\n\n|  |  |  |\n| --- | --- | --- |\n| Binary value | true | 2024\\-01\\-01 00:00:00 |\n| `=1+2 [cached: 3]` |  |  |\n".to_vec(),
            b"## Corpus ODT\n\nAlpha <strong>\xe4\xb8\xad\xe6\x96\x87</strong>\n\n- item\n\n|  |  |\n| --- | --- |\n| A | B |\n".to_vec(),
            b"## Sheet: Data\n\n|  |  |\n| --- | --- |\n| Alpha | 1 |\n| tail |  |\n| tail |  |\n".to_vec(),
            b"## Slide 1: Corpus ODP\n\nAlpha \xe4\xb8\xad\xe6\x96\x87\n\n<strong>Speaker notes</strong>\n\nSpeaker cue\n".to_vec(),
        ];
        let dto = br#"{"schemaVersion":1,"markdown":"Alpha \u4e2d\u6587 line  \nSecond line\n","document":{"blocks":[{}]}}"#.to_vec();
        let corrupt = br#"{"code":"malformed","exitCode":3}"#.to_vec();
        let pdf = br#"{"code":"componentUnavailable","exitCode":9,"message":"install the pinned PDFium runtime and set PDFIUM_LIBRARY to its exact file"}"#.to_vec();
        let image = br#"{"code":"componentUnavailable","exitCode":9,"message":"run into-md models install pp-ocrv6-tiny-zh-en"}"#.to_vec();
        let metadata = serde_json::to_vec(&serde_json::json!({
            "packages": [{"id":"root","name":"into-markdown","source":null,"manifest_path":rust.join("Cargo.toml")}],
            "resolve":{"root":"root","nodes":[{"id":"root","dependencies":[]}]}
        }))
        .unwrap();
        let mut outputs =
            VecDeque::from([ok(b"into-md 0.0.0\n".to_vec()), ok(formats), ok(doctor)]);
        outputs.extend(markdown.into_iter().map(ok));
        outputs.push_back(ok(dto));
        outputs.push_back(CommandOutput { exit_code: Some(3), stdout: vec![], stderr: corrupt });
        outputs.push_back(ok(b"Installed ZIP smoke\n".to_vec()));
        outputs.push_back(CommandOutput { exit_code: Some(9), stdout: vec![], stderr: pdf });
        outputs.push_back(CommandOutput { exit_code: Some(9), stdout: vec![], stderr: image });
        let legacy = br#"{"code":"componentUnavailable","exitCode":9,"message":"install the authority-verified legacy Office runtime for this platform"}"#.to_vec();
        for _ in 0..3 {
            outputs.push_back(CommandOutput {
                exit_code: Some(9),
                stdout: vec![],
                stderr: legacy.clone(),
            });
        }
        outputs.push_back(ok(metadata));
        outputs.push_back(ok(vec![]));
        outputs.push_back(ok(vec![]));
        outputs.push_back(ok(
            br#"{"schemaVersion":1,"markdown":"Installed Rust consumer\n"}"#.to_vec()
        ));
        let executor = FakeExecutor(Mutex::new(outputs));
        let completed =
            run_with_executor(request, &TestPlatform, &executor).unwrap_or_else(|error| {
                let report_detail = fs::read_to_string(&report)
                    .unwrap_or_else(|report_error| format!("report unavailable: {report_error}"));
                panic!("{error}: {report_detail}");
            });
        assert!(completed.passed);
        assert!(completed.cleanup.clean);
        assert!(fs::read_dir(temp_root).unwrap().next().is_none());
        assert!(report.is_file());
        fs::remove_file(rust.join("Cargo.toml")).unwrap();
        assert!(missing_library_request.validate().unwrap_err().contains("Rust library must"));
    }

    fn ok(stdout: Vec<u8>) -> CommandOutput {
        CommandOutput { exit_code: Some(0), stdout, stderr: vec![] }
    }

    fn collect_files(
        root: &std::path::Path,
        directory: &std::path::Path,
        output: &mut Vec<ArchiveFile>,
    ) {
        for item in fs::read_dir(directory).unwrap() {
            let item = item.unwrap();
            if item.file_type().unwrap().is_dir() {
                collect_files(root, &item.path(), output);
            } else {
                let bytes = fs::read(item.path()).unwrap();
                let path =
                    item.path().strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                output.push(ArchiveFile {
                    kind: if path == "core-catalog.json" {
                        ArchiveFileKind::Generated
                    } else {
                        ArchiveFileKind::Project
                    },
                    path,
                    bytes: bytes.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(&bytes)),
                    component_id: None,
                    embedded_components: vec![],
                });
            }
        }
    }
}
