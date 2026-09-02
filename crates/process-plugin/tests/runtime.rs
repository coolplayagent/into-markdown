//! Real executable and operating-system sandbox integration tests.

use into_markdown_core::{
    CancellationToken, ExecutionContext, ExecutionOptions, ProgressEvent, ProgressListener,
    ResourceLimits,
};
use into_markdown_process_plugin::{
    PluginErrorCode, PluginManifest, PluginRequest, ProcessPlugin, RuntimePolicy,
};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[test]
fn real_process_fixture_enforces_protocol_lifecycle_and_capabilities() {
    let harness = Harness::new();
    let ok = harness.execute(b"ok", ExecutionOptions::default()).unwrap();
    assert_eq!(ok.result.markdown, "ok");
    assert_eq!(ok.result.diagnostics.len(), 1);
    assert_eq!(ok.result.assets.len(), 1);
    assert_eq!(ok.result.provenance.len(), 1);
    assert_eq!(ok.plugin_id, "fixture.process-v1");

    let secret = harness.execute(b"secret", ExecutionOptions::default()).unwrap();
    assert_eq!(secret.result.markdown, "secret-denied");

    let private_temp = harness.execute(b"private-temp", ExecutionOptions::default()).unwrap();
    assert_eq!(private_temp.result.markdown, "private-temp-ready");

    let system_topology = harness.execute(b"system-topology", ExecutionOptions::default()).unwrap();
    assert_eq!(system_topology.result.markdown, "system-topology-ready");

    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), b"host-secret").unwrap();
    let file = harness
        .execute(
            format!("file:{}", outside.path().display()).as_bytes(),
            ExecutionOptions::default(),
        )
        .unwrap();
    assert_eq!(file.result.markdown, "file-denied");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let network = harness
        .execute(
            format!("network:{}", listener.local_addr().unwrap()).as_bytes(),
            ExecutionOptions::default(),
        )
        .unwrap();
    assert_eq!(network.result.markdown, "network-denied");

    let controlled = harness.execute(b"controlled-error", ExecutionOptions::default()).unwrap_err();
    assert_eq!(controlled.code, PluginErrorCode::Plugin);
    assert_eq!(controlled.detail, "plugin returned unknownFixture: unknown fixture command");
    for (mode, expected) in [
        (b"resource-error".as_slice(), PluginErrorCode::ResourceLimit),
        (b"recognition-memory-error".as_slice(), PluginErrorCode::OcrRecognitionMemory),
        (b"timeout-error".as_slice(), PluginErrorCode::Timeout),
        (b"cancelled-error".as_slice(), PluginErrorCode::Cancelled),
    ] {
        let error = harness.execute(mode, ExecutionOptions::default()).unwrap_err();
        assert_eq!(error.code, expected);
    }

    for (mode, expected) in [
        (b"malformed".as_slice(), PluginErrorCode::Protocol),
        (b"oversize".as_slice(), PluginErrorCode::FrameTooLarge),
        (b"bad-order".as_slice(), PluginErrorCode::Protocol),
        (b"invalid-result".as_slice(), PluginErrorCode::InvalidResult),
        (b"missing-error-id".as_slice(), PluginErrorCode::Protocol),
        (b"extra-after-response".as_slice(), PluginErrorCode::Protocol),
        (b"crash".as_slice(), PluginErrorCode::Crashed),
    ] {
        let error = harness.execute(mode, ExecutionOptions::default()).unwrap_err();
        assert_eq!(error.code, expected, "mode {}: {error}", String::from_utf8_lossy(mode));
    }

    let timeout = harness
        .execute(
            b"hang",
            ExecutionOptions {
                timeout: Some(Duration::from_millis(100)),
                ..ExecutionOptions::default()
            },
        )
        .unwrap_err();
    assert_eq!(timeout.code, PluginErrorCode::Timeout, "unexpected hang result: {timeout}");

    let started = std::time::Instant::now();
    let flood = harness
        .execute(
            b"frame-flood",
            ExecutionOptions {
                timeout: Some(Duration::from_millis(500)),
                ..ExecutionOptions::default()
            },
        )
        .unwrap_err();
    assert_eq!(flood.code, PluginErrorCode::Timeout, "unexpected flood result: {flood}");
    assert!(started.elapsed() < Duration::from_secs(5), "frame flood teardown deadlocked");

    let stall = Harness::new_with_mode(Some("stall-request"));
    let started = std::time::Instant::now();
    let source = vec![b'x'; 1024 * 1024];
    let error = stall.execute(&source, ExecutionOptions::default()).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::Timeout);
    assert!(started.elapsed() < Duration::from_secs(30), "blocked request write ignored deadline");
}

#[test]
fn combined_provider_and_child_memory_is_terminal_and_releases_request_resources() {
    let harness = Harness::new_with_mode(Some("aggregate-memory"));
    let context = ExecutionContext::new(
        ExecutionOptions { timeout: Some(Duration::from_secs(90)), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let error = harness
        .plugin
        .execute(
            PluginRequest {
                memory_limit: Some(512 * 1024 * 1024),
                request_id: "combined-memory",
                input_format: "text",
                source_name: Some("fixture.txt"),
                parameters_json: None,
                source: b"child-memory",
            },
            &context,
        )
        .unwrap_err();
    assert_eq!(error.code, PluginErrorCode::ProcessMemoryLimit, "{error}");
    assert_eq!(context.reserved_memory_bytes(), 0);
    assert_eq!(context.reserved_temporary_bytes(), 0);
    assert_eq!(harness.execute_with_context(b"ok", &context).unwrap().result.markdown, "ok");
    assert_eq!(context.reserved_memory_bytes(), 0);
    assert_eq!(context.reserved_temporary_bytes(), 0);
}

#[test]
fn isolated_worker_stdout_keeps_native_noise_out_of_protocol_frames() {
    let harness = Harness::new_with_mode(Some("isolate-stdout"));
    let result = harness.execute(b"ignored", ExecutionOptions::default()).unwrap();
    assert_eq!(result.result.markdown, "stdout-isolated");
}

#[test]
fn cancellation_race_is_stable_for_twenty_real_processes() {
    let harness = Harness::new();
    for iteration in 0..20 {
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let (ready, observed) = std::sync::mpsc::sync_channel(1);
        let listener = std::sync::Arc::new(CancelReady(ready));
        let trigger_thread = std::thread::spawn(move || {
            observed.recv_timeout(Duration::from_secs(30)).unwrap();
            trigger.cancel();
        });
        let context = ExecutionContext::new(
            ExecutionOptions {
                cancellation,
                progress_listener: Some(listener),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        let cancelled = harness.execute_with_context(b"cancel", &context).unwrap_err();
        trigger_thread.join().unwrap();
        assert_eq!(
            cancelled.code,
            PluginErrorCode::Cancelled,
            "iteration {iteration}: {cancelled}"
        );
        assert_temporary_budget_released(&context);
        #[cfg(windows)]
        assert!(
            !std::fs::read_dir(&harness.profile.storage)
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("into-md-plugin-"))
        );
    }
}

#[test]
fn staged_runtime_isolated_and_raii_releases_temporary_budget() {
    let harness = Harness::new();

    let success = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    assert_eq!(harness.execute_with_context(b"ok", &success).unwrap().result.markdown, "ok");
    assert_temporary_budget_released(&success);

    let failure = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    assert_eq!(
        harness.execute_with_context(b"malformed", &failure).unwrap_err().code,
        PluginErrorCode::Protocol
    );
    assert_temporary_budget_released(&failure);

    let (mutated, observed) = std::sync::mpsc::sync_channel(1);
    let mutation = std::sync::Arc::new(MutateOriginal {
        executable: harness.executable.clone(),
        complete: mutated,
    });
    let isolated = ExecutionContext::new(
        ExecutionOptions { progress_listener: Some(mutation), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    assert_eq!(
        harness.execute_with_context(b"stage-isolated", &isolated).unwrap().result.markdown,
        "stage-isolated"
    );
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(std::fs::read(&harness.executable).unwrap(), b"mutated");
    assert_temporary_budget_released(&isolated);
}

#[cfg(unix)]
#[test]
fn declared_child_process_can_execute_an_authenticated_runtime_helper() {
    let harness = Harness::new_with_mode(Some("allow-child"));
    let result = harness.execute(b"child", ExecutionOptions::default()).unwrap();
    assert_eq!(result.result.markdown, "child-ok");
}

struct MutateOriginal {
    executable: PathBuf,
    complete: std::sync::mpsc::SyncSender<()>,
}

impl ProgressListener for MutateOriginal {
    fn on_progress(&self, event: ProgressEvent) {
        if event.message.as_deref() == Some("stage-ready") {
            make_fixture_writable(&self.executable);
            std::fs::write(&self.executable, b"mutated").unwrap();
            let _ = self.complete.try_send(());
        }
    }
}

#[cfg(unix)]
fn make_fixture_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn make_fixture_writable(path: &Path) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

fn assert_temporary_budget_released(context: &ExecutionContext) {
    assert_eq!(
        context.reserved_temporary_bytes(),
        0,
        "staged runtime temporary reservation leaked"
    );
}

struct CancelReady(std::sync::mpsc::SyncSender<()>);

impl ProgressListener for CancelReady {
    fn on_progress(&self, event: ProgressEvent) {
        if event.message.as_deref() == Some("cancel-ready") {
            let _ = self.0.try_send(());
        }
    }
}

#[test]
fn executable_digest_mutation_fails_before_launch() {
    let harness = Harness::new();
    make_fixture_writable(&harness.executable);
    std::fs::write(&harness.executable, b"mutated").unwrap();
    let error = harness.execute(b"ok", ExecutionOptions::default()).unwrap_err();
    assert_eq!(error.code, PluginErrorCode::Authority);
}

#[test]
fn executable_larger_than_policy_is_rejected_before_launch() {
    let fixture_bytes = std::fs::metadata(fixture_executable()).unwrap().len();
    let error =
        Harness::try_new_with_mode_and_file_limit(None, Some(fixture_bytes.saturating_sub(1)))
            .err()
            .expect("oversized fixture unexpectedly accepted");
    assert_eq!(error.code, PluginErrorCode::Authority);
}

#[test]
fn reservation_checkpoint_preserves_cancelled_and_timeout() {
    let harness = Harness::new();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = harness
        .execute(b"ok", ExecutionOptions { cancellation, ..ExecutionOptions::default() })
        .unwrap_err();
    assert_eq!(cancelled.code, PluginErrorCode::Cancelled, "{cancelled}");

    let timed_out = harness
        .execute(
            b"ok",
            ExecutionOptions { timeout: Some(Duration::ZERO), ..ExecutionOptions::default() },
        )
        .unwrap_err();
    assert_eq!(timed_out.code, PluginErrorCode::Timeout, "{timed_out}");
}

struct Harness {
    plugin: ProcessPlugin,
    executable: PathBuf,
    _runtime: tempfile::TempDir,
    #[cfg(windows)]
    profile: WindowsProfile,
}

impl Harness {
    fn new() -> Self {
        Self::new_with_mode(None)
    }

    fn new_with_mode(mode: Option<&str>) -> Self {
        Self::try_new_with_mode_and_file_limit(mode, None).unwrap()
    }

    fn try_new_with_mode_and_file_limit(
        mode: Option<&str>,
        max_file_bytes: Option<u64>,
    ) -> Result<Self, into_markdown_process_plugin::PluginError> {
        let fixture = fixture_executable();
        let runtime =
            tempfile::Builder::new().prefix("into-md-fixture-runtime-").tempdir().unwrap();
        let executable = runtime.path().join(if cfg!(windows) { "fixture.exe" } else { "fixture" });
        std::fs::copy(&fixture, &executable).unwrap();
        let helper = runtime.path().join(if cfg!(windows) {
            "verified-helper.exe"
        } else {
            "verified-helper"
        });
        std::fs::copy(&fixture, &helper).unwrap();
        let runtime_root = runtime.path().canonicalize().unwrap();
        let executable = executable.canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o500)).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        #[cfg(windows)]
        let profile = WindowsProfile::new(&runtime_root);
        let digest = format!("{:x}", Sha256::digest(std::fs::read(&executable).unwrap()));
        #[allow(unused_mut)]
        let mut policy = RuntimePolicy {
            max_frame_bytes: 16 * 1024 * 1024,
            max_output_bytes: 8 * 1024 * 1024,
            // These tests exercise protocol and lifecycle classification, not a
            // deliberately small memory budget. A debug test process can carry
            // a larger fork-time resident high-water mark on Linux runners, so
            // keep the fixture aligned with the production policy default.
            max_memory_bytes: 512 * 1024 * 1024,
            handshake_timeout: Duration::from_secs(3),
            cancellation_grace: Duration::from_millis(50),
            ..RuntimePolicy::default()
        };
        if let Some(max_file_bytes) = max_file_bytes {
            policy.max_file_bytes = max_file_bytes;
        }
        #[cfg(windows)]
        {
            policy.windows = profile.authority();
        }
        if let Some(mode) = mode {
            policy.environment.insert("PROCESS_PLUGIN_FIXTURE_MODE".into(), mode.into());
            if mode == "stall-request" {
                policy.request_timeout = Duration::from_millis(250);
            }
            policy.allow_child_processes = matches!(mode, "allow-child" | "aggregate-memory");
            if mode == "aggregate-memory" {
                // Measure the memory guard independently of loaded CI hosts' startup latency.
                policy.handshake_timeout = Duration::from_secs(30);
            }
        }
        let plugin = ProcessPlugin::new(
            PluginManifest {
                plugin_id: "fixture.process-v1".into(),
                executable: executable.clone(),
                runtime_root,
                executable_sha256: digest,
                protocol_versions: vec![1],
            },
            policy,
        )?;
        Ok(Self {
            plugin,
            executable,
            _runtime: runtime,
            #[cfg(windows)]
            profile,
        })
    }

    fn execute(
        &self,
        source: &[u8],
        options: ExecutionOptions,
    ) -> Result<
        into_markdown_process_plugin::PluginExecution,
        into_markdown_process_plugin::PluginError,
    > {
        let context = ExecutionContext::new(options, ResourceLimits::default());
        self.execute_with_context(source, &context)
    }

    fn execute_with_context(
        &self,
        source: &[u8],
        context: &ExecutionContext,
    ) -> Result<
        into_markdown_process_plugin::PluginExecution,
        into_markdown_process_plugin::PluginError,
    > {
        self.plugin.execute(
            PluginRequest {
                memory_limit: None,
                request_id: "fixture-request",
                input_format: "text",
                source_name: Some("fixture.txt"),
                parameters_json: None,
                source,
            },
            context,
        )
    }
}

#[test]
fn payloads_larger_than_a_protocol_frame_use_the_private_staged_source() {
    let fixture_bytes = std::fs::metadata(fixture_executable()).unwrap().len();
    let max_file_bytes = fixture_bytes.max(32 * 1024 * 1024);
    let runtime = Harness::try_new_with_mode_and_file_limit(None, Some(max_file_bytes)).unwrap();
    let mut source = vec![b'x'; 13 * 1024 * 1024];
    source[..8].copy_from_slice(b"large-ok");
    let result = runtime.execute(&source, ExecutionOptions::default()).unwrap();
    assert_eq!(result.result.markdown, "large-ok");
}

fn fixture_executable() -> PathBuf {
    if let Some(value) = option_env!("CARGO_BIN_EXE_process-plugin-fixture") {
        let candidate = PathBuf::from(value);
        if candidate.is_file() {
            return candidate.canonicalize().unwrap();
        }
    }
    if let Some(runfiles) = std::env::var_os("RUNFILES_DIR") {
        let runfiles = PathBuf::from(runfiles);
        for repository in ["_main", "into_markdown"] {
            let candidate =
                runfiles.join(repository).join("crates/process-plugin").join(if cfg!(windows) {
                    "process-plugin-fixture.exe"
                } else {
                    "process-plugin-fixture"
                });
            if candidate.is_file() {
                return candidate.canonicalize().unwrap();
            }
        }
    }
    if let Some(candidate) = manifest_runfile(&[
        "_main/crates/process-plugin/process-plugin-fixture.exe",
        "into_markdown/crates/process-plugin/process-plugin-fixture.exe",
        "_main/crates/process-plugin/process-plugin-fixture",
        "into_markdown/crates/process-plugin/process-plugin-fixture",
    ]) {
        return candidate;
    }
    let current = std::env::current_exe().unwrap();
    let debug = current.parent().and_then(Path::parent).unwrap();
    debug
        .join(if cfg!(windows) { "process-plugin-fixture.exe" } else { "process-plugin-fixture" })
        .canonicalize()
        .unwrap()
}

fn manifest_runfile(logical_paths: &[&str]) -> Option<PathBuf> {
    let manifest = PathBuf::from(std::env::var_os("RUNFILES_MANIFEST_FILE")?);
    let metadata = std::fs::metadata(&manifest).ok()?;
    if metadata.len() > 64 * 1024 * 1024 {
        return None;
    }
    let contents = std::fs::read_to_string(manifest).ok()?;
    for line in contents.lines() {
        let Some((logical, physical)) = line.split_once(' ') else {
            continue;
        };
        if logical_paths.contains(&logical) {
            let candidate = PathBuf::from(physical);
            if candidate.is_file() {
                return candidate.canonicalize().ok();
            }
        }
    }
    None
}

#[cfg(windows)]
struct WindowsProfile {
    name: String,
    sid: String,
    storage: PathBuf,
}

#[cfg(windows)]
impl WindowsProfile {
    fn new(runtime: &Path) -> Self {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_sys::Win32::Security::Isolation::{
            CreateAppContainerProfile, GetAppContainerFolderPath,
        };
        use windows_sys::Win32::Security::PSID;
        use windows_sys::Win32::System::Com::CoTaskMemFree;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let name = format!(
            "into-markdown-fixture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let wide = |value: &str| {
            std::ffi::OsStr::new(value).encode_wide().chain(Some(0)).collect::<Vec<_>>()
        };
        let wide_name = wide(&name);
        let display = wide("Into Markdown fixture");
        let description = wide("Ephemeral process-plugin sandbox fixture");
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: every string is NUL-terminated, capabilities are empty, and `sid` is writable.
        let result = unsafe {
            CreateAppContainerProfile(
                wide_name.as_ptr(),
                display.as_ptr(),
                description.as_ptr(),
                std::ptr::null(),
                0,
                &raw mut sid,
            )
        };
        assert!(result >= 0 && !sid.is_null(), "CreateAppContainerProfile HRESULT={result:#x}");
        let mut text = std::ptr::null_mut();
        // SAFETY: profile creation returned a valid SID and `text` is writable.
        assert_ne!(unsafe { ConvertSidToStringSidW(sid, &raw mut text) }, 0);
        // SAFETY: successful conversion returns a NUL-terminated UTF-16 allocation.
        let sid_text = unsafe { wide_ptr(text) };
        // SAFETY: `text` is LocalAlloc-owned and released exactly once.
        unsafe {
            LocalFree(text.cast());
        }
        let wide_sid = wide(&sid_text);
        let mut storage = std::ptr::null_mut();
        // SAFETY: `wide_sid` is NUL-terminated and `storage` is writable.
        assert!(unsafe { GetAppContainerFolderPath(wide_sid.as_ptr(), &raw mut storage) } >= 0);
        // SAFETY: the successful call returns a NUL-terminated UTF-16 path.
        let storage_path = PathBuf::from(unsafe { wide_ptr(storage) }).canonicalize().unwrap();
        // SAFETY: both allocations are owned by the documented matching allocators.
        unsafe {
            CoTaskMemFree(storage.cast());
            windows_sys::Win32::Security::FreeSid(sid);
        }
        let grant = format!("*{sid_text}:(OI)(CI)RX");
        let status = std::process::Command::new("icacls.exe")
            .arg(runtime)
            .arg("/grant")
            .arg(grant)
            .arg("/T")
            .arg("/Q")
            .status()
            .unwrap();
        assert!(status.success(), "icacls failed: {status}");
        Self { name, sid: sid_text, storage: storage_path }
    }

    fn authority(&self) -> into_markdown_process_plugin::WindowsSandboxAuthority {
        into_markdown_process_plugin::WindowsSandboxAuthority {
            profile_name: self.name.clone(),
            sid: self.sid.clone(),
            storage_root: self.storage.clone(),
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsProfile {
    fn drop(&mut self) {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Security::Isolation::DeleteAppContainerProfile;
        let name =
            std::ffi::OsStr::new(&self.name).encode_wide().chain(Some(0)).collect::<Vec<_>>();
        // SAFETY: profile name is NUL-terminated and remains alive for the call.
        unsafe {
            let _ = DeleteAppContainerProfile(name.as_ptr());
        }
    }
}

#[cfg(windows)]
unsafe fn wide_ptr(value: *const u16) -> String {
    let mut length = 0_usize;
    // SAFETY: callers pass a non-null OS-owned NUL-terminated UTF-16 string.
    while unsafe { *value.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the scan established `length` initialized elements before the terminator.
    String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) }).unwrap()
}
