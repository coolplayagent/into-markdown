//! Bounded, synchronous `SQLite` persistence for local conversion tasks.
//!
//! The API is intentionally synchronous. Async callers must invoke it on a
//! blocking thread. It performs no network access and stores references, not
//! conversion inputs or checkpoint payloads.

#![allow(clippy::missing_errors_doc)]

use into_markdown_core::ConversionError;
use into_markdown_engine::{RecoveryStore, RecoveryToken, TaskPhase};
#[cfg(any(unix, windows))]
use rusqlite::OpenFlags;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
#[cfg(all(test, unix))]
use std::cell::Cell;
use std::cell::RefCell;
#[cfg(any(unix, windows))]
use std::path::Component;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[cfg(any(unix, windows))]
const SCHEMA_VERSION: i64 = 5;
#[cfg(any(unix, windows))]
const DATABASE_FILE: &str = "tasks.sqlite3";
#[cfg(any(unix, windows))]
const MAX_DATABASE_BYTES: i64 = 256 * 1024 * 1024;
#[cfg(any(unix, windows))]
const PAGE_SIZE: i64 = 4096;
const MAX_ROWS: u32 = 100;
const MAX_DIAGNOSTICS: usize = 64;
const MAX_ARTIFACTS: usize = 128;
const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 16 * 1024;
#[cfg(any(unix, windows))]
const MAX_JSON_BYTES_I32: i32 = 16 * 1024;
const MAX_TASKS: i64 = 100_000;
const ID_BYTES: usize = 16;
#[cfg(all(test, unix))]
thread_local! {
    static INJECT_PUBLISH_SOURCE_SWAP: Cell<bool> = const { Cell::new(false) };
    static INJECT_PUBLISHED_FINAL_SWAP: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone)]
struct BusyAttempt {
    #[cfg(any(unix, windows))]
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

thread_local! {
    static BUSY_ATTEMPT: RefCell<Option<(BusyAttempt, u32)>> = const { RefCell::new(None) };
}

struct BusyOperation;

impl BusyOperation {
    fn enter(control: &BusyControl) -> Result<Self, TaskStoreError> {
        if control.cancelled.load(Ordering::Acquire) {
            return Err(TaskStoreError::Cancelled);
        }
        BUSY_ATTEMPT.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some((_, depth)) = slot.as_mut() {
                *depth = depth.saturating_add(1);
            } else {
                *slot = Some((
                    BusyAttempt {
                        #[cfg(any(unix, windows))]
                        deadline: Instant::now()
                            .checked_add(control.timeout)
                            .unwrap_or_else(Instant::now),
                        cancelled: Arc::clone(&control.cancelled),
                    },
                    1,
                ));
            }
        });
        Ok(Self)
    }
}

impl Drop for BusyOperation {
    fn drop(&mut self) {
        BUSY_ATTEMPT.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some((_, depth)) = slot.as_mut() {
                if *depth > 1 {
                    *depth -= 1;
                } else {
                    *slot = None;
                }
            }
        });
    }
}

#[cfg(any(unix, windows))]
fn sqlite_busy_handler(_attempt: i32) -> bool {
    let retry = BUSY_ATTEMPT.with(|slot| {
        slot.borrow().as_ref().is_some_and(|(attempt, _)| {
            !attempt.cancelled.load(Ordering::Acquire) && Instant::now() < attempt.deadline
        })
    });
    if retry {
        std::thread::sleep(Duration::from_millis(1));
    }
    retry
}

fn busy_error() -> TaskStoreError {
    BUSY_ATTEMPT.with(|slot| {
        if slot
            .borrow()
            .as_ref()
            .is_some_and(|(attempt, _)| attempt.cancelled.load(Ordering::Acquire))
        {
            TaskStoreError::Cancelled
        } else {
            TaskStoreError::BusyTimeout
        }
    })
}

/// Stable task-store failure categories.
#[derive(Debug, Error)]
pub enum TaskStoreError {
    /// The store path or a managed file violates the local trust boundary.
    #[error("unsafe task-store path: {0}")]
    UnsafePath(String),
    /// The database schema is newer than this binary understands.
    #[error("unsupported task-store schema version {found}; maximum is {supported}")]
    UnsupportedVersion {
        /// Version read from the database header.
        found: i64,
        /// Highest schema understood by this binary.
        supported: i64,
    },
    /// Persisted data is malformed or `SQLite` reports corruption.
    #[error("corrupt task store: {0}")]
    Corrupt(String),
    /// A public DTO or query exceeds a documented limit.
    #[error("task-store limit exceeded: {0}")]
    Limit(String),
    /// A compare-and-set transition lost a race or is illegal.
    #[error("task-store conflict: {0}")]
    Conflict(String),
    /// Lock acquisition exceeded its total deadline.
    #[error("task store remained locked until its deadline")]
    BusyTimeout,
    /// Lock acquisition was cancelled by the caller.
    #[error("task-store operation was cancelled")]
    Cancelled,
    /// `SQLite` or filesystem I/O failed without a corruption diagnosis.
    #[error("task-store I/O failed: {0}")]
    Io(String),
    /// This platform lacks the audited capability-bound implementation.
    #[error("task store is unavailable on this platform: {0}")]
    PlatformUnavailable(String),
}

/// Cancellation and total lock-wait deadline used by a connection.
#[derive(Clone)]
pub struct BusyControl {
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
    interrupts: Arc<Mutex<Vec<Weak<rusqlite::InterruptHandle>>>>,
}

impl BusyControl {
    /// Create a bounded lock policy. Zero and values over 30 seconds are rejected.
    pub fn new(timeout: Duration) -> Result<Self, TaskStoreError> {
        if timeout.is_zero() || timeout > Duration::from_secs(30) {
            return Err(TaskStoreError::Limit("busy timeout must be within 1 ms and 30 s".into()));
        }
        Ok(Self {
            timeout,
            cancelled: Arc::new(AtomicBool::new(false)),
            interrupts: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Request cancellation of lock waits on this connection.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Ok(mut handles) = self.interrupts.lock() {
            handles.retain(|handle| {
                handle.upgrade().is_some_and(|handle| {
                    handle.interrupt();
                    true
                })
            });
        }
    }

    /// Clear a previous cancellation before explicitly starting more work.
    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::Release);
    }
}

impl std::fmt::Debug for BusyControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BusyControl")
            .field("timeout", &self.timeout)
            .field("cancelled", &self.cancelled.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Default for BusyControl {
    fn default() -> Self {
        Self::new(Duration::from_secs(2)).expect("constant busy timeout is valid")
    }
}

/// Canonical opaque task identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    /// Parse a 128-bit lowercase hexadecimal identifier.
    pub fn parse(value: impl Into<String>) -> Result<Self, TaskStoreError> {
        let value = value.into();
        require_hex(&value, ID_BYTES, "task id")?;
        Ok(Self(value))
    }

    /// Borrow the canonical representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Durable task state. Terminal values cannot use ordinary transitions; a
/// caller may explicitly requeue a failed, interrupted, or cancelled task
/// after authenticating its retained input and recovery checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    /// Accepted but not executing.
    Pending,
    /// Conversion is executing.
    Running,
    /// A validated converted checkpoint is durable.
    Converted,
    /// Complete-result checkpoint metadata is durable in `RecoveryStore`.
    /// External artifact publication is independent.
    Succeeded,
    /// Execution failed.
    Failed,
    /// A restart found no usable checkpoint.
    Interrupted,
    /// The user cancelled execution.
    Cancelled,
}

impl TaskStatus {
    fn as_db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Converted => "converted",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, TaskStoreError> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "converted" => Ok(Self::Converted),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(TaskStoreError::Corrupt("task contains an unknown status".into())),
        }
    }

    fn may_transition(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Running | Self::Failed | Self::Cancelled | Self::Interrupted)
                | (
                    Self::Running,
                    Self::Converted | Self::Failed | Self::Cancelled | Self::Interrupted
                )
                | (
                    Self::Converted,
                    Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
                )
        )
    }
}

/// Allowlisted output format persisted in a non-secret configuration snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputFormat {
    /// Markdown text.
    Markdown,
}

/// Explicitly allowlisted, non-secret conversion settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigurationSnapshot {
    /// Snapshot wire schema.
    pub schema_version: u32,
    /// Requested output format.
    pub output_format: OutputFormat,
    /// Whether local OCR was enabled.
    pub ocr_enabled: bool,
    /// Whether layout preservation was requested.
    pub preserve_layout: bool,
}

impl Default for ConfigurationSnapshot {
    fn default() -> Self {
        Self {
            schema_version: 1,
            output_format: OutputFormat::Markdown,
            ocr_enabled: false,
            preserve_layout: false,
        }
    }
}

/// Reference-only input metadata. Input bytes remain in filesystem/checkpoint storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputReference {
    /// Reference schema used to bind task and recovery metadata.
    pub schema_version: u32,
    /// Exact `RecoveryStore` fingerprint of resolved input and trusted metadata.
    pub input_fingerprint: String,
    /// Exact `RecoveryStore` fingerprint of format hint and conversion options.
    pub options_fingerprint: String,
    /// Resolved input byte count.
    pub byte_len: u64,
    /// Canonical `RecoveryStore` token used to locate checkpoint state.
    pub recovery_token: String,
}

/// Stable, non-free-form diagnostic categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticCode {
    /// A restart found no committed checkpoint.
    RecoveryCheckpointMissing,
    /// A checkpoint was structurally invalid.
    RecoveryCheckpointInvalid,
    /// A checkpoint belongs to different input or conversion options.
    RecoveryCheckpointIncompatible,
    /// The task conversion failed.
    ConversionFailed,
    /// The task was cancelled.
    Cancelled,
}

impl DiagnosticCode {
    fn as_db(self) -> &'static str {
        match self {
            Self::RecoveryCheckpointMissing => "recoveryCheckpointMissing",
            Self::RecoveryCheckpointInvalid => "recoveryCheckpointInvalid",
            Self::RecoveryCheckpointIncompatible => "recoveryCheckpointIncompatible",
            Self::ConversionFailed => "conversionFailed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, TaskStoreError> {
        match value {
            "recoveryCheckpointMissing" => Ok(Self::RecoveryCheckpointMissing),
            "recoveryCheckpointInvalid" => Ok(Self::RecoveryCheckpointInvalid),
            "recoveryCheckpointIncompatible" => Ok(Self::RecoveryCheckpointIncompatible),
            "conversionFailed" => Ok(Self::ConversionFailed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(TaskStoreError::Corrupt("task contains an unknown diagnostic".into())),
        }
    }
}

/// One bounded diagnostic entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskDiagnostic {
    /// Stable category; arbitrary provider messages are deliberately excluded.
    pub code: DiagnosticCode,
}

/// Allowlisted artifact type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArtifactKind {
    /// Rendered Markdown output.
    Markdown,
    /// Complete validated Document IR JSON.
    DocumentIr,
    /// Complete structured diagnostics JSON.
    Diagnostics,
    /// Extracted binary asset.
    Asset,
    /// Portable ZIP bundle containing the complete result set.
    Bundle,
}

impl ArtifactKind {
    fn as_db(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::DocumentIr => "documentIr",
            Self::Diagnostics => "diagnostics",
            Self::Asset => "asset",
            Self::Bundle => "bundle",
        }
    }

    fn parse(value: &str) -> Result<Self, TaskStoreError> {
        match value {
            "markdown" => Ok(Self::Markdown),
            "documentIr" => Ok(Self::DocumentIr),
            "diagnostics" => Ok(Self::Diagnostics),
            "asset" => Ok(Self::Asset),
            "bundle" => Ok(Self::Bundle),
            _ => Err(TaskStoreError::Corrupt("task contains an unknown artifact kind".into())),
        }
    }
}

/// Reference to an artifact stored outside `SQLite`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactReference {
    /// Opaque storage key, not a caller-controlled path or URL.
    pub storage_key: String,
    /// Artifact category.
    pub kind: ArtifactKind,
    /// Byte length of the external object.
    pub byte_len: u64,
    /// SHA-256 of the external object.
    pub sha256: String,
    /// Source IR asset ID, present only for asset artifacts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_id: Option<String>,
    /// Safe display filename for an asset artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Canonical media type for an asset artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// Complete bounded task record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRecord {
    /// Stable task identifier.
    pub id: TaskId,
    /// Milliseconds since the Unix epoch in UTC.
    pub created_at_ms: i64,
    /// Monotonically increasing persisted update timestamp.
    pub updated_at_ms: i64,
    /// Current durable state.
    pub status: TaskStatus,
    /// Integer progress in millionths, from 0 through 1,000,000.
    pub progress_millionths: u32,
    /// Reference-only input metadata.
    pub input: InputReference,
    /// Non-secret allowlisted configuration.
    pub configuration: ConfigurationSnapshot,
    /// Bounded structured diagnostics.
    pub diagnostics: Vec<TaskDiagnostic>,
    /// Bounded external artifact index.
    pub artifacts: Vec<ArtifactReference>,
    /// Monotonic generation of the currently published artifact set.
    pub artifact_generation: u64,
    /// Whether the user pinned this task against future retention policy.
    pub pinned: bool,
}

/// Creation request; the store generates the task ID and timestamps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTask {
    /// Reference-only input metadata.
    pub input: InputReference,
    /// Explicit non-secret configuration snapshot.
    pub configuration: ConfigurationSnapshot,
}

/// Atomic compare-and-set state update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskTransition {
    /// Required current state.
    pub expected: TaskStatus,
    /// Desired next state.
    pub next: TaskStatus,
    /// New progress value; it may not decrease.
    pub progress_millionths: u32,
    /// Diagnostics appended in the same transaction.
    pub diagnostics: Vec<TaskDiagnostic>,
    /// Artifacts appended in the same transaction.
    pub artifacts: Vec<ArtifactReference>,
}

/// Stable cursor for newest-first task listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskCursor {
    /// Previous page's final update timestamp.
    pub updated_at_ms: i64,
    /// Previous page's final task identifier.
    pub id: TaskId,
}

/// Result of restart reconciliation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileSummary {
    /// Tasks with a valid media chunk checkpoint that remain resumable.
    pub resumable: u32,
    /// Tasks promoted from a valid converted checkpoint.
    pub converted: u32,
    /// Reserved compatibility counter. Recovery alone never proves artifact publication.
    pub succeeded: u32,
    /// Tasks marked interrupted because no checkpoint existed.
    pub interrupted: u32,
    /// Tasks marked failed because checkpoint state was invalid.
    pub failed: u32,
}

#[derive(Clone, Copy)]
enum ReconcileOutcome {
    Resumable,
    Converted,
    Interrupted,
    Failed,
}

/// One private `SQLite` connection. It is synchronous and may be moved, but not shared.
pub struct TaskStore {
    connection: Connection,
    interrupt: Arc<rusqlite::InterruptHandle>,
    directory: SafeDirectory,
    #[cfg(any(unix, windows))]
    database_identity: (u64, u64),
    busy: BusyControl,
}

impl std::fmt::Debug for TaskStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskStore")
            .field("directory", &self.directory.path)
            .finish_non_exhaustive()
    }
}

impl Drop for TaskStore {
    fn drop(&mut self) {
        if let Ok(mut handles) = self.busy.interrupts.lock() {
            handles.retain(|handle| {
                handle.upgrade().is_some_and(|handle| !Arc::ptr_eq(&handle, &self.interrupt))
            });
        }
    }
}

impl TaskStore {
    /// Open or create a private store, configure WAL/durability, and migrate it.
    ///
    /// Migrations are transactional and only move forward. Unknown newer
    /// versions fail closed. Unix uses directory-relative no-follow handles; Windows binds the
    /// database and sidecars to protected current-user DACLs and physical file identities.
    pub fn open(root: impl Into<PathBuf>, busy: BusyControl) -> Result<Self, TaskStoreError> {
        #[cfg(windows)]
        {
            let _operation = BusyOperation::enter(&busy)?;
            let directory = SafeDirectory::open_or_create_windows(root.into())?;
            let database_identity = directory.prepare_database_file_windows(DATABASE_FILE)?;
            let connection = Connection::open_with_flags(
                directory.path.join(DATABASE_FILE),
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .map_err(map_sqlite_open)?;
            install_limits(&connection);
            configure_busy(&connection, &busy)?;
            let interrupt = Arc::new(connection.get_interrupt_handle());
            let mut interrupts = busy
                .interrupts
                .lock()
                .map_err(|_| TaskStoreError::Io("busy control lock was poisoned".into()))?;
            interrupts.try_reserve(1).map_err(|_| {
                TaskStoreError::Limit("interrupt registration allocation failed".into())
            })?;
            interrupts.push(Arc::downgrade(&interrupt));
            drop(interrupts);
            check_schema_ceiling(&connection)?;
            configure(&connection)?;
            migrate(&connection)?;
            verify_integrity(&connection)?;
            directory.verify_database_files_windows(database_identity)?;
            Ok(Self { connection, interrupt, directory, database_identity, busy })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (root, busy);
            Err(TaskStoreError::PlatformUnavailable(
                "capability-bound SQLite files are currently audited only on Unix".into(),
            ))
        }
        #[cfg(unix)]
        {
            let _operation = BusyOperation::enter(&busy)?;
            let directory = SafeDirectory::open_or_create(root.into())?;
            directory.verify_namespace()?;
            directory.prepare_database_file(DATABASE_FILE)?;
            let database_identity = directory.regular_private_identity(DATABASE_FILE)?;
            let connection = Connection::open_with_flags(
                directory.path.join(DATABASE_FILE),
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .map_err(map_sqlite_open)?;
            directory.verify_database_files(database_identity)?;
            install_limits(&connection);
            configure_busy(&connection, &busy)?;
            let interrupt = Arc::new(connection.get_interrupt_handle());
            let mut interrupts = busy
                .interrupts
                .lock()
                .map_err(|_| TaskStoreError::Io("busy control lock was poisoned".into()))?;
            interrupts.try_reserve(1).map_err(|_| {
                TaskStoreError::Limit("interrupt registration allocation failed".into())
            })?;
            interrupts.push(Arc::downgrade(&interrupt));
            drop(interrupts);
            check_schema_ceiling(&connection)?;
            configure(&connection)?;
            migrate(&connection)?;
            verify_integrity(&connection)?;
            directory.verify_database_files(database_identity)?;
            Ok(Self { connection, interrupt, directory, database_identity, busy })
        }
    }

    /// Return this connection's cancellation/deadline control.
    #[must_use]
    pub fn busy_control(&self) -> BusyControl {
        self.busy.clone()
    }

    /// Create a pending task atomically.
    #[allow(clippy::needless_pass_by_value)]
    pub fn create(&mut self, task: NewTask) -> Result<TaskRecord, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        validate_input(&task.input)?;
        validate_configuration(&task.configuration)?;
        let now = utc_now_ms()?;
        let config_json = bounded_json(&task.configuration)?;
        let started = Instant::now();
        let transaction = loop {
            match self
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            {
                Ok(transaction) => break transaction,
                Err(error) if is_busy(&error) => wait_for_busy(&self.busy, started)?,
                Err(error) => return Err(map_sqlite_generic(error)),
            }
        };
        let count: i64 = transaction
            .query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))
            .map_err(map_sqlite_generic)?;
        if count >= MAX_TASKS {
            return Err(TaskStoreError::Limit("task count reached 100000".into()));
        }
        let token_exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE recovery_token=?1)",
                [&task.input.recovery_token],
                |row| row.get(0),
            )
            .map_err(map_sqlite_generic)?;
        if token_exists {
            return Err(TaskStoreError::Conflict(
                "recovery token is already bound to another task".into(),
            ));
        }
        let mut inserted = None;
        for _ in 0..8 {
            let candidate = random_id()?;
            match transaction.execute(
                "INSERT INTO tasks(id, created_at_ms, updated_at_ms, status, progress, input_fingerprint, options_fingerprint, recovery_token, input_bytes, config_json, pinned) VALUES(?1, ?2, ?2, 'pending', 0, ?3, ?4, ?5, ?6, ?7, 0)",
                params![candidate.as_str(), now, task.input.input_fingerprint, task.input.options_fingerprint, task.input.recovery_token, task.input.byte_len, config_json],
            ) {
                Ok(_) => {
                    inserted = Some(candidate);
                    break;
                }
                Err(rusqlite::Error::SqliteFailure(inner, _))
                    if inner.code == rusqlite::ErrorCode::ConstraintViolation => {}
                Err(error) => return Err(map_sqlite_generic(error)),
            }
        }
        let id = inserted
            .ok_or_else(|| TaskStoreError::Conflict("task id collision budget exhausted".into()))?;
        transaction.commit().map_err(map_sqlite_generic)?;
        self.preflight()?;
        self.get(&id)?.ok_or_else(|| TaskStoreError::Corrupt("created task disappeared".into()))
    }

    /// Load one task and its bounded child rows.
    pub fn get(&self, id: &TaskId) -> Result<Option<TaskRecord>, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let base = self
            .connection
            .query_row(
                "SELECT created_at_ms, updated_at_ms, status, progress, input_fingerprint, options_fingerprint, recovery_token, input_bytes, config_json, artifact_generation, pinned FROM tasks WHERE id=?1",
                [id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| self.map_sqlite(error))?;
        let Some((
            created,
            updated,
            status,
            progress,
            input_fingerprint,
            options_fingerprint,
            recovery_token,
            input_bytes,
            configuration,
            artifact_generation,
            pinned,
        )) = base
        else {
            return Ok(None);
        };
        if created < 0 || updated < created || !(0..=1_000_000).contains(&progress) {
            return Err(TaskStoreError::Corrupt("task timestamps or progress are invalid".into()));
        }
        let status = TaskStatus::parse(&status)?;
        if status == TaskStatus::Succeeded && progress != 1_000_000 {
            return Err(TaskStoreError::Corrupt(
                "succeeded task does not have complete progress".into(),
            ));
        }
        if input_bytes < 0 {
            return Err(TaskStoreError::Corrupt("task input byte length is invalid".into()));
        }
        let input = InputReference {
            schema_version: 1,
            input_fingerprint,
            options_fingerprint,
            recovery_token,
            byte_len: u64::try_from(input_bytes)
                .map_err(|_| TaskStoreError::Corrupt("task input byte length is invalid".into()))?,
        };
        validate_input(&input)?;
        let configuration: ConfigurationSnapshot = decode_bounded(&configuration, "configuration")?;
        validate_configuration(&configuration)?;
        let diagnostics = self.load_diagnostics(id)?;
        let artifacts = self.load_artifacts(id)?;
        let artifact_generation = u64::try_from(artifact_generation)
            .map_err(|_| TaskStoreError::Corrupt("artifact generation is invalid".into()))?;
        Ok(Some(TaskRecord {
            id: id.clone(),
            created_at_ms: created,
            updated_at_ms: updated,
            status,
            progress_millionths: u32::try_from(progress)
                .map_err(|_| TaskStoreError::Corrupt("task progress is invalid".into()))?,
            input,
            configuration,
            diagnostics,
            artifacts,
            artifact_generation,
            pinned: match pinned {
                0 => false,
                1 => true,
                _ => return Err(TaskStoreError::Corrupt("task pinned marker is invalid".into())),
            },
        }))
    }

    /// Replace the complete artifact set of a succeeded task with generation CAS.
    ///
    /// This is used for metadata-only rerenders such as anonymous speaker relabeling.
    /// The task remains succeeded and conversion is not run again.
    pub fn replace_succeeded_artifacts(
        &mut self,
        id: &TaskId,
        expected_generation: u64,
        artifacts: Vec<ArtifactReference>,
    ) -> Result<TaskRecord, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        if artifacts.is_empty() || artifacts.len() > MAX_ARTIFACTS {
            return Err(TaskStoreError::Limit(
                "replacement artifact count must be within 1 and 128".into(),
            ));
        }
        if i64::try_from(expected_generation).is_err() {
            return Err(TaskStoreError::Limit("artifact generation exceeds SQLite range".into()));
        }
        let total = artifacts.iter().try_fold(0_u64, |total, artifact| {
            validate_artifact(artifact)?;
            total.checked_add(artifact.byte_len).ok_or_else(|| {
                TaskStoreError::Limit("artifact byte length total overflowed".into())
            })
        })?;
        if total > MAX_ARTIFACT_BYTES {
            return Err(TaskStoreError::Limit("task artifact bytes exceed 2 GiB".into()));
        }
        let now = utc_now_ms()?;
        let started = Instant::now();
        let transaction = loop {
            match self
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            {
                Ok(transaction) => break transaction,
                Err(error) if is_busy(&error) => wait_for_busy(&self.busy, started)?,
                Err(error) => return Err(map_sqlite_generic(error)),
            }
        };
        let current: Option<(String, i64, i64)> = transaction
            .query_row(
                "SELECT status, artifact_generation, updated_at_ms FROM tasks WHERE id=?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(map_sqlite_generic)?;
        let Some((status, generation, updated)) = current else {
            return Err(TaskStoreError::Conflict("task does not exist".into()));
        };
        if TaskStatus::parse(&status)? != TaskStatus::Succeeded
            || generation != i64::try_from(expected_generation).unwrap_or(i64::MAX)
        {
            return Err(TaskStoreError::Conflict(
                "artifact generation compare-and-set was rejected".into(),
            ));
        }
        let next_generation = generation
            .checked_add(1)
            .ok_or_else(|| TaskStoreError::Limit("artifact generation overflowed".into()))?;
        transaction
            .execute("DELETE FROM artifacts WHERE task_id=?1", [id.as_str()])
            .map_err(map_sqlite_generic)?;
        for artifact in artifacts {
            transaction
                .execute(
                    "INSERT INTO artifacts(task_id, storage_key, kind, byte_len, sha256, asset_id, filename, media_type) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![id.as_str(), artifact.storage_key, artifact.kind.as_db(), artifact.byte_len, artifact.sha256, artifact.asset_id, artifact.filename, artifact.media_type],
                )
                .map_err(map_sqlite_generic)?;
        }
        let updated = now.max(updated.saturating_add(1));
        let changed = transaction
            .execute(
                "UPDATE tasks SET artifact_generation=?1, updated_at_ms=?2 WHERE id=?3 AND status='succeeded' AND artifact_generation=?4",
                params![next_generation, updated, id.as_str(), generation],
            )
            .map_err(map_sqlite_generic)?;
        if changed != 1 {
            return Err(TaskStoreError::Conflict(
                "artifact generation compare-and-set lost a race".into(),
            ));
        }
        transaction.commit().map_err(map_sqlite_generic)?;
        self.preflight()?;
        self.get(id)?.ok_or_else(|| TaskStoreError::Corrupt("updated task disappeared".into()))
    }

    /// List a stable newest-first page. The maximum page size is 100.
    pub fn list(
        &self,
        limit: u32,
        after: Option<&TaskCursor>,
    ) -> Result<Vec<TaskRecord>, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        if limit == 0 || limit > MAX_ROWS {
            return Err(TaskStoreError::Limit("list limit must be within 1 and 100".into()));
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT id FROM tasks WHERE (?1 IS NULL OR updated_at_ms < ?1 OR (updated_at_ms = ?1 AND id < ?2)) ORDER BY updated_at_ms DESC, id DESC LIMIT ?3",
            )
            .map_err(|error| self.map_sqlite(error))?;
        let cursor_time = after.map(|cursor| cursor.updated_at_ms);
        let cursor_id = after.map(|cursor| cursor.id.as_str());
        let rows = statement
            .query_map(params![cursor_time, cursor_id, limit], |row| row.get::<_, String>(0))
            .map_err(|error| self.map_sqlite(error))?;
        let mut ids = Vec::new();
        ids.try_reserve(limit as usize)
            .map_err(|_| TaskStoreError::Limit("list allocation failed".into()))?;
        for row in rows {
            ids.push(TaskId::parse(row.map_err(|error| self.map_sqlite(error))?)?);
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(ids.len())
            .map_err(|_| TaskStoreError::Limit("list result allocation failed".into()))?;
        for id in ids {
            records.push(
                self.get(&id)?
                    .ok_or_else(|| TaskStoreError::Corrupt("listed task disappeared".into()))?,
            );
        }
        Ok(records)
    }

    /// Atomically compare-and-set state, progress, diagnostics, and artifacts.
    pub fn transition(
        &mut self,
        id: &TaskId,
        transition: TaskTransition,
    ) -> Result<TaskRecord, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        validate_transition(&transition)?;
        let now = utc_now_ms()?;
        let started = Instant::now();
        let transaction = loop {
            match self
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            {
                Ok(transaction) => break transaction,
                Err(error) if is_busy(&error) => wait_for_busy(&self.busy, started)?,
                Err(error) => return Err(map_sqlite_generic(error)),
            }
        };
        let current: Option<(String, i64, i64)> = transaction
            .query_row(
                "SELECT status, progress, updated_at_ms FROM tasks WHERE id=?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(map_sqlite_generic)?;
        let Some((current, progress, updated)) = current else {
            return Err(TaskStoreError::Conflict("task does not exist".into()));
        };
        let current = TaskStatus::parse(&current)?;
        if current != transition.expected || !current.may_transition(transition.next) {
            return Err(TaskStoreError::Conflict("task state transition was rejected".into()));
        }
        if i64::from(transition.progress_millionths) < progress {
            return Err(TaskStoreError::Conflict("task progress cannot decrease".into()));
        }
        let existing_diagnostics: i64 = transaction
            .query_row("SELECT count(*) FROM diagnostics WHERE task_id=?1", [id.as_str()], |row| {
                row.get(0)
            })
            .map_err(map_sqlite_generic)?;
        let existing_artifacts: i64 = transaction
            .query_row("SELECT count(*) FROM artifacts WHERE task_id=?1", [id.as_str()], |row| {
                row.get(0)
            })
            .map_err(map_sqlite_generic)?;
        if usize::try_from(existing_diagnostics).unwrap_or(usize::MAX)
            + transition.diagnostics.len()
            > MAX_DIAGNOSTICS
            || usize::try_from(existing_artifacts).unwrap_or(usize::MAX)
                + transition.artifacts.len()
                > MAX_ARTIFACTS
        {
            return Err(TaskStoreError::Limit("task child row count exceeded".into()));
        }
        validate_artifact_budget(&transaction, id, &transition.artifacts)?;
        let updated = now.max(updated.saturating_add(1));
        let changed = transaction
            .execute(
                "UPDATE tasks SET status=?1, progress=?2, updated_at_ms=?3, completed_at_ms=CASE WHEN ?1 IN ('succeeded','failed','interrupted','cancelled') THEN ?3 ELSE completed_at_ms END WHERE id=?4 AND status=?5 AND progress<=?2",
                params![transition.next.as_db(), transition.progress_millionths, updated, id.as_str(), transition.expected.as_db()],
            )
            .map_err(map_sqlite_generic)?;
        if changed != 1 {
            return Err(TaskStoreError::Conflict("task state compare-and-set lost a race".into()));
        }
        for diagnostic in transition.diagnostics {
            transaction
                .execute(
                    "INSERT INTO diagnostics(task_id, ordinal, code) VALUES(?1, (SELECT count(*) FROM diagnostics WHERE task_id=?1), ?2)",
                    params![id.as_str(), diagnostic.code.as_db()],
                )
                .map_err(map_sqlite_generic)?;
        }
        for artifact in transition.artifacts {
            validate_artifact(&artifact)?;
            transaction
                .execute(
                    "INSERT INTO artifacts(task_id, storage_key, kind, byte_len, sha256, asset_id, filename, media_type) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![id.as_str(), artifact.storage_key, artifact.kind.as_db(), artifact.byte_len, artifact.sha256, artifact.asset_id, artifact.filename, artifact.media_type],
                )
                .map_err(map_sqlite_generic)?;
        }
        transaction.commit().map_err(map_sqlite_generic)?;
        self.preflight()?;
        self.get(id)?.ok_or_else(|| TaskStoreError::Corrupt("updated task disappeared".into()))
    }

    /// Atomically change the pinned marker without changing state or progress.
    pub fn set_pinned(&mut self, id: &TaskId, pinned: bool) -> Result<(), TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let now = utc_now_ms()?;
        let started = Instant::now();
        let changed = loop {
            match self.connection.execute(
                "UPDATE tasks SET pinned=?1, updated_at_ms=max(?2, updated_at_ms + 1) WHERE id=?3",
                params![pinned, now, id.as_str()],
            ) {
                Ok(changed) => break changed,
                Err(error) if is_busy(&error) => wait_for_busy(&self.busy, started)?,
                Err(error) => return Err(self.map_sqlite(error)),
            }
        };
        if changed != 1 {
            return Err(TaskStoreError::Conflict("task does not exist".into()));
        }
        self.preflight()
    }

    /// Atomically clear terminal diagnostics and return a task to `Pending`.
    ///
    /// The caller must first authenticate the retained input and a compatible
    /// recovery checkpoint. Successful tasks and any task with artifacts are
    /// never eligible for this operation.
    pub fn requeue_terminal(&mut self, id: &TaskId) -> Result<TaskRecord, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let now = utc_now_ms()?;
        let started = Instant::now();
        let transaction = loop {
            match self
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            {
                Ok(transaction) => break transaction,
                Err(error) if is_busy(&error) => wait_for_busy(&self.busy, started)?,
                Err(error) => return Err(map_sqlite_generic(error)),
            }
        };
        let current: Option<(String, i64)> = transaction
            .query_row(
                "SELECT status, (SELECT count(*) FROM artifacts WHERE task_id=tasks.id) FROM tasks WHERE id=?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_generic)?;
        let Some((status, artifacts)) = current else {
            return Err(TaskStoreError::Conflict("task does not exist".into()));
        };
        let status = TaskStatus::parse(&status)?;
        if !matches!(status, TaskStatus::Failed | TaskStatus::Interrupted | TaskStatus::Cancelled)
            || artifacts != 0
        {
            return Err(TaskStoreError::Conflict(
                "only terminal tasks without artifacts can be requeued".into(),
            ));
        }
        transaction
            .execute("DELETE FROM diagnostics WHERE task_id=?1", [id.as_str()])
            .map_err(map_sqlite_generic)?;
        let changed = transaction
            .execute(
                "UPDATE tasks SET status='pending', progress=0, completed_at_ms=NULL, updated_at_ms=max(?1, updated_at_ms + 1) WHERE id=?2 AND status=?3",
                params![now, id.as_str(), status.as_db()],
            )
            .map_err(map_sqlite_generic)?;
        if changed != 1 {
            return Err(TaskStoreError::Conflict(
                "task state changed before it was requeued".into(),
            ));
        }
        transaction.commit().map_err(map_sqlite_generic)?;
        self.preflight()?;
        self.get(id)?.ok_or_else(|| TaskStoreError::Corrupt("requeued task disappeared".into()))
    }

    /// Delete one terminal task and all of its child rows in one transaction.
    ///
    /// Active work and pinned tasks are rejected. Callers coordinating external
    /// objects must quarantine those objects before this commit and restore the
    /// quarantine if the transaction fails.
    pub fn delete_terminal(
        &mut self,
        id: &TaskId,
        allow_pinned: bool,
    ) -> Result<(), TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let started = Instant::now();
        let transaction = loop {
            match self
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            {
                Ok(transaction) => break transaction,
                Err(error) if is_busy(&error) => wait_for_busy(&self.busy, started)?,
                Err(error) => return Err(map_sqlite_generic(error)),
            }
        };
        let current: Option<(String, i64)> = transaction
            .query_row("SELECT status, pinned FROM tasks WHERE id=?1", [id.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .map_err(map_sqlite_generic)?;
        let Some((status, pinned)) = current else {
            return Err(TaskStoreError::Conflict("task does not exist".into()));
        };
        let status = TaskStatus::parse(&status)?;
        if !matches!(
            status,
            TaskStatus::Succeeded
                | TaskStatus::Failed
                | TaskStatus::Interrupted
                | TaskStatus::Cancelled
        ) {
            return Err(TaskStoreError::Conflict("active task cannot be deleted".into()));
        }
        if pinned == 1 && !allow_pinned {
            return Err(TaskStoreError::Conflict(
                "pinned task cannot be retained automatically".into(),
            ));
        }
        transaction
            .execute("DELETE FROM tasks WHERE id=?1", [id.as_str()])
            .map_err(map_sqlite_generic)?;
        transaction.commit().map_err(map_sqlite_generic)?;
        // A successful commit is the unambiguous coordination point for the
        // caller's quarantined filesystem objects. Do not perform fallible
        // work after it and accidentally report that the row still exists.
        Ok(())
    }

    /// Return whether a recovery token is still owned by a durable task.
    ///
    /// Retention recovery uses this to ensure a forged or stale trash entry
    /// cannot select the checkpoint of a different live task for deletion.
    pub fn recovery_token_in_use(&self, token: &str) -> Result<bool, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        if token.len() != 32 || !token.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(TaskStoreError::UnsafePath("invalid recovery token".into()));
        }
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE recovery_token=?1)",
                [token],
                |row| row.get(0),
            )
            .map_err(|error| self.map_sqlite(error))
    }

    /// Return the persisted instant at which a task first became terminal.
    pub fn completed_at_ms(&self, id: &TaskId) -> Result<Option<i64>, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        self.connection
            .query_row("SELECT completed_at_ms FROM tasks WHERE id=?1", [id.as_str()], |row| {
                row.get(0)
            })
            .optional()
            .map(Option::flatten)
            .map_err(|error| self.map_sqlite(error))
    }

    /// Reconcile nonterminal tasks against committed `RecoveryStore` metadata.
    /// Checkpoints are inspected but never deleted or garbage-collected.
    pub fn reconcile(
        &mut self,
        recovery: &RecoveryStore,
    ) -> Result<ReconcileSummary, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let mut summary = ReconcileSummary::default();
        let high_water: Option<String> = self
            .connection
            .query_row("SELECT max(id) FROM tasks", [], |row| row.get(0))
            .map_err(|error| self.map_sqlite(error))?;
        let Some(high_water) = high_water else { return Ok(summary) };
        let mut cursor = String::new();
        loop {
            let candidates = self.nonterminal_candidates(&cursor, &high_water)?;
            if candidates.is_empty() {
                break;
            }
            candidates
                .last()
                .ok_or_else(|| TaskStoreError::Corrupt("reconcile batch vanished".into()))?
                .0
                .as_str()
                .clone_into(&mut cursor);
            for (id, status, token) in candidates {
                let record = self
                    .get(&id)?
                    .ok_or_else(|| TaskStoreError::Corrupt("reconcile task disappeared".into()))?;
                if record.status != status {
                    continue;
                }
                let token = RecoveryToken::parse(token).map_err(|_| {
                    TaskStoreError::Corrupt("task recovery token is invalid".into())
                })?;
                let (next, progress, diagnostic, outcome) = match recovery.inspect(&token) {
                    Ok(Some(checkpoint))
                        if !fingerprints_match(
                            &record.input.input_fingerprint,
                            &checkpoint.input_fingerprint,
                        ) || !fingerprints_match(
                            &record.input.options_fingerprint,
                            &checkpoint.options_fingerprint,
                        ) =>
                    {
                        (
                            TaskStatus::Failed,
                            0,
                            Some(DiagnosticCode::RecoveryCheckpointIncompatible),
                            ReconcileOutcome::Failed,
                        )
                    }
                    // A complete RecoveryStore result can replay artifact
                    // publication, but cannot prove that publication already
                    // committed. Never infer Web success from this checkpoint.
                    Ok(Some(checkpoint)) if checkpoint.phase == TaskPhase::Succeeded => {
                        (TaskStatus::Converted, 900_000, None, ReconcileOutcome::Converted)
                    }
                    Ok(Some(checkpoint)) if checkpoint.phase == TaskPhase::Converted => {
                        (TaskStatus::Converted, 900_000, None, ReconcileOutcome::Converted)
                    }
                    Ok(Some(checkpoint)) if checkpoint.phase == TaskPhase::Media => {
                        (status, record.progress_millionths, None, ReconcileOutcome::Resumable)
                    }
                    Ok(None) => (
                        TaskStatus::Interrupted,
                        0,
                        Some(DiagnosticCode::RecoveryCheckpointMissing),
                        ReconcileOutcome::Interrupted,
                    ),
                    Err(ConversionError::Recovery {
                        reason: "corrupt" | "unsupportedVersion" | "incompatible",
                        ..
                    }) => (
                        TaskStatus::Failed,
                        0,
                        Some(DiagnosticCode::RecoveryCheckpointInvalid),
                        ReconcileOutcome::Failed,
                    ),
                    Ok(Some(_)) => {
                        return Err(TaskStoreError::Corrupt("checkpoint phase is unknown".into()));
                    }
                    Err(_) => {
                        return Err(TaskStoreError::Io(
                            "recovery store inspection is temporarily unavailable".into(),
                        ));
                    }
                };
                if self.reconcile_transition(&id, status, next, progress, diagnostic)? {
                    match outcome {
                        ReconcileOutcome::Resumable => summary.resumable += 1,
                        ReconcileOutcome::Converted => summary.converted += 1,
                        ReconcileOutcome::Interrupted => summary.interrupted += 1,
                        ReconcileOutcome::Failed => summary.failed += 1,
                    }
                }
            }
        }
        Ok(summary)
    }

    /// Produce a consistent standalone `SQLite` backup inside the store root.
    ///
    /// The destination is an opaque canonical name and is created no-replace.
    /// Recovery is intentionally not automatic: an operator must authorize and
    /// perform replacement while the primary store is closed.
    pub fn backup(&mut self, backup_id: &TaskId) -> Result<PathBuf, TaskStoreError> {
        #[cfg(unix)]
        return self.backup_unix(backup_id);
        #[cfg(not(unix))]
        {
            let _ = (self, backup_id);
            Err(TaskStoreError::PlatformUnavailable(
                "capability-bound SQLite backups are currently audited only on Unix".into(),
            ))
        }
    }

    #[cfg(unix)]
    fn backup_unix(&mut self, backup_id: &TaskId) -> Result<PathBuf, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let name = format!("backup-{}.sqlite3", backup_id.as_str());
        let temporary = format!(".backup-{}.tmp", random_id()?.as_str());
        self.directory.create_private_file(&temporary)?;
        let temporary_identity = self.directory.regular_private_identity(&temporary)?;
        let temporary_path = self.directory.path.join(&temporary);
        let result = (|| {
            self.preflight()?;
            if self.directory.regular_private_identity(&temporary)? != temporary_identity {
                return Err(TaskStoreError::UnsafePath(
                    "backup temporary file was replaced".into(),
                ));
            }
            let mut destination = Connection::open_with_flags(
                &temporary_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .map_err(map_sqlite_open)?;
            configure_busy(&destination, &self.busy)?;
            if self.directory.regular_private_identity(&temporary)? != temporary_identity {
                return Err(TaskStoreError::UnsafePath(
                    "backup temporary file was replaced".into(),
                ));
            }
            {
                let backup = rusqlite::backup::Backup::new(&self.connection, &mut destination)
                    .map_err(|error| self.map_sqlite(error))?;
                loop {
                    match backup.step(64).map_err(|error| self.map_sqlite(error))? {
                        rusqlite::backup::StepResult::Done => break,
                        rusqlite::backup::StepResult::More => {}
                        rusqlite::backup::StepResult::Busy
                        | rusqlite::backup::StepResult::Locked => wait_for_current_busy()?,
                        _ => {
                            return Err(TaskStoreError::Io("unknown SQLite backup state".into()));
                        }
                    }
                }
            }
            self.preflight()?;
            if self.directory.regular_private_identity(&temporary)? != temporary_identity {
                return Err(TaskStoreError::UnsafePath(
                    "backup temporary file was replaced".into(),
                ));
            }
            destination
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
                .map_err(|error| self.map_sqlite(error))?;
            verify_integrity(&destination)?;
            drop(destination);
            self.directory.sync_file_if_identity(&temporary, temporary_identity)?;
            #[cfg(test)]
            if std::env::var_os("INTO_MD_TASK_STORE_BACKUP_ABORT_CHILD").is_some() {
                std::process::abort();
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.directory.unlink_if_identity(&temporary, temporary_identity);
            return Err(error);
        }
        self.directory.sync()?;
        if let Err(error) =
            self.directory.publish_verified_link(&temporary, &name, temporary_identity)
        {
            let _ = self.directory.unlink_if_identity(&temporary, temporary_identity);
            return Err(error);
        }
        #[cfg(all(test, unix))]
        if INJECT_PUBLISHED_FINAL_SWAP.with(|flag| flag.replace(false)) {
            rustix::fs::renameat(
                &self.directory.fd,
                &name,
                &self.directory.fd,
                ".publish-test-final-original",
            )
            .map_err(path_io)?;
            self.directory.create_private_file(&name)?;
        }
        if let Err(error) = self.directory.verify_or_remove_published(&name, temporary_identity) {
            let _ = self.directory.unlink_if_identity(&temporary, temporary_identity);
            return Err(error);
        }
        self.directory.sync()?;
        if let Err(error) = self.directory.verify_or_remove_published(&name, temporary_identity) {
            let _ = self.directory.unlink_if_identity(&temporary, temporary_identity);
            return Err(error);
        }
        self.directory.unlink_if_identity(&temporary, temporary_identity)?;
        self.directory.verify_or_remove_published(&name, temporary_identity)?;
        self.directory.sync()?;
        self.directory.verify_or_remove_published(&name, temporary_identity)?;
        Ok(self.directory.path.join(name))
    }

    fn preflight(&self) -> Result<(), TaskStoreError> {
        #[cfg(unix)]
        return self.directory.verify_database_files(self.database_identity);
        #[cfg(windows)]
        return self.directory.verify_database_files_windows(self.database_identity);
        #[cfg(not(any(unix, windows)))]
        {
            let _ = &self.directory;
            Err(TaskStoreError::PlatformUnavailable(
                "capability-bound SQLite files are currently audited only on Unix".into(),
            ))
        }
    }

    fn load_diagnostics(&self, id: &TaskId) -> Result<Vec<TaskDiagnostic>, TaskStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT code FROM diagnostics WHERE task_id=?1 ORDER BY ordinal LIMIT 65")
            .map_err(|error| self.map_sqlite(error))?;
        let rows = statement
            .query_map([id.as_str()], |row| row.get::<_, String>(0))
            .map_err(|error| self.map_sqlite(error))?;
        let mut values = Vec::new();
        values
            .try_reserve(MAX_DIAGNOSTICS.min(65))
            .map_err(|_| TaskStoreError::Limit("diagnostic allocation failed".into()))?;
        for row in rows {
            values.push(TaskDiagnostic {
                code: DiagnosticCode::parse(&row.map_err(|error| self.map_sqlite(error))?)?,
            });
        }
        if values.len() > MAX_DIAGNOSTICS {
            return Err(TaskStoreError::Corrupt("task contains too many diagnostics".into()));
        }
        Ok(values)
    }

    fn reconcile_transition(
        &mut self,
        id: &TaskId,
        expected: TaskStatus,
        next: TaskStatus,
        minimum_progress: u32,
        diagnostic: Option<DiagnosticCode>,
    ) -> Result<bool, TaskStoreError> {
        self.preflight()?;
        if !matches!(expected, TaskStatus::Pending | TaskStatus::Running | TaskStatus::Converted) {
            return Ok(false);
        }
        let started = Instant::now();
        let transaction = loop {
            match self
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            {
                Ok(transaction) => break transaction,
                Err(error) if is_busy(&error) => wait_for_busy(&self.busy, started)?,
                Err(error) => return Err(map_sqlite_generic(error)),
            }
        };
        let current: Option<(String, i64, i64)> = transaction
            .query_row(
                "SELECT status, progress, updated_at_ms FROM tasks WHERE id=?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(map_sqlite_generic)?;
        let Some((current, progress, updated)) = current else {
            return Ok(false);
        };
        if TaskStatus::parse(&current)? != expected {
            return Ok(false);
        }
        let now = utc_now_ms()?.max(updated.saturating_add(1));
        let progress = progress.max(i64::from(minimum_progress));
        let changed = transaction
            .execute(
                "UPDATE tasks SET status=?1, progress=?2, updated_at_ms=?3, completed_at_ms=CASE WHEN ?1 IN ('succeeded','failed','interrupted','cancelled') THEN ?3 ELSE completed_at_ms END WHERE id=?4 AND status=?5",
                params![next.as_db(), progress, now, id.as_str(), expected.as_db()],
            )
            .map_err(map_sqlite_generic)?;
        if changed != 1 {
            return Ok(false);
        }
        if let Some(code) = diagnostic {
            transaction
                .execute(
                    "INSERT INTO diagnostics(task_id, ordinal, code) VALUES(?1, (SELECT count(*) FROM diagnostics WHERE task_id=?1), ?2)",
                    params![id.as_str(), code.as_db()],
                )
                .map_err(map_sqlite_generic)?;
        }
        transaction.commit().map_err(map_sqlite_generic)?;
        self.preflight()?;
        Ok(true)
    }

    fn load_artifacts(&self, id: &TaskId) -> Result<Vec<ArtifactReference>, TaskStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT storage_key, kind, byte_len, sha256, asset_id, filename, media_type FROM artifacts WHERE task_id=?1 ORDER BY storage_key LIMIT 129")
            .map_err(|error| self.map_sqlite(error))?;
        let rows = statement
            .query_map([id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(|error| self.map_sqlite(error))?;
        let mut values = Vec::new();
        values
            .try_reserve(MAX_ARTIFACTS.min(129))
            .map_err(|_| TaskStoreError::Limit("artifact allocation failed".into()))?;
        let mut total_bytes = 0_u64;
        for row in rows {
            let (storage_key, kind, byte_len, sha256, asset_id, filename, media_type) =
                row.map_err(|error| self.map_sqlite(error))?;
            if byte_len < 0 {
                return Err(TaskStoreError::Corrupt("artifact byte length is invalid".into()));
            }
            let artifact = ArtifactReference {
                storage_key,
                kind: ArtifactKind::parse(&kind)?,
                byte_len: u64::try_from(byte_len).map_err(|_| {
                    TaskStoreError::Corrupt("artifact byte length is invalid".into())
                })?,
                sha256,
                asset_id,
                filename,
                media_type,
            };
            validate_loaded_artifact(&artifact)?;
            total_bytes = total_bytes
                .checked_add(artifact.byte_len)
                .ok_or_else(|| TaskStoreError::Corrupt("artifact byte total overflowed".into()))?;
            if total_bytes > MAX_ARTIFACT_BYTES {
                return Err(TaskStoreError::Corrupt("task artifact bytes exceed 2 GiB".into()));
            }
            values.push(artifact);
        }
        if values.len() > MAX_ARTIFACTS {
            return Err(TaskStoreError::Corrupt("task contains too many artifacts".into()));
        }
        Ok(values)
    }

    fn nonterminal_candidates(
        &self,
        after: &str,
        high_water: &str,
    ) -> Result<Vec<(TaskId, TaskStatus, String)>, TaskStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, status, recovery_token FROM tasks WHERE id>?1 AND id<=?2 AND status IN ('pending','running','converted') ORDER BY id LIMIT 100")
            .map_err(|error| self.map_sqlite(error))?;
        let rows = statement
            .query_map(params![after, high_water], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .map_err(|error| self.map_sqlite(error))?;
        let mut values = Vec::new();
        values
            .try_reserve(100)
            .map_err(|_| TaskStoreError::Limit("reconcile batch allocation failed".into()))?;
        for row in rows {
            let (id, status, recovery_token) = row.map_err(|error| self.map_sqlite(error))?;
            require_hex(&recovery_token, 16, "recovery token")
                .map_err(|_| TaskStoreError::Corrupt("task recovery token is invalid".into()))?;
            values.push((TaskId::parse(id)?, TaskStatus::parse(&status)?, recovery_token));
        }
        Ok(values)
    }

    fn map_sqlite(&self, error: rusqlite::Error) -> TaskStoreError {
        if self.busy.cancelled.load(Ordering::Acquire) && (is_busy(&error) || is_interrupt(&error))
        {
            TaskStoreError::Cancelled
        } else if is_busy(&error) {
            TaskStoreError::BusyTimeout
        } else {
            map_sqlite_generic(error)
        }
    }
}

#[cfg(any(unix, windows))]
fn configure_busy(connection: &Connection, busy: &BusyControl) -> Result<(), TaskStoreError> {
    let _ = busy;
    connection.busy_handler(Some(sqlite_busy_handler)).map_err(map_sqlite_generic)
}

#[cfg(unix)]
fn wait_for_current_busy() -> Result<(), TaskStoreError> {
    let retry = BUSY_ATTEMPT.with(|slot| {
        slot.borrow().as_ref().is_some_and(|(attempt, _)| {
            !attempt.cancelled.load(Ordering::Acquire) && Instant::now() < attempt.deadline
        })
    });
    if retry {
        std::thread::sleep(Duration::from_millis(1));
        Ok(())
    } else {
        Err(busy_error())
    }
}

fn wait_for_busy(busy: &BusyControl, started: Instant) -> Result<(), TaskStoreError> {
    if busy.cancelled.load(Ordering::Acquire) {
        return Err(TaskStoreError::Cancelled);
    }
    if started.elapsed() >= busy.timeout {
        return Err(TaskStoreError::BusyTimeout);
    }
    std::thread::yield_now();
    Ok(())
}

#[cfg(any(unix, windows))]
fn install_limits(connection: &Connection) {
    use rusqlite::limits::Limit;
    let _ = connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_JSON_BYTES_I32);
    let _ = connection.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 1_000_000);
    let _ = connection.set_limit(Limit::SQLITE_LIMIT_COLUMN, 64);
    let _ = connection.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 100);
    let _ = connection.set_limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT, 16);
}

#[cfg(any(unix, windows))]
fn configure(connection: &Connection) -> Result<(), TaskStoreError> {
    connection
        .execute_batch(&format!(
            "PRAGMA page_size={PAGE_SIZE}; PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA trusted_schema=OFF; PRAGMA secure_delete=ON; PRAGMA temp_store=MEMORY; PRAGMA max_page_count={};",
            MAX_DATABASE_BYTES / PAGE_SIZE
        ))
        .map_err(map_sqlite_generic)?;
    let journal: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    let foreign_keys: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    let synchronous: i64 = connection
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    let trusted_schema: i64 = connection
        .query_row("PRAGMA trusted_schema", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    let secure_delete: i64 = connection
        .query_row("PRAGMA secure_delete", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    let temp_store: i64 = connection
        .query_row("PRAGMA temp_store", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    let max_page_count: i64 = connection
        .query_row("PRAGMA max_page_count", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    if !journal.eq_ignore_ascii_case("wal")
        || foreign_keys != 1
        || synchronous != 2
        || trusted_schema != 0
        || secure_delete != 1
        || temp_store != 2
        || page_size != PAGE_SIZE
        || max_page_count != MAX_DATABASE_BYTES / PAGE_SIZE
    {
        return Err(TaskStoreError::Corrupt(
            "required SQLite safety settings were not applied".into(),
        ));
    }
    let page_count: i64 = connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    if page_count.saturating_mul(PAGE_SIZE) > MAX_DATABASE_BYTES {
        return Err(TaskStoreError::Limit("database exceeds the 256 MiB limit".into()));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
#[allow(clippy::too_many_lines)]
fn migrate(connection: &Connection) -> Result<(), TaskStoreError> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    if version > SCHEMA_VERSION {
        return Err(TaskStoreError::UnsupportedVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version < 0 {
        return Err(TaskStoreError::Corrupt("negative schema version".into()));
    }
    if version == 0 {
        let transaction = connection.unchecked_transaction().map_err(map_sqlite_generic)?;
        transaction
            .execute_batch(
                "CREATE TABLE tasks(\
                   id TEXT PRIMARY KEY CHECK(length(id)=32 AND id NOT GLOB '*[^0-9a-f]*'),\
                   created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0),\
                   updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=created_at_ms),\
                   completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms>=created_at_ms),\
                   status TEXT NOT NULL CHECK(status IN ('pending','running','converted','succeeded','failed','interrupted','cancelled')),\
                   progress INTEGER NOT NULL CHECK(progress BETWEEN 0 AND 1000000 AND (status!='succeeded' OR progress=1000000)),\
                   input_fingerprint TEXT NOT NULL CHECK(length(input_fingerprint)=64 AND input_fingerprint NOT GLOB '*[^0-9a-f]*'),\
                   options_fingerprint TEXT NOT NULL CHECK(length(options_fingerprint)=64 AND options_fingerprint NOT GLOB '*[^0-9a-f]*'),\
                   recovery_token TEXT NOT NULL UNIQUE CHECK(length(recovery_token)=32 AND recovery_token NOT GLOB '*[^0-9a-f]*'),\
                   input_bytes INTEGER NOT NULL CHECK(input_bytes>=0),\
                   config_json TEXT NOT NULL CHECK(length(config_json)<=16384),\
                   artifact_generation INTEGER NOT NULL DEFAULT 0 CHECK(artifact_generation>=0),\
                   pinned INTEGER NOT NULL CHECK(pinned IN (0,1))\
                 ) STRICT;\
                 CREATE INDEX tasks_order ON tasks(updated_at_ms DESC, id DESC);\
                 CREATE TABLE diagnostics(\
                   task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,\
                   ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 63),\
                   code TEXT NOT NULL CHECK(code IN ('recoveryCheckpointMissing','recoveryCheckpointInvalid','recoveryCheckpointIncompatible','conversionFailed','cancelled')),\
                   PRIMARY KEY(task_id, ordinal)\
                 ) STRICT;\
                 CREATE TABLE artifacts(\
                   task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,\
                   storage_key TEXT NOT NULL CHECK(length(storage_key)=32 AND storage_key NOT GLOB '*[^0-9a-f]*'),\
                   kind TEXT NOT NULL CHECK(kind IN ('markdown','documentIr','diagnostics','asset','bundle')),\
                   byte_len INTEGER NOT NULL CHECK(byte_len>=0),\
                   sha256 TEXT NOT NULL CHECK(length(sha256)=64 AND sha256 NOT GLOB '*[^0-9a-f]*'),\
                   asset_id TEXT, filename TEXT, media_type TEXT,\
                   PRIMARY KEY(task_id, storage_key)\
                 ) STRICT;\
                 CREATE TRIGGER artifacts_limit BEFORE INSERT ON artifacts WHEN (SELECT count(*) FROM artifacts WHERE task_id=NEW.task_id)>=128 BEGIN SELECT RAISE(ABORT, 'artifact limit'); END;\
                 CREATE TRIGGER artifacts_terminal BEFORE INSERT ON artifacts WHEN (SELECT status FROM tasks WHERE id=NEW.task_id) IN ('failed','interrupted','cancelled') BEGIN SELECT RAISE(ABORT, 'terminal artifact'); END;\
                 PRAGMA user_version=5;",
            )
            .map_err(map_sqlite_generic)?;
        #[cfg(test)]
        if std::env::var_os("INTO_MD_TASK_STORE_MIGRATION_ABORT_CHILD").is_some() {
            std::process::abort();
        }
        transaction.commit().map_err(map_sqlite_generic)?;
    }
    if version == 1 {
        let transaction = connection.unchecked_transaction().map_err(map_sqlite_generic)?;
        transaction
            .execute_batch(
                "ALTER TABLE artifacts RENAME TO artifacts_v1;\
                 CREATE TABLE artifacts(\
                   task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,\
                   storage_key TEXT NOT NULL CHECK(length(storage_key)=32 AND storage_key NOT GLOB '*[^0-9a-f]*'),\
                   kind TEXT NOT NULL CHECK(kind IN ('markdown','documentIr','diagnostics','asset','bundle')),\
                   byte_len INTEGER NOT NULL CHECK(byte_len>=0),\
                   sha256 TEXT NOT NULL CHECK(length(sha256)=64 AND sha256 NOT GLOB '*[^0-9a-f]*'),\
                   asset_id TEXT, filename TEXT, media_type TEXT,\
                   PRIMARY KEY(task_id, storage_key)\
                 ) STRICT;\
                 INSERT INTO artifacts(task_id,storage_key,kind,byte_len,sha256) SELECT task_id,storage_key,kind,byte_len,sha256 FROM artifacts_v1;\
                 DROP TABLE artifacts_v1;\
                 CREATE TRIGGER artifacts_limit BEFORE INSERT ON artifacts WHEN (SELECT count(*) FROM artifacts WHERE task_id=NEW.task_id)>=128 BEGIN SELECT RAISE(ABORT, 'artifact limit'); END;\
                 CREATE TRIGGER artifacts_terminal BEFORE INSERT ON artifacts WHEN (SELECT status FROM tasks WHERE id=NEW.task_id) IN ('failed','interrupted','cancelled') BEGIN SELECT RAISE(ABORT, 'terminal artifact'); END;\
                 PRAGMA user_version=3;",
            )
            .map_err(map_sqlite_generic)?;
        transaction.commit().map_err(map_sqlite_generic)?;
    }
    if version == 2 {
        let transaction = connection.unchecked_transaction().map_err(map_sqlite_generic)?;
        for (phase, statement) in [
            "ALTER TABLE artifacts ADD COLUMN asset_id TEXT",
            "ALTER TABLE artifacts ADD COLUMN filename TEXT",
            "ALTER TABLE artifacts ADD COLUMN media_type TEXT",
            "PRAGMA user_version=3",
        ]
        .into_iter()
        .enumerate()
        {
            #[cfg(not(test))]
            let _ = phase;
            transaction.execute_batch(statement).map_err(map_sqlite_generic)?;
            #[cfg(test)]
            if std::env::var("INTO_MD_TASK_STORE_V2_MIGRATION_ABORT_PHASE")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                == Some(phase + 1)
            {
                std::process::abort();
            }
        }
        transaction.commit().map_err(map_sqlite_generic)?;
    }
    let current: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    if current == 3 {
        let transaction = connection.unchecked_transaction().map_err(map_sqlite_generic)?;
        transaction
            .execute_batch(
                "ALTER TABLE tasks ADD COLUMN completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms>=created_at_ms);\
                 UPDATE tasks SET completed_at_ms=updated_at_ms WHERE status IN ('succeeded','failed','interrupted','cancelled');\
                 PRAGMA user_version=4;",
            )
            .map_err(map_sqlite_generic)?;
        transaction.commit().map_err(map_sqlite_generic)?;
    }
    let current: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    if current == 4 {
        let transaction = connection.unchecked_transaction().map_err(map_sqlite_generic)?;
        transaction
            .execute_batch(
                "ALTER TABLE tasks ADD COLUMN artifact_generation INTEGER NOT NULL DEFAULT 0 CHECK(artifact_generation>=0);\
                 PRAGMA user_version=5;",
            )
            .map_err(map_sqlite_generic)?;
        transaction.commit().map_err(map_sqlite_generic)?;
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn check_schema_ceiling(connection: &Connection) -> Result<(), TaskStoreError> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    if version > SCHEMA_VERSION {
        return Err(TaskStoreError::UnsupportedVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version < 0 {
        return Err(TaskStoreError::Corrupt("negative schema version".into()));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn verify_integrity(connection: &Connection) -> Result<(), TaskStoreError> {
    let result: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(map_sqlite_generic)?;
    if result != "ok" {
        return Err(TaskStoreError::Corrupt("SQLite quick_check failed".into()));
    }
    Ok(())
}

fn validate_input(input: &InputReference) -> Result<(), TaskStoreError> {
    if input.schema_version != 1 {
        return Err(TaskStoreError::Limit("input reference schema version is unsupported".into()));
    }
    require_hex(&input.input_fingerprint, 32, "input fingerprint")?;
    require_hex(&input.options_fingerprint, 32, "options fingerprint")?;
    if i64::try_from(input.byte_len).is_err() {
        return Err(TaskStoreError::Limit("input byte length exceeds SQLite integer range".into()));
    }
    require_hex(&input.recovery_token, 16, "recovery token")
}

fn fingerprints_match(expected: &str, actual: &str) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected
        .bytes()
        .zip(actual.bytes())
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn validate_configuration(configuration: &ConfigurationSnapshot) -> Result<(), TaskStoreError> {
    if configuration.schema_version != 1 {
        return Err(TaskStoreError::Limit("configuration schema version is unsupported".into()));
    }
    Ok(())
}

fn validate_artifact(artifact: &ArtifactReference) -> Result<(), TaskStoreError> {
    require_hex(&artifact.storage_key, 16, "artifact storage key")?;
    if i64::try_from(artifact.byte_len).is_err() {
        return Err(TaskStoreError::Limit(
            "artifact byte length exceeds SQLite integer range".into(),
        ));
    }
    require_hex(&artifact.sha256, 32, "artifact SHA-256")?;
    match artifact.kind {
        ArtifactKind::Asset => {
            let fields = [
                artifact.asset_id.as_deref(),
                artifact.filename.as_deref(),
                artifact.media_type.as_deref(),
            ];
            if fields.iter().any(Option::is_none)
                || fields.into_iter().flatten().any(|value| {
                    value.is_empty() || value.len() > 255 || value.chars().any(char::is_control)
                })
            {
                return Err(TaskStoreError::Limit(
                    "asset metadata must be complete and bounded".into(),
                ));
            }
            let filename = artifact.filename.as_deref().unwrap_or_default();
            let media_type = artifact.media_type.as_deref().unwrap_or_default();
            if filename == "."
                || filename == ".."
                || filename.contains('/')
                || filename.contains('\\')
                || !media_type.is_ascii()
                || !media_type.contains('/')
                || media_type
                    .bytes()
                    .any(|byte| byte.is_ascii_uppercase() || byte.is_ascii_whitespace())
            {
                return Err(TaskStoreError::Limit(
                    "asset filename or media type is not canonical".into(),
                ));
            }
        }
        _ if artifact.asset_id.is_some()
            || artifact.filename.is_some()
            || artifact.media_type.is_some() =>
        {
            return Err(TaskStoreError::Conflict(
                "non-asset artifact cannot contain asset metadata".into(),
            ));
        }
        _ => {}
    }
    Ok(())
}

fn validate_loaded_artifact(artifact: &ArtifactReference) -> Result<(), TaskStoreError> {
    if artifact.kind == ArtifactKind::Asset
        && artifact.asset_id.is_none()
        && artifact.filename.is_none()
        && artifact.media_type.is_none()
    {
        require_hex(&artifact.storage_key, 16, "artifact storage key")?;
        if i64::try_from(artifact.byte_len).is_err() {
            return Err(TaskStoreError::Corrupt(
                "legacy artifact byte length exceeds SQLite integer range".into(),
            ));
        }
        require_hex(&artifact.sha256, 32, "artifact SHA-256")?;
        return Ok(());
    }
    validate_artifact(artifact)
}

fn validate_artifact_budget(
    transaction: &rusqlite::Transaction<'_>,
    id: &TaskId,
    additions: &[ArtifactReference],
) -> Result<(), TaskStoreError> {
    let existing: i64 = transaction
        .query_row(
            "SELECT coalesce(sum(byte_len), 0) FROM artifacts WHERE task_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .map_err(map_sqlite_generic)?;
    let existing = u64::try_from(existing)
        .map_err(|_| TaskStoreError::Corrupt("negative artifact byte total".into()))?;
    let added = additions.iter().try_fold(0_u64, |total, artifact| {
        total
            .checked_add(artifact.byte_len)
            .ok_or_else(|| TaskStoreError::Limit("artifact byte length total overflowed".into()))
    })?;
    if existing.checked_add(added).is_none_or(|total| total > MAX_ARTIFACT_BYTES) {
        return Err(TaskStoreError::Limit("task artifact bytes exceed 2 GiB".into()));
    }
    Ok(())
}

fn validate_transition(transition: &TaskTransition) -> Result<(), TaskStoreError> {
    if transition.progress_millionths > 1_000_000 {
        return Err(TaskStoreError::Limit("progress exceeds 1000000 millionths".into()));
    }
    if transition.diagnostics.len() > MAX_DIAGNOSTICS || transition.artifacts.len() > MAX_ARTIFACTS
    {
        return Err(TaskStoreError::Limit("transition child row count exceeded".into()));
    }
    if transition.next == TaskStatus::Succeeded && transition.progress_millionths != 1_000_000 {
        return Err(TaskStoreError::Conflict("succeeded tasks require complete progress".into()));
    }
    if matches!(
        transition.next,
        TaskStatus::Failed | TaskStatus::Interrupted | TaskStatus::Cancelled
    ) && !transition.artifacts.is_empty()
    {
        return Err(TaskStoreError::Conflict(
            "failed, interrupted, and cancelled transitions cannot add artifacts".into(),
        ));
    }
    for artifact in &transition.artifacts {
        validate_artifact(artifact)?;
    }
    Ok(())
}

fn bounded_json<T: Serialize>(value: &T) -> Result<String, TaskStoreError> {
    let value = serde_json::to_string(value)
        .map_err(|_| TaskStoreError::Limit("configuration or input cannot be encoded".into()))?;
    if value.len() > MAX_JSON_BYTES {
        return Err(TaskStoreError::Limit("configuration or input JSON is oversized".into()));
    }
    Ok(value)
}

fn decode_bounded<T: for<'de> Deserialize<'de>>(
    value: &str,
    label: &str,
) -> Result<T, TaskStoreError> {
    if value.len() > MAX_JSON_BYTES {
        return Err(TaskStoreError::Corrupt(format!("persisted {label} JSON is oversized")));
    }
    serde_json::from_str(value)
        .map_err(|_| TaskStoreError::Corrupt(format!("persisted {label} JSON is invalid")))
}

fn require_hex(value: &str, bytes: usize, label: &str) -> Result<(), TaskStoreError> {
    if value.len() != bytes * 2
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(TaskStoreError::Limit(format!("{label} is not canonical")));
    }
    Ok(())
}

fn random_id() -> Result<TaskId, TaskStoreError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; ID_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| TaskStoreError::Io(format!("generate task id: {error}")))?;
    let mut encoded = [0_u8; ID_BYTES * 2];
    for (index, byte) in bytes.iter().copied().enumerate() {
        encoded[index * 2] = HEX[usize::from(byte >> 4)];
        encoded[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
    }
    let mut value = String::new();
    value
        .try_reserve_exact(encoded.len())
        .map_err(|_| TaskStoreError::Limit("task id allocation failed".into()))?;
    for byte in encoded {
        value.push(char::from(byte));
    }
    TaskId::parse(value)
}

fn utc_now_ms() -> Result<i64, TaskStoreError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TaskStoreError::Io("system clock precedes the Unix epoch".into()))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| TaskStoreError::Io("system clock cannot be represented".into()))
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn is_interrupt(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::OperationInterrupted
    )
}

#[cfg(any(unix, windows))]
fn map_sqlite_open(error: rusqlite::Error) -> TaskStoreError {
    map_sqlite_generic(error)
}

#[allow(clippy::needless_pass_by_value)]
fn map_sqlite_generic(error: rusqlite::Error) -> TaskStoreError {
    match &error {
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::SchemaChanged
            ) =>
        {
            TaskStoreError::Corrupt("SQLite rejected the database structure".into())
        }
        _ if is_busy(&error) || is_interrupt(&error) => busy_error(),
        _ => TaskStoreError::Io(error.to_string()),
    }
}

#[derive(Debug)]
struct SafeDirectory {
    path: PathBuf,
    #[cfg(unix)]
    fd: rustix::fd::OwnedFd,
    #[cfg(unix)]
    identity: (u64, u64),
}

#[cfg(unix)]
impl SafeDirectory {
    fn open_or_create(path: PathBuf) -> Result<Self, TaskStoreError> {
        let path = resolved_absolute(path)?;
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(path_io)?;
        for component in path.components() {
            let Component::Normal(name) = component else { continue };
            fd = match rustix::fs::openat(
                &fd,
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            ) {
                Ok(opened) => opened,
                Err(rustix::io::Errno::NOENT) => {
                    rustix::fs::mkdirat(
                        &fd,
                        name,
                        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
                    )
                    .map_err(path_io)?;
                    rustix::fs::fsync(&fd).map_err(path_io)?;
                    rustix::fs::openat(
                        &fd,
                        name,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(path_io)?
                }
                Err(error) => return Err(path_io(error)),
            };
        }
        let identity = private_directory_identity(&fd)?;
        Ok(Self { path, fd, identity })
    }

    fn verify_namespace(&self) -> Result<(), TaskStoreError> {
        if private_directory_identity(&self.fd)? != self.identity {
            return Err(TaskStoreError::UnsafePath("retained directory identity changed".into()));
        }
        let reopened = Self::open_existing(&self.path)?;
        if reopened.identity != self.identity {
            return Err(TaskStoreError::UnsafePath("directory namespace was replaced".into()));
        }
        Ok(())
    }

    fn open_existing(path: &Path) -> Result<Self, TaskStoreError> {
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(path_io)?;
        for component in path.components() {
            let Component::Normal(name) = component else { continue };
            fd = rustix::fs::openat(
                &fd,
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(path_io)?;
        }
        let identity = private_directory_identity(&fd)?;
        Ok(Self { path: path.to_path_buf(), fd, identity })
    }

    fn prepare_database_file(&self, name: &str) -> Result<(), TaskStoreError> {
        self.verify_namespace()?;
        match rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        ) {
            Ok(file) => {
                rustix::fs::fsync(&file).map_err(path_io)?;
                self.sync()?;
            }
            Err(rustix::io::Errno::EXIST) => self.verify_regular_private(name)?,
            Err(error) => return Err(path_io(error)),
        }
        self.verify_namespace()
    }

    fn create_private_file(&self, name: &str) -> Result<(), TaskStoreError> {
        self.verify_namespace()?;
        let file = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                TaskStoreError::Conflict("backup destination already exists".into())
            } else {
                path_io(error)
            }
        })?;
        rustix::fs::fsync(&file).map_err(path_io)?;
        self.sync()
    }

    fn publish_verified_link(
        &self,
        source: &str,
        target: &str,
        expected: (u64, u64),
    ) -> Result<(), TaskStoreError> {
        if self.regular_private_identity(source)? != expected {
            return Err(TaskStoreError::UnsafePath("backup source identity changed".into()));
        }
        #[cfg(test)]
        if INJECT_PUBLISH_SOURCE_SWAP.with(|flag| flag.replace(false)) {
            rustix::fs::renameat(&self.fd, source, &self.fd, ".publish-test-original")
                .map_err(path_io)?;
            self.create_private_file(source)?;
        }
        rustix::fs::linkat(&self.fd, source, &self.fd, target, rustix::fs::AtFlags::empty())
            .map_err(|error| {
                if error == rustix::io::Errno::EXIST {
                    TaskStoreError::Conflict("backup destination already exists".into())
                } else {
                    path_io(error)
                }
            })?;
        let published = self.regular_private_identity(target)?;
        if published != expected {
            self.unlink_if_identity(target, published)?;
            return Err(TaskStoreError::UnsafePath(
                "backup source changed during publication".into(),
            ));
        }
        if self.regular_private_identity(source)? != expected {
            self.unlink_if_identity(target, expected)?;
            return Err(TaskStoreError::UnsafePath(
                "backup source changed during publication".into(),
            ));
        }
        Ok(())
    }

    fn verify_or_remove_published(
        &self,
        name: &str,
        expected: (u64, u64),
    ) -> Result<(), TaskStoreError> {
        let actual = self.regular_private_identity(name)?;
        if actual == expected {
            return Ok(());
        }
        self.unlink_if_identity(name, actual)?;
        self.sync()?;
        Err(TaskStoreError::UnsafePath("published backup identity changed".into()))
    }

    fn unlink_if_identity(&self, name: &str, expected: (u64, u64)) -> Result<(), TaskStoreError> {
        if self.regular_private_identity(name)? != expected {
            return Err(TaskStoreError::UnsafePath("temporary file identity changed".into()));
        }
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty()).map_err(path_io)
    }

    fn sync_file_if_identity(
        &self,
        name: &str,
        expected: (u64, u64),
    ) -> Result<(), TaskStoreError> {
        let file = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(path_io)?;
        let stat = rustix::fs::fstat(&file).map_err(path_io)?;
        let identity = (
            u64::try_from(stat.st_dev)
                .map_err(|_| TaskStoreError::UnsafePath("invalid file device".into()))?,
            stat.st_ino,
        );
        if identity != expected {
            return Err(TaskStoreError::UnsafePath("temporary file identity changed".into()));
        }
        rustix::fs::fsync(&file).map_err(path_io)
    }

    fn verify_database_files(&self, database_identity: (u64, u64)) -> Result<(), TaskStoreError> {
        self.verify_namespace()?;
        if self.regular_private_identity(DATABASE_FILE)? != database_identity {
            return Err(TaskStoreError::UnsafePath("primary database identity changed".into()));
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let name = format!("{DATABASE_FILE}{suffix}");
            match self.verify_regular_private(&name) {
                Ok(()) => {}
                Err(TaskStoreError::Io(message)) if message.contains("No such file") => {}
                Err(error) => return Err(error),
            }
        }
        self.verify_namespace()
    }

    fn verify_regular_private(&self, name: &str) -> Result<(), TaskStoreError> {
        self.regular_private_identity(name).map(|_| ())
    }

    fn regular_private_identity(&self, name: &str) -> Result<(u64, u64), TaskStoreError> {
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(path_io)?;
        let stat = rustix::fs::fstat(&fd).map_err(path_io)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o077 != 0
        {
            return Err(TaskStoreError::UnsafePath(format!(
                "managed file {name} is not private regular storage"
            )));
        }
        let size = u64::try_from(stat.st_size)
            .map_err(|_| TaskStoreError::Corrupt("managed file size is invalid".into()))?;
        let maximum = u64::try_from(MAX_DATABASE_BYTES + 1024 * 1024).unwrap_or(u64::MAX);
        if size > maximum {
            return Err(TaskStoreError::Limit("managed SQLite file exceeds 257 MiB".into()));
        }
        Ok((
            u64::try_from(stat.st_dev)
                .map_err(|_| TaskStoreError::UnsafePath("invalid file device".into()))?,
            stat.st_ino,
        ))
    }

    fn sync(&self) -> Result<(), TaskStoreError> {
        rustix::fs::fsync(&self.fd).map_err(path_io)
    }
}

#[cfg(windows)]
impl SafeDirectory {
    fn open_or_create_windows(path: PathBuf) -> Result<Self, TaskStoreError> {
        let path = resolved_absolute(path)?;
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                into_markdown_process_plugin::create_windows_plugin_store_directory(&path)
                    .map_err(|error| TaskStoreError::UnsafePath(error.to_string()))?;
            }
            Err(error) => return Err(TaskStoreError::Io(error.to_string())),
        }
        into_markdown_process_plugin::verify_windows_plugin_store_path(&path)
            .map_err(|error| TaskStoreError::UnsafePath(error.to_string()))?;
        Ok(Self { path })
    }

    fn prepare_database_file_windows(&self, name: &str) -> Result<(u64, u64), TaskStoreError> {
        let path = self.path.join(name);
        match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
            Ok(file) => file.sync_all().map_err(|error| TaskStoreError::Io(error.to_string()))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(TaskStoreError::Io(error.to_string())),
        }
        self.private_file_identity_windows(name)
    }

    fn private_file_identity_windows(&self, name: &str) -> Result<(u64, u64), TaskStoreError> {
        use std::os::windows::fs::OpenOptionsExt as _;
        let path = self.path.join(name);
        into_markdown_process_plugin::verify_windows_plugin_store_child(&path)
            .map_err(|error| TaskStoreError::UnsafePath(error.to_string()))?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .custom_flags(0x0020_0000)
            .open(&path)
            .map_err(|error| TaskStoreError::Io(error.to_string()))?;
        let information = winapi_util::file::information(&file)
            .map_err(|error| TaskStoreError::Io(error.to_string()))?;
        if !file.metadata().map_err(|error| TaskStoreError::Io(error.to_string()))?.is_file()
            || information.file_attributes() & 0x400 != 0
            || information.number_of_links() != 1
        {
            return Err(TaskStoreError::UnsafePath(
                "SQLite authority is not a singly linked physical file".into(),
            ));
        }
        Ok((information.volume_serial_number(), information.file_index()))
    }

    fn verify_database_files_windows(
        &self,
        database_identity: (u64, u64),
    ) -> Result<(), TaskStoreError> {
        if self.private_file_identity_windows(DATABASE_FILE)? != database_identity {
            return Err(TaskStoreError::UnsafePath("SQLite database identity changed".into()));
        }
        for name in [format!("{DATABASE_FILE}-wal"), format!("{DATABASE_FILE}-shm")] {
            match std::fs::symlink_metadata(self.path.join(&name)) {
                Ok(_) => {
                    let _ = self.private_file_identity_windows(&name)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(TaskStoreError::Io(error.to_string())),
            }
        }
        into_markdown_process_plugin::verify_windows_plugin_store_path(&self.path)
            .map_err(|error| TaskStoreError::UnsafePath(error.to_string()))
    }
}

#[cfg(unix)]
fn private_directory_identity(fd: &rustix::fd::OwnedFd) -> Result<(u64, u64), TaskStoreError> {
    let stat = rustix::fs::fstat(fd).map_err(path_io)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o077 != 0
    {
        return Err(TaskStoreError::UnsafePath(
            "store root must be a private directory owned by the effective user".into(),
        ));
    }
    Ok((
        u64::try_from(stat.st_dev)
            .map_err(|_| TaskStoreError::UnsafePath("invalid directory device".into()))?,
        stat.st_ino,
    ))
}

#[cfg(unix)]
fn resolved_absolute(path: PathBuf) -> Result<PathBuf, TaskStoreError> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map_err(|error| TaskStoreError::Io(error.to_string()))?.join(path)
    };
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(TaskStoreError::UnsafePath(
                    "store root must be absolute and normalized".into(),
                ));
            }
        }
    }
    if normalized == Path::new("/") {
        return Err(TaskStoreError::UnsafePath("filesystem root is not a store".into()));
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&normalized)
        && metadata.file_type().is_symlink()
    {
        return Err(TaskStoreError::UnsafePath("store root cannot be a symbolic link".into()));
    }
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| TaskStoreError::UnsafePath("store has no existing ancestor".into()))?;
        missing
            .try_reserve(1)
            .map_err(|_| TaskStoreError::Limit("path component allocation failed".into()))?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| TaskStoreError::UnsafePath("store has no existing ancestor".into()))?;
    }
    let mut resolved =
        std::fs::canonicalize(existing).map_err(|error| TaskStoreError::Io(error.to_string()))?;
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

#[cfg(windows)]
fn resolved_absolute(path: PathBuf) -> Result<PathBuf, TaskStoreError> {
    use std::os::windows::fs::MetadataExt as _;
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map_err(|error| TaskStoreError::Io(error.to_string()))?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                return Err(TaskStoreError::UnsafePath(
                    "store root must be absolute and normalized".into(),
                ));
            }
        }
    }
    if !normalized.is_absolute() || normalized.parent().is_none() {
        return Err(TaskStoreError::UnsafePath("volume root is not a store".into()));
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&normalized)
        && metadata.file_attributes() & 0x400 != 0
    {
        return Err(TaskStoreError::UnsafePath("store root cannot be a reparse point".into()));
    }
    Ok(normalized)
}

#[cfg(unix)]
fn path_io(error: rustix::io::Errno) -> TaskStoreError {
    if error == rustix::io::Errno::LOOP {
        TaskStoreError::UnsafePath("symbolic links are denied".into())
    } else {
        TaskStoreError::Io(error.to_string())
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, not(any(unix, windows))))]
mod non_unix_tests {
    use super::{BusyControl, TaskStore, TaskStoreError};

    #[test]
    fn open_fails_closed_before_creating_storage() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("must-not-be-created");

        let result = TaskStore::open(&root, BusyControl::default());

        assert!(matches!(result, Err(TaskStoreError::PlatformUnavailable(_))));
        assert!(!root.exists());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{BusyControl, TaskStore};

    #[test]
    fn open_creates_a_private_identity_bound_store() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("windows-store");
        let store = TaskStore::open(&root, BusyControl::default()).unwrap();
        assert!(store.list(1, None).unwrap().is_empty());
        into_markdown_process_plugin::verify_windows_plugin_store_path(&root).unwrap();
        into_markdown_process_plugin::verify_windows_plugin_store_child(
            &root.join("tasks.sqlite3"),
        )
        .unwrap();
    }
}
