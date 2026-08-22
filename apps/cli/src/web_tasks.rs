//! Durable upload, conversion queue, and artifact publication for the local Web service.

use crate::output;
use crate::transaction::SafeDir;
use into_markdown::{
    AiMode, ArtifactKind, ArtifactReference, AsrOptions, BusyControl, CancellationToken,
    ConfigurationSnapshot, ConversionOptions, ConversionRequest, DiagnosticCode, Engine,
    ExecutionOptions, FormatHint, InputFormat, InputRef, InputReference, NewTask, OcrPolicy,
    ProgressEvent, ProgressListener, RecoveryStore, RecoveryToken, TaskCursor, TaskDiagnostic,
    TaskId, TaskRecord, TaskStatus, TaskStore, TaskStoreError, TaskTransition,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
#[cfg(test)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const STORE_METADATA_HEADROOM: u64 = 4 * 1024 * 1024;
const STORE_MUTATION_RESERVATION: u64 = 1024 * 1024;
const MAX_NAME_BYTES: usize = 255;
const MAX_QUEUE: usize = 100_000;
const MAX_WORKERS: usize = 4;
const EVENT_REPLAY_CAPACITY: usize = 64;
const EVENT_BROADCAST_CAPACITY: usize = 128;
const MAX_ALLOWED_HOSTS: usize = 64;
const COPY_CHUNK: usize = 64 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_WEB_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_WEB_TEMPORARY_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_TASK_DURABLE_GROWTH: u64 = 512 * 1024 * 1024;
const MAX_TASK_METADATA_GROWTH: u64 = 4 * 1024 * 1024;
const MAX_TASK_TOTAL_DURABLE_GROWTH: u64 =
    2 * MAX_CHECKPOINT_BYTES + MAX_TASK_DURABLE_GROWTH + MAX_TASK_METADATA_GROWTH;
const DEFAULT_RETENTION_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_RETENTION_BYTES: u64 = 10 * 1024 * 1024 * 1024;
// Retained history plus one conservative durable-growth reservation per worker
// and an isolated SQLite metadata reserve. This keeps the 10 GiB policy
// reachable without allowing concurrent work to cross the managed ceiling.
const MAX_GLOBAL_BYTES: u64 = DEFAULT_RETENTION_BYTES
    + (MAX_WORKERS as u64 * MAX_TASK_TOTAL_DURABLE_GROWTH)
    + STORE_METADATA_HEADROOM;
const MAX_DATA_BYTES: u64 = MAX_GLOBAL_BYTES - STORE_METADATA_HEADROOM;
const _: () = assert!(DEFAULT_RETENTION_BYTES <= MAX_DATA_BYTES);
#[cfg(test)]
thread_local! {
    static SWAP_ROOT_AFTER_TASK_STORE_OPEN: Cell<bool> = const { Cell::new(false) };
}

/// Stable backend error categories exposed by the HTTP adapter.
#[derive(Debug, thiserror::Error)]
pub enum WebTaskError {
    #[error("unsafe managed storage: {0}")]
    Unsafe(String),
    #[error("resource limit exceeded: {0}")]
    Limit(String),
    #[error("task was cancelled")]
    Cancelled,
    #[error("task was not found")]
    NotFound,
    #[error("task state conflict: {0}")]
    Conflict(String),
    #[error("invalid task request: {0}")]
    Invalid(String),
    #[error("backend I/O failed: {0}")]
    Io(String),
}

/// One-time grants accompanying a Web upload. Grants are checked before task
/// creation and deliberately excluded from the persisted request descriptor.
#[derive(Clone, Debug, Default, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WebTaskAuthorization {
    pub(crate) network: bool,
    pub(crate) private_network: bool,
    pub(crate) provider: bool,
}

/// Product surface that created a browser task. This is persisted with the
/// authenticated descriptor so document conversions and meeting transcripts
/// can share one durable queue without mixing their histories.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WebWorkflow {
    #[default]
    Conversion,
    MeetingTranscript,
}

/// Versioned browser request using the same `FormatHint` and
/// `ConversionOptions` types as the CLI and Engine.
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WebTaskRequest {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) workflow: WebWorkflow,
    pub(crate) format: Option<InputFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) batch_id: Option<String>,
    pub(crate) options: ConversionOptions,
    pub(crate) authorization: WebTaskAuthorization,
}

impl Default for WebTaskRequest {
    fn default() -> Self {
        Self {
            schema_version: 1,
            workflow: WebWorkflow::Conversion,
            format: None,
            batch_id: None,
            options: web_options(),
            authorization: WebTaskAuthorization::default(),
        }
    }
}

pub(crate) fn decode_web_task_request(bytes: &[u8]) -> Result<WebTaskRequest, WebTaskError> {
    if bytes.len() > 16 * 1024 {
        return Err(WebTaskError::Limit("task request exceeds 16 KiB".into()));
    }
    validate_json_shape(bytes, 16, 512, 4 * 1024)?;
    let request: WebTaskRequest = serde_json::from_slice(bytes)
        .map_err(|_| WebTaskError::Invalid("task request JSON is invalid".into()))?;
    validate_web_task_request(&request)?;
    Ok(request)
}

fn validate_web_task_request(request: &WebTaskRequest) -> Result<(), WebTaskError> {
    validate_web_task_identity(request)?;
    let options = &request.options;
    if !options.ocr.minimum_confidence.is_finite()
        || !(0.0..=1.0).contains(&options.ocr.minimum_confidence)
    {
        return Err(WebTaskError::Invalid("OCR confidence must be within 0 and 1".into()));
    }
    validate_web_asr_options(&options.asr)?;
    let diarization = &options.diarization;
    if !(1..=64).contains(&diarization.max_speakers)
        || diarization
            .expected_speakers
            .is_some_and(|expected| expected == 0 || expected > diarization.max_speakers)
        || !diarization.enabled && diarization.expected_speakers.is_some()
    {
        return Err(WebTaskError::Invalid("unsupported Web diarization policy".into()));
    }
    let limits = &options.limits;
    if limits.max_input_bytes == 0 || limits.max_input_bytes > MAX_FILE_BYTES {
        return Err(WebTaskError::Invalid("max_input_bytes must be within 1 and 512 MiB".into()));
    }
    if limits.max_memory_bytes == 0 || limits.max_memory_bytes > MAX_WEB_MEMORY_BYTES {
        return Err(WebTaskError::Invalid("max_memory_bytes exceeds the Web profile".into()));
    }
    if limits.max_temporary_bytes == 0 || limits.max_temporary_bytes > MAX_WEB_TEMPORARY_BYTES {
        return Err(WebTaskError::Invalid("max_temporary_bytes exceeds the Web profile".into()));
    }
    if limits.max_asset_bytes > 64 * 1024 * 1024
        || limits.max_total_asset_bytes > 128 * 1024 * 1024
        || limits.max_pages == 0
        || limits.max_pages > 10_000
        || limits.max_decompressed_bytes == 0
        || limits.max_decompressed_bytes > 1024 * 1024 * 1024
        || limits.max_archive_entries == 0
        || limits.max_archive_entries > 100_000
        || limits.max_archive_depth == 0
        || limits.max_archive_depth > 16
        || limits.max_archive_entry_bytes == 0
        || limits.max_archive_entry_bytes > 256 * 1024 * 1024
        || limits.max_archive_compression_ratio == 0
        || limits.max_archive_compression_ratio > 100
        || limits.max_nesting_depth == 0
        || limits.max_nesting_depth > 256
        || limits.max_table_rows == 0
        || limits.max_table_rows > 100_000
        || limits.max_table_columns == 0
        || limits.max_table_columns > 16_384
        || limits.max_table_cells == 0
        || limits.max_table_cells > 1_000_000
        || limits.max_field_bytes == 0
        || limits.max_field_bytes > 16 * 1024 * 1024
        || limits.max_feed_entries == 0
        || limits.max_feed_entries > 10_000
        || limits.max_feed_text_bytes == 0
        || limits.max_feed_text_bytes > 64 * 1024 * 1024
        || limits.max_feed_html_bytes == 0
        || limits.max_feed_html_bytes > 64 * 1024 * 1024
    {
        return Err(WebTaskError::Invalid("resource limits exceed the Web profile".into()));
    }
    if options.output.flavor != "gfm"
        || options.output.asset_directory_suffix != "_assets"
        || options.output.asset_uri_prefix.is_some()
    {
        return Err(WebTaskError::Invalid("unsupported Web output policy".into()));
    }
    if options.network.max_redirects > 3
        || options.network.allowed_hosts.len() > MAX_ALLOWED_HOSTS
        || options.network.allowed_hosts.iter().any(|host| {
            host.is_empty()
                || host.len() > 253
                || !host.is_ascii()
                || host.contains(['/', ':', '@', '?', '#'])
        })
    {
        return Err(WebTaskError::Invalid("network host allowlist is invalid".into()));
    }
    if options.network.enabled && !request.authorization.network {
        return Err(WebTaskError::Invalid("network access lacks this-upload authorization".into()));
    }
    if !options.network.deny_private_networks
        && (!options.network.enabled || !request.authorization.private_network)
    {
        return Err(WebTaskError::Invalid(
            "private-network access lacks this-upload authorization".into(),
        ));
    }
    let ai = &options.ai;
    let uses_ai = [
        ai.vision_ocr,
        ai.image_description,
        ai.layout_repair,
        ai.table_repair,
        ai.formula_repair,
        ai.markdown_postprocess,
    ]
    .into_iter()
    .any(|mode| mode != AiMode::Off);
    if uses_ai && !request.authorization.provider {
        return Err(WebTaskError::Invalid(
            "AI use lacks this-upload Provider authorization".into(),
        ));
    }
    if request.workflow == WebWorkflow::MeetingTranscript
        && (!matches!(request.format, Some(InputFormat::Audio | InputFormat::Video))
            || ai.audio_transcription == AiMode::Off
            || options.ocr.policy != OcrPolicy::Off
            || ai.vision_ocr != AiMode::Off
            || ai.image_description != AiMode::Off
            || ai.layout_repair != AiMode::Off
            || ai.table_repair != AiMode::Off
            || ai.formula_repair != AiMode::Off
            || ai.markdown_postprocess != AiMode::Off)
    {
        return Err(WebTaskError::Invalid("meeting transcript policy is inconsistent".into()));
    }
    Ok(())
}

fn validate_web_task_identity(request: &WebTaskRequest) -> Result<(), WebTaskError> {
    if request.schema_version != 1 {
        return Err(WebTaskError::Invalid("task request schemaVersion must be 1".into()));
    }
    if request.batch_id.as_deref().is_some_and(|value| !valid_batch_id(value)) {
        return Err(WebTaskError::Invalid(
            "batchId must be 32 lowercase hexadecimal characters".into(),
        ));
    }
    Ok(())
}

fn valid_batch_id(value: &str) -> bool {
    value.len() == 32
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !value.bytes().any(|byte| byte.is_ascii_uppercase())
}

fn validate_web_asr_options(options: &AsrOptions) -> Result<(), WebTaskError> {
    if options.language.as_ref().is_none_or(|language| {
        !language.is_empty()
            && language.len() <= 35
            && language.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) && (1..=8).contains(&options.max_threads)
        && options.max_duration_ms != Some(0)
        && (1..=100_000).contains(&options.max_segments)
        && (256 * 1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&options.max_native_memory_bytes)
    {
        Ok(())
    } else {
        Err(WebTaskError::Invalid("unsupported Web ASR policy".into()))
    }
}

impl From<std::io::Error> for WebTaskError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<TaskStoreError> for WebTaskError {
    fn from(error: TaskStoreError) -> Self {
        match error {
            TaskStoreError::Limit(detail) => Self::Limit(detail),
            TaskStoreError::Conflict(detail) => Self::Conflict(detail),
            TaskStoreError::Cancelled => Self::Cancelled,
            TaskStoreError::UnsafePath(detail) => Self::Unsafe(detail),
            other => Self::Io(other.to_string()),
        }
    }
}

#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedRequest {
    schema_version: u32,
    #[serde(default)]
    workflow: WebWorkflow,
    name: String,
    hint: FormatHint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    batch_id: Option<String>,
    options: ConversionOptions,
}

/// Browser-facing task metadata recovered from the authenticated request file.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebTaskRecord {
    #[serde(flatten)]
    pub(crate) record: TaskRecord,
    pub(crate) workflow: WebWorkflow,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) format: Option<InputFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) batch_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpeakerLabels {
    pub(crate) schema_version: u32,
    pub(crate) artifact_generation: u64,
    pub(crate) speakers: Vec<SpeakerLabel>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpeakerLabel {
    pub(crate) id: String,
    pub(crate) name: String,
}

#[derive(Debug)]
struct Job {
    id: TaskId,
    token: RecoveryToken,
    request: PersistedRequest,
    cancellation: CancellationToken,
    admission_ticket: Option<u64>,
}

#[derive(Default)]
struct QueueState {
    jobs: VecDeque<Job>,
    cancellations: BTreeMap<TaskId, CancellationToken>,
    stopped: bool,
}

struct Shared {
    root_handle: SafeDir,
    objects: SafeDir,
    incoming: SafeDir,
    snapshots: SafeDir,
    trash: SafeDir,
    task_store: Mutex<TaskStore>,
    history_mutation: Mutex<()>,
    recovery: RecoveryStore,
    engine: Engine,
    media_services: crate::services::WebMediaServiceCache,
    events: EventHub,
    queue: Mutex<QueueState>,
    queue_changed: Condvar,
    disk_bytes: Mutex<DiskQuota>,
    disk_changed: Condvar,
    write_failure_after: AtomicUsize,
    #[cfg(test)]
    active_workers: AtomicUsize,
    #[cfg(test)]
    max_active_workers: AtomicUsize,
    #[cfg(test)]
    dequeue_order: Mutex<Vec<TaskId>>,
    #[cfg(test)]
    admission_order: Mutex<Vec<TaskId>>,
    #[cfg(test)]
    disk_waiters: AtomicUsize,
    #[cfg(test)]
    disk_ticket_admissions: Mutex<Vec<u64>>,
    #[cfg(test)]
    conversion_gate: Mutex<Option<Arc<std::sync::Barrier>>>,
    #[cfg(test)]
    conversion_entries: AtomicUsize,
    #[cfg(test)]
    pre_acquire_gate: Mutex<Option<Arc<std::sync::Barrier>>>,
    #[cfg(test)]
    pre_acquire_panic: AtomicUsize,
    #[cfg(test)]
    snapshot_failure: AtomicUsize,
    #[cfg(test)]
    reconcile_failures: AtomicUsize,
    #[cfg(test)]
    fail_transition_failures: AtomicUsize,
    #[cfg(test)]
    dequeue_get_failures: AtomicUsize,
    #[cfg(test)]
    dequeue_transition_failures: AtomicUsize,
    #[cfg(test)]
    success_transition_failures: AtomicUsize,
    #[cfg(test)]
    retention_failure: AtomicUsize,
    #[cfg(test)]
    retention_quarantine_gate: Mutex<Option<Arc<std::sync::Barrier>>>,
    #[cfg(test)]
    history_scan_gate: Mutex<Option<Arc<std::sync::Barrier>>>,
    publication_failure: AtomicUsize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RetentionPolicy {
    pub(crate) max_age: Duration,
    pub(crate) max_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self { max_age: DEFAULT_RETENTION_AGE, max_bytes: DEFAULT_RETENTION_BYTES }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CleanupSummary {
    pub(crate) schema_version: u32,
    pub(crate) deleted_tasks: u32,
    pub(crate) reclaimed_bytes: u64,
}

impl Default for CleanupSummary {
    fn default() -> Self {
        Self { schema_version: 1, deleted_tasks: 0, reclaimed_bytes: 0 }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TaskHistoryPage {
    pub(crate) tasks: Vec<TaskRecord>,
    pub(crate) next: Option<TaskCursor>,
}

/// Version 1 task-event wire DTO. Event IDs are process-generation scoped;
/// `sequence` is monotonic within one task and generation.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskEventDto {
    pub(crate) schema_version: u32,
    pub(crate) sequence: u64,
    pub(crate) task_id: TaskId,
    pub(crate) kind: TaskEventKind,
    pub(crate) status: TaskStatus,
    pub(crate) progress_millionths: u32,
    pub(crate) terminal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) execution: Option<ProgressEvent>,
    #[serde(skip)]
    pub(crate) event_id: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TaskEventKind {
    Snapshot,
    Progress,
}

struct TaskEventLog {
    next_sequence: u64,
    terminal: bool,
    latest_record: TaskRecord,
    events: VecDeque<Arc<TaskEventDto>>,
}

struct EventHubState {
    logs: BTreeMap<TaskId, TaskEventLog>,
}

struct EventHub {
    generation: String,
    state: Mutex<EventHubState>,
    sender: broadcast::Sender<Arc<TaskEventDto>>,
}

pub(crate) struct TaskEventSubscription {
    pub(crate) replay: VecDeque<Arc<TaskEventDto>>,
    pub(crate) receiver: broadcast::Receiver<Arc<TaskEventDto>>,
}

impl EventHub {
    fn new(generation: String) -> Self {
        let (sender, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Self { generation, state: Mutex::new(EventHubState { logs: BTreeMap::new() }), sender }
    }

    fn event(
        &self,
        log: &mut TaskEventLog,
        record: &TaskRecord,
        kind: TaskEventKind,
        progress_millionths: u32,
        execution: Option<ProgressEvent>,
    ) -> Arc<TaskEventDto> {
        let sequence = log.next_sequence;
        log.next_sequence = log.next_sequence.saturating_add(1);
        Arc::new(TaskEventDto {
            schema_version: 1,
            sequence,
            task_id: record.id.clone(),
            kind,
            status: record.status,
            progress_millionths,
            terminal: is_terminal(record.status),
            execution,
            event_id: format!("{}:{sequence}", self.generation),
        })
    }

    fn append(&self, log: &mut TaskEventLog, event: Arc<TaskEventDto>) {
        log.terminal |= event.terminal;
        if log.events.len() == EVENT_REPLAY_CAPACITY {
            log.events.pop_front();
        }
        log.events.push_back(Arc::clone(&event));
        // `broadcast::Sender::send` never waits for receivers. A lagging or
        // closed browser is repaired from the bounded log/current snapshot.
        let _ = self.sender.send(event);
    }

    fn restart(&self, record: &TaskRecord) {
        let mut state = lock(&self.state);
        let next_sequence = state.logs.get(&record.id).map_or(1, |log| log.next_sequence);
        state.logs.insert(
            record.id.clone(),
            TaskEventLog {
                next_sequence,
                terminal: false,
                latest_record: record.clone(),
                events: VecDeque::new(),
            },
        );
    }

    fn publish_snapshot(&self, record: &TaskRecord) {
        let mut state = lock(&self.state);
        let log = state.logs.entry(record.id.clone()).or_insert_with(|| TaskEventLog {
            next_sequence: 1,
            terminal: false,
            latest_record: record.clone(),
            events: VecDeque::new(),
        });
        if record.updated_at_ms < log.latest_record.updated_at_ms
            || (log.terminal && !is_terminal(record.status))
        {
            return;
        }
        log.latest_record = record.clone();
        let event =
            self.event(log, record, TaskEventKind::Snapshot, record.progress_millionths, None);
        self.append(log, event);
    }

    fn publish_progress(&self, record: &TaskRecord, progress: ProgressEvent) {
        let mut state = lock(&self.state);
        let log = state.logs.entry(record.id.clone()).or_insert_with(|| TaskEventLog {
            next_sequence: 1,
            terminal: false,
            latest_record: record.clone(),
            events: VecDeque::new(),
        });
        if log.terminal {
            return;
        }
        let progress_millionths = u32::from(progress.basis_points) * 100;
        let event =
            self.event(log, record, TaskEventKind::Progress, progress_millionths, Some(progress));
        self.append(log, event);
    }

    fn subscribe(&self, record: &TaskRecord, cursor: Option<(&str, u64)>) -> TaskEventSubscription {
        let mut state = lock(&self.state);
        let log = state.logs.entry(record.id.clone()).or_insert_with(|| TaskEventLog {
            next_sequence: 1,
            terminal: false,
            latest_record: record.clone(),
            events: VecDeque::new(),
        });
        if record.updated_at_ms > log.latest_record.updated_at_ms {
            log.latest_record = record.clone();
        }
        let replayable = cursor.is_some_and(|(generation, sequence)| {
            generation == self.generation
                && sequence < log.next_sequence
                && log
                    .events
                    .front()
                    .is_none_or(|first| sequence.saturating_add(1) >= first.sequence)
        });
        let replay = if replayable {
            let sequence = cursor.map_or(0, |(_, sequence)| sequence);
            log.events.iter().filter(|event| event.sequence > sequence).cloned().collect()
        } else {
            let current = log.latest_record.clone();
            let event = self.event(
                log,
                &current,
                TaskEventKind::Snapshot,
                current.progress_millionths,
                None,
            );
            self.append(log, Arc::clone(&event));
            VecDeque::from([event])
        };
        // Subscribe while the hub lock is still held, after any reset snapshot
        // was appended. This avoids both a publication race and delivering the
        // reset event twice (from replay and from broadcast).
        let receiver = self.sender.subscribe();
        TaskEventSubscription { replay, receiver }
    }
}

struct EventProgressListener {
    shared: Weak<Shared>,
    id: TaskId,
}

impl ProgressListener for EventProgressListener {
    fn on_progress(&self, event: ProgressEvent) {
        let Some(shared) = self.shared.upgrade() else { return };
        let Ok(Some(record)) = lock(&shared.task_store).get(&self.id) else { return };
        if !is_terminal(record.status) {
            shared.events.publish_progress(&record, event);
        }
    }
}

fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Succeeded
            | TaskStatus::Failed
            | TaskStatus::Interrupted
            | TaskStatus::Cancelled
    )
}

struct DiskQuota {
    used: u64,
    reserved: u64,
    next_ticket: u64,
    waiters: VecDeque<u64>,
}

struct DiskLease<'a> {
    shared: &'a Shared,
    amount: u64,
}

struct RegisteredDiskWaiter<'a> {
    shared: &'a Shared,
    ticket: Option<u64>,
}

impl RegisteredDiskWaiter<'_> {
    fn consume(&mut self) {
        self.ticket = None;
    }
}

impl Drop for RegisteredDiskWaiter<'_> {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket {
            let mut quota = lock(&self.shared.disk_bytes);
            remove_disk_waiter(&mut quota, ticket);
            self.shared.disk_changed.notify_all();
        }
    }
}

struct CancellableFile<'a> {
    file: &'a mut File,
    cancellation: &'a CancellationToken,
    staged_bytes: &'a mut u64,
    write_failure_after: &'a AtomicUsize,
}

struct BoundedVecWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedVecWriter {
    fn new(limit: usize) -> Result<Self, WebTaskError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(limit)
            .map_err(|_| WebTaskError::Limit("JSON buffer allocation failed".into()))?;
        Ok(Self { bytes, limit })
    }
}

impl Write for BoundedVecWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().checked_add(bytes.len()).is_none_or(|total| total > self.limit) {
            return Err(std::io::Error::other("JSON byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct ArtifactSnapshot {
    file: File,
    shared: Arc<Shared>,
    charged_bytes: u64,
}

struct NamedSnapshotGuard<'a> {
    parent: &'a SafeDir,
    directory: &'a SafeDir,
    nonce: &'a str,
    active: bool,
}

impl NamedSnapshotGuard<'_> {
    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for NamedSnapshotGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.directory.remove_regular_private(std::ffi::OsStr::new("payload"));
            let _ = self.parent.remove_empty_child_private(std::ffi::OsStr::new(self.nonce));
        }
    }
}

#[cfg(test)]
fn snapshot_failure_checkpoint(shared: &Shared, phase: usize) -> Result<(), WebTaskError> {
    if shared
        .snapshot_failure
        .compare_exchange(phase, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return Err(WebTaskError::Io(format!("injected snapshot failure at phase {phase}")));
    }
    Ok(())
}

#[cfg(not(test))]
#[allow(clippy::unnecessary_wraps)]
fn snapshot_failure_checkpoint(_shared: &Shared, _phase: usize) -> Result<(), WebTaskError> {
    Ok(())
}

#[cfg(test)]
fn publication_failure_checkpoint(shared: &Shared, phase: usize) -> Result<(), WebTaskError> {
    publication_failure_checkpoint_atomic(&shared.publication_failure, phase)
}

#[cfg(not(test))]
#[allow(clippy::unnecessary_wraps)]
fn publication_failure_checkpoint(_shared: &Shared, _phase: usize) -> Result<(), WebTaskError> {
    Ok(())
}

fn publication_failure_checkpoint_atomic(
    failure: &AtomicUsize,
    phase: usize,
) -> Result<(), WebTaskError> {
    if failure.compare_exchange(phase, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        return Err(WebTaskError::Io(format!("injected publication ENOSPC at phase {phase}")));
    }
    Ok(())
}

#[cfg(test)]
fn retention_failure_checkpoint(shared: &Shared, phase: usize) -> Result<(), WebTaskError> {
    if shared
        .retention_failure
        .compare_exchange(phase, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return Err(WebTaskError::Io(format!("injected retention failure at phase {phase}")));
    }
    Ok(())
}

#[cfg(not(test))]
fn retention_failure_checkpoint(_shared: &Shared, _phase: usize) -> Result<(), WebTaskError> {
    Ok(())
}

impl ArtifactSnapshot {
    #[cfg(test)]
    fn metadata(&self) -> std::io::Result<fs::Metadata> {
        self.file.metadata()
    }
}

impl Read for ArtifactSnapshot {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(bytes)
    }
}

impl Seek for ArtifactSnapshot {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

impl Drop for ArtifactSnapshot {
    fn drop(&mut self) {
        let mut disk = lock(&self.shared.disk_bytes);
        disk.reserved = disk.reserved.saturating_sub(self.charged_bytes);
        if let Ok(measured) = measured_managed_bytes(&self.shared.root_handle) {
            disk.used = measured;
        }
        self.shared.disk_changed.notify_all();
    }
}

impl Write for CancellableFile<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "task cancelled"));
        }
        let requested = u64::try_from(bytes.len())
            .map_err(|_| std::io::Error::other("artifact write length overflow"))?;
        if self
            .staged_bytes
            .checked_add(requested)
            .is_none_or(|total| total > MAX_TASK_DURABLE_GROWTH)
        {
            return Err(std::io::Error::other("task artifact limit exceeded"));
        }
        let failure_after = self.write_failure_after.load(Ordering::SeqCst);
        if failure_after == 0 {
            return Err(std::io::Error::from_raw_os_error(28));
        }
        let permitted =
            if failure_after == usize::MAX { bytes.len() } else { bytes.len().min(failure_after) };
        let written = self.file.write(&bytes[..permitted])?;
        if failure_after != usize::MAX {
            self.write_failure_after.fetch_sub(written, Ordering::SeqCst);
        }
        *self.staged_bytes = self
            .staged_bytes
            .checked_add(u64::try_from(written).unwrap_or(u64::MAX))
            .ok_or_else(|| std::io::Error::other("artifact byte accounting overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Seek for CancellableFile<'_> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "task cancelled"));
        }
        self.file.seek(position)
    }
}

impl<'a> DiskLease<'a> {
    fn register_waiter(shared: &Shared) -> Result<u64, WebTaskError> {
        let mut quota = lock(&shared.disk_bytes);
        quota
            .waiters
            .try_reserve(1)
            .map_err(|_| WebTaskError::Limit("disk waiter allocation failed".into()))?;
        let ticket = quota.next_ticket;
        quota.next_ticket = quota
            .next_ticket
            .checked_add(1)
            .ok_or_else(|| WebTaskError::Limit("disk admission ticket overflow".into()))?;
        quota.waiters.push_back(ticket);
        Ok(ticket)
    }

    #[cfg(test)]
    fn acquire(shared: &'a Shared, amount: u64) -> Result<Self, WebTaskError> {
        let mut quota = lock(&shared.disk_bytes);
        loop {
            let required = quota
                .used
                .checked_add(amount)
                .ok_or_else(|| WebTaskError::Limit("global storage accounting overflow".into()))?;
            if required > MAX_DATA_BYTES {
                return Err(WebTaskError::Limit(
                    "managed storage reservations exceed the global ceiling".into(),
                ));
            }
            if required
                .checked_add(quota.reserved)
                .is_some_and(|committed| committed <= MAX_DATA_BYTES)
            {
                break;
            }
            #[cfg(test)]
            shared.disk_waiters.fetch_add(1, Ordering::SeqCst);
            quota =
                shared.disk_changed.wait(quota).unwrap_or_else(std::sync::PoisonError::into_inner);
            #[cfg(test)]
            shared.disk_waiters.fetch_sub(1, Ordering::SeqCst);
        }
        quota.reserved += amount;
        Ok(Self { shared, amount })
    }

    fn acquire_interruptible(
        shared: &'a Shared,
        amount: u64,
        cancellation: &CancellationToken,
        deadline: Instant,
        registered_ticket: Option<u64>,
    ) -> Result<Self, WebTaskError> {
        let ticket = match registered_ticket {
            Some(ticket) => ticket,
            None => Self::register_waiter(shared)?,
        };
        let mut quota = lock(&shared.disk_bytes);
        loop {
            drop(quota);
            let stopped = lock(&shared.queue).stopped;
            quota = lock(&shared.disk_bytes);
            if cancellation.is_cancelled() || stopped {
                remove_disk_waiter(&mut quota, ticket);
                shared.disk_changed.notify_all();
                return Err(WebTaskError::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                remove_disk_waiter(&mut quota, ticket);
                shared.disk_changed.notify_all();
                return Err(WebTaskError::Limit("task timed out waiting for disk quota".into()));
            }
            let required = quota
                .used
                .checked_add(amount)
                .ok_or_else(|| WebTaskError::Limit("global storage accounting overflow".into()))?;
            if required > MAX_DATA_BYTES {
                remove_disk_waiter(&mut quota, ticket);
                shared.disk_changed.notify_all();
                return Err(WebTaskError::Limit(
                    "managed storage reservation is unavailable".into(),
                ));
            }
            if quota.waiters.front() == Some(&ticket)
                && required
                    .checked_add(quota.reserved)
                    .is_some_and(|committed| committed <= MAX_DATA_BYTES)
            {
                quota.waiters.pop_front();
                quota.reserved += amount;
                #[cfg(test)]
                lock(&shared.disk_ticket_admissions).push(ticket);
                shared.disk_changed.notify_all();
                return Ok(Self { shared, amount });
            }
            #[cfg(test)]
            shared.disk_waiters.fetch_add(1, Ordering::SeqCst);
            let wait = deadline.saturating_duration_since(now).min(Duration::from_millis(100));
            quota = shared
                .disk_changed
                .wait_timeout(quota, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
            #[cfg(test)]
            shared.disk_waiters.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn remove_disk_waiter(quota: &mut DiskQuota, ticket: u64) {
    if let Some(position) = quota.waiters.iter().position(|candidate| *candidate == ticket) {
        quota.waiters.remove(position);
    }
}

impl Drop for DiskLease<'_> {
    fn drop(&mut self) {
        let mut quota = lock(&self.shared.disk_bytes);
        quota.reserved = quota.reserved.saturating_sub(self.amount);
        // Reconcile every completed reservation even while other jobs remain
        // active. Otherwise a continuously full worker pool could repeatedly
        // spend the same released reservation without charging durable bytes.
        // Counting another active job's partial files in both `used` and its
        // full reservation is intentionally conservative and never unsafe.
        quota.used = measured_managed_bytes(&self.shared.root_handle).unwrap_or_else(|_| {
            // Concurrent descriptor-bound publication can make a full-tree
            // measurement transiently unavailable. Preserve safety by charging
            // the entire released plan, without permanently poisoning the
            // backend as though the disk were already full.
            quota.used.saturating_add(self.amount).min(MAX_GLOBAL_BYTES)
        });
        self.shared.disk_changed.notify_all();
    }
}

struct Owner {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        {
            let mut queue = lock(&self.shared.queue);
            queue.stopped = true;
            for cancellation in queue.cancellations.values() {
                cancellation.cancel();
            }
            self.shared.queue_changed.notify_all();
            self.shared.disk_changed.notify_all();
        }
        for worker in lock(&self.workers).drain(..) {
            let _ = worker.join();
        }
    }
}

/// Production local backend. Worker threads consume a stable FIFO queue.
#[derive(Clone)]
pub struct WebTaskBackend {
    owner: Arc<Owner>,
}

impl WebTaskBackend {
    #[cfg(test)]
    pub(crate) fn test_reserved_bytes(&self) -> u64 {
        lock(&self.owner.shared.disk_bytes).reserved
    }

    /// Open durable stores, recover nonterminal tasks, and start bounded workers.
    #[allow(clippy::too_many_lines)]
    #[cfg(test)]
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, WebTaskError> {
        Self::open_internal(root.into(), None)
    }

    pub(crate) fn open_with_media_config(
        root: impl Into<PathBuf>,
        loaded: crate::config::LoadedConfig,
        cwd: PathBuf,
    ) -> Result<Self, WebTaskError> {
        Self::open_internal(root.into(), Some((loaded, cwd)))
    }

    pub(crate) fn update_media_config(&self, loaded: crate::config::LoadedConfig) {
        self.owner.shared.media_services.update_config(loaded);
    }

    fn open_internal(
        root: PathBuf,
        loaded: Option<(crate::config::LoadedConfig, PathBuf)>,
    ) -> Result<Self, WebTaskError> {
        let root = private_directory(root)?;
        let root_handle = SafeDir::open_absolute(&root)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        root_handle
            .verify_private_namespace()
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let task_store = TaskStore::open(root.join("database"), BusyControl::default())?;
        #[cfg(test)]
        if SWAP_ROOT_AFTER_TASK_STORE_OPEN.with(|swap| swap.replace(false)) {
            let moved = root.with_extension("authenticated");
            fs::rename(&root, &moved)?;
            create_private_directory(&root)?;
        }
        root_handle
            .verify_private_namespace()
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let recovery = RecoveryStore::open(root.join("recovery"))
            .map_err(|error| WebTaskError::Io(error.to_string()))?;
        root_handle
            .verify_private_namespace()
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let open_managed = |name: &str| {
            root_handle
                .open_child_private_optional(std::ffi::OsStr::new(name))
                .and_then(|existing| {
                    existing.map_or_else(
                        || root_handle.create_child_private(std::ffi::OsStr::new(name)),
                        Ok,
                    )
                })
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))
        };
        let objects = open_managed("objects")?;
        let incoming = open_managed("incoming")?;
        let snapshots = open_managed("snapshots")?;
        let trash = open_managed("trash")?;
        for directory in [&objects, &incoming, &snapshots, &trash] {
            directory
                .verify_private_namespace()
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        }
        cleanup_flat_private(&snapshots, "payload")?;
        cleanup_crash_residue(&incoming, &objects)?;
        recover_retention_trash(&objects, &trash, &task_store, &recovery)?;
        let used = measured_managed_bytes(&root_handle)?;
        if used > MAX_GLOBAL_BYTES {
            return Err(WebTaskError::Limit("managed storage exceeds the global ceiling".into()));
        }
        let engine =
            into_markdown::default_engine().map_err(|error| WebTaskError::Io(error.to_string()))?;
        let shared = Arc::new(Shared {
            root_handle,
            objects,
            incoming,
            snapshots,
            trash,
            task_store: Mutex::new(task_store),
            history_mutation: Mutex::new(()),
            recovery,
            engine,
            media_services: loaded
                .map_or_else(crate::services::WebMediaServiceCache::default, |(loaded, cwd)| {
                    crate::services::WebMediaServiceCache::with_config(loaded, cwd)
                }),
            events: EventHub::new(random_hex()?),
            queue: Mutex::new(QueueState::default()),
            queue_changed: Condvar::new(),
            disk_bytes: Mutex::new(DiskQuota {
                used,
                reserved: 0,
                next_ticket: 0,
                waiters: VecDeque::new(),
            }),
            disk_changed: Condvar::new(),
            write_failure_after: AtomicUsize::new(usize::MAX),
            #[cfg(test)]
            active_workers: AtomicUsize::new(0),
            #[cfg(test)]
            max_active_workers: AtomicUsize::new(0),
            #[cfg(test)]
            dequeue_order: Mutex::new(Vec::new()),
            #[cfg(test)]
            admission_order: Mutex::new(Vec::new()),
            #[cfg(test)]
            disk_waiters: AtomicUsize::new(0),
            #[cfg(test)]
            disk_ticket_admissions: Mutex::new(Vec::new()),
            #[cfg(test)]
            conversion_gate: Mutex::new(None),
            #[cfg(test)]
            conversion_entries: AtomicUsize::new(0),
            #[cfg(test)]
            pre_acquire_gate: Mutex::new(None),
            #[cfg(test)]
            pre_acquire_panic: AtomicUsize::new(0),
            #[cfg(test)]
            snapshot_failure: AtomicUsize::new(0),
            #[cfg(test)]
            reconcile_failures: AtomicUsize::new(0),
            #[cfg(test)]
            fail_transition_failures: AtomicUsize::new(0),
            #[cfg(test)]
            dequeue_get_failures: AtomicUsize::new(0),
            #[cfg(test)]
            dequeue_transition_failures: AtomicUsize::new(0),
            #[cfg(test)]
            success_transition_failures: AtomicUsize::new(0),
            #[cfg(test)]
            retention_failure: AtomicUsize::new(0),
            #[cfg(test)]
            retention_quarantine_gate: Mutex::new(None),
            #[cfg(test)]
            history_scan_gate: Mutex::new(None),
            publication_failure: AtomicUsize::new(0),
        });
        let backend = Self { owner: Arc::new(Owner { shared, workers: Mutex::new(Vec::new()) }) };
        backend.recover()?;
        backend.cleanup(RetentionPolicy::default(), unix_now_ms()?)?;
        for index in 0..MAX_WORKERS {
            let shared = Arc::clone(&backend.owner.shared);
            let worker = std::thread::Builder::new()
                .name(format!("into-md-web-worker-{index}"))
                .spawn(move || worker(shared))
                .map_err(|error| WebTaskError::Io(format!("start queue worker: {error}")))?;
            lock(&backend.owner.workers).push(worker);
        }
        Ok(backend)
    }

    /// Begin one streamed upload. The returned guard removes uncommitted bytes.
    #[cfg(test)]
    pub fn begin_upload(
        &self,
        display_name: &str,
        declared_bytes: Option<u64>,
    ) -> Result<Upload, WebTaskError> {
        self.begin_upload_configured(display_name, declared_bytes, WebTaskRequest::default())
    }

    /// Begin one upload with a validated shared conversion request.
    pub(crate) fn begin_upload_configured(
        &self,
        display_name: &str,
        declared_bytes: Option<u64>,
        request: WebTaskRequest,
    ) -> Result<Upload, WebTaskError> {
        self.cleanup(RetentionPolicy::default(), unix_now_ms()?)?;
        validate_display_name(display_name)?;
        validate_web_task_request(&request)?;
        self.owner
            .shared
            .incoming
            .verify_private_namespace()
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        if declared_bytes.is_some_and(|bytes| bytes > MAX_FILE_BYTES) {
            return Err(WebTaskError::Limit("file exceeds 512 MiB".into()));
        }
        let nonce = random_hex()?;
        let directory = self
            .owner
            .shared
            .incoming
            .create_child_private(std::ffi::OsStr::new(&nonce))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let file = directory
            .create_regular_private(std::ffi::OsStr::new("payload"))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let mut name = String::new();
        name.try_reserve_exact(display_name.len())
            .map_err(|_| WebTaskError::Limit("display filename allocation failed".into()))?;
        name.push_str(display_name);
        Ok(Upload {
            backend: self.clone(),
            directory,
            nonce,
            file: Some(file),
            name,
            request,
            bytes: 0,
            committed: false,
        })
    }

    /// Return one bounded task record.
    pub fn get(&self, id: &TaskId) -> Result<TaskRecord, WebTaskError> {
        let record = lock(&self.owner.shared.task_store).get(id)?.ok_or(WebTaskError::NotFound)?;
        if record.status == TaskStatus::Succeeded {
            validate_web_success(&self.owner.shared, &record)?;
        }
        Ok(record)
    }

    /// Enrich a durable task record with bounded, non-secret browser metadata.
    pub(crate) fn web_record(&self, record: TaskRecord) -> Result<WebTaskRecord, WebTaskError> {
        let _history = lock(&self.owner.shared.history_mutation);
        let request = self.persisted_request(&record.id)?;
        Ok(WebTaskRecord {
            record,
            workflow: request.as_ref().map_or(WebWorkflow::Conversion, |value| value.workflow),
            display_name: request.as_ref().map(|request| request.name.clone()),
            format: request.as_ref().and_then(|request| request.hint.format),
            batch_id: request.and_then(|request| request.batch_id),
        })
    }

    fn persisted_request(&self, id: &TaskId) -> Result<Option<PersistedRequest>, WebTaskError> {
        let task = self
            .owner
            .shared
            .objects
            .open_child_private_optional(std::ffi::OsStr::new(id.as_str()))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        task.as_ref().map(load_persisted_request).transpose().map(Option::flatten)
    }

    /// Return a stable, newest-first filtered page.
    pub(crate) fn list(
        &self,
        limit: u32,
        after: Option<&TaskCursor>,
        status: Option<TaskStatus>,
        pinned: Option<bool>,
    ) -> Result<TaskHistoryPage, WebTaskError> {
        self.list_filtered(limit, after, status, pinned, None)
    }

    /// Return one stable page restricted to a persisted browser batch.
    pub(crate) fn list_batch(
        &self,
        limit: u32,
        after: Option<&TaskCursor>,
        status: Option<TaskStatus>,
        pinned: Option<bool>,
        batch_id: &str,
    ) -> Result<TaskHistoryPage, WebTaskError> {
        if !valid_batch_id(batch_id) {
            return Err(WebTaskError::Invalid("batch ID is invalid".into()));
        }
        self.list_filtered(limit, after, status, pinned, Some(batch_id))
    }

    fn list_filtered(
        &self,
        limit: u32,
        after: Option<&TaskCursor>,
        status: Option<TaskStatus>,
        pinned: Option<bool>,
        batch_id: Option<&str>,
    ) -> Result<TaskHistoryPage, WebTaskError> {
        if limit == 0 || limit > 100 {
            return Err(WebTaskError::Invalid("history limit must be within 1 and 100".into()));
        }
        let mut cursor = after.cloned();
        let mut matched = Vec::new();
        matched
            .try_reserve_exact(usize::try_from(limit).unwrap_or(100) + 1)
            .map_err(|_| WebTaskError::Limit("history page allocation failed".into()))?;
        let store = lock(&self.owner.shared.task_store);
        loop {
            let page = store.list(100, cursor.as_ref())?;
            if page.is_empty() {
                break;
            }
            #[cfg(test)]
            if cursor.is_none()
                && let Some(gate) = { lock(&self.owner.shared.history_scan_gate).clone() }
            {
                gate.wait();
                gate.wait();
            }
            let last =
                page.last().ok_or_else(|| WebTaskError::Io("history page vanished".into()))?;
            cursor = Some(TaskCursor { updated_at_ms: last.updated_at_ms, id: last.id.clone() });
            for record in page {
                let batch_matches = match batch_id {
                    None => true,
                    Some(expected) => {
                        self.persisted_request(&record.id)?
                            .and_then(|request| request.batch_id)
                            .as_deref()
                            == Some(expected)
                    }
                };
                if status.is_none_or(|value| record.status == value)
                    && pinned.is_none_or(|value| record.pinned == value)
                    && batch_matches
                {
                    matched.push(record);
                    if matched.len() > usize::try_from(limit).unwrap_or(100) {
                        let extra = matched.pop().expect("matched page has an extra row");
                        let last = matched.last().expect("non-empty requested history page");
                        let next =
                            TaskCursor { updated_at_ms: last.updated_at_ms, id: last.id.clone() };
                        let _ = extra;
                        return Ok(TaskHistoryPage { tasks: matched, next: Some(next) });
                    }
                }
            }
            if cursor.is_none() {
                break;
            }
        }
        Ok(TaskHistoryPage { tasks: matched, next: None })
    }

    pub(crate) fn set_pinned(&self, id: &TaskId, pinned: bool) -> Result<TaskRecord, WebTaskError> {
        let _history = lock(&self.owner.shared.history_mutation);
        metadata_store_mutation(&self.owner.shared, STORE_MUTATION_RESERVATION, |store| {
            store.get(id)?.ok_or(WebTaskError::NotFound)?;
            store.set_pinned(id, pinned)?;
            store.get(id)?.ok_or(WebTaskError::NotFound)
        })
    }

    /// Permanently remove one terminal task. The object tree is first moved to
    /// a private quarantine so a failed DB transaction can restore it.
    pub(crate) fn delete(&self, id: &TaskId) -> Result<(), WebTaskError> {
        let _history = lock(&self.owner.shared.history_mutation);
        self.delete_inner(id, true)
    }

    fn delete_inner(&self, id: &TaskId, allow_pinned: bool) -> Result<(), WebTaskError> {
        let mut store = lock(&self.owner.shared.task_store);
        let record = store.get(id)?.ok_or(WebTaskError::NotFound)?;
        if !is_terminal(record.status) {
            return Err(WebTaskError::Conflict("active task cannot be deleted".into()));
        }
        if record.pinned && !allow_pinned {
            return Err(WebTaskError::Conflict("pinned task is retained".into()));
        }
        let token = RecoveryToken::parse(record.input.recovery_token)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let object_name = std::ffi::OsStr::new(id.as_str());
        if self
            .owner
            .shared
            .objects
            .open_child_private_optional(object_name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
            .is_none()
        {
            return Err(WebTaskError::Unsafe("task object tree is missing".into()));
        }
        validate_private_tree(&self.owner.shared.objects, object_name, 0)?;
        let trash_name = retention_trash_name(id, &token);
        let trash_name = std::ffi::OsStr::new(&trash_name);
        self.owner
            .shared
            .objects
            .rename_child_private_to_no_replace(object_name, &self.owner.shared.trash, trash_name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let checkpoint_purge = match self.owner.shared.recovery.quarantine_purge(&token) {
            Ok(purge) => purge,
            Err(error) => {
                self.owner
                    .shared
                    .trash
                    .rename_child_private_to_no_replace(
                        trash_name,
                        &self.owner.shared.objects,
                        object_name,
                    )
                    .map_err(|restore| WebTaskError::Unsafe(restore.to_string()))?;
                return Err(WebTaskError::Unsafe(error.to_string()));
            }
        };
        #[cfg(test)]
        if let Some(gate) = { lock(&self.owner.shared.retention_quarantine_gate).clone() } {
            gate.wait();
            gate.wait();
        }
        if let Err(error) = self.owner.shared.recovery.verify_purge(&token) {
            self.owner
                .shared
                .recovery
                .restore_purge(checkpoint_purge)
                .map_err(|restore| WebTaskError::Unsafe(restore.to_string()))?;
            self.owner
                .shared
                .trash
                .rename_child_private_to_no_replace(
                    trash_name,
                    &self.owner.shared.objects,
                    object_name,
                )
                .map_err(|restore| WebTaskError::Unsafe(restore.to_string()))?;
            return Err(WebTaskError::Unsafe(error.to_string()));
        }

        if let Err(error) = retention_failure_checkpoint(&self.owner.shared, 1)
            .and_then(|()| store.delete_terminal(id, allow_pinned).map_err(Into::into))
        {
            self.owner
                .shared
                .recovery
                .restore_purge(checkpoint_purge)
                .map_err(|restore| WebTaskError::Unsafe(restore.to_string()))?;
            self.owner
                .shared
                .trash
                .rename_child_private_to_no_replace(
                    trash_name,
                    &self.owner.shared.objects,
                    object_name,
                )
                .map_err(|restore| WebTaskError::Unsafe(restore.to_string()))?;
            return Err(error);
        }
        drop(store);
        // Once the SQLite commit succeeds, the encoded trash entry is the
        // durable deletion intent. A crash or failure from here is completed
        // idempotently by `recover_retention_trash` on the next start.
        retention_failure_checkpoint(&self.owner.shared, 2)?;
        self.owner
            .shared
            .recovery
            .finish_purge(checkpoint_purge)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        remove_quarantined_task(&self.owner.shared.trash, trash_name)?;
        let mut disk = lock(&self.owner.shared.disk_bytes);
        disk.used = measured_managed_bytes(&self.owner.shared.root_handle)?;
        self.owner.shared.disk_changed.notify_all();
        Ok(())
    }

    pub(crate) fn retry(&self, id: &TaskId) -> Result<TaskRecord, WebTaskError> {
        if let Some(record) = self.resume_cancelled_media(id)? {
            return Ok(record);
        }
        let (request, bytes) = {
            // Keep deletion and retention from quarantining the source while
            // retry authenticates and copies it. Release the lock before the
            // new upload runs its own automatic retention pass.
            let _history = lock(&self.owner.shared.history_mutation);
            let record = self.get(id)?;
            if !is_terminal(record.status) {
                return Err(WebTaskError::Conflict("active task cannot be retried".into()));
            }
            let task = self
                .owner
                .shared
                .objects
                .open_child_private(std::ffi::OsStr::new(id.as_str()))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            let request = load_persisted_request(&task)?
                .ok_or_else(|| WebTaskError::Conflict("original request is unavailable".into()))?;
            let input = task
                .open_regular_private(std::ffi::OsStr::new("input"))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            (request, read_file_bounded(input, MAX_FILE_BYTES)?)
        };
        let configured = WebTaskRequest {
            schema_version: 1,
            workflow: request.workflow,
            format: request.hint.format,
            batch_id: None,
            options: request.options,
            authorization: WebTaskAuthorization::default(),
        };
        let mut upload = self.begin_upload_configured(
            &request.name,
            Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
            configured,
        )?;
        upload.write_chunk(&bytes)?;
        upload.finish()
    }

    fn resume_cancelled_media(&self, id: &TaskId) -> Result<Option<TaskRecord>, WebTaskError> {
        let _history = lock(&self.owner.shared.history_mutation);
        let record = self.get(id)?;
        if record.status != TaskStatus::Cancelled {
            return Ok(None);
        }
        let token = RecoveryToken::parse(record.input.recovery_token.clone())
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let Some(checkpoint) = self
            .owner
            .shared
            .recovery
            .inspect(&token)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
        else {
            return Ok(None);
        };
        if checkpoint.phase != into_markdown::TaskPhase::Media
            || !constant_time_equal(&checkpoint.input_fingerprint, &record.input.input_fingerprint)
            || !constant_time_equal(
                &checkpoint.options_fingerprint,
                &record.input.options_fingerprint,
            )
        {
            return Ok(None);
        }
        let task = self
            .owner
            .shared
            .objects
            .open_child_private(std::ffi::OsStr::new(id.as_str()))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_directory_handle(&task)?;
        let input_file = task
            .open_regular_private(std::ffi::OsStr::new("input"))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_file(&input_file)?;
        let input = read_file_bounded(input_file, MAX_FILE_BYTES)?;
        let request = load_persisted_request(&task)?
            .ok_or_else(|| WebTaskError::Conflict("original request is unavailable".into()))?;
        if request.workflow != WebWorkflow::MeetingTranscript {
            return Ok(None);
        }
        let (input_fingerprint, options_fingerprint) = Engine::recoverable_fingerprints(
            &input,
            Some(&request.name),
            &request.hint,
            &request.options,
        )
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        if u64::try_from(input.len()).unwrap_or(u64::MAX) != record.input.byte_len
            || !constant_time_equal(&input_fingerprint, &record.input.input_fingerprint)
            || !constant_time_equal(&options_fingerprint, &record.input.options_fingerprint)
        {
            return Err(WebTaskError::Unsafe(
                "cancelled media input no longer matches its recovery checkpoint".into(),
            ));
        }
        let resumed =
            metadata_store_mutation(&self.owner.shared, STORE_MUTATION_RESERVATION, |store| {
                Ok(store.requeue_terminal(id)?)
            })?;
        self.owner.shared.events.restart(&resumed);
        if let Err(error) = self.enqueue(Job {
            id: resumed.id.clone(),
            token,
            request,
            cancellation: CancellationToken::new(),
            admission_ticket: None,
        }) {
            terminal_transition(
                &self.owner.shared,
                &resumed.id,
                TaskStatus::Interrupted,
                DiagnosticCode::RecoveryCheckpointMissing,
            )?;
            return Err(error);
        }
        self.owner.shared.events.publish_snapshot(&resumed);
        Ok(Some(resumed))
    }

    pub(crate) fn cleanup(
        &self,
        policy: RetentionPolicy,
        now_ms: i64,
    ) -> Result<CleanupSummary, WebTaskError> {
        let _history = lock(&self.owner.shared.history_mutation);
        if now_ms < 0 {
            return Err(WebTaskError::Invalid(
                "retention clock must not precede Unix epoch".into(),
            ));
        }
        if policy.max_bytes > MAX_DATA_BYTES {
            return Err(WebTaskError::Invalid(
                "retention capacity exceeds the managed storage ceiling".into(),
            ));
        }
        let before = measured_live_managed_bytes(&self.owner.shared.root_handle)?;
        let age_ms = i64::try_from(policy.max_age.as_millis()).unwrap_or(i64::MAX);
        let cutoff = now_ms.saturating_sub(age_ms);
        let mut cursor = None;
        let mut candidates = Vec::new();
        let store = lock(&self.owner.shared.task_store);
        loop {
            let page = store.list(100, cursor.as_ref())?;
            if page.is_empty() {
                break;
            }
            #[cfg(test)]
            if cursor.is_none()
                && let Some(gate) = { lock(&self.owner.shared.history_scan_gate).clone() }
            {
                gate.wait();
                gate.wait();
            }
            let last = page.last().expect("task page is non-empty");
            cursor = Some(TaskCursor { updated_at_ms: last.updated_at_ms, id: last.id.clone() });
            for record in
                page.into_iter().filter(|record| is_terminal(record.status) && !record.pinned)
            {
                let completed_at_ms = store.completed_at_ms(&record.id)?.ok_or_else(|| {
                    WebTaskError::Io("terminal task is missing its completion timestamp".into())
                })?;
                candidates.push((completed_at_ms, record));
            }
        }
        drop(store);
        candidates
            .sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.id.cmp(&right.1.id)));
        let mut summary = CleanupSummary::default();
        let mut used = before;
        for (completed_at_ms, record) in candidates {
            if completed_at_ms > cutoff && used <= policy.max_bytes {
                break;
            }
            match self.delete_inner(&record.id, false) {
                Ok(()) => summary.deleted_tasks = summary.deleted_tasks.saturating_add(1),
                Err(WebTaskError::NotFound | WebTaskError::Conflict(_)) => continue,
                Err(error) => return Err(error),
            }
            used = measured_live_managed_bytes(&self.owner.shared.root_handle)?;
        }
        summary.reclaimed_bytes = before.saturating_sub(used);
        Ok(summary)
    }

    /// Subscribe without holding a conversion or persistence lock. A cursor
    /// from another process generation (or one that fell behind the bounded
    /// replay window) receives a fresh durable snapshot.
    pub(crate) fn events(
        &self,
        id: &TaskId,
        cursor: Option<(&str, u64)>,
    ) -> Result<TaskEventSubscription, WebTaskError> {
        let record = self.get(id)?;
        Ok(self.owner.shared.events.subscribe(&record, cursor))
    }

    /// Cancel queued/running work. Terminal tasks are left unchanged.
    pub fn cancel(&self, id: &TaskId) -> Result<TaskRecord, WebTaskError> {
        let token = {
            let queue = lock(&self.owner.shared.queue);
            queue.cancellations.get(id).cloned()
        };
        if let Some(token) = token {
            token.cancel();
            self.owner.shared.disk_changed.notify_all();
        }
        let record = lock(&self.owner.shared.task_store).get(id)?.ok_or(WebTaskError::NotFound)?;
        if record.status == TaskStatus::Pending {
            terminal_transition(
                &self.owner.shared,
                id,
                TaskStatus::Cancelled,
                DiagnosticCode::Cancelled,
            )?;
            return lock(&self.owner.shared.task_store).get(id)?.ok_or(WebTaskError::NotFound);
        }
        Ok(record)
    }

    /// Resolve an opaque artifact ID to an authenticated regular file.
    #[allow(clippy::too_many_lines)]
    pub fn artifact(
        &self,
        id: &TaskId,
        key: &str,
    ) -> Result<(ArtifactSnapshot, ArtifactReference), WebTaskError> {
        validate_key(key)?;
        let record = self.get(id)?;
        if record.status != TaskStatus::Succeeded {
            return Err(WebTaskError::Conflict("artifacts are not published".into()));
        }
        let reference = record
            .artifacts
            .iter()
            .find(|artifact| artifact.storage_key == key)
            .cloned()
            .ok_or(WebTaskError::NotFound)?;
        self.owner
            .shared
            .objects
            .verify_private_namespace()
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let task = self
            .owner
            .shared
            .objects
            .open_child_private(std::ffi::OsStr::new(id.as_str()))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        task.verify_private_namespace().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let published = task
            .open_child_private(std::ffi::OsStr::new(&publication_directory_name(&record)))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        published
            .verify_private_namespace()
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let mut file = published
            .open_regular_private(std::ffi::OsStr::new(key))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_file(&file)?;
        let metadata = file.metadata()?;
        if metadata.len() != reference.byte_len || link_count(&metadata) != 1 {
            return Err(WebTaskError::Unsafe("artifact identity or size changed".into()));
        }
        let nonce = random_hex()?;
        let mut disk = lock(&self.owner.shared.disk_bytes);
        if disk
            .used
            .checked_add(disk.reserved)
            .and_then(|total| total.checked_add(reference.byte_len))
            .is_none_or(|total| total > MAX_DATA_BYTES)
        {
            return Err(WebTaskError::Limit(
                "artifact snapshot exceeds global storage quota".into(),
            ));
        }
        let directory = self
            .owner
            .shared
            .snapshots
            .create_child_private(std::ffi::OsStr::new(&nonce))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        let mut named_snapshot = NamedSnapshotGuard {
            parent: &self.owner.shared.snapshots,
            directory: &directory,
            nonce: &nonce,
            active: true,
        };
        let copied = (|| {
            let mut snapshot = directory
                .create_regular_private(std::ffi::OsStr::new("payload"))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            let mut hash = Sha256::new();
            let mut remaining = reference.byte_len;
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(COPY_CHUNK)
                .map_err(|_| WebTaskError::Limit("snapshot buffer allocation failed".into()))?;
            buffer.resize(COPY_CHUNK, 0);
            while remaining != 0 {
                let wanted =
                    usize::try_from(remaining.min(COPY_CHUNK as u64)).unwrap_or(COPY_CHUNK);
                let read = file.read(&mut buffer[..wanted])?;
                if read == 0 {
                    return Err(WebTaskError::Unsafe("artifact shrank while snapshotting".into()));
                }
                write_all_checked(&mut snapshot, &buffer[..read])?;
                hash.update(&buffer[..read]);
                remaining -= u64::try_from(read).unwrap_or(u64::MAX);
            }
            if file.read(&mut buffer[..1])? != 0 {
                return Err(WebTaskError::Unsafe("artifact grew while snapshotting".into()));
            }
            snapshot.sync_all()?;
            directory.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            let digest = format!("{:x}", hash.finalize());
            if digest != reference.sha256 {
                return Err(WebTaskError::Unsafe(
                    "artifact digest does not match its durable index".into(),
                ));
            }
            drop(snapshot);
            directory
                .open_regular_private(std::ffi::OsStr::new("payload"))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))
        })();
        let snapshot = copied?;
        snapshot_failure_checkpoint(&self.owner.shared, 1)?;
        if snapshot.metadata()?.len() != reference.byte_len {
            return Err(WebTaskError::Unsafe("artifact snapshot size changed".into()));
        }
        snapshot_failure_checkpoint(&self.owner.shared, 2)?;
        directory
            .remove_regular_private(std::ffi::OsStr::new("payload"))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        snapshot_failure_checkpoint(&self.owner.shared, 3)?;
        self.owner
            .shared
            .snapshots
            .remove_empty_child_private(std::ffi::OsStr::new(&nonce))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        named_snapshot.disarm();
        disk.reserved = disk
            .reserved
            .checked_add(reference.byte_len)
            .ok_or_else(|| WebTaskError::Limit("snapshot accounting overflow".into()))?;
        drop(disk);
        Ok((
            ArtifactSnapshot {
                file: snapshot,
                shared: Arc::clone(&self.owner.shared),
                charged_bytes: reference.byte_len,
            },
            reference,
        ))
    }

    /// Apply anonymous speaker display names and atomically publish a rerendered artifact set.
    pub(crate) fn relabel_speakers(
        &self,
        id: &TaskId,
        expected_generation: u64,
        assignments: &BTreeMap<String, String>,
    ) -> Result<TaskRecord, WebTaskError> {
        if assignments.is_empty() || assignments.len() > 64 {
            return Err(WebTaskError::Invalid(
                "speaker assignments must contain between 1 and 64 entries".into(),
            ));
        }
        let _history = lock(&self.owner.shared.history_mutation);
        let record = lock(&self.owner.shared.task_store).get(id)?.ok_or(WebTaskError::NotFound)?;
        if record.status != TaskStatus::Succeeded {
            return Err(WebTaskError::Conflict(
                "speaker labels can only be changed on a succeeded task".into(),
            ));
        }
        if record.artifact_generation != expected_generation {
            return Err(WebTaskError::Conflict("artifact generation changed".into()));
        }
        let task = self
            .owner
            .shared
            .objects
            .open_child_private(std::ffi::OsStr::new(id.as_str()))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_directory_handle(&task)?;
        // A crash between publishing the next generation and committing its
        // TaskStore CAS can leave an unselected canonical directory behind.
        // Remove it before rendering so a retry can never adopt stale labels.
        cleanup_unselected_publications(&task, &record)?;
        let request = self
            .persisted_request(id)?
            .ok_or_else(|| WebTaskError::Conflict("original task request is unavailable".into()))?;
        if request.workflow != WebWorkflow::MeetingTranscript {
            return Err(WebTaskError::Conflict(
                "speaker labels are only available for meeting transcripts".into(),
            ));
        }
        if record.artifacts.iter().any(|artifact| artifact.kind == ArtifactKind::Asset) {
            return Err(WebTaskError::Unsafe(
                "meeting transcript unexpectedly contains binary assets".into(),
            ));
        }
        let ir = record
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::DocumentIr)
            .ok_or_else(|| WebTaskError::Conflict("document IR artifact is unavailable".into()))?;
        if ir.byte_len > MAX_TASK_DURABLE_GROWTH {
            return Err(WebTaskError::Limit("document IR exceeds the rerender limit".into()));
        }
        let (mut snapshot, _) = self.artifact(id, &ir.storage_key)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(usize::try_from(ir.byte_len).map_err(|_| {
                WebTaskError::Limit("document IR exceeds the addressable memory limit".into())
            })?)
            .map_err(|_| WebTaskError::Limit("document IR allocation failed".into()))?;
        snapshot.read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != ir.byte_len {
            return Err(WebTaskError::Unsafe("document IR snapshot length changed".into()));
        }
        let mut document: into_markdown::Document = serde_json::from_slice(&bytes)
            .map_err(|_| WebTaskError::Unsafe("document IR artifact is invalid".into()))?;
        drop(snapshot);
        drop(bytes);
        document.validate().map_err(|error| {
            WebTaskError::Unsafe(format!("document IR validation failed: {error}"))
        })?;
        let diagnostics = load_diagnostics_artifact(self, id, &record)?;
        let provenance = document_provenance(&document)?;
        let speaker_ids = document_speaker_ids(&document);
        for (speaker, label) in assignments {
            validate_speaker_assignment(speaker, label)?;
            if !speaker_ids.contains(speaker) {
                return Err(WebTaskError::Invalid(format!(
                    "speaker assignment refers to unknown ID {speaker}"
                )));
            }
            document
                .metadata
                .properties
                .insert(format!("media.speaker.{speaker}.label"), label.clone());
        }
        let markdown = into_markdown::render_markdown(&document, &[], &request.options)
            .map_err(|error| WebTaskError::Io(format!("rerender meeting transcript: {error}")))?;
        let result = into_markdown::ConversionResult::new(
            document,
            markdown,
            Vec::new(),
            diagnostics,
            provenance,
        );
        let next_generation = expected_generation
            .checked_add(1)
            .ok_or_else(|| WebTaskError::Limit("artifact generation overflowed".into()))?;
        let directory_name = format!("published-{next_generation}");
        let cancellation = CancellationToken::new();
        let artifacts =
            publish_result_named(&self.owner.shared, id, &result, &cancellation, &directory_name)?;
        let replaced =
            metadata_store_mutation(&self.owner.shared, MAX_TASK_METADATA_GROWTH, |store| {
                Ok(store.replace_succeeded_artifacts(id, expected_generation, artifacts.clone())?)
            });
        let record = match replaced {
            Ok(record) => record,
            Err(error) => {
                if let Ok(task) =
                    self.owner.shared.objects.open_child_private(std::ffi::OsStr::new(id.as_str()))
                    && let Ok(directory) =
                        task.open_child_private(std::ffi::OsStr::new(&directory_name))
                {
                    let _ = remove_private_files(&directory);
                    let _ = task.remove_empty_child_private(std::ffi::OsStr::new(&directory_name));
                }
                return Err(error);
            }
        };
        let prior_directory = if expected_generation == 0 {
            "published".to_owned()
        } else {
            format!("published-{expected_generation}")
        };
        if let Ok(task) =
            self.owner.shared.objects.open_child_private(std::ffi::OsStr::new(id.as_str()))
            && let Ok(Some(directory)) =
                task.open_child_private_optional(std::ffi::OsStr::new(&prior_directory))
        {
            let _ = remove_private_files(&directory);
            let _ = task.remove_empty_child_private(std::ffi::OsStr::new(&prior_directory));
        }
        self.owner.shared.events.publish_snapshot(&record);
        Ok(record)
    }

    pub(crate) fn speaker_labels(&self, id: &TaskId) -> Result<SpeakerLabels, WebTaskError> {
        let _history = lock(&self.owner.shared.history_mutation);
        let record = self.get(id)?;
        if record.status != TaskStatus::Succeeded {
            return Err(WebTaskError::Conflict("speaker labels require a succeeded task".into()));
        }
        let request = self
            .persisted_request(id)?
            .ok_or_else(|| WebTaskError::Conflict("original task request is unavailable".into()))?;
        if request.workflow != WebWorkflow::MeetingTranscript {
            return Err(WebTaskError::Conflict(
                "speaker labels are only available for meeting transcripts".into(),
            ));
        }
        let ir = record
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::DocumentIr)
            .ok_or_else(|| WebTaskError::Conflict("document IR artifact is unavailable".into()))?;
        if ir.byte_len > MAX_TASK_DURABLE_GROWTH {
            return Err(WebTaskError::Limit("document IR exceeds the label-list limit".into()));
        }
        let (mut snapshot, _) = self.artifact(id, &ir.storage_key)?;
        let document: into_markdown::Document = serde_json::from_reader(&mut snapshot)
            .map_err(|_| WebTaskError::Unsafe("document IR artifact is invalid".into()))?;
        document.validate().map_err(|error| {
            WebTaskError::Unsafe(format!("document IR validation failed: {error}"))
        })?;
        let speakers = document_speaker_ids(&document)
            .into_iter()
            .map(|id| {
                let name = document
                    .metadata
                    .properties
                    .get(&format!("media.speaker.{id}.label"))
                    .cloned()
                    .unwrap_or_else(|| default_speaker_name(&id));
                SpeakerLabel { id, name }
            })
            .collect();
        Ok(SpeakerLabels {
            schema_version: 1,
            artifact_generation: record.artifact_generation,
            speakers,
        })
    }

    fn enqueue(&self, job: Job) -> Result<(), WebTaskError> {
        let mut queue = lock(&self.owner.shared.queue);
        while queue.jobs.len() >= MAX_QUEUE && !queue.stopped {
            queue = self
                .owner
                .shared
                .queue_changed
                .wait(queue)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if queue.stopped {
            return Err(WebTaskError::Conflict("task queue is stopping".into()));
        }
        queue.cancellations.insert(job.id.clone(), job.cancellation.clone());
        queue.jobs.push_back(job);
        self.owner.shared.queue_changed.notify_one();
        Ok(())
    }

    fn recover(&self) -> Result<(), WebTaskError> {
        let mut cursor = None;
        let mut records = Vec::new();
        let store = lock(&self.owner.shared.task_store);
        loop {
            let page = store.list(100, cursor.as_ref())?;
            if page.is_empty() {
                break;
            }
            let last =
                page.last().ok_or_else(|| WebTaskError::Io("recovery page vanished".into()))?;
            cursor = Some(TaskCursor { updated_at_ms: last.updated_at_ms, id: last.id.clone() });
            records.extend(page);
        }
        drop(store);
        for record in records {
            self.recover_record(record)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn recover_record(&self, record: TaskRecord) -> Result<(), WebTaskError> {
        if record.status == TaskStatus::Succeeded {
            let task = self
                .owner
                .shared
                .objects
                .open_child_private(std::ffi::OsStr::new(record.id.as_str()))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            validate_private_directory_handle(&task)?;
            let published = task
                .open_child_private(std::ffi::OsStr::new(&publication_directory_name(&record)))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            validate_private_directory_handle(&published)?;
            let artifacts = validate_manifest_handle(&published)?;
            if !artifact_sets_equal(&artifacts, &record.artifacts) {
                return Err(WebTaskError::Unsafe(
                    "published artifact manifest does not match TaskStore".into(),
                ));
            }
            cleanup_unselected_publications(&task, &record)?;
            return Ok(());
        }
        if matches!(
            record.status,
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::Converted
        ) {
            let Some(task) = self
                .owner
                .shared
                .objects
                .open_child_private_optional(std::ffi::OsStr::new(record.id.as_str()))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
            else {
                terminal_transition(
                    &self.owner.shared,
                    &record.id,
                    TaskStatus::Interrupted,
                    DiagnosticCode::RecoveryCheckpointMissing,
                )?;
                return Ok(());
            };
            validate_private_directory_handle(&task)?;
            let Some(input_file) = task
                .open_regular_optional(std::ffi::OsStr::new("input"))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
            else {
                terminal_transition(
                    &self.owner.shared,
                    &record.id,
                    TaskStatus::Interrupted,
                    DiagnosticCode::RecoveryCheckpointMissing,
                )?;
                return Ok(());
            };
            validate_private_file(&input_file)?;
            let input = read_file_bounded(input_file, MAX_FILE_BYTES)?;
            let Some(request) = load_persisted_request(&task)? else {
                terminal_transition(
                    &self.owner.shared,
                    &record.id,
                    TaskStatus::Interrupted,
                    DiagnosticCode::RecoveryCheckpointMissing,
                )?;
                return Ok(());
            };
            let (input_fingerprint, options_fingerprint) = Engine::recoverable_fingerprints(
                &input,
                Some(&request.name),
                &request.hint,
                &request.options,
            )
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            if !constant_time_equal(&input_fingerprint, &record.input.input_fingerprint)
                || !constant_time_equal(&options_fingerprint, &record.input.options_fingerprint)
                || u64::try_from(input.len()).unwrap_or(u64::MAX) != record.input.byte_len
            {
                return Err(WebTaskError::Unsafe(
                    "persisted request does not match durable fingerprints".into(),
                ));
            }
            self.enqueue(Job {
                id: record.id,
                token: RecoveryToken::parse(record.input.recovery_token)
                    .map_err(|error| WebTaskError::Unsafe(error.to_string()))?,
                request,
                cancellation: CancellationToken::new(),
                admission_ticket: None,
            })?;
        } else if matches!(
            record.status,
            TaskStatus::Failed | TaskStatus::Interrupted | TaskStatus::Cancelled
        ) && let Some(task) = self
            .owner
            .shared
            .objects
            .open_child_private_optional(std::ffi::OsStr::new(record.id.as_str()))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
            && task
                .open_child_private_optional(std::ffi::OsStr::new("published"))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
                .is_some()
        {
            return Err(WebTaskError::Unsafe(
                "terminal non-success task has a published artifact set".into(),
            ));
        }
        Ok(())
    }
}

fn validate_web_success(shared: &Shared, record: &TaskRecord) -> Result<(), WebTaskError> {
    let task = shared
        .objects
        .open_child_private(std::ffi::OsStr::new(record.id.as_str()))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let published = task
        .open_child_private(std::ffi::OsStr::new(&publication_directory_name(record)))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let artifacts = validate_manifest_handle(&published)?;
    if !artifact_sets_equal(&artifacts, &record.artifacts) {
        return Err(WebTaskError::Unsafe(
            "published artifact manifest does not match TaskStore".into(),
        ));
    }
    Ok(())
}

/// In-progress streamed upload.
pub struct Upload {
    backend: WebTaskBackend,
    directory: SafeDir,
    nonce: String,
    file: Option<File>,
    name: String,
    request: WebTaskRequest,
    bytes: u64,
    committed: bool,
}

impl Upload {
    /// Append one body chunk after cancellation and all quota checks.
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), WebTaskError> {
        let amount = u64::try_from(chunk.len())
            .map_err(|_| WebTaskError::Limit("chunk length overflow".into()))?;
        let next = self
            .bytes
            .checked_add(amount)
            .ok_or_else(|| WebTaskError::Limit("upload length overflow".into()))?;
        if next > MAX_FILE_BYTES {
            return Err(WebTaskError::Limit("file exceeds 512 MiB".into()));
        }
        {
            let mut global = lock(&self.backend.owner.shared.disk_bytes);
            let next_global = global
                .used
                .checked_add(amount)
                .and_then(|used| used.checked_add(global.reserved))
                .ok_or_else(|| WebTaskError::Limit("global storage accounting overflow".into()))?;
            if next_global > MAX_DATA_BYTES {
                return Err(WebTaskError::Limit(
                    "managed storage exceeds the global ceiling".into(),
                ));
            }
            let file = self
                .file
                .as_mut()
                .ok_or_else(|| WebTaskError::Conflict("upload is already finished".into()))?;
            let mut remaining = chunk;
            while !remaining.is_empty() {
                let written = match file.write(remaining) {
                    Ok(0) => return Err(WebTaskError::Io("file write made no progress".into())),
                    Ok(written) => written,
                    Err(error) => return Err(error.into()),
                };
                let written = u64::try_from(written)
                    .map_err(|_| WebTaskError::Limit("upload write length overflow".into()))?;
                global.used = global.used.checked_add(written).ok_or_else(|| {
                    WebTaskError::Limit("global storage accounting overflow".into())
                })?;
                self.bytes = self.bytes.checked_add(written).ok_or_else(|| {
                    WebTaskError::Limit("upload length accounting overflow".into())
                })?;
                remaining = &remaining[usize::try_from(written)
                    .map_err(|_| WebTaskError::Limit("upload write length overflow".into()))?..];
            }
        }
        debug_assert_eq!(self.bytes, next);
        Ok(())
    }

    /// fsync input, bind fingerprints/token/task ID, and enqueue conversion.
    #[allow(clippy::too_many_lines)]
    pub fn finish(mut self) -> Result<TaskRecord, WebTaskError> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| WebTaskError::Conflict("upload is already finished".into()))?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        self.directory.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_directory_handle(&self.directory)?;
        let payload = self
            .directory
            .open_regular_private(std::ffi::OsStr::new("payload"))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_file(&payload)?;
        let bytes = read_file_bounded(payload, MAX_FILE_BYTES)?;
        let options = self.request.options.clone();
        let hint = FormatHint {
            format: self.request.format,
            filename: Some(self.name.clone()),
            ..FormatHint::default()
        };
        let (input_fingerprint, options_fingerprint) =
            Engine::recoverable_fingerprints(&bytes, Some(&self.name), &hint, &options)
                .map_err(|error| WebTaskError::Io(error.to_string()))?;
        let ocr_enabled = options.ocr.policy != OcrPolicy::Off;
        let preserve_layout = options.ai.layout_repair != AiMode::Off;
        let persisted = PersistedRequest {
            schema_version: 1,
            workflow: self.request.workflow,
            name: self.name.clone(),
            hint,
            batch_id: self.request.batch_id.clone(),
            options,
        };
        let request_json = bounded_json(&persisted, 64 * 1024, "persisted request")?;
        let _metadata_lease = DiskLease::acquire_interruptible(
            &self.backend.owner.shared,
            1024 * 1024,
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
            None,
        )?;
        let token = self
            .backend
            .owner
            .shared
            .recovery
            .create_token()
            .map_err(|error| WebTaskError::Io(error.to_string()))?;
        let record = metadata_store_mutation(
            &self.backend.owner.shared,
            STORE_MUTATION_RESERVATION,
            |store| {
                Ok(store.create(NewTask {
                    input: InputReference {
                        schema_version: 1,
                        input_fingerprint,
                        options_fingerprint,
                        byte_len: self.bytes,
                        recovery_token: token.as_str().to_owned(),
                    },
                    configuration: ConfigurationSnapshot {
                        schema_version: 1,
                        output_format: into_markdown::OutputFormat::Markdown,
                        ocr_enabled,
                        preserve_layout,
                    },
                })?)
            },
        )?;
        let finalized = (|| {
            self.backend
                .owner
                .shared
                .objects
                .verify_private_namespace()
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            let task = self
                .backend
                .owner
                .shared
                .objects
                .create_child_private(std::ffi::OsStr::new(record.id.as_str()))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            write_private_handle(&task, "request.json", &request_json)?;
            self.directory
                .rename_child_private_to_no_replace(
                    std::ffi::OsStr::new("payload"),
                    &task,
                    std::ffi::OsStr::new("input"),
                )
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            self.backend
                .owner
                .shared
                .incoming
                .remove_empty_child_private(std::ffi::OsStr::new(&self.nonce))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            Ok::<_, WebTaskError>(())
        })();
        match finalized {
            Ok(()) => {}
            Err(error) => {
                if terminal_transition(
                    &self.backend.owner.shared,
                    &record.id,
                    TaskStatus::Interrupted,
                    DiagnosticCode::RecoveryCheckpointMissing,
                )
                .is_err()
                {
                    stop_unhealthy(&self.backend.owner.shared);
                }
                return Err(error);
            }
        }
        self.committed = true;
        if let Err(error) = self.backend.enqueue(Job {
            id: record.id.clone(),
            token,
            request: persisted,
            cancellation: CancellationToken::new(),
            admission_ticket: None,
        }) {
            if terminal_transition(
                &self.backend.owner.shared,
                &record.id,
                TaskStatus::Interrupted,
                DiagnosticCode::RecoveryCheckpointMissing,
            )
            .is_err()
            {
                stop_unhealthy(&self.backend.owner.shared);
            }
            return Err(error);
        }
        self.backend.owner.shared.events.publish_snapshot(&record);
        Ok(record)
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.file.take();
            let removed_file =
                self.directory.remove_regular_private(std::ffi::OsStr::new("payload"));
            let removed_directory = self
                .backend
                .owner
                .shared
                .incoming
                .remove_empty_child_private(std::ffi::OsStr::new(&self.nonce));
            let mut global = lock(&self.backend.owner.shared.disk_bytes);
            if removed_file.is_ok() && removed_directory.is_ok() {
                global.used = global.used.saturating_sub(self.bytes);
                self.backend.owner.shared.disk_changed.notify_all();
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn worker(shared: Arc<Shared>) {
    loop {
        let (job, registration_failed) = {
            let mut queue = lock(&shared.queue);
            while queue.jobs.is_empty() && !queue.stopped {
                queue = shared
                    .queue_changed
                    .wait(queue)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if queue.stopped {
                return;
            }
            let mut job = queue.jobs.pop_front();
            let mut registration_failed = false;
            if let Some(job) = &mut job {
                if let Ok(ticket) = DiskLease::register_waiter(&shared) {
                    job.admission_ticket = Some(ticket);
                } else {
                    registration_failed = true;
                }
            }
            shared.queue_changed.notify_all();
            #[cfg(test)]
            if let Some(job) = &job {
                lock(&shared.dequeue_order).push(job.id.clone());
            }
            (job, registration_failed)
        };
        if registration_failed {
            settle_unhealthy_queue(&shared, job.as_ref().map(|value| &value.id));
            return;
        }
        if let Some(job) = job {
            #[cfg(test)]
            let _active = ActiveWorker::enter(&shared);
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&shared, &job)))
                .is_err()
            {
                reconcile_or_fail(&shared, &job.id);
            }
            let mut queue = lock(&shared.queue);
            if queue
                .cancellations
                .get(&job.id)
                .is_some_and(|current| current.same_instance(&job.cancellation))
            {
                queue.cancellations.remove(&job.id);
            }
        }
    }
}

fn settle_unhealthy_queue(shared: &Shared, current: Option<&TaskId>) {
    {
        let mut queue = lock(&shared.queue);
        queue.stopped = true;
        for cancellation in queue.cancellations.values() {
            cancellation.cancel();
        }
        shared.queue_changed.notify_all();
        shared.disk_changed.notify_all();
    }
    if let Some(id) = current
        && !finish_terminal_or_stop(shared, id, false)
    {
        return;
    }
    loop {
        let next = {
            let mut queue = lock(&shared.queue);
            let next = queue.jobs.pop_front();
            shared.queue_changed.notify_all();
            next
        };
        let Some(job) = next else { break };
        if terminal_transition(
            shared,
            &job.id,
            TaskStatus::Interrupted,
            DiagnosticCode::RecoveryCheckpointMissing,
        )
        .is_err()
        {
            // The metadata reserve is exhausted or persistence is unhealthy.
            // Do not continue consuming the one emergency headroom with more
            // transactions, and preserve this job for restart reconciliation.
            lock(&shared.queue).jobs.push_front(job);
            stop_unhealthy(shared);
            return;
        }
    }
}

#[cfg(test)]
struct ActiveWorker<'a>(&'a Shared);

#[cfg(test)]
impl<'a> ActiveWorker<'a> {
    fn enter(shared: &'a Shared) -> Self {
        let active = shared.active_workers.fetch_add(1, Ordering::SeqCst) + 1;
        shared.max_active_workers.fetch_max(active, Ordering::SeqCst);
        Self(shared)
    }
}

#[cfg(test)]
impl Drop for ActiveWorker<'_> {
    fn drop(&mut self) {
        self.0.active_workers.fetch_sub(1, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_lines)]
fn run_job(shared: &Arc<Shared>, job: &Job) {
    let mut registered_waiter = RegisteredDiskWaiter { shared, ticket: job.admission_ticket };
    #[cfg(test)]
    if let Some(gate) = { lock(&shared.pre_acquire_gate).clone() } {
        gate.wait();
    }
    #[cfg(test)]
    assert_eq!(
        shared.pre_acquire_panic.swap(0, Ordering::SeqCst),
        0,
        "injected panic before disk admission"
    );
    if job.cancellation.is_cancelled() {
        finish_terminal_or_stop(shared, &job.id, true);
        return;
    }
    let deadline = Instant::now() + Duration::from_mins(30);
    let _disk_lease = match DiskLease::acquire_interruptible(
        shared,
        MAX_TASK_TOTAL_DURABLE_GROWTH,
        &job.cancellation,
        deadline,
        job.admission_ticket,
    ) {
        Ok(lease) => {
            registered_waiter.consume();
            lease
        }
        Err(WebTaskError::Cancelled) => {
            finish_terminal_or_stop(shared, &job.id, true);
            return;
        }
        Err(_) => {
            finish_terminal_or_stop(shared, &job.id, false);
            return;
        }
    };
    #[cfg(test)]
    lock(&shared.admission_order).push(job.id.clone());
    let Some(record) = retry_dequeued_get(shared, &job.id) else {
        stop_unhealthy(shared);
        return;
    };
    if record.status == TaskStatus::Pending {
        let transition = TaskTransition {
            expected: TaskStatus::Pending,
            next: TaskStatus::Running,
            progress_millionths: 1,
            diagnostics: Vec::new(),
            artifacts: Vec::new(),
        };
        if !retry_dequeued_transition(shared, &job.id, &transition) {
            stop_unhealthy(shared);
            return;
        }
    }
    #[cfg(test)]
    let conversion_gate = { lock(&shared.conversion_gate).clone() };
    #[cfg(test)]
    if let Some(gate) = conversion_gate {
        shared.conversion_entries.fetch_add(1, Ordering::SeqCst);
        gate.wait();
    }
    let result = (|| {
        let task = shared
            .objects
            .open_child_private(std::ffi::OsStr::new(job.id.as_str()))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_directory_handle(&task)?;
        let input_file = task
            .open_regular_private(std::ffi::OsStr::new("input"))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_file(&input_file)?;
        let bytes = read_file_bounded(input_file, MAX_FILE_BYTES)?;
        let persisted = load_persisted_request(&task)?.ok_or_else(|| {
            WebTaskError::Unsafe("persisted request is missing before first execution".into())
        })?;
        let persisted_wire = bounded_json(&persisted, 64 * 1024, "persisted request")?;
        let queued_wire = bounded_json(&job.request, 64 * 1024, "queued request")?;
        if !constant_time_equal_bytes(&persisted_wire, &queued_wire) {
            return Err(WebTaskError::Unsafe(
                "queued request differs from its descriptor-bound durable request".into(),
            ));
        }
        let durable = lock(&shared.task_store).get(&job.id)?.ok_or(WebTaskError::NotFound)?;
        let (input_fingerprint, options_fingerprint) = Engine::recoverable_fingerprints(
            &bytes,
            Some(&persisted.name),
            &persisted.hint,
            &persisted.options,
        )
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != durable.input.byte_len
            || !constant_time_equal(&input_fingerprint, &durable.input.input_fingerprint)
            || !constant_time_equal(&options_fingerprint, &durable.input.options_fingerprint)
            || !constant_time_equal(job.token.as_str(), &durable.input.recovery_token)
        {
            return Err(WebTaskError::Unsafe(
                "task input or persisted request changed after authentication".into(),
            ));
        }
        let execution = ExecutionOptions {
            cancellation: job.cancellation.clone(),
            timeout: Some(deadline.saturating_duration_since(Instant::now())),
            progress_listener: Some(Arc::new(EventProgressListener {
                shared: Arc::downgrade(shared),
                id: job.id.clone(),
            })),
        };
        let routed_engine = if persisted.workflow == WebWorkflow::MeetingTranscript {
            let services = shared
                .media_services
                .assemble(&persisted.options)
                .map_err(|error| WebTaskError::Io(error.to_string()))?;
            Some(
                into_markdown::default_engine_with_services(services)
                    .map_err(|error| WebTaskError::Io(error.to_string()))?,
            )
        } else {
            shared
                .media_services
                .assemble_conversion(&persisted.options, web_invocation_capabilities(&persisted))
                .map_err(|error| WebTaskError::Io(error.to_string()))?
                .map(into_markdown::default_engine_with_services)
                .transpose()
                .map_err(|error| WebTaskError::Io(error.to_string()))?
        };
        let engine = routed_engine.as_ref().unwrap_or(&shared.engine);
        let mut request = ConversionRequest::new(InputRef::bytes(
            Arc::<[u8]>::from(bytes),
            Some(persisted.name.clone()),
        ));
        request.hint = persisted.hint;
        request.options = persisted.options;
        request.execution = execution;
        let converted = futures::executor::block_on(engine.convert_recoverable(
            request,
            &shared.recovery,
            &job.token,
        ))
        .map_err(|error| {
            if job.cancellation.is_cancelled() {
                WebTaskError::Cancelled
            } else {
                WebTaskError::Io(error.to_string())
            }
        })?;
        let current = lock(&shared.task_store).get(&job.id)?.ok_or(WebTaskError::NotFound)?;
        if current.status == TaskStatus::Running {
            let converted_record = lock(&shared.task_store).transition(
                &job.id,
                TaskTransition {
                    expected: TaskStatus::Running,
                    next: TaskStatus::Converted,
                    progress_millionths: 900_000,
                    diagnostics: Vec::new(),
                    artifacts: Vec::new(),
                },
            )?;
            shared.events.publish_snapshot(&converted_record);
        }
        if job.cancellation.is_cancelled() {
            return Err(WebTaskError::Cancelled);
        }
        let artifacts = publish_result(shared, &job.id, &converted, &job.cancellation)?;
        if promote_published_success(shared, &job.id, &artifacts).is_err() {
            // The immutable publication won, but persistence cannot currently
            // record it. Stop admission rather than silently leaving this
            // process healthy with a permanently Converted task.
            stop_unhealthy(shared);
            return Ok(());
        }
        crash_hook("after-taskstore-success");
        Ok(())
    })();
    if let Err(error) = result {
        let published = shared
            .objects
            .open_child_private(std::ffi::OsStr::new(job.id.as_str()))
            .and_then(|task| task.open_child_private_optional(std::ffi::OsStr::new("published")))
            .ok()
            .flatten();
        if published.is_some() {
            reconcile_or_fail(shared, &job.id);
            return;
        }
        if matches!(error, WebTaskError::Cancelled) {
            finish_terminal_or_stop(shared, &job.id, true);
        } else {
            finish_terminal_or_stop(shared, &job.id, false);
        }
    }
}

fn web_invocation_capabilities(
    request: &PersistedRequest,
) -> crate::services::InvocationCapabilities {
    let format = request.hint.format.or_else(|| {
        request.hint.extension.as_deref().and_then(InputFormat::from_extension).or_else(|| {
            Path::new(&request.name)
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(InputFormat::from_extension)
        })
    });
    crate::services::InvocationCapabilities {
        ocr: request.options.ocr.policy != OcrPolicy::Off
            || request.options.ai.vision_ocr != AiMode::Off,
        transcription: matches!(format, Some(InputFormat::Audio | InputFormat::Video))
            || request.options.ai.audio_transcription != AiMode::Off,
        diarization: request.options.diarization.enabled,
        legacy_office: matches!(
            format,
            Some(InputFormat::Doc | InputFormat::Xls | InputFormat::Ppt)
        ),
    }
}

fn promote_published_success(
    shared: &Shared,
    id: &TaskId,
    artifacts: &[ArtifactReference],
) -> Result<(), WebTaskError> {
    for _ in 0..3 {
        #[cfg(test)]
        let injected = shared
            .success_transition_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| remaining.checked_sub(1))
            .is_ok();
        #[cfg(not(test))]
        let injected = false;
        if !injected {
            let mut transition_artifacts = Vec::new();
            transition_artifacts
                .try_reserve_exact(artifacts.len())
                .map_err(|_| WebTaskError::Limit("artifact transition allocation failed".into()))?;
            transition_artifacts.extend_from_slice(artifacts);
            let transition = metadata_store_mutation(shared, MAX_TASK_METADATA_GROWTH, |store| {
                Ok(store.transition(
                    id,
                    TaskTransition {
                        expected: TaskStatus::Converted,
                        next: TaskStatus::Succeeded,
                        progress_millionths: 1_000_000,
                        diagnostics: Vec::new(),
                        artifacts: transition_artifacts,
                    },
                )?)
            });
            if let Ok(record) = transition {
                shared.events.publish_snapshot(&record);
                return Ok(());
            }
        }
        if let Some(record) = lock(&shared.task_store).get(id)?
            && record.status == TaskStatus::Succeeded
            && artifact_sets_equal(&record.artifacts, artifacts)
        {
            return Ok(());
        }
        std::thread::yield_now();
    }
    Err(WebTaskError::Io("published success transition did not converge".into()))
}

fn finish_terminal_or_stop(shared: &Shared, id: &TaskId, cancelled: bool) -> bool {
    for _ in 0..3 {
        let result = if cancelled { cancel_durable(shared, id) } else { fail_durable(shared, id) };
        match result {
            Ok(()) => return true,
            Err(WebTaskError::Limit(_) | WebTaskError::Unsafe(_)) => break,
            Err(_) => {}
        }
        std::thread::yield_now();
    }
    stop_unhealthy(shared);
    false
}

fn retry_dequeued_get(shared: &Shared, id: &TaskId) -> Option<TaskRecord> {
    for _ in 0..3 {
        #[cfg(test)]
        if shared
            .dequeue_get_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| value.checked_sub(1))
            .is_ok()
        {
            std::thread::yield_now();
            continue;
        }
        match lock(&shared.task_store).get(id) {
            Ok(Some(record)) => return Some(record),
            Ok(None) | Err(_) => std::thread::yield_now(),
        }
    }
    None
}

fn retry_dequeued_transition(shared: &Shared, id: &TaskId, transition: &TaskTransition) -> bool {
    for _ in 0..3 {
        #[cfg(test)]
        if shared
            .dequeue_transition_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| value.checked_sub(1))
            .is_ok()
        {
            std::thread::yield_now();
            continue;
        }
        if let Ok(record) = lock(&shared.task_store).transition(id, transition.clone()) {
            shared.events.publish_snapshot(&record);
            return true;
        }
        std::thread::yield_now();
    }
    false
}

fn stop_unhealthy(shared: &Shared) {
    let mut queue = lock(&shared.queue);
    queue.stopped = true;
    for cancellation in queue.cancellations.values() {
        cancellation.cancel();
    }
    shared.queue_changed.notify_all();
    shared.disk_changed.notify_all();
}

#[allow(clippy::too_many_lines)]
fn publish_result(
    shared: &Shared,
    id: &TaskId,
    result: &into_markdown::ConversionResult,
    cancellation: &CancellationToken,
) -> Result<Vec<ArtifactReference>, WebTaskError> {
    publish_result_named(shared, id, result, cancellation, "published")
}

#[allow(clippy::too_many_lines)]
fn publish_result_named(
    shared: &Shared,
    id: &TaskId,
    result: &into_markdown::ConversionResult,
    cancellation: &CancellationToken,
    publication_name: &str,
) -> Result<Vec<ArtifactReference>, WebTaskError> {
    if publication_name != "published"
        && publication_name
            .strip_prefix("published-")
            .and_then(|value| value.parse::<u64>().ok())
            .is_none()
    {
        return Err(WebTaskError::Invalid("artifact publication name is invalid".into()));
    }
    cancelled(cancellation)?;
    let markdown = result.markdown.as_bytes();
    {
        let disk = lock(&shared.disk_bytes);
        if disk.used.checked_add(disk.reserved).is_none_or(|total| total > MAX_DATA_BYTES) {
            return Err(WebTaskError::Limit(
                "artifact publication exceeds the global storage ceiling".into(),
            ));
        }
    }
    shared
        .objects
        .verify_private_namespace()
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let task = shared
        .objects
        .open_child_private(std::ffi::OsStr::new(id.as_str()))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    validate_private_directory_handle(&task)?;
    if let Some(published) = task
        .open_child_private_optional(std::ffi::OsStr::new(publication_name))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
    {
        return validate_manifest_handle_cancel(&published, Some(cancellation));
    }
    let stage_name = format!("stage-{}", random_hex()?);
    publication_failure_checkpoint(shared, 1)?;
    let stage = task
        .create_child_private(std::ffi::OsStr::new(&stage_name))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    validate_private_directory_handle(&stage)?;
    let staged = (|| {
        if result.assets.iter().filter(|asset| !asset.bytes.is_empty()).count() > 124 {
            return Err(WebTaskError::Limit("asset count exceeds 124".into()));
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(128)
            .map_err(|_| WebTaskError::Limit("artifact index allocation failed".into()))?;
        let mut staged_bytes = 0_u64;
        add_artifact(
            &stage,
            ArtifactKind::Markdown,
            markdown,
            &mut entries,
            cancellation,
            &mut staged_bytes,
            &shared.write_failure_after,
            &shared.publication_failure,
            None,
        )?;
        add_json_artifact(
            &stage,
            ArtifactKind::DocumentIr,
            &result.document,
            &mut entries,
            cancellation,
            &mut staged_bytes,
            &shared.write_failure_after,
            &shared.publication_failure,
        )?;
        add_json_artifact(
            &stage,
            ArtifactKind::Diagnostics,
            &DiagnosticsArtifact { schema_version: 1, diagnostics: &result.diagnostics },
            &mut entries,
            cancellation,
            &mut staged_bytes,
            &shared.write_failure_after,
            &shared.publication_failure,
        )?;
        for asset in &result.assets {
            if !asset.bytes.is_empty() {
                add_artifact(
                    &stage,
                    ArtifactKind::Asset,
                    &asset.bytes,
                    &mut entries,
                    cancellation,
                    &mut staged_bytes,
                    &shared.write_failure_after,
                    &shared.publication_failure,
                    Some((
                        asset.id.0.as_str(),
                        asset.filename.as_deref().unwrap_or(asset.id.0.as_str()),
                        asset.media_type.as_str(),
                    )),
                )?;
            }
        }
        add_bundle_artifact(
            &stage,
            result,
            &mut entries,
            cancellation,
            &mut staged_bytes,
            &shared.write_failure_after,
            &shared.publication_failure,
        )?;
        let manifest = bounded_json(
            &ArtifactManifest { schema_version: 1, entries: &entries },
            64 * 1024,
            "artifact manifest",
        )?;
        write_private_handle_budgeted(
            &stage,
            "manifest.json",
            &manifest,
            cancellation,
            &mut staged_bytes,
            &shared.write_failure_after,
            &shared.publication_failure,
        )?;
        publication_failure_checkpoint(shared, 3)?;
        stage.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        crash_hook("after-stage-fsync");
        cancelled(cancellation)?;
        Ok::<_, WebTaskError>((entries, staged_bytes))
    })();
    let (entries, staged_bytes) = match staged {
        Ok(staged) => staged,
        Err(error) => {
            let cleanup = remove_owned_stage(&stage, &[]).and_then(|()| {
                task.remove_empty_child_private(std::ffi::OsStr::new(&stage_name))
                    .map_err(|cleanup| WebTaskError::Unsafe(cleanup.to_string()))
            });
            if let Err(cleanup) = cleanup {
                return Err(WebTaskError::Unsafe(format!(
                    "artifact staging failed ({error}); cleanup also failed ({cleanup})"
                )));
            }
            return Err(error);
        }
    };
    let renamed = publication_failure_checkpoint(shared, 4).and_then(|()| {
        task.rename_child_private_no_replace(
            std::ffi::OsStr::new(&stage_name),
            std::ffi::OsStr::new(publication_name),
        )
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))
    });
    if let Err(error) = renamed {
        remove_owned_stage(&stage, &[])?;
        task.remove_empty_child_private(std::ffi::OsStr::new(&stage_name))
            .map_err(|cleanup| WebTaskError::Unsafe(cleanup.to_string()))?;
        return Err(error);
    }
    crash_hook("after-published-rename");
    let _ = staged_bytes;
    Ok(entries)
}

fn publication_directory_name(record: &TaskRecord) -> String {
    if record.artifact_generation == 0 {
        "published".into()
    } else {
        format!("published-{}", record.artifact_generation)
    }
}

fn cleanup_unselected_publications(
    task: &SafeDir,
    record: &TaskRecord,
) -> Result<(), WebTaskError> {
    let selected = publication_directory_name(record);
    let names = task.names_private().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    for name in names {
        let Some(text) = name.to_str() else {
            return Err(WebTaskError::Unsafe("task member name is not UTF-8".into()));
        };
        let publication_generation = if text == "published" {
            Some(0)
        } else {
            text.strip_prefix("published-")
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|generation| *generation > 0)
                .filter(|generation| format!("published-{generation}") == text)
        };
        if publication_generation.is_none() || text == selected {
            continue;
        }
        let directory = task
            .open_child_private(&name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        remove_private_files(&directory)?;
        directory.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        task.remove_empty_child_private(&name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    }
    task.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))
}

fn validate_speaker_assignment(speaker: &str, label: &str) -> Result<(), WebTaskError> {
    let number = speaker
        .strip_prefix("speaker-")
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| (1..=64).contains(value));
    if number.is_none()
        || label.is_empty()
        || label.chars().count() > 80
        || label.trim() != label
        || label.chars().any(char::is_control)
    {
        return Err(WebTaskError::Invalid("speaker assignment is invalid".into()));
    }
    Ok(())
}

fn default_speaker_name(speaker: &str) -> String {
    speaker
        .strip_prefix("speaker-")
        .and_then(|value| value.parse::<u8>().ok())
        .map_or_else(|| speaker.to_owned(), |value| format!("Speaker {value}"))
}

fn document_speaker_ids(document: &into_markdown::Document) -> Vec<String> {
    fn visit(
        blocks: &[into_markdown::BlockNode],
        seen: &mut std::collections::BTreeSet<String>,
        values: &mut Vec<String>,
    ) {
        for node in blocks {
            match &node.block {
                into_markdown::Block::TimedSegment { speaker: Some(speaker), .. } => {
                    if seen.insert(speaker.clone()) {
                        values.push(speaker.clone());
                    }
                }
                into_markdown::Block::List { items, .. } => {
                    for item in items {
                        visit(&item.blocks, seen, values);
                    }
                }
                into_markdown::Block::Table { rows, .. } => {
                    for row in rows {
                        for cell in &row.cells {
                            visit(&cell.blocks, seen, values);
                        }
                    }
                }
                into_markdown::Block::Footnote { blocks, .. }
                | into_markdown::Block::Page { blocks, .. }
                | into_markdown::Block::Slide { blocks, .. }
                | into_markdown::Block::Sheet { blocks, .. } => visit(blocks, seen, values),
                _ => {}
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut ordered = Vec::new();
    visit(&document.blocks, &mut seen, &mut ordered);
    ordered
}

#[derive(Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OwnedManifest {
    schema_version: u32,
    entries: Vec<ArtifactReference>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactManifest<'a> {
    schema_version: u32,
    entries: &'a [ArtifactReference],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticsArtifact<'a> {
    schema_version: u32,
    diagnostics: &'a [into_markdown::Diagnostic],
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OwnedDiagnosticsArtifact {
    schema_version: u32,
    diagnostics: Vec<into_markdown::Diagnostic>,
}

fn load_diagnostics_artifact(
    backend: &WebTaskBackend,
    id: &TaskId,
    record: &TaskRecord,
) -> Result<Vec<into_markdown::Diagnostic>, WebTaskError> {
    let Some(reference) =
        record.artifacts.iter().find(|artifact| artifact.kind == ArtifactKind::Diagnostics)
    else {
        // Successful records created before diagnostics became a required Web
        // artifact remain relabelable. They had no durable diagnostics to keep.
        return Ok(Vec::new());
    };
    if reference.byte_len > into_markdown::MAX_DTO_JSON_BYTES as u64 {
        return Err(WebTaskError::Limit("diagnostics exceed the rerender limit".into()));
    }
    let (mut snapshot, _) = backend.artifact(id, &reference.storage_key)?;
    let length = usize::try_from(reference.byte_len)
        .map_err(|_| WebTaskError::Limit("diagnostics exceed addressable memory".into()))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| WebTaskError::Limit("diagnostics allocation failed".into()))?;
    snapshot.read_to_end(&mut bytes)?;
    if bytes.len() != length {
        return Err(WebTaskError::Unsafe("diagnostics snapshot length changed".into()));
    }
    validate_json_shape(
        &bytes,
        into_markdown::MAX_DTO_DEPTH,
        into_markdown::MAX_DTO_VALUES,
        into_markdown::MAX_DTO_TOTAL_STRING_BYTES,
    )?;
    let artifact: OwnedDiagnosticsArtifact = serde_json::from_slice(&bytes)
        .map_err(|_| WebTaskError::Unsafe("diagnostics artifact is invalid".into()))?;
    if artifact.schema_version != 1
        || artifact.diagnostics.len() > into_markdown::MAX_DTO_DIAGNOSTICS
    {
        return Err(WebTaskError::Unsafe("diagnostics artifact is incompatible".into()));
    }
    Ok(artifact.diagnostics)
}

fn document_provenance(
    document: &into_markdown::Document,
) -> Result<Vec<into_markdown::Provenance>, WebTaskError> {
    fn visit(
        blocks: &[into_markdown::BlockNode],
        values: &mut Vec<into_markdown::Provenance>,
    ) -> Result<(), WebTaskError> {
        for node in blocks {
            if values.len() >= into_markdown::MAX_DTO_PROVENANCE {
                return Err(WebTaskError::Limit("provenance inventory exceeds its limit".into()));
            }
            values
                .try_reserve(1)
                .map_err(|_| WebTaskError::Limit("provenance allocation failed".into()))?;
            values.push(node.provenance.clone());
            match &node.block {
                into_markdown::Block::List { items, .. } => {
                    for item in items {
                        visit(&item.blocks, values)?;
                    }
                }
                into_markdown::Block::Table { rows, .. } => {
                    for row in rows {
                        for cell in &row.cells {
                            visit(&cell.blocks, values)?;
                        }
                    }
                }
                into_markdown::Block::Footnote { blocks, .. }
                | into_markdown::Block::Page { blocks, .. }
                | into_markdown::Block::Slide { blocks, .. }
                | into_markdown::Block::Sheet { blocks, .. } => visit(blocks, values)?,
                _ => {}
            }
        }
        Ok(())
    }

    let mut values = Vec::new();
    visit(&document.blocks, &mut values)?;
    Ok(values)
}

fn validate_manifest_handle(directory: &SafeDir) -> Result<Vec<ArtifactReference>, WebTaskError> {
    validate_manifest_handle_cancel(directory, None)
}

fn artifact_sets_equal(left: &[ArtifactReference], right: &[ArtifactReference]) -> bool {
    left.len() == right.len()
        && left.iter().all(|entry| {
            right.iter().any(|candidate| {
                candidate.storage_key == entry.storage_key
                    && candidate.kind == entry.kind
                    && candidate.byte_len == entry.byte_len
                    && constant_time_equal(&candidate.sha256, &entry.sha256)
                    && candidate.asset_id == entry.asset_id
                    && candidate.filename == entry.filename
                    && candidate.media_type == entry.media_type
            })
        })
}

fn validate_manifest_handle_cancel(
    directory: &SafeDir,
    cancellation: Option<&CancellationToken>,
) -> Result<Vec<ArtifactReference>, WebTaskError> {
    validate_private_directory_handle(directory)?;
    let manifest_file = directory
        .open_regular_private(std::ffi::OsStr::new("manifest.json"))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    validate_private_file(&manifest_file)?;
    let bytes = read_file_bounded(manifest_file, 64 * 1024)?;
    validate_json_shape(&bytes, 16, 4096, 4096)?;
    let manifest: OwnedManifest = serde_json::from_slice(&bytes)
        .map_err(|error| WebTaskError::Unsafe(format!("invalid artifact manifest: {error}")))?;
    validate_manifest_metadata(&manifest)?;
    let mut member_count = 0_usize;
    let mut total_bytes = 0_u64;
    for member in
        directory.names_private().map_err(|error| WebTaskError::Unsafe(error.to_string()))?
    {
        let name = member
            .into_string()
            .map_err(|_| WebTaskError::Unsafe("artifact member name is not UTF-8".into()))?;
        if name != "manifest.json"
            && !manifest.entries.iter().any(|entry| entry.storage_key == name)
        {
            return Err(WebTaskError::Unsafe("published artifact set has an extra member".into()));
        }
        member_count = member_count
            .checked_add(1)
            .ok_or_else(|| WebTaskError::Limit("artifact member count overflow".into()))?;
    }
    if member_count != manifest.entries.len() + 1 {
        return Err(WebTaskError::Unsafe("published artifact set is incomplete".into()));
    }
    for entry in &manifest.entries {
        if let Some(cancellation) = cancellation {
            cancelled(cancellation)?;
        }
        total_bytes = total_bytes
            .checked_add(entry.byte_len)
            .ok_or_else(|| WebTaskError::Unsafe("artifact manifest byte total overflow".into()))?;
        if total_bytes > MAX_TASK_DURABLE_GROWTH {
            return Err(WebTaskError::Unsafe(
                "Web artifact manifest byte total exceeds 512 MiB".into(),
            ));
        }
        validate_key(&entry.storage_key)?;
        let mut file = directory
            .open_regular_private(std::ffi::OsStr::new(&entry.storage_key))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_file(&file)?;
        let metadata = file.metadata()?;
        if metadata.len() != entry.byte_len || link_count(&metadata) != 1 {
            return Err(WebTaskError::Unsafe("published artifact identity changed".into()));
        }
        let digest = digest_reader_cancel(&mut file, entry.byte_len, cancellation)?;
        if digest != entry.sha256 {
            return Err(WebTaskError::Unsafe("published artifact digest changed".into()));
        }
    }
    Ok(manifest.entries)
}

fn validate_manifest_metadata(manifest: &OwnedManifest) -> Result<(), WebTaskError> {
    if manifest.schema_version != 1 || manifest.entries.len() > 128 {
        return Err(WebTaskError::Unsafe("artifact manifest limits are invalid".into()));
    }
    for (index, entry) in manifest.entries.iter().enumerate() {
        if entry.byte_len > MAX_TASK_DURABLE_GROWTH
            || entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(WebTaskError::Unsafe(
                "artifact manifest contains non-canonical metadata".into(),
            ));
        }
        if manifest.entries[..index].iter().any(|prior| prior.storage_key == entry.storage_key) {
            return Err(WebTaskError::Unsafe("artifact manifest repeats a storage key".into()));
        }
    }
    for required in [
        ArtifactKind::Markdown,
        ArtifactKind::DocumentIr,
        ArtifactKind::Diagnostics,
        ArtifactKind::Bundle,
    ] {
        if manifest.entries.iter().filter(|entry| entry.kind == required).count() != 1 {
            return Err(WebTaskError::Unsafe(
                "artifact manifest has an invalid required set".into(),
            ));
        }
    }
    let asset_count =
        manifest.entries.iter().filter(|entry| entry.kind == ArtifactKind::Asset).count();
    if asset_count > 124 {
        return Err(WebTaskError::Unsafe("artifact manifest has too many assets".into()));
    }
    for (index, asset) in manifest.entries.iter().enumerate() {
        if asset.kind != ArtifactKind::Asset {
            continue;
        }
        let (Some(asset_id), Some(filename), Some(media_type)) =
            (asset.asset_id.as_deref(), asset.filename.as_deref(), asset.media_type.as_deref())
        else {
            return Err(WebTaskError::Unsafe("asset metadata is incomplete".into()));
        };
        validate_asset_metadata(asset_id, filename, media_type)?;
        if manifest.entries[..index].iter().any(|prior| {
            prior.kind == ArtifactKind::Asset && prior.asset_id.as_deref() == Some(asset_id)
        }) {
            return Err(WebTaskError::Unsafe("artifact manifest repeats an asset ID".into()));
        }
    }
    if manifest.entries.iter().any(|entry| {
        entry.kind != ArtifactKind::Asset
            && (entry.asset_id.is_some() || entry.filename.is_some() || entry.media_type.is_some())
    }) {
        return Err(WebTaskError::Unsafe("non-asset metadata is present".into()));
    }
    Ok(())
}

fn validate_asset_metadata(
    asset_id: &str,
    filename: &str,
    media_type: &str,
) -> Result<(), WebTaskError> {
    for (label, value) in
        [("asset ID", asset_id), ("asset filename", filename), ("media type", media_type)]
    {
        if value.is_empty() || value.len() > 255 || value.chars().any(char::is_control) {
            return Err(WebTaskError::Unsafe(format!("{label} is not canonical")));
        }
    }
    if filename == "."
        || filename == ".."
        || filename.contains('/')
        || filename.contains('\\')
        || !media_type.is_ascii()
        || !media_type.contains('/')
        || media_type.bytes().any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_uppercase())
    {
        return Err(WebTaskError::Unsafe("asset filename or media type is not canonical".into()));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_artifact(
    directory: &SafeDir,
    kind: ArtifactKind,
    bytes: &[u8],
    entries: &mut Vec<ArtifactReference>,
    cancellation: &CancellationToken,
    staged_bytes: &mut u64,
    write_failure_after: &AtomicUsize,
    publication_failure: &AtomicUsize,
    asset_metadata: Option<(&str, &str, &str)>,
) -> Result<(), WebTaskError> {
    cancelled(cancellation)?;
    if entries.len() >= 128 {
        return Err(WebTaskError::Limit("artifact count exceeds 128".into()));
    }
    entries
        .try_reserve(1)
        .map_err(|_| WebTaskError::Limit("artifact index allocation failed".into()))?;
    let key = random_hex()?;
    let asset_filename = asset_metadata.map(|metadata| {
        if validate_display_name(metadata.1).is_ok() { metadata.1.to_owned() } else { key.clone() }
    });
    write_private_handle_budgeted(
        directory,
        &key,
        bytes,
        cancellation,
        staged_bytes,
        write_failure_after,
        publication_failure,
    )?;
    entries.push(ArtifactReference {
        storage_key: key,
        kind,
        byte_len: u64::try_from(bytes.len())
            .map_err(|_| WebTaskError::Limit("artifact size overflow".into()))?,
        sha256: hex_digest_cancel(bytes, cancellation)?,
        asset_id: asset_metadata.map(|metadata| metadata.0.to_owned()),
        filename: asset_filename,
        media_type: asset_metadata.map(|metadata| metadata.2.to_owned()),
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_json_artifact<T: Serialize>(
    directory: &SafeDir,
    kind: ArtifactKind,
    value: &T,
    entries: &mut Vec<ArtifactReference>,
    cancellation: &CancellationToken,
    staged_bytes: &mut u64,
    write_failure_after: &AtomicUsize,
    publication_failure: &AtomicUsize,
) -> Result<(), WebTaskError> {
    cancelled(cancellation)?;
    if entries.len() >= 128 {
        return Err(WebTaskError::Limit("artifact count exceeds 128".into()));
    }
    entries
        .try_reserve(1)
        .map_err(|_| WebTaskError::Limit("artifact index allocation failed".into()))?;
    let key = random_hex()?;
    let mut file = directory
        .create_regular_private(std::ffi::OsStr::new(&key))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    {
        let mut writer =
            CancellableFile { file: &mut file, cancellation, staged_bytes, write_failure_after };
        serde_json::to_writer_pretty(&mut writer, value)
            .map_err(|error| WebTaskError::Io(format!("stream JSON artifact: {error}")))?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    cancelled(cancellation)?;
    publication_failure_checkpoint_atomic(publication_failure, 2)?;
    file.sync_all()?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_TASK_DURABLE_GROWTH || link_count(&metadata) != 1
    {
        return Err(WebTaskError::Limit("JSON artifact exceeds its durable byte limit".into()));
    }
    drop(file);
    let mut file = directory
        .open_regular_private(std::ffi::OsStr::new(&key))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let byte_len = metadata.len();
    let sha256 = digest_reader_cancel(&mut file, byte_len, Some(cancellation))?;
    entries.push(ArtifactReference {
        storage_key: key,
        kind,
        byte_len,
        sha256,
        asset_id: None,
        filename: None,
        media_type: None,
    });
    Ok(())
}

fn add_bundle_artifact(
    directory: &SafeDir,
    result: &into_markdown::ConversionResult,
    entries: &mut Vec<ArtifactReference>,
    cancellation: &CancellationToken,
    staged_bytes: &mut u64,
    write_failure_after: &AtomicUsize,
    publication_failure: &AtomicUsize,
) -> Result<(), WebTaskError> {
    cancelled(cancellation)?;
    if entries.len() >= 128 {
        return Err(WebTaskError::Limit("artifact count exceeds 128".into()));
    }
    entries
        .try_reserve(1)
        .map_err(|_| WebTaskError::Limit("artifact index allocation failed".into()))?;
    let key = random_hex()?;
    let mut file = directory
        .create_regular_private(std::ffi::OsStr::new(&key))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    output::write_bundle(
        result,
        CancellableFile { file: &mut file, cancellation, staged_bytes, write_failure_after },
    )
    .map_err(|error| {
        if cancellation.is_cancelled() {
            WebTaskError::Cancelled
        } else {
            WebTaskError::Io(error.to_string())
        }
    })?;
    cancelled(cancellation)?;
    file.flush()?;
    publication_failure_checkpoint_atomic(publication_failure, 2)?;
    file.sync_all()?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_TASK_DURABLE_GROWTH || link_count(&metadata) != 1
    {
        return Err(WebTaskError::Limit("bundle exceeds its durable byte limit".into()));
    }
    drop(file);
    let mut file = directory
        .open_regular_private(std::ffi::OsStr::new(&key))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let byte_len = metadata.len();
    let sha256 = digest_reader_cancel(&mut file, byte_len, Some(cancellation))?;
    entries.push(ArtifactReference {
        storage_key: key,
        kind: ArtifactKind::Bundle,
        byte_len,
        sha256,
        asset_id: None,
        filename: None,
        media_type: None,
    });
    Ok(())
}

fn fail_durable(shared: &Shared, id: &TaskId) -> Result<(), WebTaskError> {
    #[cfg(test)]
    if shared
        .fail_transition_failures
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| remaining.checked_sub(1))
        .is_ok()
    {
        return Err(WebTaskError::Io("injected failed transition error".into()));
    }
    terminal_transition(shared, id, TaskStatus::Failed, DiagnosticCode::ConversionFailed)
}

fn reconcile_worker_panic(shared: &Shared, id: &TaskId) -> Result<(), WebTaskError> {
    let task = shared
        .objects
        .open_child_private(std::ffi::OsStr::new(id.as_str()))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    validate_private_directory_handle(&task)?;
    let published = task
        .open_child_private_optional(std::ffi::OsStr::new("published"))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    if let Some(published) = published {
        let artifacts = validate_manifest_handle(&published)?;
        let record = lock(&shared.task_store).get(id)?.ok_or(WebTaskError::NotFound)?;
        if record.status == TaskStatus::Converted {
            promote_published_success(shared, id, &artifacts)?;
            return Ok(());
        }
        if record.status == TaskStatus::Succeeded
            && artifact_sets_equal(&record.artifacts, &artifacts)
        {
            return Ok(());
        }
        return Err(WebTaskError::Conflict(
            "published artifacts exist outside the converted publication boundary".into(),
        ));
    }
    fail_durable(shared, id)
}

fn reconcile_or_fail(shared: &Shared, id: &TaskId) {
    #[cfg(test)]
    let injected_reconcile_failure = shared
        .reconcile_failures
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| remaining.checked_sub(1))
        .is_ok();
    #[cfg(not(test))]
    let injected_reconcile_failure = false;
    if injected_reconcile_failure || reconcile_worker_panic(shared, id).is_err() {
        // An invalid or out-of-phase published directory never wins the
        // publication boundary. Stabilize every nonterminal record as failed.
        for _ in 0..3 {
            if fail_durable(shared, id).is_ok() && quarantine_invalid_published(shared, id).is_ok()
            {
                return;
            }
            std::thread::yield_now();
        }
        // Persistence is unhealthy. Do not kill this worker or claim success;
        // stop further dequeue until a restart can perform durable recovery.
        let mut queue = lock(&shared.queue);
        queue.stopped = true;
        drop(queue);
        shared.queue_changed.notify_all();
    }
}

fn quarantine_invalid_published(shared: &Shared, id: &TaskId) -> Result<(), WebTaskError> {
    let task = shared
        .objects
        .open_child_private(std::ffi::OsStr::new(id.as_str()))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let published = task
        .open_child_private_optional(std::ffi::OsStr::new("published"))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    if let Some(published) = published {
        remove_private_files(&published)?;
        published.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        task.remove_empty_child_private(std::ffi::OsStr::new("published"))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    }
    let mut disk = lock(&shared.disk_bytes);
    disk.used = measured_managed_bytes(&shared.root_handle)?;
    shared.disk_changed.notify_all();
    Ok(())
}

fn remove_private_files(directory: &SafeDir) -> Result<(), WebTaskError> {
    let names =
        directory.names_private().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    for name in &names {
        let file = directory
            .open_regular_private(name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        validate_private_file(&file)?;
    }
    for name in names {
        directory
            .remove_regular_private(&name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    }
    Ok(())
}

fn cancel_durable(shared: &Shared, id: &TaskId) -> Result<(), WebTaskError> {
    terminal_transition(shared, id, TaskStatus::Cancelled, DiagnosticCode::Cancelled)
}

fn terminal_transition(
    shared: &Shared,
    id: &TaskId,
    next: TaskStatus,
    code: DiagnosticCode,
) -> Result<(), WebTaskError> {
    let changed = metadata_store_mutation(shared, STORE_MUTATION_RESERVATION, |store| {
        let record = store.get(id)?.ok_or(WebTaskError::NotFound)?;
        if matches!(
            record.status,
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::Converted
        ) {
            return Ok(Some(store.transition(
                id,
                TaskTransition {
                    expected: record.status,
                    next,
                    progress_millionths: record.progress_millionths,
                    diagnostics: vec![TaskDiagnostic { code }],
                    artifacts: Vec::new(),
                },
            )?));
        }
        Ok(None)
    })?;
    if let Some(record) = changed {
        shared.events.publish_snapshot(&record);
    }
    Ok(())
}

fn metadata_store_mutation<T>(
    shared: &Shared,
    reservation: u64,
    mutation: impl FnOnce(&mut TaskStore) -> Result<T, WebTaskError>,
) -> Result<T, WebTaskError> {
    // Keep the quota lock across the bounded SQLite transaction. This makes
    // metadata writers single-file and prevents uploads or other reservations
    // from spending the physical headroom between preflight and commit.
    let mut quota = lock(&shared.disk_bytes);
    let measured_before = match measured_metadata_bytes(shared) {
        Ok(measured) => measured,
        Err(error) => {
            drop(quota);
            stop_unhealthy(shared);
            return Err(error);
        }
    };
    let Some(planned_data) = quota.used.checked_add(quota.reserved) else {
        drop(quota);
        stop_unhealthy(shared);
        return Err(WebTaskError::Limit("global storage accounting overflow".into()));
    };
    let occupied = measured_before.max(planned_data);
    if occupied.checked_add(reservation).is_none_or(|total| total > MAX_GLOBAL_BYTES) {
        drop(quota);
        stop_unhealthy(shared);
        return Err(WebTaskError::Limit("durable task metadata reservation is unavailable".into()));
    }
    quota.used = measured_before;
    let result = mutation(&mut lock(&shared.task_store));
    let measured_after = match measured_metadata_bytes(shared) {
        Ok(measured) => measured,
        Err(error) => {
            quota.used = quota.used.saturating_add(reservation).min(MAX_GLOBAL_BYTES);
            drop(quota);
            stop_unhealthy(shared);
            return Err(WebTaskError::Unsafe(format!(
                "cannot reconcile durable task metadata: {error}"
            )));
        }
    };
    let growth = measured_after.saturating_sub(measured_before);
    quota.used = measured_after;
    shared.disk_changed.notify_all();
    if growth > reservation || measured_after > MAX_GLOBAL_BYTES {
        drop(quota);
        stop_unhealthy(shared);
        return Err(WebTaskError::Unsafe(
            "durable task metadata exceeded its physical reservation".into(),
        ));
    }
    result
}

fn measured_metadata_bytes(shared: &Shared) -> Result<u64, WebTaskError> {
    measured_live_managed_bytes(&shared.root_handle)
}

#[cfg(test)]
fn reconcile_managed_usage(shared: &Shared) -> Result<(), WebTaskError> {
    let mut quota = lock(&shared.disk_bytes);
    let Ok(measured) = measured_managed_bytes(&shared.root_handle) else {
        quota.used = quota.used.saturating_add(STORE_METADATA_HEADROOM).min(MAX_GLOBAL_BYTES);
        shared.disk_changed.notify_all();
        return Ok(());
    };
    quota.used = measured;
    if quota.used > MAX_GLOBAL_BYTES {
        return Err(WebTaskError::Limit("managed storage exceeds the global ceiling".into()));
    }
    shared.disk_changed.notify_all();
    Ok(())
}

fn web_options() -> ConversionOptions {
    let mut options = ConversionOptions::default();
    options.limits.max_input_bytes = MAX_FILE_BYTES;
    options.limits.max_asset_bytes = 64 * 1024 * 1024;
    options.limits.max_total_asset_bytes = 128 * 1024 * 1024;
    options.limits.max_memory_bytes = MAX_CHECKPOINT_BYTES;
    options.limits.max_temporary_bytes = MAX_CHECKPOINT_BYTES;
    options
}

fn validate_display_name(name: &str) -> Result<(), WebTaskError> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.chars().any(char::is_control)
        || name.contains('/')
        || name.contains('\\')
        || matches!(name, "." | "..")
    {
        return Err(WebTaskError::Unsafe("display filename is invalid".into()));
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), WebTaskError> {
    if key.len() != 32
        || !key.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(WebTaskError::Unsafe("artifact ID is not canonical".into()));
    }
    Ok(())
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    constant_time_equal_bytes(left.as_bytes(), right.as_bytes())
}

fn constant_time_equal_bytes(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right).fold(0_u8, |difference, (left, right)| difference | (left ^ right)) == 0
}

fn cancelled(token: &CancellationToken) -> Result<(), WebTaskError> {
    if token.is_cancelled() { Err(WebTaskError::Cancelled) } else { Ok(()) }
}

#[cfg(test)]
fn crash_hook(phase: &str) {
    if std::env::var_os("INTO_MD_WEB_CRASH_PHASE").as_deref() == Some(std::ffi::OsStr::new(phase)) {
        std::process::abort();
    }
}

#[cfg(not(test))]
fn crash_hook(_phase: &str) {}

fn random_hex() -> Result<String, WebTaskError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| WebTaskError::Io(format!("generate storage capability: {error}")))?;
    let mut output = String::new();
    output
        .try_reserve_exact(32)
        .map_err(|_| WebTaskError::Limit("storage capability allocation failed".into()))?;
    for byte in bytes {
        write!(&mut output, "{byte:02x}")
            .map_err(|_| WebTaskError::Limit("storage capability allocation failed".into()))?;
    }
    Ok(output)
}

fn read_file_bounded(file: File, limit: u64) -> Result<Vec<u8>, WebTaskError> {
    let size = file.metadata()?.len();
    if size > limit {
        return Err(WebTaskError::Limit("managed file exceeds its byte limit".into()));
    }
    let capacity = usize::try_from(size)
        .map_err(|_| WebTaskError::Limit("managed file size is not representable".into()))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| WebTaskError::Limit("managed file allocation failed".into()))?;
    bytes.resize(capacity, 0);
    let mut reader = file;
    reader.read_exact(&mut bytes)?;
    let mut probe = [0_u8; 1];
    if reader.read(&mut probe)? != 0 || reader.metadata()?.len() != size {
        return Err(WebTaskError::Unsafe("managed file changed while reading".into()));
    }
    Ok(bytes)
}

fn validate_private_file(file: &File) -> Result<(), WebTaskError> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(WebTaskError::Unsafe(
                "managed file is not private, owner-bound, and singly linked".into(),
            ));
        }
    }
    #[cfg(not(unix))]
    return Err(WebTaskError::Unsafe("private file validation is unavailable".into()));
    #[cfg(unix)]
    Ok(())
}

fn validate_private_directory_handle(directory: &SafeDir) -> Result<(), WebTaskError> {
    directory.verify_private_namespace().map_err(|error| WebTaskError::Unsafe(error.to_string()))
}

fn validate_json_shape(
    bytes: &[u8],
    max_depth: usize,
    max_structural_tokens: usize,
    max_string_bytes: usize,
) -> Result<(), WebTaskError> {
    let mut depth = 0_usize;
    let mut structural = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_bytes = 0_usize;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            } else {
                string_bytes = string_bytes
                    .checked_add(1)
                    .ok_or_else(|| WebTaskError::Limit("JSON string length overflow".into()))?;
                if string_bytes > max_string_bytes {
                    return Err(WebTaskError::Limit("JSON string exceeds its limit".into()));
                }
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string_bytes = 0;
            }
            b'{' | b'[' => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| WebTaskError::Limit("JSON depth overflow".into()))?;
                if depth > max_depth {
                    return Err(WebTaskError::Limit("JSON nesting exceeds its limit".into()));
                }
                structural += 1;
            }
            b'}' | b']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| WebTaskError::Unsafe("JSON delimiters are unbalanced".into()))?;
                structural += 1;
            }
            b',' | b':' => structural += 1,
            _ => {}
        }
        if structural > max_structural_tokens {
            return Err(WebTaskError::Limit("JSON width exceeds its limit".into()));
        }
    }
    if in_string || escaped || depth != 0 {
        return Err(WebTaskError::Unsafe("JSON structure is incomplete".into()));
    }
    Ok(())
}

fn bounded_json<T: Serialize>(
    value: &T,
    limit: usize,
    context: &str,
) -> Result<Vec<u8>, WebTaskError> {
    let mut writer = BoundedVecWriter::new(limit)?;
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| WebTaskError::Limit(format!("encode {context}: {error}")))?;
    Ok(writer.bytes)
}

fn load_persisted_request(task: &SafeDir) -> Result<Option<PersistedRequest>, WebTaskError> {
    let Some(request_file) = task
        .open_regular_optional(std::ffi::OsStr::new("request.json"))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
    else {
        return Ok(None);
    };
    validate_private_file(&request_file)?;
    let request_bytes = read_file_bounded(request_file, 64 * 1024)?;
    validate_json_shape(&request_bytes, 32, 4096, 4096)?;
    let request: PersistedRequest = serde_json::from_slice(&request_bytes)
        .map_err(|error| WebTaskError::Unsafe(format!("invalid persisted request: {error}")))?;
    if request.schema_version != 1
        || validate_display_name(&request.name).is_err()
        || request.batch_id.as_deref().is_some_and(|value| !valid_batch_id(value))
    {
        return Err(WebTaskError::Unsafe("persisted request is incompatible".into()));
    }
    Ok(Some(request))
}

fn write_all_checked(file: &mut impl Write, bytes: &[u8]) -> Result<(), WebTaskError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match file.write(remaining)? {
            0 => return Err(WebTaskError::Io("file write made no progress".into())),
            written => remaining = &remaining[written..],
        }
    }
    Ok(())
}

#[cfg(test)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), WebTaskError> {
    let mut file = create_private_file(path)?;
    finish_private_write(&mut file, bytes)
}

fn write_private_handle(directory: &SafeDir, name: &str, bytes: &[u8]) -> Result<(), WebTaskError> {
    let mut file = directory
        .create_regular_private(std::ffi::OsStr::new(name))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    finish_private_write(&mut file, bytes)
}

fn write_private_handle_budgeted(
    directory: &SafeDir,
    name: &str,
    bytes: &[u8],
    cancellation: &CancellationToken,
    staged_bytes: &mut u64,
    write_failure_after: &AtomicUsize,
    publication_failure: &AtomicUsize,
) -> Result<(), WebTaskError> {
    let mut file = directory
        .create_regular_private(std::ffi::OsStr::new(name))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    {
        let mut writer =
            CancellableFile { file: &mut file, cancellation, staged_bytes, write_failure_after };
        for chunk in bytes.chunks(COPY_CHUNK) {
            writer.write_all(chunk)?;
        }
        writer.flush()?;
    }
    file.flush()?;
    publication_failure_checkpoint_atomic(publication_failure, 2)?;
    file.sync_all()?;
    let metadata = file.metadata()?;
    if metadata.len() != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
        || link_count(&metadata) != 1
    {
        return Err(WebTaskError::Unsafe("staged artifact identity changed".into()));
    }
    Ok(())
}

fn finish_private_write(file: &mut File, bytes: &[u8]) -> Result<(), WebTaskError> {
    write_all_checked(file, bytes)?;
    file.flush()?;
    file.sync_all()?;
    let metadata = file.metadata()?;
    if metadata.len() != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
        || link_count(&metadata) != 1
    {
        return Err(WebTaskError::Unsafe("staged artifact identity changed".into()));
    }
    Ok(())
}

fn digest_reader_cancel(
    file: &mut File,
    expected: u64,
    cancellation: Option<&CancellationToken>,
) -> Result<String, WebTaskError> {
    let mut hash = Sha256::new();
    let mut seen = 0_u64;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(COPY_CHUNK)
        .map_err(|_| WebTaskError::Limit("digest buffer allocation failed".into()))?;
    buffer.resize(COPY_CHUNK, 0);
    loop {
        if let Some(cancellation) = cancellation {
            cancelled(cancellation)?;
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        seen = seen
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| WebTaskError::Limit("artifact digest length overflow".into()))?;
        if seen > expected {
            return Err(WebTaskError::Unsafe("artifact grew while hashing".into()));
        }
        hash.update(&buffer[..read]);
    }
    if seen != expected {
        return Err(WebTaskError::Unsafe("artifact shrank while hashing".into()));
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn hex_digest_cancel(
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<String, WebTaskError> {
    let mut hash = Sha256::new();
    for chunk in bytes.chunks(COPY_CHUNK) {
        cancelled(cancellation)?;
        hash.update(chunk);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn measured_managed_bytes(root: &SafeDir) -> Result<u64, WebTaskError> {
    root.measured_tree_bytes(8, 1_000_000).map_err(|error| WebTaskError::Unsafe(error.to_string()))
}

fn measured_live_managed_bytes(root: &SafeDir) -> Result<u64, WebTaskError> {
    let mut last_error = None;
    for attempt in 0..64 {
        match measured_managed_bytes(root) {
            Ok(measured) => return Ok(measured),
            Err(error) => {
                last_error = Some(error);
                // A descriptor-bound upload or publication may atomically
                // rename a member between readdir, stat, and open. Restart the
                // complete scan after a short bounded pause so the mutation can
                // finish. Persistent unsafe namespace state still fails closed.
                if attempt != 63 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| WebTaskError::Unsafe("managed storage measurement failed".into())))
}

fn unix_now_ms() -> Result<i64, WebTaskError> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| WebTaskError::Io("system clock precedes Unix epoch".into()))?
        .as_millis();
    i64::try_from(milliseconds).map_err(|_| WebTaskError::Limit("system time overflow".into()))
}

fn recover_retention_trash(
    objects: &SafeDir,
    trash: &SafeDir,
    store: &TaskStore,
    recovery: &RecoveryStore,
) -> Result<(), WebTaskError> {
    for name in trash
        .names_bounded(MAX_QUEUE + 1)
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
    {
        let text =
            name.to_str().ok_or_else(|| WebTaskError::Unsafe("trash entry is not UTF-8".into()))?;
        let (id, token) = parse_retention_trash_name(text)?;
        if let Some(record) = store.get(&id)? {
            if !is_terminal(record.status) {
                return Err(WebTaskError::Unsafe(
                    "retention trash refers to an active task".into(),
                ));
            }
            if record.input.recovery_token != token.as_str() {
                return Err(WebTaskError::Unsafe(
                    "retention trash token does not match its durable task".into(),
                ));
            }
            if objects
                .open_child_private_optional(std::ffi::OsStr::new(id.as_str()))
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
                .is_some()
            {
                return Err(WebTaskError::Unsafe(
                    "retention recovery found duplicate task objects".into(),
                ));
            }
            validate_private_tree(trash, &name, 0)?;
            recovery
                .restore_quarantined_purge(&token)
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            trash
                .rename_child_private_to_no_replace(
                    &name,
                    objects,
                    std::ffi::OsStr::new(id.as_str()),
                )
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        } else {
            if store.recovery_token_in_use(token.as_str())? {
                return Err(WebTaskError::Unsafe(
                    "retention trash selected a recovery token owned by another task".into(),
                ));
            }
            recovery
                .remove_quarantined_purge(&token)
                .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            recovery.purge(&token).map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            remove_private_tree(trash, &name, 0)?;
        }
    }
    Ok(())
}

fn retention_trash_name(id: &TaskId, token: &RecoveryToken) -> String {
    format!("{}.{}", id.as_str(), token.as_str())
}

fn parse_retention_trash_name(value: &str) -> Result<(TaskId, RecoveryToken), WebTaskError> {
    let (id, token) = value
        .split_once('.')
        .ok_or_else(|| WebTaskError::Unsafe("retention trash name is malformed".into()))?;
    if token.contains('.') {
        return Err(WebTaskError::Unsafe("retention trash name is malformed".into()));
    }
    let id = TaskId::parse(id.to_owned())
        .map_err(|_| WebTaskError::Unsafe("retention trash task ID is invalid".into()))?;
    let token = RecoveryToken::parse(token.to_owned())
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    Ok((id, token))
}

fn remove_quarantined_task(trash: &SafeDir, name: &std::ffi::OsStr) -> Result<(), WebTaskError> {
    remove_private_tree(trash, name, 0)
}

fn validate_private_tree(
    parent: &SafeDir,
    name: &std::ffi::OsStr,
    depth: u8,
) -> Result<(), WebTaskError> {
    if depth > 3 {
        return Err(WebTaskError::Unsafe("managed task tree is too deep".into()));
    }
    let directory =
        parent.open_child_private(name).map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    for member in
        directory.names_bounded(1024).map_err(|error| WebTaskError::Unsafe(error.to_string()))?
    {
        match directory.open_regular_private(&member) {
            Ok(file) if link_count(&file.metadata()?) == 1 => {}
            Ok(_) => {
                return Err(WebTaskError::Unsafe(
                    "managed task member has an external hard link".into(),
                ));
            }
            Err(_) => {
                if directory
                    .open_child_private_optional(&member)
                    .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
                    .is_none()
                {
                    return Err(WebTaskError::Unsafe(
                        "managed task member is not a private file or directory".into(),
                    ));
                }
                validate_private_tree(&directory, &member, depth.saturating_add(1))?;
            }
        }
    }
    Ok(())
}

fn remove_private_tree(
    parent: &SafeDir,
    name: &std::ffi::OsStr,
    depth: u8,
) -> Result<(), WebTaskError> {
    if depth > 3 {
        return Err(WebTaskError::Unsafe("managed task tree is too deep".into()));
    }
    let directory =
        parent.open_child_private(name).map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let names =
        directory.names_bounded(1024).map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    for member in names {
        match directory.open_regular_private(&member) {
            Ok(file) => {
                if link_count(&file.metadata()?) != 1 {
                    return Err(WebTaskError::Unsafe(
                        "managed task member has an external hard link".into(),
                    ));
                }
                drop(file);
                directory
                    .remove_regular_private(&member)
                    .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            }
            Err(_) => {
                if directory
                    .open_child_private_optional(&member)
                    .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
                    .is_none()
                {
                    return Err(WebTaskError::Unsafe(
                        "managed task member is not a private file or directory".into(),
                    ));
                }
                remove_private_tree(&directory, &member, depth.saturating_add(1))?;
            }
        }
    }
    parent.remove_empty_child_private(name).map_err(|error| WebTaskError::Unsafe(error.to_string()))
}

fn cleanup_crash_residue(incoming: &SafeDir, objects: &SafeDir) -> Result<(), WebTaskError> {
    for name in incoming.names_private().map_err(|error| WebTaskError::Unsafe(error.to_string()))? {
        let name = name
            .into_string()
            .map_err(|_| WebTaskError::Unsafe("incoming capability name is not UTF-8".into()))?;
        validate_key(&name)?;
        let stage = incoming
            .open_child_private(std::ffi::OsStr::new(&name))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        remove_owned_stage(&stage, &["payload"])?;
        incoming
            .remove_empty_child_private(std::ffi::OsStr::new(&name))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    }
    for task_name in objects
        .names_bounded(MAX_QUEUE + 1)
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
    {
        let task_name = task_name
            .into_string()
            .map_err(|_| WebTaskError::Unsafe("task object name is not UTF-8".into()))?;
        validate_key(&task_name)?;
        let task = objects
            .open_child_private(std::ffi::OsStr::new(&task_name))
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        for name in task.names_private().map_err(|error| WebTaskError::Unsafe(error.to_string()))? {
            let name = name
                .into_string()
                .map_err(|_| WebTaskError::Unsafe("task member name is not UTF-8".into()))?;
            if name.starts_with("stage-") {
                validate_key(name.trim_start_matches("stage-"))?;
                let stage = task
                    .open_child_private(std::ffi::OsStr::new(&name))
                    .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
                remove_owned_stage(&stage, &[])?;
                task.remove_empty_child_private(std::ffi::OsStr::new(&name))
                    .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
            }
        }
    }
    incoming.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    objects.sync().map_err(|error| WebTaskError::Unsafe(error.to_string()))
}

fn cleanup_flat_private(parent: &SafeDir, member: &str) -> Result<(), WebTaskError> {
    for name in parent
        .names_bounded(MAX_QUEUE + 1)
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?
    {
        let text = name
            .to_str()
            .ok_or_else(|| WebTaskError::Unsafe("capability name is not UTF-8".into()))?;
        validate_key(text)?;
        let directory = parent
            .open_child_private(&name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        remove_owned_stage(&directory, &[member])?;
        parent
            .remove_empty_child_private(&name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    }
    Ok(())
}

fn remove_owned_stage(stage: &SafeDir, allowlist: &[&str]) -> Result<(), WebTaskError> {
    validate_private_directory_handle(stage)?;
    let names = stage.names_private().map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    for name in &names {
        let text = name
            .clone()
            .into_string()
            .map_err(|_| WebTaskError::Unsafe("stage member name is not UTF-8".into()))?;
        if !allowlist.contains(&text.as_str())
            && text != "manifest.json"
            && validate_key(&text).is_err()
        {
            return Err(WebTaskError::Unsafe("stage contains an unmanaged member".into()));
        }
        let file = stage
            .open_regular_private(name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
        if link_count(&file.metadata()?) != 1 {
            return Err(WebTaskError::Unsafe("stage member has an external hard link".into()));
        }
    }
    for name in names {
        stage
            .remove_regular_private(&name)
            .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn private_directory(path: PathBuf) -> Result<PathBuf, WebTaskError> {
    if path.components().any(|component| matches!(component, Component::ParentDir)) {
        return Err(WebTaskError::Unsafe("managed path contains parent traversal".into()));
    }
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(&path)?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
        }
        Err(error) => return Err(error.into()),
    }
    verify_private_directory(&path)?;
    path.canonicalize().map_err(Into::into)
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<(), WebTaskError> {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_directory(_path: &Path) -> Result<(), WebTaskError> {
    Err(WebTaskError::Unsafe(
        "capability-bound Web storage is currently audited only on Unix".into(),
    ))
}

#[cfg(test)]
fn create_private_child(path: &Path) -> Result<(), WebTaskError> {
    fs::create_dir(path)?;
    set_private_permissions(path)?;
    verify_private_directory(path)
}

#[cfg(all(test, unix))]
fn set_private_permissions(path: &Path) -> Result<(), WebTaskError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(all(test, not(unix)))]
fn set_private_permissions(_path: &Path) -> Result<(), WebTaskError> {
    Err(WebTaskError::Unsafe(
        "capability-bound Web storage is currently audited only on Unix".into(),
    ))
}

fn verify_private_directory(path: &Path) -> Result<(), WebTaskError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WebTaskError::Unsafe("managed directory identity is invalid".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.permissions().mode() & 0o077 != 0
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(WebTaskError::Unsafe(
                "managed directory is not private or owner-bound".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn create_private_file(path: &Path) -> Result<File, WebTaskError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(Into::into)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(WebTaskError::Unsafe("secure file creation is unavailable".into()))
    }
}

#[cfg(test)]
fn open_regular_nofollow(path: &Path) -> Result<File, WebTaskError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || link_count(&metadata) != 1 {
            return Err(WebTaskError::Unsafe("managed file is not a private regular file".into()));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(WebTaskError::Unsafe("secure file open is unavailable".into()))
    }
}

#[cfg(unix)]
fn link_count(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink()
}

#[cfg(not(unix))]
fn link_count(_metadata: &fs::Metadata) -> u64 {
    0
}

fn sync_directory(path: &Path) -> Result<(), WebTaskError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_web_requests_assemble_capabilities_from_options_and_filename() {
        let mut request = PersistedRequest {
            schema_version: 1,
            workflow: WebWorkflow::Conversion,
            name: "scan.jpg".into(),
            hint: FormatHint::default(),
            batch_id: None,
            options: ConversionOptions::default(),
        };
        let image = web_invocation_capabilities(&request);
        assert!(image.ocr);
        assert!(!image.legacy_office);

        request.name = "archive.DOC".into();
        let legacy = web_invocation_capabilities(&request);
        assert!(legacy.legacy_office);

        request.options.ocr.policy = OcrPolicy::Off;
        request.name = "meeting.webm".into();
        let media = web_invocation_capabilities(&request);
        assert!(!media.ocr);
        assert!(media.transcription);
    }

    #[test]
    fn web_task_request_uses_shared_options_and_requires_one_time_grants() {
        let mut request = WebTaskRequest {
            format: Some(InputFormat::Pdf),
            batch_id: Some("ab".repeat(16)),
            ..WebTaskRequest::default()
        };
        request.options.ocr.policy = OcrPolicy::Always;
        request.options.network.enabled = true;
        request.options.network.allowed_hosts = vec!["api.example.com".into()];
        let bytes = serde_json::to_vec(&request).unwrap();
        assert!(matches!(decode_web_task_request(&bytes), Err(WebTaskError::Invalid(_))));

        request.authorization.network = true;
        let decoded = decode_web_task_request(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!(decoded.format, Some(InputFormat::Pdf));
        assert_eq!(decoded.batch_id.as_deref(), Some("abababababababababababababababab"));
        assert_eq!(decoded.options.ocr.policy, OcrPolicy::Always);
        assert_eq!(decoded.options.network.allowed_hosts, ["api.example.com"]);

        request.options.network.deny_private_networks = false;
        assert!(matches!(
            decode_web_task_request(&serde_json::to_vec(&request).unwrap()),
            Err(WebTaskError::Invalid(_))
        ));
        request.authorization.private_network = true;
        assert!(decode_web_task_request(&serde_json::to_vec(&request).unwrap()).is_ok());

        request.batch_id = Some("AB".repeat(16));
        assert!(matches!(
            decode_web_task_request(&serde_json::to_vec(&request).unwrap()),
            Err(WebTaskError::Invalid(_))
        ));
    }

    #[test]
    fn web_task_request_rejects_ungranted_provider_and_unsafe_limits() {
        let mut request = WebTaskRequest::default();
        request.options.ai.markdown_postprocess = AiMode::Prefer;
        assert!(matches!(
            decode_web_task_request(&serde_json::to_vec(&request).unwrap()),
            Err(WebTaskError::Invalid(_))
        ));
        request.authorization.provider = true;
        assert!(decode_web_task_request(&serde_json::to_vec(&request).unwrap()).is_ok());

        request.options.ai.markdown_postprocess = AiMode::Off;
        assert!(decode_web_task_request(&serde_json::to_vec(&request).unwrap()).is_ok());

        request.options.limits.max_input_bytes = MAX_FILE_BYTES + 1;
        assert!(matches!(
            decode_web_task_request(&serde_json::to_vec(&request).unwrap()),
            Err(WebTaskError::Invalid(_))
        ));
    }

    #[test]
    fn web_task_request_rejects_unknown_or_oversized_envelopes() {
        assert!(matches!(
            decode_web_task_request(br#"{"schemaVersion":1,"unexpected":true}"#),
            Err(WebTaskError::Invalid(_))
        ));
        assert!(matches!(
            decode_web_task_request(&vec![b' '; 16 * 1024 + 1]),
            Err(WebTaskError::Limit(_))
        ));
    }

    #[test]
    fn web_task_request_defaults_omitted_asr_and_rejects_unreviewed_asr_profiles() {
        let mut value = serde_json::to_value(WebTaskRequest::default()).unwrap();
        value["options"].as_object_mut().unwrap().remove("asr");
        let decoded = decode_web_task_request(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.options.asr, AsrOptions::default());

        let mut request = WebTaskRequest::default();
        request.options.asr.max_duration_ms = Some(60_000);
        assert_eq!(
            decode_web_task_request(&serde_json::to_vec(&request).unwrap())
                .unwrap()
                .options
                .asr
                .max_duration_ms,
            Some(60_000)
        );
        request.options.asr.max_duration_ms = None;
        assert!(decode_web_task_request(&serde_json::to_vec(&request).unwrap()).is_ok());
        request.options.asr.max_duration_ms = Some(0);
        assert!(matches!(
            decode_web_task_request(&serde_json::to_vec(&request).unwrap()),
            Err(WebTaskError::Invalid(_))
        ));
        request.options.asr.max_duration_ms = None;
        request.options.asr.max_threads = 9;
        assert!(matches!(
            decode_web_task_request(&serde_json::to_vec(&request).unwrap()),
            Err(WebTaskError::Invalid(_))
        ));
    }

    fn event_record(status: TaskStatus, progress_millionths: u32) -> TaskRecord {
        TaskRecord {
            id: TaskId::parse("11111111111111111111111111111111").unwrap(),
            created_at_ms: 1,
            updated_at_ms: 2,
            status,
            progress_millionths,
            input: InputReference {
                schema_version: 1,
                input_fingerprint: "22".repeat(32),
                options_fingerprint: "33".repeat(32),
                byte_len: 1,
                recovery_token: "44444444444444444444444444444444".into(),
            },
            configuration: ConfigurationSnapshot::default(),
            diagnostics: Vec::new(),
            artifacts: Vec::new(),
            artifact_generation: 0,
            pinned: false,
        }
    }

    fn progress(basis_points: u16) -> ProgressEvent {
        ProgressEvent {
            stage: into_markdown::ExecutionStage::Converting,
            basis_points,
            completed_units: None,
            total_units: None,
            message: None,
        }
    }

    #[test]
    fn event_hub_replays_last_event_id_and_resets_gaps_without_blocking_publishers() {
        let generation = "aa".repeat(16);
        let hub = EventHub::new(generation.clone());
        let record = event_record(TaskStatus::Running, 1);
        let mut initial = hub.subscribe(&record, None);
        let first = initial.replay.pop_front().unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(first.event_id, format!("{generation}:1"));

        hub.publish_progress(&record, progress(5_000));
        let mut replay = hub.subscribe(&record, Some((&generation, 1)));
        let second = replay.replay.pop_front().unwrap();
        assert_eq!(second.sequence, 2);
        assert!(matches!(second.kind, TaskEventKind::Progress));
        assert_eq!(second.progress_millionths, 500_000);

        // A receiver that never drains cannot backpressure this loop. Once it
        // falls outside the replay window, reconnect gets a current snapshot.
        for basis_points in 0..1_000 {
            hub.publish_progress(&record, progress(basis_points));
        }
        drop(initial);
        let mut reset = hub.subscribe(&record, Some((&generation, 1)));
        let reset = reset.replay.pop_front().unwrap();
        assert!(matches!(reset.kind, TaskEventKind::Snapshot));
        assert!(reset.sequence > EVENT_REPLAY_CAPACITY as u64);
    }

    #[test]
    fn event_hub_restart_cursor_yields_durable_terminal_snapshot() {
        let first_generation = "aa".repeat(16);
        let first_hub = EventHub::new(first_generation.clone());
        let pending = event_record(TaskStatus::Pending, 0);
        let first = first_hub.subscribe(&pending, None).replay.pop_front().unwrap();

        let second_hub = EventHub::new("bb".repeat(16));
        let terminal = event_record(TaskStatus::Succeeded, 1_000_000);
        let mut resumed =
            second_hub.subscribe(&terminal, Some((&first_generation, first.sequence)));
        let snapshot = resumed.replay.pop_front().unwrap();
        assert!(matches!(snapshot.kind, TaskEventKind::Snapshot));
        assert!(snapshot.terminal);
        assert_eq!(snapshot.status, TaskStatus::Succeeded);
        assert_ne!(snapshot.event_id, first.event_id);
    }

    #[test]
    fn event_hub_can_restart_the_same_task_after_a_terminal_cancellation() {
        let generation = "aa".repeat(16);
        let hub = EventHub::new(generation.clone());
        let cancelled = event_record(TaskStatus::Cancelled, 450_000);
        hub.publish_snapshot(&cancelled);
        let terminal = hub.subscribe(&cancelled, None).replay.pop_back().unwrap();
        assert!(terminal.terminal);

        let mut pending = event_record(TaskStatus::Pending, 0);
        pending.updated_at_ms = cancelled.updated_at_ms + 1;
        hub.restart(&pending);
        hub.publish_snapshot(&pending);
        let resumed = hub
            .subscribe(&pending, Some((&generation, terminal.sequence)))
            .replay
            .pop_back()
            .unwrap();
        assert_eq!(resumed.status, TaskStatus::Pending);
        assert!(!resumed.terminal);
        assert!(resumed.sequence > terminal.sequence);
    }

    #[cfg(unix)]
    #[test]
    fn browser_disconnect_does_not_cancel_and_last_event_id_replays_final_state() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        *lock(&backend.owner.shared.conversion_gate) = Some(Arc::clone(&gate));
        let mut upload = backend.begin_upload("resume.txt", None).unwrap();
        upload.write_chunk(b"resume after browser close").unwrap();
        let task = upload.finish().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.owner.shared.conversion_entries.load(Ordering::SeqCst) != 1 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }

        let mut browser = backend.events(&task.id, None).unwrap();
        let observed = browser.replay.pop_back().unwrap();
        let (generation, sequence) = observed.event_id.split_once(':').unwrap();
        let generation = generation.to_owned();
        let sequence = sequence.parse::<u64>().unwrap();
        drop(browser); // Closing an SSE connection is observation-only.

        *lock(&backend.owner.shared.conversion_gate) = None;
        gate.wait();
        assert_eq!(wait_terminal(&backend, &task.id).status, TaskStatus::Succeeded);

        let resumed = backend.events(&task.id, Some((&generation, sequence))).unwrap();
        assert!(
            resumed
                .replay
                .iter()
                .any(|event| event.terminal && event.status == TaskStatus::Succeeded),
            "terminal event was not retained for reconnect"
        );
    }

    #[cfg(unix)]
    #[test]
    fn running_cancel_is_idempotent_and_reaches_the_engine_token() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        *lock(&backend.owner.shared.conversion_gate) = Some(Arc::clone(&gate));
        let mut upload = backend.begin_upload("cancel-race.txt", None).unwrap();
        upload.write_chunk(b"cancel race").unwrap();
        let task = upload.finish().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.owner.shared.conversion_entries.load(Ordering::SeqCst) != 1 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }

        assert_eq!(backend.cancel(&task.id).unwrap().status, TaskStatus::Running);
        assert_eq!(backend.cancel(&task.id).unwrap().status, TaskStatus::Running);
        *lock(&backend.owner.shared.conversion_gate) = None;
        gate.wait();
        assert_eq!(wait_terminal(&backend, &task.id).status, TaskStatus::Cancelled);
        assert_eq!(backend.cancel(&task.id).unwrap().status, TaskStatus::Cancelled);
    }

    struct ShortWriter {
        bytes: Vec<u8>,
        max_write: usize,
        fail_after: usize,
    }

    struct DiskFullWriter;

    impl Write for DiskFullWriter {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::StorageFull, "injected disk full"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len() >= self.fail_after {
                return Ok(0);
            }
            let written = bytes.len().min(self.max_write).min(self.fail_after - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn wait_terminal(backend: &WebTaskBackend, id: &TaskId) -> TaskRecord {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let current = backend.get(id).unwrap();
            if matches!(
                current.status,
                TaskStatus::Succeeded
                    | TaskStatus::Failed
                    | TaskStatus::Interrupted
                    | TaskStatus::Cancelled
            ) {
                return current;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn meeting_speaker_relabel_is_metadata_only_generation_cas_and_atomic_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        *lock(&backend.owner.shared.conversion_gate) = Some(Arc::clone(&gate));
        let mut request = WebTaskRequest {
            workflow: WebWorkflow::MeetingTranscript,
            format: Some(InputFormat::Audio),
            ..WebTaskRequest::default()
        };
        request.options.ocr.policy = OcrPolicy::Off;
        request.options.ai.audio_transcription = AiMode::Only;
        let mut upload =
            backend.begin_upload_configured("meeting.wav", Some(4), request.clone()).unwrap();
        upload.write_chunk(b"RIFF").unwrap();
        let task = upload.finish().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.owner.shared.conversion_entries.load(Ordering::SeqCst) != 1 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }

        let range = into_markdown::TimeRange { start_ms: 1_000, end_ms: 2_000 };
        let document = into_markdown::Document {
            blocks: vec![into_markdown::BlockNode {
                id: into_markdown::NodeId("segment-1".into()),
                block: into_markdown::Block::TimedSegment {
                    range,
                    speaker: Some("speaker-1".into()),
                    speaker_confidence: Some(0.9),
                    tokens: Vec::new(),
                    content: vec![into_markdown::Inline::Text {
                        value: "hello".into(),
                        marks: Vec::new(),
                    }],
                },
                provenance: into_markdown::Provenance {
                    kind: into_markdown::ProvenanceKind::AiProvider,
                    provider: "test/model@sha256:abcd".into(),
                    locator: into_markdown::SourceLocator {
                        time: Some(range),
                        ..into_markdown::SourceLocator::default()
                    },
                    confidence: Some(0.8),
                },
            }],
            ..into_markdown::Document::default()
        };
        let original_node = document.blocks[0].clone();
        let original_provenance = original_node.provenance.clone();
        let original_diagnostic = into_markdown::Diagnostic {
            code: "speakerAssignmentAmbiguous".into(),
            severity: into_markdown::DiagnosticSeverity::Warning,
            message: "speaker could not be assigned reliably".into(),
            locator: Some(into_markdown::SourceLocator {
                time: Some(range),
                ..into_markdown::SourceLocator::default()
            }),
        };
        let markdown = into_markdown::render_markdown(&document, &[], &request.options).unwrap();
        let result = into_markdown::ConversionResult::new(
            document,
            markdown,
            Vec::new(),
            vec![original_diagnostic.clone()],
            vec![original_provenance.clone()],
        );
        let converted = lock(&backend.owner.shared.task_store)
            .transition(
                &task.id,
                TaskTransition {
                    expected: TaskStatus::Running,
                    next: TaskStatus::Converted,
                    progress_millionths: 900_000,
                    diagnostics: Vec::new(),
                    artifacts: Vec::new(),
                },
            )
            .unwrap();
        backend.owner.shared.events.publish_snapshot(&converted);
        let artifacts =
            publish_result(&backend.owner.shared, &task.id, &result, &CancellationToken::new())
                .unwrap();
        promote_published_success(&backend.owner.shared, &task.id, &artifacts).unwrap();

        let labels = backend.speaker_labels(&task.id).unwrap();
        assert_eq!(labels.artifact_generation, 0);
        assert_eq!(labels.speakers[0].name, "Speaker 1");
        let retry_orphan = root.join("objects").join(task.id.as_str()).join("published-1");
        create_private_child(&retry_orphan).unwrap();
        write_private(&retry_orphan.join("orphan"), b"stale failed relabel").unwrap();
        let assignments = BTreeMap::from([("speaker-1".to_owned(), "张三".to_owned())]);
        let relabeled = backend.relabel_speakers(&task.id, 0, &assignments).unwrap();
        assert!(!retry_orphan.join("orphan").exists());
        assert_eq!(relabeled.status, TaskStatus::Succeeded);
        assert_eq!(relabeled.artifact_generation, 1);
        assert!(matches!(
            backend.relabel_speakers(&task.id, 0, &assignments),
            Err(WebTaskError::Conflict(_))
        ));
        let labels = backend.speaker_labels(&task.id).unwrap();
        assert_eq!(labels.speakers[0].name, "张三");
        let markdown = relabeled
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Markdown)
            .unwrap();
        let (mut snapshot, _) = backend.artifact(&task.id, &markdown.storage_key).unwrap();
        let mut rendered = String::new();
        snapshot.read_to_string(&mut rendered).unwrap();
        assert!(rendered.contains("张三"));
        let ir = relabeled
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::DocumentIr)
            .unwrap();
        let (snapshot, _) = backend.artifact(&task.id, &ir.storage_key).unwrap();
        let rerendered: into_markdown::Document = serde_json::from_reader(snapshot).unwrap();
        assert_eq!(rerendered.blocks[0], original_node);
        assert_eq!(rerendered.metadata.properties["media.speaker.speaker-1.label"], "张三");
        let diagnostics = relabeled
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Diagnostics)
            .unwrap();
        let (snapshot, _) = backend.artifact(&task.id, &diagnostics.storage_key).unwrap();
        let diagnostics: OwnedDiagnosticsArtifact = serde_json::from_reader(snapshot).unwrap();
        assert_eq!(diagnostics.schema_version, 1);
        assert_eq!(diagnostics.diagnostics, vec![original_diagnostic]);
        let bundle = relabeled
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Bundle)
            .unwrap();
        let (snapshot, _) = backend.artifact(&task.id, &bundle.storage_key).unwrap();
        let mut archive = zip::ZipArchive::new(snapshot).unwrap();
        let mut provenance = String::new();
        archive.by_name("provenance.json").unwrap().read_to_string(&mut provenance).unwrap();
        let provenance = into_markdown::ProvenanceListDto::from_bundle_json(
            &provenance,
            into_markdown::DTO_SCHEMA_VERSION,
        )
        .unwrap();
        assert_eq!(provenance.provenance.len(), 1);
        assert_eq!(provenance.provenance[0].provider, original_provenance.provider);

        let orphan = root.join("objects").join(task.id.as_str()).join("published-2");
        create_private_child(&orphan).unwrap();
        write_private(&orphan.join("orphan"), b"uncommitted generation").unwrap();

        *lock(&backend.owner.shared.conversion_gate) = None;
        gate.wait();
        drop(backend);
        let reopened = WebTaskBackend::open(&root).unwrap();
        assert!(!orphan.exists());
        assert_eq!(reopened.get(&task.id).unwrap().artifact_generation, 1);
    }

    #[cfg(unix)]
    #[test]
    fn history_filters_retry_pin_delete_and_retention_are_restart_safe() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let mut first = backend.begin_upload("first.txt", None).unwrap();
        first.write_chunk(b"first").unwrap();
        let first = wait_terminal(&backend, &first.finish().unwrap().id);
        let mut second = backend.begin_upload("second.txt", None).unwrap();
        second.write_chunk(b"second").unwrap();
        let second = wait_terminal(&backend, &second.finish().unwrap().id);

        let pinned = backend.set_pinned(&first.id, true).unwrap();
        assert!(pinned.pinned);
        let page = backend.list(1, None, Some(TaskStatus::Succeeded), None).unwrap();
        assert_eq!(page.tasks.len(), 1);
        assert!(page.next.is_some());
        let pinned_page = backend.list(25, None, None, Some(true)).unwrap();
        assert_eq!(pinned_page.tasks.iter().map(|task| &task.id).collect::<Vec<_>>(), [&first.id]);

        let retried = backend.retry(&second.id).unwrap();
        assert_eq!(wait_terminal(&backend, &retried.id).status, TaskStatus::Succeeded);
        let retry_token = RecoveryToken::parse(retried.input.recovery_token.clone()).unwrap();
        assert!(matches!(backend.delete(&retried.id), Ok(())));
        assert!(matches!(backend.get(&retried.id), Err(WebTaskError::NotFound)));
        assert!(backend.owner.shared.recovery.inspect(&retry_token).unwrap().is_none());

        let summary = backend
            .cleanup(
                RetentionPolicy { max_age: Duration::ZERO, max_bytes: DEFAULT_RETENTION_BYTES },
                unix_now_ms().unwrap(),
            )
            .unwrap();
        assert!(summary.deleted_tasks >= 1);
        assert!(backend.get(&first.id).unwrap().pinned);

        // Simulate interruption after quarantine but before the DB commit.
        let name = std::ffi::OsStr::new(first.id.as_str());
        let token = RecoveryToken::parse(first.input.recovery_token.clone()).unwrap();
        let trash_name = retention_trash_name(&first.id, &token);
        backend
            .owner
            .shared
            .objects
            .rename_child_private_to_no_replace(
                name,
                &backend.owner.shared.trash,
                std::ffi::OsStr::new(&trash_name),
            )
            .unwrap();
        drop(backend);
        let reopened = WebTaskBackend::open(&root).unwrap();
        assert!(reopened.get(&first.id).unwrap().pinned);
        assert!(reopened.owner.shared.trash.names_private().unwrap().is_empty());
        reopened.delete(&first.id).unwrap();
        assert!(matches!(reopened.get(&first.id), Err(WebTaskError::NotFound)));
    }

    #[test]
    fn history_scan_keeps_one_store_snapshot_across_the_hundred_row_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let mut ids = Vec::new();
        {
            let mut store = lock(&backend.owner.shared.task_store);
            for _ in 0..101 {
                let token = backend.owner.shared.recovery.create_token().unwrap();
                let record = store
                    .create(NewTask {
                        input: InputReference {
                            schema_version: 1,
                            input_fingerprint: "a".repeat(64),
                            options_fingerprint: "b".repeat(64),
                            byte_len: 1,
                            recovery_token: token.as_str().to_owned(),
                        },
                        configuration: ConfigurationSnapshot::default(),
                    })
                    .unwrap();
                ids.push(record.id);
            }
        }
        let gate = Arc::new(std::sync::Barrier::new(2));
        *lock(&backend.owner.shared.history_scan_gate) = Some(Arc::clone(&gate));
        let listing = backend.clone();
        let list_thread = std::thread::spawn(move || listing.list(100, None, None, None));
        gate.wait();
        let pinning = backend.clone();
        let oldest = ids.first().unwrap().clone();
        let pin_thread = std::thread::spawn(move || pinning.set_pinned(&oldest, true));
        gate.wait();
        let page = list_thread.join().unwrap().unwrap();
        assert_eq!(page.tasks.len(), 100);
        assert!(page.next.is_some());
        assert!(pin_thread.join().unwrap().unwrap().pinned);
        assert!(matches!(
            backend.set_pinned(&TaskId::parse("f".repeat(32)).unwrap(), true),
            Err(WebTaskError::NotFound)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn retention_age_capacity_pinning_and_ceiling_boundaries_are_exact() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("age")).unwrap();
        let mut upload = backend.begin_upload("boundary.txt", None).unwrap();
        upload.write_chunk(b"boundary").unwrap();
        let terminal = wait_terminal(&backend, &upload.finish().unwrap().id);
        let completed =
            lock(&backend.owner.shared.task_store).completed_at_ms(&terminal.id).unwrap().unwrap();
        let retained_bytes = measured_managed_bytes(&backend.owner.shared.root_handle).unwrap();
        let age = Duration::from_secs(30 * 24 * 60 * 60);
        let age_ms = i64::try_from(age.as_millis()).unwrap();
        assert_eq!(
            backend
                .cleanup(
                    RetentionPolicy { max_age: age, max_bytes: retained_bytes },
                    completed + age_ms - 1,
                )
                .unwrap()
                .deleted_tasks,
            0
        );
        assert_eq!(
            backend
                .cleanup(
                    RetentionPolicy { max_age: age, max_bytes: retained_bytes },
                    completed + age_ms,
                )
                .unwrap()
                .deleted_tasks,
            1
        );

        let backend = WebTaskBackend::open(temporary.path().join("capacity")).unwrap();
        let mut upload = backend.begin_upload("single.txt", None).unwrap();
        upload.write_chunk(b"single task").unwrap();
        let terminal = wait_terminal(&backend, &upload.finish().unwrap().id);
        let completed =
            lock(&backend.owner.shared.task_store).completed_at_ms(&terminal.id).unwrap().unwrap();
        let exact = measured_managed_bytes(&backend.owner.shared.root_handle).unwrap();
        let young = Duration::from_secs(1);
        assert_eq!(
            backend
                .cleanup(RetentionPolicy { max_age: young, max_bytes: exact }, completed)
                .unwrap()
                .deleted_tasks,
            0
        );
        backend.set_pinned(&terminal.id, true).unwrap();
        let pinned_used = measured_managed_bytes(&backend.owner.shared.root_handle).unwrap();
        assert_eq!(
            backend
                .cleanup(RetentionPolicy { max_age: young, max_bytes: pinned_used - 1 }, completed,)
                .unwrap()
                .deleted_tasks,
            0
        );
        backend.set_pinned(&terminal.id, false).unwrap();
        let unpinned_used = measured_managed_bytes(&backend.owner.shared.root_handle).unwrap();
        assert_eq!(
            backend
                .cleanup(
                    RetentionPolicy { max_age: young, max_bytes: unpinned_used - 1 },
                    completed,
                )
                .unwrap()
                .deleted_tasks,
            1
        );
        assert!(matches!(
            backend.cleanup(
                RetentionPolicy { max_age: Duration::ZERO, max_bytes: MAX_DATA_BYTES + 1 },
                completed,
            ),
            Err(WebTaskError::Invalid(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn retention_failures_before_and_after_commit_recover_durably() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let mut upload = backend.begin_upload("durable.txt", None).unwrap();
        upload.write_chunk(b"durable").unwrap();
        let terminal = wait_terminal(&backend, &upload.finish().unwrap().id);
        let token = RecoveryToken::parse(terminal.input.recovery_token.clone()).unwrap();
        assert!(backend.owner.shared.recovery.inspect(&token).unwrap().is_some());

        backend.owner.shared.retention_failure.store(1, Ordering::SeqCst);
        assert!(matches!(backend.delete(&terminal.id), Err(WebTaskError::Io(_))));
        assert!(backend.get(&terminal.id).is_ok());
        assert!(backend.owner.shared.trash.names_private().unwrap().is_empty());
        assert!(backend.owner.shared.recovery.inspect(&token).unwrap().is_some());

        backend.owner.shared.retention_failure.store(2, Ordering::SeqCst);
        assert!(matches!(backend.delete(&terminal.id), Err(WebTaskError::Io(_))));
        assert!(matches!(backend.get(&terminal.id), Err(WebTaskError::NotFound)));
        assert_eq!(backend.owner.shared.trash.names_private().unwrap().len(), 1);
        assert!(backend.owner.shared.recovery.inspect(&token).unwrap().is_none());
        drop(backend);

        let reopened = WebTaskBackend::open(&root).unwrap();
        assert!(reopened.owner.shared.trash.names_private().unwrap().is_empty());
        assert!(reopened.owner.shared.recovery.inspect(&token).unwrap().is_none());
        assert!(matches!(reopened.get(&terminal.id), Err(WebTaskError::NotFound)));
    }

    #[cfg(unix)]
    #[test]
    fn retention_recovery_rejects_active_rows_and_unsafe_quarantine_trees() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        for case in ["active", "regular-root", "symlink", "public-child", "hardlink"] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("backend");
            let backend = WebTaskBackend::open(&root).unwrap();
            let active_gate = (case == "active").then(|| Arc::new(std::sync::Barrier::new(2)));
            if let Some(gate) = &active_gate {
                *lock(&backend.owner.shared.conversion_gate) = Some(Arc::clone(gate));
            }
            let mut upload = backend.begin_upload("quarantine.txt", None).unwrap();
            upload.write_chunk(b"quarantine").unwrap();
            let task = upload.finish().unwrap();
            if case == "active" {
                let deadline = Instant::now() + Duration::from_secs(10);
                while backend.owner.shared.conversion_entries.load(Ordering::SeqCst) == 0 {
                    assert!(Instant::now() < deadline);
                    std::thread::yield_now();
                }
            }
            let record = if case == "active" { task } else { wait_terminal(&backend, &task.id) };
            let token = RecoveryToken::parse(record.input.recovery_token.clone()).unwrap();
            let trash_name = retention_trash_name(&record.id, &token);
            backend
                .owner
                .shared
                .objects
                .rename_child_private_to_no_replace(
                    std::ffi::OsStr::new(record.id.as_str()),
                    &backend.owner.shared.trash,
                    std::ffi::OsStr::new(&trash_name),
                )
                .unwrap();
            let quarantined = root.join("trash").join(&trash_name);
            let outside = temporary.path().join("outside");
            fs::write(&outside, b"outside remains").unwrap();
            match case {
                "active" => {}
                "regular-root" => {
                    fs::remove_dir_all(&quarantined).unwrap();
                    fs::write(&quarantined, b"not a directory").unwrap();
                }
                "symlink" => symlink(&outside, quarantined.join("attacker")).unwrap(),
                "public-child" => {
                    let child = quarantined.join("public");
                    fs::create_dir(&child).unwrap();
                    fs::set_permissions(&child, fs::Permissions::from_mode(0o777)).unwrap();
                }
                "hardlink" => fs::hard_link(&outside, quarantined.join("attacker")).unwrap(),
                _ => unreachable!(),
            }
            let result = {
                let store = lock(&backend.owner.shared.task_store);
                recover_retention_trash(
                    &backend.owner.shared.objects,
                    &backend.owner.shared.trash,
                    &store,
                    &backend.owner.shared.recovery,
                )
            };
            assert!(matches!(result, Err(WebTaskError::Unsafe(_))), "case {case}: {result:?}");
            assert_eq!(fs::read(&outside).unwrap(), b"outside remains");
            assert!(lock(&backend.owner.shared.task_store).get(&record.id).unwrap().is_some());
            if let Some(gate) = active_gate {
                gate.wait();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn delete_checkpoint_replacement_before_commit_never_deletes_the_database_row() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let mut upload = backend.begin_upload("race.txt", None).unwrap();
        upload.write_chunk(b"race").unwrap();
        let terminal = wait_terminal(&backend, &upload.finish().unwrap().id);
        let gate = Arc::new(std::sync::Barrier::new(2));
        *lock(&backend.owner.shared.retention_quarantine_gate) = Some(Arc::clone(&gate));
        let deleting = backend.clone();
        let id = terminal.id.clone();
        let thread = std::thread::spawn(move || deleting.delete(&id));
        gate.wait();
        let outside = temporary.path().join("outside-checkpoint");
        fs::write(&outside, b"outside remains").unwrap();
        let token = terminal.input.recovery_token;
        fs::hard_link(
            &outside,
            root.join("recovery").join(format!("{token}.succeeded.checkpoint")),
        )
        .unwrap();
        gate.wait();
        assert!(matches!(thread.join().unwrap(), Err(WebTaskError::Unsafe(_))));
        assert!(lock(&backend.owner.shared.task_store).get(&terminal.id).unwrap().is_some());
        assert_eq!(fs::read(&outside).unwrap(), b"outside remains");
    }

    #[cfg(unix)]
    #[test]
    fn retention_never_deletes_a_concurrently_completing_task() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        *lock(&backend.owner.shared.conversion_gate) = Some(Arc::clone(&gate));
        let mut upload = backend.begin_upload("active.txt", None).unwrap();
        upload.write_chunk(b"active").unwrap();
        let task = upload.finish().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.owner.shared.conversion_entries.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let summary = backend
            .cleanup(
                RetentionPolicy { max_age: Duration::ZERO, max_bytes: 0 },
                unix_now_ms().unwrap(),
            )
            .unwrap();
        assert_eq!(summary.deleted_tasks, 0);
        assert!(!is_terminal(backend.get(&task.id).unwrap().status));
        gate.wait();
        assert_eq!(wait_terminal(&backend, &task.id).status, TaskStatus::Succeeded);
        let summary = backend
            .cleanup(
                RetentionPolicy { max_age: Duration::ZERO, max_bytes: 0 },
                unix_now_ms().unwrap(),
            )
            .unwrap();
        assert_eq!(summary.deleted_tasks, 1);
    }

    #[cfg(unix)]
    #[test]
    fn history_delete_refuses_links_without_touching_external_paths_or_database() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let mut upload = backend.begin_upload("safe.txt", None).unwrap();
        upload.write_chunk(b"safe").unwrap();
        let terminal = wait_terminal(&backend, &upload.finish().unwrap().id);
        let outside = temporary.path().join("outside");
        fs::write(&outside, b"do not delete").unwrap();
        symlink(&outside, root.join("objects").join(terminal.id.as_str()).join("attacker"))
            .unwrap();
        assert!(matches!(backend.delete(&terminal.id), Err(WebTaskError::Unsafe(_))));
        assert_eq!(fs::read(&outside).unwrap(), b"do not delete");
        assert!(lock(&backend.owner.shared.task_store).get(&terminal.id).unwrap().is_some());
    }

    fn create_pending_without_workers(root: &Path, bytes: &[u8]) -> TaskId {
        let backend = WebTaskBackend::open(root).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        let mut upload = backend.begin_upload("crash.txt", None).unwrap();
        upload.write_chunk(bytes).unwrap();
        upload.finish().unwrap().id
    }

    #[test]
    fn web_crash_helper() {
        let Some(root) = std::env::var_os("INTO_MD_WEB_CRASH_ROOT") else {
            return;
        };
        let backend = WebTaskBackend::open(PathBuf::from(root)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(backend);
        panic!("crash hook was not reached");
    }

    #[test]
    fn filename_and_opaque_key_validation_reject_paths() {
        assert!(validate_display_name("report.md").is_ok());
        assert!(validate_display_name("../report.md").is_err());
        assert!(validate_display_name("a/b").is_err());
        assert!(validate_key("00112233445566778899aabbccddeeff").is_ok());
        assert!(validate_key("../00112233445566778899aabbccdd").is_err());
    }

    #[test]
    fn streamed_quota_is_checked_before_each_write_and_drop_refunds() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let baseline = lock(&backend.owner.shared.disk_bytes).used;
        let mut upload = backend.begin_upload("a.txt", None).unwrap();
        upload.write_chunk(b"abc").unwrap();
        assert_eq!(lock(&backend.owner.shared.disk_bytes).used, baseline + 3);
        drop(upload);
        assert_eq!(lock(&backend.owner.shared.disk_bytes).used, baseline);
    }

    #[test]
    fn short_writes_are_retried_and_zero_progress_is_a_hard_failure() {
        let mut short = ShortWriter { bytes: Vec::new(), max_write: 2, fail_after: usize::MAX };
        write_all_checked(&mut short, b"abcdef").unwrap();
        assert_eq!(short.bytes, b"abcdef");
        let mut full = ShortWriter { bytes: Vec::new(), max_write: 2, fail_after: 3 };
        assert!(matches!(write_all_checked(&mut full, b"abcdef"), Err(WebTaskError::Io(_))));
        assert_eq!(full.bytes, b"abc");
        assert!(matches!(
            write_all_checked(&mut DiskFullWriter, b"x"),
            Err(WebTaskError::Io(detail)) if detail.contains("injected disk full")
        ));
    }

    #[test]
    fn staged_writer_rejects_quota_before_touching_the_file() {
        let temporary = tempfile::tempfile().unwrap();
        let mut file = temporary;
        let cancellation = CancellationToken::new();
        let mut staged = MAX_TASK_DURABLE_GROWTH - 1;
        let write_failure_after = AtomicUsize::new(usize::MAX);
        {
            let mut writer = CancellableFile {
                file: &mut file,
                cancellation: &cancellation,
                staged_bytes: &mut staged,
                write_failure_after: &write_failure_after,
            };
            assert!(writer.write_all(b"xx").is_err());
        }
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert_eq!(staged, MAX_TASK_DURABLE_GROWTH - 1);
    }

    #[test]
    fn partial_enospc_in_production_publication_fails_only_that_task_and_cleans_stage() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        backend.owner.shared.write_failure_after.store(3, Ordering::SeqCst);
        let mut upload = backend.begin_upload("full.txt", None).unwrap();
        upload.write_chunk(b"publication is larger than three bytes").unwrap();
        let failed = upload.finish().unwrap();
        assert_eq!(wait_terminal(&backend, &failed.id).status, TaskStatus::Failed);
        let task = root.join("objects").join(failed.id.as_str());
        assert!(!task.join("published").exists());
        assert!(
            fs::read_dir(&task).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("stage-"))
        );

        backend.owner.shared.write_failure_after.store(usize::MAX, Ordering::SeqCst);
        let mut upload = backend.begin_upload("next.txt", None).unwrap();
        upload.write_chunk(b"next task still runs").unwrap();
        let next = upload.finish().unwrap();
        assert_eq!(wait_terminal(&backend, &next.id).status, TaskStatus::Succeeded);
    }

    #[test]
    fn production_publication_create_sync_dirsync_and_rename_failures_are_isolated() {
        for phase in 1..=4 {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("backend");
            let backend = WebTaskBackend::open(&root).unwrap();
            backend.owner.shared.publication_failure.store(phase, Ordering::SeqCst);
            let mut upload = backend.begin_upload("failure.txt", None).unwrap();
            upload.write_chunk(b"publication phase failure").unwrap();
            let failed = upload.finish().unwrap();
            assert_eq!(wait_terminal(&backend, &failed.id).status, TaskStatus::Failed);
            let task = root.join("objects").join(failed.id.as_str());
            assert!(!task.join("published").exists());
            assert!(
                fs::read_dir(&task).unwrap().all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("stage-"))
            );
            let mut next = backend.begin_upload("next.txt", None).unwrap();
            next.write_chunk(b"next task").unwrap();
            let next = next.finish().unwrap();
            assert_eq!(wait_terminal(&backend, &next.id).status, TaskStatus::Succeeded);
        }
    }

    #[test]
    fn waiting_disk_lease_recomputes_used_bytes_after_every_wakeup() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = 0;
            disk.reserved = MAX_DATA_BYTES;
        }
        let shared = Arc::clone(&backend.owner.shared);
        let waiter = std::thread::spawn(move || DiskLease::acquire(&shared, 1).map(drop));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while backend.owner.shared.disk_waiters.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = MAX_GLOBAL_BYTES;
            disk.reserved = 0;
            backend.owner.shared.disk_changed.notify_all();
        }
        assert!(matches!(waiter.join().unwrap(), Err(WebTaskError::Limit(_))));
    }

    #[test]
    fn aborted_upload_wakes_the_only_disk_lease_waiter() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let mut upload = backend.begin_upload("wake.txt", None).unwrap();
        upload.write_chunk(b"x").unwrap();
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = MAX_DATA_BYTES - 1;
            disk.reserved = 1;
        }
        let shared = Arc::clone(&backend.owner.shared);
        let waiter = std::thread::spawn(move || DiskLease::acquire(&shared, 1).map(drop));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while backend.owner.shared.disk_waiters.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(upload);
        assert!(waiter.join().unwrap().is_ok());
        lock(&backend.owner.shared.disk_bytes).reserved = 0;
    }

    #[test]
    fn two_checkpoints_and_publication_reserve_three_copies_without_double_spend() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = MAX_DATA_BYTES - MAX_TASK_TOTAL_DURABLE_GROWTH;
            disk.reserved = 0;
        }
        let first =
            DiskLease::acquire(&backend.owner.shared, MAX_TASK_TOTAL_DURABLE_GROWTH).unwrap();
        let shared = Arc::clone(&backend.owner.shared);
        let waiter = std::thread::spawn(move || {
            DiskLease::acquire(&shared, MAX_TASK_TOTAL_DURABLE_GROWTH).map(drop)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while backend.owner.shared.disk_waiters.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        {
            let disk = lock(&backend.owner.shared.disk_bytes);
            assert_eq!(disk.used + disk.reserved, MAX_DATA_BYTES);
        }
        drop(first);
        assert!(waiter.join().unwrap().is_ok());
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = MAX_DATA_BYTES - MAX_TASK_TOTAL_DURABLE_GROWTH + 1;
            disk.reserved = 0;
        }
        assert!(matches!(
            DiskLease::acquire(&backend.owner.shared, MAX_TASK_TOTAL_DURABLE_GROWTH),
            Err(WebTaskError::Limit(_))
        ));
    }

    #[test]
    fn disk_wait_observes_cancel_and_deadline_without_waiting_for_holder() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = 0;
            disk.reserved = MAX_GLOBAL_BYTES;
        }
        let cancellation = CancellationToken::new();
        let waiting_token = cancellation.clone();
        let shared = Arc::clone(&backend.owner.shared);
        let waiter = std::thread::spawn(move || {
            DiskLease::acquire_interruptible(
                &shared,
                1,
                &waiting_token,
                Instant::now() + Duration::from_secs(5),
                None,
            )
            .map(drop)
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while backend.owner.shared.disk_waiters.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        cancellation.cancel();
        backend.owner.shared.disk_changed.notify_all();
        assert!(matches!(waiter.join().unwrap(), Err(WebTaskError::Cancelled)));
        let timeout = DiskLease::acquire_interruptible(
            &backend.owner.shared,
            1,
            &CancellationToken::new(),
            Instant::now() + Duration::from_millis(10),
            None,
        );
        assert!(matches!(timeout, Err(WebTaskError::Limit(_))));
    }

    #[test]
    fn disk_wait_shutdown_is_bounded_and_cancelled_waiter_does_not_block_successor() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = 0;
            disk.reserved = MAX_GLOBAL_BYTES;
        }

        let cancelled = CancellationToken::new();
        let cancelled_waiter = cancelled.clone();
        let first_shared = Arc::clone(&backend.owner.shared);
        let first = std::thread::spawn(move || {
            DiskLease::acquire_interruptible(
                &first_shared,
                1,
                &cancelled_waiter,
                Instant::now() + Duration::from_secs(5),
                None,
            )
            .map(drop)
        });
        let successor_shared = Arc::clone(&backend.owner.shared);
        let successor = std::thread::spawn(move || {
            DiskLease::acquire_interruptible(
                &successor_shared,
                1,
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(5),
                None,
            )
            .map(drop)
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while backend.owner.shared.disk_waiters.load(Ordering::SeqCst) != 2 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        cancelled.cancel();
        backend.owner.shared.disk_changed.notify_all();
        assert!(matches!(first.join().unwrap(), Err(WebTaskError::Cancelled)));
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.reserved = 0;
        }
        backend.owner.shared.disk_changed.notify_all();
        assert!(successor.join().unwrap().is_ok());

        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.reserved = MAX_GLOBAL_BYTES;
        }
        let shutdown_shared = Arc::clone(&backend.owner.shared);
        let shutdown_waiter = std::thread::spawn(move || {
            DiskLease::acquire_interruptible(
                &shutdown_shared,
                1,
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(30),
                None,
            )
            .map(drop)
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while backend.owner.shared.disk_waiters.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let started = Instant::now();
        drop(backend);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(shutdown_waiter.join().unwrap(), Err(WebTaskError::Cancelled)));
    }

    #[test]
    fn cancel_api_is_bounded_while_quota_is_full_and_waiting_task_converges_cancelled() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        let mut upload = backend.begin_upload("cancel-wait.txt", None).unwrap();
        upload.write_chunk(b"cancel while waiting").unwrap();
        let task = upload.finish().unwrap();
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = 0;
            disk.reserved = MAX_DATA_BYTES;
        }
        let shared = Arc::clone(&backend.owner.shared);
        lock(&backend.owner.workers).push(std::thread::spawn(move || worker(shared)));
        backend.owner.shared.queue_changed.notify_all();
        let deadline = Instant::now() + Duration::from_secs(1);
        while backend.owner.shared.disk_waiters.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let started = Instant::now();
        let result = backend.cancel(&task.id).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(result.status, TaskStatus::Cancelled);
        assert_eq!(wait_terminal(&backend, &task.id).status, TaskStatus::Cancelled);
        lock(&backend.owner.shared.disk_bytes).reserved = 0;
    }

    #[test]
    fn permanent_store_headroom_allows_terminal_mutation_at_real_data_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        let mut upload = backend.begin_upload("boundary.txt", None).unwrap();
        upload.write_chunk(b"terminal metadata boundary").unwrap();
        let task = upload.finish().unwrap();
        let before = measured_managed_bytes(&backend.owner.shared.root_handle).unwrap();
        let filler = backend
            .owner
            .shared
            .root_handle
            .create_regular_private(std::ffi::OsStr::new("quota-boundary"))
            .unwrap();
        filler.set_len(MAX_DATA_BYTES - before).unwrap();
        filler.sync_all().unwrap();
        reconcile_managed_usage(&backend.owner.shared).unwrap();
        assert_eq!(lock(&backend.owner.shared.disk_bytes).used, MAX_DATA_BYTES);
        let cancelled = backend.cancel(&task.id).unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        assert!(
            measured_managed_bytes(&backend.owner.shared.root_handle).unwrap() <= MAX_GLOBAL_BYTES
        );
    }

    #[test]
    fn finish_metadata_wait_is_bounded_cleans_incoming_and_allows_the_next_upload() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let baseline = lock(&backend.owner.shared.disk_bytes).used;
        let mut upload = backend.begin_upload("blocked-finish.txt", None).unwrap();
        upload.write_chunk(b"blocked finish").unwrap();
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.reserved = MAX_GLOBAL_BYTES - disk.used;
        }
        let started = Instant::now();
        assert!(matches!(upload.finish(), Err(WebTaskError::Limit(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(backend.owner.shared.incoming.names_private().unwrap().is_empty());
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.reserved = 0;
            disk.used = measured_managed_bytes(&backend.owner.shared.root_handle).unwrap();
            assert!(disk.used <= baseline + MAX_TASK_METADATA_GROWTH);
        }
        backend.owner.shared.disk_changed.notify_all();
        let mut next = backend.begin_upload("next.txt", None).unwrap();
        next.write_chunk(b"next").unwrap();
        let task = next.finish().unwrap();
        assert_eq!(wait_terminal(&backend, &task.id).status, TaskStatus::Succeeded);
    }

    #[test]
    fn four_small_task_plans_fit_and_history_above_two_gib_does_not_disable_service() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = 3 * 1024 * 1024 * 1024;
            disk.reserved = 0;
        }
        let lease =
            DiskLease::acquire(&backend.owner.shared, MAX_TASK_TOTAL_DURABLE_GROWTH).unwrap();
        drop(lease);
        {
            let mut disk = lock(&backend.owner.shared.disk_bytes);
            disk.used = 0;
        }
        let leases = (0..4)
            .map(|_| {
                DiskLease::acquire(&backend.owner.shared, MAX_TASK_TOTAL_DURABLE_GROWTH).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(leases.len(), 4);
    }

    #[test]
    fn four_workers_really_cross_quota_acquisition_into_conversion_together() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(MAX_WORKERS));
        *lock(&backend.owner.shared.conversion_gate) = Some(Arc::clone(&gate));
        let mut tasks = Vec::new();
        for index in 0..MAX_WORKERS {
            let mut upload = backend.begin_upload(&format!("parallel-{index}.txt"), None).unwrap();
            upload.write_chunk(format!("parallel item {index}").as_bytes()).unwrap();
            tasks.push(upload.finish().unwrap().id);
        }
        // Every worker reaches this barrier only after acquiring its complete
        // durable plan and transitioning Pending -> Running.
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.owner.shared.conversion_entries.load(Ordering::SeqCst) != MAX_WORKERS {
            assert!(
                Instant::now() < deadline,
                "only {} workers entered conversion; quota={:?}, waiters={}",
                backend.owner.shared.conversion_entries.load(Ordering::SeqCst),
                {
                    let disk = lock(&backend.owner.shared.disk_bytes);
                    (disk.used, disk.reserved)
                },
                backend.owner.shared.disk_waiters.load(Ordering::SeqCst)
            );
            std::thread::yield_now();
        }
        *lock(&backend.owner.shared.conversion_gate) = None;
        for task in tasks {
            assert_eq!(wait_terminal(&backend, &task).status, TaskStatus::Succeeded);
        }
    }

    #[test]
    fn each_completed_lease_charges_durable_growth_during_a_continuous_pipeline() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let keeper = DiskLease::acquire(&backend.owner.shared, 1).unwrap();
        let mut measured_floor = lock(&backend.owner.shared.disk_bytes).used;
        for index in 0..8 {
            let lease = DiskLease::acquire(&backend.owner.shared, 64 * 1024 * 1024).unwrap();
            let name = format!("pipeline-{index:02}");
            let file = backend
                .owner
                .shared
                .root_handle
                .create_regular_private(std::ffi::OsStr::new(&name))
                .unwrap();
            file.set_len(64 * 1024 * 1024).unwrap();
            file.sync_all().unwrap();
            drop(lease);
            let disk = lock(&backend.owner.shared.disk_bytes);
            assert!(disk.used >= measured_floor + 64 * 1024 * 1024);
            assert!(disk.used.checked_add(disk.reserved).unwrap() <= MAX_GLOBAL_BYTES);
            measured_floor = disk.used;
        }
        drop(keeper);
    }

    #[test]
    fn upload_queue_publishes_complete_artifact_set() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let request = WebTaskRequest {
            format: Some(InputFormat::Text),
            batch_id: Some("12".repeat(16)),
            ..WebTaskRequest::default()
        };
        let mut upload =
            backend.begin_upload_configured("Quarterly report.txt", Some(5), request).unwrap();
        upload.write_chunk(b"hello").unwrap();
        let record = upload.finish().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let current = backend.get(&record.id).unwrap();
            if current.status == TaskStatus::Succeeded {
                let browser = backend.web_record(current.clone()).unwrap();
                assert_eq!(browser.display_name.as_deref(), Some("Quarterly report.txt"));
                assert_eq!(browser.format, Some(InputFormat::Text));
                assert_eq!(browser.batch_id.as_deref(), Some("12121212121212121212121212121212"));
                let batch = backend
                    .list_batch(10, None, None, None, "12121212121212121212121212121212")
                    .unwrap();
                assert_eq!(batch.tasks.iter().filter(|item| item.id == record.id).count(), 1);
                assert!(current.artifacts.iter().any(|value| value.kind == ArtifactKind::Markdown));
                assert!(
                    current.artifacts.iter().any(|value| value.kind == ArtifactKind::DocumentIr)
                );
                assert!(
                    current.artifacts.iter().any(|value| value.kind == ArtifactKind::Diagnostics)
                );
                assert!(current.artifacts.iter().any(|value| value.kind == ArtifactKind::Bundle));
                for artifact in current.artifacts {
                    let (file, reference) =
                        backend.artifact(&record.id, &artifact.storage_key).unwrap();
                    assert_eq!(file.metadata().unwrap().len(), reference.byte_len);
                }
                break;
            }
            assert!(!matches!(current.status, TaskStatus::Failed | TaskStatus::Interrupted));
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn published_success_transition_retries_or_stops_backend_fail_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("transient")).unwrap();
        backend.owner.shared.success_transition_failures.store(2, Ordering::SeqCst);
        let mut upload = backend.begin_upload("transient.txt", None).unwrap();
        upload.write_chunk(b"transient publication transition").unwrap();
        let transient = upload.finish().unwrap();
        assert_eq!(wait_terminal(&backend, &transient.id).status, TaskStatus::Succeeded);
        assert!(!lock(&backend.owner.shared.queue).stopped);

        let persistent = WebTaskBackend::open(temporary.path().join("persistent")).unwrap();
        {
            let mut queue = lock(&persistent.owner.shared.queue);
            queue.stopped = true;
            persistent.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&persistent.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&persistent.owner.shared.queue).stopped = false;
        persistent.owner.shared.success_transition_failures.store(3, Ordering::SeqCst);
        let mut first = persistent.begin_upload("persistent.txt", None).unwrap();
        first.write_chunk(b"persistent publication transition").unwrap();
        let first = first.finish().unwrap();
        let mut following = persistent.begin_upload("following.txt", None).unwrap();
        following.write_chunk(b"must not continue").unwrap();
        let following = following.finish().unwrap();
        let shared = Arc::clone(&persistent.owner.shared);
        lock(&persistent.owner.workers).push(std::thread::spawn(move || worker(shared)));
        persistent.owner.shared.queue_changed.notify_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let current = persistent.get(&first.id).unwrap();
            if current.status == TaskStatus::Converted
                && lock(&persistent.owner.shared.queue).stopped
            {
                break;
            }
            assert!(Instant::now() < deadline, "{current:?}");
            std::thread::yield_now();
        }
        assert_ne!(persistent.get(&following.id).unwrap().status, TaskStatus::Succeeded);
    }

    #[test]
    fn one_conversion_failure_does_not_stop_following_work() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let mut invalid = backend.begin_upload("broken.json", None).unwrap();
        invalid.write_chunk(br#"{"unterminated":"#).unwrap();
        let invalid = invalid.finish().unwrap();
        let mut valid = backend.begin_upload("valid.txt", None).unwrap();
        valid.write_chunk(b"still runs").unwrap();
        let valid = valid.finish().unwrap();
        assert_eq!(wait_terminal(&backend, &invalid.id).status, TaskStatus::Failed);
        assert_eq!(wait_terminal(&backend, &valid.id).status, TaskStatus::Succeeded);
    }

    #[test]
    fn durable_request_tamper_before_first_dequeue_fails_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        let mut upload = backend.begin_upload("a.txt", None).unwrap();
        upload.write_chunk(b"request-bound").unwrap();
        let task = upload.finish().unwrap();
        let request = root.join("objects").join(task.id.as_str()).join("request.json");
        let mut bytes = fs::read(&request).unwrap();
        let position = bytes.windows(5).position(|window| window == b"a.txt").unwrap();
        bytes[position] = b'b';
        let mut file = OpenOptions::new().write(true).truncate(true).open(request).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        let shared = Arc::clone(&backend.owner.shared);
        lock(&backend.owner.workers).push(std::thread::spawn(move || worker(shared)));
        backend.owner.shared.queue_changed.notify_all();
        assert_eq!(wait_terminal(&backend, &task.id).status, TaskStatus::Failed);
    }

    #[test]
    fn transient_dequeued_store_failures_retry_without_losing_the_job() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        let mut tasks = Vec::new();
        for name in ["retry-first.txt", "retry-next.txt"] {
            let mut upload = backend.begin_upload(name, None).unwrap();
            upload.write_chunk(b"retryable task").unwrap();
            tasks.push(upload.finish().unwrap().id);
        }
        backend.owner.shared.dequeue_get_failures.store(1, Ordering::SeqCst);
        backend.owner.shared.dequeue_transition_failures.store(1, Ordering::SeqCst);
        for _ in 0..MAX_WORKERS {
            let shared = Arc::clone(&backend.owner.shared);
            lock(&backend.owner.workers).push(std::thread::spawn(move || worker(shared)));
        }
        backend.owner.shared.queue_changed.notify_all();
        assert_eq!(wait_terminal(&backend, &tasks[0]).status, TaskStatus::Succeeded);
        assert_eq!(wait_terminal(&backend, &tasks[1]).status, TaskStatus::Succeeded);
    }

    #[test]
    fn registered_admission_ticket_is_released_on_preacquire_cancel_and_panic() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;

        let mut ids = Vec::new();
        for name in ["cancel-before-acquire.txt", "after-cancel.txt"] {
            let mut upload = backend.begin_upload(name, None).unwrap();
            upload.write_chunk(b"ticket cleanup").unwrap();
            ids.push(upload.finish().unwrap().id);
        }
        let gate = Arc::new(std::sync::Barrier::new(2));
        *lock(&backend.owner.shared.pre_acquire_gate) = Some(Arc::clone(&gate));
        let cancellation = lock(&backend.owner.shared.queue).cancellations[&ids[0]].clone();
        let shared = Arc::clone(&backend.owner.shared);
        lock(&backend.owner.workers).push(std::thread::spawn(move || worker(shared)));
        backend.owner.shared.queue_changed.notify_all();
        cancellation.cancel();
        gate.wait();
        *lock(&backend.owner.shared.pre_acquire_gate) = None;
        assert_eq!(wait_terminal(&backend, &ids[0]).status, TaskStatus::Cancelled);
        assert_eq!(wait_terminal(&backend, &ids[1]).status, TaskStatus::Succeeded);

        backend.owner.shared.pre_acquire_panic.store(1, Ordering::SeqCst);
        let mut panic_upload = backend.begin_upload("panic-before-acquire.txt", None).unwrap();
        panic_upload.write_chunk(b"panic ticket cleanup").unwrap();
        let panic_task = panic_upload.finish().unwrap();
        let mut following = backend.begin_upload("after-panic.txt", None).unwrap();
        following.write_chunk(b"still progresses").unwrap();
        let following = following.finish().unwrap();
        assert_eq!(wait_terminal(&backend, &panic_task.id).status, TaskStatus::Failed);
        assert_eq!(wait_terminal(&backend, &following.id).status, TaskStatus::Succeeded);
        assert!(lock(&backend.owner.shared.disk_bytes).waiters.is_empty());
    }

    #[test]
    fn admission_registration_failure_settles_current_and_queued_tasks() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        let mut ids = Vec::new();
        for name in ["ticket-overflow.txt", "queued-after-overflow.txt"] {
            let mut upload = backend.begin_upload(name, None).unwrap();
            upload.write_chunk(b"settle admission failure").unwrap();
            ids.push(upload.finish().unwrap().id);
        }
        lock(&backend.owner.shared.disk_bytes).next_ticket = u64::MAX;
        let shared = Arc::clone(&backend.owner.shared);
        lock(&backend.owner.workers).push(std::thread::spawn(move || worker(shared)));
        backend.owner.shared.queue_changed.notify_all();
        assert_eq!(wait_terminal(&backend, &ids[0]).status, TaskStatus::Failed);
        assert_eq!(wait_terminal(&backend, &ids[1]).status, TaskStatus::Interrupted);
        assert!(lock(&backend.owner.shared.disk_bytes).waiters.is_empty());
        assert!(lock(&backend.owner.shared.queue).stopped);
    }

    #[test]
    fn metadata_headroom_serializes_multiple_admission_failure_transitions_at_data_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;

        let mut ids = Vec::new();
        for index in 0..3 {
            let mut upload = backend.begin_upload(&format!("metadata-{index}.txt"), None).unwrap();
            upload.write_chunk(b"bounded metadata settlement").unwrap();
            ids.push(upload.finish().unwrap().id);
        }
        let before = measured_managed_bytes(&backend.owner.shared.root_handle).unwrap();
        let filler = backend
            .owner
            .shared
            .root_handle
            .create_regular_private(std::ffi::OsStr::new("metadata-boundary"))
            .unwrap();
        filler.set_len(MAX_DATA_BYTES - before).unwrap();
        filler.sync_all().unwrap();
        reconcile_managed_usage(&backend.owner.shared).unwrap();
        assert_eq!(lock(&backend.owner.shared.disk_bytes).used, MAX_DATA_BYTES);

        lock(&backend.owner.shared.disk_bytes).next_ticket = u64::MAX;
        let shared = Arc::clone(&backend.owner.shared);
        lock(&backend.owner.workers).push(std::thread::spawn(move || worker(shared)));
        backend.owner.shared.queue_changed.notify_all();
        assert_eq!(wait_terminal(&backend, &ids[0]).status, TaskStatus::Failed);
        assert_eq!(wait_terminal(&backend, &ids[1]).status, TaskStatus::Interrupted);
        assert_eq!(wait_terminal(&backend, &ids[2]).status, TaskStatus::Interrupted);
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        assert!(
            measured_managed_bytes(&backend.owner.shared.root_handle).unwrap() <= MAX_GLOBAL_BYTES
        );
        assert!(lock(&backend.owner.shared.queue).stopped);
    }

    #[test]
    fn exhausted_metadata_reservation_stops_bulk_settlement_without_swallowing_the_error() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;

        let mut ids = Vec::new();
        for index in 0..3 {
            let mut upload =
                backend.begin_upload(&format!("metadata-full-{index}.txt"), None).unwrap();
            upload.write_chunk(b"must remain recoverable").unwrap();
            ids.push(upload.finish().unwrap().id);
        }
        let before = measured_managed_bytes(&backend.owner.shared.root_handle).unwrap();
        let filler = backend
            .owner
            .shared
            .root_handle
            .create_regular_private(std::ffi::OsStr::new("metadata-exhausted"))
            .unwrap();
        filler.set_len(MAX_GLOBAL_BYTES - STORE_MUTATION_RESERVATION + 1 - before).unwrap();
        filler.sync_all().unwrap();
        reconcile_managed_usage(&backend.owner.shared).unwrap();

        lock(&backend.owner.shared.disk_bytes).next_ticket = u64::MAX;
        let shared = Arc::clone(&backend.owner.shared);
        lock(&backend.owner.workers).push(std::thread::spawn(move || worker(shared)));
        backend.owner.shared.queue_changed.notify_all();
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        assert!(lock(&backend.owner.shared.queue).stopped);
        for id in &ids {
            assert_eq!(backend.get(id).unwrap().status, TaskStatus::Pending);
        }
        assert_eq!(lock(&backend.owner.shared.queue).jobs.len(), 2);
        assert!(
            measured_managed_bytes(&backend.owner.shared.root_handle).unwrap() <= MAX_GLOBAL_BYTES
        );
    }

    #[test]
    fn batch_dequeues_in_stable_order_respects_concurrency_and_isolates_failures() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        let mut submitted = Vec::new();
        for index in 0..32 {
            let mut upload = backend.begin_upload(&format!("{index:02}.txt"), None).unwrap();
            if index % 7 == 0 {
                upload.write_chunk(&[0, 0xff, 0]).unwrap();
            } else {
                upload.write_chunk(format!("item {index}").as_bytes()).unwrap();
            }
            submitted.push(upload.finish().unwrap().id);
        }
        let terminal: Vec<_> = submitted.iter().map(|id| wait_terminal(&backend, id)).collect();
        assert!(terminal.iter().any(|record| record.status == TaskStatus::Failed));
        assert!(terminal.iter().any(|record| record.status == TaskStatus::Succeeded));
        assert_eq!(*lock(&backend.owner.shared.dequeue_order), submitted);
        assert_eq!(*lock(&backend.owner.shared.admission_order), submitted);
        assert!(backend.owner.shared.max_active_workers.load(Ordering::SeqCst) <= MAX_WORKERS);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while backend.owner.shared.active_workers.load(Ordering::SeqCst) != 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn cancellation_race_has_only_prepublication_cancel_or_complete_success() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        for index in 0..20 {
            let mut upload = backend.begin_upload(&format!("cancel-{index}.txt"), None).unwrap();
            upload.write_chunk(&vec![b'x'; 256 * 1024]).unwrap();
            let task = upload.finish().unwrap();
            backend.cancel(&task.id).unwrap();
            let terminal = wait_terminal(&backend, &task.id);
            match terminal.status {
                TaskStatus::Cancelled => {
                    assert!(
                        !root.join("objects").join(task.id.as_str()).join("published").exists()
                    );
                }
                TaskStatus::Succeeded => {
                    let published = root.join("objects").join(task.id.as_str()).join("published");
                    assert!(published.is_dir());
                    assert_eq!(
                        validate_manifest_handle(&SafeDir::open_absolute(&published).unwrap())
                            .unwrap(),
                        terminal.artifacts
                    );
                }
                other => panic!("invalid cancellation race outcome: {other:?}"),
            }
        }
    }

    #[test]
    fn restart_recovers_more_than_one_task_store_page_without_skipping() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let mut submitted = Vec::new();
        {
            let backend = WebTaskBackend::open(&root).unwrap();
            {
                let mut queue = lock(&backend.owner.shared.queue);
                queue.stopped = true;
                backend.owner.shared.queue_changed.notify_all();
            }
            for worker in lock(&backend.owner.workers).drain(..) {
                worker.join().unwrap();
            }
            lock(&backend.owner.shared.queue).stopped = false;
            for index in 0..101 {
                let mut upload = backend.begin_upload(&format!("{index:03}.txt"), None).unwrap();
                upload.write_chunk(b"recover me").unwrap();
                submitted.push(upload.finish().unwrap().id);
            }
        }
        let backend = WebTaskBackend::open(&root).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            let recovered = queue.jobs.len() + lock(&backend.owner.shared.dequeue_order).len();
            assert_eq!(recovered, submitted.len());
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
    }

    #[test]
    fn process_abort_at_each_publication_phase_recovers_without_false_failure() {
        for phase in ["after-stage-fsync", "after-published-rename", "after-taskstore-success"] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("backend");
            let id = create_pending_without_workers(&root, b"crash recovery");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "web_tasks::tests::web_crash_helper", "--nocapture"])
                .env("INTO_MD_WEB_CRASH_ROOT", &root)
                .env("INTO_MD_WEB_CRASH_PHASE", phase)
                .status()
                .unwrap();
            assert!(!status.success(), "phase {phase} did not abort");
            let backend = WebTaskBackend::open(&root).unwrap();
            let recovered = wait_terminal(&backend, &id);
            assert_eq!(recovered.status, TaskStatus::Succeeded, "phase {phase}: {recovered:?}");
            let task = root.join("objects").join(id.as_str());
            assert!(
                fs::read_dir(task).unwrap().all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("stage-"))
            );
        }
    }

    #[test]
    fn same_length_artifact_tamper_is_rejected_by_digest() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let mut upload = backend.begin_upload("a.txt", None).unwrap();
        upload.write_chunk(b"hello").unwrap();
        let task = upload.finish().unwrap();
        let complete = wait_terminal(&backend, &task.id);
        let artifact = complete
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Markdown)
            .unwrap();
        let path = root
            .join("objects")
            .join(task.id.as_str())
            .join("published")
            .join(&artifact.storage_key);
        let mut file = OpenOptions::new().write(true).truncate(true).open(path).unwrap();
        file.write_all(&vec![b'x'; usize::try_from(artifact.byte_len).unwrap()]).unwrap();
        file.sync_all().unwrap();
        assert!(matches!(
            backend.artifact(&task.id, &artifact.storage_key),
            Err(WebTaskError::Unsafe(_))
        ));
    }

    #[test]
    fn verified_download_snapshot_is_stable_after_published_inode_overwrite_and_cleans_up() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let mut upload = backend.begin_upload("a.txt", None).unwrap();
        upload.write_chunk(b"hello").unwrap();
        let task = upload.finish().unwrap();
        let complete = wait_terminal(&backend, &task.id);
        let artifact = complete
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Markdown)
            .unwrap();
        let (mut snapshot, _) = backend.artifact(&task.id, &artifact.storage_key).unwrap();
        let published = root
            .join("objects")
            .join(task.id.as_str())
            .join("published")
            .join(&artifact.storage_key);
        let mut changed = OpenOptions::new().write(true).truncate(true).open(published).unwrap();
        changed.write_all(&vec![b'x'; usize::try_from(artifact.byte_len).unwrap()]).unwrap();
        changed.sync_all().unwrap();
        let mut bytes = Vec::new();
        snapshot.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"hello\n");
        drop(snapshot);
        assert_eq!(fs::read_dir(root.join("snapshots")).unwrap().count(), 0);
    }

    #[test]
    fn snapshot_failures_leave_no_named_inode_or_quota_charge() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let mut upload = backend.begin_upload("a.txt", None).unwrap();
        upload.write_chunk(b"hello").unwrap();
        let task = upload.finish().unwrap();
        let complete = wait_terminal(&backend, &task.id);
        let artifact = complete
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == ArtifactKind::Markdown)
            .unwrap();
        let baseline = lock(&backend.owner.shared.disk_bytes).reserved;
        for phase in 1..=3 {
            backend.owner.shared.snapshot_failure.store(phase, Ordering::SeqCst);
            assert!(matches!(
                backend.artifact(&task.id, &artifact.storage_key),
                Err(WebTaskError::Io(_))
            ));
            assert_eq!(fs::read_dir(root.join("snapshots")).unwrap().count(), 0);
            assert_eq!(lock(&backend.owner.shared.disk_bytes).reserved, baseline);
        }
    }

    #[test]
    fn invalid_published_sets_never_leave_converted_tasks_hanging() {
        for corruption in ["manifest", "extra", "missing", "digest"] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("backend");
            let backend = WebTaskBackend::open(&root).unwrap();
            {
                let mut queue = lock(&backend.owner.shared.queue);
                queue.stopped = true;
                backend.owner.shared.queue_changed.notify_all();
            }
            for worker in lock(&backend.owner.workers).drain(..) {
                worker.join().unwrap();
            }
            lock(&backend.owner.shared.queue).stopped = false;
            let mut upload = backend.begin_upload("a.txt", None).unwrap();
            upload.write_chunk(b"hello").unwrap();
            let task = upload.finish().unwrap();
            let mut store = lock(&backend.owner.shared.task_store);
            store
                .transition(
                    &task.id,
                    TaskTransition {
                        expected: TaskStatus::Pending,
                        next: TaskStatus::Running,
                        progress_millionths: 1,
                        diagnostics: Vec::new(),
                        artifacts: Vec::new(),
                    },
                )
                .unwrap();
            store
                .transition(
                    &task.id,
                    TaskTransition {
                        expected: TaskStatus::Running,
                        next: TaskStatus::Converted,
                        progress_millionths: 900_000,
                        diagnostics: Vec::new(),
                        artifacts: Vec::new(),
                    },
                )
                .unwrap();
            drop(store);
            let cancellation = CancellationToken::new();
            let result = into_markdown::ConversionResult::new(
                into_markdown::Document::default(),
                "hello\n".into(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            );
            let references =
                publish_result(&backend.owner.shared, &task.id, &result, &cancellation).unwrap();
            let published = root.join("objects").join(task.id.as_str()).join("published");
            match corruption {
                "manifest" => {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(published.join("manifest.json"))
                        .unwrap();
                    file.write_all(b"broken").unwrap();
                    file.sync_all().unwrap();
                }
                "extra" => write_private(&published.join("extra"), b"x").unwrap(),
                "missing" => fs::remove_file(published.join(&references[0].storage_key)).unwrap(),
                "digest" => {
                    let path = published.join(&references[0].storage_key);
                    let length = usize::try_from(references[0].byte_len).unwrap();
                    let mut file =
                        OpenOptions::new().write(true).truncate(true).open(path).unwrap();
                    file.write_all(&vec![b'x'; length]).unwrap();
                    file.sync_all().unwrap();
                }
                _ => unreachable!(),
            }
            reconcile_or_fail(&backend.owner.shared, &task.id);
            assert_eq!(backend.get(&task.id).unwrap().status, TaskStatus::Failed);
            assert!(!published.exists());
            drop(backend);
            assert!(WebTaskBackend::open(&root).is_ok());
        }
    }

    #[test]
    fn transient_reconcile_and_failure_transition_errors_do_not_kill_queue_progress() {
        let temporary = tempfile::tempdir().unwrap();
        let backend = WebTaskBackend::open(temporary.path().join("backend")).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        let mut first = backend.begin_upload("first.txt", None).unwrap();
        first.write_chunk(b"first").unwrap();
        let first = first.finish().unwrap();
        backend.owner.shared.reconcile_failures.store(1, Ordering::SeqCst);
        backend.owner.shared.fail_transition_failures.store(2, Ordering::SeqCst);
        reconcile_or_fail(&backend.owner.shared, &first.id);
        assert_eq!(backend.get(&first.id).unwrap().status, TaskStatus::Failed);
        assert!(!lock(&backend.owner.shared.queue).stopped);
        lock(&backend.owner.shared.queue).jobs.clear();
        for index in 0..MAX_WORKERS {
            let shared = Arc::clone(&backend.owner.shared);
            lock(&backend.owner.workers).push(
                std::thread::Builder::new()
                    .name(format!("retry-worker-{index}"))
                    .spawn(move || worker(shared))
                    .unwrap(),
            );
        }

        let mut next = backend.begin_upload("next.txt", None).unwrap();
        next.write_chunk(b"next").unwrap();
        let next = next.finish().unwrap();
        assert_eq!(wait_terminal(&backend, &next.id).status, TaskStatus::Succeeded);
        assert_eq!(lock(&backend.owner.workers).len(), MAX_WORKERS);
    }

    #[test]
    fn published_asset_index_preserves_ir_id_filename_and_media_type() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        {
            let mut queue = lock(&backend.owner.shared.queue);
            queue.stopped = true;
            backend.owner.shared.queue_changed.notify_all();
        }
        for worker in lock(&backend.owner.workers).drain(..) {
            worker.join().unwrap();
        }
        lock(&backend.owner.shared.queue).stopped = false;
        let mut upload = backend.begin_upload("asset.txt", None).unwrap();
        upload.write_chunk(b"asset fixture").unwrap();
        let task = upload.finish().unwrap();
        let asset = into_markdown::Asset {
            id: into_markdown::AssetId("image-1".into()),
            filename: Some("image.png".into()),
            media_type: "image/png".into(),
            bytes: vec![1, 2, 3],
            external_uri: None,
        };
        let document = into_markdown::Document {
            blocks: vec![into_markdown::BlockNode {
                id: into_markdown::NodeId("block-1".into()),
                block: into_markdown::Block::Image {
                    asset: asset.id.clone(),
                    alt: Some("image".into()),
                },
                provenance: into_markdown::Provenance {
                    kind: into_markdown::ProvenanceKind::NativeParser,
                    provider: "test".into(),
                    locator: into_markdown::SourceLocator::default(),
                    confidence: None,
                },
            }],
            ..into_markdown::Document::default()
        };
        let result = into_markdown::ConversionResult::new(
            document,
            "![image](image.png)\n".into(),
            vec![asset],
            Vec::new(),
            Vec::new(),
        );
        let references =
            publish_result(&backend.owner.shared, &task.id, &result, &CancellationToken::new())
                .unwrap();
        let reference =
            references.iter().find(|reference| reference.kind == ArtifactKind::Asset).unwrap();
        assert_eq!(reference.asset_id.as_deref(), Some("image-1"));
        assert_eq!(reference.filename.as_deref(), Some("image.png"));
        assert_eq!(reference.media_type.as_deref(), Some("image/png"));
        let published = backend
            .owner
            .shared
            .objects
            .open_child_private(std::ffi::OsStr::new(task.id.as_str()))
            .unwrap()
            .open_child_private(std::ffi::OsStr::new("published"))
            .unwrap();
        assert!(artifact_sets_equal(&references, &validate_manifest_handle(&published).unwrap()));
    }

    #[test]
    fn restart_removes_only_authenticated_crash_residue_and_joins_workers() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        {
            let _backend = WebTaskBackend::open(&root).unwrap();
            let incoming = root.join("incoming").join("00112233445566778899aabbccddeeff");
            create_private_child(&incoming).unwrap();
            write_private(&incoming.join("payload"), b"partial").unwrap();
            // Dropping the last handle wakes and joins all four idle workers.
        }
        let _backend = WebTaskBackend::open(&root).unwrap();
        assert_eq!(fs::read_dir(root.join("incoming")).unwrap().count(), 0);
    }

    #[test]
    fn restart_marks_database_only_create_boundary_interrupted_instead_of_orphaning() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let id = {
            let backend = WebTaskBackend::open(&root).unwrap();
            let token = backend.owner.shared.recovery.create_token().unwrap();
            let record = lock(&backend.owner.shared.task_store)
                .create(NewTask {
                    input: InputReference {
                        schema_version: 1,
                        input_fingerprint: "a".repeat(64),
                        options_fingerprint: "b".repeat(64),
                        byte_len: 1,
                        recovery_token: token.as_str().to_owned(),
                    },
                    configuration: ConfigurationSnapshot::default(),
                })
                .unwrap();
            record.id
        };
        let backend = WebTaskBackend::open(&root).unwrap();
        assert_eq!(backend.get(&id).unwrap().status, TaskStatus::Interrupted);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_hardlink_artifacts_fail_closed() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let directory = private_directory(temporary.path().join("private")).unwrap();
        symlink("outside", directory.join("linked")).unwrap();
        assert!(open_regular_nofollow(&directory.join("linked")).is_err());
        let source = directory.join("source");
        write_private(&source, b"x").unwrap();
        fs::hard_link(&source, directory.join("alias")).unwrap();
        assert!(open_regular_nofollow(&source).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unquarantinable_symlink_member_marks_backend_unhealthy() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let mut upload = backend.begin_upload("a.txt", None).unwrap();
        upload.write_chunk(b"hello").unwrap();
        let task = upload.finish().unwrap();
        assert_eq!(wait_terminal(&backend, &task.id).status, TaskStatus::Succeeded);
        let published = root.join("objects").join(task.id.as_str()).join("published");
        symlink("outside", published.join("attacker-link")).unwrap();
        reconcile_or_fail(&backend.owner.shared, &task.id);
        assert!(lock(&backend.owner.shared.queue).stopped);
        assert!(backend.get(&task.id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn retained_objects_descriptor_never_follows_a_renamed_symlink_replacement() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        let outside = temporary.path().join("outside");
        create_private_child(&outside).unwrap();
        fs::rename(root.join("objects"), root.join("objects-moved")).unwrap();
        symlink(&outside, root.join("objects")).unwrap();
        match backend.begin_upload("swap.txt", None) {
            Ok(mut upload) => {
                upload.write_chunk(b"never outside").unwrap();
                assert!(matches!(upload.finish(), Err(WebTaskError::Unsafe(_))));
            }
            Err(WebTaskError::Unsafe(_)) => {}
            Err(error) => panic!("unexpected replacement failure: {error:?}"),
        }
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn root_swap_between_store_opens_fails_before_splitting_namespaces() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        SWAP_ROOT_AFTER_TASK_STORE_OPEN.with(|swap| swap.set(true));
        assert!(WebTaskBackend::open(&root).is_err());
        assert!(!root.join("recovery").exists());
        assert!(root.with_extension("authenticated").join("database").exists());
    }

    #[cfg(unix)]
    #[test]
    fn managed_directory_permission_change_after_open_fails_closed() {
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("backend");
        let backend = WebTaskBackend::open(&root).unwrap();
        fs::set_permissions(root.join("objects"), fs::Permissions::from_mode(0o755)).unwrap();
        let mut upload = backend.begin_upload("mode.txt", None).unwrap();
        upload.write_chunk(b"private").unwrap();
        assert!(matches!(upload.finish(), Err(WebTaskError::Unsafe(_))));
    }
}
