//! Bounded subprocess execution with kill-and-reap cancellation.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::process_tree;

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;

pub(crate) struct CommandSpec<'a> {
    pub program: &'a Path,
    pub arguments: &'a [String],
    pub current_dir: &'a Path,
    pub home: &'a Path,
    pub environment: BTreeMap<String, String>,
    pub stdin: &'a [u8],
    pub timeout: Duration,
    pub cancel_file: Option<&'a Path>,
}

#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) trait Executor {
    fn execute(&self, spec: CommandSpec<'_>) -> Result<CommandOutput, String>;
}

pub(crate) struct RealExecutor {
    base_environment: BTreeMap<String, String>,
}

impl RealExecutor {
    pub(crate) fn new(base_environment: BTreeMap<String, String>) -> Self {
        let base_environment = base_environment
            .into_iter()
            .filter(|(name, _)| matches!(name.as_str(), "SystemRoot" | "WINDIR"))
            .collect();
        Self { base_environment }
    }
}

impl Executor for RealExecutor {
    fn execute(&self, spec: CommandSpec<'_>) -> Result<CommandOutput, String> {
        let mut command = Command::new(spec.program);
        command
            .args(spec.arguments)
            .current_dir(spec.current_dir)
            .env_clear()
            .envs(&self.base_environment)
            .envs(spec.environment)
            .env("HOME", spec.home)
            .env("USERPROFILE", spec.home)
            .env("XDG_CONFIG_HOME", spec.home.join("xdg-config"))
            .env("XDG_DATA_HOME", spec.home.join("xdg-data"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process_tree::configure(&mut command);
        let mut child =
            command.spawn().map_err(|error| format!("cannot start process: {error}"))?;
        let mut tree = match process_tree::own(&child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if let Some(mut input) = child.stdin.take()
            && let Err(error) = input.write_all(spec.stdin)
        {
            tree.terminate();
            let _ = child.wait();
            return Err(format!("cannot write process input: {error}"));
        }
        let Some(stdout) = child.stdout.take() else {
            tree.terminate();
            let _ = child.wait();
            return Err("process stdout is unavailable".into());
        };
        let Some(stderr) = child.stderr.take() else {
            tree.terminate();
            let _ = child.wait();
            return Err("process stderr is unavailable".into());
        };
        let stdout_thread = read_bounded(stdout);
        let stderr_thread = read_bounded(stderr);
        let deadline = Instant::now() + spec.timeout;
        let status = loop {
            if spec.cancel_file.is_some_and(Path::exists) {
                tree.terminate();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err("process cancelled".into());
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    tree.terminate();
                    let _ = child.wait();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(format!("cannot poll process: {error}"));
                }
            }
            if Instant::now() >= deadline {
                tree.terminate();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err("process deadline exceeded".into());
            }
            thread::sleep(Duration::from_millis(10));
        };
        // A command that exits while leaving a grandchild holding an inherited
        // pipe must not hang the reader joins or escape the smoke run.
        tree.terminate();
        let stdout = stdout_thread.join().map_err(|_| "stdout reader panicked".to_owned())??;
        let stderr = stderr_thread.join().map_err(|_| "stderr reader panicked".to_owned())??;
        Ok(CommandOutput { exit_code: status.code(), stdout, stderr })
    }
}

fn read_bounded(
    mut input: impl Read + Send + 'static,
) -> thread::JoinHandle<Result<Vec<u8>, String>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let count = input
                .read(&mut buffer)
                .map_err(|error| format!("cannot read process output: {error}"))?;
            if count == 0 {
                return Ok(output);
            }
            if output.len().saturating_add(count) > OUTPUT_LIMIT {
                return Err("process output limit exceeded".into());
            }
            output.extend_from_slice(&buffer[..count]);
        }
    })
}

pub(crate) fn command_environment(home: &Path) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::from([
        ("TMPDIR".to_owned(), home.join("tmp").display().to_string()),
        ("TEMP".to_owned(), home.join("tmp").display().to_string()),
        ("TMP".to_owned(), home.join("tmp").display().to_string()),
    ]);
    #[cfg(windows)]
    environment.extend([
        ("APPDATA".to_owned(), home.join("appdata-roaming").display().to_string()),
        ("LOCALAPPDATA".to_owned(), home.join("appdata-local").display().to_string()),
        ("INTO_MARKDOWN_USER_DATA_HOME".to_owned(), home.join("user-data").display().to_string()),
    ]);
    environment
}

pub(crate) fn prepare_home(root: &Path, name: &str) -> Result<PathBuf, String> {
    let home = root.join(name);
    let mut directories =
        vec![home.clone(), home.join("tmp"), home.join("xdg-config"), home.join("xdg-data")];
    #[cfg(windows)]
    directories.extend([home.join("appdata-roaming"), home.join("appdata-local")]);
    for directory in directories {
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot prepare isolated home: {error}"))?;
    }
    #[cfg(windows)]
    {
        let user_data = home.join("user-data");
        if user_data.exists() {
            into_markdown_process_plugin::verify_windows_plugin_store_path(&user_data)
                .map_err(|error| format!("cannot verify isolated user data: {error}"))?;
        } else {
            into_markdown_process_plugin::create_windows_plugin_store_directory(&user_data)
                .map_err(|error| format!("cannot prepare isolated user data: {error}"))?;
        }
    }
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_command_environment_uses_isolated_appdata() {
        let home = Path::new(r"C:\isolated");
        let environment = command_environment(home);
        assert_eq!(
            environment.get("APPDATA").map(String::as_str),
            Some(r"C:\isolated\appdata-roaming")
        );
        assert_eq!(
            environment.get("LOCALAPPDATA").map(String::as_str),
            Some(r"C:\isolated\appdata-local")
        );
        assert_eq!(
            environment.get("INTO_MARKDOWN_USER_DATA_HOME").map(String::as_str),
            Some(r"C:\isolated\user-data")
        );
    }

    #[test]
    fn cancellation_kills_a_started_process() {
        let temporary = tempfile::tempdir().unwrap();
        let cancel = temporary.path().join("cancel");
        std::fs::write(&cancel, b"cancel").unwrap();
        let executable = std::env::current_exe().unwrap();
        let arguments = vec!["--help".into()];
        let error = RealExecutor::new(BTreeMap::new())
            .execute(CommandSpec {
                program: &executable,
                arguments: &arguments,
                current_dir: temporary.path(),
                home: temporary.path(),
                environment: BTreeMap::new(),
                stdin: &[],
                timeout: Duration::from_secs(2),
                cancel_file: Some(&cancel),
            })
            .unwrap_err();
        assert_eq!(error, "process cancelled");
    }

    #[test]
    fn child_does_not_inherit_development_path_or_flags() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let arguments = vec![
            "--exact".into(),
            "process::tests::environment_helper".into(),
            "--nocapture".into(),
        ];
        let output = RealExecutor::new(BTreeMap::from([
            ("PATH".into(), "/developer/bin".into()),
            ("HOME".into(), "/malicious/user-home".into()),
            ("RUSTFLAGS".into(), "--cfg malicious".into()),
        ]))
        .execute(CommandSpec {
            program: &executable,
            arguments: &arguments,
            current_dir: temporary.path(),
            home: temporary.path(),
            environment: BTreeMap::from([("SMOKE_ENV_HELPER".into(), "1".into())]),
            stdin: &[],
            timeout: Duration::from_secs(5),
            cancel_file: None,
        })
        .unwrap();
        assert_eq!(output.exit_code, Some(0));
    }

    #[test]
    fn deadline_kills_and_reaps_child() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let arguments = helper_arguments();
        let error = RealExecutor::new(BTreeMap::new())
            .execute(CommandSpec {
                program: &executable,
                arguments: &arguments,
                current_dir: temporary.path(),
                home: temporary.path(),
                environment: BTreeMap::from([("SMOKE_SLEEP_HELPER".into(), "1".into())]),
                stdin: &[],
                timeout: Duration::from_millis(30),
                cancel_file: None,
            })
            .unwrap_err();
        assert_eq!(error, "process deadline exceeded");
    }

    #[cfg(unix)]
    #[test]
    fn completed_parent_cannot_leave_pipe_holding_grandchild() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("spawn.sh");
        std::fs::write(&script, b"#!/bin/sh\n(sleep 30) &\nprintf done\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let output = RealExecutor::new(BTreeMap::new())
            .execute(CommandSpec {
                program: &script,
                arguments: &[],
                current_dir: temporary.path(),
                home: temporary.path(),
                environment: BTreeMap::new(),
                stdin: &[],
                timeout: Duration::from_secs(2),
                cancel_file: None,
            })
            .unwrap();
        assert_eq!(output.stdout, b"done");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_the_owned_grandchild_group() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("tree.sh");
        let pid_file = temporary.path().join("grandchild.pid");
        let cancel = temporary.path().join("cancel");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n", pid_file.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let pid_for_cancel = pid_file.clone();
        let cancel_for_thread = cancel.clone();
        let trigger = thread::spawn(move || {
            for _ in 0..200 {
                if pid_for_cancel.is_file() {
                    std::fs::write(cancel_for_thread, b"cancel").unwrap();
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("grandchild PID was not published");
        });
        let error = RealExecutor::new(BTreeMap::new())
            .execute(CommandSpec {
                program: &script,
                arguments: &[],
                current_dir: temporary.path(),
                home: temporary.path(),
                environment: BTreeMap::new(),
                stdin: &[],
                timeout: Duration::from_secs(3),
                cancel_file: Some(&cancel),
            })
            .unwrap_err();
        trigger.join().unwrap();
        assert_eq!(error, "process cancelled");
        let pid = std::fs::read_to_string(pid_file).unwrap();
        let mut absent = false;
        for _ in 0..100 {
            absent = !std::process::Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if absent {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(absent, "grandchild remained after cancellation");
    }

    #[test]
    fn excessive_process_output_fails_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let arguments = helper_arguments();
        let error = RealExecutor::new(BTreeMap::new())
            .execute(CommandSpec {
                program: &executable,
                arguments: &arguments,
                current_dir: temporary.path(),
                home: temporary.path(),
                environment: BTreeMap::from([("SMOKE_OUTPUT_HELPER".into(), "1".into())]),
                stdin: &[],
                timeout: Duration::from_secs(5),
                cancel_file: None,
            })
            .unwrap_err();
        assert_eq!(error, "process output limit exceeded");
    }

    fn helper_arguments() -> Vec<String> {
        vec!["--exact".into(), "process::tests::environment_helper".into(), "--nocapture".into()]
    }

    #[test]
    fn environment_helper() {
        if std::env::var_os("SMOKE_ENV_HELPER").is_none() {
            if std::env::var_os("SMOKE_SLEEP_HELPER").is_some() {
                std::thread::sleep(Duration::from_secs(2));
            }
            if std::env::var_os("SMOKE_OUTPUT_HELPER").is_some() {
                print!("{}", "x".repeat(OUTPUT_LIMIT + 1));
            }
            return;
        }
        assert!(std::env::var_os("PATH").is_none());
        assert!(std::env::var_os("RUSTFLAGS").is_none());
        assert!(std::env::var_os("CARGO_HOME").is_none());
        assert_ne!(std::env::var("HOME").unwrap(), "/malicious/user-home");
    }
}
