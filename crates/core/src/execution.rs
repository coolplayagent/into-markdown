use crate::{ConversionError, ResourceLimits};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
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
#[derive(Clone)]
pub struct ExecutionContext {
    shared: Arc<ExecutionShared>,
}

struct ExecutionShared {
    cancellation: CancellationToken,
    deadline: Option<Instant>,
    timed_out: AtomicBool,
    timer_stop: Arc<TimerStop>,
    progress: Mutex<ProgressState>,
    dispatcher: Option<ProgressDispatcher>,
    limits: ResourceLimits,
    memory_bytes: AtomicU64,
    temporary_bytes: AtomicU64,
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
        let deadline = options.timeout.map(|duration| {
            let now = Instant::now();
            now.checked_add(duration).unwrap_or(now)
        });
        let dispatcher = options.progress_listener.map(ProgressDispatcher::new);
        let timer_stop = Arc::new(TimerStop::default());
        let shared = Arc::new(ExecutionShared {
            cancellation: options.cancellation,
            deadline,
            timed_out: AtomicBool::new(false),
            timer_stop: Arc::clone(&timer_stop),
            progress: Mutex::new(ProgressState::default()),
            dispatcher,
            limits,
            memory_bytes: AtomicU64::new(0),
            temporary_bytes: AtomicU64::new(0),
        });
        if let Some(deadline) = deadline {
            let weak = Arc::downgrade(&shared);
            std::thread::spawn(move || {
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
        }
        Self { shared }
    }

    /// Return a typed cancellation or timeout error at a cooperative checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ConversionError::Cancelled`] or [`ConversionError::Timeout`].
    pub fn checkpoint(&self) -> Result<(), ConversionError> {
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
        {
            return Ok(());
        }
        progress.started = true;
        progress.rank = stage.rank();
        progress.stage = Some(stage);
        progress.basis_points = basis_points;
        progress.completed = stage == ExecutionStage::Completed;
        drop(progress);
        if let Some(dispatcher) = &self.shared.dispatcher {
            dispatcher.publish(ProgressEvent {
                stage,
                basis_points,
                completed_units,
                total_units,
                message: message.map(Into::into),
            });
        }
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

    /// Create an automatically cleaned temporary file charged as bytes are written.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, resource-limit, or local I/O error.
    pub fn temporary_file(&self, prefix: &str) -> Result<TemporaryFile, ConversionError> {
        self.checkpoint()?;
        let safe_prefix = prefix
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            .take(40)
            .collect::<String>();
        let safe_prefix = if safe_prefix.is_empty() { "into-md" } else { &safe_prefix };
        for attempt in 0_u32..128 {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
            let path = std::env::temp_dir()
                .join(format!("{safe_prefix}-{}-{nonce}-{attempt}.tmp", std::process::id()));
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
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

    fn reserve(
        &self,
        kind: ResourceKind,
        bytes: u64,
    ) -> Result<ResourceReservation, ConversionError> {
        self.checkpoint()?;
        let (counter, limit, name) = match kind {
            ResourceKind::Memory => {
                (&self.shared.memory_bytes, self.shared.limits.max_memory_bytes, "max_memory_bytes")
            }
        };
        checked_charge(counter, bytes, limit, name)?;
        Ok(ResourceReservation { context: self.clone(), kind, bytes })
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
}

/// RAII resource-budget reservation.
pub struct ResourceReservation {
    context: ExecutionContext,
    kind: ResourceKind,
    bytes: u64,
}

impl Drop for ResourceReservation {
    fn drop(&mut self) {
        let counter = match self.kind {
            ResourceKind::Memory => &self.context.shared.memory_bytes,
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
            &self.context.shared.temporary_bytes,
            amount,
            self.context.shared.limits.max_temporary_bytes,
            "max_temporary_bytes",
        )?;
        let Some(file) = self.file.as_mut() else {
            self.context.shared.temporary_bytes.fetch_sub(amount, Ordering::AcqRel);
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
                        .temporary_bytes
                        .fetch_sub(amount - written_u64, Ordering::AcqRel);
                }
                Ok(written)
            }
            Err(error) => {
                self.context.shared.temporary_bytes.fetch_sub(amount, Ordering::AcqRel);
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

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
        self.context.shared.temporary_bytes.fetch_sub(self.charged, Ordering::AcqRel);
    }
}

struct ProgressDispatcher {
    mailbox: Arc<ProgressMailbox>,
}

struct ProgressMailbox {
    queue: Mutex<VecDeque<ProgressEvent>>,
    ready: Condvar,
    closed: AtomicBool,
}

impl ProgressDispatcher {
    fn new(listener: Arc<dyn ProgressListener>) -> Self {
        let mailbox = Arc::new(ProgressMailbox {
            queue: Mutex::new(VecDeque::with_capacity(8)),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        let worker_mailbox = Arc::clone(&mailbox);
        std::thread::spawn(move || {
            loop {
                let event = {
                    let mut queue = lock_unpoisoned(&worker_mailbox.queue);
                    while queue.is_empty() && !worker_mailbox.closed.load(Ordering::Acquire) {
                        queue = wait_unpoisoned(&worker_mailbox.ready, queue);
                    }
                    queue.pop_front()
                };
                let Some(event) = event else { break };
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    listener.on_progress(event);
                }));
            }
        });
        Self { mailbox }
    }

    fn publish(&self, event: ProgressEvent) {
        if self.mailbox.closed.load(Ordering::Acquire) {
            return;
        }
        let mut queue = lock_unpoisoned(&self.mailbox.queue);
        if let Some(last) = queue.back_mut()
            && last.stage == event.stage
        {
            *last = event;
        } else if queue.len() < 8 {
            queue.push_back(event);
        } else {
            // Preserve the oldest already queued boundaries and guarantee the
            // newest boundary, especially Completed, is eventually observed.
            queue.pop_back();
            queue.push_back(event);
        }
        drop(queue);
        self.mailbox.ready.notify_one();
    }
}

impl Drop for ProgressDispatcher {
    fn drop(&mut self) {
        self.mailbox.closed.store(true, Ordering::Release);
        self.mailbox.ready.notify_all();
    }
}

fn checked_charge(
    counter: &AtomicU64,
    amount: u64,
    limit: u64,
    name: &'static str,
) -> Result<(), ConversionError> {
    let result = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_add(amount).filter(|next| *next <= limit)
    });
    result.map(|_| ()).map_err(|current| ConversionError::ResourceLimit {
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
    fn saturated_progress_mailbox_still_delivers_completed() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = Arc::new(RecordingListener {
            events: Arc::clone(&events),
            delay: Duration::from_millis(20),
            panic_once: AtomicBool::new(false),
        });
        let dispatcher = ProgressDispatcher::new(listener);
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
            dispatcher.publish(ProgressEvent {
                stage: stages[usize::from(sequence) % stages.len()],
                basis_points: sequence,
                completed_units: None,
                total_units: None,
                message: None,
            });
        }
        dispatcher.publish(ProgressEvent {
            stage: ExecutionStage::Completed,
            basis_points: 10_000,
            completed_units: Some(1),
            total_units: Some(1),
            message: None,
        });
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
}
