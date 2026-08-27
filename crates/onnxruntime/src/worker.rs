//! Isolated worker lifecycle, hard process limits, and bounded IPC.

use super::native::{NativeError, NativeSession};
use super::protocol::{
    self, ERROR, ERROR_ABI, ERROR_INFERENCE, ERROR_PROTOCOL, ERROR_RESOURCE, ERROR_SESSION, Frame,
    INIT, INIT_OK, MAX_MESSAGES, RUN, RUN_OK, SHUTDOWN,
};
use super::{RuntimeLibrary, authority, current_target, ort_error, private_tempdir};
use into_markdown_core::{ConversionError, ExecutionContext, Tensor};
use into_markdown_ocr::{Dimension, ModelContract, ModelMetadata, SessionOptions, TensorSpec};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use tempfile::TempDir;

const MAX_FRAME_BYTES: usize = 512 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(2);

pub(crate) struct WorkerClient {
    process: WorkerProcess,
    stdin: Option<ChildStdin>,
    responses: mpsc::Receiver<Result<Frame, ()>>,
    reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    request_id: u64,
    stopped: bool,
    _working_directory: TempDir,
}

impl WorkerClient {
    pub(crate) fn start(
        library: &RuntimeLibrary,
        worker_executable: &Path,
        model: &[u8],
        contract: &ModelContract,
        options: &SessionOptions,
        context: &ExecutionContext,
        authenticated_snapshot: bool,
    ) -> Result<(Self, ModelMetadata), ConversionError> {
        context.checkpoint()?;
        let limits = worker_limits(library, model, contract)?;
        let working_directory = private_tempdir("into-md-ort-worker-cwd-", authenticated_snapshot)
            .map_err(|_| ort_error("workerLaunch"))?;
        let mut process = spawn_worker(
            worker_executable,
            library.private_path(),
            working_directory.path(),
            limits,
            authenticated_snapshot,
        )?;
        let stdin = process.child.stdin.take().ok_or_else(|| ort_error("workerLaunch"))?;
        let mut stdout = process.child.stdout.take().ok_or_else(|| ort_error("workerLaunch"))?;
        let mut stderr_pipe =
            process.child.stderr.take().ok_or_else(|| ort_error("workerLaunch"))?;
        let (sender, responses) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut count = 0_u64;
            loop {
                count = match count.checked_add(1) {
                    Some(value) if value <= MAX_MESSAGES => value,
                    _ => {
                        let _ = sender.send(Err(()));
                        return;
                    }
                };
                if let Ok(frame) = protocol::read_frame(&mut stdout, MAX_FRAME_BYTES) {
                    if sender.send(Ok(frame)).is_err() {
                        return;
                    }
                } else {
                    let _ = sender.send(Err(()));
                    return;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_copy = Arc::clone(&stderr);
        let stderr_reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                let Ok(count) = stderr_pipe.read(&mut buffer) else { return };
                if count == 0 {
                    return;
                }
                let mut captured =
                    stderr_copy.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let remaining = MAX_STDERR_BYTES.saturating_sub(captured.len());
                captured.extend_from_slice(&buffer[..count.min(remaining)]);
            }
        });
        let mut client = Self {
            process,
            stdin: Some(stdin),
            responses,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            stderr,
            request_id: 0,
            stopped: false,
            _working_directory: working_directory,
        };
        let payload = protocol::encode_init(model, contract, options)
            .map_err(|()| resource_error("workerProtocol"))?;
        if payload.len() > MAX_FRAME_BYTES {
            client.terminate();
            return Err(resource_error("workerProtocol"));
        }
        let frame = client.round_trip(INIT, &payload, context)?;
        let metadata = match frame.kind {
            INIT_OK => protocol::decode_metadata(&frame.payload)
                .map_err(|()| ort_error("workerProtocol"))?,
            ERROR => return Err(map_worker_error(&frame.payload)),
            _ => return Err(ort_error("workerProtocol")),
        };
        Ok((client, metadata))
    }

    pub(crate) fn run(
        &mut self,
        inputs: &[Tensor],
        outputs: &[TensorSpec],
        context: &ExecutionContext,
    ) -> Result<Vec<Tensor>, ConversionError> {
        let payload =
            protocol::encode_tensors(inputs).map_err(|()| resource_error("workerProtocol"))?;
        if payload.len() > MAX_FRAME_BYTES {
            return Err(resource_error("workerProtocol"));
        }
        let frame = self.round_trip(RUN, &payload, context)?;
        match frame.kind {
            RUN_OK => protocol::decode_tensors(&frame.payload, outputs)
                .map_err(|()| ort_error("workerProtocol")),
            ERROR => Err(map_worker_error(&frame.payload)),
            _ => {
                self.terminate();
                Err(ort_error("workerProtocol"))
            }
        }
    }

    fn round_trip(
        &mut self,
        kind: u16,
        payload: &[u8],
        context: &ExecutionContext,
    ) -> Result<Frame, ConversionError> {
        context.checkpoint()?;
        self.process.enforce_memory_limit()?;
        self.request_id = self
            .request_id
            .checked_add(1)
            .filter(|value| *value <= MAX_MESSAGES)
            .ok_or_else(|| resource_error("workerMessages"))?;
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(ort_error("workerTerminated"));
        };
        if protocol::write_frame(stdin, kind, self.request_id, payload).is_err() {
            self.terminate();
            return Err(ort_error("workerTerminated"));
        }
        loop {
            match self.responses.recv_timeout(POLL_INTERVAL) {
                Ok(Ok(frame)) => {
                    self.process.enforce_memory_limit()?;
                    if frame.request_id != self.request_id {
                        self.terminate();
                        return Err(ort_error("workerProtocol"));
                    }
                    return Ok(frame);
                }
                Ok(Err(())) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.terminate();
                    return Err(resource_error("nativeWorkerMemory"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(error) = context.checkpoint() {
                        self.terminate();
                        return Err(error);
                    }
                    self.process.enforce_memory_limit()?;
                }
            }
        }
    }

    fn terminate(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.stdin.take();
        let _ = self.process.child.kill();
        let _ = self.process.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        let _ = self.stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len();
    }
}

impl Drop for WorkerClient {
    fn drop(&mut self) {
        if !self.stopped {
            if let Some(stdin) = self.stdin.as_mut() {
                let request = self.request_id.saturating_add(1).min(MAX_MESSAGES);
                let _ = protocol::write_frame(stdin, SHUTDOWN, request, &[]);
            }
            self.terminate();
        }
    }
}

struct WorkerProcess {
    child: Child,
    #[cfg(target_os = "macos")]
    physical_memory_limit: u64,
    #[cfg(windows)]
    _job: std::os::windows::io::OwnedHandle,
}

impl WorkerProcess {
    // The cross-platform protocol loop calls one uniform hook. Windows and Linux enforce the
    // limit at process creation, while macOS additionally samples the child's physical footprint.
    #[cfg_attr(not(target_os = "macos"), allow(clippy::unused_self, clippy::unnecessary_wraps))]
    fn enforce_memory_limit(&mut self) -> Result<(), ConversionError> {
        #[cfg(target_os = "macos")]
        {
            let pid =
                i32::try_from(self.child.id()).map_err(|_| ort_error("workerLimitUnavailable"))?;
            let mut usage = std::mem::MaybeUninit::<RusageInfoV2>::zeroed();
            // SAFETY: `pid` identifies the owned child and the output buffer
            // has Darwin's exact versioned rusage_info_v2 layout.
            let result = unsafe { proc_pid_rusage(pid, 2, usage.as_mut_ptr().cast()) };
            if result != 0 {
                if self.child.try_wait().ok().flatten().is_some() {
                    return Err(ort_error("workerTerminated"));
                }
                return Err(ort_error("workerLimitUnavailable"));
            }
            // SAFETY: successful `proc_pid_rusage` initialized the structure.
            let resident = unsafe { usage.assume_init() }.ri_phys_footprint;
            if resident > self.physical_memory_limit {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(resource_error("nativeWorkerMemory"));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[allow(clippy::struct_field_names)]
struct RusageInfoV2 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,
    ri_diskio_byteswritten: u64,
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pid_rusage(pid: libc::c_int, flavor: libc::c_int, buffer: *mut libc::c_void) -> i32;
}

fn spawn_worker(
    executable: &Path,
    runtime: &Path,
    working_directory: &Path,
    limits: WorkerLimits,
    authenticated_snapshot: bool,
) -> Result<WorkerProcess, ConversionError> {
    validate_worker_path(executable, authenticated_snapshot)?;
    if !runtime.is_absolute() || !working_directory.is_absolute() {
        return Err(ort_error("workerLaunch"));
    }
    #[cfg(unix)]
    {
        let mut command = Command::new(executable);
        command
            .arg("--runtime")
            .arg(runtime)
            .arg("--address-limit")
            .arg(limits.address_space.to_string())
            .arg("--physical-limit")
            .arg(limits.physical_memory.to_string())
            .current_dir(working_directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The worker installs the platform-native limit before authority parsing,
        // ORT loading, model receipt, or additional threads. On macOS the parent
        // enforces the fixed physical-footprint ceiling instead of RLIMIT_AS.
        let child = command.spawn().map_err(classify_unix_spawn_error)?;
        return Ok(WorkerProcess {
            child,
            #[cfg(target_os = "macos")]
            physical_memory_limit: limits.physical_memory,
        });
    }
    #[cfg(windows)]
    {
        return spawn_worker_windows(executable, runtime, working_directory, limits);
    }
    #[allow(unreachable_code)]
    Err(ConversionError::ComponentUnavailable {
        component: "onnxruntime-cpu".into(),
        detail: "workerLimitUnavailable".into(),
    })
}

#[cfg(unix)]
fn classify_unix_spawn_error(error: std::io::Error) -> ConversionError {
    let detail = match error.raw_os_error() {
        Some(code) if code == libc::EACCES || code == libc::EPERM => "workerLaunchDenied",
        Some(code) if code == libc::ENOMEM || code == libc::EAGAIN => "workerLimitUnavailable",
        _ => "workerLaunch",
    };
    ort_error(detail)
}

#[cfg(windows)]
fn spawn_worker_windows(
    executable: &Path,
    runtime: &Path,
    working_directory: &Path,
    limits: WorkerLimits,
) -> Result<WorkerProcess, ConversionError> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: *mut core::ffi::c_void) -> i32;
    }

    let mut system_information = SYSTEM_INFO::default();
    // SAFETY: the pointer addresses a live, correctly sized SYSTEM_INFO value.
    unsafe { GetSystemInfo(&raw mut system_information) };
    let page_size = u64::from(system_information.dwPageSize);
    let physical_memory = align_physical_memory_limit(limits.physical_memory, page_size)
        .ok_or_else(|| resource_error("nativeWorkerMemory"))?;

    let mut command = Command::new(executable);
    command
        .arg("--runtime")
        .arg(runtime)
        .arg("--address-limit")
        .arg(limits.address_space.to_string())
        .arg("--physical-limit")
        .arg(physical_memory.to_string())
        .current_dir(working_directory)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let mut child = command.spawn().map_err(|_| ort_error("workerLaunch"))?;
    // SAFETY: null name/security creates a private job object.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ort_error("workerLimitUnavailable"));
    }
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    information.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    information.ProcessMemoryLimit =
        usize::try_from(physical_memory).map_err(|_| resource_error("nativeWorkerMemory"))?;
    // SAFETY: job/process handles are live and information has the exact API
    // layout and size. The process is still suspended during both calls.
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const information).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap(),
        ) != 0
            && AssignProcessToJobObject(job, child.as_raw_handle()) != 0
    };
    if !configured {
        // SAFETY: job is a live handle not yet wrapped.
        unsafe { CloseHandle(job) };
        let _ = child.kill();
        let _ = child.wait();
        return Err(ort_error("workerLimitUnavailable"));
    }
    // SAFETY: the process was created suspended, assigned to its hard-limit job,
    // and this call resumes it only after the limit is active.
    if unsafe { NtResumeProcess(child.as_raw_handle()) } != 0 {
        // SAFETY: job is a live handle not yet wrapped.
        unsafe { CloseHandle(job) };
        let _ = child.kill();
        let _ = child.wait();
        return Err(ort_error("workerLimitUnavailable"));
    }
    // SAFETY: ownership of the unique live job handle transfers here.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    Ok(WorkerProcess { child, _job: job })
}

#[cfg(any(windows, test))]
const fn align_physical_memory_limit(limit: u64, page_size: u64) -> Option<u64> {
    if page_size == 0 {
        return None;
    }
    let aligned = limit / page_size * page_size;
    if aligned == 0 { None } else { Some(aligned) }
}

pub(crate) fn validate_worker_path(
    path: &Path,
    authenticated_snapshot: bool,
) -> Result<(), ConversionError> {
    if !path.is_absolute() {
        return Err(ort_error("workerLaunch"));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ort_error("workerLaunch"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ort_error("workerLaunch"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ort_error("workerLaunch"));
        }
        if authenticated_snapshot {
            return Ok(());
        }
    }
    #[cfg(not(windows))]
    let _ = authenticated_snapshot;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ort_error("workerLaunch"));
        }
    }
    let canonical = path.canonicalize().map_err(|_| ort_error("workerLaunch"))?;
    if canonical != path {
        return Err(ort_error("workerLaunch"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct WorkerLimits {
    address_space: u64,
    physical_memory: u64,
}

fn worker_limits(
    library: &RuntimeLibrary,
    model: &[u8],
    contract: &ModelContract,
) -> Result<WorkerLimits, ConversionError> {
    let target_name = current_target().ok_or_else(|| ort_error("unsupportedTarget"))?;
    let authority = authority().map_err(|_| ort_error("runtimeAuthority"))?;
    let target =
        authority.targets.get(target_name).ok_or_else(|| ort_error("unsupportedTarget"))?;
    if library.version() != authority.version || library.api_version() != authority.api_version {
        return Err(ort_error("runtimeAuthority"));
    }
    let dynamic = contract
        .session_memory_bytes
        .checked_add(contract.run_memory_bytes)
        .and_then(|value| value.checked_add(u64::try_from(model.len()).ok()?))
        .ok_or_else(|| resource_error("nativeWorkerMemory"))?;
    let address_space = target
        .worker_address_space_overhead_bytes
        .checked_add(dynamic)
        .ok_or_else(|| resource_error("nativeWorkerMemory"))?;
    let physical_memory = target
        .worker_physical_memory_overhead_bytes
        .checked_add(dynamic)
        .ok_or_else(|| resource_error("nativeWorkerMemory"))?;
    Ok(WorkerLimits { address_space, physical_memory })
}

fn bind_native_metadata(
    native: &ModelMetadata,
    contract: &ModelContract,
) -> Result<ModelMetadata, NativeError> {
    Ok(ModelMetadata {
        ir_version: native.ir_version,
        opsets: native.opsets.clone(),
        inputs: bind_specs(&native.inputs, &contract.inputs)?,
        overridable_inputs: bind_specs(&native.overridable_inputs, &contract.overridable_inputs)?,
        outputs: bind_specs(&native.outputs, &contract.outputs)?,
    })
}

fn bind_specs(
    native: &[TensorSpec],
    contract: &[TensorSpec],
) -> Result<Vec<TensorSpec>, NativeError> {
    if native.len() != contract.len() {
        return Err(NativeError::Metadata);
    }
    let mut bound = Vec::new();
    bound.try_reserve_exact(native.len()).map_err(|_| NativeError::Resource)?;
    for (native, contract) in native.iter().zip(contract) {
        if native.name != contract.name
            || native.element_type != contract.element_type
            || native.dimensions.len() != contract.dimensions.len()
        {
            return Err(NativeError::Metadata);
        }
        for (native_dimension, contract_dimension) in
            native.dimensions.iter().zip(&contract.dimensions)
        {
            let compatible = match (native_dimension, contract_dimension) {
                (Dimension::Exact(native), Dimension::Exact(contract)) => native == contract,
                (Dimension::Exact(native), Dimension::Dynamic { min, max }) => {
                    native >= min && native <= max
                }
                (
                    Dimension::Dynamic { min: 1, max },
                    Dimension::Dynamic { min, max: contract_max },
                ) => *max == usize::MAX && *min > 0 && min <= contract_max,
                _ => false,
            };
            if !compatible {
                return Err(NativeError::Metadata);
            }
        }
        bound.push(contract.clone());
    }
    Ok(bound)
}

fn map_native_error(error: NativeError) -> u8 {
    match error {
        NativeError::Abi => ERROR_ABI,
        NativeError::Session | NativeError::Metadata => ERROR_SESSION,
        NativeError::Inference => ERROR_INFERENCE,
        NativeError::Resource => ERROR_RESOURCE,
    }
}

fn map_worker_error(payload: &[u8]) -> ConversionError {
    match protocol::decode_error(payload) {
        Ok(ERROR_RESOURCE) => resource_error("nativeWorkerMemory"),
        Ok(ERROR_ABI) => ort_error("runtimeAbi"),
        Ok(ERROR_SESSION) => ort_error("sessionLoad"),
        Ok(ERROR_INFERENCE) => ort_error("inference"),
        _ => ort_error("workerProtocol"),
    }
}

fn resource_error(limit: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: "ONNX Runtime worker budget exceeded".into() }
}

pub(crate) fn worker_entry() -> ExitCode {
    std::panic::set_hook(Box::new(|_| {}));
    match std::panic::catch_unwind(worker_entry_inner) {
        Ok(Ok(())) => ExitCode::SUCCESS,
        _ => ExitCode::from(70),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the bounded worker protocol state machine is kept contiguous for auditability"
)]
fn worker_entry_inner() -> Result<(), ()> {
    let mut args = std::env::args_os();
    let _program = args.next().ok_or(())?;
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--runtime")) {
        return Err(());
    }
    let runtime = PathBuf::from(args.next().ok_or(())?);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--address-limit")) {
        return Err(());
    }
    let address_limit = parse_decimal(args.next().ok_or(())?)?;
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--physical-limit")) {
        return Err(());
    }
    let physical_limit = parse_decimal(args.next().ok_or(())?)?;
    if args.next().is_some()
        || !runtime.is_absolute()
        || !install_and_verify_limit(address_limit, physical_limit)
    {
        return Err(());
    }
    let authority = authority().map_err(|_| ())?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let init = protocol::read_frame(&mut input, MAX_FRAME_BYTES)?;
    if init.kind != INIT || init.request_id != 1 {
        return Err(());
    }
    let maximum_model =
        usize::try_from(address_limit.min(MAX_FRAME_BYTES as u64)).map_err(|_| ())?;
    let Ok((model, contract, options)) = protocol::decode_init(&init.payload, maximum_model) else {
        protocol::write_frame(
            &mut output,
            ERROR,
            init.request_id,
            &protocol::error_payload(ERROR_PROTOCOL),
        )?;
        return Ok(());
    };
    let mut session = match NativeSession::new(
        &runtime,
        &authority.version,
        authority.api_version,
        model,
        &contract,
        &options,
    ) {
        Ok(session) => session,
        Err(error) => {
            protocol::write_frame(
                &mut output,
                ERROR,
                init.request_id,
                &protocol::error_payload(map_native_error(error)),
            )?;
            return Ok(());
        }
    };
    let metadata = match bind_native_metadata(session.metadata(), &contract) {
        Ok(metadata) => metadata,
        Err(error) => {
            protocol::write_frame(
                &mut output,
                ERROR,
                init.request_id,
                &protocol::error_payload(map_native_error(error)),
            )?;
            return Ok(());
        }
    };
    protocol::write_frame(
        &mut output,
        INIT_OK,
        init.request_id,
        &protocol::encode_metadata(&metadata)?,
    )?;
    let mut expected_request = 2_u64;
    while expected_request <= MAX_MESSAGES {
        let frame = protocol::read_frame(&mut input, MAX_FRAME_BYTES)?;
        if frame.request_id != expected_request {
            protocol::write_frame(
                &mut output,
                ERROR,
                frame.request_id,
                &protocol::error_payload(ERROR_PROTOCOL),
            )?;
            return Ok(());
        }
        match frame.kind {
            RUN => {
                let Ok(mut tensors) = protocol::decode_tensors(&frame.payload, &metadata.inputs)
                else {
                    protocol::write_frame(
                        &mut output,
                        ERROR,
                        frame.request_id,
                        &protocol::error_payload(ERROR_PROTOCOL),
                    )?;
                    return Ok(());
                };
                match session.run(&mut tensors) {
                    Ok(outputs) => protocol::write_frame(
                        &mut output,
                        RUN_OK,
                        frame.request_id,
                        &protocol::encode_tensors(&outputs)?,
                    )?,
                    Err(error) => protocol::write_frame(
                        &mut output,
                        ERROR,
                        frame.request_id,
                        &protocol::error_payload(map_native_error(error)),
                    )?,
                }
            }
            SHUTDOWN if frame.payload.is_empty() => return Ok(()),
            _ => {
                protocol::write_frame(
                    &mut output,
                    ERROR,
                    frame.request_id,
                    &protocol::error_payload(ERROR_PROTOCOL),
                )?;
                return Ok(());
            }
        }
        expected_request += 1;
    }
    Err(())
}

fn parse_decimal(value: OsString) -> Result<u64, ()> {
    let value = value.into_string().map_err(|_| ())?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    value.parse().map_err(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn install_and_verify_limit(expected: u64, physical: u64) -> bool {
    if physical == 0 || physical > expected {
        return false;
    }
    let limit: libc::rlim_t = expected;
    let requested = libc::rlimit { rlim_cur: limit, rlim_max: limit };
    // SAFETY: this is the worker's first operation after argument parsing and
    // runs before authority parsing, ORT loading, model receipt, or threads.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &raw const requested) } != 0 {
        return false;
    }
    let mut limits = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: points to writable storage of the exact platform structure.
    let installed = unsafe { libc::getrlimit(libc::RLIMIT_AS, &raw mut limits) == 0 };
    installed && limits.rlim_cur == expected && limits.rlim_max == expected
}

#[cfg(target_os = "macos")]
const fn install_and_verify_limit(address_space: u64, physical: u64) -> bool {
    physical >= 256 * 1024 * 1024 && physical <= address_space
}

#[cfg(windows)]
fn install_and_verify_limit(_address_space: u64, physical: u64) -> bool {
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut in_job = 0;
    // SAFETY: current process pseudo-handle and scalar out pointer are valid.
    if unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &raw mut in_job) } == 0
        || in_job == 0
    {
        return false;
    }
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: query buffer has the exact class layout and size.
    let queried = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut information).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap(),
            std::ptr::null_mut(),
        ) != 0
    };
    queried
        && information.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_PROCESS_MEMORY != 0
        && u64::try_from(information.ProcessMemoryLimit) == Ok(physical)
}

#[cfg(not(any(unix, windows)))]
const fn install_and_verify_limit(_address_space: u64, _physical: u64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_limit_is_checked_arithmetically() {
        let contract = ModelContract {
            ir_version: 9,
            opsets: [(String::new(), 18)].into_iter().collect(),
            inputs: vec![TensorSpec {
                name: "x".into(),
                element_type: into_markdown_ocr::TensorElementType::Float32,
                dimensions: vec![Dimension::Exact(1)],
            }],
            overridable_inputs: Vec::new(),
            outputs: vec![TensorSpec {
                name: "y".into(),
                element_type: into_markdown_ocr::TensorElementType::Float32,
                dimensions: vec![Dimension::Exact(1)],
            }],
            session_memory_bytes: u64::MAX,
            run_memory_bytes: 1,
        };
        let authority = authority().unwrap();
        let target = authority.targets.get(current_target().unwrap()).unwrap();
        assert!(
            target
                .worker_address_space_overhead_bytes
                .checked_add(contract.session_memory_bytes)
                .and_then(|value| value.checked_add(contract.run_memory_bytes))
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_worker_spawn_errors_keep_authority_and_resource_failures_distinct() {
        for code in [libc::EACCES, libc::EPERM] {
            assert!(
                classify_unix_spawn_error(std::io::Error::from_raw_os_error(code))
                    .to_string()
                    .contains("workerLaunchDenied")
            );
        }
        for code in [libc::ENOMEM, libc::EAGAIN] {
            assert!(
                classify_unix_spawn_error(std::io::Error::from_raw_os_error(code))
                    .to_string()
                    .contains("workerLimitUnavailable")
            );
        }
        assert!(
            classify_unix_spawn_error(std::io::Error::from_raw_os_error(libc::ENOENT))
                .to_string()
                .contains("workerLaunch")
        );
    }

    #[test]
    fn physical_memory_limit_uses_the_same_page_aligned_value_for_parent_and_child() {
        assert_eq!(align_physical_memory_limit(65_537, 4_096), Some(65_536));
        assert_eq!(align_physical_memory_limit(4_096, 4_096), Some(4_096));
        assert_eq!(align_physical_memory_limit(4_095, 4_096), None);
        assert_eq!(align_physical_memory_limit(4_096, 0), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_launch_flags_require_suspend_before_job_assignment() {
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        assert_ne!(CREATE_SUSPENDED, 0);
        assert_ne!(CREATE_NO_WINDOW, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_parent_enforces_worker_physical_memory_limit() {
        let child = Command::new("/usr/bin/yes").stdout(Stdio::null()).spawn().unwrap();
        let mut worker = WorkerProcess { child, physical_memory_limit: 1 };
        let error = worker.enforce_memory_limit().unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
        assert!(worker.child.try_wait().unwrap().is_some());
    }
}
