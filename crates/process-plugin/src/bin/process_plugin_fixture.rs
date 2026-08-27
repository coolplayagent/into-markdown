//! Cross-platform adversarial executable for the real process sandbox tests.

use into_markdown_core::{
    Asset, AssetId, ConversionResult, Diagnostic, DiagnosticSeverity, Document, DtoJsonStyle,
    Provenance, ProvenanceKind, ResultDto, SourceLocator,
};
use into_markdown_process_plugin::worker::{self, WorkerError};
use std::io::Write as _;
use std::time::Duration;

// This fixture intentionally keeps the complete protocol branch matrix visible in one process.
#[allow(clippy::too_many_lines)]
fn main() -> std::io::Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--child-probe")) {
        print!("child-ok");
        return Ok(());
    }
    if std::env::var("PROCESS_PLUGIN_FIXTURE_MODE").as_deref() == Ok("stall-request") {
        return stall_after_hello();
    }
    if std::env::var("PROCESS_PLUGIN_FIXTURE_MODE").as_deref() == Ok("isolate-stdout") {
        return worker::serve_with_isolated_stdout(
            "fixture.process-v1",
            16 * 1024 * 1024,
            |_request, _events, _cancellation| {
                print!("native-library-noise");
                std::io::stdout()
                    .flush()
                    .map_err(|_| WorkerError::new("stdout", "stdout noise failed"))?;
                Ok(result("stdout-isolated"))
            },
        );
    }
    worker::serve("fixture.process-v1", 16 * 1024 * 1024, |request, events, cancellation| {
        let command = String::from_utf8_lossy(&request.source);
        if exit_for_raw_fixture(&command, &request.request_id) {
            unreachable!("raw fixtures always exit the process")
        }
        match command.as_ref() {
            "crash" => std::process::abort(),
            "hang" => loop {
                std::thread::sleep(Duration::from_millis(10));
            },
            "cancel" => {
                events
                    .progress("converting", Some(0), Some(1), Some("cancel-ready".into()))
                    .map_err(|_| WorkerError::new("event", "progress failed"))?;
                while !cancellation.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(WorkerError::new("cancelled", "cancelled"))
            }
            "stage-isolated" => {
                events
                    .progress("converting", Some(0), Some(1), Some("stage-ready".into()))
                    .map_err(|_| WorkerError::new("event", "progress failed"))?;
                std::thread::sleep(Duration::from_millis(500));
                Ok(result("stage-isolated"))
            }
            "frame-flood" => {
                for sequence in 1..=64 {
                    raw_json(&serde_json::json!({
                        "type":"progress", "protocol_version":1,
                        "request_id":request.request_id, "sequence":sequence,
                        "stage":"converting", "completed_units":sequence,
                        "total_units":64, "message":"flood"
                    }));
                }
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            value if value.starts_with("file:") => {
                let denied = std::fs::read(&value[5..]).is_err();
                Ok(result(if denied { "file-denied" } else { "file-leaked" }))
            }
            value if value.starts_with("network:") => {
                let denied = std::net::TcpStream::connect(&value[8..]).is_err();
                Ok(result(if denied { "network-denied" } else { "network-leaked" }))
            }
            "secret" => Ok(result(
                if std::env::var_os("PATH").is_none()
                    && std::env::var_os("AWS_SECRET_ACCESS_KEY").is_none()
                {
                    "secret-denied"
                } else {
                    "secret-leaked"
                },
            )),
            "private-temp" => {
                let private =
                    std::env::var_os("INTO_MARKDOWN_PRIVATE_TEMP").map(std::path::PathBuf::from);
                let standard = std::env::var_os(if cfg!(windows) { "TEMP" } else { "TMPDIR" })
                    .map(std::path::PathBuf::from);
                let usable = private.as_ref().is_some_and(|root| {
                    root.is_absolute()
                        && (cfg!(windows) || standard.as_ref() == Some(root))
                        && root.is_dir()
                        && std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(root.join("private-temp-probe"))
                            .and_then(|_| std::fs::remove_file(root.join("private-temp-probe")))
                            .is_ok()
                });
                Ok(result(if usable { "private-temp-ready" } else { "private-temp-invalid" }))
            }
            "child" => {
                let executable = std::env::current_exe()
                    .map_err(|_| WorkerError::new("childPrepare", "child helper is unavailable"))?;
                let helper = executable.with_file_name(if cfg!(windows) {
                    "verified-helper.exe"
                } else {
                    "verified-helper"
                });
                let child_directory = std::env::var_os("INTO_MARKDOWN_PRIVATE_TEMP")
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| {
                        WorkerError::new("childPrepare", "private child directory is unavailable")
                    })?;
                let mut command = std::process::Command::new(helper);
                command
                    .arg("--child-probe")
                    .current_dir(child_directory)
                    .env_clear()
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::process::CommandExt as _;
                    // Force fork+exec: Rocky 8's posix_spawn uses tgkill on the
                    // new child, which the inherited sandbox intentionally denies.
                    unsafe { command.pre_exec(|| Ok(())) };
                }
                let mut child = command
                    .spawn()
                    .map_err(|error| {
                        WorkerError::new(
                            "childLaunch",
                            format!("child helper launch failed: {error}"),
                        )
                    })?;
                drop(child.stdin.take());
                let output = child
                    .wait_with_output()
                    .map_err(|_| WorkerError::new("childWait", "child helper wait failed"))?;
                Ok(result(if output.status.success() && output.stdout == b"child-ok" {
                    "child-ok"
                } else {
                    "child-failed"
                }))
            }
            "ok" => {
                events
                    .progress("converting", Some(1), Some(2), Some("fixture".into()))
                    .map_err(|_| WorkerError::new("event", "progress failed"))?;
                events
                    .diagnostic(Diagnostic {
                        code: "fixtureNotice".into(),
                        severity: DiagnosticSeverity::Info,
                        message: "fixture diagnostic".into(),
                        locator: None,
                    })
                    .map_err(|_| WorkerError::new("event", "diagnostic failed"))?;
                Ok(result("ok"))
            }
            value if value.starts_with("large-ok") => Ok(result("large-ok")),
            _ => Err(WorkerError::new("unknownFixture", "unknown fixture command")),
        }
    })
}

fn exit_for_raw_fixture(command: &str, request_id: &str) -> bool {
    match command {
        "malformed" => raw_and_exit(b"\x01\x00\x00\x00{"),
        "oversize" => raw_and_exit(&(16_u32 * 1024 * 1024 + 1).to_le_bytes()),
        "bad-order" => {
            for (sequence, completed) in [(2, 1), (1, 2)] {
                raw_json(&serde_json::json!({
                    "type":"progress", "protocol_version":1,
                    "request_id":request_id, "sequence":sequence,
                    "stage":"converting", "completed_units":completed,
                    "total_units":2, "message":null
                }));
            }
            std::process::exit(0);
        }
        "invalid-result" => {
            raw_json(&serde_json::json!({
                "type":"response", "protocol_version":1,
                "request_id":request_id, "result_json":"{}"
            }));
            std::process::exit(0);
        }
        "missing-error-id" => {
            raw_json(&serde_json::json!({
                "type":"error", "protocol_version":1,
                "request_id":null, "code":"fixtureError", "message":"missing identity"
            }));
            std::process::exit(0);
        }
        "extra-after-response" => {
            let result_json =
                ResultDto::json_from_result(&result("first"), DtoJsonStyle::Compact).unwrap();
            raw_json(&serde_json::json!({
                "type":"response", "protocol_version":1,
                "request_id":request_id, "result_json":result_json
            }));
            raw_json(&serde_json::json!({
                "type":"progress", "protocol_version":1,
                "request_id":request_id, "sequence":1,
                "stage":"completed", "completed_units":1,
                "total_units":1, "message":null
            }));
            std::process::exit(0);
        }
        _ => false,
    }
}

fn stall_after_hello() -> std::io::Result<()> {
    use std::io::Read as _;
    let mut stdin = std::io::stdin().lock();
    let mut prefix = [0_u8; 4];
    stdin.read_exact(&mut prefix)?;
    let length = u32::from_le_bytes(prefix);
    if length == 0 || length > 64 * 1024 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0_u8; length as usize];
    stdin.read_exact(&mut bytes)?;
    let hello: serde_json::Value = serde_json::from_slice(&bytes)?;
    raw_json(&serde_json::json!({
        "type":"hello",
        "selected_version":1,
        "plugin_id":hello.get("plugin_id").and_then(serde_json::Value::as_str),
        "nonce":hello.get("nonce").and_then(serde_json::Value::as_str)
    }));
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn result(markdown: &str) -> ConversionResult {
    ConversionResult::new(
        Document::default(),
        markdown.into(),
        vec![Asset {
            id: AssetId("fixture-resource".into()),
            filename: Some("fixture.bin".into()),
            media_type: "application/octet-stream".into(),
            bytes: vec![1, 2, 3],
            external_uri: None,
        }],
        Vec::new(),
        vec![Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "fixture.process-v1".into(),
            locator: SourceLocator::default(),
            confidence: Some(1.0),
        }],
    )
}

fn raw_json(value: &serde_json::Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&u32::try_from(bytes.len()).unwrap().to_le_bytes()).unwrap();
    stdout.write_all(&bytes).unwrap();
    stdout.flush().unwrap();
}

fn raw_and_exit(bytes: &[u8]) -> ! {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).unwrap();
    stdout.flush().unwrap();
    std::process::exit(0)
}
