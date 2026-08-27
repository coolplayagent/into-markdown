use crate::{PluginError, PluginErrorCode, RuntimePolicy, ValidatedPlugin};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
pub(crate) mod windows;

pub(crate) fn working_directory(policy: &RuntimePolicy) -> Result<tempfile::TempDir, PluginError> {
    #[cfg(unix)]
    {
        let _ = policy;
        return tempfile::Builder::new().prefix("into-md-plugin-").tempdir().map_err(|_| {
            PluginError::new(PluginErrorCode::Launch, "private working directory unavailable")
        });
    }
    #[cfg(windows)]
    {
        return windows::working_directory(policy);
    }
    #[allow(unreachable_code)]
    Err(PluginError::new(PluginErrorCode::SandboxUnavailable, "unsupported target"))
}

pub(crate) fn authorize_request_source(
    policy: &RuntimePolicy,
    path: &std::path::Path,
) -> Result<(), PluginError> {
    #[cfg(windows)]
    {
        windows::authorize_request_source(&policy.windows, path)
    }
    #[cfg(not(windows))]
    {
        let _ = (policy, path);
        Ok(())
    }
}

pub(crate) fn spawn(
    plugin: &ValidatedPlugin,
    policy: &RuntimePolicy,
    directory: &std::path::Path,
) -> Result<SandboxChild, PluginError> {
    let mut command = Command::new(&plugin.executable);
    command
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (name, value) in &policy.environment {
        command.env(name, value);
    }
    #[cfg(unix)]
    command.env("TMPDIR", directory).env("INTO_MARKDOWN_PRIVATE_TEMP", directory);
    command.env("INTO_MARKDOWN_PLUGIN_PROTOCOL", "process-v1");
    #[cfg(target_os = "macos")]
    if policy.macos_compatibility_child {
        command.env("INTO_MARKDOWN_INHERITED_SANDBOX", "process-v1");
    }
    #[cfg(unix)]
    unix::prepare(&mut command, plugin, policy, directory)?;
    #[cfg(windows)]
    return windows::spawn(command, plugin, policy, directory);
    #[cfg(unix)]
    return command.spawn().map(SandboxChild::Unix).map_err(|error| {
        PluginError::new(
            PluginErrorCode::Launch,
            format!("plugin launch failed (os={:?})", error.raw_os_error()),
        )
    });
    #[allow(unreachable_code)]
    Err(PluginError::new(PluginErrorCode::SandboxUnavailable, "unsupported target"))
}

pub(crate) enum SandboxChild {
    #[cfg(unix)]
    Unix(std::process::Child),
    #[cfg(windows)]
    Windows(windows::Child),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChildExit {
    Success,
    Failure,
    #[cfg(unix)]
    Signaled(i32),
}

impl ChildExit {
    pub(crate) fn success(self) -> bool {
        matches!(self, Self::Success)
    }
}

impl SandboxChild {
    pub(crate) fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => child.stdin.take().map(|value| Box::new(value) as _),
            #[cfg(windows)]
            Self::Windows(child) => child.take_stdin().map(|value| Box::new(value) as _),
        }
    }

    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => child.stdout.take().map(|value| Box::new(value) as _),
            #[cfg(windows)]
            Self::Windows(child) => child.take_stdout().map(|value| Box::new(value) as _),
        }
    }

    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => child.stderr.take().map(|value| Box::new(value) as _),
            #[cfg(windows)]
            Self::Windows(child) => child.take_stderr().map(|value| Box::new(value) as _),
        }
    }

    pub(crate) fn try_wait(&mut self) -> Result<Option<ChildExit>, ()> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => {
                use std::os::unix::process::ExitStatusExt as _;

                child
                    .try_wait()
                    .map(|value| {
                        value.map(|status| {
                            if status.success() {
                                ChildExit::Success
                            } else if let Some(signal) = status.signal() {
                                ChildExit::Signaled(signal)
                            } else {
                                ChildExit::Failure
                            }
                        })
                    })
                    .map_err(|_| ())
            }
            #[cfg(windows)]
            Self::Windows(child) => child.try_wait().map(|value| {
                value.map(|success| if success { ChildExit::Success } else { ChildExit::Failure })
            }),
        }
    }

    // The cross-platform worker loop intentionally has one fallible physical-memory query
    // contract. Linux reads the kernel-owned per-PID high-water mark while macOS uses the
    // platform physical-footprint API; Windows enforces the same value in a Job Object.
    #[cfg_attr(windows, allow(clippy::unused_self, clippy::unnecessary_wraps))]
    pub(crate) fn memory_exceeded(&self, limit: u64) -> Result<bool, ()> {
        #[cfg(target_os = "linux")]
        {
            let Self::Unix(child) = self;
            return match std::fs::read_to_string(format!("/proc/{}/status", child.id())) {
                Ok(status) => linux_peak_resident_bytes(&status).map(|bytes| bytes > limit),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(()),
            };
        }
        #[cfg(target_os = "macos")]
        {
            let Self::Unix(child) = self;
            let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
            // SAFETY: the owned child PID is live while borrowed and the V2
            // output buffer has the exact layout requested by the flavor.
            let result = unsafe {
                libc::proc_pid_rusage(
                    i32::try_from(child.id()).map_err(|_| ())?,
                    libc::RUSAGE_INFO_V2,
                    (&raw mut usage).cast(),
                )
            };
            if result == 0 {
                return Ok(usage.ri_phys_footprint.max(usage.ri_resident_size) > limit);
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return Ok(false);
            }
            return Err(());
        }
        #[cfg(windows)]
        {
            let _ = limit;
            Ok(false)
        }
    }

    pub(crate) fn terminate(&mut self) {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => {
                // SAFETY: `process_group(0)` created a group whose ID is the child PID; a negative
                // PID addresses precisely that group. SIGKILL requires no pointer validity.
                unsafe {
                    let _ = libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
            }
            #[cfg(windows)]
            Self::Windows(child) => child.terminate(),
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_peak_resident_bytes(status: &str) -> Result<u64, ()> {
    let mut maximum_kibibytes = None;
    for line in status.lines() {
        let Some(value) = line.strip_prefix("VmRSS:").or_else(|| line.strip_prefix("VmHWM:"))
        else {
            continue;
        };
        let mut fields = value.split_ascii_whitespace();
        let kibibytes = fields.next().ok_or(())?.parse::<u64>().map_err(|_| ())?;
        if fields.next() != Some("kB") || fields.next().is_some() {
            return Err(());
        }
        maximum_kibibytes =
            Some(maximum_kibibytes.map_or(kibibytes, |current: u64| current.max(kibibytes)));
    }
    maximum_kibibytes.ok_or(())?.checked_mul(1024).ok_or(())
}

#[cfg(all(test, target_os = "linux"))]
mod linux_memory_tests {
    use super::linux_peak_resident_bytes;

    #[test]
    fn physical_memory_parser_uses_the_peak_and_rejects_ambiguous_units() {
        let status = "Name:\tfixture\nVmHWM:\t2048 kB\nVmRSS:\t1024 kB\n";
        assert_eq!(linux_peak_resident_bytes(status), Ok(2 * 1024 * 1024));
        assert!(linux_peak_resident_bytes("VmRSS: 1 MB\n").is_err());
        assert!(linux_peak_resident_bytes("Name: fixture\n").is_err());
    }
}

impl Drop for SandboxChild {
    fn drop(&mut self) {
        // A failed status query is not proof of exit. Fail closed by attempting tree termination.
        if !matches!(self.try_wait(), Ok(Some(_))) {
            self.terminate();
        }
    }
}
