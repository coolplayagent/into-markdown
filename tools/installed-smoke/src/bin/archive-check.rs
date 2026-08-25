//! Standalone archive projection hash and closed-tree verifier.

use license_check::schema::ArchiveProjection;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

const MANIFEST_LIMIT: u64 = 16 * 1024 * 1024;
const ENTRY_LIMIT: usize = 200_000;
const HASH_BUFFER: usize = 16 * 1024;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("archive-check: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let root = arguments.next().ok_or_else(|| "missing archive root".to_owned())?;
    if arguments.next().is_some() {
        return Err("unexpected argument".into());
    }
    let root =
        PathBuf::from(root).canonicalize().map_err(|_| "archive root is unavailable".to_owned())?;
    if !root.is_dir() {
        return Err("archive root is not a directory".into());
    }
    let manifest = root.join("archive-manifest.json");
    if fs::metadata(&manifest).map_err(|_| "archive manifest is missing")?.len() > MANIFEST_LIMIT {
        return Err("archive manifest exceeds its size limit".into());
    }
    let projection: ArchiveProjection = serde_json::from_slice(
        &fs::read(&manifest).map_err(|_| "archive manifest cannot be read")?,
    )
    .map_err(|_| "archive manifest is invalid")?;
    if projection.schema_version != 1 || projection.files.len() > ENTRY_LIMIT {
        return Err("archive projection contract is unsupported".into());
    }
    let mut expected = BTreeMap::new();
    for entry in &projection.files {
        if !safe_relative(&entry.path) || expected.insert(entry.path.as_str(), entry).is_some() {
            return Err("archive manifest contains an unsafe or duplicate path".into());
        }
        verify_file(&root, entry)?;
    }
    let mut pending = vec![root.clone()];
    let mut visited = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|_| "archive tree cannot be read")? {
            let entry = entry.map_err(|_| "archive tree entry cannot be read")?;
            visited = visited.checked_add(1).ok_or("archive entry count overflowed")?;
            if visited > ENTRY_LIMIT {
                return Err("archive tree exceeds its entry limit".into());
            }
            let kind = entry.file_type().map_err(|_| "archive entry type cannot be read")?;
            if kind.is_symlink() {
                return Err("archive tree contains a symbolic link".into());
            }
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && entry.path() != manifest {
                let relative = manifest_relative(&root, &entry.path())?;
                if !expected.contains_key(relative.as_str()) {
                    return Err("archive tree contains an unmanifested file".into());
                }
            } else if !kind.is_file() {
                return Err("archive tree contains a non-regular entry".into());
            }
        }
    }
    Ok(())
}

fn manifest_relative(root: &Path, path: &Path) -> Result<String, String> {
    path.strip_prefix(root)
        .map_err(|_| "archive entry escapes its root")?
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "archive entry path is not UTF-8".to_owned()),
            _ => Err("archive entry path is not portable".to_owned()),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}

fn verify_file(root: &Path, entry: &license_check::schema::ArchiveFile) -> Result<(), String> {
    let path = root.join(&entry.path);
    let metadata = fs::symlink_metadata(&path).map_err(|_| "manifest file is missing")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != entry.bytes {
        return Err("manifest file metadata disagrees".into());
    }
    let mut file = File::open(path).map_err(|_| "manifest file cannot be opened")?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER];
    loop {
        let count = file.read(&mut buffer).map_err(|_| "manifest file cannot be hashed")?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    if format!("{:x}", digest.finalize()) != entry.sha256 {
        return Err("manifest file hash disagrees".into());
    }
    Ok(())
}

fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && value.is_ascii()
        && !value.bytes().any(|byte| byte.is_ascii_control() || matches!(byte, b'\\' | 0x7f))
        && Path::new(value).components().all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_relative_uses_portable_separators() {
        let root = Path::new("archive");
        assert_eq!(
            manifest_relative(root, &root.join("share").join("fixture.txt")).unwrap(),
            "share/fixture.txt"
        );
    }
}
