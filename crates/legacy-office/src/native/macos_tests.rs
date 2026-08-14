use super::*;
use std::process::{Command, Stdio};

#[test]
fn seatbelt_denies_ip_outside_writes_and_additional_exec() {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let runtime = executable.parent().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let profile = macos_soffice_profile(runtime, temporary.path(), &executable).unwrap();
    for (probe, value) in [
        ("network", listener.local_addr().unwrap().to_string()),
        ("write", outside.path().join("canary").display().to_string()),
        ("exec", String::new()),
    ] {
        let status = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile])
            .arg(&executable)
            .args(["--exact", "native::macos_tests::seatbelt_probe_child", "--nocapture"])
            .env_clear()
            .env("INTO_MD_SEATBELT_PROBE", probe)
            .env("INTO_MD_SEATBELT_VALUE", value)
            .env("HOME", temporary.path())
            .env("TMPDIR", temporary.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "seatbelt probe failed: {probe}");
    }
    assert!(!outside.path().join("canary").exists());
}

#[test]
fn seatbelt_probe_child() {
    let Ok(probe) = std::env::var("INTO_MD_SEATBELT_PROBE") else {
        return;
    };
    let value = std::env::var("INTO_MD_SEATBELT_VALUE").unwrap();
    let denied = match probe.as_str() {
        "network" => std::net::TcpStream::connect(value).is_err(),
        "write" => std::fs::write(value, b"canary").is_err(),
        "exec" => Command::new("/usr/bin/true").status().is_err(),
        _ => false,
    };
    assert!(denied, "seatbelt unexpectedly allowed {probe}");
}
