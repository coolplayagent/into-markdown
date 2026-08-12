//! Real-process exit-status contracts shared by Cargo and Bazel.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn binary() -> PathBuf {
    option_env!("CARGO_BIN_EXE_into-md")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("INTO_MD_BIN").map(PathBuf::from))
        .expect("Cargo or Bazel must provide the into-md binary")
}

fn run(arguments: &[&str]) -> (i32, String) {
    let output = Command::new(binary()).args(arguments).output().unwrap();
    (
        output.status.code().expect("CLI must exit normally"),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn run_with_stdin(arguments: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = Command::new(binary())
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn real_cli_preserves_stable_policy_component_and_usage_exits() {
    let (exit, stderr) = run(&["--no-config", "https://example.invalid/document"]);
    assert_eq!(exit, 5);
    assert!(stderr.contains("networkDenied"));

    let (exit, stderr) = run(&["--no-config", "models", "install", "missing"]);
    assert_eq!(exit, 9);
    assert!(stderr.contains("componentUnavailable"));

    let (exit, _) = run(&["--definitely-unknown-option"]);
    assert_eq!(exit, 2);
}

#[test]
fn real_cli_converts_txt_files_and_explicit_charset_stdin() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mixed.txt");
    std::fs::write(&path, "中文\r\nEnglish\n\nCafe\u{301} 😀\n").unwrap();
    let output =
        Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "中文  \nEnglish\n\nCafe\u{301} 😀\n");

    let output = run_with_stdin(&["--no-config", "--charset", "cp1252", "-"], b"caf\xe9\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "café\n");
}

#[test]
fn structured_text_is_never_consumed_by_txt_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let mut large_json = "[".repeat(200);
    large_json.push('"');
    large_json.extend(std::iter::repeat_n('x', 1024 * 1024 + 50_000));
    large_json.push('"');
    large_json.push_str(&"]".repeat(200));

    let json_path = directory.path().join("misleading.txt");
    std::fs::write(&json_path, &large_json).unwrap();
    let output =
        Command::new(binary()).args(["--no-config", json_path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("# JSON"));

    let output = run_with_stdin(&["--no-config", "-"], large_json.as_bytes());
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("# JSON"));

    let csv_path = directory.path().join("table.txt");
    std::fs::write(&csv_path, b"name,age\nAlice,42\nBob,30\n").unwrap();
    let output =
        Command::new(binary()).args(["--no-config", csv_path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("<strong>name</strong>"));

    let output = run_with_stdin(&["--no-config", "-"], b"name\tage\nAlice\t42\nBob\t30\n");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("<strong>age</strong>"));

    let output =
        run_with_stdin(&["--no-config", "-"], b"Today, we walked home\nTomorrow, we will rest\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Today, we walked home  \nTomorrow, we will rest\n"
    );

    let prefix = "{\"value\":\"";
    let suffix = "\"}";
    let mut boundary_junk = String::new();
    boundary_junk.push_str(prefix);
    boundary_junk.extend(std::iter::repeat_n('x', 1024 * 1024 - prefix.len() - suffix.len()));
    boundary_junk.push_str(suffix);
    boundary_junk.push_str(" trailing prose");
    let output = run_with_stdin(&["--no-config", "-"], boundary_junk.as_bytes());
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed"));
}

#[test]
fn full_input_unicode_controls_never_auto_detect_as_text() {
    let directory = tempfile::tempdir().unwrap();
    for (name, suffix) in [("del", b"\x7f".as_slice()), ("c1", b"\xc2\x80".as_slice())] {
        let path = directory.path().join(name);
        let mut contents = vec![b'A'; 70 * 1024];
        contents.extend_from_slice(suffix);
        std::fs::write(&path, contents).unwrap();

        let detected = Command::new(binary())
            .args(["formats", "detect", "--no-config", "--json", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(detected.status.success(), "{}", String::from_utf8_lossy(&detected.stderr));
        let detected: serde_json::Value = serde_json::from_slice(&detected.stdout).unwrap();
        assert!(
            detected["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .all(|candidate| candidate["format"] != "text")
        );

        let converted =
            Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
        assert!(!converted.status.success());
    }

    let safe_path = directory.path().join("safe.txt");
    let mut safe = vec![b'A'; 70 * 1024];
    safe.extend_from_slice(" 安全文本\tline\r\n".as_bytes());
    std::fs::write(&safe_path, safe).unwrap();
    let converted =
        Command::new(binary()).args(["--no-config", safe_path.to_str().unwrap()]).output().unwrap();
    assert!(converted.status.success(), "{}", String::from_utf8_lossy(&converted.stderr));
}

#[test]
fn excessive_txt_inlines_exit_as_resource_limit_not_internal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("too-many-lines.txt");
    std::fs::write(&path, "x\n".repeat(500_001)).unwrap();
    let output =
        Command::new(binary()).args(["--no-config", path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resourceLimit"));
    assert!(!stderr.contains("internal"));
}
