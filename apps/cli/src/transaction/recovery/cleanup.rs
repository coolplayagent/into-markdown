#[cfg(windows)]
use super::super::EXTERNAL_LOCK_PREFIX;
#[cfg(any(unix, windows))]
use super::super::lease::remove_journal_parent_leases;
#[cfg(not(any(unix, windows)))]
use super::super::lease::transaction_platform_unavailable;
use super::super::{
    CLEANUP_PREFIX, CliError, ExecutionContext, ExitClass, File, JOURNAL_LOG_NAME, Journal,
    JournalPhase, MAX_JOURNAL_ENTRIES, MAX_RECOVERY_DIRECTORY_ENTRIES, OsStr, OsString,
    PARENT_MARKER_PREFIX, Path, ResourceReservation, SafeDir, backup_name, fs, handle_rename,
    inspect_transaction_lease_member, managed_nonce, parent_marker_name, recovery_error,
    stage_name, transaction_registry, validate_journal,
};

fn allowed_parent_markers(
    journal: &Journal,
    context: &ExecutionContext,
) -> Result<(Vec<OsString>, ResourceReservation), CliError> {
    #[cfg(any(unix, windows))]
    {
        let marker_count = journal.parent_identities.len();
        let element_bytes = u64::try_from(std::mem::size_of::<OsString>()).unwrap_or(u64::MAX);
        let storage_bytes = u64::try_from(marker_count)
            .unwrap_or(u64::MAX)
            .checked_mul(element_bytes)
            .ok_or_else(|| recovery_error("parent-marker memory overflowed"))?;
        let marker_name_bytes = directory_name_retained_bytes(OsStr::new(
            "parent-0000000000000000000000000000000000000000000000000000000000000000.json",
        ));
        let retained_bytes = u64::try_from(marker_count)
            .unwrap_or(u64::MAX)
            .checked_mul(marker_name_bytes)
            .and_then(|bytes| bytes.checked_add(storage_bytes))
            .ok_or_else(|| recovery_error("parent-marker memory overflowed"))?;
        let mut memory = context.reserve_memory(retained_bytes).map_err(CliError::from)?;
        let mut markers = Vec::new();
        markers.try_reserve_exact(marker_count).map_err(|_| {
            CliError::new(ExitClass::Policy, "resourceLimit", "parent-marker allocation failed")
        })?;
        let capacity_growth = markers.capacity().saturating_sub(marker_count);
        if capacity_growth != 0 {
            let extra_bytes = u64::try_from(capacity_growth)
                .unwrap_or(u64::MAX)
                .checked_mul(element_bytes)
                .ok_or_else(|| recovery_error("parent-marker memory overflowed"))?;
            memory.grow(extra_bytes).map_err(CliError::from)?;
        }
        markers.extend(journal.parent_identities.iter().map(parent_marker_name));
        markers.sort_unstable();
        Ok((markers, memory))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = journal;
        Ok((Vec::new(), context.reserve_memory(0).map_err(CliError::from)?))
    }
}

#[cfg(any(unix, windows))]
fn directory_name_retained_bytes(name: &OsStr) -> u64 {
    u64::try_from(name.as_encoded_bytes().len())
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(64)
}

pub(super) fn validate_recovery_layout(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    validate_journal(root, directory, journal)?;
    let (allowed_parent_markers, _marker_memory) = allowed_parent_markers(journal, context)?;
    #[cfg(any(unix, windows))]
    let directory_handle = SafeDir::open_absolute(directory)?;
    #[cfg(any(unix, windows))]
    directory_handle.for_each_name_bounded(MAX_RECOVERY_DIRECTORY_ENTRIES, context, |name| {
        if !is_allowed_transaction_name(journal, &allowed_parent_markers, name) {
            return Err(recovery_error(format!(
                "unexpected transaction member: {}",
                directory.join(name).display()
            )));
        }
        if name.to_string_lossy().starts_with(PARENT_MARKER_PREFIX) {
            inspect_transaction_lease_member(&directory_handle, name)?
                .ok_or_else(|| recovery_error("transaction parent marker disappeared"))?;
        } else {
            let _ = directory_handle.open_regular(name)?;
        }
        Ok(())
    })?;
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

pub(super) fn remove_transaction_directory(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("transaction cleanup requires an owned lock"))?;
    #[cfg(any(unix, windows))]
    let root_handle = SafeDir::open_absolute(root)?;
    #[cfg(any(unix, windows))]
    let registry_handle = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("transaction registry is missing"))?;
    #[cfg(any(unix, windows))]
    let directory_handle = registry_handle.open_child(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )?;
    #[cfg(any(unix, windows))]
    for index in 0..journal.entries.len() {
        remove_regular_handle_if_present(&directory_handle, &stage_name(index))?;
        remove_regular_handle_if_present(&directory_handle, &backup_name(index))?;
    }
    if journal.phase == JournalPhase::Staging {
        directory_handle.for_each_name_bounded(
            MAX_RECOVERY_DIRECTORY_ENTRIES,
            context,
            |name| {
                if orphan_staging_index(journal, name).is_some() {
                    remove_regular_handle_if_present(&directory_handle, name)?;
                }
                Ok(())
            },
        )?;
    }
    #[cfg(any(unix, windows))]
    directory_handle.sync()?;

    // Atomically remove the directory from the recovery namespace while its
    // signed journals and exclusive lock still exist. Cleanup failures after
    // this point cannot cause a later recovery to reinterpret a completed set.
    let nonce = managed_nonce(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )
    .ok_or_else(|| recovery_error("transaction directory name is invalid"))?;
    let cleanup_name = OsString::from(format!("{CLEANUP_PREFIX}{nonce}"));
    #[cfg(windows)]
    let external_lock_name = OsString::from(format!("{EXTERNAL_LOCK_PREFIX}{nonce}"));
    #[cfg(any(unix, windows))]
    if registry_handle.open_child_optional(&cleanup_name)?.is_some() {
        return Err(recovery_error("transaction cleanup path already exists"));
    }
    #[cfg(windows)]
    {
        match directory_handle.inspect_regular(OsStr::new("transaction.lock")) {
            Ok(Some(_)) => {
                fs::hard_link(
                    directory_handle.path.join("transaction.lock"),
                    registry_handle.path.join(&external_lock_name),
                )?;
            }
            Ok(None) => {
                if registry_handle.inspect_regular(&external_lock_name)?.is_none() {
                    return Err(recovery_error("cleanup lock handoff disappeared"));
                }
            }
            Err(_) => {
                if directory_handle.inspect_lease_file(OsStr::new("transaction.lock"))?.is_none()
                    || registry_handle.inspect_lease_file(&external_lock_name)?.is_none()
                {
                    return Err(recovery_error("cleanup lock handoff identity mismatch"));
                }
            }
        }
        if directory_handle.inspect_lease_file(OsStr::new("transaction.lock"))?.is_some() {
            directory_handle.remove_lease_file(OsStr::new("transaction.lock"))?;
        }
        registry_handle.sync()?;
    }
    #[cfg(any(unix, windows))]
    handle_rename(
        &registry_handle,
        directory.file_name().expect("transaction name checked"),
        &registry_handle,
        &cleanup_name,
    )?;
    #[cfg(any(unix, windows))]
    registry_handle.sync()?;

    #[cfg(any(unix, windows))]
    let cleanup_handle = registry_handle.open_child(&cleanup_name)?;
    #[cfg(any(unix, windows))]
    remove_journal_parent_leases(&root_handle, &cleanup_handle, journal)?;
    drop(lock);
    #[cfg(windows)]
    registry_handle.remove_regular_private(&external_lock_name)?;
    #[cfg(any(unix, windows))]
    for name in ["journal-a.json", "journal-b.json", JOURNAL_LOG_NAME, "transaction.lock"] {
        remove_regular_handle_if_present(&cleanup_handle, OsStr::new(name))?;
    }
    #[cfg(any(unix, windows))]
    cleanup_handle.sync()?;
    #[cfg(any(unix, windows))]
    drop(cleanup_handle);
    #[cfg(any(unix, windows))]
    registry_handle.remove_empty_child(&cleanup_name)?;
    #[cfg(any(unix, windows))]
    registry_handle.sync()?;
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn remove_regular_handle_if_present(
    directory: &SafeDir,
    name: &OsStr,
) -> Result<(), CliError> {
    match directory.inspect_regular(name) {
        Ok(Some(_)) => {
            directory.remove_regular(name)?;
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

pub(super) fn is_allowed_transaction_name(
    journal: &Journal,
    parent_markers: &[OsString],
    name: &OsStr,
) -> bool {
    if matches!(
        name.to_str(),
        Some("journal-a.json" | "journal-b.json" | JOURNAL_LOG_NAME | "transaction.lock")
    ) {
        return true;
    }
    if parent_markers.binary_search_by(|marker| marker.as_os_str().cmp(name)).is_ok() {
        return true;
    }
    managed_numbered_member(name, "stage-", journal.entries.len())
        || managed_numbered_member(name, "backup-", journal.entries.len())
        || orphan_staging_index(journal, name).is_some()
}

pub(super) fn managed_numbered_member(name: &OsStr, prefix: &str, limit: usize) -> bool {
    let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(prefix)) else {
        return false;
    };
    if suffix.is_empty() || (suffix.len() > 1 && suffix.starts_with('0')) {
        return false;
    }
    suffix.parse::<usize>().is_ok_and(|index| index < limit)
}

pub(super) fn orphan_staging_index(journal: &Journal, name: &OsStr) -> Option<usize> {
    if journal.phase != JournalPhase::Staging {
        return None;
    }
    let suffix = name.to_str()?.strip_prefix("stage-")?;
    if suffix.is_empty() || (suffix.len() > 1 && suffix.starts_with('0')) {
        return None;
    }
    let index = suffix.parse::<usize>().ok()?;
    (index >= journal.entries.len() && index < MAX_JOURNAL_ENTRIES).then_some(index)
}
