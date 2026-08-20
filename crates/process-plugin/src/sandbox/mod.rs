use crate::{PluginError, PluginErrorCode, RuntimePolicy, ValidatedPlugin};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

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
    command.env("INTO_MARKDOWN_PLUGIN_PROTOCOL", "process-v1");
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

    pub(crate) fn try_wait(&mut self) -> Result<Option<bool>, ()> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => {
                child.try_wait().map(|value| value.map(|status| status.success())).map_err(|_| ())
            }
            #[cfg(windows)]
            Self::Windows(child) => child.try_wait(),
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

impl Drop for SandboxChild {
    fn drop(&mut self) {
        // A failed status query is not proof of exit. Fail closed by attempting tree termination.
        if !matches!(self.try_wait(), Ok(Some(_))) {
            self.terminate();
        }
    }
}
