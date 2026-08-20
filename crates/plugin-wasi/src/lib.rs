// SPDX-License-Identifier: Apache-2.0
//! Capability-scoped WASI Preview 2 plugin execution.
//!
//! Plugins are WebAssembly components implementing `wasi:cli/run@0.2.x`.
//! The request and response are bounded JSON envelopes on stdin/stdout. WASI
//! interfaces are always linkable so standard command components instantiate;
//! calls without a matching manifest capability trap closed.

use bytes::Bytes;
use into_markdown_core::{
    Document, ExecutionContext, IrErrorCode, ResourceReservation, ValidationLimits,
};
use serde::de::{Deserializer as _, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component as PathComponent, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::io::AsyncWrite;
use wasmtime::component::{Component, HasData, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder, Trap};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream, WasiCli, WasiCliView as _};
use wasmtime_wasi::clocks::WasiClocksView as _;
use wasmtime_wasi::filesystem::{
    Descriptor, Dir as WasiDir, OpenMode, WasiFilesystem, WasiFilesystemView as _,
};
use wasmtime_wasi::p2::DynPollable;
use wasmtime_wasi::p2::bindings;
use wasmtime_wasi::p2::pipe::MemoryInputPipe;
use wasmtime_wasi::random::WasiRandomView as _;
use wasmtime_wasi::sockets::{SocketAddrUse, WasiSockets, WasiSocketsView as _};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{OutputStream, StreamError};

/// Protocol version accepted on the JSON request and response boundary.
pub const PROTOCOL_VERSION: u32 = 1;
/// Exact runtime version reviewed by the repository authority.
pub const WASMTIME_VERSION: &str = "39.0.1";
/// Maximum manifest-controlled linear-memory allowance.
pub const MAX_LINEAR_MEMORY_BYTES: usize = 512 * 1024 * 1024;
/// Maximum manifest-controlled stdout or stderr allowance.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Absolute source bytes accepted by the WASI transport boundary.
pub const MAX_PLUGIN_INPUT_BYTES: usize = 128 * 1024 * 1024;
/// Absolute component bytes accepted before hashing or compilation.
pub const MAX_COMPONENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_FUEL: u64 = 10_000_000_000;
const MAX_SOURCE_NAME_BYTES: usize = 4096;
// Compilation is request-local, not cached. This conservative charge covers
// the component bytes plus Wasmtime's transient compiled representation.
const COMPONENT_COMPILE_ACCOUNTING_MULTIPLIER: u64 = 32;
// A JSON byte can introduce at most one scalar/container slot. The 256-byte
// charge covers the simultaneous peak of serde_json Value scalar + map/sequence
// slot and geometric capacity, core structural-preflight bookkeeping,
// `from_value`'s typed Document slot/vector capacity, and the decoded String.
// Punctuation makes dense nested/wide inputs more expensive in bytes, never less.
const DOCUMENT_PARSE_BYTES_PER_JSON_BYTE: u64 = 256;
// Each resource JSON byte is charged for the raw buffer, decoded field/vector
// storage (2x geometric growth), normalized case-fold key, temporary device-name
// fold and SHA state, plus BTree node/pointer storage. Even an all-empty resource
// consumes enough JSON syntax bytes for this 16x bound to dominate fixed slots.
const RESOURCE_MATERIALIZE_BYTES_PER_RAW_BYTE: u64 = 16;
const CLOCKS_DENIED: &str = "capability denied: clocks";
const RANDOM_DENIED: &str = "capability denied: random";
const SUPPORTED_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
];

/// Stable machine-readable WASI plugin error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum WasiPluginErrorCode {
    /// Manifest fields, grants, or target declarations are invalid.
    InvalidManifest,
    /// The current host target is not explicitly supported.
    UnsupportedPlatform,
    /// Component bytes do not match the manifest digest.
    HashMismatch,
    /// The JSON protocol version is unsupported.
    ProtocolMismatch,
    /// The component imports an interface not granted by its manifest.
    CapabilityDenied,
    /// A non-WASI or unsupported host interface was requested.
    InvalidHostcall,
    /// A deterministic fuel allowance was exhausted.
    FuelExhausted,
    /// Linear memory, output, resource, or structural limits were exceeded.
    ResourceLimit,
    /// Guest linear memory was accessed out of bounds.
    MemoryOutOfBounds,
    /// The request was cancelled.
    Cancelled,
    /// The request deadline elapsed.
    Timeout,
    /// The guest returned `err` from `wasi:cli/run` or trapped.
    GuestFailure,
    /// The response envelope was malformed or exceeded its contract.
    InvalidOutput,
    /// Returned Document IR or provenance was invalid.
    InvalidIr,
    /// A granted preopen could not be established.
    Io,
    /// Wasmtime could not compile or instantiate a valid component.
    Runtime,
}

impl WasiPluginErrorCode {
    /// Stable lower-camel-case code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidManifest => "invalidManifest",
            Self::UnsupportedPlatform => "unsupportedPlatform",
            Self::HashMismatch => "hashMismatch",
            Self::ProtocolMismatch => "protocolMismatch",
            Self::CapabilityDenied => "capabilityDenied",
            Self::InvalidHostcall => "invalidHostcall",
            Self::FuelExhausted => "fuelExhausted",
            Self::ResourceLimit => "resourceLimit",
            Self::MemoryOutOfBounds => "memoryOutOfBounds",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::GuestFailure => "guestFailure",
            Self::InvalidOutput => "invalidOutput",
            Self::InvalidIr => "invalidIr",
            Self::Io => "io",
            Self::Runtime => "runtime",
        }
    }
}

/// Controlled failure from the untrusted component boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasiPluginError {
    /// Stable category for callers and HTTP/CLI adapters.
    pub code: WasiPluginErrorCode,
    /// Sanitized human-readable detail.
    pub detail: String,
}

impl std::fmt::Display for WasiPluginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for WasiPluginError {}

fn failure(code: WasiPluginErrorCode, detail: impl Into<String>) -> WasiPluginError {
    WasiPluginError { code, detail: detail.into() }
}

/// One capability-scoped preopened host directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreopenGrant {
    /// Canonical host directory supplied by the trusted installer/caller.
    pub host_path: PathBuf,
    /// Absolute guest path, for example `/input`.
    pub guest_path: String,
    /// Permit directory and file mutations beneath the capability root.
    #[serde(default)]
    pub writable: bool,
}

/// One explicitly allowed network endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkGrant {
    /// Literal IP address. DNS names are intentionally not accepted.
    pub address: IpAddr,
    /// Exact TCP port.
    pub port: u16,
    /// Allow private, loopback, link-local, or unspecified addresses.
    #[serde(default)]
    pub allow_private: bool,
}

/// Manifest-declared WASI authority. Every field defaults to no authority.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct WasiCapabilities {
    /// Preopened directories visible to `wasi:filesystem`.
    pub preopens: Vec<PreopenGrant>,
    /// Link wall-clock and monotonic-clock interfaces.
    pub clocks: bool,
    /// Link secure and insecure random interfaces.
    pub random: bool,
    /// Link TCP interfaces for these exact socket addresses.
    pub network: Vec<NetworkGrant>,
}

/// Per-invocation Wasmtime limits fixed by the reviewed plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WasiLimits {
    /// Deterministic WebAssembly fuel.
    pub fuel: u64,
    /// Maximum aggregate linear-memory bytes in the store.
    pub max_linear_memory_bytes: usize,
    /// Maximum stdout bytes, including the response envelope.
    pub max_output_bytes: usize,
    /// Maximum stderr diagnostic bytes.
    pub max_stderr_bytes: usize,
    /// Maximum returned resource count.
    pub max_resources: usize,
    /// Maximum bytes across returned resources.
    pub max_resource_bytes: usize,
}

/// Installed WASI plugin execution manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WasiPluginManifest {
    /// Stable plugin identifier.
    pub id: String,
    /// Must be `wasi-v1`.
    pub protocol: String,
    /// Must be `preview2`.
    pub wasi_preview: String,
    /// Exact Wasmtime version required by this host.
    pub runtime_version: String,
    /// SHA-256 of the exact component bytes.
    pub component_sha256: String,
    /// Exact component byte length, checked before hashing or compilation.
    pub component_bytes: u64,
    /// Explicit supported Rust target triples.
    pub supported_targets: BTreeSet<String>,
    /// Capability grants.
    #[serde(default)]
    pub capabilities: WasiCapabilities,
    /// Execution limits.
    pub limits: WasiLimits,
}

/// Versioned request serialized to the guest's stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginRequest {
    /// Must equal [`PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Stable non-secret source label.
    pub source_name: String,
    /// Exact source bytes.
    pub input: Vec<u8>,
}

/// One returned resource validated independently of Document IR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginResource {
    /// Safe relative resource path using `/` separators.
    pub path: String,
    /// Bounded media type.
    pub media_type: String,
    /// Exact bytes.
    pub bytes: Vec<u8>,
    /// SHA-256 of `bytes`.
    pub sha256: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PluginResponse {
    protocol_version: u32,
    document_json: String,
    #[serde(default)]
    resources: Vec<PluginResource>,
}

/// Fully validated result of one plugin invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRunOutput {
    /// Valid unified Document IR.
    pub document: Document,
    /// Bounded and hash-authenticated resources.
    pub resources: Vec<PluginResource>,
}

/// Pinned Wasmtime execution engine.
pub struct WasiPluginRuntime {
    engine: Engine,
    execution_gate: ExecutionGate,
    // Process-level cache contract: exactly one <=16 MiB component and its
    // conservatively bounded 32x compiled representation may be retained.
    compiled: Mutex<Option<(String, Component)>>,
}

impl WasiPluginRuntime {
    /// Construct a deterministic, interruptible engine for untrusted components.
    ///
    /// # Errors
    /// Returns [`WasiPluginErrorCode::Runtime`] if the pinned engine cannot initialize.
    pub fn new() -> Result<Self, WasiPluginError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.wasm_memory64(false);
        config.cranelift_nan_canonicalization(true);
        config.max_wasm_stack(512 * 1024);
        Engine::new(&config)
            .map(|engine| Self {
                engine,
                execution_gate: ExecutionGate::default(),
                compiled: Mutex::new(None),
            })
            .map_err(|error| failure(WasiPluginErrorCode::Runtime, error.to_string()))
    }

    /// Execute one hash-pinned WASI Preview 2 command component.
    ///
    /// # Errors
    /// Returns a stable [`WasiPluginErrorCode`] for every manifest, capability,
    /// resource, trap, protocol, or IR failure.
    pub fn run(
        &self,
        component_bytes: &[u8],
        manifest: &WasiPluginManifest,
        request: &PluginRequest,
        execution: &ExecutionContext,
    ) -> Result<PluginRunOutput, WasiPluginError> {
        self.run_inner(component_bytes, manifest, request, execution, || {})
    }

    fn run_inner<F: FnOnce()>(
        &self,
        component_bytes: &[u8],
        manifest: &WasiPluginManifest,
        request: &PluginRequest,
        execution: &ExecutionContext,
        preopen_barrier: F,
    ) -> Result<PluginRunOutput, WasiPluginError> {
        validate_manifest(manifest, component_bytes)?;
        validate_request_protocol(request)?;
        let _execution_guard = self.execution_gate.acquire(execution)?;
        let (input, mut memory_reservation) = prepare_request(manifest, request, execution)?;
        let stdout = BoundedOutput::new(manifest.limits.max_output_bytes);
        let stderr = BoundedOutput::new(manifest.limits.max_stderr_bytes);
        let mut wasi = WasiCtxBuilder::new();
        wasi.stdin(MemoryInputPipe::new(input));
        wasi.stdout(stdout.clone());
        wasi.stderr(stderr.clone());
        let preopens = resolve_preopens(&manifest.capabilities.preopens)?;
        preopen_barrier();
        configure_capabilities(&mut wasi, &manifest.capabilities);

        let component = {
            let mut compiled =
                self.compiled.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((digest, component)) = compiled.as_ref() {
                if constant_time_eq(digest.as_bytes(), manifest.component_sha256.as_bytes()) {
                    component.clone()
                } else {
                    let component =
                        Component::from_binary(&self.engine, component_bytes).map_err(|error| {
                            failure(WasiPluginErrorCode::Runtime, error.to_string())
                        })?;
                    *compiled = Some((manifest.component_sha256.clone(), component.clone()));
                    component
                }
            } else {
                let component = Component::from_binary(&self.engine, component_bytes)
                    .map_err(|error| failure(WasiPluginErrorCode::Runtime, error.to_string()))?;
                *compiled = Some((manifest.component_sha256.clone(), component.clone()));
                component
            }
        };
        execution.checkpoint().map_err(map_execution_error)?;
        let limit_denied = Arc::new(AtomicBool::new(false));
        let limits = StoreLimitsBuilder::new()
            .memory_size(manifest.limits.max_linear_memory_bytes)
            .memories(1)
            .instances(8)
            .tables(16)
            .build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                ctx: wasi.build(),
                table: ResourceTable::new(),
                limits: TrackingLimits { inner: limits, denied: Arc::clone(&limit_denied) },
                allow_clocks: manifest.capabilities.clocks,
                allow_random: manifest.capabilities.random,
                preopens,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(manifest.limits.fuel)
            .map_err(|error| failure(WasiPluginErrorCode::Runtime, error.to_string()))?;
        store.set_epoch_deadline(1);
        let watcher = EpochWatcher::start(self.engine.clone(), execution.clone())?;
        let mut linker = Linker::new(&self.engine);
        add_base_interfaces(&mut linker)?;
        add_capability_interfaces(&mut linker, &manifest.capabilities)?;
        let command = bindings::sync::Command::instantiate(&mut store, &component, &linker)
            .map_err(|error| classify_wasmtime(&error, watcher.reason()))?;
        let run_result = match command.wasi_cli_run().call_run(&mut store) {
            Ok(result) => result,
            Err(error) => {
                let reason = watcher.reason();
                drop(watcher);
                if stdout.overflowed()
                    || stderr.overflowed()
                    || limit_denied.load(Ordering::Acquire)
                {
                    return Err(failure(
                        WasiPluginErrorCode::ResourceLimit,
                        "guest output limit exceeded",
                    ));
                }
                return Err(classify_wasmtime(&error, reason));
            }
        };
        drop(watcher);
        let output = stdout.take_contents();
        if run_result.is_err() {
            if stdout.overflowed()
                || stderr.overflowed()
                || output.len() == manifest.limits.max_output_bytes
                || (manifest.limits.max_stderr_bytes > 0
                    && stderr.len() == manifest.limits.max_stderr_bytes)
            {
                return Err(failure(
                    WasiPluginErrorCode::ResourceLimit,
                    "guest output limit exceeded",
                ));
            }
            return Err(failure(WasiPluginErrorCode::GuestFailure, "guest returned failure"));
        }
        decode_response(manifest, execution, &mut memory_reservation, output)
    }
}

#[derive(Default)]
struct ExecutionGate {
    active: Mutex<bool>,
    changed: Condvar,
}

impl ExecutionGate {
    fn acquire<'a>(
        &'a self,
        execution: &ExecutionContext,
    ) -> Result<ExecutionGuard<'a>, WasiPluginError> {
        loop {
            execution.checkpoint().map_err(map_execution_error)?;
            let mut active = self.active.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !*active {
                *active = true;
                return Ok(ExecutionGuard { gate: self });
            }
            let _ = self
                .changed
                .wait_timeout(active, Duration::from_millis(5))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

struct ExecutionGuard<'a> {
    gate: &'a ExecutionGate,
}

impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        *self.gate.active.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        self.gate.changed.notify_one();
    }
}

fn validate_request_protocol(request: &PluginRequest) -> Result<(), WasiPluginError> {
    if request.protocol_version != PROTOCOL_VERSION {
        return Err(failure(
            WasiPluginErrorCode::ProtocolMismatch,
            format!("expected protocol {PROTOCOL_VERSION}, got {}", request.protocol_version),
        ));
    }
    Ok(())
}

fn prepare_request(
    manifest: &WasiPluginManifest,
    request: &PluginRequest,
    execution: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), WasiPluginError> {
    if request.input.len() > MAX_PLUGIN_INPUT_BYTES
        || u64::try_from(request.input.len()).unwrap_or(u64::MAX)
            > execution.resource_limits().max_input_bytes
        || request.source_name.len() > MAX_SOURCE_NAME_BYTES
    {
        return Err(failure(
            WasiPluginErrorCode::ResourceLimit,
            "plugin input exceeds its byte limit",
        ));
    }
    execution.checkpoint().map_err(map_execution_error)?;
    let input_worst_case = serialized_request_upper_bound(request)?;
    let planned_memory = host_memory_plan(manifest, input_worst_case)?;
    let mut reservation = execution.reserve_memory(planned_memory).map_err(map_execution_error)?;
    execution.checkpoint().map_err(map_execution_error)?;
    let input = serde_json::to_vec(request)
        .map_err(|error| failure(WasiPluginErrorCode::InvalidOutput, error.to_string()))?;
    let input_actual = u64::try_from(input.len()).map_err(|_| {
        failure(WasiPluginErrorCode::ResourceLimit, "serialized input size overflow")
    })?;
    reservation
        .shrink(input_worst_case.saturating_sub(input_actual))
        .map_err(map_execution_error)?;
    execution.checkpoint().map_err(map_execution_error)?;
    Ok((input, reservation))
}

struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    limits: TrackingLimits,
    allow_clocks: bool,
    allow_random: bool,
    preopens: Vec<PinnedPreopen>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BorrowedPluginResponse<'a> {
    protocol_version: u32,
    #[serde(borrow)]
    document_json: &'a RawValue,
    #[serde(borrow)]
    resources: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BorrowedResource<'a> {
    #[serde(borrow)]
    path: &'a RawValue,
    #[serde(borrow)]
    media_type: &'a RawValue,
    #[serde(borrow)]
    bytes: &'a RawValue,
    #[serde(borrow)]
    sha256: &'a RawValue,
}

struct TrackingLimits {
    inner: StoreLimits,
    denied: Arc<AtomicBool>,
}

impl ResourceLimiter for TrackingLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let permitted = self.inner.memory_growing(current, desired, maximum)?;
        if !permitted {
            self.denied.store(true, Ordering::Release);
        }
        Ok(permitted)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let permitted = self.inner.table_growing(current, desired, maximum)?;
        if !permitted {
            self.denied.store(true, Ordering::Release);
        }
        Ok(permitted)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

#[derive(Clone)]
struct BoundedOutput {
    capacity: usize,
    buffer: Arc<Mutex<Vec<u8>>>,
    overflowed: Arc<AtomicBool>,
}

impl BoundedOutput {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            buffer: Arc::new(Mutex::new(Vec::with_capacity(capacity))),
            overflowed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn take_contents(&self) -> Vec<u8> {
        std::mem::take(&mut *self.buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn len(&self) -> usize {
        self.buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }

    fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }
}

impl IsTerminal for BoundedOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for BoundedOutput {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }

    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for BoundedOutput {
    async fn ready(&mut self) {}
}

impl OutputStream for BoundedOutput {
    fn write(&mut self, bytes: Bytes) -> Result<(), StreamError> {
        let mut buffer = self.buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if bytes.len() > self.capacity.saturating_sub(buffer.len()) {
            self.overflowed.store(true, Ordering::Release);
            return Err(StreamError::Closed);
        }
        buffer.extend_from_slice(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StreamError> {
        Ok(())
    }

    fn check_write(&mut self) -> Result<usize, StreamError> {
        let used = self.buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len();
        if used >= self.capacity {
            self.overflowed.store(true, Ordering::Release);
            Err(StreamError::Closed)
        } else {
            Ok(self.capacity - used)
        }
    }
}

impl AsyncWrite for BoundedOutput {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut buffer = self.buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = self.capacity.saturating_sub(buffer.len());
        if remaining == 0 {
            self.overflowed.store(true, Ordering::Release);
            return Poll::Ready(Err(std::io::Error::other("WASI output limit exceeded")));
        }
        let written = remaining.min(bytes.len());
        buffer.extend_from_slice(&bytes[..written]);
        if written < bytes.len() {
            self.overflowed.store(true, Ordering::Release);
        }
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.ctx, table: &mut self.table }
    }
}

impl bindings::filesystem::preopens::Host for HostState {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<Descriptor>, String)>> {
        self.preopens
            .iter()
            .map(|preopen| {
                let descriptor = self.table.push(Descriptor::Dir(preopen.dir.clone()))?;
                Ok((descriptor, preopen.guest_path.clone()))
            })
            .collect()
    }
}

impl bindings::clocks::wall_clock::Host for HostState {
    fn now(&mut self) -> wasmtime::Result<bindings::clocks::wall_clock::Datetime> {
        if !self.allow_clocks {
            return Err(wasmtime::Error::msg(CLOCKS_DENIED));
        }
        bindings::clocks::wall_clock::Host::now(&mut self.clocks())
    }

    fn resolution(&mut self) -> wasmtime::Result<bindings::clocks::wall_clock::Datetime> {
        if !self.allow_clocks {
            return Err(wasmtime::Error::msg(CLOCKS_DENIED));
        }
        bindings::clocks::wall_clock::Host::resolution(&mut self.clocks())
    }
}

impl bindings::clocks::monotonic_clock::Host for HostState {
    fn now(&mut self) -> wasmtime::Result<bindings::clocks::monotonic_clock::Instant> {
        if !self.allow_clocks {
            return Err(wasmtime::Error::msg(CLOCKS_DENIED));
        }
        bindings::clocks::monotonic_clock::Host::now(&mut self.clocks())
    }

    fn resolution(&mut self) -> wasmtime::Result<bindings::clocks::monotonic_clock::Instant> {
        if !self.allow_clocks {
            return Err(wasmtime::Error::msg(CLOCKS_DENIED));
        }
        bindings::clocks::monotonic_clock::Host::resolution(&mut self.clocks())
    }

    fn subscribe_instant(
        &mut self,
        when: bindings::clocks::monotonic_clock::Instant,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        if !self.allow_clocks {
            return Err(wasmtime::Error::msg(CLOCKS_DENIED));
        }
        bindings::clocks::monotonic_clock::Host::subscribe_instant(&mut self.clocks(), when)
    }

    fn subscribe_duration(
        &mut self,
        duration: bindings::clocks::monotonic_clock::Duration,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        if !self.allow_clocks {
            return Err(wasmtime::Error::msg(CLOCKS_DENIED));
        }
        bindings::clocks::monotonic_clock::Host::subscribe_duration(&mut self.clocks(), duration)
    }
}

impl bindings::random::random::Host for HostState {
    fn get_random_bytes(&mut self, len: u64) -> wasmtime::Result<Vec<u8>> {
        if !self.allow_random {
            return Err(wasmtime::Error::msg(RANDOM_DENIED));
        }
        bindings::random::random::Host::get_random_bytes(&mut self.random(), len)
    }

    fn get_random_u64(&mut self) -> wasmtime::Result<u64> {
        if !self.allow_random {
            return Err(wasmtime::Error::msg(RANDOM_DENIED));
        }
        bindings::random::random::Host::get_random_u64(&mut self.random())
    }
}

impl bindings::random::insecure::Host for HostState {
    fn get_insecure_random_bytes(&mut self, len: u64) -> wasmtime::Result<Vec<u8>> {
        if !self.allow_random {
            return Err(wasmtime::Error::msg(RANDOM_DENIED));
        }
        bindings::random::insecure::Host::get_insecure_random_bytes(&mut self.random(), len)
    }

    fn get_insecure_random_u64(&mut self) -> wasmtime::Result<u64> {
        if !self.allow_random {
            return Err(wasmtime::Error::msg(RANDOM_DENIED));
        }
        bindings::random::insecure::Host::get_insecure_random_u64(&mut self.random())
    }
}

impl bindings::random::insecure_seed::Host for HostState {
    fn insecure_seed(&mut self) -> wasmtime::Result<(u64, u64)> {
        if !self.allow_random {
            return Err(wasmtime::Error::msg(RANDOM_DENIED));
        }
        bindings::random::insecure_seed::Host::insecure_seed(&mut self.random())
    }
}

fn validate_manifest(
    manifest: &WasiPluginManifest,
    component: &[u8],
) -> Result<(), WasiPluginError> {
    let component_len = u64::try_from(component.len()).map_err(|_| {
        failure(WasiPluginErrorCode::InvalidManifest, "component size is not representable")
    })?;
    if manifest.component_bytes != component_len || component.len() > MAX_COMPONENT_BYTES {
        return Err(failure(
            WasiPluginErrorCode::InvalidManifest,
            "componentBytes must match and remain within the absolute limit",
        ));
    }
    if manifest.id.is_empty()
        || manifest.id.len() > 128
        || !manifest.id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(failure(WasiPluginErrorCode::InvalidManifest, "invalid plugin id"));
    }
    if manifest.protocol != "wasi-v1" || manifest.wasi_preview != "preview2" {
        return Err(failure(
            WasiPluginErrorCode::InvalidManifest,
            "protocol must be wasi-v1 with WASI preview2",
        ));
    }
    if manifest.runtime_version != WASMTIME_VERSION {
        return Err(failure(
            WasiPluginErrorCode::InvalidManifest,
            format!("runtimeVersion must be {WASMTIME_VERSION}"),
        ));
    }
    if manifest.component_sha256.len() != 64
        || !manifest
            .component_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(failure(
            WasiPluginErrorCode::InvalidManifest,
            "componentSha256 must be 64 lowercase hexadecimal characters",
        ));
    }
    let digest = hex_digest(component);
    if !constant_time_eq(digest.as_bytes(), manifest.component_sha256.as_bytes()) {
        return Err(failure(WasiPluginErrorCode::HashMismatch, "component digest mismatch"));
    }
    if manifest.supported_targets.is_empty()
        || !manifest
            .supported_targets
            .iter()
            .all(|target| SUPPORTED_TARGETS.contains(&target.as_str()))
    {
        return Err(failure(
            WasiPluginErrorCode::InvalidManifest,
            "supportedTargets contains an unreviewed target",
        ));
    }
    if !manifest.supported_targets.contains(current_target()) {
        return Err(failure(
            WasiPluginErrorCode::UnsupportedPlatform,
            format!("target {} is not declared", current_target()),
        ));
    }
    let limits = &manifest.limits;
    if limits.fuel == 0
        || limits.fuel > MAX_FUEL
        || !(64 * 1024..=MAX_LINEAR_MEMORY_BYTES).contains(&limits.max_linear_memory_bytes)
        || limits.max_output_bytes == 0
        || limits.max_output_bytes > MAX_OUTPUT_BYTES
        || limits.max_stderr_bytes > MAX_OUTPUT_BYTES
        || limits.max_resources > 4096
        || limits.max_resource_bytes > MAX_OUTPUT_BYTES
    {
        return Err(failure(WasiPluginErrorCode::InvalidManifest, "unsafe execution limits"));
    }
    validate_capability_grants(&manifest.capabilities)
}

fn validate_capability_grants(capabilities: &WasiCapabilities) -> Result<(), WasiPluginError> {
    let mut guests = BTreeSet::new();
    for preopen in &capabilities.preopens {
        if !valid_guest_preopen(&preopen.guest_path)
            || preopen.guest_path == "/"
            || !guests.insert(preopen.guest_path.clone())
        {
            return Err(failure(WasiPluginErrorCode::InvalidManifest, "invalid preopen path"));
        }
        if !preopen.host_path.is_absolute() {
            return Err(failure(
                WasiPluginErrorCode::InvalidManifest,
                "preopen host path must be absolute",
            ));
        }
        let _ = PinnedPreopen::new(preopen)?;
    }
    let mut endpoints = BTreeSet::new();
    for grant in &capabilities.network {
        if grant.port == 0 || !endpoints.insert((grant.address, grant.port)) {
            return Err(failure(WasiPluginErrorCode::InvalidManifest, "invalid network grant"));
        }
        if !grant.allow_private && is_private(grant.address) {
            return Err(failure(
                WasiPluginErrorCode::InvalidManifest,
                "private network grant requires allowPrivate",
            ));
        }
    }
    Ok(())
}

fn serialized_request_upper_bound(request: &PluginRequest) -> Result<u64, WasiPluginError> {
    let input = u64::try_from(request.input.len())
        .map_err(|_| failure(WasiPluginErrorCode::ResourceLimit, "input size overflow"))?;
    let source = u64::try_from(request.source_name.len())
        .map_err(|_| failure(WasiPluginErrorCode::ResourceLimit, "source name size overflow"))?;
    input
        .checked_mul(4)
        .and_then(|bytes| source.checked_mul(6).and_then(|name| bytes.checked_add(name)))
        .and_then(|bytes| bytes.checked_add(128))
        .ok_or_else(|| {
            failure(WasiPluginErrorCode::ResourceLimit, "serialized input size overflow")
        })
}

fn host_memory_plan(
    manifest: &WasiPluginManifest,
    serialized_input: u64,
) -> Result<u64, WasiPluginError> {
    let component = manifest.component_bytes.checked_mul(COMPONENT_COMPILE_ACCOUNTING_MULTIPLIER);
    let output = u64::try_from(manifest.limits.max_output_bytes).ok().and_then(|stdout| {
        u64::try_from(manifest.limits.max_stderr_bytes)
            .ok()
            .and_then(|stderr| stdout.checked_add(stderr))
    });
    component
        .and_then(|bytes| bytes.checked_add(serialized_input))
        .and_then(|bytes| output.and_then(|output| bytes.checked_add(output)))
        .and_then(|bytes| {
            u64::try_from(manifest.limits.max_linear_memory_bytes)
                .ok()
                .and_then(|linear| bytes.checked_add(linear))
        })
        .ok_or_else(|| failure(WasiPluginErrorCode::ResourceLimit, "host memory plan overflow"))
}

fn valid_guest_preopen(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(['\\', '\0'])
        && path
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn configure_capabilities(builder: &mut WasiCtxBuilder, capabilities: &WasiCapabilities) {
    builder.allow_udp(false);
    builder.allow_ip_name_lookup(false);
    builder.allow_tcp(!capabilities.network.is_empty());
    let allowed: BTreeSet<SocketAddr> = capabilities
        .network
        .iter()
        .map(|grant| SocketAddr::new(grant.address, grant.port))
        .collect();
    builder.socket_addr_check(move |address, use_kind| {
        let permitted = matches!(use_kind, SocketAddrUse::TcpConnect) && allowed.contains(&address);
        Box::pin(async move { permitted })
    });
}

struct HasIo;

impl HasData for HasIo {
    type Data<'a> = &'a mut ResourceTable;
}

struct HasHostState;

impl HasData for HasHostState {
    type Data<'a> = &'a mut HostState;
}

fn add_base_interfaces(linker: &mut Linker<HostState>) -> Result<(), WasiPluginError> {
    let l = linker;
    let options = bindings::sync::LinkOptions::default();
    let exit_options = bindings::cli::exit::LinkOptions::from(&options);
    wasmtime_wasi_io::bindings::wasi::io::error::add_to_linker::<HostState, HasIo>(l, |state| {
        &mut state.table
    })
    .map_err(link_error)?;
    bindings::sync::io::poll::add_to_linker::<HostState, HasIo>(l, |state| &mut state.table)
        .map_err(link_error)?;
    bindings::sync::io::streams::add_to_linker::<HostState, HasIo>(l, |state| &mut state.table)
        .map_err(link_error)?;
    bindings::cli::exit::add_to_linker::<HostState, WasiCli>(l, &exit_options, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::environment::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::stdin::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::stdout::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::stderr::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::terminal_input::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::terminal_output::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::terminal_stdin::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::terminal_stdout::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    bindings::cli::terminal_stderr::add_to_linker::<HostState, WasiCli>(l, HostState::cli)
        .map_err(link_error)?;
    Ok(())
}

fn add_capability_interfaces(
    linker: &mut Linker<HostState>,
    _capabilities: &WasiCapabilities,
) -> Result<(), WasiPluginError> {
    let l = linker;
    bindings::clocks::wall_clock::add_to_linker::<HostState, HasHostState>(l, |state| state)
        .map_err(link_error)?;
    bindings::clocks::monotonic_clock::add_to_linker::<HostState, HasHostState>(l, |state| state)
        .map_err(link_error)?;
    bindings::random::random::add_to_linker::<HostState, HasHostState>(l, |state| state)
        .map_err(link_error)?;
    bindings::random::insecure::add_to_linker::<HostState, HasHostState>(l, |state| state)
        .map_err(link_error)?;
    bindings::random::insecure_seed::add_to_linker::<HostState, HasHostState>(l, |state| state)
        .map_err(link_error)?;
    bindings::filesystem::preopens::add_to_linker::<HostState, HasHostState>(l, |state| state)
        .map_err(link_error)?;
    bindings::sync::filesystem::types::add_to_linker::<HostState, WasiFilesystem>(
        l,
        HostState::filesystem,
    )
    .map_err(link_error)?;
    let options = bindings::sync::LinkOptions::default();
    let network_options = bindings::sockets::network::LinkOptions::from(&options);
    bindings::sockets::tcp_create_socket::add_to_linker::<HostState, WasiSockets>(
        l,
        HostState::sockets,
    )
    .map_err(link_error)?;
    bindings::sockets::udp_create_socket::add_to_linker::<HostState, WasiSockets>(
        l,
        HostState::sockets,
    )
    .map_err(link_error)?;
    bindings::sockets::instance_network::add_to_linker::<HostState, WasiSockets>(
        l,
        HostState::sockets,
    )
    .map_err(link_error)?;
    bindings::sockets::network::add_to_linker::<HostState, WasiSockets>(
        l,
        &network_options,
        HostState::sockets,
    )
    .map_err(link_error)?;
    bindings::sockets::ip_name_lookup::add_to_linker::<HostState, WasiSockets>(
        l,
        HostState::sockets,
    )
    .map_err(link_error)?;
    bindings::sync::sockets::tcp::add_to_linker::<HostState, WasiSockets>(l, HostState::sockets)
        .map_err(link_error)?;
    bindings::sync::sockets::udp::add_to_linker::<HostState, WasiSockets>(l, HostState::sockets)
        .map_err(link_error)?;
    Ok(())
}

fn link_error(error: impl std::fmt::Display) -> WasiPluginError {
    failure(WasiPluginErrorCode::Runtime, error.to_string())
}

fn decode_response(
    manifest: &WasiPluginManifest,
    execution: &ExecutionContext,
    reservation: &mut ResourceReservation,
    output: Vec<u8>,
) -> Result<PluginRunOutput, WasiPluginError> {
    execution.checkpoint().map_err(map_execution_error)?;
    let borrowed: BorrowedPluginResponse<'_> = serde_json::from_slice(&output)
        .map_err(|error| failure(WasiPluginErrorCode::InvalidOutput, error.to_string()))?;
    let response_memory = preflight_response(manifest, &borrowed)?;
    reservation.grow(response_memory).map_err(|error| {
        let mapped = map_execution_error(error);
        if mapped.code == WasiPluginErrorCode::ResourceLimit {
            failure(
                WasiPluginErrorCode::ResourceLimit,
                "response rejected before owned materialization",
            )
        } else {
            mapped
        }
    })?;
    execution.checkpoint().map_err(map_execution_error)?;
    let response: PluginResponse = serde_json::from_slice(&output)
        .map_err(|error| failure(WasiPluginErrorCode::InvalidOutput, error.to_string()))?;
    drop(output);
    validate_response(manifest, response)
}

#[derive(Default)]
struct ResourcePreflight {
    count: usize,
    bytes: usize,
    raw_bytes: usize,
}

fn preflight_response(
    manifest: &WasiPluginManifest,
    response: &BorrowedPluginResponse<'_>,
) -> Result<u64, WasiPluginError> {
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(failure(
            WasiPluginErrorCode::ProtocolMismatch,
            format!("guest returned protocol {}", response.protocol_version),
        ));
    }
    if !response.document_json.get().starts_with('"') {
        return Err(failure(WasiPluginErrorCode::InvalidOutput, "documentJson must be a string"));
    }
    let resources = preflight_resources(manifest, response.resources.get())?;
    let slots = resources.count.checked_next_power_of_two().unwrap_or(usize::MAX);
    let resource_slots = slots
        .checked_mul(std::mem::size_of::<PluginResource>())
        .ok_or_else(|| failure(WasiPluginErrorCode::ResourceLimit, "response memory overflow"))?;
    let document_raw = u64::try_from(response.document_json.get().len())
        .map_err(|_| failure(WasiPluginErrorCode::ResourceLimit, "document size overflow"))?;
    let resources_raw = u64::try_from(resources.raw_bytes)
        .map_err(|_| failure(WasiPluginErrorCode::ResourceLimit, "resource size overflow"))?;
    let slots = u64::try_from(resource_slots)
        .map_err(|_| failure(WasiPluginErrorCode::ResourceLimit, "response memory overflow"))?;
    document_raw
        .checked_mul(DOCUMENT_PARSE_BYTES_PER_JSON_BYTE)
        .and_then(|bytes| bytes.checked_add(document_raw))
        .and_then(|bytes| {
            resources_raw
                .checked_mul(RESOURCE_MATERIALIZE_BYTES_PER_RAW_BYTE)
                .and_then(|resources| bytes.checked_add(resources))
        })
        .and_then(|bytes| bytes.checked_add(slots))
        .ok_or_else(|| failure(WasiPluginErrorCode::ResourceLimit, "response memory overflow"))
}

fn preflight_resources(
    manifest: &WasiPluginManifest,
    json: &str,
) -> Result<ResourcePreflight, WasiPluginError> {
    struct ResourcesVisitor<'a>(&'a WasiPluginManifest);
    impl<'de> Visitor<'de> for ResourcesVisitor<'_> {
        type Value = ResourcePreflight;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an array of plugin resources")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut result = ResourcePreflight::default();
            while let Some(raw) = sequence.next_element::<&RawValue>()? {
                result.count = result
                    .count
                    .checked_add(1)
                    .ok_or_else(|| serde::de::Error::custom("resource count overflow"))?;
                if result.count > self.0.limits.max_resources {
                    return Err(serde::de::Error::custom("too many resources"));
                }
                result.raw_bytes = result
                    .raw_bytes
                    .checked_add(raw.get().len())
                    .ok_or_else(|| serde::de::Error::custom("resource size overflow"))?;
                let resource: BorrowedResource<'_> =
                    serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                for string in [resource.path, resource.media_type, resource.sha256] {
                    if !string.get().starts_with('"') {
                        return Err(serde::de::Error::custom("resource field must be a string"));
                    }
                }
                let count =
                    count_byte_array(resource.bytes.get()).map_err(serde::de::Error::custom)?;
                result.bytes = result
                    .bytes
                    .checked_add(count)
                    .ok_or_else(|| serde::de::Error::custom("resource bytes overflow"))?;
                if result.bytes > self.0.limits.max_resource_bytes {
                    return Err(serde::de::Error::custom("resource bytes exceeded"));
                }
            }
            Ok(result)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(json);
    deserializer
        .deserialize_seq(ResourcesVisitor(manifest))
        .map_err(|error| failure(WasiPluginErrorCode::ResourceLimit, error.to_string()))
}

fn count_byte_array(json: &str) -> Result<usize, serde_json::Error> {
    struct BytesVisitor;
    impl<'de> Visitor<'de> for BytesVisitor {
        type Value = usize;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an array of bytes")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut count = 0usize;
            while sequence.next_element::<u8>()?.is_some() {
                count = count
                    .checked_add(1)
                    .ok_or_else(|| serde::de::Error::custom("byte count overflow"))?;
            }
            Ok(count)
        }
    }
    let mut deserializer = serde_json::Deserializer::from_str(json);
    deserializer.deserialize_seq(BytesVisitor)
}

fn validate_response(
    manifest: &WasiPluginManifest,
    response: PluginResponse,
) -> Result<PluginRunOutput, WasiPluginError> {
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(failure(
            WasiPluginErrorCode::ProtocolMismatch,
            format!("guest returned protocol {}", response.protocol_version),
        ));
    }
    if response.resources.len() > manifest.limits.max_resources {
        return Err(failure(WasiPluginErrorCode::ResourceLimit, "too many resources"));
    }
    let mut total = 0usize;
    let mut paths = BTreeSet::new();
    for resource in &response.resources {
        total = total
            .checked_add(resource.bytes.len())
            .ok_or_else(|| failure(WasiPluginErrorCode::ResourceLimit, "resource size overflow"))?;
        if total > manifest.limits.max_resource_bytes {
            return Err(failure(WasiPluginErrorCode::ResourceLimit, "resource bytes exceeded"));
        }
        let path_key = portable_resource_path_key(&resource.path);
        if path_key.is_none()
            || !strict_media_type(&resource.media_type)
            || !paths.insert(path_key.unwrap_or_default())
            || !constant_time_eq(hex_digest(&resource.bytes).as_bytes(), resource.sha256.as_bytes())
        {
            return Err(failure(WasiPluginErrorCode::InvalidOutput, "invalid resource"));
        }
    }
    if response.document_json.len() > manifest.limits.max_output_bytes {
        return Err(failure(WasiPluginErrorCode::ResourceLimit, "document JSON exceeded"));
    }
    let document_value: serde_json::Value = serde_json::from_str(&response.document_json)
        .map_err(|error| failure(WasiPluginErrorCode::InvalidIr, error.to_string()))?;
    validate_plugin_provenance(&document_value, &manifest.id)?;
    drop(document_value);
    let limits = ValidationLimits {
        max_json_bytes: manifest.limits.max_output_bytes,
        ..ValidationLimits::default()
    };
    let document =
        Document::from_json_with_limits(&response.document_json, &limits).map_err(|error| {
            let code = if error.code == IrErrorCode::ResourceLimit {
                WasiPluginErrorCode::ResourceLimit
            } else {
                WasiPluginErrorCode::InvalidIr
            };
            failure(code, format!("{} at {}", error.code.as_str(), error.path))
        })?;
    Ok(PluginRunOutput { document, resources: response.resources })
}

fn validate_plugin_provenance(
    value: &serde_json::Value,
    plugin_id: &str,
) -> Result<(), WasiPluginError> {
    fn visit(
        value: &serde_json::Value,
        expected: &str,
        found: &mut usize,
    ) -> Result<(), WasiPluginError> {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(provenance) = map.get("provenance") {
                    *found += 1;
                    let provider = provenance.get("provider").and_then(serde_json::Value::as_str);
                    if provider != Some(expected) {
                        return Err(failure(
                            WasiPluginErrorCode::InvalidIr,
                            "plugin provenance provider mismatch",
                        ));
                    }
                }
                for child in map.values() {
                    visit(child, expected, found)?;
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    visit(child, expected, found)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut found = 0;
    visit(value, plugin_id, &mut found)?;
    if value
        .get("blocks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|blocks| !blocks.is_empty())
        && found == 0
    {
        return Err(failure(WasiPluginErrorCode::InvalidIr, "plugin output lacks provenance"));
    }
    Ok(())
}

fn portable_resource_path_key(path: &str) -> Option<String> {
    if path.is_empty()
        || path.len() > 1024
        || path.starts_with('/')
        || path.contains(['\\', '\0'])
        || path.chars().any(char::is_control)
    {
        return None;
    }
    let mut key = String::with_capacity(path.len());
    for (index, segment) in path.split('/').enumerate() {
        if segment.is_empty()
            || segment.len() > 240
            || segment == "."
            || segment == ".."
            || segment.ends_with(['.', ' '])
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || windows_device_name(segment)
        {
            return None;
        }
        if index > 0 {
            key.push('/');
        }
        key.extend(segment.chars().map(|value| value.to_ascii_lowercase()));
    }
    Some(key)
}

#[derive(Clone)]
struct PinnedPreopen {
    guest_path: String,
    dir: WasiDir,
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct PreopenIdentity {
    device: u64,
    inode: u64,
}

impl PinnedPreopen {
    fn new(grant: &PreopenGrant) -> Result<Self, WasiPluginError> {
        let canonical = std::fs::canonicalize(&grant.host_path)
            .map_err(|_| failure(WasiPluginErrorCode::Io, "preopen is unavailable"))?;
        if !same_canonical_path(&grant.host_path, &canonical) {
            return Err(failure(WasiPluginErrorCode::InvalidManifest, "preopen is not canonical"));
        }
        let path_metadata = private_preopen_metadata(&canonical)?;
        let file = open_directory_handle_nofollow(&canonical)?;
        let handle_metadata = file
            .metadata()
            .map_err(|_| failure(WasiPluginErrorCode::Io, "preopen identity unavailable"))?;
        validate_private_preopen_metadata(&handle_metadata)?;
        #[cfg(windows)]
        let _ = &path_metadata;
        #[cfg(unix)]
        if preopen_identity(&path_metadata) != preopen_identity(&handle_metadata) {
            return Err(failure(WasiPluginErrorCode::Io, "preopen changed while opening"));
        }
        let dir_perms =
            if grant.writable { DirPerms::READ | DirPerms::MUTATE } else { DirPerms::READ };
        let file_perms =
            if grant.writable { FilePerms::READ | FilePerms::WRITE } else { FilePerms::READ };
        let open_mode =
            if grant.writable { OpenMode::READ | OpenMode::WRITE } else { OpenMode::READ };
        Ok(Self {
            guest_path: grant.guest_path.clone(),
            dir: WasiDir::new(
                cap_std::fs::Dir::from_std_file(file),
                dir_perms,
                file_perms,
                open_mode,
                true,
            ),
        })
    }
}

fn resolve_preopens(grants: &[PreopenGrant]) -> Result<Vec<PinnedPreopen>, WasiPluginError> {
    grants.iter().map(PinnedPreopen::new).collect()
}

fn open_directory_handle_nofollow(path: &Path) -> Result<std::fs::File, WasiPluginError> {
    let mut root = PathBuf::new();
    let mut normal = Vec::new();
    for component in path.components() {
        match component {
            PathComponent::Prefix(prefix) => root.push(prefix.as_os_str()),
            PathComponent::RootDir => root.push(component.as_os_str()),
            PathComponent::Normal(name) => normal.push(name.to_owned()),
            PathComponent::CurDir | PathComponent::ParentDir => {
                return Err(failure(
                    WasiPluginErrorCode::InvalidManifest,
                    "preopen is not canonical",
                ));
            }
        }
    }
    if root.as_os_str().is_empty() {
        return Err(failure(WasiPluginErrorCode::InvalidManifest, "preopen has no trusted root"));
    }
    let mut dir = cap_primitives::fs::open_ambient_dir(&root, cap_std::ambient_authority())
        .map_err(|_| failure(WasiPluginErrorCode::Io, "preopen root is unavailable"))?;
    for name in normal {
        dir = cap_primitives::fs::open_dir_nofollow(&dir, Path::new(&name)).map_err(|_| {
            failure(WasiPluginErrorCode::InvalidManifest, "preopen contains a link")
        })?;
    }
    Ok(dir)
}

#[cfg(unix)]
fn same_canonical_path(requested: &Path, canonical: &Path) -> bool {
    requested == canonical
}

#[cfg(windows)]
fn same_canonical_path(requested: &Path, canonical: &Path) -> bool {
    fn normalized(path: &Path) -> String {
        let value = path.to_string_lossy();
        value.strip_prefix(r"\\?\").unwrap_or(&value).replace('/', "\\").to_ascii_lowercase()
    }
    normalized(requested) == normalized(canonical)
}

fn private_preopen_metadata(path: &Path) -> Result<std::fs::Metadata, WasiPluginError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| failure(WasiPluginErrorCode::Io, "preopen is unavailable"))?;
    validate_private_preopen_metadata(&metadata)?;
    Ok(metadata)
}

fn validate_private_preopen_metadata(metadata: &std::fs::Metadata) -> Result<(), WasiPluginError> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(failure(WasiPluginErrorCode::Io, "preopen is not a private directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
            return Err(failure(WasiPluginErrorCode::Io, "preopen is not a private directory"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(failure(WasiPluginErrorCode::Io, "preopen is a reparse point"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn preopen_identity(metadata: &std::fs::Metadata) -> PreopenIdentity {
    use std::os::unix::fs::MetadataExt as _;
    PreopenIdentity { device: metadata.dev(), inode: metadata.ino() }
}

fn windows_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment).trim_end_matches(['.', ' ']);
    let folded = stem.to_ascii_uppercase();
    matches!(folded.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || folded.strip_prefix("COM").or_else(|| folded.strip_prefix("LPT")).is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

fn strict_media_type(value: &str) -> bool {
    let mut parts = value.split('/');
    let Some(kind) = parts.next() else { return false };
    let Some(subtype) = parts.next() else { return false };
    parts.next().is_none() && mime_token(kind) && mime_token(subtype)
}

fn mime_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-')
        })
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
}

fn is_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
                || ip.is_broadcast()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn current_target() -> &'static str {
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

fn map_execution_error(error: into_markdown_core::ConversionError) -> WasiPluginError {
    match error {
        into_markdown_core::ConversionError::Cancelled => {
            failure(WasiPluginErrorCode::Cancelled, "plugin execution cancelled")
        }
        into_markdown_core::ConversionError::Timeout => {
            failure(WasiPluginErrorCode::Timeout, "plugin execution timed out")
        }
        other @ into_markdown_core::ConversionError::ResourceLimit { .. } => {
            failure(WasiPluginErrorCode::ResourceLimit, other.to_string())
        }
        other => failure(WasiPluginErrorCode::Runtime, other.to_string()),
    }
}

fn classify_wasmtime(error: &wasmtime::Error, reason: u8) -> WasiPluginError {
    if reason == 1 {
        return failure(WasiPluginErrorCode::Cancelled, "plugin execution cancelled");
    }
    if reason == 2 {
        return failure(WasiPluginErrorCode::Timeout, "plugin execution timed out");
    }
    let detail = format!("{error:#}");
    if detail.contains(CLOCKS_DENIED) || detail.contains(RANDOM_DENIED) {
        return failure(WasiPluginErrorCode::CapabilityDenied, detail);
    }
    if let Some(trap) = error.downcast_ref::<Trap>() {
        return match trap {
            Trap::OutOfFuel => failure(WasiPluginErrorCode::FuelExhausted, "fuel exhausted"),
            Trap::MemoryOutOfBounds | Trap::TableOutOfBounds => {
                failure(WasiPluginErrorCode::MemoryOutOfBounds, "guest memory out of bounds")
            }
            _ => failure(WasiPluginErrorCode::GuestFailure, trap.to_string()),
        };
    }
    let code = if detail.contains("unknown import")
        || detail.contains("not defined")
        || detail.contains("matching implementation was not found")
        || detail.contains("implementation is missing")
    {
        if detail.contains("component imports instance `wasi:") {
            WasiPluginErrorCode::CapabilityDenied
        } else {
            WasiPluginErrorCode::InvalidHostcall
        }
    } else if detail.contains("write beyond capacity") || detail.contains("resource limit") {
        WasiPluginErrorCode::ResourceLimit
    } else {
        WasiPluginErrorCode::Runtime
    };
    failure(code, detail)
}

struct EpochWatcher {
    state: Arc<(Mutex<bool>, Condvar)>,
    reason: Arc<AtomicU8>,
    thread: Option<JoinHandle<()>>,
}

impl EpochWatcher {
    fn start(engine: Engine, execution: ExecutionContext) -> Result<Self, WasiPluginError> {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let reason = Arc::new(AtomicU8::new(0));
        let state_for_thread = Arc::clone(&state);
        let reason_for_thread = Arc::clone(&reason);
        let thread = std::thread::Builder::new()
            .name("into-markdown-wasi-epoch".into())
            .spawn(move || {
                loop {
                    if let Err(error) = execution.checkpoint() {
                        let value =
                            if matches!(error, into_markdown_core::ConversionError::Cancelled) {
                                1
                            } else {
                                2
                            };
                        reason_for_thread.store(value, Ordering::Release);
                        engine.increment_epoch();
                        break;
                    }
                    let (lock, condition) = &*state_for_thread;
                    let done = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let (done, _) = condition
                        .wait_timeout(done, Duration::from_millis(5))
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if *done {
                        break;
                    }
                }
            })
            .map_err(|_| failure(WasiPluginErrorCode::Runtime, "could not start epoch watcher"))?;
        Ok(Self { state, reason, thread: Some(thread) })
    }

    fn reason(&self) -> u8 {
        self.reason.load(Ordering::Acquire)
    }
}

impl Drop for EpochWatcher {
    fn drop(&mut self) {
        let (lock, condition) = &*self.state;
        *lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        condition.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_memory_plan_overflow_is_a_stable_resource_limit() {
        let mut manifest = WasiPluginManifest {
            id: "fixture".into(),
            protocol: "wasi-v1".into(),
            wasi_preview: "preview2".into(),
            runtime_version: WASMTIME_VERSION.into(),
            component_sha256: "0".repeat(64),
            component_bytes: u64::MAX,
            supported_targets: BTreeSet::new(),
            capabilities: WasiCapabilities::default(),
            limits: WasiLimits {
                fuel: 1,
                max_linear_memory_bytes: 64 * 1024,
                max_output_bytes: 1,
                max_stderr_bytes: 0,
                max_resources: 0,
                max_resource_bytes: 0,
            },
        };
        assert_eq!(
            host_memory_plan(&manifest, 1).unwrap_err().code,
            WasiPluginErrorCode::ResourceLimit
        );
        manifest.component_bytes = 1;
        assert_eq!(
            host_memory_plan(&manifest, u64::MAX).unwrap_err().code,
            WasiPluginErrorCode::ResourceLimit
        );
    }

    #[test]
    fn execution_gate_recovers_and_releases_after_unwind() {
        let gate = ExecutionGate::default();
        let execution = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = gate.acquire(&execution).unwrap();
            panic!("exercise RAII gate release");
        }));
        assert!(panic.is_err());
        drop(gate.acquire(&execution).unwrap());

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _active = gate.active.lock().unwrap();
            panic!("poison execution gate mutex");
        }));
        assert!(poisoned.is_err());
        drop(gate.acquire(&execution).unwrap());
    }

    #[test]
    fn portable_paths_mime_and_preopen_identity_are_fail_closed() {
        for path in [
            "",
            "/root",
            "a//b",
            "a/./b",
            "a/../b",
            "a\\b",
            "a/b. ",
            "CON",
            "aux.txt",
            "x/COM1.bin",
            "x/Lpt9",
            "CLOCK$",
            "x/line\nbreak",
            "x:y",
            "has space",
            "café",
        ] {
            assert!(portable_resource_path_key(path).is_none(), "{path:?}");
        }
        assert!(portable_resource_path_key(&"x".repeat(241)).is_none());
        assert!(portable_resource_path_key(&"x".repeat(240)).is_some());
        assert_eq!(portable_resource_path_key("Asset/X.txt").as_deref(), Some("asset/x.txt"));
        let aliases = ["Asset/X.txt", "asset/x.TXT", "ASSET/x.txt"]
            .into_iter()
            .map(portable_resource_path_key)
            .collect::<BTreeSet<_>>();
        assert_eq!(aliases.len(), 1);
        assert!(strict_media_type("application/vnd.example+json"));
        for mime in ["text", "text/", "/plain", "text/plain; charset=utf-8", "text /plain"] {
            assert!(!strict_media_type(mime), "{mime:?}");
        }

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("grant");
        std::fs::create_dir(&root).unwrap();
        let grant =
            PreopenGrant { host_path: root.clone(), guest_path: "/input".into(), writable: false };
        std::fs::write(root.join("identity"), "old").unwrap();
        let pinned = PinnedPreopen::new(&grant).unwrap();
        let old = parent.path().join("old");
        if std::fs::rename(&root, old).is_ok() {
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("identity"), "replacement").unwrap();
        }
        assert_eq!(pinned.dir.dir.read_to_string("identity").unwrap(), "old");
    }

    #[test]
    fn dense_response_is_preflighted_without_owned_resources() {
        let resource = format!(
            r#"{{"path":"{}.bin","mediaType":"application/octet-stream","bytes":[],"sha256":"{}"}}"#,
            "a".repeat(230),
            "0".repeat(64)
        );
        let resources = std::iter::repeat_n(resource, 16).collect::<Vec<_>>().join(",");
        let json =
            format!(r#"{{"protocolVersion":1,"documentJson":"{{}}","resources":[{resources}]}}"#);
        let borrowed: BorrowedPluginResponse<'_> = serde_json::from_str(&json).unwrap();
        let mut manifest = WasiPluginManifest {
            id: "fixture".into(),
            protocol: "wasi-v1".into(),
            wasi_preview: "preview2".into(),
            runtime_version: WASMTIME_VERSION.into(),
            component_sha256: "0".repeat(64),
            component_bytes: 1,
            supported_targets: BTreeSet::new(),
            capabilities: WasiCapabilities::default(),
            limits: WasiLimits {
                fuel: 1,
                max_linear_memory_bytes: 64 * 1024,
                max_output_bytes: json.len(),
                max_stderr_bytes: 0,
                max_resources: 16,
                max_resource_bytes: 0,
            },
        };
        let plan = preflight_response(&manifest, &borrowed).unwrap();
        assert!(plan >= u64::try_from(resources.len()).unwrap() * 16);
        manifest.limits.max_resources = 15;
        assert_eq!(
            preflight_response(&manifest, &borrowed).unwrap_err().code,
            WasiPluginErrorCode::ResourceLimit
        );
    }

    #[test]
    fn dense_response_succeeds_at_plan_and_fails_one_byte_below_it() {
        let blocks = (0..128)
            .map(|index| {
                serde_json::json!({
                    "id": format!("p{index}"),
                    "block": {"type": "paragraph", "data": []},
                    "provenance": {
                        "kind": "nativeParser",
                        "provider": "fixture",
                        "locator": {
                            "page": null, "slide": null, "sheet": null, "cell": null,
                            "bounds": null, "time": null, "part": null
                        },
                        "confidence": 1.0
                    }
                })
            })
            .collect::<Vec<_>>();
        let document = serde_json::json!({
            "schemaVersion": 1,
            "metadata": {"title": null, "authors": [], "properties": {}},
            "blocks": blocks,
        })
        .to_string();
        let resources = (0..16)
            .map(|index| {
                serde_json::json!({
                    "path": format!("{}-{index:02}.bin", "a".repeat(220)),
                    "mediaType": "application/octet-stream",
                    "bytes": [],
                    "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                })
            })
            .collect::<Vec<_>>();
        let output = serde_json::to_vec(&serde_json::json!({
            "protocolVersion": 1,
            "documentJson": &document,
            "resources": resources,
        }))
        .unwrap();
        let borrowed: BorrowedPluginResponse<'_> = serde_json::from_slice(&output).unwrap();
        let manifest = WasiPluginManifest {
            id: "fixture".into(),
            protocol: "wasi-v1".into(),
            wasi_preview: "preview2".into(),
            runtime_version: WASMTIME_VERSION.into(),
            component_sha256: "0".repeat(64),
            component_bytes: 1,
            supported_targets: BTreeSet::new(),
            capabilities: WasiCapabilities::default(),
            limits: WasiLimits {
                fuel: 1,
                max_linear_memory_bytes: 64 * 1024,
                max_output_bytes: output.len(),
                max_stderr_bytes: 0,
                max_resources: 16,
                max_resource_bytes: 0,
            },
        };
        let plan = preflight_response(&manifest, &borrowed).unwrap();
        let exact = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: plan,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let mut exact_reservation = exact.reserve_memory(0).unwrap();
        assert_eq!(
            decode_response(&manifest, &exact, &mut exact_reservation, output.clone())
                .unwrap()
                .resources
                .len(),
            16
        );
        drop(exact_reservation);
        assert_eq!(exact.reserved_memory_bytes(), 0);

        let below = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: plan - 1,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let mut below_reservation = below.reserve_memory(0).unwrap();
        let error = decode_response(&manifest, &below, &mut below_reservation, output).unwrap_err();
        assert_eq!(error.code, WasiPluginErrorCode::ResourceLimit);
        assert_eq!(error.detail, "response rejected before owned materialization");
        drop(below_reservation);
        assert_eq!(below.reserved_memory_bytes(), 0);
    }

    #[test]
    fn pinned_preopen_guest_reads_old_tree_after_path_replacement() {
        const GUEST: &[u8] = include_bytes!("../tests/fixtures/guest.component.wasm");
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("grant");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("probe.txt"), "preopen-ok").unwrap();
        let old = parent.path().join("old");
        let mut manifest = WasiPluginManifest {
            id: "fixture".into(),
            protocol: "wasi-v1".into(),
            wasi_preview: "preview2".into(),
            runtime_version: WASMTIME_VERSION.into(),
            component_sha256: hex_digest(GUEST),
            component_bytes: u64::try_from(GUEST.len()).unwrap(),
            supported_targets: BTreeSet::from([current_target().into()]),
            capabilities: WasiCapabilities::default(),
            limits: WasiLimits {
                fuel: 50_000_000,
                max_linear_memory_bytes: 32 * 1024 * 1024,
                max_output_bytes: 1024 * 1024,
                max_stderr_bytes: 64 * 1024,
                max_resources: 16,
                max_resource_bytes: 1024 * 1024,
            },
        };
        manifest.capabilities.preopens.push(PreopenGrant {
            host_path: root.clone(),
            guest_path: "/input".into(),
            writable: false,
        });
        let request = PluginRequest {
            protocol_version: PROTOCOL_VERSION,
            source_name: "preopen-call".into(),
            input: Vec::new(),
        };
        let execution = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        WasiPluginRuntime::new()
            .unwrap()
            .run_inner(GUEST, &manifest, &request, &execution, || {
                if let Err(error) = std::fs::rename(&root, &old) {
                    #[cfg(not(windows))]
                    panic!("unexpected replacement barrier failure: {error}");
                    #[cfg(windows)]
                    {
                        let _ = error;
                        return;
                    }
                }
                std::fs::create_dir(&root).unwrap();
                std::fs::write(root.join("probe.txt"), "replacement").unwrap();
            })
            .unwrap();
    }
}
