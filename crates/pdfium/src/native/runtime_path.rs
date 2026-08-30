//! Authentication of every component in an explicitly selected runtime path.

use crate::Error;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn contains_link_or_reparse(path: &Path) -> Result<bool, Error> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| Error::InvalidPath(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Ok(true);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;
            if metadata.file_attributes() & 0x400 != 0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::super::Platform;
    use crate::{Error, Limits};
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn explicit_runtime_rejects_linked_file_and_parent_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let artifact = Platform::current().unwrap().artifact().unwrap();
        let source = outside.join(&artifact.library);
        fs::write(&source, b"good").unwrap();

        let linked_file = dir.path().join(&artifact.library);
        symlink(&source, &linked_file).unwrap();
        assert!(matches!(
            crate::Pdfium::load_pinned(&linked_file, Limits::default()),
            Err(Error::InvalidPath(message)) if message.contains("symbolic link or reparse point")
        ));

        let linked_parent = dir.path().join("runtime");
        symlink(&outside, &linked_parent).unwrap();
        assert!(matches!(
            crate::Pdfium::load_pinned(
                &linked_parent.join(&artifact.library),
                Limits::default(),
            ),
            Err(Error::InvalidPath(message)) if message.contains("symbolic link or reparse point")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn explicit_runtime_rejects_linked_file_and_reparse_parent_directory() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let artifact = Platform::current().unwrap().artifact().unwrap();
        let source = outside.join(&artifact.library);
        fs::write(&source, b"good").unwrap();

        let linked_file = dir.path().join(&artifact.library);
        symlink_file(&source, &linked_file).unwrap();
        assert!(matches!(
            crate::Pdfium::load_pinned(&linked_file, Limits::default()),
            Err(Error::InvalidPath(message)) if message.contains("symbolic link or reparse point")
        ));

        let linked_parent = dir.path().join("runtime");
        symlink_dir(&outside, &linked_parent).unwrap();
        assert!(matches!(
            crate::Pdfium::load_pinned(
                &linked_parent.join(&artifact.library),
                Limits::default(),
            ),
            Err(Error::InvalidPath(message)) if message.contains("symbolic link or reparse point")
        ));
    }
}
