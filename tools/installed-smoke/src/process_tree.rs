//! Platform process-tree ownership for bounded smoke subprocesses.

#![allow(unsafe_code)] // Narrow OS process primitives have no safe std equivalents.

use std::process::{Child, Command};

#[cfg(unix)]
pub(crate) struct ProcessTree {
    group: i32,
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // Linux can fail while enabling subreaping.
pub(crate) fn configure(command: &mut Command) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    #[cfg(target_os = "linux")]
    // Make killed grandchildren reparent to the smoke runner, not to a
    // container PID 1 that may never reap them.
    // SAFETY: PR_SET_CHILD_SUBREAPER changes only the calling process's child
    // reparenting policy and is idempotent.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        return Err(format!(
            "cannot establish subprocess reaper: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `setpgid` is async-signal-safe and only assigns the child to a
    // fresh process group before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
        });
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn own(child: &Child) -> Result<ProcessTree, String> {
    let group = i32::try_from(child.id()).map_err(|_| "child process ID is invalid".to_owned())?;
    Ok(ProcessTree { group })
}

#[cfg(unix)]
impl ProcessTree {
    pub(crate) fn terminate_and_reap(&mut self, child: &mut Child) {
        // SAFETY: a negative PID addresses only the fresh group created for
        // this smoke subprocess. ESRCH is an already-terminated success case.
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
        let _ = child.wait();
        #[cfg(target_os = "linux")]
        loop {
            // Grandchildren killed with the group are now direct children due
            // to PR_SET_CHILD_SUBREAPER. Reap only this owned process group.
            // SAFETY: a negative first argument selects children in this fresh
            // process group; the status is intentionally discarded.
            let result = unsafe { libc::waitpid(-self.group, std::ptr::null_mut(), 0) };
            if result > 0 {
                continue;
            }
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
    }
}

#[cfg(windows)]
pub(crate) struct ProcessTree(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)] // Shared fallible cross-platform boundary.
pub(crate) fn configure(command: &mut Command) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    };
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_SUSPENDED);
    Ok(())
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
    pub(crate) fn terminate_and_reap(&mut self, child: &mut Child) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: the owned job handle remains live for this call.
        unsafe {
            TerminateJobObject(self.0.as_raw_handle(), 1);
        }
        let _ = child.wait();
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) struct ProcessTree;

#[cfg(not(any(unix, windows)))]
pub(crate) fn configure(_: &mut Command) -> Result<(), String> {
    Err("process-tree ownership is unsupported on this platform".into())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn own(_: &Child) -> Result<ProcessTree, String> {
    Err("process-tree ownership is unsupported on this platform".into())
}

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    pub(crate) fn terminate_and_reap(&mut self, _: &mut Child) {}
}
