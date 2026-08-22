use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

pub(super) fn map_spawn_error(error: &io::Error) -> into_markdown_core::ConversionError {
    let detail = match error.raw_os_error() {
        Some(libc::E2BIG) => "workerLaunchArguments",
        Some(libc::EACCES) => "workerLaunchPermission",
        Some(libc::EMFILE | libc::ENFILE | libc::EBADF) => "workerLaunchDescriptors",
        Some(libc::EINVAL) => "workerLaunchInvalid",
        Some(libc::ENOENT) => "workerLaunchMissing",
        Some(libc::ENOEXEC) => "workerLaunchFormat",
        Some(libc::ENOMEM) => "workerLaunchMemory",
        Some(libc::ENOTSUP) => "workerLaunchUnsupported",
        _ => "workerLaunch",
    };
    super::unavailable(detail)
}

pub(super) fn ensure_descriptor_budget(
    runtime_files: usize,
) -> Result<(), into_markdown_core::ConversionError> {
    let mut limit = MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit initializes the live output object on success.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(super::unavailable("workerDescriptorBudget"));
    }
    // SAFETY: the successful call initialized every field.
    let limit = unsafe { limit.assume_init() }.rlim_cur;
    let open = open_descriptor_count(limit)?;
    if !descriptor_budget_fits(runtime_files, open, limit) {
        return Err(super::unavailable("workerDescriptorBudget"));
    }
    Ok(())
}

fn open_descriptor_count(
    limit: libc::rlim_t,
) -> Result<usize, into_markdown_core::ConversionError> {
    let maximum = usize::try_from(limit).unwrap_or(usize::MAX).min(4_096);
    let mut open = 0_usize;
    for descriptor in 0..maximum {
        // F_GETFD does not open a path, so the check remains available inside
        // Landlock and Seatbelt profiles that deliberately hide /proc and
        // /dev/fd from capability processes.
        let result =
            unsafe { libc::fcntl(i32::try_from(descriptor).unwrap_or(i32::MAX), libc::F_GETFD) };
        if result >= 0 {
            open = open.saturating_add(1);
            continue;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EBADF) {
            return Err(super::unavailable("workerDescriptorBudget"));
        }
    }
    Ok(open)
}

pub(super) fn descriptor_budget_fits(
    runtime_files: usize,
    open: usize,
    limit: libc::rlim_t,
) -> bool {
    let required = runtime_files.checked_add(open).and_then(|value| value.checked_add(32));
    required.is_some_and(|value| value <= usize::try_from(limit).unwrap_or(usize::MAX))
}

pub(super) struct Child {
    pid: libc::pid_t,
    stdin: Option<File>,
    stdout: Option<File>,
    stderr: Option<File>,
    reaped: bool,
}

pub(super) struct Prepared {
    stdin_read: OwnedFd,
    stdin_write: OwnedFd,
    stdout_read: OwnedFd,
    stdout_write: OwnedFd,
    stderr_read: OwnedFd,
    stderr_write: OwnedFd,
}

impl Prepared {
    pub(super) fn new() -> io::Result<Self> {
        let (stdin_read, stdin_write) = pipe_cloexec()?;
        let (stdout_read, stdout_write) = pipe_cloexec()?;
        let (stderr_read, stderr_write) = pipe_cloexec()?;
        Ok(Self { stdin_read, stdin_write, stdout_read, stdout_write, stderr_read, stderr_write })
    }

    pub(super) fn spawn(
        self,
        executable: &Path,
        arguments: &[OsString],
        working_directory: &Path,
    ) -> io::Result<Child> {
        spawn_prepared(self, executable, arguments, working_directory)
    }
}

impl Child {
    #[cfg(test)]
    pub(super) fn spawn(
        executable: &Path,
        arguments: &[OsString],
        working_directory: &Path,
    ) -> io::Result<Self> {
        Prepared::new()?.spawn(executable, arguments, working_directory)
    }

    pub(super) fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }

    pub(super) fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }

    pub(super) fn take_stderr(&mut self) -> Option<File> {
        self.stderr.take()
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if self.reaped {
            return Err(io::Error::other("worker already reaped"));
        }
        let mut status = 0;
        // SAFETY: `pid` is the exact child returned by posix_spawn and status
        // is a live output word.
        let result = unsafe { libc::waitpid(self.pid, &raw mut status, libc::WNOHANG) };
        if result == 0 {
            Ok(None)
        } else if result == self.pid {
            self.reaped = true;
            Ok(Some(status))
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn wait(&mut self) -> io::Result<i32> {
        if self.reaped {
            return Err(io::Error::other("worker already reaped"));
        }
        loop {
            let mut status = 0;
            // SAFETY: same child ownership and live output word as try_wait.
            let result = unsafe { libc::waitpid(self.pid, &raw mut status, 0) };
            if result == self.pid {
                self.reaped = true;
                return Ok(status);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    pub(super) fn terminate_group(&self) {
        if !self.reaped {
            // SAFETY: the child was created as its own process group; negating
            // its positive pid addresses only that worker group.
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn group_usage(&self) -> io::Result<GroupUsage> {
        // SAFETY: a null buffer asks libproc for a conservative PID capacity.
        let capacity = unsafe { libc::proc_listpgrppids(self.pid, std::ptr::null_mut(), 0) };
        if capacity <= 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pids = vec![0_i32; usize::try_from(capacity).map_err(io::Error::other)? + 16];
        let bytes =
            i32::try_from(std::mem::size_of_val(pids.as_slice())).map_err(io::Error::other)?;
        // SAFETY: the PID buffer is writable for exactly `bytes` bytes.
        let count = unsafe { libc::proc_listpgrppids(self.pid, pids.as_mut_ptr().cast(), bytes) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        let mut resident_bytes = 0_u64;
        let mut processes = 0_u32;
        for pid in pids.into_iter().take(usize::try_from(count).map_err(io::Error::other)?) {
            let mut usage = MaybeUninit::<RusageInfoV2>::zeroed();
            // SAFETY: every positive PID came from libproc and the output has
            // the exact versioned Darwin rusage layout.
            let result = unsafe { proc_pid_rusage(pid, 2, usage.as_mut_ptr().cast()) };
            if result == 0 {
                processes = processes
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("process count overflow"))?;
                // SAFETY: successful proc_pid_rusage initialized the structure.
                let resident = unsafe { usage.assume_init() }.ri_phys_footprint;
                resident_bytes = resident_bytes
                    .checked_add(resident)
                    .ok_or_else(|| io::Error::other("resident byte count overflow"))?;
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        Ok(GroupUsage { processes, resident_bytes })
    }
}

#[cfg(target_os = "macos")]
pub(super) struct GroupUsage {
    pub(super) processes: u32,
    pub(super) resident_bytes: u64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[allow(clippy::struct_field_names)] // Darwin publishes these exact rusage_info_v2 field names.
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

fn spawn_prepared(
    prepared: Prepared,
    executable: &Path,
    arguments: &[OsString],
    working_directory: &Path,
) -> io::Result<Child> {
    let executable = cstring(executable.as_os_str())?;
    let working_directory = cstring(working_directory.as_os_str())?;
    let mut owned_arguments = Vec::new();
    owned_arguments.try_reserve_exact(arguments.len() + 1).map_err(io::Error::other)?;
    owned_arguments.push(executable.clone());
    for argument in arguments {
        owned_arguments.push(cstring(argument)?);
    }
    let mut argv =
        owned_arguments.iter().map(|value| value.as_ptr().cast_mut()).collect::<Vec<_>>();
    argv.push(std::ptr::null_mut());
    #[cfg(target_os = "macos")]
    let owned_environment = [
        CString::new(format!("HOME={}/requests/home", working_directory.to_string_lossy()))
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
        CString::new(format!("TMPDIR={}/requests", working_directory.to_string_lossy()))
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
        CString::new("LANG=en_US.UTF-8").expect("fixed environment has no NUL"),
        CString::new("LC_ALL=C").expect("fixed environment has no NUL"),
        CString::new("SAL_USE_VCLPLUGIN=svp").expect("fixed environment has no NUL"),
        CString::new("SAL_LOK_OPTIONS=unipoll").expect("fixed environment has no NUL"),
        CString::new("__CF_USER_TEXT_ENCODING=0x1F5:0x0:0x0")
            .expect("fixed environment has no NUL"),
    ];
    #[cfg(target_os = "macos")]
    let mut envp =
        owned_environment.iter().map(|value| value.as_ptr().cast_mut()).collect::<Vec<_>>();
    #[cfg(target_os = "macos")]
    envp.push(std::ptr::null_mut());
    #[cfg(not(target_os = "macos"))]
    let envp = [std::ptr::null_mut()];

    let Prepared { stdin_read, stdin_write, stdout_read, stdout_write, stderr_read, stderr_write } =
        prepared;
    let mut actions = SpawnActions::new()?;
    #[cfg(target_os = "macos")]
    for descriptor in [stdin_read.as_raw_fd(), stdout_write.as_raw_fd(), stderr_write.as_raw_fd()] {
        actions.inherit(descriptor)?;
    }
    actions.dup2(stdin_read.as_raw_fd(), libc::STDIN_FILENO)?;
    actions.dup2(stdout_write.as_raw_fd(), libc::STDOUT_FILENO)?;
    actions.dup2(stderr_write.as_raw_fd(), libc::STDERR_FILENO)?;
    #[cfg(target_os = "macos")]
    for descriptor in [stdin_read.as_raw_fd(), stdout_write.as_raw_fd(), stderr_write.as_raw_fd()] {
        actions.close(descriptor)?;
    }
    actions.chdir(&working_directory)?;
    #[cfg(target_os = "linux")]
    actions.close_from(3)?;
    let attributes = SpawnAttributes::new()?;
    let mut pid = 0;
    // SAFETY: all C strings, pointer arrays, initialized spawn objects, and
    // pipe descriptors remain live for the complete call. `envp` is an
    // intentionally empty environment and argv is NUL terminated.
    let result = unsafe {
        libc::posix_spawn(
            &raw mut pid,
            executable.as_ptr(),
            actions.as_ptr(),
            attributes.as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    drop(stdin_read);
    drop(stdout_write);
    drop(stderr_write);
    Ok(Child {
        pid,
        stdin: Some(File::from(stdin_write)),
        stdout: Some(File::from(stdout_read)),
        stderr: Some(File::from(stderr_read)),
        reaped: false,
    })
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    #[cfg(target_os = "linux")]
    // SAFETY: descriptors is a live two-element output array.
    let result = unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(target_os = "macos")]
    // SAFETY: descriptors is a live two-element output array.
    let result = unsafe { libc::pipe(descriptors.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe/pipe2 returned two uniquely owned descriptors.
    let read = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    // SAFETY: same as above for the write end.
    let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    #[cfg(target_os = "macos")]
    for descriptor in [read.as_raw_fd(), write.as_raw_fd()] {
        // SAFETY: these are live owned descriptors and F_SETFD changes only
        // the inheritance flag.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

struct SpawnActions {
    value: libc::posix_spawn_file_actions_t,
}

impl SpawnActions {
    fn new() -> io::Result<Self> {
        let mut value = MaybeUninit::uninit();
        // SAFETY: the C function initializes the out-parameter on success.
        let result = unsafe { libc::posix_spawn_file_actions_init(value.as_mut_ptr()) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        // SAFETY: successful init initialized the complete value.
        Ok(Self { value: unsafe { value.assume_init() } })
    }

    fn dup2(&mut self, source: i32, destination: i32) -> io::Result<()> {
        // SAFETY: the actions object is initialized and descriptors are scalar.
        cvt(unsafe {
            libc::posix_spawn_file_actions_adddup2(&raw mut self.value, source, destination)
        })
    }

    #[cfg(target_os = "macos")]
    fn inherit(&mut self, descriptor: i32) -> io::Result<()> {
        // SAFETY: the initialized action object copies this live descriptor.
        cvt(unsafe { posix_spawn_file_actions_addinherit_np(&raw mut self.value, descriptor) })
    }

    #[cfg(target_os = "macos")]
    fn close(&mut self, descriptor: i32) -> io::Result<()> {
        // SAFETY: the initialized action object records a scalar descriptor.
        cvt(unsafe { libc::posix_spawn_file_actions_addclose(&raw mut self.value, descriptor) })
    }

    fn chdir(&mut self, directory: &std::ffi::CStr) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        // SAFETY: initialized actions and a live NUL-terminated path. glibc
        // copies the path into the action object before this call returns.
        let result = unsafe {
            libc::posix_spawn_file_actions_addchdir_np(&raw mut self.value, directory.as_ptr())
        };
        #[cfg(target_os = "macos")]
        // SAFETY: same file-action and path contract as the Linux declaration.
        let result = unsafe {
            posix_spawn_file_actions_addchdir_np(&raw mut self.value, directory.as_ptr())
        };
        cvt(result)
    }

    #[cfg(target_os = "linux")]
    fn close_from(&mut self, descriptor: i32) -> io::Result<()> {
        // SAFETY: initialized actions and a non-negative descriptor follow the
        // GNU closefrom action contract. It cannot close libc's hidden exec
        // status descriptor because posix_spawn owns action execution.
        cvt(unsafe {
            libc::posix_spawn_file_actions_addclosefrom_np(&raw mut self.value, descriptor)
        })
    }

    fn as_ptr(&self) -> *const libc::posix_spawn_file_actions_t {
        &raw const self.value
    }
}

impl Drop for SpawnActions {
    fn drop(&mut self) {
        // SAFETY: value was initialized and is destroyed exactly once.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&raw mut self.value);
        }
    }
}

struct SpawnAttributes {
    value: libc::posix_spawnattr_t,
}

impl SpawnAttributes {
    fn new() -> io::Result<Self> {
        let mut value = MaybeUninit::uninit();
        // SAFETY: the C function initializes the out-parameter on success.
        let result = unsafe { libc::posix_spawnattr_init(value.as_mut_ptr()) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        // SAFETY: successful init initialized the complete value.
        let mut value = unsafe { value.assume_init() };
        cvt(unsafe { libc::posix_spawnattr_setpgroup(&raw mut value, 0) })?;
        #[cfg(target_os = "linux")]
        let flags = libc::POSIX_SPAWN_SETPGROUP;
        #[cfg(target_os = "macos")]
        let flags = libc::POSIX_SPAWN_SETPGROUP | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
        let flags =
            i16::try_from(flags).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        if let Err(error) = cvt(unsafe { libc::posix_spawnattr_setflags(&raw mut value, flags) }) {
            // SAFETY: initialized value must be destroyed on this error path.
            unsafe { libc::posix_spawnattr_destroy(&raw mut value) };
            return Err(error);
        }
        Ok(Self { value })
    }

    fn as_ptr(&self) -> *const libc::posix_spawnattr_t {
        &raw const self.value
    }
}

impl Drop for SpawnAttributes {
    fn drop(&mut self) {
        // SAFETY: value was initialized and is destroyed exactly once.
        unsafe {
            libc::posix_spawnattr_destroy(&raw mut self.value);
        }
    }
}

fn cvt(result: i32) -> io::Result<()> {
    if result == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(result)) }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        directory: *const libc::c_char,
    ) -> libc::c_int;
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        descriptor: libc::c_int,
    ) -> libc::c_int;
}
