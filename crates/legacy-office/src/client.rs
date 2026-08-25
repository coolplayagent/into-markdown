use crate::authority::{RuntimeConfig, VerifiedBundle, verify};
use crate::process::WorkerChild;
use crate::protocol::{
    self, ERROR_ENCRYPTED, ERROR_MALFORMED, ERROR_RESOURCE, ERROR_RUNTIME, ERROR_SANDBOX,
    WorkerReply,
};
use crate::{MAX_NORMALIZED_PACKAGE_BYTES, NormalizedFormat, NormalizedPackage};
use into_markdown_core::{ConversionError, ExecutionContext, InputFormat};
use std::io::Read;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(2);
const EARLY_REPLY_GRACE: Duration = Duration::from_secs(10);
const MAX_STDERR_BYTES: usize = 64 * 1024;
const WORKER_TEMP_OVERHEAD: u64 = 16 * 1024 * 1024;
const MAX_WORKER_TEMP_ENTRIES: u32 = 16_384;
const MAX_WORKER_TEMP_DEPTH: u16 = 64;

pub(crate) fn convert(
    config: &RuntimeConfig,
    bytes: &[u8],
    source_format: InputFormat,
    maximum_output_bytes: u64,
    context: &ExecutionContext,
) -> Result<NormalizedPackage, ConversionError> {
    let bundle = verify(config, context)?;
    convert_verified(
        &bundle,
        config.inherited_process_sandbox(),
        bytes,
        source_format,
        maximum_output_bytes,
        context,
    )
}

// Conversion is a security boundary: resource reservations, the immutable worker snapshot,
// protocol exchange, and output validation remain visible as one ordered transaction.
#[allow(clippy::too_many_lines)]
fn convert_verified(
    bundle: &VerifiedBundle,
    inherited_process_sandbox: bool,
    bytes: &[u8],
    source_format: InputFormat,
    maximum_output_bytes: u64,
    context: &ExecutionContext,
) -> Result<NormalizedPackage, ConversionError> {
    context.checkpoint()?;
    let expected = expected_output(source_format)?;
    let input_bytes = u64::try_from(bytes.len())
        .map_err(|_| resource("max_input_bytes", "legacy Office input size overflowed"))?;
    let input_plan = input_bytes
        .checked_add(u64::try_from(std::mem::size_of::<usize>() * 2).unwrap_or(u64::MAX))
        .ok_or_else(|| resource("max_memory_bytes", "worker input allocation plan overflowed"))?;
    let input_memory = context.reserve_memory(input_plan)?;
    let audit_memory = context.reserve_memory(crate::NORMALIZED_PACKAGE_AUDIT_MEMORY_BYTES)?;
    let maximum_output_bytes = maximum_output_bytes
        .min(MAX_NORMALIZED_PACKAGE_BYTES)
        .min(context.available_memory_bytes());
    if maximum_output_bytes == 0 {
        return Err(resource("max_archive_entry_bytes", "normalized package limit is zero"));
    }
    let temporary_plan = input_bytes
        .checked_add(maximum_output_bytes)
        .and_then(|value| value.checked_add(bundle.runtime_snapshot_bytes))
        .and_then(|value| value.checked_add(WORKER_TEMP_OVERHEAD))
        .ok_or_else(|| resource("max_temporary_bytes", "worker temporary plan overflowed"))?;
    let temporary = context.reserve_temporary(temporary_plan)?;
    let mut output_memory = context.reserve_memory(maximum_output_bytes)?;
    let working_directory = crate::process::working_directory(bundle)?;
    let worker_temporary = working_directory.path().join("requests");
    std::fs::create_dir(&worker_temporary).map_err(|_| unavailable("workerTemporaryDirectory"))?;
    let address_limit = worker_address_limit(bundle, input_bytes, maximum_output_bytes, context)?;
    let mut worker = WorkerChild::spawn(
        bundle,
        inherited_process_sandbox,
        working_directory.path(),
        &worker_temporary,
        address_limit,
        context,
    )?;
    let maximum_temporary_entries = worker_temporary_entry_limit(bundle)?;
    let input: Arc<[u8]> = Arc::from(bytes);
    let io = WorkerIo::spawn(&mut worker, input, source_format, maximum_output_bytes)?;
    let mut result = wait_for_reply(
        &mut worker,
        &io.writes,
        &io.replies,
        &worker_temporary,
        temporary_plan,
        maximum_temporary_entries,
        context,
    );
    let status = if result.is_err() {
        worker.terminate();
        Ok(())
    } else {
        wait_for_exit(
            &mut worker,
            &worker_temporary,
            temporary_plan,
            maximum_temporary_entries,
            context,
        )
    };
    if status.is_err() {
        worker.terminate();
    }
    let joined = io.join();
    if let Err(error) = result {
        result = Err(augment_worker_error(error, &joined.stderr));
    }
    drop(input_memory);
    if joined.panicked || (joined.failed && result.is_ok() && status.is_ok()) {
        return Err(unavailable("workerProtocol"));
    }
    status?;
    let reply = result?;
    let WorkerReply::Output(response) = reply else {
        let WorkerReply::Error(code) = reply else { unreachable!() };
        return Err(map_worker_error(code));
    };
    if response.format != expected {
        return Err(unavailable("workerProtocol"));
    }
    crate::package::audit(&response.bytes, expected, context).map_err(|error| match error {
        ConversionError::Cancelled
        | ConversionError::Timeout
        | ConversionError::ResourceLimit { .. } => error,
        _ => unavailable("workerProtocol"),
    })?;
    if response.bytes.capacity() > usize::try_from(maximum_output_bytes).unwrap_or(usize::MAX) {
        return Err(resource(
            "max_memory_bytes",
            "normalized package allocation exceeded its reservation",
        ));
    }
    let bytes = response.bytes.into_boxed_slice();
    let used = u64::try_from(bytes.len())
        .map_err(|_| resource("max_memory_bytes", "normalized package size overflowed"))?;
    output_memory.shrink(maximum_output_bytes.saturating_sub(used))?;
    drop(audit_memory);
    // The worker owns the immutable runtime snapshot. Reap it and let the
    // snapshot restore directory owner-write before TempDir removal.
    drop(worker);
    drop(temporary);
    drop(working_directory);
    Ok(NormalizedPackage {
        bytes,
        format: response.format,
        runtime: bundle.identity.clone(),
        memory: output_memory,
    })
}

fn worker_address_limit(
    bundle: &VerifiedBundle,
    input_bytes: u64,
    output_bytes: u64,
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    bundle
        .address_space_overhead
        .checked_add(context.available_memory_bytes())
        .and_then(|value| value.checked_add(input_bytes))
        .and_then(|value| value.checked_add(output_bytes))
        .ok_or_else(|| resource("legacy_office_worker_memory", "address limit overflowed"))
}

fn worker_temporary_entry_limit(bundle: &VerifiedBundle) -> Result<u32, ConversionError> {
    u32::try_from(bundle.runtime_files.len())
        .ok()
        .and_then(|value| value.checked_add(MAX_WORKER_TEMP_ENTRIES))
        .ok_or_else(|| resource("legacy_office_worker_temporary", "entry plan overflowed"))
}

fn wait_for_reply(
    worker: &mut WorkerChild,
    writes: &mpsc::Receiver<Result<(), ()>>,
    replies: &mpsc::Receiver<Result<ReplyEvent, ()>>,
    temporary_root: &std::path::Path,
    maximum_temporary_bytes: u64,
    maximum_temporary_entries: u32,
    context: &ExecutionContext,
) -> Result<WorkerReply, ConversionError> {
    let mut write_finished = false;
    let mut reply = None;
    let mut response_eof = false;
    let mut reply_received_at = None;
    loop {
        if !write_finished {
            match writes.try_recv() {
                Ok(Ok(())) => write_finished = true,
                Ok(Err(())) | Err(mpsc::TryRecvError::Disconnected) => {
                    return worker_protocol_or_exit(worker);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if !response_eof {
            match replies.recv_timeout(POLL_INTERVAL) {
                Ok(Ok(ReplyEvent::Frame(value))) => {
                    if reply.replace(value).is_some() {
                        return Err(unavailable("workerProtocol"));
                    }
                    reply_received_at = Some(std::time::Instant::now());
                }
                Ok(Ok(ReplyEvent::Eof)) if reply.is_some() && !response_eof => {
                    response_eof = true;
                }
                Ok(Ok(ReplyEvent::Eof)) => return worker_protocol_or_exit(worker),
                Ok(Err(())) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return worker_protocol_or_exit(worker);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        if write_finished
            && response_eof
            && let Some(reply) = reply
        {
            return Ok(reply);
        }
        if (!write_finished || !response_eof)
            && reply_received_at.is_some_and(|received| received.elapsed() >= EARLY_REPLY_GRACE)
        {
            return Err(unavailable("workerProtocol"));
        }
        check_worker_temporary(
            temporary_root,
            maximum_temporary_bytes,
            maximum_temporary_entries,
            context,
        )?;
        worker.enforce_memory_limit()?;
        if let Err(error) = context.checkpoint() {
            worker.terminate();
            return Err(error);
        }
        // Exit may race the two pipe-reader threads. Polling status here
        // records it, but the channel results remain the protocol authority;
        // EOF guarantees both threads finish without an unbounded wait.
        let _ = worker.has_exited()?;
    }
}

fn worker_protocol_or_exit(worker: &mut WorkerChild) -> Result<WorkerReply, ConversionError> {
    for _ in 0..50 {
        if worker.has_exited()? {
            if worker.exited_successfully() {
                return Err(unavailable("workerProtocol"));
            }
            return Err(unavailable(worker.failure_detail()));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(unavailable("workerProtocol"))
}

fn wait_for_exit(
    worker: &mut WorkerChild,
    temporary_root: &std::path::Path,
    maximum_temporary_bytes: u64,
    maximum_temporary_entries: u32,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    loop {
        check_worker_temporary(
            temporary_root,
            maximum_temporary_bytes,
            maximum_temporary_entries,
            context,
        )?;
        worker.enforce_memory_limit()?;
        if worker.has_exited()? {
            return worker.wait(context);
        }
        context.checkpoint()?;
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn check_worker_temporary(
    root: &std::path::Path,
    maximum_bytes: u64,
    maximum_entries: u32,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut state = TemporaryUsage { bytes: 0, entries: 0 };
    scan_temporary(root, 0, maximum_bytes, maximum_entries, &mut state, context)
}

struct TemporaryUsage {
    bytes: u64,
    entries: u32,
}

fn scan_temporary(
    directory: &std::path::Path,
    depth: u16,
    maximum_bytes: u64,
    maximum_entries: u32,
    state: &mut TemporaryUsage,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if depth > MAX_WORKER_TEMP_DEPTH {
        return Err(resource(
            "legacy_office_worker_temporary",
            "worker temporary directory nesting exceeded",
        ));
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if depth > 0 && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(unavailable("workerTemporaryDirectory")),
    };
    for entry in entries {
        context.checkpoint()?;
        let entry = entry.map_err(|_| unavailable("workerTemporaryDirectory"))?;
        state.entries = state.entries.checked_add(1).ok_or_else(|| {
            resource("legacy_office_worker_temporary", "worker temporary entry count overflowed")
        })?;
        if state.entries > maximum_entries {
            return Err(resource(
                "legacy_office_worker_temporary",
                "worker temporary entry limit exceeded",
            ));
        }
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(unavailable("workerTemporaryDirectory")),
        };
        if metadata.file_type().is_symlink() {
            return Err(unavailable("workerTemporaryOutput"));
        }
        if metadata.is_dir() {
            scan_temporary(
                &path,
                depth.saturating_add(1),
                maximum_bytes,
                maximum_entries,
                state,
                context,
            )?;
        } else if metadata.is_file() {
            state.bytes = state.bytes.checked_add(metadata.len()).ok_or_else(|| {
                resource("legacy_office_worker_temporary", "worker temporary bytes overflowed")
            })?;
            if state.bytes > maximum_bytes {
                return Err(resource(
                    "legacy_office_worker_temporary",
                    "worker temporary byte limit exceeded",
                ));
            }
        } else {
            return Err(unavailable("workerTemporaryOutput"));
        }
    }
    Ok(())
}

struct WorkerIo {
    writes: mpsc::Receiver<Result<(), ()>>,
    replies: mpsc::Receiver<Result<ReplyEvent, ()>>,
    input_thread: JoinHandle<Result<(), ()>>,
    output_thread: JoinHandle<Result<(), ()>>,
    stderr_reader: JoinHandle<Result<(), ()>>,
    stderr_bytes: Arc<Mutex<Vec<u8>>>,
}

enum ReplyEvent {
    Frame(WorkerReply),
    Eof,
}

impl WorkerIo {
    fn spawn(
        worker: &mut WorkerChild,
        input: Arc<[u8]>,
        source_format: InputFormat,
        maximum_output_bytes: u64,
    ) -> Result<Self, ConversionError> {
        let stdin = worker.take_stdin()?;
        let stdout = worker.take_stdout()?;
        let stderr = worker.take_stderr()?;
        let (write_sender, writes) = mpsc::sync_channel(1);
        let input_thread = std::thread::Builder::new()
            .name("into-md-legacy-office-input".into())
            .spawn(move || {
                let mut stdin = stdin;
                let result = protocol::write_request(
                    &mut stdin,
                    source_format,
                    &input,
                    maximum_output_bytes,
                );
                drop(stdin);
                write_sender.send(result).map_err(|_| ())?;
                result
            })
            .map_err(|_| unavailable("workerInputThread"))?;
        let (read_sender, replies) = mpsc::sync_channel(2);
        let Ok(output_thread) = std::thread::Builder::new()
            .name("into-md-legacy-office-output".into())
            .spawn(move || {
                let mut stdout = stdout;
                let Ok(reply) = protocol::read_reply_frame(&mut stdout, maximum_output_bytes)
                else {
                    let _ = read_sender.send(Err(()));
                    return Err(());
                };
                if read_sender.send(Ok(ReplyEvent::Frame(reply))).is_err() {
                    return Err(());
                }
                let result = protocol::require_eof(&mut stdout);
                read_sender.send(result.map(|()| ReplyEvent::Eof)).map_err(|_| ())?;
                result
            })
        else {
            worker.terminate();
            let _ = join_one(input_thread);
            return Err(unavailable("workerOutputThread"));
        };
        let stderr_bytes = Arc::new(Mutex::new(Vec::new()));
        let stderr_reader = match spawn_stderr_reader(stderr, Arc::clone(&stderr_bytes)) {
            Ok(stderr_reader) => stderr_reader,
            Err(error) => {
                worker.terminate();
                let _ = join_one(input_thread);
                let _ = join_one(output_thread);
                return Err(error);
            }
        };
        Ok(Self { writes, replies, input_thread, output_thread, stderr_reader, stderr_bytes })
    }

    fn join(self) -> JoinOutcome {
        let captured = Arc::clone(&self.stderr_bytes);
        let mut outcome = join_all([self.input_thread, self.output_thread, self.stderr_reader]);
        outcome
            .stderr
            .clone_from(&captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        outcome
    }
}

#[derive(Default)]
struct JoinOutcome {
    failed: bool,
    panicked: bool,
    stderr: Vec<u8>,
}

fn augment_worker_error(error: ConversionError, stderr: &[u8]) -> ConversionError {
    let ConversionError::ComponentUnavailable { component, detail } = error else {
        return error;
    };
    if component != "legacy-office-worker" || !detail.starts_with("runtimeLibrary") {
        return ConversionError::ComponentUnavailable { component, detail };
    }
    let Some(code) = loader_diagnostic(stderr) else {
        return ConversionError::ComponentUnavailable { component, detail };
    };
    ConversionError::ComponentUnavailable { component, detail: format!("{detail}:win32:{code}") }
}

fn loader_diagnostic(stderr: &[u8]) -> Option<i32> {
    const PREFIX: &str = "into-md-worker:loader-win32=";
    std::str::from_utf8(stderr)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(PREFIX)?.parse::<i32>().ok())
        .filter(|code| *code >= -1)
}

fn spawn_stderr_reader(
    stderr: impl Read + Send + 'static,
    captured: Arc<Mutex<Vec<u8>>>,
) -> Result<JoinHandle<Result<(), ()>>, ConversionError> {
    spawn_stderr_reader_with_hook(stderr, captured, None)
}

fn spawn_stderr_reader_with_hook(
    mut stderr: impl Read + Send + 'static,
    captured: Arc<Mutex<Vec<u8>>>,
    hook: Option<fn(&[u8])>,
) -> Result<JoinHandle<Result<(), ()>>, ConversionError> {
    std::thread::Builder::new()
        .name("into-md-legacy-office-stderr".into())
        .spawn(move || {
            let mut buffer = [0_u8; 4 * 1024];
            loop {
                let count = stderr.read(&mut buffer).map_err(|_| ())?;
                if count == 0 {
                    return Ok(());
                }
                if let Some(hook) = hook {
                    hook(&buffer[..count]);
                }
                let mut bytes = captured.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let remaining = MAX_STDERR_BYTES.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buffer[..count.min(remaining)]);
            }
        })
        .map_err(|_| unavailable("workerStderrThread"))
}

fn expected_output(source: InputFormat) -> Result<NormalizedFormat, ConversionError> {
    match source {
        InputFormat::Doc => Ok(NormalizedFormat::Docx),
        InputFormat::Ppt => Ok(NormalizedFormat::Pptx),
        InputFormat::Xls => Ok(NormalizedFormat::Xlsx),
        _ => Err(ConversionError::Unsupported {
            detail: "legacy Office worker accepts only DOC, PPT/PPS/POT, or XLS".into(),
        }),
    }
}

fn map_worker_error(code: u8) -> ConversionError {
    match code {
        ERROR_MALFORMED => ConversionError::Malformed {
            part: None,
            detail: "legacy Office runtime rejected the document".into(),
        },
        ERROR_ENCRYPTED => ConversionError::Encrypted,
        ERROR_RESOURCE => resource("legacy_office_worker", "compatibility worker limit exceeded"),
        ERROR_RUNTIME => unavailable("runtimeFailure"),
        ERROR_SANDBOX => unavailable("sandboxUnavailable"),
        _ => unavailable("workerProtocol"),
    }
}

fn join_one(handle: JoinHandle<Result<(), ()>>) -> std::thread::Result<Result<(), ()>> {
    handle.join()
}

fn join_all<const N: usize>(handles: [JoinHandle<Result<(), ()>>; N]) -> JoinOutcome {
    handles.into_iter().map(join_one).fold(JoinOutcome::default(), |mut outcome, result| {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(())) => outcome.failed = true,
            Err(_) => outcome.panicked = true,
        }
        outcome
    })
}

fn unavailable(detail: &'static str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "legacy-office-worker".into(),
        detail: detail.into(),
    }
}

fn resource(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenReader;

    impl Read for BrokenReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("directed read failure"))
        }
    }

    #[test]
    fn input_output_and_stderr_thread_failures_are_not_dropped() {
        let failed = std::thread::spawn(|| Err(()));
        let successful = std::thread::spawn(|| Ok(()));
        let stderr = spawn_stderr_reader(BrokenReader, Arc::new(Mutex::new(Vec::new()))).unwrap();
        let outcome = join_all([failed, successful, stderr]);
        assert!(outcome.failed);
        assert!(!outcome.panicked);
    }

    #[test]
    fn stderr_reader_panic_is_reported_by_join() {
        fn panic_hook(_: &[u8]) {
            panic!("directed stderr hook panic");
        }
        let stderr = spawn_stderr_reader_with_hook(
            std::io::Cursor::new(b"worker stderr".to_vec()),
            Arc::new(Mutex::new(Vec::new())),
            Some(panic_hook),
        )
        .unwrap();
        let outcome = join_all([stderr]);
        assert!(outcome.panicked);
    }

    #[test]
    fn loader_diagnostic_accepts_only_the_fixed_numeric_worker_token() {
        assert_eq!(loader_diagnostic(b"into-md-worker:loader-win32=126\n"), Some(126));
        assert_eq!(loader_diagnostic(b"path=C:\\private\n"), None);
        assert_eq!(loader_diagnostic(b"into-md-worker:loader-win32=126:path\n"), None);
        assert_eq!(loader_diagnostic(b"into-md-worker:loader-win32=-2\n"), None);
    }
}
