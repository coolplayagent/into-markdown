use crate::{ConversionError, InputFormat, ResourceLimits};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc::Sender;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Ordered stages shared by library, CLI, and service-provider integrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ExecutionStage {
    /// Resolve the caller's source into bounded bytes.
    Resolving,
    /// Detect candidate formats.
    Detecting,
    /// Probe registered converters.
    Probing,
    /// Convert into the unified document IR.
    Converting,
    /// Perform local OCR work requested by a converter.
    Ocr,
    /// Perform AI or transcription work requested by a converter.
    Ai,
    /// Render the validated IR.
    Rendering,
    /// The requested operation completed successfully.
    Completed,
}

impl ExecutionStage {
    const fn rank(self) -> u16 {
        match self {
            Self::Resolving => 0,
            Self::Detecting => 1,
            Self::Probing => 2,
            Self::Converting | Self::Ocr | Self::Ai => 3,
            Self::Rendering => 4,
            Self::Completed => 5,
        }
    }

    const fn base_basis_points(self) -> u16 {
        match self {
            Self::Resolving => 0,
            Self::Detecting => 1_500,
            Self::Probing => 3_000,
            Self::Converting | Self::Ocr | Self::Ai => 4_500,
            Self::Rendering => 8_000,
            Self::Completed => 10_000,
        }
    }
}

/// One monotonic, object-safe progress notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    /// Current execution stage.
    pub stage: ExecutionStage,
    /// Overall completion in basis points (`0..=10_000`).
    pub basis_points: u16,
    /// Optional completed units within this stage.
    pub completed_units: Option<u64>,
    /// Optional total units within this stage.
    pub total_units: Option<u64>,
    /// Sanitized implementation detail suitable for user interfaces.
    pub message: Option<String>,
}

/// Synchronous callback invoked on an isolated dispatcher thread.
///
/// A callback which never returns necessarily retains that dispatcher thread
/// and this listener; it cannot block the conversion or context-drop path.
pub trait ProgressListener: Send + Sync + 'static {
    /// Observe a progress event. Implementations should return promptly.
    fn on_progress(&self, event: ProgressEvent);
}

/// Cloneable cooperative cancellation handle.
#[derive(Clone, Default)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    waiters: Mutex<BTreeMap<u64, Waker>>,
    next_waiter: AtomicU64,
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancellationToken {
    /// Create an uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether two handles refer to the same cancellation generation.
    #[doc(hidden)]
    #[must_use]
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Request cancellation and wake the currently polled pipeline operation.
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.wake_waiters();
        }
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    fn wake_waiters(&self) {
        let waiters = std::mem::take(&mut *lock_unpoisoned(&self.state.waiters));
        for (_, waker) in waiters {
            waker.wake();
        }
    }
}

/// Caller controls used to create one execution context per request.
#[derive(Clone, Default)]
pub struct ExecutionOptions {
    /// Cooperative cancellation handle.
    pub cancellation: CancellationToken,
    /// Total wall-clock timeout for the complete request.
    pub timeout: Option<Duration>,
    /// Optional progress observer.
    pub progress_listener: Option<Arc<dyn ProgressListener>>,
}

impl fmt::Debug for ExecutionOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionOptions")
            .field("cancellation", &self.cancellation)
            .field("timeout", &self.timeout)
            .field("progress_listener", &self.progress_listener.as_ref().map(|_| "registered"))
            .finish()
    }
}

/// Request-scoped cancellation, deadline, progress, and resource accounting.
pub struct ExecutionContext {
    shared: Arc<ExecutionShared>,
    memory_credit: Option<Arc<MemoryCredit>>,
    ignore_request_controls: bool,
}

/// Invocation-wide resource accounting shared by a root context and every fork.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionResourceUsage {
    /// Configured shared memory lease budget.
    pub shared_lease_budget_bytes: u64,
    /// Historical shared memory lease high-water mark.
    pub shared_lease_peak_bytes: u64,
    /// Configured shared temporary-storage lease budget.
    pub temporary_lease_budget_bytes: u64,
    /// Historical shared temporary-storage lease high-water mark.
    pub temporary_lease_peak_bytes: u64,
    /// OCR regions that survived component filtering and merge.
    pub ocr_recognized_regions: u64,
    /// Unicode scalar values contributed by those OCR regions.
    pub ocr_recognized_chars: u64,
}

impl Clone for ExecutionContext {
    fn clone(&self) -> Self {
        // A preflight credit is a scoped capability borrowed from one mutable
        // parent reservation. Public context clones keep the request controls
        // and global counters, but deliberately cannot detach that capability
        // from its borrow. Reservations created through the scoped context use
        // `clone_with_memory_account` below so their RAII drops still debit the
        // exact child counter they charged.
        Self {
            shared: Arc::clone(&self.shared),
            memory_credit: None,
            ignore_request_controls: self.ignore_request_controls,
        }
    }
}

struct MemoryCredit {
    backing: Arc<MemoryBacking>,
    bytes: AtomicU64,
}

struct MemoryBacking {
    shared: Arc<ExecutionShared>,
    bytes: AtomicU64,
}

impl Drop for MemoryBacking {
    fn drop(&mut self) {
        self.shared
            .resources
            .memory_bytes
            .fetch_sub(self.bytes.load(Ordering::Acquire), Ordering::AcqRel);
    }
}

#[derive(Clone, Default)]
struct SharedResourceAccounting {
    memory_bytes: Arc<AtomicU64>,
    memory_peak_bytes: Arc<AtomicU64>,
    temporary_bytes: Arc<AtomicU64>,
    temporary_peak_bytes: Arc<AtomicU64>,
    ocr: Arc<Mutex<OcrAccounting>>,
}

#[derive(Default)]
struct OcrAccounting {
    recognized_regions: u64,
    recognized_chars: u64,
}

struct ExecutionShared {
    cancellation: CancellationToken,
    deadline: Option<Instant>,
    deadline_timer_available: AtomicBool,
    timed_out: AtomicBool,
    timer_stop: Arc<TimerStop>,
    progress: Mutex<ProgressState>,
    detected_format: Mutex<Option<InputFormat>>,
    dispatcher: Option<ProgressDispatcher>,
    limits: ResourceLimits,
    resources: SharedResourceAccounting,
    temporary_directory: PathBuf,
    media_checkpoint: Mutex<Option<crate::media_checkpoint::SharedMediaCheckpointBackend>>,
}

#[derive(Default)]
struct TimerStop {
    stopped: Mutex<bool>,
    changed: Condvar,
}

#[derive(Default)]
struct ProgressState {
    rank: u16,
    stage: Option<ExecutionStage>,
    basis_points: u16,
    started: bool,
    completed: bool,
    sequence: u64,
    completed_units: Option<u64>,
    total_units: Option<u64>,
    message: Option<String>,
}

impl fmt::Debug for ExecutionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionContext")
            .field("cancelled", &self.shared.cancellation.is_cancelled())
            .field("deadline", &self.shared.deadline)
            .field("limits", &self.shared.limits)
            .finish_non_exhaustive()
    }
}

impl ExecutionContext {
    /// Construct a context from request controls and conversion limits.
    #[must_use]
    pub fn new(options: ExecutionOptions, limits: ResourceLimits) -> Self {
        Self::new_with_timer_spawner(
            options,
            limits,
            std::env::temp_dir(),
            std::thread::Builder::spawn,
        )
    }

    /// Construct a context whose automatically cleaned temporary files stay in an
    /// already-authorized request directory.
    ///
    /// Sandboxed process hosts use this when the platform temp-directory API points
    /// outside the sandbox despite an explicit private working directory.
    #[doc(hidden)]
    #[must_use]
    pub fn new_with_temporary_directory(
        options: ExecutionOptions,
        limits: ResourceLimits,
        temporary_directory: PathBuf,
    ) -> Self {
        Self::new_with_timer_spawner(
            options,
            limits,
            temporary_directory,
            std::thread::Builder::spawn,
        )
    }

    fn new_with_timer_spawner<F>(
        options: ExecutionOptions,
        limits: ResourceLimits,
        temporary_directory: PathBuf,
        spawn: F,
    ) -> Self
    where
        F: FnOnce(
            std::thread::Builder,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> io::Result<JoinHandle<()>>,
    {
        Self::new_with_timer_spawner_and_counters(
            options,
            limits,
            temporary_directory,
            SharedResourceAccounting::default(),
            spawn,
        )
    }

    fn new_with_timer_spawner_and_counters<F>(
        options: ExecutionOptions,
        limits: ResourceLimits,
        temporary_directory: PathBuf,
        resources: SharedResourceAccounting,
        spawn: F,
    ) -> Self
    where
        F: FnOnce(
            std::thread::Builder,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> io::Result<JoinHandle<()>>,
    {
        let now = Instant::now();
        let deadline = options.timeout.and_then(|duration| now.checked_add(duration));
        let dispatcher = options.progress_listener.and_then(ProgressDispatcher::new);
        let timer_stop = Arc::new(TimerStop::default());
        let timer_deadline = deadline.filter(|deadline| *deadline > now);
        let shared = Arc::new(ExecutionShared {
            cancellation: options.cancellation,
            deadline,
            deadline_timer_available: AtomicBool::new(timer_deadline.is_none()),
            timed_out: AtomicBool::new(false),
            timer_stop: Arc::clone(&timer_stop),
            progress: Mutex::new(ProgressState::default()),
            detected_format: Mutex::new(None),
            dispatcher,
            limits,
            resources,
            temporary_directory,
            media_checkpoint: Mutex::new(None),
        });
        if let Some(deadline) = timer_deadline {
            let weak = Arc::downgrade(&shared);
            let task = Box::new(move || {
                let now = Instant::now();
                if deadline > now {
                    let wait = deadline.duration_since(now);
                    let guard = lock_unpoisoned(&timer_stop.stopped);
                    let (guard, _) = timer_stop
                        .changed
                        .wait_timeout_while(guard, wait, |stopped| !*stopped)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if *guard {
                        return;
                    }
                }
                let Some(shared) = weak.upgrade() else { return };
                if Instant::now() >= deadline && !shared.cancellation.is_cancelled() {
                    shared.timed_out.store(true, Ordering::Release);
                    shared.cancellation.wake_waiters();
                }
            });
            if spawn(std::thread::Builder::new().name("into-markdown-deadline".into()), task)
                .is_ok()
            {
                shared.deadline_timer_available.store(true, Ordering::Release);
            }
        }
        Self { shared, memory_credit: None, ignore_request_controls: false }
    }

    /// Create an independently cancellable request which leases from this
    /// context's shared memory and temporary-storage pools.
    ///
    /// Batch schedulers use this to prevent a per-request limit from being
    /// multiplied by concurrency while retaining per-item deadlines and
    /// progress state.
    #[doc(hidden)]
    #[must_use]
    pub fn fork_with_shared_resources(&self, options: ExecutionOptions) -> Self {
        Self::new_with_timer_spawner_and_counters(
            options,
            self.shared.limits.clone(),
            self.shared.temporary_directory.clone(),
            self.shared.resources.clone(),
            std::thread::Builder::spawn,
        )
    }

    /// Record the best format established by detection or authoritative probing.
    #[doc(hidden)]
    pub fn record_detected_format(&self, format: InputFormat) {
        *lock_unpoisoned(&self.shared.detected_format) = Some(format);
    }

    /// Return the format established so far, including after a later conversion failure.
    #[doc(hidden)]
    #[must_use]
    pub fn detected_format(&self) -> Option<InputFormat> {
        *lock_unpoisoned(&self.shared.detected_format)
    }

    /// Return a typed cancellation or timeout error at a cooperative checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::Cancelled`], [`ConversionError::Timeout`], or a
    /// stable [`ConversionError::ComponentUnavailable`] when the process could
    /// not start the deadline timer.
    pub fn checkpoint(&self) -> Result<(), ConversionError> {
        if self.ignore_request_controls {
            return Ok(());
        }
        if !self.shared.deadline_timer_available.load(Ordering::Acquire) {
            return Err(ConversionError::ComponentUnavailable {
                component: "deadline-timer".into(),
                detail: "deadline timer could not be started".into(),
            });
        }
        if self.shared.cancellation.is_cancelled() {
            return Err(ConversionError::Cancelled);
        }
        if self.shared.timed_out.load(Ordering::Acquire)
            || self.shared.deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.shared.timed_out.store(true, Ordering::Release);
            return Err(ConversionError::Timeout);
        }
        Ok(())
    }

    /// Create a cleanup-only view which shares this request's exact resource
    /// counters and limits while allowing mandatory rollback after cancellation
    /// or timeout has already won.
    #[doc(hidden)]
    #[must_use]
    pub fn cleanup_scope(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            memory_credit: None,
            ignore_request_controls: true,
        }
    }

    /// Install the durable media checkpoint seam for one recoverable request.
    #[doc(hidden)]
    pub fn install_media_checkpoint_backend(
        &self,
        backend: Arc<dyn crate::MediaCheckpointBackend>,
    ) -> Result<(), ConversionError> {
        let mut current = lock_unpoisoned(&self.shared.media_checkpoint);
        if current.is_some() {
            return Err(ConversionError::Internal {
                detail: "media checkpoint backend was installed more than once".into(),
            });
        }
        *current = Some(backend);
        Ok(())
    }

    /// Load the latest durable media checkpoint when conversion is recoverable.
    ///
    /// # Errors
    ///
    /// Returns an error when cancellation wins or the checkpoint backend rejects the read.
    pub fn load_media_checkpoint(
        &self,
    ) -> Result<Option<crate::RecoveredMediaCheckpoint>, ConversionError> {
        self.checkpoint()?;
        let backend = lock_unpoisoned(&self.shared.media_checkpoint).clone();
        backend.map_or(Ok(None), |backend| backend.load(self))
    }

    /// Atomically commit one long-form media processing boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is invalid, cancellation wins, or persistence fails.
    pub fn commit_media_checkpoint(
        &self,
        checkpoint: &crate::MediaCheckpoint,
    ) -> Result<(), ConversionError> {
        self.checkpoint()?;
        checkpoint.validate()?;
        let backend = lock_unpoisoned(&self.shared.media_checkpoint).clone();
        if let Some(backend) = backend {
            backend.commit(checkpoint, self)?;
        }
        self.checkpoint()
    }

    /// Remaining time before the request deadline, when one exists.
    ///
    /// The returned duration is derived from the same monotonic clock used by
    /// [`Self::checkpoint`]. Callers must still checkpoint after every blocking
    /// operation because cancellation can win independently of the deadline.
    #[must_use]
    pub fn remaining_time(&self) -> Option<Duration> {
        self.shared.deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Await one provider operation while enforcing the total request deadline.
    pub fn run<F>(&self, future: F) -> CheckedFuture<'_, F>
    where
        F: Future,
    {
        let waiter_id = self.shared.cancellation.state.next_waiter.fetch_add(1, Ordering::Relaxed);
        CheckedFuture { context: self, future: Box::pin(future), waiter_id, registered: false }
    }

    /// Publish monotonic progress without blocking on the listener.
    ///
    /// # Errors
    ///
    /// Returns a cancellation or timeout error from the pre-publication checkpoint.
    pub fn report(
        &self,
        stage: ExecutionStage,
        completed_units: Option<u64>,
        total_units: Option<u64>,
        message: Option<impl Into<String>>,
    ) -> Result<(), ConversionError> {
        self.checkpoint()?;
        let message = message.map(Into::into);
        let stage_fraction = u16::try_from(match (completed_units, total_units) {
            (Some(completed), Some(total)) if total > 0 => {
                u128::from(completed.min(total)) * 1_000 / u128::from(total)
            }
            _ => 0,
        })
        .unwrap_or(1_000);
        let basis_points = if stage == ExecutionStage::Completed {
            10_000
        } else {
            stage.base_basis_points().saturating_add(stage_fraction.min(999))
        };
        let mut progress = lock_unpoisoned(&self.shared.progress);
        if progress.completed || progress.started && stage.rank() < progress.rank {
            return Ok(());
        }
        let basis_points = basis_points.max(progress.basis_points);
        if progress.started
            && progress.stage == Some(stage)
            && basis_points == progress.basis_points
            && progress.completed_units == completed_units
            && progress.total_units == total_units
            && progress.message == message
        {
            return Ok(());
        }
        progress.started = true;
        progress.rank = stage.rank();
        progress.stage = Some(stage);
        progress.basis_points = basis_points;
        progress.completed_units = completed_units;
        progress.total_units = total_units;
        progress.message.clone_from(&message);
        progress.completed = stage == ExecutionStage::Completed;
        progress.sequence = progress.sequence.saturating_add(1);
        let sequence = progress.sequence;
        if let Some(dispatcher) = &self.shared.dispatcher {
            dispatcher.publish(
                sequence,
                ProgressEvent { stage, basis_points, completed_units, total_units, message },
            );
        }
        drop(progress);
        Ok(())
    }

    /// Reserve accounted memory until the returned guard is dropped.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, or resource-limit error.
    pub fn reserve_memory(&self, bytes: u64) -> Result<ResourceReservation, ConversionError> {
        self.reserve(ResourceKind::Memory, bytes)
    }

    /// Current live memory reservations for boundary and lifecycle audits.
    #[doc(hidden)]
    #[must_use]
    pub fn reserved_memory_bytes(&self) -> u64 {
        if let Some(credit) = &self.memory_credit {
            credit.bytes.load(Ordering::Acquire)
        } else {
            self.shared.resources.memory_bytes.load(Ordering::Acquire)
        }
    }

    /// Remaining request memory available for a preflighted SPI allocation plan.
    #[doc(hidden)]
    #[must_use]
    pub fn available_memory_bytes(&self) -> u64 {
        if let Some(credit) = &self.memory_credit {
            credit
                .backing
                .bytes
                .load(Ordering::Acquire)
                .saturating_sub(credit.bytes.load(Ordering::Acquire))
        } else {
            self.shared
                .limits
                .max_memory_bytes
                .saturating_sub(self.shared.resources.memory_bytes.load(Ordering::Acquire))
        }
    }

    /// Return the invocation-wide historical resource accounting shared with every fork.
    #[doc(hidden)]
    #[must_use]
    pub fn resource_usage(&self) -> ExecutionResourceUsage {
        let ocr = lock_unpoisoned(&self.shared.resources.ocr);
        ExecutionResourceUsage {
            shared_lease_budget_bytes: self.shared.limits.max_memory_bytes,
            shared_lease_peak_bytes: self
                .shared
                .resources
                .memory_peak_bytes
                .load(Ordering::Acquire),
            temporary_lease_budget_bytes: self.shared.limits.max_temporary_bytes,
            temporary_lease_peak_bytes: self
                .shared
                .resources
                .temporary_peak_bytes
                .load(Ordering::Acquire),
            ocr_recognized_regions: ocr.recognized_regions,
            ocr_recognized_chars: ocr.recognized_chars,
        }
    }

    /// Record OCR text only after recognized regions survive filtering, deduplication, and merge.
    #[doc(hidden)]
    pub fn record_ocr_contribution(
        &self,
        regions: u64,
        characters: u64,
    ) -> Result<(), ConversionError> {
        if regions == 0 && characters == 0 {
            return Ok(());
        }
        if regions == 0 || characters == 0 {
            return Err(ConversionError::Internal {
                detail: "OCR contribution regions and characters must both be positive".into(),
            });
        }
        let mut ocr = lock_unpoisoned(&self.shared.resources.ocr);
        let recognized_regions = ocr.recognized_regions.checked_add(regions).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_archive_entries",
                detail: "OCR region telemetry overflow".into(),
            }
        })?;
        let recognized_chars = ocr.recognized_chars.checked_add(characters).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_field_bytes",
                detail: "OCR character telemetry overflow".into(),
            }
        })?;
        ocr.recognized_regions = recognized_regions;
        ocr.recognized_chars = recognized_chars;
        Ok(())
    }

    /// Return the immutable request resource envelope carried by this context.
    #[doc(hidden)]
    #[must_use]
    pub fn resource_limits(&self) -> &ResourceLimits {
        &self.shared.limits
    }

    /// Whether this context already spends from an enclosing preflight credit.
    #[doc(hidden)]
    #[must_use]
    pub fn has_memory_credit(&self) -> bool {
        self.memory_credit.is_some()
    }

    /// Borrow a same-context memory reservation as an SPI credit while
    /// cancellation, deadline, progress, and temporary storage remain
    /// request-scoped. The derived context cannot outlive or detach from the
    /// authentic reservation.
    ///
    /// # Errors
    ///
    /// Returns a stable internal error when the reservation is not a memory
    /// charge from this exact context.
    ///
    /// The mutable borrow prevents one parent permit from backing concurrent
    /// credits and statically prevents dropping the permit before its child:
    ///
    /// ```compile_fail
    /// # use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};
    /// let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    /// let mut permit = context.reserve_memory(1).unwrap();
    /// let first = context.with_memory_credit(&mut permit).unwrap();
    /// let second = context.with_memory_credit(&mut permit).unwrap();
    /// let _ = (first, second);
    /// ```
    ///
    /// ```compile_fail
    /// # use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};
    /// let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    /// let mut permit = context.reserve_memory(1).unwrap();
    /// let credit = context.with_memory_credit(&mut permit).unwrap();
    /// drop(permit);
    /// let _ = credit.reserve_memory(1);
    /// ```
    ///
    /// ```compile_fail
    /// # use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};
    /// let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    /// let forged = context.with_memory_credit(u64::MAX).unwrap();
    /// # let _ = forged;
    /// ```
    #[doc(hidden)]
    pub fn with_memory_credit<'a>(
        &self,
        reservation: &'a mut ResourceReservation,
    ) -> Result<PreflightMemoryCredit<'a>, ConversionError> {
        if !reservation.belongs_to_memory_context(self) {
            return Err(ConversionError::Internal {
                detail: "preflight credit requires an authentic same-context memory reservation"
                    .into(),
            });
        }
        let backing = reservation.global_memory_backing.as_ref().ok_or_else(|| {
            ConversionError::Internal {
                detail: "preflight credit requires a globally charged memory reservation".into(),
            }
        })?;
        if reservation.active_memory_credit.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(ConversionError::Internal {
                detail: "preflight reservation already backs a live memory credit".into(),
            });
        }
        let memory_credit =
            Arc::new(MemoryCredit { backing: Arc::clone(backing), bytes: AtomicU64::new(0) });
        reservation.active_memory_credit = Some(Arc::downgrade(&memory_credit));
        Ok(PreflightMemoryCredit {
            context: Self {
                shared: Arc::clone(&self.shared),
                memory_credit: Some(memory_credit),
                ignore_request_controls: self.ignore_request_controls,
            },
            _reservation: reservation,
        })
    }

    /// Reserve request-scoped temporary storage until the returned guard is dropped.
    ///
    /// This is intended for same-filesystem staging directories whose atomic
    /// publication semantics cannot use the temporary-file helper.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, or resource-limit error.
    pub fn reserve_temporary(&self, bytes: u64) -> Result<ResourceReservation, ConversionError> {
        self.reserve(ResourceKind::Temporary, bytes)
    }

    /// Current live temporary-storage reservations for boundary and lifecycle audits.
    #[doc(hidden)]
    #[must_use]
    pub fn reserved_temporary_bytes(&self) -> u64 {
        self.shared.resources.temporary_bytes.load(Ordering::Acquire)
    }

    /// Remaining request temporary storage available to a bounded streaming
    /// producer before it starts native work.
    #[doc(hidden)]
    #[must_use]
    pub fn available_temporary_bytes(&self) -> u64 {
        self.shared
            .limits
            .max_temporary_bytes
            .saturating_sub(self.shared.resources.temporary_bytes.load(Ordering::Acquire))
    }

    /// Create an automatically cleaned temporary file charged as bytes are written.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, resource-limit, or local I/O error.
    pub fn temporary_file(&self, prefix: &str) -> Result<TemporaryFile, ConversionError> {
        self.temporary_file_in(&self.shared.temporary_directory, prefix)
    }

    /// Directory used by request-scoped native helpers for private temporary children.
    #[doc(hidden)]
    #[must_use]
    pub fn temporary_directory(&self) -> &Path {
        &self.shared.temporary_directory
    }

    /// Create an automatically cleaned, accounted temporary file in `directory`.
    ///
    /// Callers use this form when a later atomic rename requires the stage file
    /// to reside on a specific filesystem.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, resource-limit, or local I/O error.
    pub fn temporary_file_in(
        &self,
        directory: impl AsRef<std::path::Path>,
        prefix: &str,
    ) -> Result<TemporaryFile, ConversionError> {
        self.checkpoint()?;
        let safe_prefix = prefix
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            .take(40)
            .collect::<String>();
        let safe_prefix = if safe_prefix.is_empty() { "into-md" } else { &safe_prefix };
        for attempt in 0_u32..128 {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
            let path = directory
                .as_ref()
                .join(format!("{safe_prefix}-{}-{nonce}-{attempt}.tmp", std::process::id()));
            match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(TemporaryFile {
                        path,
                        file: Some(file),
                        context: self.clone(),
                        charged: 0,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(ConversionError::Io { detail: "could not allocate a temporary file".into() })
    }

    /// Create an automatically cleaned, accounted temporary file at an exact path.
    ///
    /// The path is opened with create-new semantics. This form is intended for a
    /// caller which has already reserved and authenticated a private staging
    /// directory and needs every possible crash artifact to have a journaled name.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, already-existing-path, or local I/O error.
    pub fn temporary_file_at(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<TemporaryFile, ConversionError> {
        self.checkpoint()?;
        let path = path.as_ref().to_path_buf();
        let file =
            std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        Ok(TemporaryFile { path, file: Some(file), context: self.clone(), charged: 0 })
    }

    fn reserve(
        &self,
        kind: ResourceKind,
        bytes: u64,
    ) -> Result<ResourceReservation, ConversionError> {
        self.checkpoint()?;
        let (counter, peak, limit, name) = match kind {
            ResourceKind::Memory => self.memory_account(),
            ResourceKind::Temporary => (
                self.shared.resources.temporary_bytes.as_ref(),
                Some(self.shared.resources.temporary_peak_bytes.as_ref()),
                self.shared.limits.max_temporary_bytes,
                "max_temporary_bytes",
            ),
        };
        checked_charge(counter, peak, bytes, limit, name)?;
        let global_memory_backing =
            if matches!(kind, ResourceKind::Memory) && self.memory_credit.is_none() {
                Some(Arc::new(MemoryBacking {
                    shared: Arc::clone(&self.shared),
                    bytes: AtomicU64::new(bytes),
                }))
            } else {
                None
            };
        Ok(ResourceReservation {
            context: self.clone_with_memory_account(),
            kind,
            bytes,
            global_memory_backing,
            active_memory_credit: None,
        })
    }

    fn clone_with_memory_account(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            memory_credit: self.memory_credit.as_ref().map(Arc::clone),
            ignore_request_controls: self.ignore_request_controls,
        }
    }

    fn memory_account(&self) -> (&AtomicU64, Option<&AtomicU64>, u64, &'static str) {
        if let Some(credit) = &self.memory_credit {
            (&credit.bytes, None, credit.backing.bytes.load(Ordering::Acquire), "max_memory_bytes")
        } else {
            (
                &self.shared.resources.memory_bytes,
                Some(&self.shared.resources.memory_peak_bytes),
                self.shared.limits.max_memory_bytes,
                "max_memory_bytes",
            )
        }
    }
}

/// Scoped execution context backed by one authentic parent reservation.
#[doc(hidden)]
pub struct PreflightMemoryCredit<'a> {
    context: ExecutionContext,
    _reservation: &'a ResourceReservation,
}

impl std::ops::Deref for PreflightMemoryCredit<'_> {
    type Target = ExecutionContext;

    fn deref(&self) -> &Self::Target {
        &self.context
    }
}

/// Future wrapper that wakes and returns when cancellation or deadline wins.
pub struct CheckedFuture<'a, F> {
    context: &'a ExecutionContext,
    future: Pin<Box<F>>,
    waiter_id: u64,
    registered: bool,
}

impl<F: Future> Future for CheckedFuture<'_, F> {
    type Output = Result<F::Output, ConversionError>;

    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut waiters = lock_unpoisoned(&this.context.shared.cancellation.state.waiters);
        if let Err(error) = this.context.checkpoint() {
            waiters.remove(&this.waiter_id);
            this.registered = false;
            return Poll::Ready(Err(error));
        }
        waiters.insert(this.waiter_id, task.waker().clone());
        this.registered = true;
        drop(waiters);
        match this.future.as_mut().poll(task) {
            Poll::Ready(output) => {
                lock_unpoisoned(&this.context.shared.cancellation.state.waiters)
                    .remove(&this.waiter_id);
                this.registered = false;
                Poll::Ready(Ok(output))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<F> Drop for CheckedFuture<'_, F> {
    fn drop(&mut self) {
        if self.registered {
            lock_unpoisoned(&self.context.shared.cancellation.state.waiters)
                .remove(&self.waiter_id);
        }
    }
}

impl Drop for ExecutionShared {
    fn drop(&mut self) {
        *lock_unpoisoned(&self.timer_stop.stopped) = true;
        self.timer_stop.changed.notify_all();
    }
}

#[derive(Clone, Copy)]
enum ResourceKind {
    Memory,
    Temporary,
}

/// RAII resource-budget reservation.
pub struct ResourceReservation {
    context: ExecutionContext,
    kind: ResourceKind,
    bytes: u64,
    global_memory_backing: Option<Arc<MemoryBacking>>,
    active_memory_credit: Option<Weak<MemoryCredit>>,
}

impl fmt::Debug for ResourceReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceReservation")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl ResourceReservation {
    /// Increase this reservation using the same checked request budget.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, arithmetic-overflow, or resource-limit error.
    pub fn grow(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.ensure_credit_inactive()?;
        let next = self.bytes.checked_add(bytes).ok_or_else(|| ConversionError::ResourceLimit {
            limit: match self.kind {
                ResourceKind::Memory => "max_memory_bytes",
                ResourceKind::Temporary => "max_temporary_bytes",
            },
            detail: "resource reservation overflowed".into(),
        })?;
        let (counter, peak, limit, name) = match self.kind {
            ResourceKind::Memory => self.context.memory_account(),
            ResourceKind::Temporary => (
                self.context.shared.resources.temporary_bytes.as_ref(),
                Some(self.context.shared.resources.temporary_peak_bytes.as_ref()),
                self.context.shared.limits.max_temporary_bytes,
                "max_temporary_bytes",
            ),
        };
        checked_charge(counter, peak, bytes, limit, name)?;
        self.bytes = next;
        if let Some(backing) = &self.global_memory_backing {
            backing.bytes.fetch_add(bytes, Ordering::AcqRel);
        }
        Ok(())
    }

    /// Reduce this reservation after a conservatively charged allocation peak ends.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error if `bytes` exceeds the held reservation.
    pub fn shrink(&mut self, bytes: u64) -> Result<(), ConversionError> {
        self.ensure_credit_inactive()?;
        let next = self.bytes.checked_sub(bytes).ok_or_else(|| ConversionError::ResourceLimit {
            limit: match self.kind {
                ResourceKind::Memory => "max_memory_bytes",
                ResourceKind::Temporary => "max_temporary_bytes",
            },
            detail: "resource reservation underflowed".into(),
        })?;
        self.bytes = next;
        if let Some(backing) = &self.global_memory_backing {
            backing.bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
        let counter = match self.kind {
            ResourceKind::Memory => self.context.memory_account().0,
            ResourceKind::Temporary => &self.context.shared.resources.temporary_bytes,
        };
        counter.fetch_sub(bytes, Ordering::AcqRel);
        Ok(())
    }

    pub(crate) fn accounts_memory_for(&self, context: &ExecutionContext, bytes: u64) -> bool {
        self.belongs_to_memory_context(context) && self.bytes == bytes
    }

    pub(crate) fn belongs_to_memory_context(&self, context: &ExecutionContext) -> bool {
        matches!(self.kind, ResourceKind::Memory)
            && Arc::ptr_eq(&self.context.shared, &context.shared)
            && match (&self.context.memory_credit, &context.memory_credit) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    fn ensure_credit_inactive(&self) -> Result<(), ConversionError> {
        if self.active_memory_credit.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(ConversionError::Internal {
                detail: "cannot resize a preflight reservation while credited children are live"
                    .into(),
            });
        }
        Ok(())
    }
}

impl Drop for ResourceReservation {
    fn drop(&mut self) {
        if self.global_memory_backing.is_some() {
            // The backing owns the global charge. A derived credit also holds
            // the backing, so dropping this guard cannot release the request
            // budget until every detached child reservation is gone.
            return;
        }
        let counter = match self.kind {
            ResourceKind::Memory => self.context.memory_account().0,
            ResourceKind::Temporary => &self.context.shared.resources.temporary_bytes,
        };
        counter.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Automatically removed request temporary file.
pub struct TemporaryFile {
    path: PathBuf,
    file: Option<std::fs::File>,
    context: ExecutionContext,
    charged: u64,
}

impl TemporaryFile {
    /// Temporary path, intended only for passing to a scoped native component.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the authenticated temporary file handle for bounded downstream reads.
    #[doc(hidden)]
    pub fn as_file(&self) -> Result<&std::fs::File, ConversionError> {
        self.file
            .as_ref()
            .ok_or_else(|| ConversionError::Internal { detail: "temporary file is closed".into() })
    }

    /// Flush buffered file data.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, internal-state, or local I/O error.
    pub fn flush(&mut self) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.file
            .as_mut()
            .ok_or_else(|| ConversionError::Internal { detail: "temporary file is closed".into() })?
            .flush()
            .map_err(Into::into)
    }

    /// Flush file contents and metadata to stable storage.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, closed-file, or local I/O error.
    pub fn sync_all(&mut self) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.file
            .as_mut()
            .ok_or_else(|| ConversionError::Internal { detail: "temporary file is closed".into() })?
            .sync_all()
            .map_err(Into::into)
    }

    /// Close the file and transfer cleanup responsibility to the caller.
    #[must_use]
    pub fn persist(mut self) -> PathBuf {
        self.file.take();
        self.context.shared.resources.temporary_bytes.fetch_sub(self.charged, Ordering::AcqRel);
        self.charged = 0;
        std::mem::take(&mut self.path)
    }

    /// Write the complete buffer with stable cancellation and budget errors.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, resource-limit, internal-state, or I/O error.
    pub fn write_all_checked(&mut self, bytes: &[u8]) -> Result<(), ConversionError> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match self.write_checked(remaining) {
                Ok(0) => {
                    return Err(ConversionError::Io {
                        detail: "temporary file write made no progress".into(),
                    });
                }
                Ok(written) => remaining = &remaining[written..],
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn write_checked(&mut self, bytes: &[u8]) -> Result<usize, ConversionError> {
        self.context.checkpoint()?;
        let amount = u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_temporary_bytes",
            detail: "write size cannot be represented as u64".into(),
        })?;
        let next_charged =
            self.charged.checked_add(amount).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_temporary_bytes",
                detail: "temporary byte accounting overflowed".into(),
            })?;
        checked_charge(
            &self.context.shared.resources.temporary_bytes,
            Some(&self.context.shared.resources.temporary_peak_bytes),
            amount,
            self.context.shared.limits.max_temporary_bytes,
            "max_temporary_bytes",
        )?;
        let Some(file) = self.file.as_mut() else {
            self.context.shared.resources.temporary_bytes.fetch_sub(amount, Ordering::AcqRel);
            return Err(ConversionError::Internal { detail: "temporary file is closed".into() });
        };
        match file.write(bytes) {
            Ok(written) => {
                let written_u64 =
                    u64::try_from(written).map_err(|_| ConversionError::Internal {
                        detail: "temporary write result cannot be represented as u64".into(),
                    })?;
                self.charged = next_charged - (amount - written_u64);
                if written_u64 < amount {
                    self.context
                        .shared
                        .resources
                        .temporary_bytes
                        .fetch_sub(amount - written_u64, Ordering::AcqRel);
                }
                Ok(written)
            }
            Err(error) => {
                self.context.shared.resources.temporary_bytes.fetch_sub(amount, Ordering::AcqRel);
                Err(error.into())
            }
        }
    }
}

impl Write for TemporaryFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_checked(bytes).map_err(io::Error::other)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.context.checkpoint().map_err(io::Error::other)?;
        self.file.as_mut().ok_or_else(|| io::Error::other("temporary file is closed"))?.flush()
    }
}

impl Seek for TemporaryFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.context.checkpoint().map_err(io::Error::other)?;
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("temporary file is closed"))?
            .seek(position)
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        self.file.take();
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
        self.context.shared.resources.temporary_bytes.fetch_sub(self.charged, Ordering::AcqRel);
    }
}

struct ProgressDispatcher {
    mailbox: Arc<ProgressMailbox>,
    worker: Option<JoinHandle<()>>,
}

struct ProgressMailbox {
    inner: Mutex<ProgressMailboxInner>,
    ready: Condvar,
}

struct ProgressEnvelope {
    event: ProgressEvent,
}

struct ProgressMailboxInner {
    queue: VecDeque<ProgressEnvelope>,
    newest_sequence: u64,
    closed: bool,
}

impl ProgressDispatcher {
    fn new(listener: Arc<dyn ProgressListener>) -> Option<Self> {
        Self::new_with_hooks(listener, ProgressWorkerHooks::default())
    }

    fn new_with_hooks(
        listener: Arc<dyn ProgressListener>,
        hooks: ProgressWorkerHooks,
    ) -> Option<Self> {
        Self::new_with_spawner(listener, hooks, std::thread::Builder::spawn)
    }

    fn new_with_spawner<F>(
        listener: Arc<dyn ProgressListener>,
        hooks: ProgressWorkerHooks,
        spawn: F,
    ) -> Option<Self>
    where
        F: FnOnce(
            std::thread::Builder,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> io::Result<JoinHandle<()>>,
    {
        let mailbox = Arc::new(ProgressMailbox {
            inner: Mutex::new(ProgressMailboxInner {
                queue: VecDeque::with_capacity(8),
                newest_sequence: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        });
        let worker_mailbox = Arc::clone(&mailbox);
        let task = Box::new(move || {
            ProgressWorker { mailbox: worker_mailbox, listener, hooks }.run();
        });
        let worker =
            spawn(std::thread::Builder::new().name("into-markdown-progress".into()), task).ok()?;
        Some(Self { mailbox, worker: Some(worker) })
    }

    fn publish(&self, sequence: u64, event: ProgressEvent) {
        let mut inner = lock_unpoisoned(&self.mailbox.inner);
        if inner.closed || sequence <= inner.newest_sequence {
            return;
        }
        inner.newest_sequence = sequence;
        let terminal = event.stage == ExecutionStage::Completed;
        let envelope = ProgressEnvelope { event };
        if let Some(last) = inner.queue.back_mut()
            && last.event.stage == envelope.event.stage
        {
            *last = envelope;
        } else if inner.queue.len() < 8 {
            inner.queue.push_back(envelope);
        } else {
            // Preserve the oldest already queued boundaries and guarantee the
            // newest boundary, especially Completed, is eventually observed.
            inner.queue.pop_back();
            inner.queue.push_back(envelope);
        }
        inner.closed = terminal;
        drop(inner);
        self.mailbox.ready.notify_one();
    }
}

impl Drop for ProgressDispatcher {
    fn drop(&mut self) {
        lock_unpoisoned(&self.mailbox.inner).closed = true;
        self.mailbox.ready.notify_all();
        if let Some(worker) = self.worker.take() {
            retire_progress_worker(worker);
        }
    }
}

#[derive(Default)]
struct ProgressWorkerHooks {
    #[cfg(test)]
    start_gate: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    before_wait: Option<Sender<()>>,
}

struct ProgressWorker {
    mailbox: Arc<ProgressMailbox>,
    listener: Arc<dyn ProgressListener>,
    hooks: ProgressWorkerHooks,
}

impl ProgressWorker {
    fn run(self) {
        let Self { mailbox, listener, hooks } = self;
        #[cfg(test)]
        let ProgressWorkerHooks { start_gate, before_wait } = hooks;
        #[cfg(test)]
        if let Some(start_gate) = start_gate {
            start_gate.wait();
        }
        #[cfg(not(test))]
        let _ = hooks;
        #[cfg(test)]
        let mut before_wait = before_wait;
        loop {
            let event = {
                let mut inner = lock_unpoisoned(&mailbox.inner);
                while inner.queue.is_empty() && !inner.closed {
                    #[cfg(test)]
                    if let Some(sender) = before_wait.take() {
                        let _ = sender.send(());
                    }
                    inner = wait_unpoisoned(&mailbox.ready, inner);
                }
                inner.queue.pop_front()
            };
            let Some(envelope) = event else { break };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                listener.on_progress(envelope.event);
            }));
        }
    }
}

fn retire_progress_worker(worker: JoinHandle<()>) {
    let Some(joiner) = progress_joiner() else { return };
    try_retire_progress_worker(joiner, worker);
}

fn try_retire_progress_worker(joiner: &SyncSender<JoinHandle<()>>, worker: JoinHandle<()>) {
    match joiner.try_send(worker) {
        Ok(()) => {}
        Err(TrySendError::Full(worker) | TrySendError::Disconnected(worker)) => {
            // Dropping the handle detaches the already-closing worker.
            drop(worker);
        }
    }
}

fn progress_joiner() -> Option<&'static SyncSender<JoinHandle<()>>> {
    static JOINER: std::sync::OnceLock<Option<SyncSender<JoinHandle<()>>>> =
        std::sync::OnceLock::new();
    JOINER.get_or_init(|| start_progress_joiner(std::thread::Builder::spawn)).as_ref()
}

fn start_progress_joiner<F>(spawn: F) -> Option<SyncSender<JoinHandle<()>>>
where
    F: FnOnce(
        std::thread::Builder,
        Box<dyn FnOnce() + Send + 'static>,
    ) -> io::Result<JoinHandle<()>>,
{
    const MAX_PENDING_JOINS: usize = 64;
    let (sender, receiver) = mpsc::sync_channel::<JoinHandle<()>>(MAX_PENDING_JOINS);
    let task = Box::new(move || {
        let mut workers = Vec::<JoinHandle<()>>::new();
        loop {
            match if workers.is_empty() {
                receiver.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            } else {
                receiver.recv_timeout(Duration::from_millis(10))
            } {
                Ok(worker) if workers.len() < MAX_PENDING_JOINS => workers.push(worker),
                Ok(worker) => drop(worker),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let mut index = 0;
            while index < workers.len() {
                if workers[index].is_finished() {
                    let worker = workers.swap_remove(index);
                    let _ = worker.join();
                } else {
                    index += 1;
                }
            }
        }
    });
    spawn(std::thread::Builder::new().name("into-markdown-progress-joiner".into()), task)
        .ok()
        .map(|_| sender)
}

fn checked_charge(
    counter: &AtomicU64,
    peak: Option<&AtomicU64>,
    amount: u64,
    limit: u64,
    name: &'static str,
) -> Result<(), ConversionError> {
    let result = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_add(amount).filter(|next| *next <= limit)
    });
    result
        .map(|previous| {
            if let Some(peak) = peak {
                let current = previous
                    .checked_add(amount)
                    .expect("a successful checked charge cannot overflow");
                peak.fetch_max(current, Ordering::AcqRel);
            }
        })
        .map_err(|current| ConversionError::ResourceLimit {
            limit: name,
            detail: format!("{current} + {amount} exceeds {limit}"),
        })
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_unpoisoned<'a, T>(condition: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condition.wait(guard).unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    struct Never;

    impl Future for Never {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    #[derive(Default)]
    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn poll_once<F: Future>(future: Pin<&mut F>, wake: &Arc<CountingWake>) -> Poll<F::Output> {
        let waker = Waker::from(Arc::clone(wake));
        future.poll(&mut Context::from_waker(&waker))
    }

    #[test]
    fn cancellation_wakes_every_pending_waiter_without_lost_wakeup() {
        let cancellation = CancellationToken::new();
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let wake_a = Arc::new(CountingWake::default());
        let wake_b = Arc::new(CountingWake::default());
        let mut future_a = Box::pin(context.run(Never));
        let mut future_b = Box::pin(context.run(Never));
        assert!(poll_once(future_a.as_mut(), &wake_a).is_pending());
        assert!(poll_once(future_b.as_mut(), &wake_b).is_pending());
        cancellation.cancel();
        assert_eq!(wake_a.0.load(Ordering::Acquire), 1);
        assert_eq!(wake_b.0.load(Ordering::Acquire), 1);
        assert!(matches!(
            poll_once(future_a.as_mut(), &wake_a),
            Poll::Ready(Err(ConversionError::Cancelled))
        ));
        assert!(matches!(
            poll_once(future_b.as_mut(), &wake_b),
            Poll::Ready(Err(ConversionError::Cancelled))
        ));

        let mut late = Box::pin(context.run(Never));
        assert!(matches!(
            poll_once(late.as_mut(), &wake_a),
            Poll::Ready(Err(ConversionError::Cancelled))
        ));
    }

    #[test]
    fn deadline_wakes_a_pending_runtime_independent_future() {
        let context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_millis(20)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let wake = Arc::new(CountingWake::default());
        let mut future = Box::pin(context.run(Never));
        assert!(poll_once(future.as_mut(), &wake).is_pending());
        let limit = Instant::now() + Duration::from_secs(2);
        while wake.0.load(Ordering::Acquire) == 0 && Instant::now() < limit {
            std::thread::yield_now();
        }
        assert!(wake.0.load(Ordering::Acquire) > 0);
        assert!(matches!(
            poll_once(future.as_mut(), &wake),
            Poll::Ready(Err(ConversionError::Timeout))
        ));
    }

    #[test]
    fn deadline_timer_spawn_failure_is_stable_for_clones_and_waiters() {
        let cancellation = CancellationToken::new();
        let context = ExecutionContext::new_with_timer_spawner(
            ExecutionOptions {
                cancellation: cancellation.clone(),
                timeout: Some(Duration::from_hours(1)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
            std::env::temp_dir(),
            |_, _| Err(io::Error::other("injected deadline timer spawn failure")),
        );
        let clone = context.clone();
        cancellation.cancel();

        for candidate in [&context, &clone] {
            let error = candidate.checkpoint().unwrap_err();
            assert_eq!(error.code(), crate::ErrorCode::ComponentUnavailable);
            match error {
                ConversionError::ComponentUnavailable { component, detail } => {
                    assert_eq!(component, "deadline-timer");
                    assert_eq!(detail, "deadline timer could not be started");
                }
                other => panic!("unexpected timer startup error: {other:?}"),
            }

            let wake = Arc::new(CountingWake::default());
            let mut future = Box::pin(candidate.run(Never));
            assert!(matches!(
                poll_once(future.as_mut(), &wake),
                Poll::Ready(Err(ConversionError::ComponentUnavailable { ref component, .. }))
                    if component == "deadline-timer"
            ));
        }
        assert!(lock_unpoisoned(&context.shared.cancellation.state.waiters).is_empty());
        assert_eq!(Arc::strong_count(&context.shared.timer_stop), 1);
    }

    #[test]
    fn immediate_and_unrepresentable_deadlines_do_not_need_a_timer_thread() {
        for timeout in [Duration::ZERO, Duration::MAX] {
            let spawn_calls = AtomicUsize::new(0);
            let context = ExecutionContext::new_with_timer_spawner(
                ExecutionOptions { timeout: Some(timeout), ..ExecutionOptions::default() },
                ResourceLimits::default(),
                std::env::temp_dir(),
                |_, _| {
                    spawn_calls.fetch_add(1, Ordering::AcqRel);
                    Err(io::Error::other("timer must not be spawned"))
                },
            );
            assert_eq!(spawn_calls.load(Ordering::Acquire), 0);
            if timeout.is_zero() {
                assert!(matches!(context.checkpoint(), Err(ConversionError::Timeout)));
            } else {
                assert!(context.checkpoint().is_ok());
            }
        }
    }

    #[test]
    fn timer_spawn_failure_releases_listener_when_context_is_dropped() {
        let listener = Arc::new(EmptyListener);
        let weak = Arc::downgrade(&listener);
        let context = ExecutionContext::new_with_timer_spawner(
            ExecutionOptions {
                timeout: Some(Duration::from_hours(1)),
                progress_listener: Some(listener.clone()),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
            std::env::temp_dir(),
            |_, _| Err(io::Error::other("injected deadline timer spawn failure")),
        );
        drop(listener);
        drop(context);
        wait_until_released(&weak);
    }

    #[test]
    fn checked_budget_arithmetic_and_raii_release_are_stable() {
        let limits = ResourceLimits { max_memory_bytes: 10, ..ResourceLimits::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let first = context.reserve_memory(6).unwrap();
        let error = context.reserve_memory(u64::MAX).err().unwrap();
        assert_eq!(error.code(), crate::ErrorCode::ResourceLimit);
        drop(first);
        assert!(context.reserve_memory(10).is_ok());
    }

    #[test]
    fn temporary_budget_failure_leaves_no_artifact() {
        let limits = ResourceLimits { max_temporary_bytes: 3, ..ResourceLimits::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let mut temporary = context.temporary_file("cleanup-test").unwrap();
        let path = temporary.path().to_path_buf();
        let error = temporary.write_all_checked(b"four").unwrap_err();
        assert_eq!(error.code(), crate::ErrorCode::ResourceLimit);
        drop(temporary);
        assert!(!path.exists());
    }

    #[test]
    fn exact_temporary_path_is_create_new_accounted_and_scoped() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let seed = context.temporary_file("exact-path-test").unwrap();
        let path = seed.path().to_path_buf();
        drop(seed);

        let mut temporary = context.temporary_file_at(&path).unwrap();
        assert!(context.temporary_file_at(&path).is_err());
        temporary.write_all_checked(b"accounted").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"accounted");
        drop(temporary);
        assert!(!path.exists());
    }

    struct RecordingListener {
        events: Arc<Mutex<Vec<ProgressEvent>>>,
        delay: Duration,
        panic_once: AtomicBool,
    }

    impl ProgressListener for RecordingListener {
        fn on_progress(&self, event: ProgressEvent) {
            assert!(
                !self.panic_once.swap(false, Ordering::AcqRel),
                "listener failure must be isolated"
            );
            std::thread::sleep(self.delay);
            lock_unpoisoned(&self.events).push(event);
        }
    }

    fn wait_until_released<T: ?Sized>(weak: &std::sync::Weak<T>) {
        let limit = Instant::now() + Duration::from_secs(2);
        while weak.strong_count() != 0 && Instant::now() < limit {
            std::thread::yield_now();
        }
        assert_eq!(weak.strong_count(), 0, "listener was not released before the deadline");
    }

    struct EmptyListener;

    impl ProgressListener for EmptyListener {
        fn on_progress(&self, _: ProgressEvent) {}
    }

    #[test]
    fn close_before_worker_waits_drains_the_queue_and_releases_listener() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = Arc::new(RecordingListener {
            events: Arc::clone(&events),
            delay: Duration::ZERO,
            panic_once: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&listener);
        let start_gate = Arc::new(std::sync::Barrier::new(2));
        let dispatcher = ProgressDispatcher::new_with_hooks(
            listener,
            ProgressWorkerHooks { start_gate: Some(Arc::clone(&start_gate)), before_wait: None },
        )
        .unwrap();
        dispatcher.publish(
            1,
            ProgressEvent {
                stage: ExecutionStage::Resolving,
                basis_points: 0,
                completed_units: None,
                total_units: None,
                message: None,
            },
        );
        drop(dispatcher);
        start_gate.wait();
        wait_until_released(&weak);
        assert_eq!(lock_unpoisoned(&events).len(), 1);
    }

    #[test]
    fn close_after_worker_begins_waiting_releases_listener() {
        let listener = Arc::new(EmptyListener);
        let weak = Arc::downgrade(&listener);
        let (before_wait, waiting) = mpsc::channel();
        let dispatcher = ProgressDispatcher::new_with_hooks(
            listener,
            ProgressWorkerHooks { start_gate: None, before_wait: Some(before_wait) },
        )
        .unwrap();
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(dispatcher);
        wait_until_released(&weak);
    }

    #[test]
    fn dropping_context_does_not_wait_for_a_slow_listener() {
        struct BlockingListener {
            entered: Sender<()>,
            release: Arc<(Mutex<bool>, Condvar)>,
        }

        impl ProgressListener for BlockingListener {
            fn on_progress(&self, _: ProgressEvent) {
                let _ = self.entered.send(());
                let (released, changed) = &*self.release;
                let mut released = lock_unpoisoned(released);
                while !*released {
                    released = wait_unpoisoned(changed, released);
                }
            }
        }

        let (entered, callback_entered) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let listener = Arc::new(BlockingListener { entered, release: Arc::clone(&release) });
        let weak = Arc::downgrade(&listener);
        let context = ExecutionContext::new(
            ExecutionOptions {
                progress_listener: Some(listener.clone()),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        context.report(ExecutionStage::Resolving, None, None, None::<String>).unwrap();
        callback_entered.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(listener);
        let start = Instant::now();
        drop(context);
        assert!(start.elapsed() < Duration::from_millis(250));
        let (released, changed) = &*release;
        *lock_unpoisoned(released) = true;
        changed.notify_all();
        wait_until_released(&weak);
    }

    #[test]
    fn listener_can_drop_the_last_context_without_self_joining() {
        struct ContextDroppingListener {
            context: Arc<Mutex<Option<ExecutionContext>>>,
            dropped: Sender<()>,
        }

        impl ProgressListener for ContextDroppingListener {
            fn on_progress(&self, _: ProgressEvent) {
                drop(lock_unpoisoned(&self.context).take());
                let _ = self.dropped.send(());
            }
        }

        let held_context = Arc::new(Mutex::new(None));
        let (dropped, callback_dropped) = mpsc::channel();
        let listener =
            Arc::new(ContextDroppingListener { context: Arc::clone(&held_context), dropped });
        let weak = Arc::downgrade(&listener);
        let context = ExecutionContext::new(
            ExecutionOptions {
                progress_listener: Some(listener.clone()),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        *lock_unpoisoned(&held_context) = Some(context.clone());
        context.report(ExecutionStage::Completed, Some(1), Some(1), None::<String>).unwrap();
        drop(context);
        drop(listener);
        callback_dropped.recv_timeout(Duration::from_secs(2)).unwrap();
        wait_until_released(&weak);
    }

    #[test]
    fn dispatcher_thread_spawn_failure_disables_and_releases_listener() {
        let listener = Arc::new(EmptyListener);
        let weak = Arc::downgrade(&listener);
        let dispatcher = ProgressDispatcher::new_with_spawner(
            listener,
            ProgressWorkerHooks::default(),
            |_, _| Err(io::Error::other("injected progress worker spawn failure")),
        );
        assert!(dispatcher.is_none());
        wait_until_released(&weak);
    }

    #[test]
    fn joiner_thread_spawn_failure_is_a_stable_detach_fallback() {
        let joiner = start_progress_joiner(|_, _| {
            Err(io::Error::other("injected progress joiner spawn failure"))
        });
        assert!(joiner.is_none());
    }

    #[test]
    fn saturated_join_queue_detaches_without_blocking_worker_cleanup() {
        let (joiner, pending) = mpsc::sync_channel(1);
        let queued_worker = std::thread::spawn(|| {});
        joiner.try_send(queued_worker).unwrap();

        let captured = Arc::new(());
        let weak = Arc::downgrade(&captured);
        let release = Arc::new(std::sync::Barrier::new(2));
        let worker_release = Arc::clone(&release);
        let detached_worker = std::thread::spawn(move || {
            worker_release.wait();
            drop(captured);
        });
        let start = Instant::now();
        try_retire_progress_worker(&joiner, detached_worker);
        assert!(start.elapsed() < Duration::from_millis(250));
        release.wait();
        wait_until_released(&weak);

        pending.recv_timeout(Duration::from_secs(2)).unwrap().join().unwrap();
    }

    #[test]
    fn slow_and_panicking_listener_cannot_block_or_break_progress() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = Arc::new(RecordingListener {
            events: Arc::clone(&events),
            delay: Duration::from_millis(10),
            panic_once: AtomicBool::new(true),
        });
        let context = ExecutionContext::new(
            ExecutionOptions { progress_listener: Some(listener), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let start = Instant::now();
        for stage in [
            ExecutionStage::Resolving,
            ExecutionStage::Detecting,
            ExecutionStage::Probing,
            ExecutionStage::Converting,
            ExecutionStage::Rendering,
            ExecutionStage::Completed,
        ] {
            context.report(stage, None, None, None::<String>).unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(10));
        let limit = Instant::now() + Duration::from_secs(2);
        while lock_unpoisoned(&events).last().map(|event| event.stage)
            != Some(ExecutionStage::Completed)
            && Instant::now() < limit
        {
            std::thread::yield_now();
        }
        let recorded = lock_unpoisoned(&events);
        assert_eq!(recorded.last().map(|event| event.stage), Some(ExecutionStage::Completed));
        assert!(recorded.windows(2).all(|pair| pair[0].basis_points < pair[1].basis_points));
    }

    #[test]
    fn extreme_progress_ratio_and_provider_interleaving_remain_monotonic() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = Arc::new(RecordingListener {
            events: Arc::clone(&events),
            delay: Duration::ZERO,
            panic_once: AtomicBool::new(false),
        });
        let context = ExecutionContext::new(
            ExecutionOptions { progress_listener: Some(listener), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        context
            .report(ExecutionStage::Resolving, Some(u64::MAX), Some(u64::MAX), None::<String>)
            .unwrap();
        for stage in [
            ExecutionStage::Converting,
            ExecutionStage::Ai,
            ExecutionStage::Ocr,
            ExecutionStage::Ai,
            ExecutionStage::Rendering,
            ExecutionStage::Completed,
        ] {
            context.report(stage, None, None, None::<String>).unwrap();
        }
        let limit = Instant::now() + Duration::from_secs(2);
        while lock_unpoisoned(&events).last().map(|event| event.stage)
            != Some(ExecutionStage::Completed)
            && Instant::now() < limit
        {
            std::thread::yield_now();
        }
        let recorded = lock_unpoisoned(&events);
        assert_eq!(recorded[0].basis_points, 999);
        assert!(recorded.iter().any(|event| event.stage == ExecutionStage::Ocr));
        assert!(recorded.windows(2).all(|pair| pair[0].basis_points <= pair[1].basis_points));
    }

    #[test]
    fn unknown_total_progress_publishes_new_completed_units_at_the_same_fraction() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = Arc::new(RecordingListener {
            events: Arc::clone(&events),
            delay: Duration::ZERO,
            panic_once: AtomicBool::new(false),
        });
        let context = ExecutionContext::new(
            ExecutionOptions { progress_listener: Some(listener), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        context.report(ExecutionStage::Ai, Some(16_000), None, Some("asr.normalize")).unwrap();
        context.report(ExecutionStage::Ai, Some(32_000), None, Some("asr.normalize")).unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while lock_unpoisoned(&events).last().and_then(|event| event.completed_units)
            != Some(32_000)
            && Instant::now() < limit
        {
            std::thread::yield_now();
        }
        let recorded = lock_unpoisoned(&events);
        assert_eq!(recorded.last().and_then(|event| event.completed_units), Some(32_000));
        assert!(recorded.windows(2).all(|pair| pair[0].basis_points <= pair[1].basis_points));
    }

    #[test]
    fn saturated_progress_mailbox_still_delivers_completed() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = Arc::new(RecordingListener {
            events: Arc::clone(&events),
            delay: Duration::ZERO,
            panic_once: AtomicBool::new(false),
        });
        let start_gate = Arc::new(std::sync::Barrier::new(2));
        let dispatcher = ProgressDispatcher::new_with_hooks(
            listener,
            ProgressWorkerHooks { start_gate: Some(Arc::clone(&start_gate)), before_wait: None },
        )
        .unwrap();
        let stages = [
            ExecutionStage::Resolving,
            ExecutionStage::Detecting,
            ExecutionStage::Probing,
            ExecutionStage::Converting,
            ExecutionStage::Ai,
            ExecutionStage::Ocr,
            ExecutionStage::Rendering,
        ];
        for sequence in 0_u16..32 {
            dispatcher.publish(
                u64::from(sequence) + 1,
                ProgressEvent {
                    stage: stages[usize::from(sequence) % stages.len()],
                    basis_points: sequence,
                    completed_units: None,
                    total_units: None,
                    message: None,
                },
            );
        }
        assert_eq!(lock_unpoisoned(&dispatcher.mailbox.inner).queue.len(), 8);
        dispatcher.publish(
            33,
            ProgressEvent {
                stage: ExecutionStage::Completed,
                basis_points: 10_000,
                completed_units: Some(1),
                total_units: Some(1),
                message: None,
            },
        );
        dispatcher.publish(
            34,
            ProgressEvent {
                stage: ExecutionStage::Resolving,
                basis_points: 10_000,
                completed_units: None,
                total_units: None,
                message: Some("must be rejected after Completed".into()),
            },
        );
        {
            let inner = lock_unpoisoned(&dispatcher.mailbox.inner);
            assert_eq!(inner.queue.len(), 8);
            assert!(inner.closed);
            assert_eq!(inner.newest_sequence, 33);
            assert_eq!(
                inner.queue.back().map(|item| item.event.stage),
                Some(ExecutionStage::Completed)
            );
        }
        start_gate.wait();
        let limit = Instant::now() + Duration::from_secs(2);
        while lock_unpoisoned(&events).last().map(|event| event.stage)
            != Some(ExecutionStage::Completed)
            && Instant::now() < limit
        {
            std::thread::yield_now();
        }
        assert_eq!(
            lock_unpoisoned(&events).last().map(|event| event.stage),
            Some(ExecutionStage::Completed)
        );
    }

    #[test]
    fn terminal_progress_rejects_a_stale_concurrent_publication() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = Arc::new(RecordingListener {
            events: Arc::clone(&events),
            delay: Duration::ZERO,
            panic_once: AtomicBool::new(false),
        });
        let dispatcher = Arc::new(ProgressDispatcher::new(listener).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let stale_dispatcher = Arc::clone(&dispatcher);
        let stale_barrier = Arc::clone(&barrier);
        let stale = std::thread::spawn(move || {
            stale_barrier.wait();
            std::thread::yield_now();
            stale_dispatcher.publish(
                1,
                ProgressEvent {
                    stage: ExecutionStage::Resolving,
                    basis_points: 0,
                    completed_units: None,
                    total_units: None,
                    message: None,
                },
            );
        });
        dispatcher.publish(
            2,
            ProgressEvent {
                stage: ExecutionStage::Completed,
                basis_points: 10_000,
                completed_units: Some(1),
                total_units: Some(1),
                message: None,
            },
        );
        barrier.wait();
        stale.join().unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while lock_unpoisoned(&events).is_empty() && Instant::now() < limit {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(10));
        let recorded = lock_unpoisoned(&events);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].stage, ExecutionStage::Completed);
    }

    #[test]
    fn dropping_context_stops_long_deadline_and_releases_listener() {
        struct Listener;
        impl ProgressListener for Listener {
            fn on_progress(&self, _: ProgressEvent) {}
        }
        let listener: Arc<dyn ProgressListener> = Arc::new(Listener);
        let weak = Arc::downgrade(&listener);
        let context = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(Duration::from_hours(1)),
                progress_listener: Some(Arc::clone(&listener)),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        drop(listener);
        drop(context);
        let limit = Instant::now() + Duration::from_secs(2);
        while weak.upgrade().is_some() && Instant::now() < limit {
            std::thread::yield_now();
        }
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn unrepresentable_deadline_saturates_to_no_deadline() {
        let context = ExecutionContext::new(
            ExecutionOptions { timeout: Some(Duration::MAX), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        assert!(context.shared.deadline.is_none());
        assert!(context.checkpoint().is_ok());
    }

    #[test]
    fn zero_library_timeout_is_an_immediate_deadline() {
        let context = ExecutionContext::new(
            ExecutionOptions { timeout: Some(Duration::ZERO), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        assert!(matches!(context.checkpoint(), Err(ConversionError::Timeout)));
    }

    #[test]
    fn cleanup_scope_bypasses_terminal_controls_but_shares_exact_budgets() {
        let cancellation = CancellationToken::new();
        let context = ExecutionContext::new(
            ExecutionOptions {
                cancellation: cancellation.clone(),
                timeout: Some(Duration::ZERO),
                ..ExecutionOptions::default()
            },
            ResourceLimits {
                max_memory_bytes: 8,
                max_temporary_bytes: 8,
                ..ResourceLimits::default()
            },
        );
        cancellation.cancel();
        assert!(matches!(context.checkpoint(), Err(ConversionError::Cancelled)));

        let cleanup = context.cleanup_scope();
        assert!(cleanup.checkpoint().is_ok());
        let memory = cleanup.reserve_memory(8).unwrap();
        let temporary = cleanup.reserve_temporary(8).unwrap();
        assert_eq!(context.reserved_memory_bytes(), 8);
        assert_eq!(context.reserved_temporary_bytes(), 8);
        assert!(cleanup.reserve_memory(1).is_err());
        assert!(cleanup.reserve_temporary(1).is_err());
        drop((memory, temporary));
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert_eq!(context.reserved_temporary_bytes(), 0);
    }

    #[test]
    fn preflight_credit_is_bounded_without_double_charging_parent() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 100, ..ResourceLimits::default() },
        );
        let mut parent = context.reserve_memory(80).unwrap();
        let credit = context.with_memory_credit(&mut parent).unwrap();
        let inner = credit.reserve_memory(79).unwrap();
        assert_eq!(context.reserved_memory_bytes(), 80);
        assert_eq!(credit.reserved_memory_bytes(), 79);
        assert!(credit.reserve_memory(2).is_err());
        assert!(!inner.belongs_to_memory_context(&context));
        drop(inner);
        assert_eq!(credit.reserved_memory_bytes(), 0);
        drop(credit);
        drop(parent);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn cloned_credit_context_cannot_escape_or_mint_more_credit() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() },
        );
        let mut parent = context.reserve_memory(1).unwrap();
        let credit = context.with_memory_credit(&mut parent).unwrap();
        let detached = ExecutionContext::clone(&credit);

        assert_eq!(detached.available_memory_bytes(), 0);
        assert!(detached.reserve_memory(1).is_err());
        let child = credit.reserve_memory(1).unwrap();
        assert!(credit.reserve_memory(1).is_err());
        drop(child);
        assert_eq!(credit.available_memory_bytes(), 1);
    }

    #[test]
    fn detached_children_keep_global_backing_until_the_last_drop() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 2, ..ResourceLimits::default() },
        );
        let mut parent = context.reserve_memory(2).unwrap();
        let (first, second) = {
            let credit = context.with_memory_credit(&mut parent).unwrap();
            (credit.reserve_memory(1).unwrap(), credit.reserve_memory(1).unwrap())
        };

        assert!(context.with_memory_credit(&mut parent).is_err());
        assert!(parent.shrink(1).is_err());
        drop(parent);
        assert_eq!(context.reserved_memory_bytes(), 2);
        assert!(context.reserve_memory(1).is_err());
        drop(first);
        assert_eq!(context.reserved_memory_bytes(), 2);
        assert!(context.reserve_memory(1).is_err());
        drop(second);
        assert_eq!(context.reserved_memory_bytes(), 0);
        assert!(context.reserve_memory(2).is_ok());
    }

    #[test]
    fn credited_child_unwind_releases_backing_exactly_once() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() },
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut parent = context.reserve_memory(1).unwrap();
            let child = {
                let credit = context.with_memory_credit(&mut parent).unwrap();
                credit.reserve_memory(1).unwrap()
            };
            drop(parent);
            assert_eq!(context.reserved_memory_bytes(), 1);
            let _child = child;
            panic!("exercise unwind");
        }));
        assert!(result.is_err());
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn converter_output_child_keeps_parent_charge_across_handoff() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 4_096, ..ResourceLimits::default() },
        );
        let mut parent = context.reserve_memory(4_096).unwrap();
        let child = {
            let credit = context.with_memory_credit(&mut parent).unwrap();
            credit.reserve_memory(1_024).unwrap()
        };
        let output = crate::ConverterOutput::new_with_memory_reservations(
            crate::Document::default(),
            Vec::new(),
            Vec::new(),
            vec![child],
        );
        drop(parent);
        assert_eq!(context.reserved_memory_bytes(), 4_096);
        assert!(context.reserve_memory(1).is_err());
        drop(output);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn memory_credit_rejects_cross_context_parent() {
        let first = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let second = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let mut permit = first.reserve_memory(1).unwrap();
        assert!(second.with_memory_credit(&mut permit).is_err());
    }

    #[test]
    fn forked_requests_share_the_resource_ceiling() {
        let pool = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits {
                max_memory_bytes: 10,
                max_temporary_bytes: 10,
                ..ResourceLimits::default()
            },
        );
        let first = pool.fork_with_shared_resources(ExecutionOptions::default());
        let second = pool.fork_with_shared_resources(ExecutionOptions::default());
        let memory = first.reserve_memory(6).unwrap();
        let temporary = second.reserve_temporary(6).unwrap();

        assert!(second.reserve_memory(5).is_err());
        assert!(first.reserve_temporary(5).is_err());
        drop(memory);
        drop(temporary);
        assert!(second.reserve_memory(10).is_ok());
        assert!(first.reserve_temporary(10).is_ok());
    }

    #[test]
    fn concurrent_forks_publish_one_stable_historical_peak() {
        let pool = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 10, ..ResourceLimits::default() },
        );
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let (ready, observed) = std::sync::mpsc::channel();
        let mut workers = Vec::new();
        for amount in [4, 6] {
            let context = pool.fork_with_shared_resources(ExecutionOptions::default());
            let barrier = Arc::clone(&barrier);
            let ready = ready.clone();
            workers.push(std::thread::spawn(move || {
                let reservation = context.reserve_memory(amount).unwrap();
                ready.send(()).unwrap();
                barrier.wait();
                drop(reservation);
            }));
        }
        observed.recv().unwrap();
        observed.recv().unwrap();

        assert_eq!(pool.reserved_memory_bytes(), 10);
        assert_eq!(pool.resource_usage().shared_lease_peak_bytes, 10);
        assert!(pool.reserve_memory(1).is_err());
        assert_eq!(pool.resource_usage().shared_lease_peak_bytes, 10);
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(pool.reserved_memory_bytes(), 0);
        assert_eq!(pool.resource_usage().shared_lease_peak_bytes, 10);
    }

    #[test]
    fn grow_shrink_drop_temporary_and_preflight_credit_preserve_true_peaks() {
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits {
                max_memory_bytes: 12,
                max_temporary_bytes: 12,
                ..ResourceLimits::default()
            },
        );
        let mut memory = context.reserve_memory(3).unwrap();
        memory.grow(4).unwrap();
        memory.shrink(5).unwrap();
        assert_eq!(context.resource_usage().shared_lease_peak_bytes, 7);
        drop(memory);
        assert_eq!(context.reserved_memory_bytes(), 0);

        let mut parent = context.reserve_memory(10).unwrap();
        {
            let credit = context.with_memory_credit(&mut parent).unwrap();
            let mut child = credit.reserve_memory(4).unwrap();
            child.grow(6).unwrap();
            assert_eq!(credit.reserved_memory_bytes(), 10);
            assert_eq!(context.resource_usage().shared_lease_peak_bytes, 10);
        }
        drop(parent);
        assert_eq!(context.reserved_memory_bytes(), 0);

        let mut temporary = context.reserve_temporary(3).unwrap();
        temporary.grow(2).unwrap();
        temporary.shrink(4).unwrap();
        drop(temporary);
        let mut file = context.temporary_file("usage-peak").unwrap();
        file.write_all_checked(b"1234567").unwrap();
        drop(file);
        let usage = context.resource_usage();
        assert_eq!(context.reserved_temporary_bytes(), 0);
        assert_eq!(usage.temporary_lease_peak_bytes, 7);
        assert_eq!(usage.temporary_lease_budget_bytes, 12);
    }

    #[test]
    fn rejected_cancelled_and_timed_out_reserves_do_not_raise_peak() {
        let limits = ResourceLimits { max_memory_bytes: 5, ..ResourceLimits::default() };
        let ordinary = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        assert!(ordinary.reserve_memory(6).is_err());
        assert_eq!(ordinary.resource_usage().shared_lease_peak_bytes, 0);

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            limits.clone(),
        );
        assert!(matches!(cancelled.reserve_memory(1), Err(ConversionError::Cancelled)));
        assert_eq!(cancelled.resource_usage().shared_lease_peak_bytes, 0);

        let timed_out = ExecutionContext::new(
            ExecutionOptions { timeout: Some(Duration::ZERO), ..ExecutionOptions::default() },
            limits,
        );
        assert!(matches!(timed_out.reserve_memory(1), Err(ConversionError::Timeout)));
        assert_eq!(timed_out.resource_usage().shared_lease_peak_bytes, 0);
    }

    #[test]
    fn ocr_contributions_aggregate_across_forks_and_zeroes_are_not_hits() {
        let pool = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let fork = pool.fork_with_shared_resources(ExecutionOptions::default());
        pool.record_ocr_contribution(0, 0).unwrap();
        assert!(matches!(
            pool.record_ocr_contribution(1, 0),
            Err(ConversionError::Internal { .. })
        ));
        pool.record_ocr_contribution(1, 2).unwrap();
        fork.record_ocr_contribution(2, 5).unwrap();
        let usage = pool.resource_usage();
        assert_eq!(usage.ocr_recognized_regions, 3);
        assert_eq!(usage.ocr_recognized_chars, 7);
    }
}
