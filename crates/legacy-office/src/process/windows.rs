use crate::authority::AppContainerAuthority;
use crate::windows_support::AppContainerSid;
use into_markdown_core::ConversionError;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

const INFINITE: u32 = u32::MAX;
const FAILED_RESUME: u32 = u32::MAX;

pub(super) struct Child {
    process: OwnedHandle,
    _job: OwnedHandle,
    stdin: Option<File>,
    stdout: Option<File>,
    stderr: Option<File>,
}

impl Child {
    pub(super) fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }

    pub(super) fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }

    pub(super) fn take_stderr(&mut self) -> Option<File> {
        self.stderr.take()
    }

    pub(super) fn try_wait(&mut self) -> Result<Option<u32>, ()> {
        // SAFETY: process is a live owned process handle and zero is a poll.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => exit_code(self.process.as_raw_handle()).map(Some),
            _ => Err(()),
        }
    }

    pub(super) fn wait(&mut self) -> Result<u32, ()> {
        // SAFETY: process is a live owned process handle.
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(());
        }
        exit_code(self.process.as_raw_handle())
    }

    pub(super) fn terminate(&mut self) {
        // SAFETY: terminating an already-exited process is harmless and the
        // subsequent wait ensures the kernel process object is reaped.
        unsafe {
            let _ = TerminateProcess(self.process.as_raw_handle(), 1);
            let _ = WaitForSingleObject(self.process.as_raw_handle(), INFINITE);
        }
    }
}

pub(super) fn spawn(
    executable: &Path,
    arguments: &[OsString],
    working_directory: &Path,
    address_limit: u64,
    authority: &AppContainerAuthority,
) -> Result<Child, ConversionError> {
    let sid =
        AppContainerSid::derive(authority).map_err(|()| unavailable("appContainerIdentity"))?;
    let stdin = pipe(false)?;
    let stdout = pipe(true)?;
    let stderr = pipe(true)?;
    let inherited = [stdin.child_raw(), stdout.child_raw(), stderr.child_raw()];
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid.as_ptr(),
        Capabilities: std::ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let mut attributes = AttributeList::new(2)?;
    attributes.update(
        usize::try_from(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES)
            .map_err(|_| unavailable("workerLaunch"))?,
        (&raw const capabilities).cast(),
        size_of::<SECURITY_CAPABILITIES>(),
    )?;
    attributes.update(
        usize::try_from(PROC_THREAD_ATTRIBUTE_HANDLE_LIST)
            .map_err(|_| unavailable("workerLaunch"))?,
        inherited.as_ptr().cast(),
        size_of::<[HANDLE; 3]>(),
    )?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb =
        u32::try_from(size_of::<STARTUPINFOEXW>()).map_err(|_| unavailable("workerLaunch"))?;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[2];
    startup.lpAttributeList = attributes.as_mut_ptr();
    let application = wide_path(executable)?;
    let mut command_line = command_line(executable.as_os_str(), arguments)?;
    let current_directory = wide_path(working_directory)?;
    // An explicit double-NUL block prevents inheritance of PATH, proxy,
    // loader, HOME, and all other parent environment variables.
    let environment = [0_u16, 0];
    let mut information = PROCESS_INFORMATION::default();
    // SAFETY: all strings, handles, attributes, and startup fields remain live
    // for the call. The handle list is the complete inherited-handle set.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            current_directory.as_ptr(),
            &raw const startup.StartupInfo,
            &raw mut information,
        )
    } == 0
    {
        return Err(unavailable("workerLaunch"));
    }
    // SAFETY: CreateProcessW returned two unique owned handles.
    let process = unsafe { OwnedHandle::from_raw_handle(information.hProcess) };
    // SAFETY: same successful call returned this thread handle.
    let thread = unsafe { OwnedHandle::from_raw_handle(information.hThread) };
    let job = match create_job(address_limit) {
        Ok(job) => job,
        Err(error) => {
            rollback(process.as_raw_handle());
            return Err(error);
        }
    };
    // SAFETY: the process is suspended, and the live job has active-process,
    // memory, and kill-on-close limits installed before this assignment.
    let mut launch = SuspendedProcess {
        process: process.as_raw_handle(),
        thread: thread.as_raw_handle(),
        job: job.as_raw_handle(),
        expected_sid: &authority.sid,
    };
    secure_resume(&mut launch).map_err(unavailable)?;
    drop(thread);
    drop(attributes);
    Ok(Child {
        process,
        _job: job,
        stdin: Some(stdin.into_parent()),
        stdout: Some(stdout.into_parent()),
        stderr: Some(stderr.into_parent()),
    })
}

trait SuspendedLaunch {
    fn assign_job(&mut self) -> bool;
    fn token_matches(&mut self) -> Result<bool, ()>;
    fn resume_once(&mut self) -> bool;
    fn terminate_and_wait(&mut self);
}

fn secure_resume(launch: &mut impl SuspendedLaunch) -> Result<(), &'static str> {
    if !launch.assign_job() {
        launch.terminate_and_wait();
        return Err("workerLimitUnavailable");
    }
    if !matches!(launch.token_matches(), Ok(true)) {
        launch.terminate_and_wait();
        return Err("appContainerIdentity");
    }
    if !launch.resume_once() {
        launch.terminate_and_wait();
        return Err("workerLaunch");
    }
    Ok(())
}

struct SuspendedProcess<'a> {
    process: HANDLE,
    thread: HANDLE,
    job: HANDLE,
    expected_sid: &'a str,
}

impl SuspendedLaunch for SuspendedProcess<'_> {
    fn assign_job(&mut self) -> bool {
        // SAFETY: both handles are live and the primary thread is suspended.
        unsafe { AssignProcessToJobObject(self.job, self.process) != 0 }
    }

    fn token_matches(&mut self) -> Result<bool, ()> {
        crate::windows_support::process_token_matches(self.process, self.expected_sid)
    }

    fn resume_once(&mut self) -> bool {
        // SAFETY: this is the primary thread created once with CREATE_SUSPENDED.
        let previous = unsafe { ResumeThread(self.thread) };
        previous != FAILED_RESUME && previous == 1
    }

    fn terminate_and_wait(&mut self) {
        rollback(self.process);
    }
}

fn create_job(address_limit: u64) -> Result<OwnedHandle, ConversionError> {
    // SAFETY: null security and name create a request-private job object.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(unavailable("workerLimitUnavailable"));
    }
    // SAFETY: CreateJobObjectW returned a unique owned handle.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.ProcessMemoryLimit =
        usize::try_from(address_limit).map_err(|_| unavailable("workerLimitUnavailable"))?;
    // SAFETY: job and limits are live with the documented information layout.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .map_err(|_| unavailable("workerLimitUnavailable"))?,
        )
    } == 0
    {
        return Err(unavailable("workerLimitUnavailable"));
    }
    Ok(job)
}

fn rollback(process: HANDLE) {
    // SAFETY: failure paths own a suspended process and must terminate and wait
    // before returning. All handles then close through their owning wrappers.
    unsafe {
        let _ = TerminateProcess(process, 1);
        let _ = WaitForSingleObject(process, INFINITE);
    }
}

fn exit_code(process: HANDLE) -> Result<u32, ()> {
    let mut code = 0_u32;
    // SAFETY: caller supplies a live, signaled process handle.
    if unsafe { GetExitCodeProcess(process, &raw mut code) } == 0 { Err(()) } else { Ok(code) }
}

struct Pipe {
    child: OwnedHandle,
    parent: File,
}

impl Pipe {
    fn child_raw(&self) -> HANDLE {
        self.child.as_raw_handle()
    }

    fn into_parent(self) -> File {
        self.parent
    }
}

fn pipe(parent_reads: bool) -> Result<Pipe, ConversionError> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|_| unavailable("workerLaunch"))?,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    // SAFETY: output pointers and security structure are live.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, &raw mut attributes, 0) } == 0 {
        return Err(unavailable("workerLaunch"));
    }
    let (child, parent) = if parent_reads { (write, read) } else { (read, write) };
    // SAFETY: CreatePipe returned unique live handles; parent must not inherit.
    if unsafe { SetHandleInformation(parent, HANDLE_FLAG_INHERIT, 0) } == 0 {
        unsafe {
            CloseHandle(read);
            CloseHandle(write);
        }
        return Err(unavailable("workerLaunch"));
    }
    // SAFETY: ownership of the disjoint handles transfers exactly once.
    Ok(Pipe {
        child: unsafe { OwnedHandle::from_raw_handle(child) },
        parent: unsafe { File::from_raw_handle(parent) },
    })
}

struct AttributeList {
    storage: Vec<usize>,
}

impl AttributeList {
    fn new(count: u32) -> Result<Self, ConversionError> {
        let mut bytes = 0_usize;
        // SAFETY: the documented sizing call uses a null list and returns the
        // required byte count; its false result is expected.
        unsafe {
            let _ =
                InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut bytes);
        }
        if bytes == 0 || bytes > 64 * 1024 {
            return Err(unavailable("workerLaunch"));
        }
        let words = bytes.div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; words];
        // SAFETY: storage is aligned and contains at least `bytes` writable bytes.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), count, 0, &raw mut bytes)
        } == 0
        {
            return Err(unavailable("workerLaunch"));
        }
        Ok(Self { storage })
    }

    fn as_mut_ptr(&mut self) -> *mut core::ffi::c_void {
        self.storage.as_mut_ptr().cast()
    }

    fn update(
        &mut self,
        attribute: usize,
        value: *const core::ffi::c_void,
        bytes: usize,
    ) -> Result<(), ConversionError> {
        // SAFETY: list is initialized and value remains live through process creation.
        if unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                attribute,
                value,
                bytes,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(unavailable("workerLaunch"));
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: new initialized this list exactly once.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

fn command_line(executable: &OsStr, arguments: &[OsString]) -> Result<Vec<u16>, ConversionError> {
    let mut output = Vec::new();
    append_argument(&mut output, executable)?;
    for argument in arguments {
        output.push(u16::from(b' '));
        append_argument(&mut output, argument)?;
    }
    output.push(0);
    if output.len() > 32_767 {
        return Err(unavailable("workerLaunch"));
    }
    Ok(output)
}

fn append_argument(output: &mut Vec<u16>, argument: &OsStr) -> Result<(), ConversionError> {
    let units: Vec<u16> = argument.encode_wide().collect();
    if units.contains(&0) {
        return Err(unavailable("workerLaunch"));
    }
    output.push(u16::from(b'"'));
    let mut slashes = 0_usize;
    for unit in units {
        if unit == u16::from(b'\\') {
            slashes += 1;
            continue;
        }
        if unit == u16::from(b'"') {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2 + 1));
        } else {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
        }
        slashes = 0;
        output.push(unit);
    }
    output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2));
    output.push(u16::from(b'"'));
    Ok(())
}

fn wide_path(path: &Path) -> Result<Vec<u16>, ConversionError> {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.is_empty() || units.len() >= 32_767 || units.contains(&0) {
        return Err(unavailable("workerLaunch"));
    }
    Ok(units.into_iter().chain(Some(0)).collect())
}

fn unavailable(detail: &'static str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "legacy-office-worker".into(),
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum Failure {
        None,
        Assign,
        TokenError,
        TokenMismatch,
        Resume,
    }

    struct MockLaunch {
        failure: Failure,
        events: Vec<&'static str>,
    }

    impl SuspendedLaunch for MockLaunch {
        fn assign_job(&mut self) -> bool {
            self.events.push("assign");
            !matches!(self.failure, Failure::Assign)
        }

        fn token_matches(&mut self) -> Result<bool, ()> {
            self.events.push("token");
            match self.failure {
                Failure::TokenError => Err(()),
                Failure::TokenMismatch => Ok(false),
                _ => Ok(true),
            }
        }

        fn resume_once(&mut self) -> bool {
            self.events.push("resume");
            !matches!(self.failure, Failure::Resume)
        }

        fn terminate_and_wait(&mut self) {
            self.events.push("terminate");
            self.events.push("wait");
        }
    }

    #[test]
    fn suspended_launch_assigns_job_checks_identity_then_resumes() {
        let mut launch = MockLaunch { failure: Failure::None, events: Vec::new() };
        assert!(secure_resume(&mut launch).is_ok());
        assert_eq!(launch.events, ["assign", "token", "resume"]);
    }

    #[test]
    fn every_post_create_failure_terminates_and_waits_without_resuming() {
        for (failure, expected) in [
            (Failure::Assign, &["assign", "terminate", "wait"][..]),
            (Failure::TokenError, &["assign", "token", "terminate", "wait"]),
            (Failure::TokenMismatch, &["assign", "token", "terminate", "wait"]),
            (Failure::Resume, &["assign", "token", "resume", "terminate", "wait"]),
        ] {
            let mut launch = MockLaunch { failure, events: Vec::new() };
            assert!(secure_resume(&mut launch).is_err());
            assert_eq!(launch.events, expected);
        }
    }

    #[test]
    fn command_line_escapes_quotes_and_trailing_backslashes() {
        let arguments = [OsString::from(r#"a\"b"#), OsString::from(r"path\")];
        let line = command_line(OsStr::new(r"C:\worker.exe"), &arguments).unwrap();
        assert_eq!(*line.last().unwrap(), 0);
        let line = String::from_utf16(&line[..line.len() - 1]).unwrap();
        assert_eq!(line, r#""C:\worker.exe" "a\\\"b" "path\\""#);
    }
}
