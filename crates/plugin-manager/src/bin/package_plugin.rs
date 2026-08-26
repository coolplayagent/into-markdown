// SPDX-License-Identifier: Apache-2.0
//! Reproducible signer for into-markdown plugin packages.

#![forbid(unsafe_code)]

use base64::Engine as _;
use into_markdown_plugin_manager::{
    PackageFile, PackageManifest, PackageSignature, canonical_signed_payload,
    validate_package_file_path,
};
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PACKAGE_INPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FILES: usize = 25_000;
const IO_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Template {
    schema_version: u32,
    id: String,
    version: String,
    protocol: String,
    supported_targets: BTreeSet<String>,
    entrypoints: BTreeMap<String, String>,
    runtime_manifest: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.len() != 5 {
        return Err("usage: package_plugin SOURCE TEMPLATE KEY_PKCS8 KEY_ID OUTPUT".into());
    }
    let source = PathBuf::from(&arguments[0]).canonicalize()?;
    let template_path = PathBuf::from(&arguments[1]);
    let key_path = PathBuf::from(&arguments[2]);
    let key_id = arguments[3].to_str().ok_or("KEY_ID must be UTF-8")?.to_owned();
    let output = PathBuf::from(&arguments[4]);
    if !source.is_dir() || output.exists() {
        return Err("SOURCE must be a directory and OUTPUT must not exist".into());
    }
    let template_bytes = bounded_read(&template_path, 1024 * 1024)?;
    let template: Template = serde_json::from_slice(&template_bytes)?;
    let key_bytes = bounded_read(&key_path, 64 * 1024)?;
    // OpenSSL emits PKCS#8 v1 for Ed25519 while ring's generator emits v2.
    // Both are standard DER encodings; v1 has no stored public key, so ring
    // derives it from the private seed before the package fingerprint is made.
    let key = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&key_bytes)
        .map_err(|_| "invalid Ed25519 PKCS#8 key")?;
    let public = key.public_key().as_ref();
    let fingerprint = digest(public);

    let paths = collect_files(&source)?;
    let mut total = 0_u64;
    let mut files = Vec::with_capacity(paths.len());
    for (relative, path) in &paths {
        validate_package_file_path(relative)
            .map_err(|error| format!("package path rejected ({relative}): {error}"))?;
        let (bytes, sha256) = bounded_digest(path, MAX_FILE_BYTES)?;
        total = total.checked_add(bytes).ok_or("input size overflow")?;
        if total > MAX_PACKAGE_INPUT_BYTES {
            return Err("package input exceeds 2 GiB".into());
        }
        files.push(PackageFile {
            path: relative.clone(),
            bytes,
            sha256,
            executable: is_executable(path)?,
        });
    }
    let mut manifest = PackageManifest {
        schema_version: template.schema_version,
        id: template.id,
        version: template.version,
        protocol: template.protocol,
        supported_targets: template.supported_targets,
        entrypoints: template.entrypoints,
        runtime_manifest: template.runtime_manifest,
        files,
        signature: PackageSignature {
            signed_payload_version: 1,
            algorithm: "ed25519".to_owned(),
            key_id,
            public_key_base64: base64::engine::general_purpose::STANDARD.encode(public),
            public_key_sha256: fingerprint,
            signed_payload_sha256: String::new(),
            signature_base64: String::new(),
        },
    };
    let payload = canonical_signed_payload(&manifest)?;
    manifest.signature.signed_payload_sha256 = digest(&payload);
    manifest.signature.signature_base64 =
        base64::engine::general_purpose::STANDARD.encode(key.sign(&payload).as_ref());

    let output_file = File::options().create_new(true).read(true).write(true).open(output)?;
    let mut writer = zip::ZipWriter::new(output_file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    writer.start_file("plugin.json", options)?;
    writer.write_all(&serde_json::to_vec(&manifest)?)?;
    let mut buffer = io_buffer()?;
    for (authority, (_, path)) in manifest.files.iter().zip(paths) {
        writer.start_file(&authority.path, options)?;
        let mut source = File::open(path)?;
        let mut digest = Sha256::new();
        let mut copied = 0_u64;
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            copied = copied.checked_add(count as u64).ok_or("input size overflow")?;
            if copied > authority.bytes {
                return Err("input file changed while packaging".into());
            }
            digest.update(&buffer[..count]);
            writer.write_all(&buffer[..count])?;
        }
        if copied != authority.bytes || format!("{:x}", digest.finalize()) != authority.sha256 {
            return Err("input file changed while packaging".into());
        }
    }
    let file = writer.finish()?;
    file.sync_all()?;
    Ok(())
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err("input file size rejected".into());
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| "input file size overflow")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| "input file allocation rejected")?;
    File::open(path)?.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        return Err("input file changed while reading".into());
    }
    Ok(bytes)
}

fn bounded_digest(path: &Path, maximum: u64) -> Result<(u64, String), Box<dyn std::error::Error>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err("input file size rejected".into());
    }
    let mut source = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = io_buffer()?;
    let mut total = 0_u64;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total.checked_add(count as u64).ok_or("input size overflow")?;
        if total > maximum {
            return Err("input file size rejected".into());
        }
        digest.update(&buffer[..count]);
    }
    if total != metadata.len() {
        return Err("input file changed while reading".into());
    }
    Ok((total, format!("{:x}", digest.finalize())))
}

fn io_buffer() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(IO_BUFFER_BYTES).map_err(|_| "I/O buffer allocation rejected")?;
    buffer.resize(IO_BUFFER_BYTES, 0);
    Ok(buffer)
}

fn collect_files(root: &Path) -> Result<Vec<(String, PathBuf)>, Box<dyn std::error::Error>> {
    fn visit(
        root: &Path,
        directory: &Path,
        output: &mut Vec<(String, PathBuf)>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err("links are not package inputs".into());
            }
            if metadata.is_dir() {
                visit(root, &entry.path(), output)?;
            } else if metadata.is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .ok_or("non-UTF-8 path")?
                    .replace('\\', "/");
                if relative == "plugin.json" {
                    return Err("SOURCE must not contain plugin.json".into());
                }
                output.push((relative, entry.path()));
                if output.len() > MAX_FILES {
                    return Err(format!("package contains more than {MAX_FILES} files").into());
                }
            } else {
                return Err("special files are not package inputs".into());
            }
        }
        Ok(())
    }
    let mut output = Vec::new();
    visit(root, root, &mut output)?;
    output.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(output)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt as _;
    Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
// Keep one fallible cross-platform callback signature; Unix reads executable mode metadata.
#[allow(clippy::unnecessary_wraps)]
fn is_executable(_path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    Ok(false)
}
