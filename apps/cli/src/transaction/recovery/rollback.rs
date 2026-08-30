#[cfg(not(any(unix, windows)))]
use super::super::lease::transaction_platform_unavailable;
use super::super::{
    AuthenticatedTarget, CliError, ExecutionContext, ExitClass, File, FileIdentity, Journal,
    JournalEntry, JournalPath, JournalPhase, Path, SafeDir, TargetAuthenticator, backup_name,
    decode_path, fs, handle_rename, io, recovery_error, stage_name, transaction_registry,
    verify_file_content, verify_handle_content,
};
use super::cleanup::{remove_transaction_directory, validate_recovery_layout};

pub(in crate::transaction) fn rollback_transaction(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    validate_recovery_layout(root, directory, journal, context)?;
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
    rollback_transaction_streaming(&root_handle, &directory_handle, journal, context)?;
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    remove_transaction_directory(root, directory, journal, lock, context)?;
    remove_created_output_directories(root, journal)
}

#[cfg(any(unix, windows))]
fn rollback_transaction_streaming(
    root: &SafeDir,
    directory: &SafeDir,
    journal: &Journal,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let mut authenticator = TargetAuthenticator::new(root, context)?;
    let mut failures = Vec::new();
    for (index, entry) in journal.entries.iter().enumerate().rev() {
        context.checkpoint().map_err(CliError::from)?;
        let result = authenticator
            .authenticate(entry, &journal.parent_identities)
            .and_then(|target| rollback_entry_handle(directory, &target, journal, index, entry));
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

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn remove_created_output_directories(
    root: &Path,
    journal: &Journal,
) -> Result<(), CliError> {
    let root_handle = SafeDir::open_absolute(root)?;
    for created in journal.created_directories.iter().rev() {
        remove_created_output_directory(&root_handle, &created.path, Some(&created.identity))?;
    }
    // An intent without an authenticated post-create identity does not prove
    // ownership. A different process may have created the same empty directory
    // after the intent was synced, so recovery deliberately preserves it.
    Ok(())
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn remove_created_output_directory(
    root: &SafeDir,
    encoded: &JournalPath,
    expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    let relative = decode_path(encoded)?;
    match fs::symlink_metadata(root.path.join(&relative)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let Some(name) = relative.file_name() else {
        return Err(recovery_error("created output directory has no name"));
    };
    let parent = root.open_descendant(parent_relative)?;
    let Some(child) = parent.open_child_optional(name)? else {
        return Ok(());
    };
    if expected.is_some_and(|identity| &child.identity != identity) {
        return Err(recovery_error("created output directory identity changed"));
    }
    if child.is_empty()? {
        drop(child);
        parent.remove_empty_child(name)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn remove_created_output_directories(
    _root: &Path,
    _journal: &Journal,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
pub(super) fn rollback_entry_handle(
    directory: &SafeDir,
    target: &AuthenticatedTarget,
    journal: &Journal,
    index: usize,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    let backup = backup_name(index);
    let staged = stage_name(index);
    let backup_identity = directory.inspect_regular(&backup)?;
    let may_have_mutated_target = matches!(journal.phase, JournalPhase::Committing);
    let target_identity =
        if may_have_mutated_target { target.parent.inspect_regular(&target.name)? } else { None };
    let target_is_ours = target_identity
        .as_ref()
        .zip(entry.staged_identity.as_ref())
        .is_some_and(|(target, staged)| target == staged);
    if may_have_mutated_target && let Some(original) = &entry.original {
        if let Some(found) = &backup_identity {
            if found != original {
                return Err(recovery_error(format!(
                    "backup identity mismatch: {}",
                    directory.path.join(&backup).display()
                )));
            }
            if target_is_ours {
                verify_handle_content(target, entry)?;
                target.parent.remove_regular(&target.name)?;
                target.parent.sync()?;
            } else if target_identity.is_some() {
                return Err(recovery_error(format!(
                    "installed output identity changed while restoring backup: {}",
                    target.parent.path.join(&target.name).display()
                )));
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
    } else if may_have_mutated_target && target_is_ours {
        verify_handle_content(target, entry)?;
        target.parent.remove_regular(&target.name)?;
        target.parent.sync()?;
    }

    if let Some(staged_identity) = directory.inspect_regular(&staged)? {
        if entry.staged_identity.as_ref().is_some_and(|expected| expected != &staged_identity) {
            return Err(recovery_error("transaction stage identity changed during rollback"));
        }
        if journal.phase != JournalPhase::Staging {
            verify_file_content(
                directory.open_regular(&staged)?,
                &directory.path.join(&staged),
                entry,
            )?;
        }
        directory.remove_regular(&staged)?;
        directory.sync()?;
    }
    Ok(())
}

pub(in crate::transaction) fn finish_committed(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    validate_recovery_layout(root, directory, journal, context)?;
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
    let mut authenticator = TargetAuthenticator::new(&root_handle, context)?;
    #[cfg(any(unix, windows))]
    for (index, entry) in journal.entries.iter().enumerate() {
        context.checkpoint().map_err(CliError::from)?;
        let target = authenticator.authenticate(entry, &journal.parent_identities)?;
        verify_handle_content(&target, entry)?;
        let backup = backup_name(index);
        if let Some(identity) = directory_handle.inspect_regular(&backup)? {
            if entry.original.as_ref() != Some(&identity) {
                return Err(recovery_error(format!(
                    "committed backup identity mismatch: {}",
                    directory_handle.path.join(&backup).display()
                )));
            }
            directory_handle.remove_committed_backup(&backup, &identity)?;
        }
        let staged = stage_name(index);
        if directory_handle.inspect_regular(&staged)?.is_some() {
            verify_file_content(
                directory_handle.open_regular(&staged)?,
                &directory_handle.path.join(&staged),
                entry,
            )?;
            directory_handle.remove_regular(&staged)?;
        }
        directory_handle.sync()?;
    }
    #[cfg(any(unix, windows))]
    drop(authenticator);
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    remove_transaction_directory(root, directory, journal, lock, context)
}
