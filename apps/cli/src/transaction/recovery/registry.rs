#[cfg(windows)]
use super::super::EXTERNAL_LOCK_PREFIX;
#[cfg(any(unix, windows))]
use super::super::lease::remove_journal_parent_leases;
#[cfg(any(unix, windows))]
use super::super::registry::lock_registry_epoch;
use super::super::{
    AuthenticatedTarget, BTreeMap, CLEANUP_PREFIX, CliError, Digest, ExecutionContext, ExitClass,
    File, FileIdentity, INITIAL_PREFIX, JOURNAL_LOG_NAME, Journal, JournalEntry, JournalPath,
    JournalPhase, MAX_JOURNAL_ENTRIES, MAX_RECOVERY_DIRECTORY_ENTRIES, MAX_RECOVERY_TRANSACTIONS,
    OsStr, OsString, PARENT_MARKER_PREFIX, Path, PathBuf, REGISTRY_NAME, ResourceReservation,
    SafeDir, TRANSACTION_PREFIX, TargetAuthenticator, TransactionSource, active_transactions,
    backup_name, decode_path, fs, handle_rename, inspect_transaction_lease_member, io,
    load_journal, load_journal_handle, load_parent_lease, managed_nonce, parent_marker_name,
    recovery_error, recovery_failed, remove_initial_transaction_with_external_lock, stage_name,
    transaction_registry, try_cleanup_empty_registry, try_recovery_lock_handle, validate_journal,
    validate_parent_lease, verify_file_content, verify_handle_content,
};
use super::cleanup::{remove_regular_handle_if_present, validate_recovery_layout};
use super::rollback::{finish_committed, rollback_transaction};

pub(super) struct RecoveryReference {
    root: PathBuf,
    directory: PathBuf,
    directory_handle: SafeDir,
    initial: bool,
    lock: File,
}

/// Recover transactions named by per-transaction leases in the authenticated
/// physical target-parent directories. Recovery completes before this call
/// asks the caller to repeat preflight.
pub(in crate::transaction) fn recover_parent_transactions(
    parents: &[SafeDir],
    context: &ExecutionContext,
) -> Result<(), CliError> {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (parents, context);
        return Err(transaction_platform_unavailable());
    }
    #[cfg(any(unix, windows))]
    {
        recover_parent_groups(parents, context)
    }
}

#[cfg(any(unix, windows))]
type ParentRecoveryGroups = BTreeMap<(FileIdentity, PathBuf), BTreeMap<String, Vec<usize>>>;

#[cfg(any(unix, windows))]
fn group_parent_transactions(
    parents: &[SafeDir],
    context: &ExecutionContext,
) -> Result<ParentRecoveryGroups, CliError> {
    let mut groups: ParentRecoveryGroups = BTreeMap::new();
    for (index, parent) in parents.iter().enumerate() {
        context.checkpoint().map_err(CliError::from)?;
        let Some(lease) = load_parent_lease(parent)? else { continue };
        let root_path = decode_path(&lease.root)?;
        groups
            .entry((lease.root_identity, root_path))
            .or_default()
            .entry(lease.nonce)
            .or_default()
            .push(index);
    }
    let transaction_count = groups.values().try_fold(0_usize, |count, group| {
        count
            .checked_add(group.len())
            .ok_or_else(|| recovery_error("physical parent transaction count overflowed"))
    })?;
    if transaction_count > MAX_RECOVERY_TRANSACTIONS {
        return Err(CliError::new(
            ExitClass::Io,
            "transactionRecoveryLimit",
            "too many physical parent transactions require recovery",
        ));
    }
    Ok(groups)
}

#[cfg(any(unix, windows))]
fn recover_parent_groups(parents: &[SafeDir], context: &ExecutionContext) -> Result<(), CliError> {
    let groups = group_parent_transactions(parents, context)?;
    let mut recovered_any = false;
    for ((root_identity, root_path), transactions) in groups {
        let references =
            lock_parent_group(parents, &root_identity, &root_path, transactions, context)?;
        recovered_any |= !references.is_empty();
        for reference in references {
            recover_reference(reference, context).map_err(|error| {
                recovery_operation_error("recover physical parent transaction", error)
            })?;
        }
        try_cleanup_empty_registry(&root_path);
    }
    if recovered_any {
        return Err(CliError::new(
            ExitClass::Io,
            "transactionRecoveredRetry",
            "an interrupted transaction covering this target was recovered; retry the write",
        ));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn lock_parent_group(
    parents: &[SafeDir],
    root_identity: &FileIdentity,
    root_path: &Path,
    transactions: BTreeMap<String, Vec<usize>>,
    context: &ExecutionContext,
) -> Result<Vec<RecoveryReference>, CliError> {
    let root = SafeDir::open_absolute(root_path)
        .map_err(|error| recovery_failed("open leased transaction root", &error))?;
    if &root.identity != root_identity {
        return Err(recovery_error("leased transaction root identity changed"));
    }
    let Some(epoch) = lock_registry_epoch(&root, false)? else {
        return Err(recovery_error("leased transaction registry is missing"));
    };
    let mut candidates = Vec::with_capacity(transactions.len());
    for (nonce, parent_indexes) in transactions {
        match locate_parent_candidate(epoch.registry(), &nonce)? {
            Some((name, handle, initial)) => {
                candidates.push((name, nonce, parent_indexes, handle, initial));
            }
            None => {
                if !all_parent_leases_absent(parents, &parent_indexes)? {
                    return Err(recovery_error("leased transaction directory is missing"));
                }
            }
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    let mut references = Vec::with_capacity(candidates.len());
    for (name, nonce, parent_indexes, directory_handle, initial) in candidates {
        context.checkpoint().map_err(CliError::from)?;
        let Some(lock) = try_recovery_lock_handle(&directory_handle)
            .map_err(|error| recovery_failed("authenticate transaction lock", &error))?
        else {
            continue;
        };
        let directory = epoch.root().path.join(REGISTRY_NAME).join(&name);
        let journal =
            load_journal_handle(epoch.root(), &directory_handle, &directory, &nonce, context)?;
        let mut validated = 0_usize;
        for parent_index in parent_indexes {
            let parent = &parents[parent_index];
            let Some(lease) = load_parent_lease(parent)? else { continue };
            if lease.nonce != nonce
                || lease.root_identity != *root_identity
                || decode_path(&lease.root)? != root_path
            {
                return Err(recovery_error("physical parent lease changed during discovery"));
            }
            validate_parent_lease(parent, &directory_handle, &journal, &lease)?;
            validated += 1;
        }
        if validated != 0 {
            references.push(RecoveryReference {
                root: epoch.root().path.clone(),
                directory,
                directory_handle,
                initial,
                lock,
            });
        }
    }
    drop(epoch);
    Ok(references)
}

#[cfg(any(unix, windows))]
fn locate_parent_candidate(
    registry: &SafeDir,
    nonce: &str,
) -> Result<Option<(OsString, SafeDir, bool)>, CliError> {
    let names = [
        (OsString::from(format!("{TRANSACTION_PREFIX}{nonce}")), false),
        (OsString::from(format!("{INITIAL_PREFIX}{nonce}")), true),
        (OsString::from(format!("{CLEANUP_PREFIX}{nonce}")), false),
    ];
    let mut found = None;
    for (name, initial) in names {
        if let Some(handle) = registry.open_child_optional(&name)? {
            if found.is_some() {
                return Err(recovery_error(
                    "physical parent lease ambiguously names a transaction",
                ));
            }
            found = Some((name, handle, initial));
        }
    }
    Ok(found)
}

#[cfg(any(unix, windows))]
fn all_parent_leases_absent(
    parents: &[SafeDir],
    parent_indexes: &[usize],
) -> Result<bool, CliError> {
    for index in parent_indexes {
        if load_parent_lease(&parents[*index])?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(any(unix, windows))]
fn recover_reference(
    reference: RecoveryReference,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    if reference.initial {
        return recover_initial_transaction(
            &reference.root,
            &reference.directory,
            &reference.directory_handle,
            Some(reference.lock),
            context,
        );
    }
    if reference
        .directory
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with(CLEANUP_PREFIX))
    {
        return recover_cleanup_transaction(
            &reference.root,
            &reference.directory,
            &reference.directory_handle,
            Some(reference.lock),
            context,
        );
    }
    recover_transaction(&reference.root, &reference.directory, Some(reference.lock), context)
}

/// Recover every exact manager transaction directory directly under `root`.
#[cfg(all(test, any(unix, windows)))]
pub fn recover_pending(root: &Path) -> Result<(), CliError> {
    let context = ExecutionContext::new(Default::default(), Default::default());
    recover_root_transactions(root, &context)
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn recover_root_transactions(
    root: &Path,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let root = root.canonicalize()?;
    let root_handle = SafeDir::open_absolute(&root)?;
    let Some(epoch) = lock_registry_epoch(&root_handle, false)? else { return Ok(()) };
    let registry_path = root.join(REGISTRY_NAME);
    let mut recovery_names = Vec::new();
    let entry_bytes = u64::try_from(std::mem::size_of::<RootRecoveryName>()).unwrap_or(u64::MAX);
    let planned_storage_bytes = u64::try_from(MAX_RECOVERY_TRANSACTIONS)
        .unwrap_or(u64::MAX)
        .checked_mul(entry_bytes)
        .ok_or_else(|| recovery_error("recovery transaction-name memory overflowed"))?;
    let mut recovery_name_memory =
        context.reserve_memory(planned_storage_bytes).map_err(CliError::from)?;
    recovery_names.try_reserve_exact(MAX_RECOVERY_TRANSACTIONS).map_err(|_| {
        CliError::new(
            ExitClass::Policy,
            "resourceLimit",
            "recovery transaction-name allocation failed",
        )
    })?;
    if recovery_names.capacity() > MAX_RECOVERY_TRANSACTIONS {
        let extra_entries = recovery_names.capacity() - MAX_RECOVERY_TRANSACTIONS;
        let extra_bytes = u64::try_from(extra_entries)
            .unwrap_or(u64::MAX)
            .checked_mul(entry_bytes)
            .ok_or_else(|| recovery_error("recovery transaction-name memory overflowed"))?;
        recovery_name_memory.grow(extra_bytes).map_err(CliError::from)?;
    }
    epoch.registry().for_each_name_bounded(MAX_RECOVERY_DIRECTORY_ENTRIES, context, |name| {
        collect_recovery_name(
            &root,
            &registry_path,
            name,
            &mut recovery_names,
            &mut recovery_name_memory,
        )
    })?;
    recovery_names.sort_by(|left, right| left.name.cmp(&right.name));
    let mut references = Vec::new();
    for candidate in recovery_names {
        let path = root.join(REGISTRY_NAME).join(&candidate.name);
        let directory_handle = epoch
            .registry()
            .open_child(&candidate.name)
            .map_err(|error| recovery_failed("authenticate transaction directory", &error))?;
        let Some(lock) = try_recovery_lock_handle(&directory_handle)
            .map_err(|error| recovery_failed("authenticate transaction lock", &error))?
        else {
            continue;
        };
        references.push(RootRecoveryReference {
            directory: path,
            directory_handle,
            nonce: candidate.nonce,
            initial: candidate.initial,
            cleanup: candidate.cleanup,
            lock,
        });
    }
    drop(epoch);
    for reference in references {
        recover_root_reference(&root, reference, context)
            .map_err(|error| recovery_operation_error("recover root transaction", error))?;
    }
    try_cleanup_empty_registry(&root);
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn recover_root_transactions(
    _root: &Path,
    _context: &ExecutionContext,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
struct RootRecoveryReference {
    directory: PathBuf,
    directory_handle: SafeDir,
    nonce: String,
    initial: bool,
    cleanup: bool,
    lock: File,
}

#[cfg(any(unix, windows))]
struct RootRecoveryName {
    name: OsString,
    nonce: String,
    initial: bool,
    cleanup: bool,
}

#[cfg(any(unix, windows))]
fn collect_recovery_name(
    root: &Path,
    registry_path: &Path,
    name: &OsStr,
    names: &mut Vec<RootRecoveryName>,
    memory: &mut ResourceReservation,
) -> Result<(), CliError> {
    let Some((nonce, initial, cleanup)) = recovery_name(name) else { return Ok(()) };
    let active = active_transactions().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if active
        .iter()
        .any(|path| path.parent() == Some(registry_path) && path.file_name() == Some(name))
    {
        return Ok(());
    }
    drop(active);
    if names.len() >= MAX_RECOVERY_TRANSACTIONS {
        return Err(CliError::new(
            ExitClass::Io,
            "transactionRecoveryLimit",
            format!(
                "more than {MAX_RECOVERY_TRANSACTIONS} pending transactions under {}",
                root.display()
            ),
        ));
    }
    let variable_bytes = directory_name_retained_bytes(name)
        .checked_add(u64::try_from(nonce.len()).unwrap_or(u64::MAX).saturating_add(64))
        .ok_or_else(|| recovery_error("recovery transaction-name memory overflowed"))?;
    memory.grow(variable_bytes).map_err(CliError::from)?;
    names.push(RootRecoveryName {
        name: name.to_os_string(),
        nonce: nonce.to_owned(),
        initial,
        cleanup,
    });
    Ok(())
}

#[cfg(any(unix, windows))]
fn directory_name_retained_bytes(name: &OsStr) -> u64 {
    u64::try_from(name.as_encoded_bytes().len())
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(64)
}

#[cfg(any(unix, windows))]
fn recover_root_reference(
    root: &Path,
    reference: RootRecoveryReference,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    if reference.initial {
        return recover_initial_transaction(
            root,
            &reference.directory,
            &reference.directory_handle,
            Some(reference.lock),
            context,
        );
    }
    if reference.cleanup {
        return recover_cleanup_transaction(
            root,
            &reference.directory,
            &reference.directory_handle,
            Some(reference.lock),
            context,
        );
    }
    let journal = load_journal(root, &reference.directory, &reference.nonce, context)?;
    if journal.phase == JournalPhase::Committed {
        finish_committed(root, &reference.directory, &journal, Some(reference.lock), context)
    } else {
        rollback_transaction(root, &reference.directory, &journal, Some(reference.lock), context)
    }
}

fn recovery_operation_error(operation: &str, error: CliError) -> CliError {
    if matches!(error.code(), "resourceLimit" | "cancelled" | "timeout") {
        error
    } else {
        recovery_failed(operation, &error)
    }
}

#[cfg(any(unix, windows))]
fn recovery_name(name: &OsStr) -> Option<(&str, bool, bool)> {
    let text = name.to_str()?;
    let (nonce, initial, cleanup) = if let Some(nonce) = text.strip_prefix(TRANSACTION_PREFIX) {
        (nonce, false, false)
    } else if let Some(nonce) = text.strip_prefix(INITIAL_PREFIX) {
        (nonce, true, false)
    } else {
        (text.strip_prefix(CLEANUP_PREFIX)?, false, true)
    };
    (nonce.len() == 32
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some((nonce, initial, cleanup))
}

pub(in crate::transaction) fn recover_transaction(
    root: &Path,
    directory: &Path,
    lock: Option<File>,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let name = directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?;
    let nonce = managed_nonce(name).ok_or_else(|| recovery_error("invalid transaction name"))?;
    let journal = load_journal(root, directory, &nonce, context)?;
    if journal.phase == JournalPhase::Committed {
        finish_committed(root, directory, &journal, lock, context)
    } else {
        rollback_transaction(root, directory, &journal, lock, context)
    }
}

#[cfg(any(unix, windows))]
pub(super) fn recover_initial_transaction(
    root: &Path,
    directory: &Path,
    directory_handle: &SafeDir,
    lock: Option<File>,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("initial recovery requires an owned lock"))?;
    let nonce = directory
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(INITIAL_PREFIX))
        .ok_or_else(|| recovery_error("invalid initial transaction name"))?;
    let root_handle = SafeDir::open_absolute(root)?;
    let journal = load_journal_handle(&root_handle, directory_handle, directory, nonce, context)?;
    if journal.phase != JournalPhase::Staging {
        return Err(recovery_error("initial transaction has advanced beyond staging"));
    }
    validate_recovery_layout(root, directory, &journal, context)?;
    remove_journal_parent_leases(&root_handle, directory_handle, &journal)?;
    drop(lock);
    let registry = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("initial transaction registry is missing"))?;
    remove_initial_transaction_with_external_lock(
        &registry,
        directory.file_name().ok_or_else(|| recovery_error("initial transaction has no name"))?,
        &journal.nonce,
    )
}

#[cfg(any(unix, windows))]
pub(super) fn recover_cleanup_transaction(
    root: &Path,
    directory: &Path,
    directory_handle: &SafeDir,
    lock: Option<File>,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("cleanup recovery requires an owned lock"))?;
    let nonce = directory
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(CLEANUP_PREFIX))
        .ok_or_else(|| recovery_error("invalid cleanup transaction name"))?;
    let root_handle = SafeDir::open_absolute(root)?;
    let journal = load_journal_handle(&root_handle, directory_handle, directory, nonce, context)?;
    validate_recovery_layout(root, directory, &journal, context)?;
    remove_journal_parent_leases(&root_handle, directory_handle, &journal)?;
    drop(lock);
    let registry = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("cleanup transaction registry is missing"))?;
    #[cfg(windows)]
    remove_external_lock_if_present(&registry, nonce)?;
    for name in ["journal-a.json", "journal-b.json", JOURNAL_LOG_NAME, "transaction.lock"] {
        remove_regular_handle_if_present(directory_handle, OsStr::new(name))?;
    }
    directory_handle.sync()?;
    registry.remove_empty_child(
        directory.file_name().ok_or_else(|| recovery_error("cleanup has no name"))?,
    )?;
    registry.sync()
}

#[cfg(windows)]
pub(in crate::transaction) fn remove_external_lock_if_present(
    registry: &SafeDir,
    nonce: &str,
) -> Result<(), CliError> {
    let name = OsString::from(format!("{EXTERNAL_LOCK_PREFIX}{nonce}"));
    match registry.inspect_regular(&name) {
        Ok(Some(_)) => registry.remove_regular_private(&name),
        Ok(None) => Ok(()),
        Err(_) => {
            if registry.inspect_lease_file(&name)?.is_some() {
                registry.remove_lease_file(&name)
            } else {
                Ok(())
            }
        }
    }
}
