//! Crash-recoverable output-set transactions.

#![cfg_attr(not(unix), allow(dead_code, unused_imports, unused_mut, unused_variables))]
#![cfg_attr(not(unix), allow(unreachable_code))]

use crate::error::{CliError, ExitClass};
use into_markdown::{ExecutionContext, ResourceReservation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::fd::OwnedFd;

const JOURNAL_SIGNATURE: &str = "into-markdown-output-transaction";
const JOURNAL_VERSION: u32 = 1;
const TRANSACTION_PREFIX: &str = ".into-md-txn-01-";
const INITIAL_PREFIX: &str = ".into-md-init-01-";
const CLEANUP_PREFIX: &str = ".into-md-clean-01-";
const EXTERNAL_LOCK_PREFIX: &str = ".into-md-lock-01-";
const PARENT_MARKER_PREFIX: &str = "parent-";
const PARENT_LEASE_NAME: &str = ".into-md-output-parent-lease-01";
const REGISTRY_NAME: &str = ".into-md-output-transactions-01";
const MAX_RECOVERY_TRANSACTIONS: usize = 128;
const MAX_RECOVERY_RETRIES: usize = 8;
const MAX_JOURNAL_ENTRIES: usize = 100_001;
const MAX_RECOVERY_DIRECTORY_ENTRIES: usize = MAX_JOURNAL_ENTRIES * 3 + 4_096;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PATH_UNITS: usize = 32_768;
const MAX_AUTHENTICATED_PARENT_HANDLES: usize = 4_096;

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
static ACTIVE_TRANSACTIONS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
static PREPARING_TRANSACTION_ROOTS: OnceLock<Mutex<BTreeMap<PathBuf, usize>>> = OnceLock::new();
static PARENT_LEASE_COORDINATORS: OnceLock<Vec<Mutex<()>>> = OnceLock::new();
const PARENT_LEASE_COORDINATOR_COUNT: usize = 64;

mod accounting;
mod commit;
mod config;
mod identity;
mod journal;
mod lease;
mod model;
mod recovery;
mod registry;
mod stage;
#[cfg(all(test, any(unix, windows)))]
mod tests;

#[cfg(all(test, windows))]
mod windows_config_tests {
    #[test]
    fn config_replace_crash_child() {
        super::config::config_replace_crash_child();
    }
}

use accounting::{
    DIRECTORY_ENTRY_TEMPORARY_BYTES, FILE_ENTRY_TEMPORARY_BYTES, PARENT_LEASE_TEMPORARY_BYTES,
    TRANSACTION_METADATA_TEMPORARY_BYTES, checked_usize_bytes, journal_entry_retained_bytes,
    journal_identity_retained_bytes, journal_path_retained_bytes, journal_retained_bytes,
    streaming_identity_index_bytes, streaming_index_capacity_plan, streaming_path_index_bytes,
    transaction_index_limit, verify_streaming_index_capacity,
};
#[cfg(windows)]
use identity::securely_open_regular_or_directory;
use identity::{
    TargetAuthenticator, decode_path, encode_path, ensure_same_filesystem, file_identity,
    handle_rename, install_stage_no_replace_handle, read_limited_regular_handle,
    validate_relative_path, validate_single_name, verify_file_content, verify_handle_content,
    verify_name_identity, verify_target_handle_identity,
};
use journal::{
    JOURNAL_LOG_NAME, load_journal, load_journal_handle, persist_journal_handle, validate_journal,
};
use lease::{
    ensure_transaction_platform, for_each_journal_parent, inspect_transaction_lease_member,
    load_parent_lease, parent_marker_name, remove_journal_parent_leases,
    validate_journal_parent_leases, validate_parent_lease,
};
use model::{
    AuthenticatedTarget, CreatedDirectory, EntryState, FileIdentity, Journal, JournalEntry,
    JournalPath, JournalPhase, JournalRecord, ParentLease, TransactionHandles, backup_name,
    stage_name, target_path,
};
#[cfg(windows)]
use recovery::remove_external_lock_if_present;
use recovery::{
    finish_committed, recover_parent_transactions, recover_root_transactions, recover_transaction,
    remove_created_output_directories, remove_regular_handle_if_present,
};
use registry::{
    PreparingTransactionRoot, REGISTRY_LOCK_DIRECTORY_NAME, active_transactions,
    create_initial_transaction_in_registry, lock_registry_epoch, managed_nonce,
    remove_initial_transaction_with_external_lock, transaction_registry,
    try_cleanup_empty_registry, try_recovery_lock_handle,
};
use stage::{TransactionSource, call_hook, crash_point};

#[cfg(test)]
use journal::JOURNAL_RECORD_MAGIC;
#[cfg(test)]
use recovery::recover_pending;
#[cfg(test)]
use stage::{StreamingTargetIndex, prepare_sources_with_hook_internal};

pub(crate) use config::{
    ConfigExpectedAuthority, atomic_replace_config_in_dir, recover_config_in_dir,
};
#[cfg(any(unix, windows))]
pub(crate) use identity::SafeDir;
pub use model::{HookDecision, PreparedTransaction};
#[cfg(test)]
pub(crate) use stage::prepare_with_hook;
pub use stage::{
    FileTarget, StreamingFileTransaction, Target, prepare, prepare_file_and_bytes, prepare_files,
    recover_for_paths,
};

#[cfg(unix)]
pub(crate) use config::atomic_replace_config;

fn sha256_hex(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn recovery_error(detail: impl Into<String>) -> CliError {
    CliError::new(ExitClass::Io, "transactionRecoveryUnsafe", detail.into())
}

fn recovery_failed(operation: &str, error: &CliError) -> CliError {
    CliError::new(
        ExitClass::Io,
        "transactionRecoveryFailed",
        format!("{operation}: {}: {}", error.code(), error.message()),
    )
}
