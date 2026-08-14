use crate::authority::VerifiedBundle;
use crate::protocol;
use crate::{NormalizedFormat, native};
use into_markdown_core::{ConversionError, ExecutionContext, InputFormat};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;
#[cfg(unix)]
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};

#[cfg(windows)]
mod windows;

const WAIT_INTERVAL: Duration = Duration::from_millis(2);
const EARLY_RESPONSE_INPUT_BYTES: u64 = 4 * 1024 * 1024 + 17;
const PARTIAL_RESPONSE_INPUT_BYTES: u64 = 4 * 1024 * 1024 + 18;
const EARLY_HANG_INPUT_BYTES: u64 = 4 * 1024 * 1024 + 19;

pub(crate) fn working_directory(
    bundle: &VerifiedBundle,
) -> Result<tempfile::TempDir, ConversionError> {
    #[cfg(unix)]
    {
        let _ = bundle;
        return tempfile::Builder::new()
            .prefix("into-md-legacy-office-")
            .tempdir()
            .map_err(|_| unavailable("workerTemporaryDirectory"));
    }
    #[cfg(windows)]
    {
        let parent = crate::windows_support::storage_path(&bundle.app_container.sid)
            .map_err(|()| unavailable("appContainerStorage"))?;
        let metadata =
            std::fs::symlink_metadata(&parent).map_err(|_| unavailable("appContainerStorage"))?;
        if !metadata.is_dir()
            || metadata.file_attributes() & 0x400 != 0
            || parent.canonicalize().map_err(|_| unavailable("appContainerStorage"))? != parent
        {
            return Err(unavailable("appContainerStorage"));
        }
        return tempfile::Builder::new()
            .prefix("into-md-legacy-office-")
            .tempdir_in(parent)
            .map_err(|_| unavailable("workerTemporaryDirectory"));
    }
    #[allow(unreachable_code)]
    Err(unavailable("unsupportedTarget"))
}

pub(crate) struct WorkerChild {
    #[cfg(unix)]
    child: Child,
    #[cfg(windows)]
    child: windows::Child,
    _load_files: Vec<std::fs::File>,
    status: Option<WorkerStatus>,
}

#[cfg(unix)]
type WorkerStatus = std::process::ExitStatus;
#[cfg(windows)]
type WorkerStatus = u32;

#[cfg(unix)]
type WorkerStdin = ChildStdin;
#[cfg(windows)]
type WorkerStdin = std::fs::File;
#[cfg(unix)]
type WorkerStdout = ChildStdout;
#[cfg(windows)]
type WorkerStdout = std::fs::File;
#[cfg(unix)]
type WorkerStderr = ChildStderr;
#[cfg(windows)]
type WorkerStderr = std::fs::File;

impl WorkerChild {
    pub(crate) fn spawn(
        bundle: &VerifiedBundle,
        working_directory: &Path,
        address_limit: u64,
    ) -> Result<Self, ConversionError> {
        if !working_directory.is_absolute() || !working_directory.is_dir() {
            return Err(unavailable("workerTemporaryDirectory"));
        }
        let working_directory = working_directory
            .canonicalize()
            .map_err(|_| unavailable("workerTemporaryDirectory"))?;
        let executable_tree = crate::snapshot::copy_tree(
            &bundle.worker_load_files,
            &working_directory.join(".worker-runtime"),
        )
        .map_err(|()| unavailable("workerIdentity"))?;
        let executable_path = executable_tree
            .path(
                bundle
                    .worker
                    .strip_prefix(&bundle.root)
                    .ok()
                    .and_then(Path::to_str)
                    .ok_or_else(|| unavailable("workerIdentity"))?,
            )
            .ok_or_else(|| unavailable("workerIdentity"))?
            .to_owned();
        let load_files = executable_tree.into_files();
        let mut arguments = vec![
            OsString::from("--runtime-root"),
            bundle.root.as_os_str().to_owned(),
            OsString::from("--worker-original"),
            bundle.worker.as_os_str().to_owned(),
            OsString::from("--worker-sha256"),
            OsString::from(&bundle.worker_sha256),
            OsString::from("--install-root"),
            bundle.install_root.as_os_str().to_owned(),
            OsString::from("--kit-library"),
            bundle.kit_library.as_os_str().to_owned(),
            OsString::from("--kit-sha256"),
            OsString::from(&bundle.kit_sha256),
            OsString::from("--authority-sha256"),
            OsString::from(&bundle.authority_sha256),
            OsString::from("--temporary-root"),
            working_directory.as_os_str().to_owned(),
            OsString::from("--address-limit"),
            OsString::from(address_limit.to_string()),
            OsString::from("--file-limit"),
            OsString::from(bundle.file_size_limit.to_string()),
            OsString::from("--open-file-limit"),
            OsString::from(bundle.open_file_limit.to_string()),
        ];
        for path in &bundle.system_read_paths {
            arguments.push(OsString::from("--system-read"));
            arguments.push(path.as_os_str().to_owned());
        }
        #[cfg(windows)]
        {
            arguments.push(OsString::from("--app-container-sid"));
            arguments.push(OsString::from(&bundle.app_container.sid));
        }
        #[cfg(unix)]
        let mut command = std::process::Command::new(&executable_path);
        #[cfg(unix)]
        command
            .args(&arguments)
            .current_dir(&working_directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        return spawn_platform(command, address_limit, load_files);
        #[cfg(windows)]
        return windows::spawn(
            &executable_path,
            &arguments,
            &working_directory,
            address_limit,
            &bundle.app_container,
        )
        .map(|child| Self { child, _load_files: load_files, status: None });
        #[allow(unreachable_code)]
        Err(unavailable("unsupportedTarget"))
    }

    pub(crate) fn take_stdin(&mut self) -> Result<WorkerStdin, ConversionError> {
        take_stdin(&mut self.child)
    }

    pub(crate) fn take_stdout(&mut self) -> Result<WorkerStdout, ConversionError> {
        take_stdout(&mut self.child)
    }

    pub(crate) fn take_stderr(&mut self) -> Result<WorkerStderr, ConversionError> {
        take_stderr(&mut self.child)
    }

    pub(crate) fn has_exited(&mut self) -> Result<bool, ConversionError> {
        if self.status.is_none() {
            self.status = try_wait(&mut self.child)?;
        }
        Ok(self.status.is_some())
    }

    pub(crate) fn wait(&mut self, context: &ExecutionContext) -> Result<(), ConversionError> {
        loop {
            if self.has_exited()? {
                return if self.status.is_some_and(status_success) {
                    Ok(())
                } else {
                    Err(unavailable(self.failure_detail()))
                };
            }
            context.checkpoint()?;
            std::thread::sleep(WAIT_INTERVAL);
        }
    }

    pub(crate) fn failure_detail(&self) -> &'static str {
        match self.status.and_then(status_code) {
            Some(70) => "sandboxUnavailable",
            Some(71) => "workerProtocol",
            Some(72) => "runtimeFailure",
            Some(_) | None => "workerTerminated",
        }
    }

    pub(crate) fn terminate(&mut self) {
        if self.status.is_some() {
            return;
        }
        terminate_platform(&mut self.child);
        self.status = wait_platform(&mut self.child).ok();
    }
}

impl Drop for WorkerChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn spawn_platform(
    mut command: std::process::Command,
    _address_limit: u64,
    load_files: Vec<std::fs::File>,
) -> Result<WorkerChild, ConversionError> {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
    let descriptor_ceiling = descriptor_ceiling()?;
    // SAFETY: the closure invokes only raw async-signal-safe syscalls between
    // fork and exec. Stdio has already been duplicated to 0/1/2 by Command.
    unsafe {
        command.pre_exec(move || close_inherited_descriptors(descriptor_ceiling));
    }
    let child = command.spawn().map_err(|_| unavailable("workerLaunch"))?;
    Ok(WorkerChild { child, _load_files: load_files, status: None })
}

#[cfg(target_os = "linux")]
fn close_inherited_descriptors(_: i32) -> std::io::Result<()> {
    // SAFETY: close_range takes scalar arguments and is async-signal-safe as a
    // direct system call. The worker contract inherits only stdin/out/err.
    let result = unsafe { libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
fn close_inherited_descriptors(ceiling: i32) -> std::io::Result<()> {
    if ceiling < 3 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    // SAFETY: close(2) is async-signal-safe. The upper bound was captured from
    // getrlimit before fork, so this loop performs no allocation or lookup.
    for descriptor in 3..ceiling {
        unsafe {
            libc::close(descriptor);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn descriptor_ceiling() -> Result<i32, ConversionError> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: limit is a live out-parameter with the exact libc layout.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return Err(unavailable("workerLaunch"));
    }
    i32::try_from(limit.rlim_cur).map_err(|_| unavailable("workerLaunch"))
}

#[cfg(unix)]
fn terminate_platform(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: a negative, validated child PID addresses only the process
        // group created for this worker. SIGKILL cannot be handled or ignored.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(unix)]
fn take_stdin(child: &mut Child) -> Result<WorkerStdin, ConversionError> {
    child.stdin.take().ok_or_else(|| unavailable("workerLaunch"))
}

#[cfg(windows)]
fn take_stdin(child: &mut windows::Child) -> Result<WorkerStdin, ConversionError> {
    child.take_stdin().ok_or_else(|| unavailable("workerLaunch"))
}

#[cfg(unix)]
fn take_stdout(child: &mut Child) -> Result<WorkerStdout, ConversionError> {
    child.stdout.take().ok_or_else(|| unavailable("workerLaunch"))
}

#[cfg(windows)]
fn take_stdout(child: &mut windows::Child) -> Result<WorkerStdout, ConversionError> {
    child.take_stdout().ok_or_else(|| unavailable("workerLaunch"))
}

#[cfg(unix)]
fn take_stderr(child: &mut Child) -> Result<WorkerStderr, ConversionError> {
    child.stderr.take().ok_or_else(|| unavailable("workerLaunch"))
}

#[cfg(windows)]
fn take_stderr(child: &mut windows::Child) -> Result<WorkerStderr, ConversionError> {
    child.take_stderr().ok_or_else(|| unavailable("workerLaunch"))
}

#[cfg(unix)]
fn try_wait(child: &mut Child) -> Result<Option<WorkerStatus>, ConversionError> {
    child.try_wait().map_err(|_| unavailable("workerWait"))
}

#[cfg(windows)]
fn try_wait(child: &mut windows::Child) -> Result<Option<WorkerStatus>, ConversionError> {
    child.try_wait().map_err(|()| unavailable("workerWait"))
}

#[cfg(unix)]
fn wait_platform(child: &mut Child) -> Result<WorkerStatus, ()> {
    child.wait().map_err(|_| ())
}

#[cfg(windows)]
fn wait_platform(child: &mut windows::Child) -> Result<WorkerStatus, ()> {
    child.wait()
}

#[cfg(windows)]
fn terminate_platform(child: &mut windows::Child) {
    child.terminate();
}

#[cfg(unix)]
fn status_success(status: WorkerStatus) -> bool {
    status.success()
}

#[cfg(windows)]
const fn status_success(status: WorkerStatus) -> bool {
    status == 0
}

#[cfg(unix)]
fn status_code(status: WorkerStatus) -> Option<i32> {
    status.code()
}

#[cfg(windows)]
fn status_code(status: WorkerStatus) -> Option<i32> {
    i32::try_from(status).ok()
}

pub(crate) fn worker_main() -> ExitCode {
    match native::run_from_args(std::env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

pub(crate) fn test_worker_main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let temporary_root = worker_argument(&arguments, "--temporary-root");
    match run_test_worker(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        temporary_root.as_deref(),
        &arguments,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::from(71),
    }
}

fn worker_argument(arguments: &[OsString], flag: &str) -> Option<std::path::PathBuf> {
    let mut arguments = arguments.iter();
    while let Some(candidate) = arguments.next() {
        let value = arguments.next()?;
        if candidate == flag {
            return Some(value.into());
        }
    }
    None
}

fn run_test_worker(
    mut input: impl Read,
    mut output: impl Write,
    temporary_root: Option<&Path>,
    arguments: &[OsString],
) -> Result<(), ()> {
    #[cfg(not(target_os = "linux"))]
    let _ = arguments;
    let metadata = protocol::read_request_meta(&mut input)?;
    if metadata.input_bytes > 8 * 1024 * 1024 {
        protocol::write_error(&mut output, protocol::ERROR_RESOURCE)?;
        return Ok(());
    }
    if metadata.input_bytes == EARLY_RESPONSE_INPUT_BYTES {
        let bytes = crate::package::fixture_package(NormalizedFormat::Docx)?;
        protocol::write_response(&mut output, NormalizedFormat::Docx, &bytes)?;
        return Ok(());
    }
    if metadata.input_bytes == PARTIAL_RESPONSE_INPUT_BYTES {
        output.write_all(b"IMOW").map_err(|_| ())?;
        return Ok(());
    }
    if metadata.input_bytes == EARLY_HANG_INPUT_BYTES {
        let bytes = crate::package::fixture_package(NormalizedFormat::Docx)?;
        protocol::write_response(&mut output, NormalizedFormat::Docx, &bytes)?;
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let mut source = Vec::new();
    source
        .try_reserve_exact(usize::try_from(metadata.input_bytes).map_err(|_| ())?)
        .map_err(|_| ())?;
    protocol::copy_request_body(&mut input, &mut source, &metadata)?;
    protocol::require_eof(&mut input)?;
    if source.starts_with(b"fixture:crash") {
        return Err(());
    }
    if source.starts_with(b"fixture:hang") {
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    if source.starts_with(b"fixture:temporary-limit") {
        let path = temporary_root.ok_or(())?.join("fixture-temporary-exhaustion");
        let file = std::fs::File::create(path).map_err(|_| ())?;
        file.set_len(128 * 1024 * 1024).map_err(|_| ())?;
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    if source.starts_with(b"fixture:encrypted") {
        protocol::write_error(&mut output, protocol::ERROR_ENCRYPTED)?;
        return Ok(());
    }
    if source.starts_with(b"fixture:response-then-nonzero") {
        let bytes = crate::package::fixture_package(NormalizedFormat::Docx)?;
        protocol::write_response(&mut output, NormalizedFormat::Docx, &bytes)?;
        return Err(());
    }
    #[cfg(target_os = "linux")]
    if source.starts_with(b"fixture:sandbox-syscalls") {
        crate::sandbox::install(&crate::sandbox::Policy::parse(arguments.iter().cloned())?)?;
        linux_sandbox_probe()?;
    }
    let format = match metadata.source {
        InputFormat::Doc => NormalizedFormat::Docx,
        InputFormat::Ppt => NormalizedFormat::Pptx,
        InputFormat::Xls => NormalizedFormat::Xlsx,
        _ => return Err(()),
    };
    if source.starts_with(b"fixture:pk-garbage") {
        return protocol::write_response(&mut output, format, b"PKgarbage");
    }
    if source.starts_with(b"fixture:wrong-family") {
        let wrong = match format {
            NormalizedFormat::Docx => NormalizedFormat::Pptx,
            NormalizedFormat::Pptx | NormalizedFormat::Xlsx => NormalizedFormat::Docx,
        };
        let bytes = crate::package::fixture_package(wrong)?;
        return protocol::write_response(&mut output, format, &bytes);
    }
    let bytes = crate::package::fixture_package(format)?;
    if u64::try_from(bytes.len()).map_err(|_| ())? > metadata.maximum_output_bytes {
        protocol::write_error(&mut output, protocol::ERROR_RESOURCE)
    } else {
        protocol::write_response(&mut output, format, &bytes)
    }
}

#[cfg(target_os = "linux")]
fn linux_sandbox_probe() -> Result<(), ()> {
    fn denied(result: libc::c_long) -> Result<(), ()> {
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            Ok(())
        } else {
            Err(())
        }
    }

    // SAFETY: signal zero is non-mutating; seccomp must reject the syscall
    // before the kernel considers the live parent PID.
    denied(unsafe { libc::syscall(libc::SYS_kill, libc::getppid(), 0) })?;
    // SAFETY: signal zero is non-mutating; a parent tgkill must be denied while
    // the same-process form remains available to the threading runtime.
    denied(unsafe { libc::syscall(libc::SYS_tgkill, libc::getppid(), libc::getppid(), 0) })?;
    if unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), libc::gettid(), 0) } != 0 {
        return Err(());
    }
    // SAFETY: all pointer/count arguments are intentionally inert. The filter
    // must return EPERM before any descriptor or address validation.
    denied(unsafe {
        libc::syscall(libc::SYS_pidfd_send_signal, -1, 0, std::ptr::null::<libc::siginfo_t>(), 0)
    })?;
    denied(unsafe {
        libc::syscall(
            libc::SYS_process_vm_readv,
            libc::getppid(),
            std::ptr::null::<libc::iovec>(),
            0,
            std::ptr::null::<libc::iovec>(),
            0,
            0,
        )
    })?;
    denied(unsafe {
        libc::syscall(
            libc::SYS_process_vm_writev,
            libc::getppid(),
            std::ptr::null::<libc::iovec>(),
            0,
            std::ptr::null::<libc::iovec>(),
            0,
            0,
        )
    })?;
    denied(unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, std::ptr::null::<u8>()) })?;
    denied(unsafe {
        libc::syscall(libc::SYS_io_uring_enter, -1, 0, 0, 0, std::ptr::null::<u8>(), 0)
    })?;
    denied(unsafe { libc::syscall(libc::SYS_io_uring_register, -1, 0, 0, 0) })?;
    Ok(())
}

fn unavailable(detail: &'static str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "legacy-office-worker".into(),
        detail: detail.into(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use into_markdown_core::{CancellationToken, ExecutionOptions, ResourceLimits};
    use std::io::BufRead as _;
    use std::os::fd::AsRawFd as _;

    fn shell(script: &str, directory: &Path) -> WorkerChild {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .current_dir(directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let executable = std::fs::File::open("/bin/sh").unwrap();
        spawn_platform(command, 512 * 1024 * 1024, vec![executable]).unwrap()
    }

    #[test]
    fn termination_kills_and_reaps_the_complete_process_group() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let mut worker = shell("/bin/cat >/dev/null & echo $!; wait", root.path());
        let mut stdout = std::io::BufReader::new(worker.take_stdout().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let descendant: i32 = line.trim().parse().unwrap();
        worker.terminate();
        assert!(worker.has_exited().unwrap());
        for _ in 0..100 {
            // SAFETY: signal zero performs no mutation and only probes this
            // exact child PID returned by the controlled shell fixture.
            if unsafe { libc::kill(descendant, 0) } != 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // SAFETY: same non-mutating PID existence probe as above.
        assert_ne!(unsafe { libc::kill(descendant, 0) }, 0);
        drop(stdout);
        drop(worker);
        drop(root);
        assert!(!path.exists());
    }

    #[test]
    fn cancellation_returns_stable_error_before_reaping() {
        let root = tempfile::tempdir().unwrap();
        let mut worker = shell("while :; do :; done", root.path());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        assert!(matches!(worker.wait(&context), Err(ConversionError::Cancelled)));
        worker.terminate();
        assert!(worker.has_exited().unwrap());
    }

    #[test]
    fn spawn_closes_non_cloexec_files_and_sockets_but_preserves_stdio() {
        let root = tempfile::tempdir().unwrap();
        let secret = std::fs::File::create(root.path().join("secret")).unwrap();
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        clear_cloexec(secret.as_raw_fd());
        clear_cloexec(socket.as_raw_fd());
        let script = format!(
            "if (: <&{}) 2>/dev/null || (: <&{}) 2>/dev/null; then exit 41; fi; IFS= read -r line; printf '%s' \"$line\"",
            secret.as_raw_fd(),
            socket.as_raw_fd()
        );
        let mut worker = shell(&script, root.path());
        let mut stdin = worker.take_stdin().unwrap();
        stdin.write_all(b"stdio-preserved\n").unwrap();
        drop(stdin);
        let mut stdout = String::new();
        worker.take_stdout().unwrap().read_to_string(&mut stdout).unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        worker.wait(&context).unwrap();
        assert_eq!(stdout, "stdio-preserved");
    }

    fn clear_cloexec(descriptor: i32) {
        // SAFETY: the descriptors are live test-owned file/socket handles and
        // F_SETFD mutates only their inheritance flag.
        assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_SETFD, 0) }, 0);
    }
}
