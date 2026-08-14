//! No-follow path and archive-entry policy.

use std::path::{Component, Path, PathBuf};

pub(crate) fn canonical_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    reject_symlink(path, label)?;
    let value = path.canonicalize().map_err(|error| format!("cannot resolve {label}: {error}"))?;
    if !value.is_file() {
        return Err(format!("{label} is not a regular file"));
    }
    Ok(value)
}

pub(crate) fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    reject_symlink(path, label)?;
    let value = path.canonicalize().map_err(|error| format!("cannot resolve {label}: {error}"))?;
    if !value.is_dir() {
        return Err(format!("{label} is not a directory"));
    }
    Ok(value)
}

pub(crate) fn contained(root: &Path, path: &Path, label: &str) -> Result<(), String> {
    if path == root || !path.starts_with(root) {
        return Err(format!("{label} escapes the installation root"));
    }
    Ok(())
}

pub(crate) fn safe_relative(path: &str) -> bool {
    let value = Path::new(path);
    !path.is_empty()
        && !path.contains('\\')
        && !value.is_absolute()
        && value.components().all(|part| matches!(part, Component::Normal(_)))
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{label} must not be a symbolic link"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths_reject_escape_and_windows_separators() {
        assert!(safe_relative("bin/into-md"));
        for unsafe_path in ["", "/bin/into-md", "../bin/into-md", "bin/../into-md", "bin\\into-md"]
        {
            assert!(!safe_relative(unsafe_path), "accepted {unsafe_path:?}");
        }
    }
}
