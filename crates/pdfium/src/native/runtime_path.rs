//! Authentication of every component in an explicitly selected runtime path.

use crate::Error;
use std::fs;
use std::path::{Path, PathBuf};

/// Windows supports locking the installed image and its physical directory chain
/// across LoadLibraryEx. This also works when no temporary directory is writable.
#[cfg(windows)]
pub(super) fn lock_installed_runtime(
    path: &Path,
    authenticated: &[u8],
) -> Result<(fs::File, Vec<fs::File>), Error> {
    use std::io::Read as _;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::path::{Component, Prefix};

    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_TRAVERSE: u32 = 0x20;
    const FILE_SHARE_READ: u32 = 1;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const REPARSE_POINT: u32 = 0x400;

    let mut parts = path.components();
    if !matches!(parts.next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::VerbatimDisk(_) | Prefix::Disk(_)))
        || parts.next() != Some(Component::RootDir)
    {
        return Err(Error::InvalidPath(
            "read-only runtime requires a local absolute disk path".into(),
        ));
    }
    let invalid =
        |error: std::io::Error| Error::InvalidPath(format!("lock installed runtime: {error}"));
    let mut ancestors = path.ancestors().skip(1).collect::<Vec<_>>();
    ancestors.reverse();
    let mut parents = Vec::new();
    for directory in ancestors.into_iter().skip(1) {
        let handle = fs::OpenOptions::new()
            // Traverse participates in Windows share checks even when the
            // caller only needs to inspect directory attributes.
            .access_mode(FILE_READ_ATTRIBUTES | FILE_TRAVERSE)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
            .open(directory)
            .map_err(invalid)?;
        let info = handle.metadata().map_err(invalid)?;
        if !info.is_dir() || info.file_attributes() & REPARSE_POINT != 0 {
            return Err(Error::InvalidPath("installed runtime has a non-physical parent".into()));
        }
        parents.push(handle);
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)
        .map_err(invalid)?;
    let info = file.metadata().map_err(invalid)?;
    if !info.is_file()
        || info.file_attributes() & REPARSE_POINT != 0
        || info.len() != authenticated.len() as u64
    {
        return Err(Error::InvalidPath("installed runtime identity changed".into()));
    }
    let mut buffer = vec![0; 64 * 1024];
    for chunk in authenticated.chunks(buffer.len()) {
        file.read_exact(&mut buffer[..chunk.len()]).map_err(invalid)?;
        if chunk != &buffer[..chunk.len()] {
            return Err(Error::InvalidPath("installed runtime bytes changed".into()));
        }
    }
    Ok((file, parents))
}

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

    #[cfg(windows)]
    #[test]
    fn read_only_loader_requires_the_authenticated_bytes_and_retains_the_file() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("pdfium.dll");
        fs::write(&file, b"verified").unwrap();
        let path = file.canonicalize().unwrap();
        assert!(super::lock_installed_runtime(&path, b"mismatch").is_err());
        let locked = super::lock_installed_runtime(&path, b"verified").unwrap();
        assert!(fs::OpenOptions::new().write(true).open(&file).is_err());
        let directory_access = || {
            fs::OpenOptions::new()
                .access_mode(0x0001_0000) // DELETE
                .share_mode(7)
                .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
                .open(directory.path())
        };
        assert_eq!(directory_access().unwrap_err().raw_os_error(), Some(32));
        drop(locked);
        assert!(directory_access().is_ok());
        assert_eq!(fs::read(file).unwrap(), b"verified");
    }

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
