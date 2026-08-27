//! Signed installation through real process-v1 manager dispatch.

#![allow(clippy::struct_field_names, clippy::too_many_lines)]

use base64::Engine as _;
use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};
use into_markdown_plugin_manager::{PluginManager, TrustedSigners};
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Write as _};
use std::path::PathBuf;
use zip::write::SimpleFileOptions;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileAuthority {
    path: String,
    bytes: u64,
    sha256: String,
    executable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Signature {
    signed_payload_version: u32,
    algorithm: String,
    key_id: String,
    public_key_base64: String,
    public_key_sha256: String,
    signed_payload_sha256: String,
    signature_base64: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    id: String,
    version: String,
    protocol: String,
    supported_targets: BTreeSet<String>,
    entrypoints: BTreeMap<String, String>,
    runtime_manifest: Option<String>,
    files: Vec<FileAuthority>,
    signature: Signature,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignedPayload<'a> {
    signature_domain: &'static str,
    signed_payload_version: u32,
    algorithm: &'a str,
    key_id: &'a str,
    public_key_sha256: &'a str,
    schema_version: u32,
    id: &'a str,
    version: &'a str,
    protocol: &'a str,
    supported_targets: &'a BTreeSet<String>,
    entrypoints: &'a BTreeMap<String, String>,
    runtime_manifest: &'a Option<String>,
    files: &'a [FileAuthority],
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn target() -> &'static str {
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    return "x86_64-pc-windows-msvc";
    #[cfg(all(target_arch = "aarch64", target_os = "windows"))]
    return "aarch64-pc-windows-msvc";
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    return "x86_64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    return "aarch64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    return "aarch64-apple-darwin";
    #[allow(unreachable_code)]
    "unsupported"
}

fn fixture_executable() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_plugin_manager_process_fixture") {
        if PathBuf::from(path).is_file() {
            return fs::canonicalize(path).expect("Cargo fixture executable");
        }
    }
    if let Some(runfiles) = std::env::var_os("RUNFILES_DIR") {
        let executable = if cfg!(windows) {
            "plugin_manager_process_fixture.exe"
        } else {
            "plugin_manager_process_fixture"
        };
        for repository in ["_main", "into_markdown"] {
            let candidate = PathBuf::from(&runfiles)
                .join(repository)
                .join("crates/plugin-manager")
                .join(executable);
            if candidate.is_file() {
                return fs::canonicalize(candidate).expect("Bazel fixture executable");
            }
        }
    }
    if let Some(candidate) = manifest_runfile(&[
        "_main/crates/plugin-manager/plugin_manager_process_fixture.exe",
        "into_markdown/crates/plugin-manager/plugin_manager_process_fixture.exe",
        "_main/crates/plugin-manager/plugin_manager_process_fixture",
        "into_markdown/crates/plugin-manager/plugin_manager_process_fixture",
    ]) {
        return candidate;
    }
    panic!("process fixture executable is unavailable")
}

fn manifest_runfile(logical_paths: &[&str]) -> Option<PathBuf> {
    let manifest = PathBuf::from(std::env::var_os("RUNFILES_MANIFEST_FILE")?);
    let metadata = fs::metadata(&manifest).ok()?;
    if metadata.len() > 64 * 1024 * 1024 {
        return None;
    }
    let contents = fs::read_to_string(manifest).ok()?;
    for line in contents.lines() {
        let Some((logical, physical)) = line.split_once(' ') else {
            continue;
        };
        if logical_paths.contains(&logical) {
            let candidate = PathBuf::from(physical);
            if candidate.is_file() {
                return fs::canonicalize(candidate).ok();
            }
        }
    }
    None
}

#[test]
fn signed_install_prepares_and_executes_real_process_guest() {
    let executable = fixture_executable();
    let executable_bytes = fs::read(&executable).expect("fixture bytes");
    let entry = if cfg!(windows) { "bin/plugin.exe" } else { "bin/plugin" };
    let key = Ed25519KeyPair::from_seed_unchecked(&[29_u8; 32]).expect("key");
    let public = key.public_key().as_ref();
    let fingerprint = sha256(public);
    let files = vec![FileAuthority {
        path: entry.into(),
        bytes: executable_bytes.len() as u64,
        sha256: sha256(&executable_bytes),
        executable: true,
    }];
    let mut manifest = Manifest {
        schema_version: 1,
        id: "fixture.manager-process".into(),
        version: "1.0.0".into(),
        protocol: "process-v1".into(),
        supported_targets: BTreeSet::from([target().into()]),
        entrypoints: BTreeMap::from([(target().into(), entry.into())]),
        runtime_manifest: None,
        files,
        signature: Signature {
            signed_payload_version: 1,
            algorithm: "ed25519".into(),
            key_id: "publisher.process-e2e".into(),
            public_key_base64: base64::engine::general_purpose::STANDARD.encode(public),
            public_key_sha256: fingerprint.clone(),
            signed_payload_sha256: String::new(),
            signature_base64: String::new(),
        },
    };
    let payload = serde_json::to_vec(&SignedPayload {
        signature_domain: "into-markdown/plugin-package/v1",
        signed_payload_version: 1,
        algorithm: &manifest.signature.algorithm,
        key_id: &manifest.signature.key_id,
        public_key_sha256: &manifest.signature.public_key_sha256,
        schema_version: 1,
        id: &manifest.id,
        version: &manifest.version,
        protocol: &manifest.protocol,
        supported_targets: &manifest.supported_targets,
        entrypoints: &manifest.entrypoints,
        runtime_manifest: &manifest.runtime_manifest,
        files: &manifest.files,
    })
    .expect("payload");
    manifest.signature.signed_payload_sha256 = sha256(&payload);
    manifest.signature.signature_base64 =
        base64::engine::general_purpose::STANDARD.encode(key.sign(&payload).as_ref());
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o700);
    writer.start_file("plugin.json", options).expect("manifest entry");
    writer
        .write_all(&serde_json::to_vec(&manifest).expect("manifest json"))
        .expect("manifest bytes");
    writer.start_file(entry, options).expect("executable entry");
    writer.write_all(&executable_bytes).expect("executable bytes");
    let package = writer.finish().expect("package").into_inner();

    let temporary = tempfile::tempdir().expect("temporary");
    let anchor = temporary.path().join("anchor");
    #[cfg(windows)]
    into_markdown_process_plugin::create_windows_plugin_store_directory(&anchor)
        .expect("private anchor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::create_dir(&anchor).expect("anchor");
        fs::set_permissions(&anchor, fs::Permissions::from_mode(0o700)).expect("private anchor");
    }
    let manager = PluginManager::open_scoped(
        &anchor,
        std::path::Path::new("plugins"),
        TrustedSigners {
            fingerprints: BTreeMap::from([("publisher.process-e2e".into(), fingerprint)]),
            revoked: BTreeSet::new(),
            revoked_fingerprints: BTreeSet::new(),
        },
    )
    .expect("manager");
    let execution = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    manager.install_bytes(&package, Some(&sha256(&package)), &execution).expect("install");
    let prepared = manager
        .process_manifest(
            "fixture.manager-process",
            into_markdown_process_plugin::RuntimePolicy::default(),
            &execution,
        )
        .expect("prepare");
    let second = manager
        .process_manifest(
            "fixture.manager-process",
            into_markdown_process_plugin::RuntimePolicy::default(),
            &execution,
        )
        .expect("prepare a second runtime from the same store");
    manager
        .verify("fixture.manager-process", &execution)
        .expect("store remains available while snapshots are retained");
    // Dispatch must consume the manager-owned verified snapshot directly. A
    // zero temporary-byte budget still permits the fresh working directory and
    // inline request, but would reject the former second runtime-tree copy.
    let dispatch_limits = ResourceLimits { max_temporary_bytes: 0, ..ResourceLimits::default() };
    let dispatch_execution = ExecutionContext::new(ExecutionOptions::default(), dispatch_limits);
    let output = prepared
        .execute(
            into_markdown_process_plugin::PluginRequest {
                request_id: "manager-e2e",
                input_format: "fixture",
                source_name: Some("fixture.bin"),
                source: b"ok",
                parameters_json: None,
            },
            &dispatch_execution,
        )
        .expect("execute");
    assert_eq!(output.result.markdown, "manager-process-ok");
    let second_output = second
        .execute(
            into_markdown_process_plugin::PluginRequest {
                request_id: "manager-e2e-second",
                input_format: "fixture",
                source_name: Some("fixture.bin"),
                source: b"ok",
                parameters_json: None,
            },
            &dispatch_execution,
        )
        .expect("execute second retained runtime");
    assert_eq!(second_output.result.markdown, "manager-process-ok");
}
