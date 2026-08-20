// SPDX-License-Identifier: Apache-2.0
//! End-to-end tests for the pinned WASI Preview 2 component boundary.

use into_markdown_core::{CancellationToken, ExecutionContext, ExecutionOptions, ResourceLimits};
use into_markdown_plugin_wasi::{
    NetworkGrant, PluginRequest, PreopenGrant, WASMTIME_VERSION, WasiCapabilities, WasiLimits,
    WasiPluginErrorCode, WasiPluginManifest, WasiPluginRuntime,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant};

const GUEST: &[u8] = include_bytes!("fixtures/guest.component.wasm");

fn target() -> &'static str {
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    return "x86_64-pc-windows-msvc";
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    return "x86_64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    return "aarch64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    return "aarch64-apple-darwin";
    #[allow(unreachable_code)]
    "unsupported"
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut output, byte| {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
        output
    })
}

fn manifest() -> WasiPluginManifest {
    WasiPluginManifest {
        id: "fixture".into(),
        protocol: "wasi-v1".into(),
        wasi_preview: "preview2".into(),
        runtime_version: WASMTIME_VERSION.into(),
        component_sha256: digest(GUEST),
        component_bytes: GUEST.len() as u64,
        supported_targets: BTreeSet::from([target().into()]),
        capabilities: WasiCapabilities::default(),
        limits: WasiLimits {
            fuel: 50_000_000,
            max_linear_memory_bytes: 32 * 1024 * 1024,
            max_output_bytes: 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            max_resources: 16,
            max_resource_bytes: 1024 * 1024,
        },
    }
}

fn context() -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
}

fn request(source_name: &str) -> PluginRequest {
    PluginRequest {
        protocol_version: 1,
        source_name: source_name.into(),
        input: b"fixture".to_vec(),
    }
}

#[test]
fn real_preview2_component_round_trips_valid_document_ir() {
    let execution = context();
    let output = WasiPluginRuntime::new()
        .unwrap()
        .run(GUEST, &manifest(), &request("valid"), &execution)
        .unwrap();
    assert!(output.document.blocks.is_empty());
    assert!(output.resources.is_empty());
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn resources_and_plugin_provenance_are_authenticated_before_return() {
    let runtime = WasiPluginRuntime::new().unwrap();
    let output = runtime.run(GUEST, &manifest(), &request("valid-resource"), &context()).unwrap();
    assert_eq!(output.document.blocks.len(), 1);
    assert_eq!(output.resources.len(), 1);
    assert_eq!(output.resources[0].bytes, b"abc");

    for (source, expected) in [
        ("bad-resource", WasiPluginErrorCode::InvalidOutput),
        ("alias-resource", WasiPluginErrorCode::InvalidOutput),
        ("bad-mime", WasiPluginErrorCode::InvalidOutput),
        ("bad-provenance", WasiPluginErrorCode::InvalidIr),
    ] {
        let error = runtime.run(GUEST, &manifest(), &request(source), &context()).unwrap_err();
        assert_eq!(error.code, expected, "{}", error.detail);
    }

    let mut count = manifest();
    count.limits.max_resources = 0;
    let execution = context();
    assert_eq!(
        runtime.run(GUEST, &count, &request("valid-resource"), &execution).unwrap_err().code,
        WasiPluginErrorCode::ResourceLimit
    );
    assert_eq!(execution.reserved_memory_bytes(), 0);

    let mut bytes = manifest();
    bytes.limits.max_resource_bytes = 2;
    let execution = context();
    assert_eq!(
        runtime.run(GUEST, &bytes, &request("valid-resource"), &execution).unwrap_err().code,
        WasiPluginErrorCode::ResourceLimit
    );
    assert_eq!(execution.reserved_memory_bytes(), 0);
}

#[test]
fn component_hash_and_protocol_are_fail_closed() {
    let runtime = WasiPluginRuntime::new().unwrap();
    let mut bad_hash = manifest();
    bad_hash.component_sha256 = "0".repeat(64);
    assert_eq!(
        runtime.run(GUEST, &bad_hash, &request("valid"), &context()).unwrap_err().code,
        WasiPluginErrorCode::HashMismatch
    );
    bad_hash.component_sha256 = "A".repeat(64);
    assert_eq!(
        runtime.run(GUEST, &bad_hash, &request("valid"), &context()).unwrap_err().code,
        WasiPluginErrorCode::InvalidManifest
    );
    let mut bad_target = manifest();
    bad_target.supported_targets.insert("wasm32-wasip2".into());
    assert_eq!(
        runtime.run(GUEST, &bad_target, &request("valid"), &context()).unwrap_err().code,
        WasiPluginErrorCode::InvalidManifest
    );
    let mut bad_size = manifest();
    bad_size.component_bytes += 1;
    assert_eq!(
        runtime.run(GUEST, &bad_size, &request("valid"), &context()).unwrap_err().code,
        WasiPluginErrorCode::InvalidManifest
    );
    let mut bad_request = request("valid");
    bad_request.protocol_version = 2;
    assert_eq!(
        runtime.run(GUEST, &manifest(), &bad_request, &context()).unwrap_err().code,
        WasiPluginErrorCode::ProtocolMismatch
    );
}

#[test]
fn input_and_host_memory_are_checked_and_released_by_raii() {
    let runtime = WasiPluginRuntime::new().unwrap();
    let over_input = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_input_bytes: 6, ..ResourceLimits::default() },
    );
    let mut seven = request("valid");
    seven.input = vec![0; 7];
    assert_eq!(
        runtime.run(GUEST, &manifest(), &seven, &over_input).unwrap_err().code,
        WasiPluginErrorCode::ResourceLimit
    );
    assert_eq!(over_input.reserved_memory_bytes(), 0);

    let low_memory = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() },
    );
    assert_eq!(
        runtime.run(GUEST, &manifest(), &request("valid"), &low_memory).unwrap_err().code,
        WasiPluginErrorCode::ResourceLimit
    );
    assert_eq!(low_memory.reserved_memory_bytes(), 0);

    let bounded_output_low_memory = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: 39_250_000, ..ResourceLimits::default() },
    );
    let error = runtime
        .run(GUEST, &manifest(), &request("valid-resource"), &bounded_output_low_memory)
        .unwrap_err();
    assert_eq!(error.code, WasiPluginErrorCode::ResourceLimit);
    assert_eq!(error.detail, "response rejected before owned materialization");
    assert_eq!(bounded_output_low_memory.reserved_memory_bytes(), 0);

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    assert_eq!(
        runtime.run(GUEST, &manifest(), &request("valid"), &cancelled).unwrap_err().code,
        WasiPluginErrorCode::Cancelled
    );
    assert_eq!(cancelled.reserved_memory_bytes(), 0);
}

#[test]
fn unsupported_non_wasi_import_is_an_invalid_hostcall() {
    let mut hostile = GUEST.to_vec();
    let wasi = b"wasi:clocks/wall-clock";
    let offset = hostile
        .windows(wasi.len())
        .position(|window| window == wasi)
        .expect("fixture imports the WASI wall clock");
    hostile[offset..offset + 4].copy_from_slice(b"evil");
    let mut hostile_manifest = manifest();
    hostile_manifest.component_sha256 = digest(&hostile);
    let error = WasiPluginRuntime::new()
        .unwrap()
        .run(&hostile, &hostile_manifest, &request("valid"), &context())
        .unwrap_err();
    assert_eq!(error.code, WasiPluginErrorCode::InvalidHostcall, "{}", error.detail);
}

#[test]
fn fuel_output_and_ir_boundaries_return_stable_errors() {
    let runtime = WasiPluginRuntime::new().unwrap();
    let mut fuel_manifest = manifest();
    fuel_manifest.limits.fuel = 10_000;
    assert_eq!(
        runtime.run(GUEST, &fuel_manifest, &request("fuel-loop"), &context()).unwrap_err().code,
        WasiPluginErrorCode::FuelExhausted
    );

    let mut output_manifest = manifest();
    output_manifest.limits.max_output_bytes = 1024;
    let output_error =
        runtime.run(GUEST, &output_manifest, &request("oversized-output"), &context()).unwrap_err();
    assert_eq!(output_error.code, WasiPluginErrorCode::ResourceLimit, "{}", output_error.detail);

    assert_eq!(
        runtime.run(GUEST, &manifest(), &request("invalid-ir"), &context()).unwrap_err().code,
        WasiPluginErrorCode::InvalidIr
    );
}

#[test]
fn clocks_and_random_trap_without_grants_and_run_with_exact_grants() {
    let runtime = WasiPluginRuntime::new().unwrap();
    for (source, capability) in [("clock-call", "clocks"), ("random-call", "random")] {
        let denied = runtime.run(GUEST, &manifest(), &request(source), &context()).unwrap_err();
        assert_eq!(denied.code, WasiPluginErrorCode::CapabilityDenied, "{}", denied.detail);

        let mut granted = manifest();
        match capability {
            "clocks" => granted.capabilities.clocks = true,
            "random" => granted.capabilities.random = true,
            _ => unreachable!(),
        }
        runtime.run(GUEST, &granted, &request(source), &context()).unwrap();
    }
}

#[test]
fn preopen_is_absent_by_default_and_scoped_when_granted() {
    let runtime = WasiPluginRuntime::new().unwrap();
    let denied = runtime.run(GUEST, &manifest(), &request("preopen-call"), &context()).unwrap_err();
    assert_eq!(denied.code, WasiPluginErrorCode::GuestFailure, "{}", denied.detail);

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("probe.txt"), "preopen-ok").unwrap();
    let mut granted = manifest();
    granted.capabilities.preopens.push(PreopenGrant {
        host_path: directory.path().to_path_buf(),
        guest_path: "/input".into(),
        writable: false,
    });
    runtime.run(GUEST, &granted, &request("preopen-call"), &context()).unwrap();

    granted.capabilities.preopens[0].guest_path = "/input/../escape".into();
    assert_eq!(
        runtime.run(GUEST, &granted, &request("valid"), &context()).unwrap_err().code,
        WasiPluginErrorCode::InvalidManifest
    );
}

#[cfg(unix)]
#[test]
fn preopen_symlink_cannot_escape_the_granted_directory() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "must-not-leak").unwrap();
    symlink(outside.path(), root.path().join("escape")).unwrap();
    let mut scoped = manifest();
    scoped.capabilities.preopens.push(PreopenGrant {
        host_path: root.path().to_path_buf(),
        guest_path: "/input".into(),
        writable: false,
    });
    assert_eq!(
        WasiPluginRuntime::new()
            .unwrap()
            .run(GUEST, &scoped, &request("symlink-escape"), &context())
            .unwrap_err()
            .code,
        WasiPluginErrorCode::GuestFailure
    );
}

#[cfg(unix)]
#[test]
fn preopen_parent_symlink_is_rejected_before_guest_start() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let real = parent.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = parent.path().join("alias");
    symlink(&real, &alias).unwrap();
    let mut scoped = manifest();
    scoped.capabilities.preopens.push(PreopenGrant {
        host_path: alias,
        guest_path: "/input".into(),
        writable: false,
    });
    assert_eq!(
        WasiPluginRuntime::new()
            .unwrap()
            .run(GUEST, &scoped, &request("valid"), &context())
            .unwrap_err()
            .code,
        WasiPluginErrorCode::InvalidManifest
    );
}

#[test]
fn tcp_is_absent_by_default_and_only_exact_loopback_endpoint_is_granted() {
    let runtime = WasiPluginRuntime::new().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let source = format!("network-call:{}", address.port());

    let denied = runtime.run(GUEST, &manifest(), &request(&source), &context()).unwrap_err();
    assert_eq!(denied.code, WasiPluginErrorCode::GuestFailure, "{}", denied.detail);
    assert!(listener.accept().is_err(), "an ungranted guest reached the listener");

    let mut granted = manifest();
    granted.capabilities.network.push(NetworkGrant {
        address: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: address.port(),
        allow_private: true,
    });
    runtime.run(GUEST, &granted, &request(&source), &context()).unwrap();
    listener.accept().expect("the exact granted loopback endpoint was reached");

    granted.capabilities.network[0].port = address.port().wrapping_add(1).max(1);
    assert_eq!(
        runtime.run(GUEST, &granted, &request(&source), &context()).unwrap_err().code,
        WasiPluginErrorCode::GuestFailure
    );
}

#[test]
fn linear_memory_growth_and_out_of_bounds_access_have_stable_errors() {
    let runtime = WasiPluginRuntime::new().unwrap();
    let mut growth_manifest = manifest();
    growth_manifest.limits.max_linear_memory_bytes = 4 * 1024 * 1024;
    assert_eq!(
        runtime
            .run(GUEST, &growth_manifest, &request("memory-growth"), &context())
            .unwrap_err()
            .code,
        WasiPluginErrorCode::ResourceLimit
    );
    assert_eq!(
        runtime.run(GUEST, &manifest(), &request("memory-oob"), &context()).unwrap_err().code,
        WasiPluginErrorCode::MemoryOutOfBounds
    );
}

#[test]
fn epoch_interrupts_tight_guest_for_cancellation_and_timeout() {
    let runtime = WasiPluginRuntime::new().unwrap();
    let mut unbounded_fuel = manifest();
    unbounded_fuel.limits.fuel = 10_000_000_000;
    runtime.run(GUEST, &unbounded_fuel, &request("valid"), &context()).unwrap();

    let cancellation = CancellationToken::new();
    let cancellation_context = ExecutionContext::new(
        ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
        ResourceLimits::default(),
    );
    let cancel_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(25));
        cancellation.cancel();
    });
    let started = Instant::now();
    let cancelled = runtime
        .run(GUEST, &unbounded_fuel, &request("fuel-loop"), &cancellation_context)
        .unwrap_err();
    cancel_thread.join().unwrap();
    assert_eq!(cancelled.code, WasiPluginErrorCode::Cancelled, "{}", cancelled.detail);
    assert!(started.elapsed() < Duration::from_secs(2));

    let timeout_context = ExecutionContext::new(
        ExecutionOptions {
            timeout: Some(Duration::from_millis(25)),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let started = Instant::now();
    let timed_out =
        runtime.run(GUEST, &unbounded_fuel, &request("fuel-loop"), &timeout_context).unwrap_err();
    assert_eq!(timed_out.code, WasiPluginErrorCode::Timeout, "{}", timed_out.detail);
    assert!(started.elapsed() < Duration::from_secs(2));
    runtime.run(GUEST, &unbounded_fuel, &request("valid"), &context()).unwrap();
}

#[test]
fn cancelling_a_gate_waiter_does_not_interrupt_the_active_guest() {
    let runtime = Arc::new(WasiPluginRuntime::new().unwrap());
    let mut unbounded_fuel = manifest();
    unbounded_fuel.limits.fuel = 10_000_000_000;
    runtime.run(GUEST, &unbounded_fuel, &request("valid"), &context()).unwrap();

    let active_cancellation = CancellationToken::new();
    let active_context = ExecutionContext::new(
        ExecutionOptions {
            cancellation: active_cancellation.clone(),
            timeout: Some(Duration::from_secs(2)),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let active_runtime = Arc::clone(&runtime);
    let active_manifest = unbounded_fuel.clone();
    let active = std::thread::spawn(move || {
        active_runtime.run(GUEST, &active_manifest, &request("fuel-loop"), &active_context)
    });
    std::thread::sleep(Duration::from_millis(50));
    assert!(!active.is_finished(), "active guest did not acquire the execution gate");

    let waiting_cancellation = CancellationToken::new();
    let waiting_context = ExecutionContext::new(
        ExecutionOptions {
            cancellation: waiting_cancellation.clone(),
            ..ExecutionOptions::default()
        },
        ResourceLimits::default(),
    );
    let waiting_runtime = Arc::clone(&runtime);
    let waiting_manifest = unbounded_fuel.clone();
    let waiting = std::thread::spawn(move || {
        waiting_runtime.run(GUEST, &waiting_manifest, &request("valid"), &waiting_context)
    });
    std::thread::sleep(Duration::from_millis(25));
    waiting_cancellation.cancel();
    let waiting_error = waiting.join().unwrap().unwrap_err();
    assert_eq!(waiting_error.code, WasiPluginErrorCode::Cancelled);
    assert!(!active.is_finished(), "cancelling a waiter interrupted the active guest");

    active_cancellation.cancel();
    let active_error = active.join().unwrap().unwrap_err();
    assert_eq!(active_error.code, WasiPluginErrorCode::Cancelled);
    runtime.run(GUEST, &unbounded_fuel, &request("valid"), &context()).unwrap();
}
