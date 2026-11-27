//! Retain authenticated PDFium bytes and the file identity used by the OS loader.
use super::*;

enum Storage {
    Private(tempfile::TempDir),
    #[cfg(windows)]
    Installed(Vec<File>),
}

pub(super) struct Snapshot {
    storage: Storage,
    path: PathBuf,
    file: Option<File>,
    bytes: Vec<u8>,
    #[cfg(unix)]
    load_by_path: bool,
}

impl Snapshot {
    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[cfg(unix)]
    pub(super) fn load_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd as _;
        let Storage::Private(directory) = &self.storage;
        debug_assert_eq!(self.path.parent(), Some(directory.path()));
        if self.load_by_path {
            return self.path.clone();
        }
        let fd = self.file.as_ref().expect("snapshot file remains open").as_raw_fd();
        if cfg!(target_os = "macos") {
            PathBuf::from(format!("/dev/fd/{fd}"))
        } else {
            PathBuf::from(format!("/proc/self/fd/{fd}"))
        }
    }

    #[cfg(windows)]
    pub(super) fn load_path(&self) -> PathBuf {
        self.path.clone()
    }

    pub(super) fn discard_validation_bytes(&mut self) {
        self.bytes = Vec::new();
        #[cfg(windows)]
        if let Storage::Installed(parents) = &mut self.storage {
            // The OS has mapped the verified image. The original file remains locked.
            parents.clear();
        }
    }
}

#[cfg_attr(windows, allow(clippy::permissions_set_readonly_false))]
impl Drop for Snapshot {
    fn drop(&mut self) {
        drop(self.file.take());
        #[cfg(windows)]
        if let Storage::Private(directory) = &self.storage
            && self.path.parent() == Some(directory.path())
            && let Ok(metadata) = fs::metadata(&self.path)
        {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = fs::set_permissions(&self.path, permissions);
        }
    }
}

pub(super) fn validated_snapshot(path: &Path, artifact: &Artifact) -> Result<Snapshot, Error> {
    validated_snapshot_with_hook(path, artifact, || {}, || {})
}

pub(super) fn validated_snapshot_with_hook(
    path: &Path,
    artifact: &Artifact,
    after_metadata: impl FnOnce(),
    after_snapshot: impl FnOnce(),
) -> Result<Snapshot, Error> {
    if !path.is_absolute()
        || path.file_name().and_then(|value| value.to_str()) != Some(&artifact.library)
    {
        return Err(Error::InvalidPath(
            "path must be absolute and use the pinned library filename".into(),
        ));
    }
    if contains_link_or_reparse(path)? {
        return Err(Error::InvalidPath(
            "runtime path must not contain a symbolic link or reparse point".into(),
        ));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|error| Error::InvalidPath(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::InvalidPath("runtime must be a non-symlink regular file".into()));
    }
    let canonical =
        fs::canonicalize(path).map_err(|error| Error::InvalidPath(error.to_string()))?;
    let parent = path.parent().ok_or_else(|| Error::InvalidPath("runtime has no parent".into()))?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|error| Error::InvalidPath(error.to_string()))?;
    if canonical.parent() != Some(canonical_parent.as_path()) {
        return Err(Error::InvalidPath("runtime escapes its canonical parent".into()));
    }
    let (bytes, _signed) = authenticate(&canonical, artifact, after_metadata)?;
    let directory = match tempfile::Builder::new().prefix("into-markdown-pdfium-").tempdir() {
        Ok(directory) => directory,
        #[cfg(windows)]
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::ReadOnlyFilesystem
                    | std::io::ErrorKind::NotFound
            ) =>
        {
            let (file, parents) = super::runtime_path::lock_installed_runtime(&canonical, &bytes)?;
            after_snapshot();
            return Ok(Snapshot {
                storage: Storage::Installed(parents),
                path: canonical,
                file: Some(file),
                bytes,
            });
        }
        Err(error) => {
            return Err(Error::InvalidPath(format!("cannot create private snapshot: {error}")));
        }
    };
    let snapshot_path = directory.path().join(&artifact.library);
    let mut snapshot_file =
        OpenOptions::new().write(true).create_new(true).open(&snapshot_path).map_err(|error| {
            Error::InvalidPath(format!("cannot create private snapshot: {error}"))
        })?;
    snapshot_file
        .write_all(&bytes)
        .and_then(|()| snapshot_file.sync_all())
        .map_err(|error| Error::InvalidPath(format!("cannot write private snapshot: {error}")))?;
    set_snapshot_read_only(&snapshot_file)?;
    drop(snapshot_file);
    let snapshot_file = open_locked(&snapshot_path)?;
    after_snapshot();
    Ok(Snapshot {
        storage: Storage::Private(directory),
        path: snapshot_path,
        file: Some(snapshot_file),
        bytes,
        #[cfg(unix)]
        load_by_path: _signed,
    })
}

fn authenticate(
    canonical: &Path,
    artifact: &Artifact,
    after_metadata: impl FnOnce(),
) -> Result<(Vec<u8>, bool), Error> {
    let mut file = open_locked(canonical)?;
    let opened_metadata = file.metadata().map_err(|error| Error::InvalidPath(error.to_string()))?;
    if !opened_metadata.is_file() {
        return Err(Error::InvalidPath("opened runtime is not a regular file".into()));
    }
    let signed_authority = if opened_metadata.len() == artifact.library_size {
        None
    } else {
        signed_derivative_authority(canonical, artifact)
    };
    let expected_size =
        signed_authority.as_ref().map_or(artifact.library_size, |authority| authority.bytes);
    if opened_metadata.len() != expected_size {
        return Err(Error::ResourceLimit {
            limit: "pdfium_runtime_bytes",
            actual: opened_metadata.len(),
            maximum: expected_size,
        });
    }
    after_metadata();
    let bounded = expected_size
        .checked_add(1)
        .ok_or_else(|| Error::BinaryValidation("runtime size bound overflowed".into()))?;
    if bounded > MAX_RUNTIME_LIBRARY_BYTES + 1 {
        return Err(Error::ResourceLimit {
            limit: "pdfium_runtime_bytes",
            actual: expected_size,
            maximum: MAX_RUNTIME_LIBRARY_BYTES,
        });
    }
    let capacity = usize::try_from(bounded).map_err(|_| Error::ResourceLimit {
        limit: "pdfium_runtime_bytes",
        actual: bounded,
        maximum: expected_size,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Allocation { operation: "pdfium_runtime_snapshot", bytes: bounded })?;
    std::io::Read::by_ref(&mut file)
        .take(bounded)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::InvalidPath(error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_size {
        return Err(Error::ResourceLimit {
            limit: "pdfium_runtime_bytes",
            actual: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            maximum: expected_size,
        });
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    let digest_matches = actual == artifact.library_sha256
        || signed_authority.as_ref().is_some_and(|authority| {
            actual == authority.sha256 && verify_platform_signature(canonical)
        });
    if !digest_matches {
        return Err(Error::DigestMismatch { expected: artifact.library_sha256.clone(), actual });
    }
    Ok((bytes, signed_authority.is_some()))
}
