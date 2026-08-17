//! Crash-recoverable output-set transactions.

#![cfg_attr(not(unix), allow(dead_code, unused_imports, unused_mut, unused_variables))]
#![cfg_attr(not(unix), allow(unreachable_code))]

use crate::error::{CliError, ExitClass};
use into_markdown::{ExecutionContext, ResourceReservation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::fd::OwnedFd;

const JOURNAL_SIGNATURE: &str = "into-markdown-output-transaction";
const JOURNAL_VERSION: u32 = 1;
const TRANSACTION_PREFIX: &str = ".into-md-txn-01-";
const INITIAL_PREFIX: &str = ".into-md-init-01-";
const CLEANUP_PREFIX: &str = ".into-md-clean-01-";
const PARENT_MARKER_PREFIX: &str = "parent-";
const PARENT_LEASE_NAME: &str = ".into-md-output-parent-lease-01";
const REGISTRY_NAME: &str = ".into-md-output-transactions-01";
const MAX_RECOVERY_TRANSACTIONS: usize = 128;
const MAX_RECOVERY_RETRIES: usize = 8;
const MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 16_384;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JOURNAL_ENTRIES: usize = 100_001;
const MAX_PATH_UNITS: usize = 32_768;

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
static ACTIVE_TRANSACTIONS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

/// Atomically replace one configuration file through an authenticated parent
/// directory handle.
#[cfg(unix)]
pub(crate) fn atomic_replace_config(
    path: &Path,
    bytes: &[u8],
    replace: bool,
) -> Result<(), CliError> {
    atomic_replace_config_inner_with_barriers(
        path,
        bytes,
        replace,
        |_, _, _| Ok(()),
        |_, _, _| Ok(()),
    )
}

#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
fn atomic_replace_config_inner(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    before_commit: impl FnOnce(&SafeDir, &OsStr, &OsStr) -> Result<(), CliError>,
) -> Result<(), CliError> {
    atomic_replace_config_inner_with_barriers(path, bytes, replace, before_commit, |_, _, _| Ok(()))
}

#[cfg(unix)]
fn atomic_replace_config_inner_with_barriers(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    before_final_check: impl FnOnce(&SafeDir, &OsStr, &OsStr) -> Result<(), CliError>,
    after_final_check: impl FnOnce(&SafeDir, &OsStr, &OsStr) -> Result<(), CliError>,
) -> Result<(), CliError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let parent_path = absolute.parent().ok_or_else(|| recovery_error("config has no parent"))?;
    let name = absolute.file_name().ok_or_else(|| recovery_error("config has no file name"))?;
    validate_single_name(name)?;
    let parent = SafeDir::open_or_create_absolute(parent_path)?;
    parent.verify_namespace()?;
    let expected = parent.inspect_regular(name)?;
    if expected.is_some() && !replace {
        return Err(CliError::config(format!("path already exists: {}", absolute.display())));
    }
    let existing_mode = if expected.is_some() {
        let file = parent.open_regular(name)?;
        let metadata = file.metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(CliError::config(format!(
                "configuration file is not owned by the current user: {}",
                absolute.display()
            )));
        }
        Some(metadata.permissions().mode() & 0o777)
    } else {
        None
    };
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let sequence = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary_name =
        OsString::from(format!(".into-md-config-{}-{nonce}-{sequence}.tmp", std::process::id()));
    let mut temporary = parent.create_regular(&temporary_name)?;
    let result = (|| {
        temporary.write_all(bytes)?;
        if let Some(mode) = existing_mode {
            temporary.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        temporary.sync_all()?;
        let temporary_identity = file_identity(&temporary)?;
        before_final_check(&parent, name, &temporary_name)?;
        parent.verify_namespace()?;
        verify_name_identity(&parent, name, expected.as_ref())?;
        verify_name_identity(&parent, &temporary_name, Some(&temporary_identity))?;
        after_final_check(&parent, name, &temporary_name)?;
        // Re-authenticate sources after the deterministic race barrier. The
        // destination itself is protected by the atomic primitive below: an
        // absent target can never be overwritten, while replacement exchanges
        // the old inode so it can be verified and restored losslessly.
        parent.verify_namespace()?;
        verify_name_identity(&parent, &temporary_name, Some(&temporary_identity))?;
        publish_config(&parent, name, &temporary_name, expected.as_ref(), &temporary_identity)?;
        // Persist the exchange before discarding the displaced old target. If
        // this fsync fails, the old inode remains recoverable under the private
        // temporary name rather than being prematurely destroyed.
        parent.sync()?;
        if let Some(expected) = expected.as_ref() {
            unlink_name_if_identity(&parent, &temporary_name, expected)?;
            parent.sync()?;
        }
        parent.verify_namespace()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlink_name_if_identity(&parent, &temporary_name, &file_identity(&temporary)?);
        let _ = parent.sync();
    }
    result
}

#[cfg(unix)]
fn publish_config(
    parent: &SafeDir,
    name: &OsStr,
    temporary_name: &OsStr,
    expected: Option<&FileIdentity>,
    temporary_identity: &FileIdentity,
) -> Result<(), CliError> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        if let Some(expected) = expected {
            rustix::fs::renameat_with(
                &parent.fd,
                temporary_name,
                &parent.fd,
                name,
                rustix::fs::RenameFlags::EXCHANGE,
            )?;
            let installed = verify_name_identity(parent, name, Some(temporary_identity));
            let displaced = verify_name_identity(parent, temporary_name, Some(expected));
            if let Err(error) = installed.and(displaced) {
                // Both namespace entries still exist after EXCHANGE. Put them
                // back before surfacing any identity mismatch; no old target is
                // discarded on the error path.
                rustix::fs::renameat_with(
                    &parent.fd,
                    temporary_name,
                    &parent.fd,
                    name,
                    rustix::fs::RenameFlags::EXCHANGE,
                )?;
                return Err(error);
            }
        } else {
            rustix::fs::renameat_with(
                &parent.fd,
                temporary_name,
                &parent.fd,
                name,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
            verify_name_identity(parent, name, Some(temporary_identity))?;
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn unlink_name_if_identity(
    directory: &SafeDir,
    name: &OsStr,
    expected: &FileIdentity,
) -> Result<(), CliError> {
    if directory.inspect_regular(name)?.as_ref() != Some(expected) {
        return Err(CliError::new(
            ExitClass::Io,
            "outputIdentityChanged",
            format!("refusing to unlink changed file: {}", directory.path.join(name).display()),
        ));
    }
    rustix::fs::unlinkat(&directory.fd, name, rustix::fs::AtFlags::empty())?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn atomic_replace_config(
    _path: &Path,
    _bytes: &[u8],
    _replace: bool,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum JournalPhase {
    Staging,
    Prepared,
    Committing,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum EntryState {
    Prepared,
    BackedUp,
    Installed,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalPath {
    encoding: String,
    units: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileIdentity {
    platform: String,
    first: u64,
    second: u64,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalEntry {
    target: JournalPath,
    original: Option<FileIdentity>,
    content_sha256: String,
    size: u64,
    state: EntryState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Journal {
    signature: String,
    version: u32,
    nonce: String,
    root: JournalPath,
    root_identity: FileIdentity,
    parent_identities: Vec<FileIdentity>,
    generation: u64,
    phase: JournalPhase,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParentLease {
    signature: String,
    version: u32,
    nonce: String,
    root: JournalPath,
    root_identity: FileIdentity,
    parent_identity: FileIdentity,
}

/// One requested target and its complete staged contents.
pub struct Target<'a> {
    pub path: PathBuf,
    pub bytes: &'a [u8],
}

#[cfg(unix)]
pub(crate) struct SafeDir {
    fd: OwnedFd,
    path: PathBuf,
    identity: FileIdentity,
}

#[cfg(unix)]
impl SafeDir {
    pub(crate) fn open_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory handle path is not absolute"));
        }
        let mut current_path = PathBuf::from("/");
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    fd = rustix::fs::openat(
                        &fd,
                        name,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    )?;
                    current_path.push(name);
                }
                _ => return Err(recovery_error("directory handle path is not normalized")),
            }
        }
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: current_path, identity })
    }

    fn open_or_create_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory creation path is not absolute"));
        }
        let mut current_path = PathBuf::from("/");
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    let opened = rustix::fs::openat(
                        &fd,
                        name,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    );
                    fd = match opened {
                        Ok(opened) => opened,
                        Err(rustix::io::Errno::NOENT) => {
                            rustix::fs::mkdirat(
                                &fd,
                                name,
                                rustix::fs::Mode::RUSR
                                    | rustix::fs::Mode::WUSR
                                    | rustix::fs::Mode::XUSR
                                    | rustix::fs::Mode::RGRP
                                    | rustix::fs::Mode::XGRP
                                    | rustix::fs::Mode::ROTH
                                    | rustix::fs::Mode::XOTH,
                            )?;
                            rustix::fs::fsync(&fd)?;
                            rustix::fs::openat(
                                &fd,
                                name,
                                rustix::fs::OFlags::RDONLY
                                    | rustix::fs::OFlags::DIRECTORY
                                    | rustix::fs::OFlags::NOFOLLOW
                                    | rustix::fs::OFlags::CLOEXEC,
                                rustix::fs::Mode::empty(),
                            )?
                        }
                        Err(error) => return Err(error.into()),
                    };
                    current_path.push(name);
                }
                _ => return Err(recovery_error("directory creation path is not normalized")),
            }
        }
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: current_path, identity })
    }

    pub(crate) fn open_child(&self, name: &OsStr) -> Result<Self, CliError> {
        validate_single_name(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: self.path.join(name), identity })
    }

    pub(crate) fn open_child_optional(&self, name: &OsStr) -> Result<Option<Self>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => {
                let identity = directory_identity(&fd)?;
                Ok(Some(Self { fd, path: self.path.join(name), identity }))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn open_descendant(&self, relative: &Path) -> Result<Self, CliError> {
        if relative.as_os_str().is_empty() {
            let fd = rustix::io::dup(&self.fd)?;
            return Ok(Self { fd, path: self.path.clone(), identity: self.identity.clone() });
        }
        validate_relative_path(relative)?;
        let mut current = Self {
            fd: rustix::io::dup(&self.fd)?,
            path: self.path.clone(),
            identity: self.identity.clone(),
        };
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(recovery_error("descendant path is not normalized"));
            };
            current = current.open_child(name)?;
        }
        Ok(current)
    }

    pub(crate) fn verify_namespace(&self) -> Result<(), CliError> {
        let changed = || {
            CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                format!("output directory changed after authentication: {}", self.path.display()),
            )
        };
        let current = Self::open_absolute(&self.path).map_err(|_| changed())?;
        if current.identity != self.identity {
            return Err(changed());
        }
        Ok(())
    }

    pub(crate) fn verify_private_namespace(&self) -> Result<(), CliError> {
        let stat = rustix::fs::fstat(&self.fd)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o777 != 0o700
        {
            return Err(recovery_error(
                "managed directory is not private, owner-bound, and descriptor-authenticated",
            ));
        }
        self.verify_namespace()
    }

    pub(crate) fn open_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child(name)?;
        self.verify_private_namespace()?;
        child.verify_private_namespace()?;
        Ok(child)
    }

    pub(crate) fn open_child_private_optional(
        &self,
        name: &OsStr,
    ) -> Result<Option<Self>, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child_optional(name)?;
        self.verify_private_namespace()?;
        if let Some(child) = &child {
            child.verify_private_namespace()?;
        }
        Ok(child)
    }

    pub(crate) fn open_regular(&self, name: &OsStr) -> Result<File, CliError> {
        validate_single_name(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let stat = rustix::fs::fstat(&fd)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
            return Err(CliError::new(
                ExitClass::Io,
                "outputTargetTypeDenied",
                format!("not a regular file: {}", self.path.join(name).display()),
            ));
        }
        Ok(File::from(fd))
    }

    pub(crate) fn open_regular_optional(&self, name: &OsStr) -> Result<Option<File>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => {
                let stat = rustix::fs::fstat(&fd)?;
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    != rustix::fs::FileType::RegularFile
                {
                    return Err(recovery_error("optional managed file is not regular"));
                }
                Ok(Some(File::from(fd)))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn create_regular(&self, name: &OsStr) -> Result<File, CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        self.verify_namespace()?;
        Ok(File::from(fd))
    }

    pub(crate) fn create_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.create_regular(name)?;
        verify_private_regular(&file)?;
        self.verify_private_namespace()?;
        Ok(file)
    }

    pub(crate) fn open_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular(name)?;
        verify_private_regular(&file)?;
        self.verify_private_namespace()?;
        Ok(file)
    }

    fn inspect_regular(&self, name: &OsStr) -> Result<Option<FileIdentity>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::RegularFile =>
            {
                let file = self.open_regular(name)?;
                Ok(Some(file_identity(&file)?))
            }
            Ok(_) => Err(CliError::new(
                ExitClass::Io,
                "outputTargetTypeDenied",
                format!(
                    "output target is not a regular non-link file: {}",
                    self.path.join(name).display()
                ),
            )),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn sync(&self) -> Result<(), CliError> {
        rustix::fs::fsync(&self.fd)?;
        Ok(())
    }

    pub(crate) fn names(&self) -> Result<Vec<OsString>, CliError> {
        self.names_bounded(MAX_RECOVERY_DIRECTORY_ENTRIES)
    }

    pub(crate) fn names_private(&self) -> Result<Vec<OsString>, CliError> {
        self.verify_private_namespace()?;
        let names = self.names()?;
        self.verify_private_namespace()?;
        Ok(names)
    }

    pub(crate) fn names_bounded(&self, limit: usize) -> Result<Vec<OsString>, CliError> {
        use std::os::unix::ffi::OsStringExt as _;
        let mut directory = rustix::fs::Dir::read_from(&self.fd)?;
        let mut names = Vec::new();
        while let Some(entry) = directory.read() {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if names.len() >= limit {
                return Err(CliError::new(
                    ExitClass::Io,
                    "transactionRecoveryLimit",
                    format!("recovery scan exceeded {limit} entries under {}", self.path.display()),
                ));
            }
            names.try_reserve(1).map_err(|error| {
                CliError::new(
                    ExitClass::Io,
                    "transactionRecoveryLimit",
                    format!("cannot reserve recovery directory entry: {error}"),
                )
            })?;
            names.push(OsString::from_vec(bytes.to_vec()));
        }
        Ok(names)
    }

    pub(crate) fn create_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        rustix::fs::mkdirat(
            &self.fd,
            name,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
        )?;
        self.sync()?;
        let child = self.open_child(name)?;
        self.verify_private_namespace()?;
        child.verify_private_namespace()?;
        Ok(child)
    }

    pub(crate) fn rename_child_private_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        self.rename_child_no_replace(source, destination)?;
        self.verify_private_namespace()
    }

    pub(crate) fn rename_child_private_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        destination_directory.verify_private_namespace()?;
        self.rename_child_to_no_replace(source, destination_directory, destination)?;
        self.verify_private_namespace()?;
        destination_directory.verify_private_namespace()
    }

    pub(crate) fn remove_regular_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular_private(name)?;
        drop(file);
        self.remove_regular(name)?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_empty_child_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child(name)?;
        child.verify_private_namespace()?;
        self.remove_empty_child(name)?;
        self.verify_private_namespace()
    }

    pub(crate) fn rename_child_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        validate_single_name(source)?;
        validate_single_name(destination)?;
        self.verify_namespace()?;
        rustix::fs::renameat_with(
            &self.fd,
            source,
            &self.fd,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn rename_child_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        validate_single_name(source)?;
        validate_single_name(destination)?;
        self.verify_namespace()?;
        destination_directory.verify_namespace()?;
        rustix::fs::renameat_with(
            &self.fd,
            source,
            &destination_directory.fd,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        self.sync()?;
        destination_directory.sync()?;
        self.verify_namespace()?;
        destination_directory.verify_namespace()
    }

    pub(crate) fn remove_regular(&self, name: &OsStr) -> Result<(), CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let file = self.open_regular(name)?;
        if rustix::fs::fstat(&file)?.st_nlink != 1 {
            return Err(recovery_error("managed file has an external hard link"));
        }
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn remove_empty_child(&self, name: &OsStr) -> Result<(), CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let child = self.open_child(name)?;
        if !child.names()?.is_empty() {
            return Err(recovery_error("managed directory is not empty"));
        }
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::REMOVEDIR)?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn measured_tree_bytes(
        &self,
        max_depth: u8,
        max_entries: usize,
    ) -> Result<u64, CliError> {
        fn visit(
            directory: &SafeDir,
            depth: u8,
            max_depth: u8,
            entries: &mut usize,
            max_entries: usize,
        ) -> Result<u64, CliError> {
            use std::os::unix::ffi::OsStrExt as _;
            if depth > max_depth {
                return Err(recovery_error("managed storage depth exceeds its limit"));
            }
            let mut reader = rustix::fs::Dir::read_from(&directory.fd)?;
            let mut total = 0_u64;
            while let Some(entry) = reader.read() {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                *entries = entries
                    .checked_add(1)
                    .ok_or_else(|| recovery_error("managed storage entry count overflow"))?;
                if *entries > max_entries {
                    return Err(recovery_error("managed storage entry count exceeds its limit"));
                }
                let stat =
                    rustix::fs::statat(&directory.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
                match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
                    rustix::fs::FileType::Directory => {
                        let child = directory.open_child(OsStr::from_bytes(name.to_bytes()))?;
                        total = total
                            .checked_add(visit(&child, depth + 1, max_depth, entries, max_entries)?)
                            .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                    }
                    rustix::fs::FileType::RegularFile if stat.st_nlink == 1 => {
                        total = total
                            .checked_add(u64::try_from(stat.st_size).map_err(|_| {
                                recovery_error("managed file size is not representable")
                            })?)
                            .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                    }
                    _ => return Err(recovery_error("managed storage contains an unsafe object")),
                }
            }
            Ok(total)
        }
        let mut entries = 0;
        visit(self, 0, max_depth, &mut entries, max_entries)
    }
}

fn validate_single_name(name: &OsStr) -> Result<(), CliError> {
    let path = Path::new(name);
    if path.as_os_str().is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(recovery_error("transaction member is not a single safe name"));
    }
    Ok(())
}

#[cfg(unix)]
fn fd_identity(fd: &impl std::os::fd::AsFd) -> Result<FileIdentity, CliError> {
    let stat = rustix::fs::fstat(fd)?;
    Ok(FileIdentity {
        platform: "unix".into(),
        first: u64::try_from(stat.st_dev).unwrap_or(u64::MAX),
        #[allow(clippy::useless_conversion)]
        second: u64::try_from(stat.st_ino).unwrap_or(u64::MAX),
        size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
    })
}

#[cfg(unix)]
fn directory_identity(fd: &impl std::os::fd::AsFd) -> Result<FileIdentity, CliError> {
    let mut identity = fd_identity(fd)?;
    identity.size = 0;
    Ok(identity)
}

#[cfg(unix)]
fn verify_private_regular(file: &File) -> Result<(), CliError> {
    let stat = rustix::fs::fstat(file)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(recovery_error("managed file is not private, owner-bound, and singly linked"));
    }
    Ok(())
}

/// Test seam for deterministic failure and crash injection.
#[derive(Debug)]
pub enum HookDecision {
    Continue,
    #[cfg(test)]
    SimulateCrash,
}

/// A fully staged transaction. Dropping it preserves the journal for recovery.
pub struct PreparedTransaction {
    root: PathBuf,
    directory: PathBuf,
    journal: Journal,
    context: ExecutionContext,
    active: bool,
    temporary_reservations: Vec<ResourceReservation>,
    lock: Option<File>,
    handles: TransactionHandles,
}

#[cfg(unix)]
struct TransactionHandles {
    root: SafeDir,
    directory: SafeDir,
}

#[cfg(not(unix))]
struct TransactionHandles {
    root: SafeDir,
    directory: SafeDir,
}

#[cfg(unix)]
struct AuthenticatedTarget {
    parent: SafeDir,
    name: OsString,
}

#[cfg(not(unix))]
struct AuthenticatedTarget {
    parent: SafeDir,
    name: OsString,
}

#[cfg(not(unix))]
pub(crate) struct SafeDir;

#[cfg(not(unix))]
impl SafeDir {
    pub(crate) fn open_absolute(_path: &Path) -> Result<Self, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn open_child(&self, _name: &OsStr) -> Result<Self, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn open_child_optional(&self, _name: &OsStr) -> Result<Option<Self>, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn open_regular(&self, _name: &OsStr) -> Result<File, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn open_regular_optional(&self, _name: &OsStr) -> Result<Option<File>, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn verify_private_namespace(&self) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn open_child_private(&self, _name: &OsStr) -> Result<Self, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn open_child_private_optional(
        &self,
        _name: &OsStr,
    ) -> Result<Option<Self>, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn create_regular(&self, _name: &OsStr) -> Result<File, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn create_regular_private(&self, _name: &OsStr) -> Result<File, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn open_regular_private(&self, _name: &OsStr) -> Result<File, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn names(&self) -> Result<Vec<OsString>, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn names_private(&self) -> Result<Vec<OsString>, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn names_bounded(&self, _limit: usize) -> Result<Vec<OsString>, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn create_child_private(&self, _name: &OsStr) -> Result<Self, CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn rename_child_no_replace(
        &self,
        _source: &OsStr,
        _destination: &OsStr,
    ) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn rename_child_private_no_replace(
        &self,
        _source: &OsStr,
        _destination: &OsStr,
    ) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn rename_child_to_no_replace(
        &self,
        _source: &OsStr,
        _destination_directory: &Self,
        _destination: &OsStr,
    ) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn rename_child_private_to_no_replace(
        &self,
        _source: &OsStr,
        _destination_directory: &Self,
        _destination: &OsStr,
    ) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn remove_regular(&self, _name: &OsStr) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn remove_regular_private(&self, _name: &OsStr) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn remove_empty_child(&self, _name: &OsStr) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn remove_empty_child_private(&self, _name: &OsStr) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn measured_tree_bytes(
        &self,
        _max_depth: u8,
        _max_entries: usize,
    ) -> Result<u64, CliError> {
        Err(transaction_platform_unavailable())
    }

    fn verify_namespace(&self) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }

    pub(crate) fn sync(&self) -> Result<(), CliError> {
        Err(transaction_platform_unavailable())
    }
}

impl PreparedTransaction {
    /// Commit every staged target, or recover the complete old set.
    pub fn commit(mut self) -> Result<Vec<PathBuf>, CliError> {
        self.commit_with_hook(|_, _| Ok(HookDecision::Continue))
    }

    /// Discard a transaction which has not begun committing.
    pub fn abort(mut self) -> Result<(), CliError> {
        self.temporary_reservations.clear();
        let result = recover_transaction(&self.root, &self.directory, self.lock.take());
        self.deactivate();
        result
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn commit_with_hook(
        &mut self,
        mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<Vec<PathBuf>, CliError> {
        self.journal.phase = JournalPhase::Committing;
        if let Err(error) = persist_journal_handle(&self.handles.directory, &mut self.journal) {
            return self.fail_and_recover(error);
        }
        if let Err(error) = crash_point(&mut hook, "committing", usize::MAX, self) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return self.fail_and_recover(error);
        }

        let authenticated = match authenticate_targets(&self.handles.root, &self.journal.entries) {
            Ok(targets) => targets,
            Err(error) => return self.fail_and_recover(error),
        };
        if let Err(error) =
            validate_parent_leases(&self.handles.directory, &authenticated, &self.journal)
        {
            return self.fail_and_recover_authenticated(error, &authenticated);
        }

        // Validate every destination immediately before the first output-set
        // mutation. This prevents a late directory/FIFO/link swap on a later
        // entry from producing an avoidable partially installed set.
        for (index, target) in authenticated.iter().enumerate() {
            if let Err(error) = self.context.checkpoint().map_err(CliError::from) {
                return self.fail_and_recover(error);
            }
            let expected = self.journal.entries[index].original.clone();
            if let Err(error) = verify_target_handle_identity(target, expected.as_ref()) {
                return self.fail_and_recover(error);
            }
        }
        if let Err(error) = crash_point(&mut hook, "afterTargetAuthentication", usize::MAX, self) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return self.fail_and_recover(error);
        }
        self.preserve_staged_files();

        for index in 0..self.journal.entries.len() {
            if let Err(error) = self.context.checkpoint().map_err(CliError::from) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = call_hook(&mut hook, "beforeTarget", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            let target = &authenticated[index];

            let expected = self.journal.entries[index].original.clone();
            if let Err(error) = target.parent.verify_namespace() {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = verify_target_handle_identity(target, expected.as_ref()) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if expected.is_some() {
                let backup = backup_name(index);
                if let Err(error) =
                    handle_rename(&target.parent, &target.name, &self.handles.directory, &backup)
                {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) =
                    verify_name_identity(&self.handles.directory, &backup, expected.as_ref())
                {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) = target.parent.sync() {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) = self.handles.directory.sync() {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) = crash_point(&mut hook, "backupRenamed", index, self) {
                    if error.code() == "simulatedCrash" {
                        return Err(error);
                    }
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                self.journal.entries[index].state = EntryState::BackedUp;
                if let Err(error) =
                    persist_journal_handle(&self.handles.directory, &mut self.journal)
                {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) = crash_point(&mut hook, "backupJournaled", index, self) {
                    if error.code() == "simulatedCrash" {
                        return Err(error);
                    }
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
            }

            if let Err(error) = install_stage_no_replace_handle(
                &self.handles.directory,
                &stage_name(index),
                &target.parent,
                &target.name,
            ) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = target.parent.sync() {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = self.handles.directory.sync() {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = verify_handle_content(target, &self.journal.entries[index]) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = crash_point(&mut hook, "targetInstalled", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            self.journal.entries[index].state = EntryState::Installed;
            if let Err(error) = persist_journal_handle(&self.handles.directory, &mut self.journal) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = crash_point(&mut hook, "installJournaled", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
        }

        for target in &authenticated {
            if let Err(error) = target.parent.verify_namespace() {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
        }

        self.journal.phase = JournalPhase::Committed;
        if let Err(error) = persist_journal_handle(&self.handles.directory, &mut self.journal) {
            return self.fail_and_recover_authenticated(error, &authenticated);
        }
        crash_point(&mut hook, "committed", usize::MAX, self)?;
        let targets = self
            .journal
            .entries
            .iter()
            .map(|entry| target_path(&self.root, entry))
            .collect::<Result<Vec<_>, _>>()?;
        match finish_committed(&self.root, &self.directory, &self.journal, self.lock.take()) {
            Ok(()) => {
                self.deactivate();
                Ok(targets)
            }
            Err(error) => {
                self.deactivate();
                Err(recovery_failed("committed output cleanup", &error))
            }
        }
    }

    fn fail_and_recover<T>(&mut self, original: CliError) -> Result<T, CliError> {
        self.temporary_reservations.clear();
        match recover_transaction(&self.root, &self.directory, self.lock.take()) {
            Ok(()) => {
                self.deactivate();
                Err(original)
            }
            Err(recovery) => {
                self.deactivate();
                Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction failed ({}: {}); rollback failed and journal was preserved ({}: {})",
                        original.code(),
                        original.message(),
                        recovery.code(),
                        recovery.message()
                    ),
                ))
            }
        }
    }

    fn fail_and_recover_authenticated<T>(
        &mut self,
        original: CliError,
        targets: &[AuthenticatedTarget],
    ) -> Result<T, CliError> {
        #[cfg(not(unix))]
        {
            let _ = targets;
            return self.fail_and_recover(original);
        }
        #[cfg(unix)]
        {
            self.temporary_reservations.clear();
            let rollback =
                rollback_transaction_with_handles(&self.handles.directory, targets, &self.journal);
            if let Err(recovery) = rollback {
                self.lock.take();
                self.deactivate();
                return Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction failed ({}: {}); rollback through authenticated handles failed and journal was preserved ({}: {})",
                        original.code(),
                        original.message(),
                        recovery.code(),
                        recovery.message()
                    ),
                ));
            }
            match recover_transaction(&self.root, &self.directory, self.lock.take()) {
                Ok(()) => {
                    self.deactivate();
                    Err(original)
                }
                Err(recovery) => {
                    self.lock.take();
                    self.deactivate();
                    Err(CliError::new(
                        ExitClass::Io,
                        "rollbackFailed",
                        format!(
                            "output transaction failed ({}: {}); the old set was restored through authenticated handles, but journal cleanup was preserved for later recovery ({}: {})",
                            original.code(),
                            original.message(),
                            recovery.code(),
                            recovery.message()
                        ),
                    ))
                }
            }
        }
    }

    fn preserve_staged_files(&mut self) {
        self.temporary_reservations.clear();
    }

    #[cfg(test)]
    fn abandon_for_test(mut self) {
        self.preserve_staged_files();
        self.lock.take();
        self.deactivate();
    }

    fn deactivate(&mut self) {
        if self.active {
            active_transactions()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.directory);
            self.active = false;
        }
    }
}

impl Drop for PreparedTransaction {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// Recover manager-owned transactions in the exact root, then fully stage a new transaction.
pub fn prepare(
    targets: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
) -> Result<PreparedTransaction, CliError> {
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        match prepare_with_hook(targets, overwrite, context, |_, _| Ok(HookDecision::Continue)) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

/// Recover any interrupted transaction owning one of these physical parent
/// directories before higher-level conflict planning observes the filesystem.
pub fn recover_for_paths(paths: &[PathBuf], context: &ExecutionContext) -> Result<(), CliError> {
    ensure_transaction_platform()?;
    let paths = paths.iter().map(|path| absolute_lexical(path)).collect::<Result<Vec<_>, _>>()?;
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        let parents = open_target_parents(&paths)?;
        match recover_parent_transactions(&parents) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

#[allow(clippy::too_many_lines)]
pub(crate) fn prepare_with_hook(
    targets: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
    mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<PreparedTransaction, CliError> {
    ensure_transaction_platform()?;
    #[cfg(not(unix))]
    return Err(transaction_platform_unavailable());
    #[cfg(unix)]
    {
        if targets.is_empty() {
            return Err(CliError::internal("empty output transaction"));
        }
        if targets.len() > MAX_JOURNAL_ENTRIES {
            return Err(CliError::new(
                ExitClass::Policy,
                "transactionJournalLimit",
                "output transaction has too many targets",
            ));
        }
        let paths = targets
            .iter()
            .map(|target| absolute_lexical(&target.path))
            .collect::<Result<Vec<_>, _>>()?;
        let parent_handles = open_target_parents(&paths)?;
        recover_parent_transactions(&parent_handles)?;
        let root = common_existing_ancestor(&paths)?;
        ensure_same_filesystem(&root, &paths)?;
        #[cfg(unix)]
        let root_handle = SafeDir::open_absolute(&root)?;

        let mut entries = Vec::with_capacity(targets.len());
        let mut seen = BTreeSet::new();
        let mut seen_originals = BTreeSet::new();
        for (target, absolute) in targets.iter().zip(&paths) {
            let relative = absolute.strip_prefix(&root).map_err(|_| {
                CliError::new(
                    ExitClass::Io,
                    "outputPathUnsupported",
                    "target is outside transaction root",
                )
            })?;
            validate_relative_path(relative)?;
            let encoded = encode_path(relative)?;
            if !seen.insert(encoded.units.clone()) {
                return Err(CliError::new(
                    ExitClass::Io,
                    "outputConflict",
                    format!("duplicate transaction target: {}", absolute.display()),
                ));
            }
            #[cfg(unix)]
            let original = {
                let parent =
                    absolute.parent().ok_or_else(|| recovery_error("target has no parent"))?;
                let parent_relative = parent.strip_prefix(&root).map_err(|_| {
                    recovery_error("target parent is outside authenticated transaction root")
                })?;
                let parent_handle = root_handle.open_descendant(parent_relative)?;
                parent_handle.verify_namespace()?;
                parent_handle.inspect_regular(
                    absolute
                        .file_name()
                        .ok_or_else(|| recovery_error("target has no file name"))?,
                )?
            };
            #[cfg(not(unix))]
            let original = None;
            if original.is_some() && !overwrite {
                return Err(CliError::new(
                    ExitClass::Io,
                    "outputConflict",
                    format!("output target already exists: {}", absolute.display()),
                ));
            }
            if let Some(identity) = &original
                && !seen_originals.insert(identity.clone())
            {
                return Err(CliError::new(
                    ExitClass::Io,
                    "outputConflict",
                    "multiple output paths resolve to the same existing file",
                ));
            }
            entries.push(JournalEntry {
                target: encoded,
                original,
                content_sha256: sha256_hex(target.bytes),
                size: u64::try_from(target.bytes.len()).map_err(|_| {
                    CliError::new(
                        ExitClass::Policy,
                        "resourceLimit",
                        "target size cannot be represented",
                    )
                })?,
                state: EntryState::Prepared,
            });
        }

        let encoded_root = encode_path(&root)?;
        let parent_identities =
            parent_handles.iter().map(|parent| parent.identity.clone()).collect::<Vec<_>>();
        let (nonce, initial_directory, directory, lock) = create_initial_transaction(&root)?;
        #[cfg(unix)]
        let registry_handle = transaction_registry(&root_handle, false)?
            .ok_or_else(|| recovery_error("transaction registry disappeared"))?;
        #[cfg(unix)]
        let initial_handle = registry_handle.open_child(
            initial_directory
                .file_name()
                .ok_or_else(|| recovery_error("initial transaction has no name"))?,
        )?;
        let mut journal = Journal {
            signature: JOURNAL_SIGNATURE.into(),
            version: JOURNAL_VERSION,
            nonce,
            root: encoded_root,
            #[cfg(unix)]
            root_identity: root_handle.identity.clone(),
            parent_identities,
            #[cfg(not(unix))]
            root_identity: FileIdentity {
                platform: "unsupported".into(),
                first: 0,
                second: 0,
                size: 0,
            },
            #[cfg(not(unix))]
            parent_identities: Vec::new(),
            generation: 0,
            phase: JournalPhase::Staging,
            entries,
        };
        if let Err(error) = persist_journal_handle(&initial_handle, &mut journal) {
            drop(lock);
            let _ = remove_initial_transaction_handle(
                &registry_handle,
                initial_directory.file_name().expect("initial transaction has a name"),
            );
            return Err(error);
        }
        if let Err(error) = create_parent_leases(&parent_handles, &initial_handle, &journal) {
            let cleanup = remove_parent_leases(&parent_handles, &initial_handle, &journal);
            drop(lock);
            if let Err(cleanup) = cleanup {
                return Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction lease creation failed ({}: {}); lease rollback failed and the initial journal was preserved ({}: {})",
                        error.code(),
                        error.message(),
                        cleanup.code(),
                        cleanup.message()
                    ),
                ));
            }
            let _ = remove_initial_transaction_handle(
                &registry_handle,
                initial_directory.file_name().expect("initial transaction has a name"),
            );
            return Err(error);
        }
        if let Err(error) = handle_rename(
            &registry_handle,
            initial_directory.file_name().expect("initial transaction has a name"),
            &registry_handle,
            directory.file_name().expect("transaction has a name"),
        ) {
            let cleanup = remove_parent_leases(&parent_handles, &initial_handle, &journal);
            drop(lock);
            if let Err(cleanup) = cleanup {
                return Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction publication failed ({}: {}); lease rollback failed and the initial journal was preserved ({}: {})",
                        error.code(),
                        error.message(),
                        cleanup.code(),
                        cleanup.message()
                    ),
                ));
            }
            let _ = remove_initial_transaction_handle(
                &registry_handle,
                initial_directory.file_name().expect("initial transaction has a name"),
            );
            return Err(error);
        }
        if let Err(error) = registry_handle.sync() {
            // The authenticated transaction is now visible. Leave it intact for a
            // later bounded recovery scan instead of guessing whether the rename
            // reached stable storage.
            drop(lock);
            return Err(error);
        }
        active_transactions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(directory.clone());
        #[cfg(unix)]
        let directory_handle = registry_handle.open_child(
            directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
        )?;
        let mut transaction = PreparedTransaction {
            root: root.clone(),
            directory: directory.clone(),
            journal,
            context: context.clone(),
            active: true,
            temporary_reservations: Vec::with_capacity(targets.len()),
            lock: Some(lock),
            handles: TransactionHandles {
                #[cfg(unix)]
                root: root_handle,
                #[cfg(unix)]
                directory: directory_handle,
                #[cfg(not(unix))]
                root: SafeDir,
                #[cfg(not(unix))]
                directory: SafeDir,
            },
        };
        if let Err(error) = crash_point(&mut hook, "journalCreated", usize::MAX, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        for (index, target) in targets.iter().enumerate() {
            if let Err(error) = call_hook(&mut hook, "beforeStage", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
            let amount = u64::try_from(target.bytes.len()).map_err(|_| {
                CliError::new(
                    ExitClass::Policy,
                    "resourceLimit",
                    "target size cannot be represented",
                )
            })?;
            let reservation = match context.reserve_temporary(amount).map_err(CliError::from) {
                Ok(reservation) => reservation,
                Err(error) => return transaction.fail_and_recover(error),
            };
            transaction.temporary_reservations.push(reservation);
            #[cfg(unix)]
            let mut file = match transaction.handles.directory.create_regular(&stage_name(index)) {
                Ok(file) => file,
                Err(error) => return transaction.fail_and_recover(error),
            };
            if let Err(error) = crash_point(&mut hook, "stageAllocated", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = context
                .checkpoint()
                .map_err(CliError::from)
                .and_then(|()| file.write_all(target.bytes).map_err(CliError::from))
            {
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = crash_point(&mut hook, "stageWritten", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = call_hook(&mut hook, "beforeStageSync", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = context
                .checkpoint()
                .map_err(CliError::from)
                .and_then(|()| file.sync_all().map_err(CliError::from))
            {
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = crash_point(&mut hook, "stageSynced", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
        }
        if let Err(error) = transaction.handles.directory.sync() {
            return transaction.fail_and_recover(error);
        }
        transaction.journal.phase = JournalPhase::Prepared;
        if let Err(error) =
            persist_journal_handle(&transaction.handles.directory, &mut transaction.journal)
        {
            return transaction.fail_and_recover(error);
        }
        if let Err(error) = crash_point(&mut hook, "prepared", usize::MAX, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        Ok(transaction)
    }
}

fn call_hook(
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    phase: &str,
    index: usize,
    transaction: &mut PreparedTransaction,
) -> Result<(), CliError> {
    #[cfg(not(test))]
    let _ = transaction;
    match hook(phase, index)? {
        HookDecision::Continue => Ok(()),
        #[cfg(test)]
        HookDecision::SimulateCrash => {
            transaction.preserve_staged_files();
            transaction.deactivate();
            Err(CliError::new(ExitClass::Io, "simulatedCrash", format!("{phase}:{index}")))
        }
    }
}

fn crash_point(
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    phase: &str,
    index: usize,
    transaction: &mut PreparedTransaction,
) -> Result<(), CliError> {
    call_hook(hook, phase, index, transaction)
}

fn active_transactions() -> &'static Mutex<BTreeSet<PathBuf>> {
    ACTIVE_TRANSACTIONS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

#[allow(clippy::unnecessary_wraps)]
fn ensure_transaction_platform() -> Result<(), CliError> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        Err(transaction_platform_unavailable())
    }
}

#[allow(dead_code)]
fn transaction_platform_unavailable() -> CliError {
    CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "output transactions require audited relative directory-handle filesystem operations",
    )
}

#[cfg(unix)]
fn open_target_parents(targets: &[PathBuf]) -> Result<Vec<SafeDir>, CliError> {
    let mut parents = BTreeMap::new();
    for target in targets {
        let name = target.file_name().ok_or_else(|| recovery_error("target has no file name"))?;
        if name == OsStr::new(PARENT_LEASE_NAME) || name == OsStr::new(REGISTRY_NAME) {
            return Err(CliError::new(
                ExitClass::Io,
                "outputPathUnsupported",
                "output target conflicts with the transaction manager namespace",
            ));
        }
        let parent = target.parent().ok_or_else(|| recovery_error("target has no parent"))?;
        let handle = SafeDir::open_or_create_absolute(parent)?;
        parents.entry(handle.identity.clone()).or_insert(handle);
    }
    Ok(parents.into_values().collect())
}

#[cfg(not(unix))]
fn open_target_parents(_targets: &[PathBuf]) -> Result<Vec<SafeDir>, CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
struct RecoveryReference {
    root: PathBuf,
    directory: PathBuf,
    directory_handle: SafeDir,
    initial: bool,
}

/// Recover transactions named by the fixed leases in the authenticated
/// physical target-parent directories. Recovery completes before this call
/// asks the caller to repeat preflight.
#[allow(clippy::too_many_lines)]
fn recover_parent_transactions(parents: &[SafeDir]) -> Result<(), CliError> {
    #[cfg(not(unix))]
    {
        let _ = parents;
        return Err(transaction_platform_unavailable());
    }
    #[cfg(unix)]
    {
        let mut references = BTreeMap::new();
        for parent in parents {
            let Some(lease) = load_parent_lease(parent)? else { continue };
            let root_path = decode_path(&lease.root)?;
            let root = SafeDir::open_absolute(&root_path)
                .map_err(|error| recovery_failed("open leased transaction root", &error))?;
            if root.identity != lease.root_identity {
                return Err(recovery_error("leased transaction root identity changed"));
            }
            let registry = transaction_registry(&root, false)?
                .ok_or_else(|| recovery_error("leased transaction registry is missing"))?;
            let managed_name = OsString::from(format!("{TRANSACTION_PREFIX}{}", lease.nonce));
            let initial_name = OsString::from(format!("{INITIAL_PREFIX}{}", lease.nonce));
            let cleanup_name = OsString::from(format!("{CLEANUP_PREFIX}{}", lease.nonce));
            let (name, directory_handle, initial) =
                if let Ok(handle) = registry.open_child(&managed_name) {
                    (managed_name, handle, false)
                } else if let Ok(handle) = registry.open_child(&initial_name) {
                    (initial_name, handle, true)
                } else if let Ok(handle) = registry.open_child(&cleanup_name) {
                    (cleanup_name, handle, false)
                } else {
                    return Err(recovery_error("leased transaction directory is missing"));
                };
            let directory = root.path.join(REGISTRY_NAME).join(&name);
            let journal = load_journal_handle(&root, &directory_handle, &directory, &lease.nonce)?;
            validate_parent_lease(parent, &directory_handle, &journal, &lease)?;
            let key = (root.identity.clone(), lease.nonce.clone());
            references.entry(key).or_insert(RecoveryReference {
                root: root.path.clone(),
                directory,
                directory_handle,
                initial,
            });
        }
        if references.is_empty() {
            return Ok(());
        }
        if references.len() > MAX_RECOVERY_TRANSACTIONS {
            return Err(CliError::new(
                ExitClass::Io,
                "transactionRecoveryLimit",
                "too many physical parent transactions require recovery",
            ));
        }
        for reference in references.into_values() {
            let lock = try_recovery_lock_handle(&reference.directory_handle)
                .map_err(|error| recovery_failed("authenticate transaction lock", &error))?
                .ok_or_else(|| {
                    CliError::new(
                        ExitClass::Io,
                        "transactionBusy",
                        format!(
                            "an active output transaction covers {}",
                            reference.directory.display()
                        ),
                    )
                })?;
            if reference.initial {
                recover_initial_transaction(
                    &reference.root,
                    &reference.directory,
                    &reference.directory_handle,
                    Some(lock),
                )?;
            } else if reference
                .directory
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(CLEANUP_PREFIX))
            {
                recover_cleanup_transaction(
                    &reference.root,
                    &reference.directory,
                    &reference.directory_handle,
                    Some(lock),
                )?;
            } else {
                recover_transaction(&reference.root, &reference.directory, Some(lock)).map_err(
                    |error| recovery_failed("recover physical parent transaction", &error),
                )?;
            }
        }
        Err(CliError::new(
            ExitClass::Io,
            "transactionRecoveredRetry",
            "an interrupted transaction covering this target was recovered; retry the write",
        ))
    }
}

/// Recover every exact manager transaction directory directly under `root`.
#[cfg(all(test, unix))]
pub fn recover_pending(root: &Path) -> Result<(), CliError> {
    let root = root.canonicalize()?;
    #[cfg(unix)]
    let root_handle = SafeDir::open_absolute(&root)?;
    #[cfg(unix)]
    let Some(registry) = transaction_registry(&root_handle, false)? else { return Ok(()) };
    let active =
        active_transactions().lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let mut managed = Vec::new();
    #[cfg(unix)]
    let recovery_names = registry.names()?;
    #[cfg(not(unix))]
    let recovery_names = Vec::<OsString>::new();
    for (scanned, name) in recovery_names.into_iter().enumerate() {
        if scanned >= MAX_RECOVERY_DIRECTORY_ENTRIES {
            return Err(CliError::new(
                ExitClass::Io,
                "transactionRecoveryLimit",
                format!(
                    "recovery scan exceeded {MAX_RECOVERY_DIRECTORY_ENTRIES} entries under {}",
                    root.display()
                ),
            ));
        }
        let Some(nonce) = managed_nonce(&name) else { continue };
        let path = root.join(REGISTRY_NAME).join(&name);
        if active.contains(&path) {
            continue;
        }
        #[cfg(unix)]
        let _ = registry
            .open_child(&name)
            .map_err(|error| recovery_failed("authenticate transaction directory", &error))?;
        #[cfg(not(unix))]
        verify_manager_directory(&path)
            .map_err(|error| recovery_failed("authenticate transaction directory", &error))?;
        managed.push((path, nonce));
        if managed.len() > MAX_RECOVERY_TRANSACTIONS {
            return Err(CliError::new(
                ExitClass::Io,
                "transactionRecoveryLimit",
                format!(
                    "more than {MAX_RECOVERY_TRANSACTIONS} pending transactions under {}",
                    root.display()
                ),
            ));
        }
    }
    managed.sort_by(|left, right| left.0.cmp(&right.0));
    for (directory, nonce) in managed {
        #[cfg(unix)]
        let directory_handle = registry.open_child(
            directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
        )?;
        #[cfg(unix)]
        let Some(lock) = try_recovery_lock_handle(&directory_handle)
            .map_err(|error| recovery_failed("authenticate transaction lock", &error))?
        else {
            continue;
        };
        let journal = load_journal(&root, &directory, &nonce)?;
        if journal.phase == JournalPhase::Committed {
            finish_committed(&root, &directory, &journal, Some(lock))
                .map_err(|error| recovery_failed("finish committed transaction", &error))?;
        } else {
            rollback_transaction(&root, &directory, &journal, Some(lock))
                .map_err(|error| recovery_failed("rollback interrupted transaction", &error))?;
        }
    }
    recover_cleanup_directories(&registry)?;
    Ok(())
}

#[cfg(all(unix, test))]
fn recover_cleanup_directories(registry: &SafeDir) -> Result<(), CliError> {
    for name in registry.names()? {
        let Some(nonce) =
            name.to_str().and_then(|name| name.strip_prefix(CLEANUP_PREFIX)).filter(|nonce| {
                nonce.len() == 32
                    && nonce
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
        else {
            continue;
        };
        let cleanup = registry.open_child(&name)?;
        for member in cleanup.names()? {
            if !matches!(
                member.to_str(),
                Some("journal-a.json" | "journal-b.json" | "transaction.lock")
            ) {
                return Err(recovery_error(format!(
                    "unexpected cleanup member for transaction {nonce}"
                )));
            }
            remove_regular_handle_if_present(&cleanup, &member)?;
        }
        cleanup.sync()?;
        rustix::fs::unlinkat(&registry.fd, &name, rustix::fs::AtFlags::REMOVEDIR)?;
        registry.sync()?;
    }
    Ok(())
}

fn recover_transaction(root: &Path, directory: &Path, lock: Option<File>) -> Result<(), CliError> {
    let name = directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?;
    let nonce = managed_nonce(name).ok_or_else(|| recovery_error("invalid transaction name"))?;
    let journal = load_journal(root, directory, &nonce)?;
    if journal.phase == JournalPhase::Committed {
        finish_committed(root, directory, &journal, lock)
    } else {
        rollback_transaction(root, directory, &journal, lock)
    }
}

#[cfg(unix)]
fn recover_initial_transaction(
    root: &Path,
    directory: &Path,
    directory_handle: &SafeDir,
    lock: Option<File>,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("initial recovery requires an owned lock"))?;
    let nonce = directory
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(INITIAL_PREFIX))
        .ok_or_else(|| recovery_error("invalid initial transaction name"))?;
    let root_handle = SafeDir::open_absolute(root)?;
    let journal = load_journal_handle(&root_handle, directory_handle, directory, nonce)?;
    if journal.phase != JournalPhase::Staging {
        return Err(recovery_error("initial transaction has advanced beyond staging"));
    }
    validate_recovery_layout(root, directory, &journal)?;
    let parents = journal_parent_handles(&root_handle, &journal)?;
    remove_parent_leases(&parents, directory_handle, &journal)?;
    drop(lock);
    let registry = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("initial transaction registry is missing"))?;
    remove_initial_transaction_handle(
        &registry,
        directory.file_name().ok_or_else(|| recovery_error("initial transaction has no name"))?,
    )
}

#[cfg(unix)]
fn recover_cleanup_transaction(
    root: &Path,
    directory: &Path,
    directory_handle: &SafeDir,
    lock: Option<File>,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("cleanup recovery requires an owned lock"))?;
    let nonce = directory
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(CLEANUP_PREFIX))
        .ok_or_else(|| recovery_error("invalid cleanup transaction name"))?;
    let root_handle = SafeDir::open_absolute(root)?;
    let journal = load_journal_handle(&root_handle, directory_handle, directory, nonce)?;
    validate_recovery_layout(root, directory, &journal)?;
    let parents = journal_parent_handles(&root_handle, &journal)?;
    remove_parent_leases(&parents, directory_handle, &journal)?;
    drop(lock);
    let registry = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("cleanup transaction registry is missing"))?;
    for name in ["journal-a.json", "journal-b.json", "transaction.lock"] {
        remove_regular_handle_if_present(directory_handle, OsStr::new(name))?;
    }
    directory_handle.sync()?;
    rustix::fs::unlinkat(
        &registry.fd,
        directory.file_name().ok_or_else(|| recovery_error("cleanup has no name"))?,
        rustix::fs::AtFlags::REMOVEDIR,
    )?;
    registry.sync()
}

fn rollback_transaction(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
) -> Result<(), CliError> {
    validate_recovery_layout(root, directory, journal)?;
    #[cfg(unix)]
    let root_handle = SafeDir::open_absolute(root)?;
    #[cfg(unix)]
    let registry_handle = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("transaction registry is missing"))?;
    #[cfg(unix)]
    let directory_handle = registry_handle.open_child(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )?;
    #[cfg(unix)]
    let targets = authenticate_targets(&root_handle, &journal.entries)?;
    #[cfg(unix)]
    rollback_transaction_with_handles(&directory_handle, &targets, journal)?;
    #[cfg(not(unix))]
    return Err(transaction_platform_unavailable());
    remove_transaction_directory(root, directory, journal, lock)
}

#[cfg(unix)]
fn rollback_transaction_with_handles(
    directory: &SafeDir,
    targets: &[AuthenticatedTarget],
    journal: &Journal,
) -> Result<(), CliError> {
    let mut failures = Vec::new();
    for (index, entry) in journal.entries.iter().enumerate().rev() {
        let result = rollback_entry_handle(directory, &targets[index], journal, index, entry);
        if let Err(error) = result
            && failures.len() < 16
        {
            failures.push(format!("entry {index}: {}: {}", error.code(), error.message()));
        }
    }
    if !failures.is_empty() {
        return Err(CliError::new(
            ExitClass::Io,
            "rollbackFailed",
            format!(
                "one or more rollback operations failed; journal and backups were preserved: {}",
                failures.join("; ")
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn rollback_entry_handle(
    directory: &SafeDir,
    target: &AuthenticatedTarget,
    journal: &Journal,
    index: usize,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    let backup = backup_name(index);
    let staged = stage_name(index);
    let backup_identity = directory.inspect_regular(&backup)?;
    let target_identity = target.parent.inspect_regular(&target.name)?;

    if let Some(original) = &entry.original {
        if let Some(found) = &backup_identity {
            if found != original {
                return Err(recovery_error(format!(
                    "backup identity mismatch: {}",
                    directory.path.join(&backup).display()
                )));
            }
            if target_identity.is_some() {
                verify_handle_content(target, entry)?;
                rustix::fs::unlinkat(
                    &target.parent.fd,
                    &target.name,
                    rustix::fs::AtFlags::empty(),
                )?;
                target.parent.sync()?;
            }
            handle_rename(directory, &backup, &target.parent, &target.name)?;
            target.parent.sync()?;
            directory.sync()?;
        } else {
            let Some(found) = target_identity else {
                return Err(recovery_error(format!(
                    "original and backup are both missing: {}",
                    target.parent.path.join(&target.name).display()
                )));
            };
            if &found != original {
                return Err(recovery_error(format!(
                    "original identity mismatch: {}",
                    target.parent.path.join(&target.name).display()
                )));
            }
        }
    } else if target_identity.is_some() {
        verify_handle_content(target, entry)?;
        rustix::fs::unlinkat(&target.parent.fd, &target.name, rustix::fs::AtFlags::empty())?;
        target.parent.sync()?;
    }

    if directory.inspect_regular(&staged)?.is_some() {
        if journal.phase != JournalPhase::Staging {
            verify_file_content(
                directory.open_regular(&staged)?,
                &directory.path.join(&staged),
                entry,
            )?;
        }
        rustix::fs::unlinkat(&directory.fd, &staged, rustix::fs::AtFlags::empty())?;
        directory.sync()?;
    }
    Ok(())
}

fn finish_committed(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
) -> Result<(), CliError> {
    validate_recovery_layout(root, directory, journal)?;
    #[cfg(unix)]
    let root_handle = SafeDir::open_absolute(root)?;
    #[cfg(unix)]
    let registry_handle = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("transaction registry is missing"))?;
    #[cfg(unix)]
    let directory_handle = registry_handle.open_child(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )?;
    #[cfg(unix)]
    let targets = authenticate_targets(&root_handle, &journal.entries)?;
    #[cfg(unix)]
    for (index, (entry, target)) in journal.entries.iter().zip(&targets).enumerate() {
        verify_handle_content(target, entry)?;
        let backup = backup_name(index);
        if let Some(identity) = directory_handle.inspect_regular(&backup)? {
            if entry.original.as_ref() != Some(&identity) {
                return Err(recovery_error(format!(
                    "committed backup identity mismatch: {}",
                    directory_handle.path.join(&backup).display()
                )));
            }
            rustix::fs::unlinkat(&directory_handle.fd, &backup, rustix::fs::AtFlags::empty())?;
        }
        let staged = stage_name(index);
        if directory_handle.inspect_regular(&staged)?.is_some() {
            verify_file_content(
                directory_handle.open_regular(&staged)?,
                &directory_handle.path.join(&staged),
                entry,
            )?;
            rustix::fs::unlinkat(&directory_handle.fd, &staged, rustix::fs::AtFlags::empty())?;
        }
        directory_handle.sync()?;
    }
    #[cfg(not(unix))]
    return Err(transaction_platform_unavailable());
    remove_transaction_directory(root, directory, journal, lock)
}

fn validate_recovery_layout(
    root: &Path,
    directory: &Path,
    journal: &Journal,
) -> Result<(), CliError> {
    validate_journal(root, directory, journal)?;
    let allowed = allowed_transaction_names(journal);
    #[cfg(unix)]
    let directory_handle = SafeDir::open_absolute(directory)?;
    #[cfg(unix)]
    for name in directory_handle.names()? {
        if !allowed.contains(&name) {
            return Err(recovery_error(format!(
                "unexpected transaction member: {}",
                directory.join(&name).display()
            )));
        }
        let _ = directory_handle.open_regular(&name)?;
    }
    #[cfg(not(unix))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

fn remove_transaction_directory(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("transaction cleanup requires an owned lock"))?;
    #[cfg(unix)]
    let root_handle = SafeDir::open_absolute(root)?;
    #[cfg(unix)]
    let registry_handle = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("transaction registry is missing"))?;
    #[cfg(unix)]
    let directory_handle = registry_handle.open_child(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )?;
    #[cfg(unix)]
    for index in 0..journal.entries.len() {
        remove_regular_handle_if_present(&directory_handle, &stage_name(index))?;
        remove_regular_handle_if_present(&directory_handle, &backup_name(index))?;
    }
    #[cfg(unix)]
    directory_handle.sync()?;

    // Atomically remove the directory from the recovery namespace while its
    // signed journals and exclusive lock still exist. Cleanup failures after
    // this point cannot cause a later recovery to reinterpret a completed set.
    let nonce = managed_nonce(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )
    .ok_or_else(|| recovery_error("transaction directory name is invalid"))?;
    let cleanup_name = OsString::from(format!("{CLEANUP_PREFIX}{nonce}"));
    #[cfg(unix)]
    match rustix::fs::statat(
        &registry_handle.fd,
        &cleanup_name,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Err(rustix::io::Errno::NOENT) => {}
        Ok(_) => return Err(recovery_error("transaction cleanup path already exists")),
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    handle_rename(
        &registry_handle,
        directory.file_name().expect("transaction name checked"),
        &registry_handle,
        &cleanup_name,
    )?;
    #[cfg(unix)]
    registry_handle.sync()?;

    #[cfg(unix)]
    let cleanup_handle = registry_handle.open_child(&cleanup_name)?;
    #[cfg(unix)]
    let parents = journal_parent_handles(&root_handle, journal)?;
    #[cfg(unix)]
    remove_parent_leases(&parents, &cleanup_handle, journal)?;
    drop(lock);
    #[cfg(unix)]
    for name in ["journal-a.json", "journal-b.json", "transaction.lock"] {
        remove_regular_handle_if_present(&cleanup_handle, OsStr::new(name))?;
    }
    #[cfg(unix)]
    cleanup_handle.sync()?;
    #[cfg(unix)]
    rustix::fs::unlinkat(&registry_handle.fd, &cleanup_name, rustix::fs::AtFlags::REMOVEDIR)?;
    #[cfg(unix)]
    registry_handle.sync()?;
    #[cfg(not(unix))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

#[cfg(unix)]
fn remove_regular_handle_if_present(directory: &SafeDir, name: &OsStr) -> Result<(), CliError> {
    match directory.inspect_regular(name) {
        Ok(Some(_)) => {
            rustix::fs::unlinkat(&directory.fd, name, rustix::fs::AtFlags::empty())?;
        }
        Ok(None) => {}
        Err(error) if error.code() == "outputTargetTypeDenied" => return Err(error),
        Err(error) => {
            if !error.message().contains("No such file or directory")
                && !error.message().contains("os error 2")
            {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn allowed_transaction_names(journal: &Journal) -> BTreeSet<OsString> {
    let mut names = BTreeSet::from([
        OsString::from("journal-a.json"),
        OsString::from("journal-b.json"),
        OsString::from("transaction.lock"),
    ]);
    for index in 0..journal.entries.len() {
        names.insert(OsString::from(format!("stage-{index}")));
        names.insert(OsString::from(format!("backup-{index}")));
    }
    for identity in &journal.parent_identities {
        names.insert(parent_marker_name(identity));
    }
    names
}

#[cfg(unix)]
fn persist_journal_handle(directory: &SafeDir, journal: &mut Journal) -> Result<(), CliError> {
    journal.generation = journal.generation.checked_add(1).ok_or_else(|| {
        CliError::new(ExitClass::Io, "transactionJournalOverflow", "journal generation overflow")
    })?;
    let name =
        if journal.generation.is_multiple_of(2) { "journal-b.json" } else { "journal-a.json" };
    let name = OsStr::new(name);
    if directory.inspect_regular(name)?.is_some() {
        rustix::fs::unlinkat(&directory.fd, name, rustix::fs::AtFlags::empty())?;
        directory.sync()?;
    }
    let bytes = serde_json::to_vec(journal).map_err(|error| {
        CliError::internal(format!("serialize output transaction journal: {error}"))
    })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_JOURNAL_BYTES {
        return Err(CliError::new(
            ExitClass::Policy,
            "transactionJournalLimit",
            "output transaction journal exceeds its byte limit",
        ));
    }
    let mut file = directory.create_regular(name)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    directory.sync()
}

#[cfg(not(unix))]
fn persist_journal_handle(_directory: &SafeDir, _journal: &mut Journal) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

fn load_journal(root: &Path, directory: &Path, nonce: &str) -> Result<Journal, CliError> {
    #[cfg(unix)]
    {
        let root_handle = SafeDir::open_absolute(root)?;
        let registry = transaction_registry(&root_handle, false)?
            .ok_or_else(|| recovery_error("transaction registry is missing"))?;
        let directory_name = directory
            .file_name()
            .ok_or_else(|| recovery_error("transaction directory has no name"))?;
        let directory_handle = registry.open_child(directory_name)?;
        load_journal_handle(&root_handle, &directory_handle, directory, nonce)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, directory, nonce);
        Err(transaction_platform_unavailable())
    }
}

#[cfg(unix)]
fn load_journal_handle(
    root: &SafeDir,
    directory: &SafeDir,
    directory_path: &Path,
    nonce: &str,
) -> Result<Journal, CliError> {
    let mut candidates = Vec::new();
    for name in ["journal-a.json", "journal-b.json"] {
        match read_limited_regular_handle(directory, OsStr::new(name), MAX_JOURNAL_BYTES) {
            Ok(bytes) => {
                if let Ok(journal) = serde_json::from_slice::<Journal>(&bytes)
                    && validate_journal_handle(root, directory_path, &journal).is_ok()
                    && journal.nonce == nonce
                {
                    candidates.push(journal);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    candidates.sort_by_key(|journal| journal.generation);
    let journal = candidates.pop().ok_or_else(|| {
        recovery_error(format!("no valid signed journal in {}", directory_path.display()))
    })?;
    if candidates.last().is_some_and(|other| other.generation == journal.generation) {
        return Err(recovery_error("ambiguous journal generations"));
    }
    Ok(journal)
}

fn validate_journal(root: &Path, directory: &Path, journal: &Journal) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        let root_handle = SafeDir::open_absolute(root)?;
        validate_journal_handle(&root_handle, directory, journal)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, directory, journal);
        Err(transaction_platform_unavailable())
    }
}

#[cfg(unix)]
fn validate_journal_handle(
    root: &SafeDir,
    directory: &Path,
    journal: &Journal,
) -> Result<(), CliError> {
    if journal.signature != JOURNAL_SIGNATURE || journal.version != JOURNAL_VERSION {
        return Err(recovery_error("invalid transaction signature or version"));
    }
    let valid_nonce = journal.nonce.len() == 32
        && journal.nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    let valid_names = [TRANSACTION_PREFIX, INITIAL_PREFIX, CLEANUP_PREFIX]
        .map(|prefix| OsString::from(format!("{prefix}{}", journal.nonce)));
    if !valid_nonce || !valid_names.iter().any(|name| directory.file_name() == Some(name)) {
        return Err(recovery_error("transaction nonce does not match directory"));
    }
    if journal.entries.is_empty() || journal.entries.len() > MAX_JOURNAL_ENTRIES {
        return Err(recovery_error("transaction entry count is outside limits"));
    }
    let encoded_root = decode_path(&journal.root)?;
    if encoded_root != root.path || journal.root_identity != root.identity {
        return Err(recovery_error("transaction root does not match recovery root"));
    }
    let mut targets = BTreeSet::new();
    for entry in &journal.entries {
        if entry.content_sha256.len() != 64
            || !entry.content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(recovery_error("invalid transaction content digest"));
        }
        let relative = decode_path(&entry.target)?;
        validate_relative_path(&relative)?;
        if !targets.insert(entry.target.units.clone()) {
            return Err(recovery_error("duplicate transaction target"));
        }
    }
    if journal.parent_identities.is_empty()
        || journal.parent_identities.len() > journal.entries.len()
        || !journal.parent_identities.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(recovery_error("journal physical parent identities are invalid"));
    }
    let _ = journal_parent_handles(root, journal)?;
    Ok(())
}

fn managed_nonce(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let nonce = name.strip_prefix(TRANSACTION_PREFIX)?;
    (nonce.len() == 32
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then(|| nonce.to_owned())
}

#[cfg(unix)]
fn create_initial_transaction(root: &Path) -> Result<(String, PathBuf, PathBuf, File), CliError> {
    let root_handle = SafeDir::open_absolute(root)?;
    let registry = transaction_registry(&root_handle, true)?
        .ok_or_else(|| recovery_error("transaction registry could not be created"))?;
    for attempt in 0_u32..128 {
        let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let digest =
            Sha256::digest(format!("{}:{time}:{counter}:{attempt}", std::process::id()).as_bytes());
        let nonce = hex_bytes(&digest[..16]);
        let initial_name = OsString::from(format!("{INITIAL_PREFIX}{nonce}"));
        let directory_name = OsString::from(format!("{TRANSACTION_PREFIX}{nonce}"));
        let initial = root.join(REGISTRY_NAME).join(&initial_name);
        let directory = root.join(REGISTRY_NAME).join(&directory_name);
        match rustix::fs::mkdirat(
            &registry.fd,
            &initial_name,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
        ) {
            Ok(()) => {
                let initial_handle = match registry.open_child(&initial_name) {
                    Ok(handle) => handle,
                    Err(error) => {
                        let _ = remove_initial_transaction_handle(&registry, &initial_name);
                        return Err(error);
                    }
                };
                let lock = match initial_handle.create_regular(OsStr::new("transaction.lock")) {
                    Ok(lock) => lock,
                    Err(error) => {
                        let _ = remove_initial_transaction_handle(&registry, &initial_name);
                        return Err(error);
                    }
                };
                if let Err(error) = lock.try_lock() {
                    drop(lock);
                    let _ = remove_initial_transaction_handle(&registry, &initial_name);
                    return Err(lock_error("create transaction lock", &error));
                }
                if let Err(error) = lock
                    .sync_all()
                    .map_err(CliError::from)
                    .and_then(|()| initial_handle.sync())
                    .and_then(|()| registry.sync())
                {
                    drop(lock);
                    let _ = remove_initial_transaction_handle(&registry, &initial_name);
                    return Err(error);
                }
                return Ok((nonce, initial, directory, lock));
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(CliError::new(
        ExitClass::Io,
        "transactionAllocationFailed",
        "could not allocate an output transaction directory",
    ))
}

#[cfg(not(unix))]
fn create_initial_transaction(_root: &Path) -> Result<(String, PathBuf, PathBuf, File), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn remove_initial_transaction_handle(registry: &SafeDir, name: &OsStr) -> Result<(), CliError> {
    let name_text =
        name.to_str().ok_or_else(|| recovery_error("initial transaction name is invalid"))?;
    let nonce = name_text
        .strip_prefix(INITIAL_PREFIX)
        .filter(|nonce| {
            nonce.len() == 32
                && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .ok_or_else(|| recovery_error("initial transaction name is not manager-owned"))?;
    let expected = format!("{INITIAL_PREFIX}{nonce}");
    if name != OsStr::new(&expected) {
        return Err(recovery_error("initial transaction nonce mismatch"));
    }
    let transaction = registry.open_child(name)?;
    for member in transaction.names()? {
        if !matches!(
            member.to_str(),
            Some("journal-a.json" | "journal-b.json" | "transaction.lock")
        ) && !member.to_string_lossy().starts_with(PARENT_MARKER_PREFIX)
        {
            return Err(recovery_error("unexpected initial transaction member"));
        }
        remove_regular_handle_if_present(&transaction, &member)?;
    }
    transaction.sync()?;
    rustix::fs::unlinkat(&registry.fd, name, rustix::fs::AtFlags::REMOVEDIR)?;
    registry.sync()?;
    Ok(())
}

#[cfg(unix)]
fn try_recovery_lock_handle(directory: &SafeDir) -> Result<Option<File>, CliError> {
    let lock = directory.open_regular(OsStr::new("transaction.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(lock_error(
            &format!("lock transaction for recovery: {}", directory.path.display()),
            &error,
        )),
    }
}

#[cfg(windows)]
fn verify_manager_directory(path: &Path) -> Result<(), CliError> {
    let handle = securely_open_regular_or_directory(path)?;
    if !handle.metadata()?.is_dir() {
        return Err(recovery_error(format!(
            "transaction path is not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_manager_directory(_path: &Path) -> Result<(), CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "secure transaction directories are unavailable",
    ))
}

fn lock_error(operation: &str, error: &std::fs::TryLockError) -> CliError {
    CliError::new(ExitClass::Io, "transactionLockFailed", format!("{operation}: {error}"))
}

fn target_path(root: &Path, entry: &JournalEntry) -> Result<PathBuf, CliError> {
    let relative = decode_path(&entry.target)?;
    validate_relative_path(&relative)?;
    Ok(root.join(relative))
}

fn stage_name(index: usize) -> OsString {
    OsString::from(format!("stage-{index}"))
}

fn backup_name(index: usize) -> OsString {
    OsString::from(format!("backup-{index}"))
}

fn parent_marker_name(identity: &FileIdentity) -> OsString {
    let mut digest = Sha256::new();
    digest.update(identity.platform.as_bytes());
    digest.update([0]);
    digest.update(identity.first.to_le_bytes());
    digest.update(identity.second.to_le_bytes());
    OsString::from(format!("{PARENT_MARKER_PREFIX}{}.json", hex_bytes(&digest.finalize())))
}

#[cfg(unix)]
fn create_parent_leases(
    parents: &[SafeDir],
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    for parent in parents {
        let name = parent_marker_name(&parent.identity);
        let lease = ParentLease {
            signature: JOURNAL_SIGNATURE.into(),
            version: JOURNAL_VERSION,
            nonce: journal.nonce.clone(),
            root: journal.root.clone(),
            root_identity: journal.root_identity.clone(),
            parent_identity: parent.identity.clone(),
        };
        let bytes = serde_json::to_vec(&lease)
            .map_err(|error| CliError::internal(format!("serialize parent lease: {error}")))?;
        let mut transaction_file = transaction.create_regular(&name)?;
        transaction_file.write_all(&bytes)?;
        transaction_file.write_all(b"\n")?;
        transaction_file.sync_all()?;
        transaction.sync()?;
        rustix::fs::linkat(
            &transaction.fd,
            &name,
            &parent.fd,
            PARENT_LEASE_NAME,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                CliError::new(
                    ExitClass::Io,
                    "transactionBusy",
                    format!("another output transaction owns parent {}", parent.path.display()),
                )
            } else {
                error.into()
            }
        })?;
        parent.sync()?;
    }
    transaction.sync()
}

#[cfg(unix)]
fn load_parent_lease(parent: &SafeDir) -> Result<Option<ParentLease>, CliError> {
    if parent.inspect_regular(OsStr::new(PARENT_LEASE_NAME))?.is_none() {
        return Ok(None);
    }
    let bytes = read_limited_regular_handle(parent, OsStr::new(PARENT_LEASE_NAME), 8 * 1024)
        .map_err(CliError::from)?;
    let lease: ParentLease =
        serde_json::from_slice(&bytes).map_err(|_| recovery_error("parent lease is malformed"))?;
    if lease.signature != JOURNAL_SIGNATURE
        || lease.version != JOURNAL_VERSION
        || lease.nonce.len() != 32
        || !lease.nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || lease.parent_identity != parent.identity
    {
        return Err(recovery_error("parent lease authentication failed"));
    }
    Ok(Some(lease))
}

#[cfg(unix)]
fn validate_parent_lease(
    parent: &SafeDir,
    transaction: &SafeDir,
    journal: &Journal,
    lease: &ParentLease,
) -> Result<(), CliError> {
    let name = parent_marker_name(&parent.identity);
    let transaction_identity = transaction.inspect_regular(&name)?;
    let parent_lease_identity = parent.inspect_regular(OsStr::new(PARENT_LEASE_NAME))?;
    let (Some(transaction_identity), Some(parent_lease_identity)) =
        (transaction_identity, parent_lease_identity)
    else {
        return Err(recovery_error("physical parent lease is missing"));
    };
    if transaction_identity != parent_lease_identity {
        return Err(recovery_error("physical parent lease identity mismatch"));
    }
    if lease.signature != JOURNAL_SIGNATURE
        || lease.version != JOURNAL_VERSION
        || lease.nonce != journal.nonce
        || lease.root != journal.root
        || lease.root_identity != journal.root_identity
        || lease.parent_identity != parent.identity
        || journal.parent_identities.binary_search(&parent.identity).is_err()
    {
        return Err(recovery_error("physical parent lease does not match journal"));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_parent_leases(
    transaction: &SafeDir,
    targets: &[AuthenticatedTarget],
    journal: &Journal,
) -> Result<(), CliError> {
    let mut parents = BTreeMap::new();
    for target in targets {
        parents.entry(target.parent.identity.clone()).or_insert(&target.parent);
    }
    for parent in parents.into_values() {
        let lease = load_parent_lease(parent)?
            .ok_or_else(|| recovery_error("physical parent lease is missing"))?;
        validate_parent_lease(parent, transaction, journal, &lease)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_parent_leases(
    _transaction: &SafeDir,
    _targets: &[AuthenticatedTarget],
    _journal: &Journal,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn remove_parent_leases(
    parents: &[SafeDir],
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    for parent in parents {
        let marker = parent_marker_name(&parent.identity);
        let transaction_identity = transaction.inspect_regular(&marker)?;
        let parent_identity = parent.inspect_regular(OsStr::new(PARENT_LEASE_NAME))?;
        let Some(transaction_identity) = transaction_identity else {
            continue;
        };
        if parent_identity.as_ref() == Some(&transaction_identity) {
            let lease = load_parent_lease(parent)?
                .ok_or_else(|| recovery_error("physical parent lease disappeared"))?;
            validate_parent_lease(parent, transaction, journal, &lease)?;
            remove_regular_handle_if_present(parent, OsStr::new(PARENT_LEASE_NAME))?;
            parent.sync()?;
        }
        remove_regular_handle_if_present(transaction, &marker)?;
        transaction.sync()?;
    }
    Ok(())
}

#[cfg(unix)]
fn journal_parent_handles(root: &SafeDir, journal: &Journal) -> Result<Vec<SafeDir>, CliError> {
    let mut parents = BTreeMap::new();
    for entry in &journal.entries {
        let relative = decode_path(&entry.target)?;
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = root.open_descendant(parent_relative)?;
        parents.entry(parent.identity.clone()).or_insert(parent);
    }
    let identities = parents.keys().cloned().collect::<Vec<_>>();
    if identities != journal.parent_identities {
        return Err(recovery_error(
            "journal physical parent identities do not match its target paths",
        ));
    }
    Ok(parents.into_values().collect())
}

#[cfg(unix)]
fn authenticate_targets(
    root: &SafeDir,
    entries: &[JournalEntry],
) -> Result<Vec<AuthenticatedTarget>, CliError> {
    entries
        .iter()
        .map(|entry| {
            let relative = decode_path(&entry.target)?;
            let parent_relative = relative.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            let parent = root.open_descendant(&parent_relative)?;
            parent.verify_namespace()?;
            let name = relative
                .file_name()
                .ok_or_else(|| recovery_error("transaction target has no file name"))?
                .to_os_string();
            Ok(AuthenticatedTarget { parent, name })
        })
        .collect()
}

#[cfg(not(unix))]
fn authenticate_targets(
    _root: &SafeDir,
    _entries: &[JournalEntry],
) -> Result<Vec<AuthenticatedTarget>, CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn verify_target_handle_identity(
    target: &AuthenticatedTarget,
    expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    verify_name_identity(&target.parent, &target.name, expected)
}

#[cfg(not(unix))]
fn verify_target_handle_identity(
    _target: &AuthenticatedTarget,
    _expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn verify_name_identity(
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

#[cfg(not(unix))]
fn verify_name_identity(
    _directory: &SafeDir,
    _name: &OsStr,
    _expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn handle_rename(
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
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

#[cfg(not(unix))]
fn handle_rename(
    _from_directory: &SafeDir,
    _from: &OsStr,
    _to_directory: &SafeDir,
    _to: &OsStr,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn install_stage_no_replace_handle(
    staged_directory: &SafeDir,
    staged: &OsStr,
    target_directory: &SafeDir,
    target: &OsStr,
) -> Result<(), CliError> {
    validate_single_name(staged)?;
    validate_single_name(target)?;
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
    target_directory.sync()?;
    rustix::fs::unlinkat(&staged_directory.fd, staged, rustix::fs::AtFlags::empty())?;
    staged_directory.sync()
}

#[cfg(not(unix))]
fn install_stage_no_replace_handle(
    _staged_directory: &SafeDir,
    _staged: &OsStr,
    _target_directory: &SafeDir,
    _target: &OsStr,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn verify_handle_content(
    target: &AuthenticatedTarget,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    verify_file_content(
        target.parent.open_regular(&target.name)?,
        &target.parent.path.join(&target.name),
        entry,
    )
}

#[cfg(not(unix))]
fn verify_handle_content(
    _target: &AuthenticatedTarget,
    _entry: &JournalEntry,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

fn verify_file_content(
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
fn file_identity(file: &File) -> Result<FileIdentity, CliError> {
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
fn file_identity(file: &File) -> Result<FileIdentity, CliError> {
    let information = winapi_util::file::information(file)?;
    Ok(FileIdentity {
        platform: "windows".into(),
        first: information.volume_serial_number(),
        second: information.file_index(),
        size: information.file_size(),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File) -> Result<FileIdentity, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "output file identity is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn read_limited_regular_handle(
    directory: &SafeDir,
    name: &OsStr,
    limit: u64,
) -> io::Result<Vec<u8>> {
    let file = directory.open_regular(name).map_err(|error| io::Error::other(error.to_string()))?;
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

fn validate_relative_path(path: &Path) -> Result<(), CliError> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(recovery_error("transaction target is not a strict relative path"));
    }
    if path.components().any(
        |component| matches!(component, Component::Normal(name) if managed_nonce(name).is_some()),
    ) {
        return Err(recovery_error("transaction target aliases a manager directory"));
    }
    Ok(())
}

#[cfg(unix)]
fn encode_path(path: &Path) -> Result<JournalPath, CliError> {
    use std::os::unix::ffi::OsStrExt as _;
    let units = path.as_os_str().as_bytes().iter().map(|byte| u32::from(*byte)).collect::<Vec<_>>();
    if units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("transaction path exceeds limit"));
    }
    Ok(JournalPath { encoding: "unixBytes".into(), units })
}

#[cfg(windows)]
fn encode_path(path: &Path) -> Result<JournalPath, CliError> {
    use std::os::windows::ffi::OsStrExt as _;
    let units = path.as_os_str().encode_wide().map(u32::from).collect::<Vec<_>>();
    if units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("transaction path exceeds limit"));
    }
    Ok(JournalPath { encoding: "windowsUtf16".into(), units })
}

#[cfg(not(any(unix, windows)))]
fn encode_path(_path: &Path) -> Result<JournalPath, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "journal paths are unavailable on this platform",
    ))
}

#[cfg(unix)]
fn decode_path(path: &JournalPath) -> Result<PathBuf, CliError> {
    use std::os::unix::ffi::OsStringExt as _;
    if path.encoding != "unixBytes" || path.units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("invalid Unix journal path encoding"));
    }
    let bytes = path
        .units
        .iter()
        .map(|unit| u8::try_from(*unit).map_err(|_| recovery_error("invalid Unix path byte")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
fn decode_path(path: &JournalPath) -> Result<PathBuf, CliError> {
    use std::os::windows::ffi::OsStringExt as _;
    if path.encoding != "windowsUtf16" || path.units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("invalid Windows journal path encoding"));
    }
    let units = path
        .units
        .iter()
        .map(|unit| u16::try_from(*unit).map_err(|_| recovery_error("invalid Windows path unit")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(not(any(unix, windows)))]
fn decode_path(_path: &JournalPath) -> Result<PathBuf, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "journal paths are unavailable on this platform",
    ))
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, CliError> {
    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "outputPathUnsupported",
                        format!("output path escapes its filesystem root: {}", path.display()),
                    ));
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    if !normalized.is_absolute() {
        return Err(CliError::new(
            ExitClass::Io,
            "outputPathUnsupported",
            format!("output path is not absolute after normalization: {}", path.display()),
        ));
    }
    Ok(normalized)
}

fn common_existing_ancestor(paths: &[PathBuf]) -> Result<PathBuf, CliError> {
    let first = paths.first().ok_or_else(|| CliError::internal("empty output transaction"))?;
    let mut candidate = first.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    while !paths.iter().all(|path| path.starts_with(&candidate)) || !candidate.exists() {
        candidate = candidate
            .parent()
            .ok_or_else(|| {
                CliError::new(
                    ExitClass::Io,
                    "outputPathUnsupported",
                    "no common existing output ancestor",
                )
            })?
            .to_path_buf();
    }
    // Keep the spelling through which the authenticated directory handle was
    // opened.  A case- or normalization-insensitive filesystem may expose the
    // same physical directory through a different lexical spelling; replacing
    // it with `canonicalize` would make otherwise valid target paths fail the
    // subsequent root-relative check.  Every mutation is still relative to the
    // no-follow handle opened below, and the durable lease records its physical
    // identity.
    Ok(candidate)
}

#[cfg(unix)]
fn transaction_registry(root: &SafeDir, create: bool) -> Result<Option<SafeDir>, CliError> {
    use std::os::unix::fs::PermissionsExt as _;

    match rustix::fs::openat(
        &root.fd,
        REGISTRY_NAME,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => {
            let identity = directory_identity(&fd)?;
            let directory = SafeDir { fd, path: root.path.join(REGISTRY_NAME), identity };
            let metadata = File::from(rustix::io::dup(&directory.fd)?).metadata()?;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(recovery_error("transaction registry is not private"));
            }
            Ok(Some(directory))
        }
        Err(rustix::io::Errno::NOENT) if !create => Ok(None),
        Err(rustix::io::Errno::NOENT) => {
            rustix::fs::mkdirat(
                &root.fd,
                REGISTRY_NAME,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
            )?;
            root.sync()?;
            Ok(Some(root.open_child(OsStr::new(REGISTRY_NAME))?))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn ensure_same_filesystem(root: &Path, targets: &[PathBuf]) -> Result<(), CliError> {
    use std::os::unix::fs::MetadataExt as _;
    let device = fs::metadata(root)?.dev();
    for target in targets {
        let mut ancestor = target.parent().unwrap_or_else(|| Path::new("."));
        while !ancestor.exists() {
            ancestor =
                ancestor.parent().ok_or_else(|| recovery_error("no existing output ancestor"))?;
        }
        if fs::metadata(ancestor)?.dev() != device {
            return Err(CliError::new(
                ExitClass::Io,
                "crossFilesystemTransaction",
                "output set crosses filesystems",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_same_filesystem(root: &Path, targets: &[PathBuf]) -> Result<(), CliError> {
    let root_file = securely_open_regular_or_directory(root)?;
    let root_volume = winapi_util::file::information(&root_file)?.volume_serial_number();
    for target in targets {
        let mut ancestor = target.parent().unwrap_or_else(|| Path::new("."));
        while !ancestor.exists() {
            ancestor =
                ancestor.parent().ok_or_else(|| recovery_error("no existing output ancestor"))?;
        }
        let handle = securely_open_regular_or_directory(ancestor)?;
        if winapi_util::file::information(&handle)?.volume_serial_number() != root_volume {
            return Err(CliError::new(
                ExitClass::Io,
                "crossFilesystemTransaction",
                "output set crosses volumes",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn securely_open_regular_or_directory(path: &Path) -> Result<File, CliError> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(CliError::new(
            ExitClass::Io,
            "symlinkDenied",
            format!("reparse point denied: {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn ensure_same_filesystem(_root: &Path, _targets: &[PathBuf]) -> Result<(), CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "safe output transactions are unavailable",
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn recovery_error(detail: impl Into<String>) -> CliError {
    CliError::new(ExitClass::Io, "transactionRecoveryFailed", detail)
}

fn recovery_failed(operation: &str, error: &CliError) -> CliError {
    CliError::new(
        ExitClass::Io,
        "transactionRecoveryFailed",
        format!("{operation}: {}: {}", error.code(), error.message()),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use into_markdown::{ExecutionOptions, ResourceLimits};
    use std::sync::Arc;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn manager_directories(root: &Path) -> Vec<PathBuf> {
        let registry = root.join(REGISTRY_NAME);
        let Ok(entries) = fs::read_dir(registry) else { return Vec::new() };
        entries
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name();
                managed_nonce(&name).map(|_| entry.path())
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn config_replace_is_fd_relative_durable_and_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let target = root.join("config.toml");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        atomic_replace_config(&target, b"new", true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o640);

        let created = root.join("created.toml");
        atomic_replace_config(&created, b"created", false).unwrap();
        assert_eq!(fs::metadata(&created).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn config_replace_rejects_target_and_temporary_identity_races() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let target = root.join("config.toml");
        fs::write(&target, b"old").unwrap();
        let held = root.join("held.toml");
        let error = atomic_replace_config_inner(&target, b"new", true, |_, _, _| {
            fs::rename(&target, &held)?;
            fs::write(&target, b"racer")?;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(&target).unwrap(), b"racer");

        fs::remove_file(&target).unwrap();
        fs::rename(&held, &target).unwrap();
        let error =
            atomic_replace_config_inner(&target, b"new", true, |parent, _, temporary_name| {
                let path = parent.path.join(temporary_name);
                fs::remove_file(&path)?;
                fs::write(path, b"attacker temporary")?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn config_publish_atomic_primitive_closes_post_check_destination_races() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();

        let absent = root.join("absent.toml");
        let error = atomic_replace_config_inner_with_barriers(
            &absent,
            b"new",
            false,
            |_, _, _| Ok(()),
            |_, _, _| {
                fs::write(&absent, b"racer")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "io");
        assert_eq!(fs::read(&absent).unwrap(), b"racer");

        let target = root.join("existing.toml");
        let held = root.join("original-held.toml");
        fs::write(&target, b"old").unwrap();
        let error = atomic_replace_config_inner_with_barriers(
            &target,
            b"new",
            true,
            |_, _, _| Ok(()),
            |_, _, _| {
                fs::rename(&target, &held)?;
                fs::write(&target, b"racer")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(&target).unwrap(), b"racer");
        assert_eq!(fs::read(&held).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn config_publish_reauthenticates_parent_and_temporary_after_final_check() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let parent = root.join("config");
        let held = root.join("config-held");
        fs::create_dir(&parent).unwrap();
        let target = parent.join("settings.toml");
        fs::write(&target, b"old").unwrap();
        let error = atomic_replace_config_inner_with_barriers(
            &target,
            b"new",
            true,
            |_, _, _| Ok(()),
            |_, _, _| {
                fs::rename(&parent, &held)?;
                fs::create_dir(&parent)?;
                fs::write(parent.join("settings.toml"), b"attacker")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(parent.join("settings.toml")).unwrap(), b"attacker");
        assert_eq!(fs::read(held.join("settings.toml")).unwrap(), b"old");

        let target = held.join("settings.toml");
        let attacker_temporary = Arc::new(Mutex::new(None::<PathBuf>));
        let captured = Arc::clone(&attacker_temporary);
        let error = atomic_replace_config_inner_with_barriers(
            &target,
            b"new",
            true,
            |_, _, _| Ok(()),
            move |directory, _, temporary_name| {
                let path = directory.path.join(temporary_name);
                fs::remove_file(&path)?;
                fs::write(&path, b"attacker temporary")?;
                *captured.lock().unwrap() = Some(path);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(&target).unwrap(), b"old");
        let attacker_temporary = attacker_temporary.lock().unwrap().clone().unwrap();
        assert_eq!(fs::read(attacker_temporary).unwrap(), b"attacker temporary");
    }

    #[cfg(unix)]
    #[test]
    fn config_replace_rejects_parent_swap_and_symlink_paths() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let parent = root.join("config");
        let held = root.join("config-held");
        fs::create_dir(&parent).unwrap();
        let target = parent.join("settings.toml");
        fs::write(&target, b"old").unwrap();
        let error = atomic_replace_config_inner(&target, b"new", true, |_, _, _| {
            fs::rename(&parent, &held)?;
            fs::create_dir(&parent)?;
            fs::write(parent.join("settings.toml"), b"attacker")?;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(parent.join("settings.toml")).unwrap(), b"attacker");

        let destination = root.join("destination.toml");
        fs::write(&destination, b"keep").unwrap();
        let link = root.join("link.toml");
        symlink(&destination, &link).unwrap();
        assert!(atomic_replace_config(&link, b"new", true).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"keep");

        let real_parent = root.join("real-parent");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = root.join("linked-parent");
        symlink(&real_parent, &linked_parent).unwrap();
        assert!(atomic_replace_config(&linked_parent.join("new.toml"), b"new", false).is_err());
        assert!(!real_parent.join("new.toml").exists());
    }

    #[test]
    fn every_durable_phase_is_recoverable_by_a_new_manager() {
        let phases = [
            "journalCreated",
            "stageAllocated",
            "stageWritten",
            "stageSynced",
            "prepared",
            "committing",
            "backupRenamed",
            "backupJournaled",
            "targetInstalled",
            "installJournaled",
            "committed",
        ];
        for phase in phases {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let first = root.join("one.md");
            let second = root.join("two.bin");
            fs::write(&first, b"old-one").unwrap();
            fs::write(&second, b"old-two").unwrap();
            let targets = [
                Target { path: first.clone(), bytes: b"new-one" },
                Target { path: second.clone(), bytes: b"new-two" },
            ];
            let mut fired = false;
            let result = prepare_with_hook(&targets, true, &context(), |seen, _| {
                if !fired && seen == phase {
                    fired = true;
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            });
            let result = match result {
                Ok(mut transaction) => transaction.commit_with_hook(|seen, _| {
                    if !fired && seen == phase {
                        fired = true;
                        Ok(HookDecision::SimulateCrash)
                    } else {
                        Ok(HookDecision::Continue)
                    }
                }),
                Err(error) => Err(error),
            };
            assert_eq!(result.unwrap_err().code(), "simulatedCrash", "{phase}");
            recover_pending(&root).unwrap();
            let values = (fs::read(&first).unwrap(), fs::read(&second).unwrap());
            let expected = if phase == "committed" {
                (b"new-one".to_vec(), b"new-two".to_vec())
            } else {
                (b"old-one".to_vec(), b"old-two".to_vec())
            };
            assert_eq!(values, expected, "wrong recovered set after {phase}");
            assert!(manager_directories(&root).is_empty());
        }
    }

    #[test]
    fn stage_failure_fsync_failure_budget_and_cancellation_leave_old_set() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("one.md");
        let second = root.join("two.bin");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
        ];

        for (phase, index, code) in [
            ("beforeStage", 1, "injectedStageFailure"),
            ("beforeStageSync", 0, "injectedFsyncFailure"),
        ] {
            let error = prepare_with_hook(&targets, true, &context(), |seen, seen_index| {
                if seen == phase && seen_index == index {
                    Err(CliError::new(ExitClass::Io, code, "injected"))
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .err()
            .expect("injected prepare failure");
            assert_eq!(error.code(), code);
            assert_eq!(fs::read(&first).unwrap(), b"old-one");
            assert_eq!(fs::read(&second).unwrap(), b"old-two");
            assert!(manager_directories(&root).is_empty());
        }

        let limited = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_temporary_bytes: 4, ..ResourceLimits::default() },
        );
        let error = prepare(&targets, true, &limited).err().expect("temporary budget failure");
        assert_eq!(error.code(), "resourceLimit");
        assert!(manager_directories(&root).is_empty());

        let token = into_markdown::CancellationToken::new();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation: token.clone(), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let transaction = prepare(&targets, true, &cancelled).unwrap();
        token.cancel();
        let error = transaction.commit().unwrap_err();
        assert_eq!(error.code(), "cancelled");
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert_eq!(fs::read(&second).unwrap(), b"old-two");
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn overlapping_cross_directory_transaction_is_recovered_before_any_new_write() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(a.join("child")).unwrap();
        fs::create_dir_all(&b).unwrap();
        let first = a.join("child/one.md");
        let second = b.join("two.bin");
        let parent_level = a.join("parent.txt");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        fs::write(&parent_level, b"old-parent").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
            Target { path: parent_level.clone(), bytes: b"new-parent" },
        ];

        for requested in [&first, &second, &parent_level] {
            fs::write(&first, b"old-one").unwrap();
            fs::write(&second, b"old-two").unwrap();
            fs::write(&parent_level, b"old-parent").unwrap();
            let mut transaction = prepare(&targets, true, &context()).unwrap();
            let error = transaction
                .commit_with_hook(|phase, index| {
                    if phase == "targetInstalled" && index == 0 {
                        Ok(HookDecision::SimulateCrash)
                    } else {
                        Ok(HookDecision::Continue)
                    }
                })
                .unwrap_err();
            assert_eq!(error.code(), "simulatedCrash");
            drop(transaction);

            let third = [Target { path: requested.clone(), bytes: b"third" }];
            prepare(&third, true, &context()).unwrap().commit().unwrap();
            assert_eq!(fs::read(requested).unwrap(), b"third");
            for untouched in [&first, &second, &parent_level] {
                if untouched != requested {
                    let expected = if untouched == &first {
                        b"old-one".as_slice()
                    } else if untouched == &second {
                        b"old-two".as_slice()
                    } else {
                        b"old-parent".as_slice()
                    };
                    assert_eq!(fs::read(untouched).unwrap(), expected);
                }
            }
            assert!(manager_directories(&root).is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn physical_parent_lease_serializes_an_absent_different_basename() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first_path = root.join("first.md");
        let second_path = root.join("second.md");
        fs::write(&first_path, b"old").unwrap();
        let first = [Target { path: first_path.clone(), bytes: b"interrupted" }];
        let mut transaction = prepare(&first, true, &context()).unwrap();

        let active = [Target { path: second_path.clone(), bytes: b"blocked" }];
        let error = prepare(&active, false, &context()).err().expect("physical parent is leased");
        assert_eq!(error.code(), "transactionBusy");

        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash");
        drop(transaction);

        let second = [Target { path: second_path.clone(), bytes: b"final" }];
        prepare(&second, false, &context()).unwrap().commit().unwrap();
        assert_eq!(fs::read(first_path).unwrap(), b"old");
        assert_eq!(fs::read(second_path).unwrap(), b"final");
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_alias_observes_the_same_physical_parent_lease() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let original = root.join("original.md");
        let alias = root.join("alias.md");
        fs::write(&original, b"old").unwrap();
        fs::hard_link(&original, &alias).unwrap();
        let first = [Target { path: original.clone(), bytes: b"first" }];
        let mut transaction = prepare(&first, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash");
        drop(transaction);

        let second = [Target { path: alias.clone(), bytes: b"second" }];
        prepare(&second, true, &context()).unwrap().commit().unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"old");
        assert_eq!(fs::read(&alias).unwrap(), b"second");
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn case_and_unicode_parent_aliases_share_the_physical_lease() {
        for (created_name, alias_name) in [("CaseParent", "caseparent"), ("é", "e\u{301}")] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let created = root.join(created_name);
            fs::create_dir(&created).unwrap();
            let alias = root.join(alias_name);
            let (Ok(created_handle), Ok(alias_handle)) =
                (SafeDir::open_absolute(&created), SafeDir::open_absolute(&alias))
            else {
                continue;
            };
            if created_handle.identity != alias_handle.identity {
                continue;
            }
            let first_path = created.join("first.md");
            let alias_path = alias.join("second.md");
            fs::write(&first_path, b"old").unwrap();
            let first = [Target { path: first_path.clone(), bytes: b"interrupted" }];
            let mut transaction = prepare(&first, true, &context()).unwrap();
            let error = transaction
                .commit_with_hook(|phase, index| {
                    if phase == "targetInstalled" && index == 0 {
                        Ok(HookDecision::SimulateCrash)
                    } else {
                        Ok(HookDecision::Continue)
                    }
                })
                .unwrap_err();
            assert_eq!(error.code(), "simulatedCrash");
            drop(transaction);

            let second = [Target { path: alias_path.clone(), bytes: b"final" }];
            prepare(&second, false, &context()).unwrap().commit().unwrap();
            assert_eq!(fs::read(first_path).unwrap(), b"old");
            assert_eq!(fs::read(alias_path).unwrap(), b"final");
            assert!(manager_directories(&created).is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn deep_parent_without_a_lease_never_uses_an_ancestor_scan_limit() {
        for depth in [130_usize, 500] {
            let temporary = tempfile::tempdir().unwrap();
            let mut parent = temporary.path().canonicalize().unwrap();
            let mut supported = true;
            for _ in 0..depth {
                parent.push("d");
                if let Err(error) = fs::create_dir(&parent) {
                    assert!(
                        matches!(error.raw_os_error(), Some(libc::ENAMETOOLONG | libc::EINVAL)),
                        "unexpected deep directory failure: {error}"
                    );
                    supported = false;
                    break;
                }
            }
            if !supported {
                continue;
            }
            let target = parent.join("document.md");
            let output = [Target { path: target.clone(), bytes: b"deep" }];
            prepare(&output, false, &context()).unwrap().commit().unwrap();
            assert_eq!(fs::read(target).unwrap(), b"deep");
        }
    }

    #[cfg(unix)]
    #[test]
    fn parent_swap_after_handle_authentication_never_writes_external_directory() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let parent = root.join("safe");
        let held = root.join("safe-held");
        let external = root.join("external");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&external).unwrap();
        let target = parent.join("document.md");
        fs::write(&target, b"old").unwrap();
        let targets = [Target { path: target.clone(), bytes: b"new" }];
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, _| {
                if phase == "afterTargetAuthentication" {
                    fs::rename(&parent, &held)?;
                    symlink(&external, &parent)?;
                }
                Ok(HookDecision::Continue)
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert!(!external.join("document.md").exists());
        assert_eq!(fs::read(held.join("document.md")).unwrap(), b"old");
        assert_eq!(manager_directories(&held).len(), 1);

        fs::remove_file(&parent).unwrap();
        fs::rename(&held, &parent).unwrap();
        recover_pending(&parent).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!external.join("document.md").exists());
        assert!(manager_directories(&root).is_empty());
    }

    #[test]
    fn rollback_failure_preserves_backup_and_a_later_recovery_completes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("one.md");
        let second = root.join("two.bin");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
        ];
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let directory = transaction.directory.clone();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    fs::remove_file(&first)?;
                    fs::create_dir(&first)?;
                    fs::write(first.join("blocker"), b"do-not-delete")?;
                    Err(CliError::new(ExitClass::Io, "injectedCommitFailure", "injected"))
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert!(error.message().contains("injectedCommitFailure"));
        assert!(error.message().contains("outputTargetTypeDenied"));
        assert_eq!(fs::read(directory.join("backup-0")).unwrap(), b"old-one");
        assert!(directory.join("journal-a.json").exists());
        assert_eq!(fs::read(&second).unwrap(), b"old-two");

        fs::remove_dir_all(&first).unwrap();
        recover_pending(&root).unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert_eq!(fs::read(&second).unwrap(), b"old-two");
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rollback_permission_failure_keeps_the_backup_recoverable() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output_parent = root.join("locked");
        fs::create_dir(&output_parent).unwrap();
        let output = output_parent.join("document.md");
        fs::write(&output, b"old").unwrap();
        let targets = [Target { path: output.clone(), bytes: b"new" }];
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let directory = transaction.directory.clone();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    fs::set_permissions(&output_parent, fs::Permissions::from_mode(0o500))?;
                    Err(CliError::new(ExitClass::Io, "injectedPermissionFailure", "injected"))
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        fs::set_permissions(&output_parent, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(error.code(), "rollbackFailed");
        assert!(error.message().contains("injectedPermissionFailure"));
        assert_eq!(fs::read(directory.join("backup-0")).unwrap(), b"old");

        recover_pending(&output_parent).unwrap();
        assert_eq!(fs::read(output).unwrap(), b"old");
        assert!(!directory.exists());
    }

    #[test]
    fn late_non_regular_target_is_rejected_before_the_first_output_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("one.md");
        let second = root.join("two.bin");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
        ];
        let held_second = root.join("held-two.bin");
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, _| {
                if phase == "committing" {
                    fs::rename(&second, &held_second)?;
                    fs::create_dir(&second)?;
                }
                Ok(HookDecision::Continue)
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert!(second.is_dir());
        assert_eq!(manager_directories(&root).len(), 1);

        fs::remove_dir(&second).unwrap();
        fs::rename(&held_second, &second).unwrap();
        recover_pending(&root).unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert_eq!(fs::read(&second).unwrap(), b"old-two");
    }

    #[test]
    fn absent_target_race_is_never_replaced_by_commit_or_rollback() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let target = root.join("document.md");
        let targets = [Target { path: target.clone(), bytes: b"new" }];
        let mut transaction = prepare(&targets, false, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "beforeTarget" && index == 0 {
                    fs::write(&target, b"racer")?;
                }
                Ok(HookDecision::Continue)
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert_eq!(fs::read(&target).unwrap(), b"racer");
        assert_eq!(manager_directories(&root).len(), 1);

        fs::remove_file(&target).unwrap();
        recover_pending(&root).unwrap();
        assert!(!target.exists());
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn directories_symlinks_fifos_and_devices_are_never_overwrite_targets() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let directory = root.join("directory");
        fs::create_dir(&directory).unwrap();
        let link = root.join("link");
        symlink(&directory, &link).unwrap();
        let fifo = root.join("fifo");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());

        for path in [directory, link, fifo, PathBuf::from("/dev/null")] {
            let target = [Target { path: path.clone(), bytes: b"new" }];
            let error = prepare(&target, true, &context()).err().expect("non-regular target");
            assert_eq!(error.code(), "outputTargetTypeDenied", "{}", path.display());
        }
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_swap_preserves_the_safe_old_primary_and_defers_unsafe_cleanup() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let primary = root.join("document.md");
        let asset_parent = root.join("assets");
        let held_parent = root.join("assets-held");
        let attacker = root.join("attacker");
        fs::write(&primary, b"old-document").unwrap();
        fs::create_dir(&asset_parent).unwrap();
        fs::create_dir(&attacker).unwrap();
        let asset = asset_parent.join("image.png");
        let targets = [
            Target { path: primary.clone(), bytes: b"new-document" },
            Target { path: asset.clone(), bytes: b"new-image" },
        ];
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "beforeTarget" && index == 1 {
                    fs::rename(&asset_parent, &held_parent)?;
                    symlink(&attacker, &asset_parent)?;
                }
                Ok(HookDecision::Continue)
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert_eq!(fs::read(&primary).unwrap(), b"old-document");
        assert!(!attacker.join("image.png").exists());
        assert_eq!(manager_directories(&root).len(), 1);

        fs::remove_file(&asset_parent).unwrap();
        fs::rename(&held_parent, &asset_parent).unwrap();
        if let Err(error) = recover_pending(&root) {
            panic!("recovery failed: {error}");
        }
        assert_eq!(fs::read(&primary).unwrap(), b"old-document");
        assert!(!asset.exists());
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn malformed_manager_directory_is_preserved_and_rejected() {
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let nonce = "0123456789abcdef0123456789abcdef";
        let registry = root.join(REGISTRY_NAME);
        fs::create_dir(&registry).unwrap();
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
        let managed = registry.join(format!("{TRANSACTION_PREFIX}{nonce}"));
        fs::create_dir(&managed).unwrap();
        fs::write(managed.join("journal-a.json"), b"not-json").unwrap();
        let error = recover_pending(&root).unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert!(managed.exists());
        let unrelated = root.join(".into-md-txn-01-not-managed");
        fs::create_dir(&unrelated).unwrap();
        assert!(unrelated.exists());
    }

    #[test]
    fn active_transaction_is_locked_and_unexpected_members_block_recovery() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output = root.join("document.md");
        fs::write(&output, b"old").unwrap();
        let targets = [Target { path: output.clone(), bytes: b"new" }];
        let transaction = prepare(&targets, true, &context()).unwrap();
        let directory = transaction.directory.clone();

        recover_pending(&root).unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"old");
        assert!(directory.exists());
        transaction.abort().unwrap();

        let transaction = prepare(&targets, true, &context()).unwrap();
        let directory = transaction.directory.clone();
        transaction.abandon_for_test();
        fs::write(directory.join("not-in-journal"), b"untrusted").unwrap();
        let error = recover_pending(&root).unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert!(directory.join("not-in-journal").exists());
        assert_eq!(fs::read(&output).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn manager_symlink_is_rejected_without_touching_its_destination() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let destination = root.join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), b"keep").unwrap();
        let registry = root.join(REGISTRY_NAME);
        fs::create_dir(&registry).unwrap();
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
        let manager =
            registry.join(format!("{TRANSACTION_PREFIX}{}", "0123456789abcdef0123456789abcdef"));
        symlink(&destination, &manager).unwrap();
        let error = recover_pending(&root).unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"keep");
        assert!(fs::symlink_metadata(manager).unwrap().file_type().is_symlink());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cross_filesystem_set_is_rejected_before_transaction_allocation() {
        use std::os::unix::fs::MetadataExt as _;

        if !Path::new("/dev/shm").is_dir()
            || fs::metadata("/dev/shm").unwrap().dev() == fs::metadata("/tmp").unwrap().dev()
        {
            return;
        }
        let first_root = tempfile::tempdir_in("/tmp").unwrap();
        let second_root = tempfile::tempdir_in("/dev/shm").unwrap();
        let first = first_root.path().join("document.md");
        let second = second_root.path().join("asset.bin");
        let targets = [
            Target { path: first.clone(), bytes: b"document" },
            Target { path: second.clone(), bytes: b"asset" },
        ];
        let error = prepare(&targets, true, &context()).err().expect("cross-filesystem rejection");
        assert_eq!(error.code(), "crossFilesystemTransaction");
        assert!(!first.exists());
        assert!(!second.exists());
    }
}
