use super::{
    Arc, CliError, Deserialize, Digest, ExecutionContext, File, OsString, Path, PathBuf,
    ResourceReservation, SafeDir, Serialize, TransactionSource, decode_path,
    validate_relative_path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum JournalPhase {
    Staging,
    Prepared,
    Committing,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum EntryState {
    Prepared,
    BackedUp,
    Installed,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct JournalPath {
    pub(super) encoding: String,
    pub(super) units: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileIdentity {
    pub(super) platform: String,
    pub(super) first: u64,
    pub(super) second: u64,
    pub(super) size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct JournalEntry {
    pub(super) target: JournalPath,
    #[serde(default)]
    pub(super) parent_index: Option<usize>,
    pub(super) original: Option<FileIdentity>,
    pub(super) content_sha256: String,
    pub(super) size: u64,
    #[serde(default)]
    pub(super) staged_identity: Option<FileIdentity>,
    pub(super) state: EntryState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreatedDirectory {
    pub(super) path: JournalPath,
    pub(super) identity: FileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StageResume {
    pub(super) config_fingerprint: String,
    pub(super) chunk_sequence: u64,
    pub(super) durable_len: u64,
    pub(super) content_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Journal {
    pub(super) signature: String,
    pub(super) version: u32,
    pub(super) nonce: String,
    pub(super) root: JournalPath,
    pub(super) root_identity: FileIdentity,
    pub(super) parent_identities: Vec<FileIdentity>,
    pub(super) generation: u64,
    #[serde(default)]
    pub(super) log_sequence: u64,
    pub(super) phase: JournalPhase,
    pub(super) entries: Vec<JournalEntry>,
    #[serde(default)]
    pub(super) created_directories: Vec<CreatedDirectory>,
    #[serde(default)]
    pub(super) pending_directories: Vec<JournalPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub(super) enum JournalRecord {
    DirectoryIntent { paths: Vec<JournalPath> },
    DirectoriesCreated { directories: Vec<CreatedDirectory> },
    TargetAdded { parent: Option<FileIdentity>, entry: JournalEntry },
    StageChunk { index: usize, resume: StageResume },
    StageSealed { index: usize, size: u64, content_sha256: String, staged_identity: FileIdentity },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ParentLease {
    pub(super) signature: String,
    pub(super) version: u32,
    pub(super) nonce: String,
    pub(super) root: JournalPath,
    pub(super) root_identity: FileIdentity,
    pub(super) parent_identity: FileIdentity,
}

pub enum HookDecision {
    Continue,
    #[cfg(test)]
    SimulateCrash,
    #[cfg(test)]
    SimulateRollbackFailure,
}

/// A fully staged transaction. Dropping it preserves the journal for recovery.
pub struct PreparedTransaction {
    pub(super) root: PathBuf,
    pub(super) directory: PathBuf,
    pub(super) journal: Journal,
    pub(super) context: ExecutionContext,
    pub(super) active: bool,
    pub(super) temporary_reservations: Vec<ResourceReservation>,
    pub(super) backup_reservations: Vec<ResourceReservation>,
    pub(super) journal_memory: ResourceReservation,
    pub(super) journal_temporary: ResourceReservation,
    pub(super) journal_slot_bytes: [u64; 2],
    pub(super) journal_log_bytes: u64,
    #[cfg(test)]
    pub(super) journal_persist_calls: u64,
    #[cfg(test)]
    pub(super) journal_record_calls: u64,
    #[cfg(test)]
    pub(super) journal_record_bytes: u64,
    #[cfg(test)]
    pub(super) journal_record_sync_calls: u64,
    #[cfg(test)]
    pub(super) simulate_rollback_failure: bool,
    pub(super) lock: Option<File>,
    pub(super) handles: TransactionHandles,
}

#[cfg(unix)]
pub(super) struct TransactionHandles {
    pub(super) root: SafeDir,
    pub(super) directory: SafeDir,
}

#[cfg(not(unix))]
pub(super) struct TransactionHandles {
    pub(super) root: SafeDir,
    pub(super) directory: SafeDir,
}

#[cfg(unix)]
pub(super) struct AuthenticatedTarget {
    pub(super) parent: Arc<SafeDir>,
    pub(super) name: OsString,
}

#[cfg(not(unix))]
pub(super) struct AuthenticatedTarget {
    pub(super) parent: Arc<SafeDir>,
    pub(super) name: OsString,
}

pub(super) fn target_path(root: &Path, entry: &JournalEntry) -> Result<PathBuf, CliError> {
    let relative = decode_path(&entry.target)?;
    validate_relative_path(&relative)?;
    Ok(root.join(relative))
}

pub(super) fn stage_name(index: usize) -> OsString {
    OsString::from(format!("stage-{index}"))
}

pub(super) fn backup_name(index: usize) -> OsString {
    OsString::from(format!("backup-{index}"))
}
