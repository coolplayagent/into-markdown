//! Platform-neutral post-install smoke contract.

mod agent_skill;
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
use std::io;
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
    let mut validated = request.validate()?;
    let run_root = validated.create_run_root()?;
    let mut cases = Vec::new();
    let mut capabilities = Vec::new();

    let result: Result<(), String> = (|| {
        let projection = manifest::verify_install(&validated)?;
        if projection.target != platform.target() {
            return Err("archive manifest target differs from the executing platform".into());
        }
        validated.rust_library = rust_consumer::extract_installed_library(
            &validated.rust_library,
            &run_root.join("rust"),
        )?;
        match agent_skill::verify(&validated.install_root) {
            Ok(()) => cases.push(CaseResult::passed(
                "agent-skill",
                "installed Agent Skill structure and license verified",
            )),
            Err(error) => cases.push(CaseResult::failed("agent-skill", "skillInvalid", &error)),
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
        rust_consumer::run(&validated, &run_root, platform.target(), executor, &mut cases);
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
    if let Err(error) = validate_cleanup_root(run_root) {
        return CleanupResult::failed(&format!(
            "refusing unsafe temporary cleanup target: {error}"
        ));
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    let mut delay = std::time::Duration::from_millis(50);
    loop {
        match remove_run_root_once(run_root) {
            Ok(()) if !run_root.exists() => return CleanupResult::clean(),
            Ok(()) if Instant::now() >= deadline => {
                return CleanupResult::failed("temporary run directory remains");
            }
            Err(error) if Instant::now() >= deadline => {
                return CleanupResult::failed(&format!(
                    "cannot remove temporary run directory after bounded retries: {error}"
                ));
            }
            Ok(()) | Err(_) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(500));
            }
        }
    }
}

fn validate_cleanup_root(run_root: &std::path::Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(run_root)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "run root is not a directory"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "run root is a reparse point"));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn remove_run_root_once(run_root: &std::path::Path) -> io::Result<()> {
    fs::remove_dir_all(run_root)
}

#[cfg(windows)]
fn remove_run_root_once(run_root: &std::path::Path) -> io::Result<()> {
    remove_windows_owned_entry(run_root)
}

#[cfg(windows)]
fn remove_windows_owned_entry(path: &std::path::Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let attributes = metadata.file_attributes();
    let directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        make_windows_entry_deletable(path, attributes)?;
        return if directory { fs::remove_dir(path) } else { fs::remove_file(path) };
    }
    if directory {
        for entry in fs::read_dir(path)? {
            remove_windows_owned_entry(&entry?.path())?;
        }
        make_windows_entry_deletable(path, attributes)?;
        fs::remove_dir(path)
    } else {
        make_windows_entry_deletable(path, attributes)?;
        fs::remove_file(path)
    }
}

#[cfg(windows)]
fn make_windows_entry_deletable(path: &std::path::Path, attributes: u32) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;

    const DELETE_BLOCKING_ATTRIBUTES: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

    if attributes & DELETE_BLOCKING_ATTRIBUTES == 0 {
        return Ok(());
    }
    let absolute = std::path::absolute(path)?;
    let encoded = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    let verbatim_prefix = [u16::from(b'\\'), u16::from(b'\\'), u16::from(b'?'), u16::from(b'\\')];
    let unc_prefix = [u16::from(b'\\'), u16::from(b'\\')];
    let mut wide = if encoded.starts_with(&verbatim_prefix) {
        encoded
    } else if encoded.starts_with(&unc_prefix) {
        "\\\\?\\UNC\\".encode_utf16().chain(encoded.into_iter().skip(2)).collect::<Vec<_>>()
    } else {
        "\\\\?\\".encode_utf16().chain(encoded).collect::<Vec<_>>()
    };
    wide.push(0);
    // SAFETY: `wide` is a NUL-terminated absolute UTF-16 path owned for the duration of this
    // synchronous call. The entry is inside the validated, runner-owned temporary root.
    if unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_NORMAL) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

    #[cfg(windows)]
    #[test]
    fn cleanup_clears_windows_delete_blocking_attributes() {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;

        let temporary = tempfile::tempdir().unwrap();
        let run_root = temporary.path().join("owned-run-root");
        let system_directory = run_root.join("system-directory");
        fs::create_dir_all(&system_directory).unwrap();
        fs::write(system_directory.join("readonly.bin"), b"owned").unwrap();
        for (path, attributes) in [
            (&system_directory, 0x0000_0002 | 0x0000_0004),
            (&system_directory.join("readonly.bin"), 0x0000_0001),
        ] {
            let absolute = std::path::absolute(path).unwrap();
            let mut wide = "\\\\?\\"
                .encode_utf16()
                .chain(absolute.as_os_str().encode_wide())
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            // SAFETY: the vector is a NUL-terminated absolute UTF-16 path to this test's file.
            assert_ne!(unsafe { SetFileAttributesW(wide.as_mut_ptr(), attributes) }, 0);
        }
        let cleanup = cleanup_run_root(&run_root);
        assert!(cleanup.clean, "{}", cleanup.detail);
        assert!(!run_root.exists());
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
        fn execute(&self, spec: CommandSpec<'_>) -> Result<CommandOutput, String> {
            let mut output = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "unexpected command".to_owned())?;
            if output.stdout == b"dynamic-installed-metadata" {
                let manifest_index = spec
                    .arguments
                    .iter()
                    .position(|argument| argument == "--manifest-path")
                    .ok_or_else(|| "metadata command omits manifest path".to_owned())?;
                let manifest = spec
                    .arguments
                    .get(manifest_index + 1)
                    .ok_or_else(|| "metadata command omits manifest value".to_owned())?;
                output.stdout = serde_json::to_vec(&serde_json::json!({
                    "packages": [{"id":"root","name":"into-markdown","source":null,"manifest_path":manifest}],
                    "resolve":{"root":"root","nodes":[{"id":"root","dependencies":[]}]}
                }))
                .unwrap();
            }
            Ok(output)
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn complete_success_contract_writes_report_and_cleans_everything() {
        let temporary = tempfile::tempdir().unwrap();
        let install = temporary.path().join("install");
        let fixtures = install.join("share/into-markdown/smoke/fixtures");
        let rust = install.join("lib/into-markdown-rust.zip");
        fs::create_dir_all(install.join("bin")).unwrap();
        fs::create_dir_all(rust.parent().unwrap()).unwrap();
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
        {
            use std::io::Write as _;
            let mut archive = zip::ZipWriter::new(fs::File::create(&rust).unwrap());
            let options = zip::write::SimpleFileOptions::default().unix_permissions(0o644);
            for (name, contents) in [
                ("Cargo.toml", b"[package]\nname='into-markdown'\nversion='0.0.0'\n".as_slice()),
                ("Cargo.lock", b"version = 4\n".as_slice()),
                ("vendor/example/checksum", b"vendor".as_slice()),
            ] {
                archive.start_file(name, options).unwrap();
                archive.write_all(contents).unwrap();
            }
            archive.finish().unwrap();
        }
        agent_skill::create_fixture(&install);
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
            version: "0.0.0".into(),
            source_revision: "a".repeat(40),
            components: vec![],
            files,
            license_materials: vec![],
            ffmpeg_evidence: None,
            native_transformations: vec![],
        };
        fs::write(&manifest, serde_json::to_vec(&projection).unwrap()).unwrap();
        let temp_root = temporary.path().join("empty-temp");
        let report = temporary.path().join("report.json");
        let audio_fixture = temporary.path().join("licensed-speech.wav");
        fs::create_dir(&temp_root).unwrap();
        fs::write(&audio_fixture, b"external fixture").unwrap();
        let fake_toolchain = temporary.path().join("fake-toolchain");
        let test_rustc = fake_toolchain.join("bin/rustc.exe");
        fs::create_dir_all(fake_toolchain.join("bin")).unwrap();
        fs::create_dir_all(fake_toolchain.join("lib/rustlib/x86_64-pc-windows-msvc/bin")).unwrap();
        fs::write(&test_rustc, b"rustc").unwrap();
        fs::write(
            fake_toolchain.join("lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe"),
            b"rust-lld",
        )
        .unwrap();
        let request = SmokeRequest {
            install_root: install,
            into_md: binary.clone(),
            rust_library: rust.with_extension(""),
            manifest,
            audio_fixture,
            fixtures,
            temp_root: temp_root.clone(),
            report: report.clone(),
            archive_sha256: "a".repeat(64),
            cargo: binary.clone(),
            rustc: test_rustc,
            pdfium_library: None,
            timeout_seconds: NonZeroU64::new(1).unwrap(),
            cancel_file: None,
        };
        let missing_library_request = request.clone();
        let formats = serde_json::to_vec(&authority.entries).unwrap();
        let doctor = br#"[{"id":"runtime.pdfium","status":"missing","detail":"install PDFium"},{"id":"runtime.ocr","status":"missing","detail":"install OCR"},{"id":"runtime.asr","status":"missing","detail":"run into-md setup media"}]"#.to_vec();
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
        let pdf = br#"{"code":"componentUnavailable","exitCode":9,"message":"repair the installed into-md Core package, then run diagnostics again"}"#.to_vec();
        let image =
            br#"{"code":"componentUnavailable","exitCode":9,"message":"run into-md setup ocr"}"#
                .to_vec();
        let mut outputs =
            VecDeque::from([ok(b"into-md 0.0.0\n".to_vec()), ok(formats), ok(doctor)]);
        outputs.extend(markdown.into_iter().map(ok));
        outputs.push_back(ok(dto));
        outputs.push_back(CommandOutput { exit_code: Some(3), stdout: vec![], stderr: corrupt });
        outputs.push_back(ok(b"Installed ZIP smoke\n".to_vec()));
        outputs.push_back(CommandOutput { exit_code: Some(9), stdout: vec![], stderr: pdf });
        outputs.push_back(CommandOutput { exit_code: Some(9), stdout: vec![], stderr: image });
        outputs.push_back(CommandOutput {
            exit_code: Some(9),
            stdout: vec![],
            stderr: br#"{"code":"componentUnavailable","exitCode":9,"message":"run into-md setup media"}"#.to_vec(),
        });
        outputs.push_back(ok(b"Corpus Alpha \xe4\xb8\xad\xe6\x96\x87\n".to_vec()));
        outputs.push_back(ok(b"## Slide 1: Corpus \xe4\xbd\xa0\xe5\xa5\xbd \xe2\x80\x93 \xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82\n\nCorpus \xe4\xbd\xa0\xe5\xa5\xbd \xe2\x80\x93 \xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82\n\nEnglish fran\xc3\xa7ais\n\n### Speaker notes\n\nNota \xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e\n\n## Slide 2: Second layout\n\nSecond layout\n\n\xd9\x85\xd8\xb1\xd8\xad\xd8\xa8\xd8\xa7\n\n![](<data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=>)\n".to_vec()));
        outputs.push_back(ok(
            br#"{"assets":[{"mediaType":"image/png","dataBase64":"AA=="}]}"#.to_vec()
        ));
        outputs.push_back(ok(b"## Sheet: Corpus\n\n|  |  |  |\n| --- | --- | --- |\n| Corpus | `=TRUE [cached: true]` | 42\\.5 |\n| 2024\\-01\\-01 00:00:00 | `=SUM(1,2) [cached: 3]` | `=cmd` |\n".to_vec()));
        outputs.push_back(ok(b"dynamic-installed-metadata".to_vec()));
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
        fs::remove_file(rust).unwrap();
        assert!(missing_library_request.validate().unwrap_err().contains("Rust library archive"));
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
                    sha1: None,
                    sha256: format!("{:x}", Sha256::digest(&bytes)),
                    component_id: None,
                    embedded_components: vec![],
                });
            }
        }
    }
}
