//! Bounded subprocess execution with kill-and-reap cancellation.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
        let mut child =
            command.spawn().map_err(|error| format!("cannot start process: {error}"))?;
        if let Some(mut input) = child.stdin.take()
            && let Err(error) = input.write_all(spec.stdin)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("cannot write process input: {error}"));
        }
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("process stdout is unavailable".into());
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("process stderr is unavailable".into());
        };
        let stdout_thread = read_bounded(stdout);
        let stderr_thread = read_bounded(stderr);
        let deadline = Instant::now() + spec.timeout;
        let status = loop {
            if spec.cancel_file.is_some_and(Path::exists) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err("process cancelled".into());
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(format!("cannot poll process: {error}"));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err("process deadline exceeded".into());
            }
            thread::sleep(Duration::from_millis(10));
        };
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
    BTreeMap::from([
        ("TMPDIR".to_owned(), home.join("tmp").display().to_string()),
        ("TEMP".to_owned(), home.join("tmp").display().to_string()),
        ("TMP".to_owned(), home.join("tmp").display().to_string()),
    ])
}

pub(crate) fn prepare_home(root: &Path, name: &str) -> Result<PathBuf, String> {
    let home = root.join(name);
    for directory in
        [home.clone(), home.join("tmp"), home.join("xdg-config"), home.join("xdg-data")]
    {
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot prepare isolated home: {error}"))?;
    }
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;

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
