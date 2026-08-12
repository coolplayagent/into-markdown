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

    let mut child = Command::new(binary())
        .args(["--no-config", "--charset", "cp1252", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"caf\xe9\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "café\n");
}
