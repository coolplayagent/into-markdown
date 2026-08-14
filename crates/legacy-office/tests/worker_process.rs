//! End-to-end authority, process, protocol, and resource-lifecycle fixtures.

use into_markdown_core::{ConversionError, ExecutionOptions, InputFormat, ResourceLimits};
use into_markdown_legacy_office::{LegacyOfficeRuntime, NormalizedFormat, RuntimeConfig};
use object::Object as _;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
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
    let system_libraries = fixture_system_libraries(&kit_object)
        .into_iter()
        .map(|identity| {
            json!({
                "path": fixture_system_library_path(&identity),
                "identity": identity,
            })
        })
        .collect::<Vec<_>>();
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
                    "systemLibraries": system_libraries,
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

#[cfg(target_os = "macos")]
fn fixture_system_library_path(identity: &str) -> String {
    identity.to_owned()
}

#[cfg(target_os = "linux")]
fn fixture_system_library_path(identity: &str) -> String {
    for root in [
        "/lib/aarch64-linux-gnu",
        "/lib/x86_64-linux-gnu",
        "/lib64",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib64",
    ] {
        let candidate = Path::new(root).join(identity);
        if let Ok(path) = candidate.canonicalize()
            && path.is_file()
        {
            return path.to_string_lossy().into_owned();
        }
    }
    panic!("fixture system library {identity} is not installed")
}

#[cfg(windows)]
fn fixture_system_library_path(identity: &str) -> String {
    format!(r"C:\Windows\System32\{}", identity.to_ascii_lowercase())
}

#[cfg(target_os = "macos")]
fn fixture_system_libraries(object: &object::File<'_>) -> std::collections::BTreeSet<String> {
    let object::File::MachO64(macho) = object else {
        panic!("macOS fixture worker must be a 64-bit Mach-O binary");
    };
    let endian = macho.endian();
    let mut commands = macho.macho_load_commands().unwrap();
    let mut libraries = std::collections::BTreeSet::new();
    while let Some(command) = commands.next().unwrap() {
        if let Some(dylib) = command.dylib().unwrap() {
            let identity = command.string(endian, dylib.dylib.name).unwrap();
            libraries.insert(std::str::from_utf8(identity).unwrap().to_owned());
        }
    }
    libraries
}

#[cfg(not(target_os = "macos"))]
fn fixture_system_libraries(object: &object::File<'_>) -> std::collections::BTreeSet<String> {
    object
        .imports()
        .unwrap()
        .filter_map(Result::ok)
        .map(|import| std::str::from_utf8(import.library()).unwrap().to_owned())
        .collect()
}

fn process_test_guard() -> MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

fn worker_temporary_roots() -> std::collections::BTreeSet<PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("into-md-legacy-office-"))
        .map(|entry| entry.path())
        .collect()
}

#[test]
#[cfg_attr(windows, ignore = "Windows fake launch is covered by the injected launcher contract")]
fn fixed_worker_protocol_converts_all_families_and_releases_leases() {
    let _guard = process_test_guard();
    let (_root, runtime) = configured_runtime();
    for (source, expected) in [
        (InputFormat::Doc, NormalizedFormat::Docx),
        (InputFormat::Ppt, NormalizedFormat::Pptx),
        (InputFormat::Xls, NormalizedFormat::Xlsx),
    ] {
        let temporary_before = worker_temporary_roots();
        let context =
            into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
        let package = runtime.convert(b"fixture:normal", source, 1024, &context).unwrap();
        assert_eq!(worker_temporary_roots(), temporary_before);
        assert_eq!(package.format, expected);
        assert!(package.bytes.starts_with(b"PK\x03\x04"));
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
    let _guard = process_test_guard();
    let (_root, runtime) = configured_runtime();
    for (source, expected) in [
        (b"fixture:crash".as_slice(), "componentUnavailable"),
        (b"fixture:encrypted".as_slice(), "encrypted"),
    ] {
        let context =
            into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
        let error = runtime.convert(source, InputFormat::Doc, 1024, &context).unwrap_err();
        assert_eq!(error.code().as_str(), expected, "{error:?}");
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

#[test]
#[cfg_attr(windows, ignore = "Windows fake launch is covered by the injected launcher contract")]
fn early_partial_and_nonzero_responses_are_protocol_failures_and_reaped() {
    let _guard = process_test_guard();
    let (_root, runtime) = configured_runtime();
    for source in [
        vec![0_u8; 4 * 1024 * 1024 + 17],
        vec![0_u8; 4 * 1024 * 1024 + 18],
        vec![0_u8; 4 * 1024 * 1024 + 19],
        b"fixture:response-then-nonzero".to_vec(),
    ] {
        // Workspace feature unification can make the audited test-worker
        // snapshot larger than the ordinary 64 MiB fixture budget. Keep this
        // protocol regression's temporary allowance above that authenticated
        // snapshot so each case reaches the malicious IPC behavior it asserts.
        let mut protocol_limits = limits();
        protocol_limits.max_temporary_bytes = 128 * 1024 * 1024;
        let context =
            into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), protocol_limits);
        let error = runtime.convert(&source, InputFormat::Doc, 1024, &context).unwrap_err();
        assert!(
            matches!(
                error,
                ConversionError::ComponentUnavailable { ref detail, .. }
                    if detail == "workerProtocol"
            ),
            "source length {} returned {error:?}",
            source.len()
        );
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_temporary_released(&context);
    }
}

#[test]
#[cfg_attr(windows, ignore = "Windows fake launch is covered by the injected launcher contract")]
fn pk_garbage_and_claimed_family_confusion_are_protocol_failures() {
    let _guard = process_test_guard();
    let (_root, runtime) = configured_runtime();
    for source in [b"fixture:pk-garbage".as_slice(), b"fixture:wrong-family".as_slice()] {
        let context =
            into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
        let error = runtime.convert(source, InputFormat::Doc, 16 * 1024, &context).unwrap_err();
        assert!(matches!(
            error,
            ConversionError::ComponentUnavailable { ref detail, .. } if detail == "workerProtocol"
        ));
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn native_linux_sandbox_blocks_cross_process_and_io_uring_syscalls() {
    let _guard = process_test_guard();
    let (_root, runtime) = configured_runtime();
    let context = into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
    let package = runtime
        .convert(b"fixture:sandbox-syscalls", InputFormat::Doc, 16 * 1024, &context)
        .unwrap();
    assert_eq!(package.format, NormalizedFormat::Docx);
    assert!(package.bytes.starts_with(b"PK\x03\x04"));
}

#[test]
#[cfg(unix)]
fn atomic_worker_path_swaps_never_execute_unverified_inode() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let _guard = process_test_guard();
    let (root, runtime) = configured_runtime();
    let swap = tempfile::tempdir().unwrap();
    let marker = swap.path().join("canary-executed");
    let canary = swap.path().join("canary-worker");
    std::fs::write(
        &canary,
        format!("#!/bin/sh\nprintf canary > '{}'\nexit 99\n", marker.display()),
    )
    .unwrap();
    executable(&canary);
    let worker_name = if cfg!(windows) { "worker.exe" } else { "worker" };
    let worker = root.path().join(worker_name).canonicalize().unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let swap_stopped = Arc::clone(&stopped);
    let swap_worker = worker.clone();
    let swap_canary = canary.clone();
    let swapper = std::thread::spawn(move || {
        while !swap_stopped.load(Ordering::Relaxed) {
            atomic_swap(&swap_worker, &swap_canary);
            std::thread::sleep(Duration::from_micros(100));
            atomic_swap(&swap_worker, &swap_canary);
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let mut successes = 0_usize;
    for _ in 0..8 {
        let context =
            into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
        match runtime.convert(b"fixture:normal", InputFormat::Doc, 16 * 1024, &context) {
            Ok(package) => {
                successes += 1;
                assert_eq!(package.format, NormalizedFormat::Docx);
            }
            Err(ConversionError::ComponentUnavailable { .. }) => {}
            Err(error) => panic!("unexpected swap result: {error:?}"),
        }
        assert!(!marker.exists());
        if successes >= 2 {
            break;
        }
    }
    stopped.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
    if successes == 0 {
        let context =
            into_markdown_core::ExecutionContext::new(ExecutionOptions::default(), limits());
        let package =
            runtime.convert(b"fixture:normal", InputFormat::Doc, 16 * 1024, &context).unwrap();
        assert_eq!(package.format, NormalizedFormat::Docx);
        successes += 1;
    }
    assert!(successes > 0);
    assert!(!marker.exists());
}

#[cfg(target_os = "macos")]
fn atomic_swap(left: &Path, right: &Path) {
    use std::os::unix::ffi::OsStrExt as _;
    let left = std::ffi::CString::new(left.as_os_str().as_bytes()).unwrap();
    let right = std::ffi::CString::new(right.as_os_str().as_bytes()).unwrap();
    // SAFETY: both C strings name live regular-file fixtures on one filesystem.
    assert_eq!(unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) }, 0);
}

#[cfg(target_os = "linux")]
fn atomic_swap(left: &Path, right: &Path) {
    use std::os::unix::ffi::OsStrExt as _;
    let left = std::ffi::CString::new(left.as_os_str().as_bytes()).unwrap();
    let right = std::ffi::CString::new(right.as_os_str().as_bytes()).unwrap();
    // SAFETY: both C strings name live regular-file fixtures on one filesystem.
    assert_eq!(
        unsafe {
            libc::renameat2(libc::AT_FDCWD, left.as_ptr(), libc::AT_FDCWD, right.as_ptr(), 2)
        },
        0
    );
}

fn assert_temporary_released(context: &into_markdown_core::ExecutionContext) {
    let reservation = context.reserve_temporary(64 * 1024 * 1024).unwrap();
    drop(reservation);
}

#[test]
#[ignore = "requires an explicitly audited local LibreOffice runtime bundle"]
fn manual_native_runtime_conversion() {
    let _guard = process_test_guard();
    let value = |name: &str| PathBuf::from(std::env::var_os(name).expect(name));
    let root = value("INTO_MD_LEGACY_OFFICE_ROOT").canonicalize().unwrap();
    let runtime = LegacyOfficeRuntime::new(RuntimeConfig::new(
        value("INTO_MD_LEGACY_OFFICE_AUTHORITY"),
        root,
        value("INTO_MD_LEGACY_OFFICE_WORKER"),
    ));
    for (variable, source, expected) in [
        ("INTO_MD_LEGACY_OFFICE_DOC_FIXTURE", InputFormat::Doc, NormalizedFormat::Docx),
        ("INTO_MD_LEGACY_OFFICE_PPT_FIXTURE", InputFormat::Ppt, NormalizedFormat::Pptx),
        ("INTO_MD_LEGACY_OFFICE_XLS_FIXTURE", InputFormat::Xls, NormalizedFormat::Xlsx),
    ] {
        let input = std::fs::read(value(variable)).unwrap();
        let context = into_markdown_core::ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_mins(1)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let package = runtime.convert(&input, source, 256 * 1024 * 1024, &context).unwrap();
        assert_eq!(package.format, expected);
        assert!(package.bytes.starts_with(b"PK\x03\x04"));
    }
}
