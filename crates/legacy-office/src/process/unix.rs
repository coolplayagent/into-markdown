use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

pub(super) struct Child {
    pid: libc::pid_t,
    stdin: Option<File>,
    stdout: Option<File>,
    stderr: Option<File>,
    reaped: bool,
}

impl Child {
    pub(super) fn spawn(
        executable: &Path,
        arguments: &[OsString],
        working_directory: &Path,
    ) -> io::Result<Self> {
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
        let envp = [std::ptr::null_mut()];

        let (stdin_read, stdin_write) = pipe_cloexec()?;
        let (stdout_read, stdout_write) = pipe_cloexec()?;
        let (stderr_read, stderr_write) = pipe_cloexec()?;
        let mut actions = SpawnActions::new()?;
        actions.dup2(stdin_read.as_raw_fd(), libc::STDIN_FILENO)?;
        actions.dup2(stdout_write.as_raw_fd(), libc::STDOUT_FILENO)?;
        actions.dup2(stderr_write.as_raw_fd(), libc::STDERR_FILENO)?;
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
        Ok(Self {
            pid,
            stdin: Some(File::from(stdin_write)),
            stdout: Some(File::from(stdout_read)),
            stderr: Some(File::from(stderr_read)),
            reaped: false,
        })
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
}
