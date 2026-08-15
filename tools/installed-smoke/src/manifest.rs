//! Installed-file verification against the #60 archive projection.

use crate::path_policy::safe_relative;
use crate::request::ValidatedRequest;
use license_check::schema::{ArchiveFile, ArchiveProjection};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::time::Instant;

const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;
const ENTRY_LIMIT: usize = 200_000;
const PATH_LIMIT: usize = 4096;
const FILE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const TOTAL_PROJECTION_LIMIT: u64 = 32 * 1024 * 1024 * 1024;
const HASH_BUFFER: usize = 4096;

pub(crate) fn verify_install(request: &ValidatedRequest) -> Result<ArchiveProjection, String> {
    let deadline = Instant::now()
        .checked_add(request.timeout)
        .ok_or_else(|| "archive verification deadline is invalid".to_owned())?;
    checkpoint(request, deadline)?;
    let manifest_size = fs::metadata(&request.manifest)
        .map_err(|error| format!("cannot inspect archive manifest: {error}"))?
        .len();
    if manifest_size > MANIFEST_LIMIT {
        return Err("archive manifest size limit exceeded".into());
    }
    let bytes = fs::read(&request.manifest)
        .map_err(|error| format!("cannot read archive manifest: {error}"))?;
    let projection: ArchiveProjection = serde_json::from_slice(&bytes)
        .map_err(|error| format!("archive manifest is invalid: {error}"))?;
    if projection.schema_version != 1 {
        return Err("archive manifest schema version is unsupported".into());
    }
    if projection.files.len() > ENTRY_LIMIT {
        return Err("archive manifest entry limit exceeded".into());
    }
    let mut paths = BTreeSet::new();
    let mut total_bytes = 0_u64;
    for file in &projection.files {
        checkpoint(request, deadline)?;
        if file.path.len() > PATH_LIMIT
            || !safe_relative(&file.path)
            || !paths.insert(file.path.as_str())
        {
            return Err("archive manifest contains an unsafe or duplicate path".into());
        }
        if file.bytes > FILE_LIMIT {
            return Err("archive manifest per-file byte limit exceeded".into());
        }
        total_bytes = total_bytes
            .checked_add(file.bytes)
            .ok_or_else(|| "archive manifest total byte count overflowed".to_owned())?;
        if total_bytes > TOTAL_PROJECTION_LIMIT {
            return Err("archive manifest total byte limit exceeded".into());
        }
        verify_file(request, file, deadline)?;
    }
    for required in ["core-catalog.json"] {
        if !projection.files.iter().any(|file| file.path == required) {
            return Err(format!("archive manifest omits required authority {required}"));
        }
    }
    let binary = relative(request, &request.into_md)?;
    let rust_manifest = relative(request, &request.rust_library.join("Cargo.toml"))?;
    for required in [binary, rust_manifest] {
        if !paths.contains(required.as_str()) {
            return Err("archive manifest does not bind a requested installed artifact".into());
        }
    }
    verify_tree_is_manifest_bound(request, &request.install_root, &paths, deadline)?;
    Ok(projection)
}

fn verify_tree_is_manifest_bound(
    request: &ValidatedRequest,
    directory: &std::path::Path,
    manifest_paths: &BTreeSet<&str>,
    deadline: Instant,
) -> Result<(), String> {
    let mut pending = vec![directory.to_owned()];
    let mut visited = 0_usize;
    while let Some(current) = pending.pop() {
        checkpoint(request, deadline)?;
        for item in
            fs::read_dir(&current).map_err(|_| "cannot inspect installed tree".to_owned())?
        {
            checkpoint(request, deadline)?;
            visited = visited
                .checked_add(1)
                .ok_or_else(|| "installed tree entry count overflowed".to_owned())?;
            if visited > ENTRY_LIMIT {
                return Err("installed tree entry limit exceeded".into());
            }
            let item = item.map_err(|_| "cannot inspect installed tree entry".to_owned())?;
            let metadata = item
                .file_type()
                .map_err(|_| "cannot inspect installed tree metadata".to_owned())?;
            if metadata.is_symlink() {
                return Err("installed tree contains a symbolic link".into());
            }
            if metadata.is_dir() {
                pending.push(item.path());
            } else if metadata.is_file() {
                if item.path().canonicalize().ok().as_ref() == Some(&request.manifest) {
                    continue;
                }
                let relative = relative(request, &item.path())?;
                if !manifest_paths.contains(relative.as_str()) {
                    return Err("installed file is absent from archive manifest".into());
                }
            } else {
                return Err("installed tree contains a non-regular entry".into());
            }
        }
    }
    Ok(())
}

fn verify_file(
    request: &ValidatedRequest,
    entry: &ArchiveFile,
    deadline: Instant,
) -> Result<(), String> {
    let path = request.install_root.join(&entry.path);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| format!("installed file is missing: {}", entry.path))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != entry.bytes {
        return Err(format!("installed file metadata disagrees with manifest: {}", entry.path));
    }
    let canonical = path.canonicalize().map_err(|_| "cannot resolve installed file".to_owned())?;
    if !canonical.starts_with(&request.install_root) {
        return Err("installed file escapes installation root".into());
    }
    let mut input = File::open(&path).map_err(|_| "cannot open installed file".to_owned())?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER];
    loop {
        checkpoint(request, deadline)?;
        let count = input.read(&mut buffer).map_err(|_| "cannot hash installed file".to_owned())?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    if format!("{:x}", digest.finalize()) != entry.sha256 {
        return Err(format!("installed file hash disagrees with manifest: {}", entry.path));
    }
    Ok(())
}

fn checkpoint(request: &ValidatedRequest, deadline: Instant) -> Result<(), String> {
    if request.cancelled() {
        return Err("archive verification cancelled".into());
    }
    if Instant::now() >= deadline {
        return Err("archive verification deadline exceeded".into());
    }
    Ok(())
}

fn relative(request: &ValidatedRequest, path: &std::path::Path) -> Result<String, String> {
    let path = path
        .strip_prefix(&request.install_root)
        .map_err(|_| "installed artifact escapes installation root".to_owned())?
        .components()
        .map(|part| {
            part.as_os_str()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "installed artifact path is not Unicode".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(path.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use license_check::schema::{ArchiveFileKind, ArchiveProjection};
    use std::time::Duration;

    #[test]
    fn mutation_and_unmanifested_installed_files_fail_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let install = temporary.path().join("install");
        let rust = install.join("lib/into-markdown-rust");
        let fixtures = install.join("share/into-markdown/smoke/fixtures/text");
        fs::create_dir_all(rust.join("vendor/example")).unwrap();
        fs::create_dir_all(&fixtures).unwrap();
        fs::create_dir_all(install.join("bin")).unwrap();
        fs::write(install.join("bin/into-md"), b"binary").unwrap();
        fs::write(rust.join("Cargo.toml"), b"[package]\nname='fixture'\nversion='0.0.0'\n")
            .unwrap();
        fs::write(rust.join("vendor/example/checksum"), b"vendor").unwrap();
        fs::write(fixtures.join("normal.txt"), b"fixture").unwrap();
        fs::write(install.join("core-catalog.json"), b"catalog").unwrap();
        let paths = [
            ("bin/into-md", ArchiveFileKind::Project),
            ("lib/into-markdown-rust/Cargo.toml", ArchiveFileKind::Project),
            ("lib/into-markdown-rust/vendor/example/checksum", ArchiveFileKind::Project),
            ("share/into-markdown/smoke/fixtures/text/normal.txt", ArchiveFileKind::Project),
            ("core-catalog.json", ArchiveFileKind::Generated),
        ];
        let projection = ArchiveProjection {
            schema_version: 1,
            target: "aarch64-apple-darwin".into(),
            components: vec![],
            files: paths
                .into_iter()
                .map(|(path, kind)| archive_file(&install, path, kind))
                .collect(),
            license_materials: vec![],
            ffmpeg_evidence: None,
        };
        let manifest = install.join("archive-manifest.json");
        fs::write(&manifest, serde_json::to_vec(&projection).unwrap()).unwrap();
        let request = ValidatedRequest {
            install_root: install.canonicalize().unwrap(),
            into_md: install.join("bin/into-md").canonicalize().unwrap(),
            rust_library: rust.canonicalize().unwrap(),
            manifest: manifest.canonicalize().unwrap(),
            fixtures: install.join("share/into-markdown/smoke/fixtures").canonicalize().unwrap(),
            temp_root: temporary.path().to_owned(),
            report: temporary.path().join("report.json"),
            archive_sha256: "a".repeat(64),
            cargo: install.join("bin/into-md"),
            rustc: install.join("bin/into-md"),
            timeout: Duration::from_secs(1),
            cancel_file: None,
        };
        verify_install(&request).unwrap();

        let mut oversized = projection.clone();
        oversized.files[0].bytes = FILE_LIMIT + 1;
        fs::write(&manifest, serde_json::to_vec(&oversized).unwrap()).unwrap();
        assert!(verify_install(&request).unwrap_err().contains("per-file byte limit"));

        fs::write(&manifest, serde_json::to_vec(&projection).unwrap()).unwrap();
        let cancel = temporary.path().join("cancel");
        fs::write(&cancel, b"cancel").unwrap();
        let mut cancelled = request.clone();
        cancelled.cancel_file = Some(cancel);
        assert_eq!(verify_install(&cancelled).unwrap_err(), "archive verification cancelled");

        let mut expired = request.clone();
        expired.timeout = Duration::ZERO;
        assert_eq!(verify_install(&expired).unwrap_err(), "archive verification deadline exceeded");

        fs::write(install.join("bin/into-md"), b"mutated").unwrap();
        assert!(verify_install(&request).unwrap_err().contains("metadata disagrees"));
        fs::write(install.join("bin/into-md"), b"binary").unwrap();
        fs::write(rust.join("developer-target-cache"), b"forbidden").unwrap();
        assert!(verify_install(&request).unwrap_err().contains("absent from archive manifest"));
    }

    fn archive_file(root: &std::path::Path, path: &str, kind: ArchiveFileKind) -> ArchiveFile {
        let bytes = fs::read(root.join(path)).unwrap();
        ArchiveFile {
            path: path.into(),
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            kind,
            component_id: None,
            embedded_components: vec![],
        }
    }
}
