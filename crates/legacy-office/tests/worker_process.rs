//! End-to-end authority, process, protocol, and resource-lifecycle fixtures.

use into_markdown_core::{ConversionError, ExecutionOptions, InputFormat, ResourceLimits};
use into_markdown_legacy_office::{LegacyOfficeRuntime, NormalizedFormat, RuntimeConfig};
use object::Object as _;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn fixture_worker() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_legacy-office-test-worker") {
        return PathBuf::from(path);
    }
    let runfiles = PathBuf::from(std::env::var_os("TEST_SRCDIR").expect("Bazel runfiles root"));
    runfiles
        .join(std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "into_markdown".into()))
        .join("crates/legacy-office/legacy_office_test_worker")
}

fn configured_runtime() -> (tempfile::TempDir, LegacyOfficeRuntime) {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let worker_name = if cfg!(windows) { "worker.exe" } else { "worker" };
    let kit_file_name = if cfg!(windows) { "fixture-kit.exe" } else { "fixture-kit" };
    let kit_name = format!("runtime/{kit_file_name}");
    let worker = root_path.join(worker_name);
    std::fs::create_dir(root_path.join("runtime")).unwrap();
    let kit = root_path.join(&kit_name);
    std::fs::copy(fixture_worker(), &worker).unwrap();
    std::fs::copy(fixture_worker(), &kit).unwrap();
    executable(&worker);
    executable(&kit);
    let kit_bytes = std::fs::read(&kit).unwrap();
    let kit_object = object::File::parse(kit_bytes.as_slice()).unwrap();
    assert!(kit_object.exports().unwrap().filter_map(Result::ok).any(|export| {
        matches!(
            export.name(),
            object::read::NameOrOrdinal::Name(b"libreofficekit_hook_2" | b"_libreofficekit_hook_2")
        )
    }));
    std::fs::write(root_path.join("LICENSE"), b"Apache-2.0 fixture only\n").unwrap();
    let target = target();
    let worker_entry = file_entry(&worker, worker_name, "worker");
    let kit_entry = file_entry(&kit, &kit_name, "kitLibrary");
    let license_entry = file_entry(&root_path.join("LICENSE"), "LICENSE", "license");
    let license_hash = license_entry["sha256"].as_str().unwrap().to_owned();
    let authority = json!({
        "schemaVersion": 1,
        "product": "LibreOffice",
        "version": "26.2.4.2-fixture",
        "sourceUrl": "https://www.libreoffice.org/download/download-libreoffice/",
        "targets": {
            target.name: {
                "artifactUrl": "https://example.invalid/fixture-runtime",
                "artifactBytes": 1,
                "artifactSha256": "0".repeat(64),
                "installRoot": "runtime",
                "kitLibrary": kit_name,
                "worker": worker_name,
                "files": [worker_entry, kit_entry, license_entry],
                "licenses": [{
                    "id": "repository-fixture",
                    "spdx": "Apache-2.0",
                    "noticePath": "LICENSE",
                    "noticeSha256": license_hash
                }],
                "abi": {
                    "binaryFormat": target.binary,
                    "architecture": target.architecture,
                    "libraryIdentity": kit_file_name,
                    "requiredExport": "libreofficekit_hook_2"
                },
                "limits": {
                    "addressSpaceOverheadBytes": 536_870_912_u64,
                    "fileSizeLimitBytes": 16_777_216_u64,
                    "openFileLimit": 64,
                    "processLimit": 1
                },
                "sandbox": {
                    "systemReadPaths": [],
                    "network": "deny",
                    "childProcesses": "deny"
                }
            }
        }
    });
    let authority_path = root_path.join("authority.json");
    std::fs::write(&authority_path, serde_json::to_vec_pretty(&authority).unwrap()).unwrap();
    let runtime = LegacyOfficeRuntime::new(RuntimeConfig::new(authority_path, root_path, worker));
    (root, runtime)
}

fn file_entry(path: &Path, relative: &str, role: &str) -> serde_json::Value {
    let bytes = std::fs::read(path).unwrap();
    json!({
        "path": relative,
        "bytes": bytes.len(),
        "sha256": format!("{:x}", Sha256::digest(&bytes)),
        "role": role
    })
}

struct Target {
    name: &'static str,
    binary: &'static str,
    architecture: &'static str,
}

fn target() -> Target {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => {
            Target { name: "aarch64-apple-darwin", binary: "mach-o", architecture: "aarch64" }
        }
        ("linux", "aarch64") => {
            Target { name: "aarch64-unknown-linux-gnu", binary: "elf", architecture: "aarch64" }
        }
        ("linux", "x86_64") => {
            Target { name: "x86_64-unknown-linux-gnu", binary: "elf", architecture: "x86_64" }
        }
        ("windows", "x86_64") => {
            Target { name: "x86_64-pc-windows-msvc", binary: "pe", architecture: "x86_64" }
        }
        target => panic!("unsupported integration-test target {target:?}"),
    }
}

#[cfg(unix)]
fn executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = path.metadata().unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(windows)]
fn executable(_: &Path) {}

fn limits() -> ResourceLimits {
    ResourceLimits {
        max_memory_bytes: 64 * 1024 * 1024,
        max_temporary_bytes: 64 * 1024 * 1024,
        ..ResourceLimits::default()
    }
}

#[test]
#[cfg_attr(windows, ignore = "Windows fake launch is covered by the injected launcher contract")]
fn fixed_worker_protocol_converts_all_families_and_releases_leases() {
    let (_root, runtime) = configured_runtime();
    for (source, expected) in [
        (InputFormat::Doc, NormalizedFormat::Docx),
        (InputFormat::Ppt, NormalizedFormat::Pptx),
        (InputFormat::Xls, NormalizedFormat::Xlsx),
    ] {
        let context =
            into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
        let package = runtime.convert(b"fixture:normal", source, 1024, &context).unwrap();
        assert_eq!(package.format, expected);
        assert!(package.bytes.starts_with(b"PK\x03\x04fixture-"));
        assert_eq!(package.runtime.version(), "26.2.4.2-fixture");
        assert_eq!(package.runtime.target(), target().name);
        assert_temporary_released(&context);
        drop(package);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
#[cfg_attr(windows, ignore = "Windows fake launch is covered by the injected launcher contract")]
fn crash_encryption_limit_and_timeout_are_stable_and_reaped() {
    let (_root, runtime) = configured_runtime();
    for (source, expected) in [
        (b"fixture:crash".as_slice(), "componentUnavailable"),
        (b"fixture:encrypted".as_slice(), "encrypted"),
    ] {
        let context =
            into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
        let error = runtime.convert(source, InputFormat::Doc, 1024, &context).unwrap_err();
        assert_eq!(error.code().as_str(), expected);
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_temporary_released(&context);
    }
    let context = into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
    assert!(matches!(
        runtime.convert(b"fixture:normal", InputFormat::Doc, 4, &context),
        Err(ConversionError::ResourceLimit { .. })
    ));
    assert_eq!(context.reserved_memory_bytes(), 0);

    let context = into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
    assert!(matches!(
        runtime.convert(b"fixture:temporary-limit", InputFormat::Doc, 1024, &context),
        Err(ConversionError::ResourceLimit { limit: "legacy_office_worker_temporary", .. })
    ));
    assert_eq!(context.reserved_memory_bytes(), 0);
    assert_temporary_released(&context);

    let context = into_markdown_core::ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(Duration::from_millis(25)),
            ..ExecutionOptions::default()
        },
        limits(),
    );
    assert!(matches!(
        runtime.convert(b"fixture:hang", InputFormat::Doc, 1024, &context),
        Err(ConversionError::Timeout)
    ));
    assert_eq!(context.reserved_memory_bytes(), 0);
}

fn assert_temporary_released(context: &into_markdown_core::ExecutionContext) {
    let reservation = context.reserve_temporary(64 * 1024 * 1024).unwrap();
    drop(reservation);
}

#[test]
#[ignore = "requires an explicitly audited local LibreOffice runtime bundle"]
fn manual_native_runtime_conversion() {
    let value = |name: &str| PathBuf::from(std::env::var_os(name).expect(name));
    let root = value("INTO_MD_LEGACY_OFFICE_ROOT").canonicalize().unwrap();
    let runtime = LegacyOfficeRuntime::new(RuntimeConfig::new(
        value("INTO_MD_LEGACY_OFFICE_AUTHORITY"),
        root,
        value("INTO_MD_LEGACY_OFFICE_WORKER"),
    ));
    let input = std::fs::read(value("INTO_MD_LEGACY_OFFICE_DOC_FIXTURE")).unwrap();
    let context = into_markdown_core::ExecutionContext::new(
        ExecutionOptions { timeout: Some(Duration::from_mins(1)), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let package = runtime.convert(&input, InputFormat::Doc, 256 * 1024 * 1024, &context).unwrap();
    assert_eq!(package.format, NormalizedFormat::Docx);
    assert!(package.bytes.starts_with(b"PK"));
}
