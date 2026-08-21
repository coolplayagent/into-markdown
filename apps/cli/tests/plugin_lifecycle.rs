// SPDX-License-Identifier: Apache-2.0
//! Isolated signed-package lifecycle through the real CLI and both runtimes.

use base64::Engine as _;
use into_markdown_plugin_manager::{
    PackageFile, PackageManifest, PackageSignature, canonical_signed_payload,
};
use into_markdown_plugin_wasi::{
    WASMTIME_VERSION, WasiCapabilities, WasiLimits, WasiPluginManifest,
};
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use zip::write::SimpleFileOptions;

#[cfg(windows)]
fn add_world_writable_ace(path: &Path) {
    use std::os::windows::fs::MetadataExt as _;

    let system_root = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"));
    assert!(system_root.is_absolute(), "SystemRoot must be absolute");
    let system_root = fs::canonicalize(system_root).expect("canonical SystemRoot");
    let system32 = fs::canonicalize(system_root.join("System32")).expect("System32");
    let icacls = fs::canonicalize(system32.join("icacls.exe")).expect("trusted icacls.exe");
    assert_eq!(icacls.parent(), Some(system32.as_path()));
    let metadata = fs::symlink_metadata(&icacls).expect("icacls metadata");
    assert!(metadata.is_file() && metadata.file_attributes() & 0x400 == 0);
    let output = Command::new(&icacls)
        .arg(path)
        .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
        .output()
        .expect("run trusted icacls");
    assert!(output.status.success(), "icacls failed: {}", String::from_utf8_lossy(&output.stderr));
}

const WASI_COMPONENT: &[u8] =
    include_bytes!("../../../crates/plugin-wasi/tests/fixtures/guest.component.wasm");

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn target() -> &'static str {
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    return "x86_64-pc-windows-msvc";
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    return "x86_64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    return "aarch64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    return "aarch64-apple-darwin";
    #[allow(unreachable_code)]
    "unsupported"
}

fn process_fixture() -> PathBuf {
    if let Some(path) = std::env::var_os("INTO_MD_PLUGIN_PROCESS_FIXTURE") {
        return fs::canonicalize(path).expect("process fixture");
    }
    if let Some(runfiles) = std::env::var_os("RUNFILES_DIR") {
        let name = if cfg!(windows) {
            "plugin_manager_process_fixture.exe"
        } else {
            "plugin_manager_process_fixture"
        };
        for repository in ["_main", "into_markdown"] {
            let candidate =
                PathBuf::from(&runfiles).join(repository).join("crates/plugin-manager").join(name);
            if candidate.is_file() {
                return fs::canonicalize(candidate).expect("Bazel process fixture");
            }
        }
    }
    panic!("INTO_MD_PLUGIN_PROCESS_FIXTURE was not provided")
}

fn cli_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_into-md") {
        return fs::canonicalize(path).expect("Cargo CLI");
    }
    if let Some(path) = std::env::var_os("INTO_MD_BIN") {
        return fs::canonicalize(path).expect("CLI");
    }
    panic!("into-md binary unavailable")
}

fn signed_package(
    id: &str,
    protocol: &str,
    entry: &str,
    files: Vec<(String, Vec<u8>)>,
    runtime_manifest: Option<String>,
    seed: [u8; 32],
    key_id: &str,
) -> (Vec<u8>, String) {
    let key = Ed25519KeyPair::from_seed_unchecked(&seed).expect("key");
    let public = key.public_key().as_ref();
    let fingerprint = sha256(public);
    let authorities = files
        .iter()
        .map(|(path, bytes)| PackageFile {
            path: path.clone(),
            bytes: bytes.len() as u64,
            sha256: sha256(bytes),
        })
        .collect();
    let mut manifest = PackageManifest {
        schema_version: 1,
        id: id.into(),
        version: "1.0.0".into(),
        protocol: protocol.into(),
        supported_targets: BTreeSet::from([target().into()]),
        entrypoints: BTreeMap::from([(target().into(), entry.into())]),
        runtime_manifest,
        files: authorities,
        signature: PackageSignature {
            signed_payload_version: 1,
            algorithm: "ed25519".into(),
            key_id: key_id.into(),
            public_key_base64: base64::engine::general_purpose::STANDARD.encode(public),
            public_key_sha256: fingerprint.clone(),
            signed_payload_sha256: String::new(),
            signature_base64: String::new(),
        },
    };
    let payload = canonical_signed_payload(&manifest).expect("payload");
    manifest.signature.signed_payload_sha256 = sha256(&payload);
    manifest.signature.signature_base64 =
        base64::engine::general_purpose::STANDARD.encode(key.sign(&payload).as_ref());

    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o700);
    writer.start_file("plugin.json", options).expect("manifest");
    writer.write_all(&serde_json::to_vec(&manifest).expect("json")).expect("manifest bytes");
    for (path, bytes) in files {
        writer.start_file(path, options).expect("file");
        writer.write_all(&bytes).expect("file bytes");
    }
    (writer.finish().expect("archive").into_inner(), fingerprint)
}

fn invoke(binary: &Path, cwd: &Path, user_data: &Path, arguments: &[&str]) -> Output {
    Command::new(binary)
        .args(arguments)
        .current_dir(cwd)
        .env("INTO_MARKDOWN_USER_DATA_HOME", user_data)
        .env("HOME", user_data)
        .env("XDG_CONFIG_HOME", user_data.join("xdg"))
        .env("APPDATA", user_data.join("appdata"))
        .env("LOCALAPPDATA", user_data.join("localappdata"))
        .output()
        .expect("run CLI")
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation}: status={:?}, stdout={}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn exercise(
    binary: &Path,
    cwd: &Path,
    user_data: &Path,
    id: &str,
    package_path: &Path,
    package_sha: &str,
    key_id: &str,
    fingerprint: &str,
    input: &Path,
    input_format: &str,
    output_marker: &str,
    scope: &str,
) {
    let package = package_path.to_str().expect("package path");
    for (operation, arguments) in [
        (
            "install",
            vec![
                "plugins",
                "install",
                package,
                "--sha256",
                package_sha,
                "--signing-key-id",
                key_id,
                "--signing-key-sha256",
                fingerprint,
                "--scope",
                scope,
            ],
        ),
        ("verify", vec!["plugins", "verify", id, "--scope", scope, "--json"]),
        ("disable", vec!["plugins", "disable", id, "--scope", scope]),
    ] {
        assert_success(&invoke(binary, cwd, user_data, &arguments), operation);
    }
    if scope == "project" {
        let project_config =
            fs::read_to_string(cwd.join(".into-markdown.toml")).expect("project config pin");
        assert!(project_config.contains(id), "project pin missing");
        let global_config =
            fs::read_to_string(user_data.join("into-markdown/config.toml")).expect("global config");
        assert!(!global_config.contains(id), "project pin leaked into global config");
    }

    let disabled = invoke(
        binary,
        cwd,
        user_data,
        &[
            "plugins",
            "run",
            id,
            input.to_str().expect("input"),
            "--input-format",
            input_format,
            "--scope",
            scope,
        ],
    );
    assert!(!disabled.status.success(), "disabled plugin executed");
    assert!(String::from_utf8_lossy(&disabled.stderr).contains("not enabled"));

    assert_success(
        &invoke(binary, cwd, user_data, &["plugins", "enable", id, "--scope", scope]),
        "enable",
    );
    let run = invoke(
        binary,
        cwd,
        user_data,
        &[
            "plugins",
            "run",
            id,
            input.to_str().expect("input"),
            "--input-format",
            input_format,
            "--scope",
            scope,
        ],
    );
    assert_success(&run, "run");
    assert!(
        String::from_utf8_lossy(&run.stdout).contains(output_marker),
        "run output did not contain {output_marker:?}: {}",
        String::from_utf8_lossy(&run.stdout)
    );
    if output_marker == "assets/probe.txt" {
        let value: serde_json::Value = serde_json::from_slice(&run.stdout).expect("WASI JSON");
        assert_eq!(value["document"]["blocks"][0]["provenance"]["provider"], "fixture");
        assert_eq!(value["resources"][0]["path"], "assets/probe.txt");
        assert_eq!(value["resources"][0]["bytes"], serde_json::json!([97, 98, 99]));
    }
    assert_success(
        &invoke(binary, cwd, user_data, &["plugins", "remove", id, "--scope", scope]),
        "remove",
    );
    let absent =
        invoke(binary, cwd, user_data, &["plugins", "verify", id, "--scope", scope, "--json"]);
    assert!(!absent.status.success(), "removed plugin still verified");
}

#[test]
fn isolated_cli_lifecycle_executes_signed_process_and_wasi_plugins() {
    let temporary = tempfile::tempdir().expect("temporary");
    let cwd = temporary.path().join("project");
    let user_data = temporary.path().join("user-data");
    fs::create_dir(&cwd).expect("project");
    #[cfg(windows)]
    into_markdown_process_plugin::create_windows_plugin_store_directory(&user_data)
        .expect("private Windows user data");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::create_dir(&user_data).expect("user data");
        fs::set_permissions(&user_data, fs::Permissions::from_mode(0o700)).expect("private");
    }
    let user_data = fs::canonicalize(user_data).expect("canonical user data");
    let binary = cli_binary();

    let executable = fs::read(process_fixture()).expect("process fixture bytes");
    let process_entry = if cfg!(windows) { "bin/plugin.exe" } else { "bin/plugin" };
    let (process_package, process_fingerprint) = signed_package(
        "fixture.manager-process",
        "process-v1",
        process_entry,
        vec![(process_entry.into(), executable)],
        None,
        [31; 32],
        "publisher.cli-process",
    );
    let process_path = temporary.path().join("process.zip");
    fs::write(&process_path, &process_package).expect("process package");
    let process_input = temporary.path().join("process-input");
    fs::write(&process_input, b"ok").expect("process input");
    exercise(
        &binary,
        &cwd,
        &user_data,
        "fixture.manager-process",
        &process_path,
        &sha256(&process_package),
        "publisher.cli-process",
        &process_fingerprint,
        &process_input,
        "fixture",
        "manager-process-ok",
        "global",
    );

    let runtime = WasiPluginManifest {
        id: "fixture".into(),
        protocol: "wasi-v1".into(),
        wasi_preview: "preview2".into(),
        runtime_version: WASMTIME_VERSION.into(),
        component_sha256: sha256(WASI_COMPONENT),
        component_bytes: WASI_COMPONENT.len() as u64,
        supported_targets: BTreeSet::from([target().into()]),
        capabilities: WasiCapabilities::default(),
        limits: WasiLimits {
            fuel: 50_000_000,
            max_linear_memory_bytes: 32 * 1024 * 1024,
            max_output_bytes: 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            max_resources: 16,
            max_resource_bytes: 1024 * 1024,
        },
    };
    let runtime_bytes = serde_json::to_vec(&runtime).expect("runtime");
    let (wasi_package, wasi_fingerprint) = signed_package(
        "fixture",
        "wasi-v1",
        "plugin.component.wasm",
        vec![
            ("plugin.component.wasm".into(), WASI_COMPONENT.to_vec()),
            ("runtime.json".into(), runtime_bytes),
        ],
        Some("runtime.json".into()),
        [37; 32],
        "publisher.cli-wasi",
    );
    let wasi_path = temporary.path().join("wasi.zip");
    fs::write(&wasi_path, &wasi_package).expect("WASI package");
    let wasi_input = temporary.path().join("valid-resource");
    fs::write(&wasi_input, b"fixture").expect("WASI input");
    exercise(
        &binary,
        &cwd,
        &user_data,
        "fixture",
        &wasi_path,
        &sha256(&wasi_package),
        "publisher.cli-wasi",
        &wasi_fingerprint,
        &wasi_input,
        "fixture",
        "assets/probe.txt",
        "global",
    );
    exercise(
        &binary,
        &cwd,
        &user_data,
        "fixture",
        &wasi_path,
        &sha256(&wasi_package),
        "publisher.cli-wasi",
        &wasi_fingerprint,
        &wasi_input,
        "fixture",
        "assets/probe.txt",
        "project",
    );
}

#[test]
fn real_cli_rejects_permissive_user_data_anchor_before_writing() {
    let temporary = tempfile::tempdir().expect("temporary");
    let cwd = temporary.path().join("project");
    let unsafe_anchor = temporary.path().join("unsafe-user-data");
    fs::create_dir(&cwd).expect("project");
    #[cfg(windows)]
    {
        into_markdown_process_plugin::create_windows_plugin_store_directory(&unsafe_anchor)
            .expect("private anchor baseline");
        add_world_writable_ace(&unsafe_anchor);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::create_dir(&unsafe_anchor).expect("unsafe anchor");
        fs::set_permissions(&unsafe_anchor, fs::Permissions::from_mode(0o777))
            .expect("permissive mode");
    }
    #[cfg(windows)]
    assert!(
        into_markdown_process_plugin::verify_windows_plugin_store_path(&unsafe_anchor).is_err(),
        "fixture must have a non-private inherited DACL"
    );
    let unsafe_anchor = fs::canonicalize(unsafe_anchor).expect("canonical unsafe anchor");
    let sentinel = temporary.path().join("outside-sentinel");
    fs::write(&sentinel, b"unchanged").expect("sentinel");
    let output = invoke(
        &cli_binary(),
        &cwd,
        &unsafe_anchor,
        &["plugins", "install", "missing.zip", "--sha256", &"0".repeat(64), "--scope", "global"],
    );
    assert!(!output.status.success(), "permissive anchor was accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("anchor")
            || String::from_utf8_lossy(&output.stderr).contains("DACL")
    );
    assert!(!unsafe_anchor.join("into-markdown").exists());
    assert_eq!(fs::read(&sentinel).expect("sentinel"), b"unchanged");
}
