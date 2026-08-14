//! Platform process-tree ownership for bounded smoke subprocesses.

#![allow(unsafe_code)] // Narrow OS process primitives have no safe std equivalents.

use std::process::{Child, Command};

#[cfg(unix)]
pub(crate) struct ProcessTree {
    group: i32,
}

#[cfg(unix)]
pub(crate) fn configure(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setpgid` is async-signal-safe and only assigns the child to a
    // fresh process group before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
        });
    }
}

#[cfg(unix)]
pub(crate) fn own(child: &Child) -> Result<ProcessTree, String> {
    let group = i32::try_from(child.id()).map_err(|_| "child process ID is invalid".to_owned())?;
    Ok(ProcessTree { group })
}

#[cfg(unix)]
impl ProcessTree {
    pub(crate) fn terminate(&mut self) {
        // SAFETY: a negative PID addresses only the fresh group created for
        // this smoke subprocess. ESRCH is an already-terminated success case.
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
pub(crate) struct ProcessTree(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
pub(crate) fn configure(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    };
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

#[cfg(windows)]
pub(crate) fn own(child: &Child) -> Result<ProcessTree, String> {
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: *mut core::ffi::c_void) -> i32;
    }

    // SAFETY: null security and name create a private job object.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err("cannot create subprocess job object".into());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: all handles and structures are live with the documented layout.
    let configured = unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap(),
        ) != 0
            && AssignProcessToJobObject(raw, child.as_raw_handle()) != 0
    };
    if !configured {
        // SAFETY: `raw` is a live unowned job handle.
        unsafe { CloseHandle(raw) };
        return Err("cannot assign subprocess job object".into());
    }
    // SAFETY: the child was created suspended and is resumed only after job
    // assignment, closing the process-tree escape race.
    if unsafe { NtResumeProcess(child.as_raw_handle()) } != 0 {
        // Closing a configured kill-on-close job terminates the suspended child.
        unsafe { CloseHandle(raw) };
        return Err("cannot resume subprocess job object".into());
    }
    // SAFETY: ownership of the unique job handle transfers here.
    Ok(ProcessTree(unsafe { OwnedHandle::from_raw_handle(raw) }))
}

#[cfg(windows)]
impl ProcessTree {
    pub(crate) fn terminate(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: the owned job handle remains live for this call.
        unsafe {
            TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) struct ProcessTree;

#[cfg(not(any(unix, windows)))]
pub(crate) fn configure(_: &mut Command) {}

#[cfg(not(any(unix, windows)))]
pub(crate) fn own(_: &Child) -> Result<ProcessTree, String> {
    Err("process-tree ownership is unsupported on this platform".into())
}

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    pub(crate) fn terminate(&mut self) {}
}
