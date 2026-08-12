//! Crash-recoverable output-set transactions.

use crate::error::{CliError, ExitClass};
use into_markdown::{ExecutionContext, TemporaryFile};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const JOURNAL_SIGNATURE: &str = "into-markdown-output-transaction";
const JOURNAL_VERSION: u32 = 1;
const TRANSACTION_PREFIX: &str = ".into-md-txn-01-";
const INITIAL_PREFIX: &str = ".into-md-init-01-";
const CLEANUP_PREFIX: &str = ".into-md-clean-01-";
const MAX_RECOVERY_TRANSACTIONS: usize = 128;
const MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 16_384;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JOURNAL_ENTRIES: usize = 100_001;
const MAX_PATH_UNITS: usize = 32_768;

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
static ACTIVE_TRANSACTIONS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalPath {
    encoding: String,
    units: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    generation: u64,
    phase: JournalPhase,
    entries: Vec<JournalEntry>,
}

/// One requested target and its complete staged contents.
pub struct Target<'a> {
    pub path: PathBuf,
    pub bytes: &'a [u8],
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
    staged_files: Vec<TemporaryFile>,
    lock: Option<File>,
}

impl PreparedTransaction {
    /// Commit every staged target, or recover the complete old set.
    pub fn commit(mut self) -> Result<Vec<PathBuf>, CliError> {
        self.commit_with_hook(|_, _| Ok(HookDecision::Continue))
    }

    /// Discard a transaction which has not begun committing.
    pub fn abort(mut self) -> Result<(), CliError> {
        self.staged_files.clear();
        let result = recover_transaction(&self.root, &self.directory, self.lock.take());
        self.deactivate();
        result
    }

    #[allow(clippy::too_many_lines)]
    fn commit_with_hook(
        &mut self,
        mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<Vec<PathBuf>, CliError> {
        self.journal.phase = JournalPhase::Committing;
        if let Err(error) = persist_journal(&self.directory, &mut self.journal) {
            return self.fail_and_recover(error);
        }
        if let Err(error) = crash_point(&mut hook, "committing", usize::MAX, self) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return self.fail_and_recover(error);
        }

        // Validate every destination immediately before the first output-set
        // mutation. This prevents a late directory/FIFO/link swap on a later
        // entry from producing an avoidable partially installed set.
        for index in 0..self.journal.entries.len() {
            if let Err(error) = self.context.checkpoint().map_err(CliError::from) {
                return self.fail_and_recover(error);
            }
            let target = target_path(&self.root, &self.journal.entries[index])?;
            if let Some(parent) = target.parent()
                && let Err(error) = reject_symlink_components(parent)
            {
                return self.fail_and_recover(error);
            }
            let expected = self.journal.entries[index].original.clone();
            if let Err(error) = verify_target_identity(&target, expected.as_ref()) {
                return self.fail_and_recover(error);
            }
        }
        self.preserve_staged_files();

        for index in 0..self.journal.entries.len() {
            if let Err(error) = self.context.checkpoint().map_err(CliError::from) {
                return self.fail_and_recover(error);
            }
            if let Err(error) = call_hook(&mut hook, "beforeTarget", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover(error);
            }
            let target = target_path(&self.root, &self.journal.entries[index])?;
            if let Some(parent) = target.parent()
                && let Err(error) = reject_symlink_components(parent)
            {
                return self.fail_and_recover(error);
            }

            let expected = self.journal.entries[index].original.clone();
            if let Err(error) = verify_target_identity(&target, expected.as_ref()) {
                return self.fail_and_recover(error);
            }
            if expected.is_some() {
                let backup = backup_path(&self.directory, index);
                if let Err(error) = fs::rename(&target, &backup).map_err(CliError::from) {
                    return self.fail_and_recover(error);
                }
                if let Err(error) = verify_target_identity(&backup, expected.as_ref()) {
                    return self.fail_and_recover(error);
                }
                if let Err(error) = sync_parent(&target) {
                    return self.fail_and_recover(error);
                }
                if let Err(error) = sync_directory(&self.directory) {
                    return self.fail_and_recover(error);
                }
                if let Err(error) = crash_point(&mut hook, "backupRenamed", index, self) {
                    if error.code() == "simulatedCrash" {
                        return Err(error);
                    }
                    return self.fail_and_recover(error);
                }
                self.journal.entries[index].state = EntryState::BackedUp;
                if let Err(error) = persist_journal(&self.directory, &mut self.journal) {
                    return self.fail_and_recover(error);
                }
                if let Err(error) = crash_point(&mut hook, "backupJournaled", index, self) {
                    if error.code() == "simulatedCrash" {
                        return Err(error);
                    }
                    return self.fail_and_recover(error);
                }
            }

            let staged = stage_path(&self.directory, index);
            if let Err(error) = install_stage_no_replace(&staged, &target) {
                return self.fail_and_recover(error);
            }
            if let Err(error) = sync_parent(&target) {
                return self.fail_and_recover(error);
            }
            if let Err(error) = sync_directory(&self.directory) {
                return self.fail_and_recover(error);
            }
            if let Err(error) = verify_content(&target, &self.journal.entries[index]) {
                return self.fail_and_recover(error);
            }
            if let Err(error) = crash_point(&mut hook, "targetInstalled", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover(error);
            }
            self.journal.entries[index].state = EntryState::Installed;
            if let Err(error) = persist_journal(&self.directory, &mut self.journal) {
                return self.fail_and_recover(error);
            }
            if let Err(error) = crash_point(&mut hook, "installJournaled", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover(error);
            }
        }

        self.journal.phase = JournalPhase::Committed;
        if let Err(error) = persist_journal(&self.directory, &mut self.journal) {
            return self.fail_and_recover(error);
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
        self.staged_files.clear();
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

    fn preserve_staged_files(&mut self) {
        for file in self.staged_files.drain(..) {
            let _ = file.persist();
        }
    }

    #[cfg(test)]
    fn abandon_for_test(mut self) {
        self.preserve_staged_files();
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
    prepare_with_hook(targets, overwrite, context, |_, _| Ok(HookDecision::Continue))
}

#[allow(clippy::too_many_lines)]
fn prepare_with_hook(
    targets: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
    mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<PreparedTransaction, CliError> {
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
    for path in &paths {
        if let Some(parent) = path.parent() {
            reject_symlink_components(parent)?;
            fs::create_dir_all(parent)?;
            reject_symlink_components(parent)?;
        }
    }
    let root = common_existing_ancestor(&paths)?;
    recover_pending(&root)?;
    reject_symlink_components(&root)?;
    ensure_same_filesystem(&root, &paths)?;

    let mut entries = Vec::with_capacity(targets.len());
    let mut seen = BTreeSet::new();
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
        let original = inspect_target(absolute)?;
        if original.is_some() && !overwrite {
            return Err(CliError::new(
                ExitClass::Io,
                "outputConflict",
                format!("output target already exists: {}", absolute.display()),
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
    let (nonce, initial_directory, directory, lock) = create_initial_transaction(&root)?;
    let mut journal = Journal {
        signature: JOURNAL_SIGNATURE.into(),
        version: JOURNAL_VERSION,
        nonce,
        root: encoded_root,
        generation: 0,
        phase: JournalPhase::Staging,
        entries,
    };
    if let Err(error) = persist_journal(&initial_directory, &mut journal) {
        drop(lock);
        let _ = remove_initial_transaction(&initial_directory);
        return Err(error);
    }
    if let Err(error) = fs::rename(&initial_directory, &directory).map_err(CliError::from) {
        drop(lock);
        let _ = remove_initial_transaction(&initial_directory);
        return Err(error);
    }
    if let Err(error) = sync_directory(&root) {
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
    let mut transaction = PreparedTransaction {
        root: root.clone(),
        directory: directory.clone(),
        journal,
        context: context.clone(),
        active: true,
        staged_files: Vec::with_capacity(targets.len()),
        lock: Some(lock),
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
        let path = stage_path(&directory, index);
        let file = match context.temporary_file_at(&path).map_err(CliError::from) {
            Ok(file) => file,
            Err(error) => return transaction.fail_and_recover(error),
        };
        transaction.staged_files.push(file);
        if let Err(error) = crash_point(&mut hook, "stageAllocated", index, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        if let Err(error) = transaction
            .staged_files
            .last_mut()
            .expect("stage file was just inserted")
            .write_all_checked(target.bytes)
            .map_err(CliError::from)
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
        if let Err(error) = transaction
            .staged_files
            .last_mut()
            .expect("stage file remains owned")
            .sync_all()
            .map_err(CliError::from)
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
    if let Err(error) = sync_directory(&directory) {
        return transaction.fail_and_recover(error);
    }
    transaction.journal.phase = JournalPhase::Prepared;
    if let Err(error) = persist_journal(&directory, &mut transaction.journal) {
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

/// Recover every exact manager transaction directory directly under `root`.
pub fn recover_pending(root: &Path) -> Result<(), CliError> {
    let root = root.canonicalize()?;
    let active =
        active_transactions().lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let mut managed = Vec::new();
    for (scanned, entry) in fs::read_dir(&root)?.enumerate() {
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
        let entry = entry?;
        let name = entry.file_name();
        let Some(nonce) = managed_nonce(&name) else { continue };
        let path = entry.path();
        if active.contains(&path) {
            continue;
        }
        if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
            return Err(CliError::new(
                ExitClass::Io,
                "transactionRecoveryFailed",
                format!("manager transaction path is not a real directory: {}", path.display()),
            ));
        }
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
        let Some(lock) = try_recovery_lock(&directory)
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

fn rollback_transaction(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
) -> Result<(), CliError> {
    validate_recovery_layout(root, directory, journal)?;
    let mut failures = Vec::new();
    for (index, entry) in journal.entries.iter().enumerate().rev() {
        if let Err(error) = rollback_entry(root, directory, journal, index, entry)
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
    remove_transaction_directory(directory, journal, lock)
}

fn rollback_entry(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    index: usize,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    let target = target_path(root, entry)?;
    if let Some(parent) = target.parent() {
        reject_symlink_components(parent)?;
    }
    let backup = backup_path(directory, index);
    let staged = stage_path(directory, index);
    let backup_identity = inspect_target(&backup)?;
    let target_identity = inspect_target(&target)?;

    if let Some(original) = &entry.original {
        if let Some(found) = &backup_identity {
            if found != original {
                return Err(recovery_error(format!(
                    "backup identity mismatch: {}",
                    backup.display()
                )));
            }
            if target_identity.is_some() {
                verify_content(&target, entry)?;
                fs::remove_file(&target)?;
                sync_parent(&target)?;
            }
            fs::rename(&backup, &target)?;
            sync_parent(&target)?;
            sync_directory(directory)?;
        } else {
            let Some(found) = target_identity else {
                return Err(recovery_error(format!(
                    "original and backup are both missing: {}",
                    target.display()
                )));
            };
            if &found != original {
                return Err(recovery_error(format!(
                    "original identity mismatch: {}",
                    target.display()
                )));
            }
        }
    } else if target_identity.is_some() {
        verify_content(&target, entry)?;
        fs::remove_file(&target)?;
        sync_parent(&target)?;
    }

    if inspect_target(&staged)?.is_some() {
        if journal.phase != JournalPhase::Staging {
            verify_content(&staged, entry)?;
        }
        fs::remove_file(&staged)?;
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
    for (index, entry) in journal.entries.iter().enumerate() {
        let target = target_path(root, entry)?;
        if let Some(parent) = target.parent() {
            reject_symlink_components(parent)?;
        }
        verify_content(&target, entry)?;
        let backup = backup_path(directory, index);
        if let Some(identity) = inspect_target(&backup)? {
            if entry.original.as_ref() != Some(&identity) {
                return Err(recovery_error(format!(
                    "committed backup identity mismatch: {}",
                    backup.display()
                )));
            }
            fs::remove_file(backup)?;
        }
        let staged = stage_path(directory, index);
        if inspect_target(&staged)?.is_some() {
            verify_content(&staged, entry)?;
            fs::remove_file(staged)?;
        }
    }
    remove_transaction_directory(directory, journal, lock)
}

fn validate_recovery_layout(
    root: &Path,
    directory: &Path,
    journal: &Journal,
) -> Result<(), CliError> {
    validate_journal(root, directory, journal)?;
    let allowed = allowed_transaction_names(journal.entries.len());
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            return Err(recovery_error(format!(
                "symlink inside transaction: {}",
                entry.path().display()
            )));
        }
        let name = entry.file_name();
        if !allowed.contains(&name) {
            return Err(recovery_error(format!(
                "unexpected transaction member: {}",
                entry.path().display()
            )));
        }
        if !entry.file_type()?.is_file() {
            return Err(recovery_error(format!(
                "non-file transaction member: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn remove_transaction_directory(
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("transaction cleanup requires an owned lock"))?;
    for index in 0..journal.entries.len() {
        remove_regular_if_present(&stage_path(directory, index))?;
        remove_regular_if_present(&backup_path(directory, index))?;
    }
    sync_directory(directory)?;

    // Atomically remove the directory from the recovery namespace while its
    // signed journals and exclusive lock still exist. Cleanup failures after
    // this point cannot cause a later recovery to reinterpret a completed set.
    let parent = directory.parent().ok_or_else(|| recovery_error("transaction has no parent"))?;
    let nonce = managed_nonce(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )
    .ok_or_else(|| recovery_error("transaction directory name is invalid"))?;
    let cleanup = parent.join(format!("{CLEANUP_PREFIX}{nonce}"));
    match fs::symlink_metadata(&cleanup) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Err(recovery_error("transaction cleanup path already exists")),
        Err(error) => return Err(error.into()),
    }
    fs::rename(directory, &cleanup)?;
    sync_directory(parent)?;
    drop(lock);

    for name in ["journal-a.json", "journal-b.json", "transaction.lock"] {
        remove_regular_if_present(&cleanup.join(name))?;
    }
    sync_directory(&cleanup)?;
    fs::remove_dir(&cleanup)?;
    sync_directory(parent)?;
    Ok(())
}

fn remove_regular_if_present(path: &Path) -> Result<(), CliError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(recovery_error(format!("unsafe transaction member: {}", path.display())))
        }
        Ok(_) => {
            let _ = securely_open_regular(path)?;
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn allowed_transaction_names(count: usize) -> BTreeSet<OsString> {
    let mut names = BTreeSet::from([
        OsString::from("journal-a.json"),
        OsString::from("journal-b.json"),
        OsString::from("transaction.lock"),
    ]);
    for index in 0..count {
        names.insert(OsString::from(format!("stage-{index}")));
        names.insert(OsString::from(format!("backup-{index}")));
    }
    names
}

fn persist_journal(directory: &Path, journal: &mut Journal) -> Result<(), CliError> {
    journal.generation = journal.generation.checked_add(1).ok_or_else(|| {
        CliError::new(ExitClass::Io, "transactionJournalOverflow", "journal generation overflow")
    })?;
    let name =
        if journal.generation.is_multiple_of(2) { "journal-b.json" } else { "journal-a.json" };
    let path = directory.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(recovery_error(format!("unsafe journal path: {}", path.display())));
        }
        Ok(_) => fs::remove_file(&path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
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
    let mut file = open_new_regular(&path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    sync_directory(directory)
}

fn load_journal(root: &Path, directory: &Path, nonce: &str) -> Result<Journal, CliError> {
    let mut candidates = Vec::new();
    for name in ["journal-a.json", "journal-b.json"] {
        let path = directory.join(name);
        match read_limited_regular(&path, MAX_JOURNAL_BYTES) {
            Ok(bytes) => {
                if let Ok(journal) = serde_json::from_slice::<Journal>(&bytes)
                    && validate_journal(root, directory, &journal).is_ok()
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
        recovery_error(format!("no valid signed journal in {}", directory.display()))
    })?;
    if candidates.last().is_some_and(|other| other.generation == journal.generation) {
        return Err(recovery_error("ambiguous journal generations"));
    }
    Ok(journal)
}

fn validate_journal(root: &Path, directory: &Path, journal: &Journal) -> Result<(), CliError> {
    if journal.signature != JOURNAL_SIGNATURE || journal.version != JOURNAL_VERSION {
        return Err(recovery_error("invalid transaction signature or version"));
    }
    let expected_name = format!("{TRANSACTION_PREFIX}{}", journal.nonce);
    if directory.file_name() != Some(OsStr::new(&expected_name))
        || managed_nonce(OsStr::new(&expected_name)).is_none()
    {
        return Err(recovery_error("transaction nonce does not match directory"));
    }
    if journal.entries.is_empty() || journal.entries.len() > MAX_JOURNAL_ENTRIES {
        return Err(recovery_error("transaction entry count is outside limits"));
    }
    let encoded_root = decode_path(&journal.root)?;
    if encoded_root != root.canonicalize()? {
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
    Ok(())
}

fn managed_nonce(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let nonce = name.strip_prefix(TRANSACTION_PREFIX)?;
    (nonce.len() == 32
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then(|| nonce.to_owned())
}

fn create_initial_transaction(root: &Path) -> Result<(String, PathBuf, PathBuf, File), CliError> {
    for attempt in 0_u32..128 {
        let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let digest =
            Sha256::digest(format!("{}:{time}:{counter}:{attempt}", std::process::id()).as_bytes());
        let nonce = hex_bytes(&digest[..16]);
        let initial = root.join(format!("{INITIAL_PREFIX}{nonce}"));
        let directory = root.join(format!("{TRANSACTION_PREFIX}{nonce}"));
        if fs::symlink_metadata(&directory).is_ok() {
            continue;
        }
        match create_private_directory(&initial) {
            Ok(()) => {
                let lock_path = initial.join("transaction.lock");
                let lock = match open_new_regular(&lock_path) {
                    Ok(lock) => lock,
                    Err(error) => {
                        let _ = fs::remove_dir(&initial);
                        return Err(error);
                    }
                };
                if let Err(error) = lock.try_lock() {
                    drop(lock);
                    let _ = fs::remove_file(lock_path);
                    let _ = fs::remove_dir(&initial);
                    return Err(lock_error("create transaction lock", &error));
                }
                if let Err(error) = lock
                    .sync_all()
                    .map_err(CliError::from)
                    .and_then(|()| sync_directory(&initial))
                    .and_then(|()| sync_directory(root))
                {
                    drop(lock);
                    let _ = remove_initial_transaction(&initial);
                    return Err(error);
                }
                return Ok((nonce, initial, directory, lock));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(CliError::new(
        ExitClass::Io,
        "transactionAllocationFailed",
        "could not allocate an output transaction directory",
    ))
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

fn remove_initial_transaction(directory: &Path) -> Result<(), CliError> {
    let name = directory
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| recovery_error("initial transaction name is invalid"))?;
    let nonce = name
        .strip_prefix(INITIAL_PREFIX)
        .filter(|nonce| {
            nonce.len() == 32
                && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .ok_or_else(|| recovery_error("initial transaction name is not manager-owned"))?;
    let expected = format!("{INITIAL_PREFIX}{nonce}");
    if directory.file_name() != Some(OsStr::new(&expected)) {
        return Err(recovery_error("initial transaction nonce mismatch"));
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !matches!(
            entry.file_name().to_str(),
            Some("journal-a.json" | "journal-b.json" | "transaction.lock")
        ) {
            return Err(recovery_error("unexpected initial transaction member"));
        }
        remove_regular_if_present(&entry.path())?;
    }
    fs::remove_dir(directory)?;
    if let Some(parent) = directory.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn try_recovery_lock(directory: &Path) -> Result<Option<File>, CliError> {
    let lock_path = directory.join("transaction.lock");
    let lock = securely_open_regular(&lock_path)?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(lock_error(
            &format!("lock transaction for recovery: {}", lock_path.display()),
            &error,
        )),
    }
}

#[cfg(unix)]
fn verify_manager_directory(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(recovery_error(format!(
            "transaction directory is not private: {}",
            path.display()
        )));
    }
    Ok(())
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

fn stage_path(directory: &Path, index: usize) -> PathBuf {
    directory.join(format!("stage-{index}"))
}

fn backup_path(directory: &Path, index: usize) -> PathBuf {
    directory.join(format!("backup-{index}"))
}

fn install_stage_no_replace(staged: &Path, target: &Path) -> Result<(), CliError> {
    fs::hard_link(staged, target).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                format!("output target appeared during commit: {}", target.display()),
            )
        } else {
            CliError::new(
                ExitClass::Io,
                "outputInstallFailed",
                format!(
                    "cannot install staged output without replacing a concurrent target: {}: {error}",
                    target.display()
                ),
            )
        }
    })?;
    sync_parent(target)?;
    fs::remove_file(staged)?;
    sync_parent(staged)
}

fn verify_content(path: &Path, entry: &JournalEntry) -> Result<(), CliError> {
    let mut file = securely_open_regular(path)?;
    let metadata = file.metadata()?;
    if metadata.len() != entry.size {
        return Err(recovery_error(format!(
            "transaction content size mismatch: {}",
            path.display()
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
            path.display()
        )));
    }
    Ok(())
}

fn inspect_target(path: &Path) -> Result<Option<FileIdentity>, CliError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(CliError::new(
                    ExitClass::Io,
                    "outputTargetTypeDenied",
                    format!("output target is not a regular non-link file: {}", path.display()),
                ));
            }
            let file = securely_open_regular(path)?;
            Ok(Some(file_identity(&file)?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn verify_target_identity(path: &Path, expected: Option<&FileIdentity>) -> Result<(), CliError> {
    let current = inspect_target(path)?;
    if current.as_ref() != expected {
        return Err(CliError::new(
            ExitClass::Io,
            "outputIdentityChanged",
            format!("output target changed after preflight: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn securely_open_regular(path: &Path) -> Result<File, CliError> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let expected = fs::symlink_metadata(path)?;
    if expected.file_type().is_symlink() || !expected.is_file() {
        return Err(CliError::new(
            ExitClass::Io,
            "outputTargetTypeDenied",
            format!("not a regular file: {}", path.display()),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || expected.dev() != opened.dev() || expected.ino() != opened.ino() {
        return Err(CliError::new(
            ExitClass::Io,
            "outputIdentityChanged",
            format!("file changed while opening: {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn securely_open_regular(path: &Path) -> Result<File, CliError> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let mut options = OpenOptions::new();
    options.read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let kind = winapi_util::file::typ(&file)?;
    let metadata = file.metadata()?;
    if !kind.is_disk()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.is_file()
    {
        return Err(CliError::new(
            ExitClass::Io,
            "outputTargetTypeDenied",
            format!("not a regular non-reparse disk file: {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn securely_open_regular(_path: &Path) -> Result<File, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "secure output target opening is unavailable on this platform",
    ))
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

fn open_new_regular(path: &Path) -> Result<File, CliError> {
    OpenOptions::new().write(true).create_new(true).open(path).map_err(Into::into)
}

fn read_limited_regular(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = securely_open_regular(path).map_err(|error| io::Error::other(error.to_string()))?;
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
    candidate.canonicalize().map_err(Into::into)
}

fn reject_symlink_components(path: &Path) -> Result<(), CliError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CliError::new(
                    ExitClass::Io,
                    "symlinkDenied",
                    format!("output path contains a symbolic link: {}", current.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
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

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(windows)]
fn sync_parent(path: &Path) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CliError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> Result<(), CliError> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options.open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(CliError::new(
            ExitClass::Io,
            "symlinkDenied",
            format!("directory sync target is unsafe: {}", path.display()),
        ));
    }
    directory.sync_all()?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_parent(_path: &Path) -> Result<(), CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "durable output directory sync is unavailable",
    ))
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_path: &Path) -> Result<(), CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "durable output directory sync is unavailable",
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

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown::{ExecutionOptions, ResourceLimits};

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn manager_directories(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                managed_nonce(&entry.file_name()).map(|_| entry.path())
            })
            .collect()
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
        recover_pending(&root).unwrap();
        assert_eq!(fs::read(&primary).unwrap(), b"old-document");
        assert!(!asset.exists());
        assert!(manager_directories(&root).is_empty());
    }

    #[test]
    fn malformed_manager_directory_is_preserved_and_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let nonce = "0123456789abcdef0123456789abcdef";
        let managed = root.join(format!("{TRANSACTION_PREFIX}{nonce}"));
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
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let destination = root.join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), b"keep").unwrap();
        let manager =
            root.join(format!("{TRANSACTION_PREFIX}{}", "0123456789abcdef0123456789abcdef"));
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
