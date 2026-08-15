use super::*;
use std::process::{Command, Stdio};

#[test]
fn seatbelt_denies_ip_outside_writes_and_additional_exec() {
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let runtime = executable.parent().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let ipc = macos_office_ipc_path(&temporary.path().join("profile"));
    let profile = macos_soffice_profile(runtime, temporary.path(), &executable, &ipc).unwrap();
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
fn seatbelt_allows_only_this_sessions_exact_office_socket() {
    use std::os::unix::net::UnixListener;

    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let runtime = executable.parent().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let ipc = macos_office_ipc_path(&temporary.path().join("profile"));
    let profile = macos_soffice_profile(runtime, temporary.path(), &executable, &ipc).unwrap();
    // SAFETY: geteuid takes no arguments and has no failure condition.
    let euid = unsafe { libc::geteuid() };
    let nonce = std::process::id();
    let network_ipc = ipc.clone();
    let paths = [
        (ipc.clone(), network_ipc, true),
        (
            PathBuf::from(format!(
                "/private/tmp/OSL_PIPE_{euid}_SingleOfficeIPC_crosstalk{nonce:x}"
            )),
            PathBuf::from(format!(
                "/private/tmp/OSL_PIPE_{euid}_SingleOfficeIPC_crosstalk{nonce:x}"
            )),
            false,
        ),
        (
            PathBuf::from(format!(
                "/private/tmp/OSL_PIPE_{}_SingleOfficeIPC_foreign{nonce:x}",
                euid.saturating_add(1)
            )),
            PathBuf::from(format!(
                "/private/tmp/OSL_PIPE_{}_SingleOfficeIPC_foreign{nonce:x}",
                euid.saturating_add(1)
            )),
            false,
        ),
        (temporary.path().join("arbitrary.sock"), temporary.path().join("arbitrary.sock"), false),
    ];
    let mut listeners = Vec::new();
    for (path, _, _) in &paths {
        let _ = std::fs::remove_file(path);
        listeners.push(UnixListener::bind(path).unwrap());
    }
    for (path, network_path, allowed) in &paths {
        let status = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile])
            .arg(&executable)
            .args(["--exact", "native::macos_tests::seatbelt_probe_child", "--nocapture"])
            .env_clear()
            .env("INTO_MD_SEATBELT_PROBE", "unix")
            .env("INTO_MD_SEATBELT_VALUE", network_path)
            .env("INTO_MD_SEATBELT_ALLOWED", allowed.to_string())
            .env("HOME", temporary.path())
            .env("TMPDIR", temporary.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "CROSSTALK probe failed: {}", path.display());
    }
    drop(listeners);
    for (path, _, _) in paths {
        macos_remove_office_ipc(&path);
        assert!(!path.exists());
    }
}

#[test]
fn office_ipc_rejects_preexisting_nodes_and_removes_its_exact_socket() {
    use std::os::unix::net::UnixListener;

    let temporary = tempfile::tempdir().unwrap();
    let profile = temporary.path().join("profile");
    let ipc = macos_office_ipc_path(&profile);
    let listener = UnixListener::bind(&ipc).unwrap();
    assert!(OfficeIpcSocket::new(&profile).is_err());
    drop(listener);
    std::fs::remove_file(&ipc).unwrap();
    let guard = OfficeIpcSocket::new(&profile).unwrap();
    let listener = UnixListener::bind(&ipc).unwrap();
    drop(listener);
    drop(guard);
    assert!(!ipc.exists());
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
        "unix" => {
            let allowed = std::env::var("INTO_MD_SEATBELT_ALLOWED").unwrap() == "true";
            std::os::unix::net::UnixStream::connect(value).is_ok() == allowed
        }
        _ => false,
    };
    assert!(denied, "seatbelt probe had unexpected result: {probe}");
}
