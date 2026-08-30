use super::{
    Arc, AuthenticatedTarget, CliError, Digest, ExecutionContext, ExitClass, File, FileIdentity,
    JournalEntry, OsStr, Path, PathBuf, Read, SafeDir, Sha256, TransactionSource, decode_path,
    hex_bytes, io, recovery_error, validate_single_name,
};
use crate::transaction::{
    ResourceReservation, journal_path_retained_bytes, streaming_identity_index_bytes,
    streaming_path_index_bytes,
};
use std::collections::{BTreeMap, VecDeque};

const AUTHENTICATED_PARENT_CACHE_SIZE: usize = 64;
const AUTHENTICATED_PARENT_CACHE_FIXED_BYTES: u64 = 512;

struct CachedParent {
    handle: Arc<SafeDir>,
    retained_bytes: u64,
}

pub(in crate::transaction) struct TargetAuthenticator<'a> {
    root: &'a SafeDir,
    memory: ResourceReservation,
    parents: BTreeMap<PathBuf, CachedParent>,
    order: VecDeque<PathBuf>,
}

impl<'a> TargetAuthenticator<'a> {
    pub(in crate::transaction) fn new(
        root: &'a SafeDir,
        context: &ExecutionContext,
    ) -> Result<Self, CliError> {
        Ok(Self {
            root,
            memory: context.reserve_memory(0).map_err(CliError::from)?,
            parents: BTreeMap::new(),
            order: VecDeque::new(),
        })
    }

    pub(in crate::transaction) fn authenticate(
        &mut self,
        entry: &JournalEntry,
        parent_identities: &[FileIdentity],
    ) -> Result<AuthenticatedTarget, CliError> {
        let parent_index = entry
            .parent_index
            .ok_or_else(|| recovery_error("transaction target has no bound parent identity"))?;
        let expected = parent_identities
            .get(parent_index)
            .ok_or_else(|| recovery_error("transaction target parent index is outside limits"))?;
        let provisional_bytes = journal_path_retained_bytes(&entry.target)?
            .checked_add(streaming_identity_index_bytes(expected)?)
            .and_then(|bytes| bytes.checked_add(AUTHENTICATED_PARENT_CACHE_FIXED_BYTES))
            .ok_or_else(|| recovery_error("authenticated parent cache budget overflowed"))?;
        self.memory.grow(provisional_bytes).map_err(CliError::from)?;
        match self.authenticate_reserved(entry, expected) {
            Ok((target, retained_bytes)) => {
                let transient_bytes =
                    provisional_bytes.checked_sub(retained_bytes).ok_or_else(|| {
                        CliError::internal("authenticated parent exceeded its provisional budget")
                    })?;
                self.memory.shrink(transient_bytes).map_err(CliError::from)?;
                Ok(target)
            }
            Err(error) => {
                self.memory.shrink(provisional_bytes).map_err(CliError::from)?;
                Err(error)
            }
        }
    }

    fn authenticate_reserved(
        &mut self,
        entry: &JournalEntry,
        expected: &FileIdentity,
    ) -> Result<(AuthenticatedTarget, u64), CliError> {
        let relative = decode_path(&entry.target)?;
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        let name = relative
            .file_name()
            .ok_or_else(|| recovery_error("transaction target has no file name"))?
            .to_os_string();
        let mut retained_bytes = 0;
        let parent = if let Some(cached) = self.parents.get(&parent_relative) {
            if &cached.handle.identity != expected {
                return Err(recovery_error(
                    "cached target parent does not match its journal identity",
                ));
            }
            Arc::clone(&cached.handle)
        } else {
            if self.parents.len() == AUTHENTICATED_PARENT_CACHE_SIZE {
                let oldest = self.order.pop_front().ok_or_else(|| {
                    CliError::internal("authenticated parent cache order is empty")
                })?;
                let evicted = self.parents.remove(&oldest).ok_or_else(|| {
                    CliError::internal("authenticated parent cache entry is missing")
                })?;
                self.memory.shrink(evicted.retained_bytes).map_err(CliError::from)?;
            }
            let opened = self.root.open_descendant(&parent_relative)?;
            opened.verify_namespace()?;
            if &opened.identity != expected {
                return Err(recovery_error(
                    "target parent path does not match its journal identity",
                ));
            }
            let inserted_bytes = streaming_path_index_bytes(&parent_relative)?
                .checked_add(streaming_identity_index_bytes(&opened.identity)?)
                .and_then(|bytes| bytes.checked_add(AUTHENTICATED_PARENT_CACHE_FIXED_BYTES))
                .ok_or_else(|| recovery_error("authenticated parent cache budget overflowed"))?;
            let handle = Arc::new(opened);
            self.parents.insert(
                parent_relative.clone(),
                CachedParent { handle: Arc::clone(&handle), retained_bytes: inserted_bytes },
            );
            self.order.push_back(parent_relative.clone());
            retained_bytes = inserted_bytes;
            handle
        };
        Ok((AuthenticatedTarget { parent, name }, retained_bytes))
    }

    #[cfg(test)]
    pub(in crate::transaction) fn cached_parent_count(&self) -> usize {
        self.parents.len()
    }

    #[cfg(test)]
    pub(in crate::transaction) fn cached_parent_limit() -> usize {
        AUTHENTICATED_PARENT_CACHE_SIZE
    }
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn verify_target_handle_identity(
    target: &AuthenticatedTarget,
    expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    verify_name_identity(&target.parent, &target.name, expected)
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn verify_target_handle_identity(
    _target: &AuthenticatedTarget,
    _expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn verify_name_identity(
    directory: &SafeDir,
    name: &OsStr,
    expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    let current = directory.inspect_regular(name)?;
    if current.as_ref() != expected {
        return Err(CliError::new(
            ExitClass::Io,
            "outputIdentityChanged",
            format!(
                "output target changed after preflight: {}",
                directory.path.join(name).display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn verify_name_identity(
    _directory: &SafeDir,
    _name: &OsStr,
    _expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn handle_rename(
    from_directory: &SafeDir,
    from: &OsStr,
    to_directory: &SafeDir,
    to: &OsStr,
) -> Result<(), CliError> {
    validate_single_name(from)?;
    validate_single_name(to)?;
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    rustix::fs::renameat_with(
        &from_directory.fd,
        from,
        &to_directory.fd,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    #[cfg(windows)]
    from_directory.rename_child_to_no_replace(from, to_directory, to)?;
    #[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn handle_rename(
    _from_directory: &SafeDir,
    _from: &OsStr,
    _to_directory: &SafeDir,
    _to: &OsStr,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn install_stage_no_replace_handle(
    staged_directory: &SafeDir,
    staged: &OsStr,
    target_directory: &SafeDir,
    target: &OsStr,
) -> Result<(), CliError> {
    validate_single_name(staged)?;
    validate_single_name(target)?;
    #[cfg(windows)]
    {
        if let Err(error) = handle_rename(staged_directory, staged, target_directory, target) {
            match target_directory.inspect_regular(target) {
                Ok(Some(_)) => {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "outputIdentityChanged",
                        format!(
                            "output target appeared during commit: {}",
                            target_directory.path.join(target).display()
                        ),
                    ));
                }
                Ok(None) => return Err(error),
                Err(target_error) => return Err(target_error),
            }
        }
        target_directory.sync()?;
        staged_directory.sync()?;
        Ok(())
    }
    #[cfg(unix)]
    rustix::fs::linkat(
        &staged_directory.fd,
        staged,
        &target_directory.fd,
        target,
        rustix::fs::AtFlags::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                format!(
                    "output target appeared during commit: {}",
                    target_directory.path.join(target).display()
                ),
            )
        } else {
            CliError::new(
                ExitClass::Io,
                "outputInstallFailed",
                format!(
                    "cannot install staged output without replacing a concurrent target: {}: {error}",
                    target_directory.path.join(target).display()
                ),
            )
        }
    })?;
    #[cfg(unix)]
    {
        target_directory.sync()?;
        rustix::fs::unlinkat(&staged_directory.fd, staged, rustix::fs::AtFlags::empty())?;
        staged_directory.sync()
    }
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn install_stage_no_replace_handle(
    _staged_directory: &SafeDir,
    _staged: &OsStr,
    _target_directory: &SafeDir,
    _target: &OsStr,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn verify_handle_content(
    target: &AuthenticatedTarget,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    verify_file_content(
        target.parent.open_regular(&target.name)?,
        &target.parent.path.join(&target.name),
        entry,
    )
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn verify_handle_content(
    _target: &AuthenticatedTarget,
    _entry: &JournalEntry,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

pub(in crate::transaction) fn verify_file_content(
    mut file: File,
    display_path: &Path,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    let metadata = file.metadata()?;
    if metadata.len() != entry.size {
        return Err(recovery_error(format!(
            "transaction content size mismatch: {}",
            display_path.display()
        )));
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if hex_bytes(&digest.finalize()) != entry.content_sha256 {
        return Err(recovery_error(format!(
            "transaction content digest mismatch: {}",
            display_path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
pub(in crate::transaction) fn file_identity(file: &File) -> Result<FileIdentity, CliError> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        platform: "unix".into(),
        first: metadata.dev(),
        second: metadata.ino(),
        size: metadata.len(),
    })
}

#[cfg(windows)]
pub(in crate::transaction) fn file_identity(file: &File) -> Result<FileIdentity, CliError> {
    let information = winapi_util::file::information(file)?;
    Ok(FileIdentity {
        platform: "windows".into(),
        first: information.volume_serial_number(),
        second: information.file_index(),
        size: information.file_size(),
    })
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn file_identity(_file: &File) -> Result<FileIdentity, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "output file identity is unavailable on this platform",
    ))
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn read_limited_regular_handle(
    directory: &SafeDir,
    name: &OsStr,
    limit: u64,
) -> io::Result<Vec<u8>> {
    let file = directory
        .open_regular_optional(name)
        .map_err(|error| io::Error::other(error.to_string()))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "managed file is absent"))?;
    let size = file.metadata()?.len();
    if size > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "journal exceeds limit"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "journal exceeds limit"));
    }
    Ok(bytes)
}
