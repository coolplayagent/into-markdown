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
    let mut large_json = String::from("{\"records\":[");
    while large_json.len() <= 1024 * 1024 + 4096 {
        large_json.push_str("{\"name\":\"中文\",\"value\":123},");
    }
    large_json.push_str("null]}");

    let json_path = directory.path().join("misleading.txt");
    std::fs::write(&json_path, &large_json).unwrap();
    let output =
        Command::new(binary()).args(["--no-config", json_path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("noConverter"));

    let output = run_with_stdin(&["--no-config", "-"], large_json.as_bytes());
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("json"));

    let csv_path = directory.path().join("table.txt");
    std::fs::write(&csv_path, b"name,city\nAlice,London\nBob,Shanghai\n").unwrap();
    let output =
        Command::new(binary()).args(["--no-config", csv_path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("csv"));

    let output =
        run_with_stdin(&["--no-config", "-"], b"name\tcity\nAlice\tLondon\nBob\tShanghai\n");
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("tsv"));

    let output = run_with_stdin(
        &["--no-config", "-"],
        b"ordinary prose, with one comma\nand a second plain line\n",
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "ordinary prose, with one comma  \nand a second plain line\n"
    );
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
