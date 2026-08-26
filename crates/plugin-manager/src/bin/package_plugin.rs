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

fn io_error(
    operation: &str,
    path: &Path,
    error: impl std::fmt::Display,
) -> Box<dyn std::error::Error> {
    format!("{operation} ({}): {error}", path.display()).into()
}

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

struct Signer {
    key: Ed25519KeyPair,
    key_id: String,
    public_key_base64: String,
    public_key_sha256: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.len() != 5 {
        return Err("usage: package_plugin SOURCE TEMPLATE KEY_PKCS8 KEY_ID OUTPUT".into());
    }
    let source_argument = PathBuf::from(&arguments[0]);
    let source = source_argument
        .canonicalize()
        .map_err(|error| io_error("cannot resolve package source", &source_argument, error))?;
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
    let public = key.public_key().as_ref().to_vec();
    let signer = Signer {
        key,
        key_id,
        public_key_base64: base64::engine::general_purpose::STANDARD.encode(&public),
        public_key_sha256: digest(&public),
    };
    write_package(&source, &output, template, &signer)
}

fn write_package(
    source: &Path,
    output: &Path,
    template: Template,
    signer: &Signer,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths = collect_files(source)?;
    let output_parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary =
        tempfile::Builder::new().prefix(".into-md-package-").tempfile_in(output_parent).map_err(
            |error| io_error("cannot create temporary package output", output_parent, error),
        )?;
    prepare_output_permissions(&temporary)?;
    let output_file = temporary.as_file().try_clone().map_err(|error| {
        io_error("cannot open temporary package output", temporary.path(), error)
    })?;
    let mut writer = zip::ZipWriter::new(output_file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    let mut buffer = io_buffer()?;
    let mut total = 0_u64;
    let mut files = Vec::with_capacity(paths.len());
    for (relative, path) in paths {
        validate_package_file_path(&relative)
            .map_err(|error| format!("package path rejected ({relative}): {error}"))?;
        let metadata = fs::metadata(&path)
            .map_err(|error| io_error("cannot inspect package input", &path, error))?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(format!("package input size rejected ({relative})").into());
        }
        total = total.checked_add(metadata.len()).ok_or("input size overflow")?;
        if total > MAX_PACKAGE_INPUT_BYTES {
            return Err("package input exceeds 2 GiB".into());
        }
        let executable = is_executable(&path)
            .map_err(|error| format!("cannot inspect package file mode ({relative}): {error}"))?;
        writer
            .start_file(&relative, options)
            .map_err(|error| format!("cannot start package entry ({relative}): {error}"))?;
        let mut source = File::open(&path)
            .map_err(|error| io_error("cannot open package input", &path, error))?;
        let mut file_digest = Sha256::new();
        let mut copied = 0_u64;
        loop {
            let count = source
                .read(&mut buffer)
                .map_err(|error| io_error("cannot read package input", &path, error))?;
            if count == 0 {
                break;
            }
            copied = copied.checked_add(count as u64).ok_or("input size overflow")?;
            if copied > metadata.len() {
                return Err(format!("package input changed while reading ({relative})").into());
            }
            file_digest.update(&buffer[..count]);
            writer.write_all(&buffer[..count]).map_err(|error| {
                io_error("cannot write temporary package output", output, error)
            })?;
        }
        if copied != metadata.len() {
            return Err(format!("package input changed while reading ({relative})").into());
        }
        files.push(PackageFile {
            path: relative,
            bytes: copied,
            sha256: format!("{:x}", file_digest.finalize()),
            executable,
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
            key_id: signer.key_id.clone(),
            public_key_base64: signer.public_key_base64.clone(),
            public_key_sha256: signer.public_key_sha256.clone(),
            signed_payload_sha256: String::new(),
            signature_base64: String::new(),
        },
    };
    let payload = canonical_signed_payload(&manifest)?;
    manifest.signature.signed_payload_sha256 = digest(&payload);
    manifest.signature.signature_base64 =
        base64::engine::general_purpose::STANDARD.encode(signer.key.sign(&payload).as_ref());

    publish_package(writer, temporary, output, options, &manifest)
}

fn publish_package(
    mut writer: zip::ZipWriter<File>,
    temporary: tempfile::NamedTempFile,
    output: &Path,
    options: SimpleFileOptions,
    manifest: &PackageManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    writer
        .start_file("plugin.json", options)
        .map_err(|error| format!("cannot start package manifest: {error}"))?;
    writer
        .write_all(&serde_json::to_vec(manifest)?)
        .map_err(|error| io_error("cannot write temporary package output", output, error))?;
    let file = writer.finish().map_err(|error| {
        format!("cannot finalize temporary package output ({}): {error}", output.display())
    })?;
    file.sync_all()
        .map_err(|error| io_error("cannot synchronize temporary package output", output, error))?;
    drop(file);
    temporary.persist_noclobber(output).map_err(|error| {
        format!("cannot publish package output ({}): {}", output.display(), error.error)
    })?;
    Ok(())
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let metadata =
        fs::metadata(path).map_err(|error| io_error("cannot inspect input", path, error))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err("input file size rejected".into());
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| "input file size overflow")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| "input file allocation rejected")?;
    File::open(path)
        .map_err(|error| io_error("cannot open input", path, error))?
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error("cannot read input", path, error))?;
    if bytes.len() as u64 != metadata.len() {
        return Err("input file changed while reading".into());
    }
    Ok(bytes)
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
        for entry in fs::read_dir(directory)
            .map_err(|error| io_error("cannot enumerate package directory", directory, error))?
        {
            let entry = entry.map_err(|error| {
                io_error("cannot read package directory entry", directory, error)
            })?;
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)
                .map_err(|error| io_error("cannot inspect package entry", &entry_path, error))?;
            if metadata.file_type().is_symlink() {
                return Err("links are not package inputs".into());
            }
            if metadata.is_dir() {
                visit(root, &entry_path, output)?;
            } else if metadata.is_file() {
                let relative = entry_path
                    .strip_prefix(root)?
                    .to_str()
                    .ok_or("non-UTF-8 path")?
                    .replace('\\', "/");
                if relative == "plugin.json" {
                    return Err("SOURCE must not contain plugin.json".into());
                }
                output.push((relative, entry_path));
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
fn prepare_output_permissions(
    output: &tempfile::NamedTempFile,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt as _;
    output
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(|error| io_error("cannot set package output permissions", output.path(), error))?;
    Ok(())
}

#[cfg(not(unix))]
// Windows does not use Unix mode bits; the protected release directory supplies its DACL.
#[allow(clippy::unnecessary_wraps)]
fn prepare_output_permissions(
    _output: &tempfile::NamedTempFile,
) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;

    fn template() -> Template {
        Template {
            schema_version: 1,
            id: "test.capability".to_owned(),
            version: "0.0.0".to_owned(),
            protocol: "process-v1".to_owned(),
            supported_targets: BTreeSet::from(["x86_64-pc-windows-msvc".to_owned()]),
            entrypoints: BTreeMap::from([(
                "x86_64-pc-windows-msvc".to_owned(),
                "bin/provider.exe".to_owned(),
            )]),
            runtime_manifest: None,
        }
    }

    fn signer() -> Signer {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .expect("generate test signing key");
        let key = Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("parse test signing key");
        let public = key.public_key().as_ref().to_vec();
        Signer {
            key,
            key_id: "test.publisher".to_owned(),
            public_key_base64: base64::engine::general_purpose::STANDARD.encode(&public),
            public_key_sha256: digest(&public),
        }
    }

    #[test]
    fn package_is_deterministic_valid_and_published_without_overwrite() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        fs::create_dir_all(source.join("bin")).expect("create fixture directories");
        fs::write(source.join("bin/provider.exe"), b"provider").expect("write provider fixture");
        fs::write(source.join("model.bin"), b"model").expect("write model fixture");
        let first = directory.path().join("first.imp");
        let second = directory.path().join("second.imp");
        let signer = signer();

        write_package(&source, &first, template(), &signer).expect("write first package");
        write_package(&source, &second, template(), &signer).expect("write second package");
        let first_bytes = fs::read(&first).expect("read first package");
        assert_eq!(first_bytes, fs::read(&second).expect("read second package"));

        let mut archive =
            zip::ZipArchive::new(File::open(&first).expect("open package")).expect("parse package");
        let mut manifest_bytes = Vec::new();
        archive
            .by_name("plugin.json")
            .expect("find package manifest")
            .read_to_end(&mut manifest_bytes)
            .expect("read package manifest");
        let manifest: PackageManifest =
            serde_json::from_slice(&manifest_bytes).expect("parse package manifest");
        assert_eq!(manifest.files.len(), 2);

        let collision = write_package(&source, &first, template(), &signer)
            .expect_err("existing output must not be overwritten");
        assert!(collision.to_string().contains("cannot publish package output"));
        assert_eq!(first_bytes, fs::read(&first).expect("reread first package"));
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("enumerate temporary directory")
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".into-md-package-"))
                .count(),
            0
        );
    }
}
