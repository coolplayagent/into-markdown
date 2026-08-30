#[cfg(unix)]
use super::identity::{directory_identity, verify_private_regular};
use super::{
    ACTIVE_TRANSACTIONS, BTreeMap, BTreeSet, CLEANUP_PREFIX, CliError, Digest, ExecutionContext,
    ExitClass, File, FileIdentity, INITIAL_PREFIX, MAX_RECOVERY_DIRECTORY_ENTRIES, Mutex,
    NONCE_COUNTER, Ordering, OsStr, OsString, PARENT_MARKER_PREFIX, PREPARING_TRANSACTION_ROOTS,
    Path, PathBuf, REGISTRY_NAME, Read, SafeDir, Sha256, SystemTime, TRANSACTION_PREFIX,
    TransactionSource, UNIX_EPOCH, file_identity, fs, hex_bytes, recovery_error,
    remove_regular_handle_if_present,
};
#[cfg(windows)]
use super::{
    EXTERNAL_LOCK_PREFIX, io, remove_external_lock_if_present, securely_open_regular_or_directory,
};

pub(super) const REGISTRY_LOCK_NAME: &str = "registry.lock";
const MAX_REGISTRY_EPOCH_RETRIES: u32 = 128;

pub(super) fn active_transactions() -> &'static Mutex<BTreeSet<PathBuf>> {
    ACTIVE_TRANSACTIONS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

pub(super) fn preparing_transaction_roots() -> &'static Mutex<BTreeMap<PathBuf, usize>> {
    PREPARING_TRANSACTION_ROOTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(super) struct PreparingTransactionRoot {
    root: PathBuf,
}

impl PreparingTransactionRoot {
    pub(super) fn enter(root: &Path) -> Result<Self, CliError> {
        let mut roots =
            preparing_transaction_roots().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = roots.entry(root.to_path_buf()).or_default();
        *count = count.checked_add(1).ok_or_else(|| {
            CliError::internal("output transaction preparing-root count overflowed")
        })?;
        Ok(Self { root: root.to_path_buf() })
    }
}

impl Drop for PreparingTransactionRoot {
    fn drop(&mut self) {
        let mut roots =
            preparing_transaction_roots().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove = {
            let count = roots
                .get_mut(&self.root)
                .expect("preparing transaction root guard must be registered");
            *count = count.checked_sub(1).expect("preparing transaction root count underflowed");
            *count == 0
        };
        if remove {
            roots.remove(&self.root);
        }
        drop(roots);
        if remove {
            try_cleanup_empty_registry(&self.root);
        }
    }
}

pub(super) fn managed_nonce(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let nonce = name.strip_prefix(TRANSACTION_PREFIX)?;
    (nonce.len() == 32
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then(|| nonce.to_owned())
}

#[cfg(unix)]
pub(super) fn create_initial_transaction_in_registry(
    root: &Path,
    registry: &SafeDir,
) -> Result<(String, PathBuf, PathBuf, File), CliError> {
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

#[cfg(windows)]
pub(super) fn create_initial_transaction_in_registry(
    root: &Path,
    registry: &SafeDir,
) -> Result<(String, PathBuf, PathBuf, File), CliError> {
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
        let initial_handle = match registry.create_child_private(&initial_name) {
            Ok(handle) => handle,
            Err(error) if error.message().contains("already exists") => continue,
            Err(error) => return Err(error),
        };
        // Keep the locked handle outside the directory during publication.
        // Windows otherwise denies renaming a directory which contains the
        // locked file, even when every directory handle shares deletion.
        let external_lock_name = OsString::from(format!("{EXTERNAL_LOCK_PREFIX}{nonce}"));
        let lock = match registry.create_regular_private(&external_lock_name) {
            Ok(lock) => lock,
            Err(error) => {
                let _ =
                    remove_initial_transaction_with_external_lock(registry, &initial_name, &nonce);
                return Err(error);
            }
        };
        if let Err(error) = lock.try_lock() {
            drop(lock);
            let _ = remove_initial_transaction_with_external_lock(registry, &initial_name, &nonce);
            return Err(lock_error("create transaction lock", &error));
        }
        if let Err(error) = fs::hard_link(
            registry.path.join(&external_lock_name),
            initial_handle.path.join("transaction.lock"),
        ) {
            drop(lock);
            let _ = remove_initial_transaction_with_external_lock(registry, &initial_name, &nonce);
            return Err(error.into());
        }
        let publication = (|| {
            if registry.inspect_lease_file(&external_lock_name)?.is_none()
                || initial_handle.inspect_lease_file(OsStr::new("transaction.lock"))?.is_none()
            {
                return Err(recovery_error("transaction lock link identity mismatch"));
            }
            lock.sync_all()?;
            initial_handle.sync()?;
            registry.sync()
        })();
        if let Err(error) = publication {
            drop(lock);
            let cleanup =
                remove_initial_transaction_with_external_lock(registry, &initial_name, &nonce);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "transaction lock publication failed ({}: {}); cleanup failed ({}: {})",
                        error.code(),
                        error.message(),
                        cleanup.code(),
                        cleanup.message()
                    ),
                )),
            };
        }
        return Ok((nonce, initial, directory, lock));
    }
    Err(CliError::new(
        ExitClass::Io,
        "transactionAllocationFailed",
        "could not allocate an output transaction directory",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn create_initial_transaction_in_registry(
    _root: &Path,
    _registry: &SafeDir,
) -> Result<(String, PathBuf, PathBuf, File), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
pub(super) fn remove_initial_transaction_handle(
    registry: &SafeDir,
    name: &OsStr,
) -> Result<(), CliError> {
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
    transaction.verify_private_namespace()?;
    let cleanup_context = ExecutionContext::new(
        into_markdown::ExecutionOptions::default(),
        into_markdown::ResourceLimits::default(),
    );
    transaction.for_each_name_bounded(
        MAX_RECOVERY_DIRECTORY_ENTRIES,
        &cleanup_context,
        |member| {
            let member = member
                .to_str()
                .ok_or_else(|| recovery_error("initial transaction member name is invalid"))?;
            if !matches!(member, "journal-a.json" | "journal-b.json" | "transaction.lock")
                && !member.starts_with(PARENT_MARKER_PREFIX)
            {
                return Err(recovery_error("unexpected initial transaction member"));
            }
            remove_regular_handle_if_present(&transaction, OsStr::new(member))
        },
    )?;
    transaction.verify_private_namespace()?;
    transaction.sync()?;
    registry.remove_empty_child(name)?;
    registry.sync()?;
    Ok(())
}

#[cfg(any(unix, windows))]
pub(super) fn remove_initial_transaction_with_external_lock(
    registry: &SafeDir,
    name: &OsStr,
    nonce: &str,
) -> Result<(), CliError> {
    #[cfg(windows)]
    remove_external_lock_if_present(registry, nonce)?;
    #[cfg(not(windows))]
    let _ = nonce;
    remove_initial_transaction_handle(registry, name)
}

#[cfg(any(unix, windows))]
pub(super) fn try_recovery_lock_handle(directory: &SafeDir) -> Result<Option<File>, CliError> {
    #[cfg(unix)]
    let lock = directory.open_regular(OsStr::new("transaction.lock"))?;
    #[cfg(windows)]
    let lock = match directory.open_regular(OsStr::new("transaction.lock")) {
        Ok(lock) => lock,
        Err(_) => {
            if let Ok(lock) = directory.open_lease_file(OsStr::new("transaction.lock")) {
                lock
            } else {
                let nonce = transaction_directory_nonce(
                    directory
                        .path
                        .file_name()
                        .ok_or_else(|| recovery_error("transaction directory has no name"))?,
                )
                .ok_or_else(|| recovery_error("transaction directory name is invalid"))?;
                let registry_path = directory
                    .path
                    .parent()
                    .ok_or_else(|| recovery_error("transaction registry path is invalid"))?;
                let registry = SafeDir::open_absolute(registry_path)?;
                registry.open_regular_private(&OsString::from(format!(
                    "{EXTERNAL_LOCK_PREFIX}{nonce}"
                )))?
            }
        }
    };
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
pub(super) fn transaction_directory_nonce(name: &OsStr) -> Option<&str> {
    let name = name.to_str()?;
    [TRANSACTION_PREFIX, INITIAL_PREFIX, CLEANUP_PREFIX]
        .into_iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .filter(|nonce| {
            nonce.len() == 32
                && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

#[cfg(windows)]
pub(super) fn verify_manager_directory(path: &Path) -> Result<(), CliError> {
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
pub(super) fn verify_manager_directory(_path: &Path) -> Result<(), CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "secure transaction directories are unavailable",
    ))
}

pub(super) fn lock_error(operation: &str, error: &std::fs::TryLockError) -> CliError {
    CliError::new(ExitClass::Io, "transactionLockFailed", format!("{operation}: {error}"))
}

/// Pins one registry generation while callers discover and lock transactions.
///
/// Lock order is registry epoch, then transaction locks in lexical transaction-name order.
/// Callers must release this guard before mutating a transaction and must release transaction
/// locks before attempting registry cleanup.
#[cfg(any(unix, windows))]
pub(super) struct RegistryEpochGuard {
    root: SafeDir,
    registry: SafeDir,
    lock: File,
}

#[cfg(any(unix, windows))]
impl RegistryEpochGuard {
    pub(super) fn root(&self) -> &SafeDir {
        &self.root
    }

    pub(super) fn registry(&self) -> &SafeDir {
        &self.registry
    }

    /// Releases this epoch without retaining an open directory handle and returns the
    /// authenticated registry identity needed to validate a later publication epoch.
    pub(super) fn release(self) -> Result<FileIdentity, CliError> {
        let Self { root, registry, lock } = self;
        let identity = registry.identity.clone();
        lock.unlock()?;
        drop(root);
        drop(registry);
        Ok(identity)
    }

    pub(super) fn matches_registry_identity(&self, expected: &FileIdentity) -> bool {
        &self.registry.identity == expected
    }

    /// Retires this registry generation when it has no transaction members.
    ///
    /// The lock name is unlinked while the locked handle remains alive. A waiter
    /// that already opened that handle must fail the identity check below and
    /// retry; a waiter that publishes a replacement lock makes the registry
    /// non-empty, so the final directory removal harmlessly loses the race.
    pub(super) fn try_cleanup(self) -> Result<bool, CliError> {
        let Self { root, registry, lock } = self;
        registry.verify_private_namespace()?;
        let cleanup_context = ExecutionContext::new(
            into_markdown::ExecutionOptions::default(),
            into_markdown::ResourceLimits::default(),
        );
        let mut empty = true;
        registry.for_each_name_bounded(
            MAX_RECOVERY_DIRECTORY_ENTRIES,
            &cleanup_context,
            |name| {
                if name != OsStr::new(REGISTRY_LOCK_NAME) {
                    empty = false;
                }
                Ok(())
            },
        )?;
        registry.verify_private_namespace()?;
        if !empty {
            return Ok(false);
        }
        let expected_lock_identity = file_identity(&lock)?;
        let current_lock = registry.open_regular_private(OsStr::new(REGISTRY_LOCK_NAME))?;
        if file_identity(&current_lock)? != expected_lock_identity {
            return Err(recovery_error(
                "transaction registry lock identity changed before cleanup",
            ));
        }
        drop(current_lock);
        registry.remove_regular_private(OsStr::new(REGISTRY_LOCK_NAME))?;
        registry.sync()?;
        lock.unlock()?;
        drop(lock);
        drop(registry);

        match root.remove_empty_child(OsStr::new(REGISTRY_NAME)) {
            Ok(()) => {
                root.sync()?;
                Ok(true)
            }
            Err(_) => {
                // A concurrent creator either retained the generation or
                // replaced it. In both cases it owns the cleanup obligation.
                Ok(transaction_registry(&root, false)?.is_none())
            }
        }
    }
}

#[cfg(any(unix, windows))]
pub(super) fn lock_registry_epoch(
    root: &SafeDir,
    create: bool,
) -> Result<Option<RegistryEpochGuard>, CliError> {
    lock_registry_epoch_inner(root, create, |_| {})
}

#[cfg(all(test, any(unix, windows)))]
pub(super) fn lock_registry_epoch_with_observer(
    root: &SafeDir,
    create: bool,
    observer: impl FnMut(&SafeDir),
) -> Result<Option<RegistryEpochGuard>, CliError> {
    lock_registry_epoch_inner(root, create, observer)
}

#[cfg(any(unix, windows))]
fn lock_registry_epoch_inner(
    root: &SafeDir,
    create: bool,
    mut before_lock: impl FnMut(&SafeDir),
) -> Result<Option<RegistryEpochGuard>, CliError> {
    for _ in 0..MAX_REGISTRY_EPOCH_RETRIES {
        let observed_root = SafeDir::open_absolute(&root.path)?;
        if observed_root.identity != root.identity {
            return Err(recovery_error("transaction root identity changed before registry lock"));
        }
        let Some(observed_registry) = transaction_registry(&observed_root, create)? else {
            return Ok(None);
        };
        let Ok(lock) = open_or_create_registry_lock(&observed_registry) else { continue };
        let observed_lock_identity = file_identity(&lock)?;
        before_lock(&observed_registry);
        lock.lock().map_err(|error| {
            CliError::new(
                ExitClass::Io,
                "transactionLockFailed",
                format!("lock transaction registry: {error}"),
            )
        })?;

        let current_root = SafeDir::open_absolute(&root.path)?;
        if current_root.identity != root.identity {
            return Err(recovery_error("transaction root identity changed after registry lock"));
        }
        let Some(current_registry) = transaction_registry(&current_root, false)? else {
            lock.unlock()?;
            continue;
        };
        if current_registry.identity != observed_registry.identity {
            lock.unlock()?;
            continue;
        }
        let Ok(current_lock) =
            current_registry.open_regular_private(OsStr::new(REGISTRY_LOCK_NAME))
        else {
            lock.unlock()?;
            continue;
        };
        if file_identity(&current_lock)? != observed_lock_identity {
            lock.unlock()?;
            continue;
        }
        drop(current_lock);
        return Ok(Some(RegistryEpochGuard {
            root: current_root,
            registry: current_registry,
            lock,
        }));
    }
    Err(CliError::new(
        ExitClass::Io,
        "transactionBusy",
        "transaction registry changed too many times while waiting for its lock",
    ))
}

#[cfg(unix)]
fn open_or_create_registry_lock(registry: &SafeDir) -> Result<File, CliError> {
    registry.verify_private_namespace()?;
    let name = OsStr::new(REGISTRY_LOCK_NAME);
    let flags =
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC;
    let (fd, created) = match rustix::fs::openat(
        &registry.fd,
        name,
        flags | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    ) {
        Ok(fd) => (fd, true),
        Err(rustix::io::Errno::EXIST) => {
            (rustix::fs::openat(&registry.fd, name, flags, rustix::fs::Mode::empty())?, false)
        }
        Err(error) => return Err(error.into()),
    };
    let file = File::from(fd);
    verify_private_regular(&file)?;
    registry.verify_private_namespace()?;
    if created {
        file.sync_all()?;
        registry.sync()?;
    }
    Ok(file)
}

#[cfg(windows)]
fn open_or_create_registry_lock(registry: &SafeDir) -> Result<File, CliError> {
    use cap_std::fs::OpenOptionsExt as _;

    registry.verify_private_namespace()?;
    let name = OsStr::new(REGISTRY_LOCK_NAME);
    let mut create_options = cap_std::fs::OpenOptions::new();
    create_options
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(0x0020_0000);
    let (file, created) = match registry.directory.open_with(name, &create_options) {
        Ok(file) => (file.into_std(), true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut open_options = cap_std::fs::OpenOptions::new();
            open_options
                .read(true)
                .write(true)
                .share_mode(0x1 | 0x2 | 0x4)
                .custom_flags(0x0020_0000);
            (registry.directory.open_with(name, &open_options)?.into_std(), false)
        }
        Err(error) => return Err(error.into()),
    };
    let expected = registry.open_regular_private(name)?;
    if file_identity(&file)? != file_identity(&expected)? {
        return Err(recovery_error("transaction registry lock identity changed during open"));
    }
    drop(expected);
    registry.verify_private_namespace()?;
    if created {
        file.sync_all()?;
        registry.sync()?;
    }
    Ok(file)
}

#[cfg(unix)]
pub(super) fn transaction_registry(
    root: &SafeDir,
    create: bool,
) -> Result<Option<SafeDir>, CliError> {
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
            directory.verify_private_namespace()?;
            Ok(Some(directory))
        }
        Err(rustix::io::Errno::NOENT) if !create => Ok(None),
        Err(rustix::io::Errno::NOENT) => {
            match rustix::fs::mkdirat(
                &root.fd,
                REGISTRY_NAME,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
            ) {
                Ok(()) => root.sync()?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
            let directory = root.open_child(OsStr::new(REGISTRY_NAME))?;
            directory.verify_private_namespace()?;
            Ok(Some(directory))
        }
        Err(error) => Err(error.into()),
    }
}

/// Remove an empty manager-owned registry under the cross-process epoch lock.
/// A non-empty generation survives for the active transaction or recovery pass.
#[cfg(any(unix, windows))]
pub(super) fn try_cleanup_empty_registry(root: &Path) {
    let Ok(root_handle) = SafeDir::open_absolute(root) else { return };
    let Ok(Some(epoch)) = lock_registry_epoch(&root_handle, false) else { return };
    let _ = epoch.try_cleanup();
}

#[cfg(not(any(unix, windows)))]
pub(super) fn try_cleanup_empty_registry(_root: &Path) {}

#[cfg(windows)]
pub(super) fn transaction_registry(
    root: &SafeDir,
    create: bool,
) -> Result<Option<SafeDir>, CliError> {
    match root.open_child_optional(OsStr::new(REGISTRY_NAME))? {
        Some(directory) => {
            directory.verify_private_namespace()?;
            Ok(Some(directory))
        }
        None if !create => Ok(None),
        None => {
            root.verify_namespace()?;
            let creation = into_markdown_process_plugin::create_windows_plugin_store_directory(
                &root.path.join(REGISTRY_NAME),
            )
            .map_err(|error| {
                recovery_error(format!(
                    "create private transaction registry ({}): {error}",
                    root.path.join(REGISTRY_NAME).display()
                ))
            });
            match creation {
                Ok(()) => root.sync()?,
                Err(error) => {
                    let Some(directory) = root.open_child_optional(OsStr::new(REGISTRY_NAME))?
                    else {
                        return Err(error);
                    };
                    directory.verify_private_namespace()?;
                    return Ok(Some(directory));
                }
            }
            let directory = root.open_child(OsStr::new(REGISTRY_NAME))?;
            directory.verify_private_namespace()?;
            Ok(Some(directory))
        }
    }
}
