//! Fail-closed host runtime for `process-v1` conversion plugins.
//!
//! Plugins are untrusted executables. The host authenticates the executable, starts it with an
//! empty environment in an operating-system sandbox, and accepts only bounded, versioned,
//! length-prefixed JSON frames. Small sources cross the pipe; larger sources use one
//! request-private, read-only staged file inside the sandbox. Returned resources always cross the
//! pipe.
#![deny(unsafe_op_in_unsafe_fn)]

mod error;
mod memory;
use error::terminal_error_code;
pub use error::{PluginError, PluginErrorCode};
mod protocol;
mod sandbox;
pub mod worker;

use base64::Engine as _;
use into_markdown_core::{
    ConversionResult, Diagnostic, DiagnosticsDto, ExecutionContext, ExecutionStage,
    ResourceReservation, ResultDto,
};
use protocol::{HostMessage, PROTOCOL_V1, PluginMessage};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

const HASH_BUFFER_SIZE: usize = 64 * 1024;
const HASH_BUFFER_BYTES: u64 = 64 * 1024;
const ABSOLUTE_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const ABSOLUTE_RUNTIME_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ABSOLUTE_RUNTIME_ENTRIES: usize = 25_000;
const ABSOLUTE_ADDRESS_SPACE_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;

/// Immutable authority for one installed executable.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    /// Stable lowercase plugin identifier.
    pub plugin_id: String,
    /// Absolute executable path.
    pub executable: PathBuf,
    /// Canonical runtime directory containing the executable and its private libraries.
    pub runtime_root: PathBuf,
    /// Lowercase SHA-256 of the executable.
    pub executable_sha256: String,
    /// Ordered supported protocol versions.
    pub protocol_versions: Vec<u32>,
}

/// Windows `AppContainer` identity provisioned and ACL-authorized by the plugin installer.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct WindowsSandboxAuthority {
    /// `AppContainer` profile name.
    pub profile_name: String,
    /// Expected derived SID.
    pub sid: String,
    /// Canonical `AppContainer` package root used to derive activated private storage.
    pub storage_root: PathBuf,
}

/// Provision the deterministic, zero-capability `AppContainer` used by an installed plugin.
///
/// # Errors
///
/// Returns an error when the profile, SID, storage, or ACL authority cannot be proven.
#[cfg(windows)]
pub fn provision_windows_sandbox(
    scope_plugin_identity: &str,
) -> Result<WindowsSandboxAuthority, PluginError> {
    sandbox::windows::provision(scope_plugin_identity)
}

/// Remove a plugin's deterministic `AppContainer` profile after uninstall commits.
///
/// # Errors
///
/// Returns an error when the profile cannot be safely removed or verified absent.
#[cfg(windows)]
pub fn remove_windows_sandbox(scope_plugin_identity: &str) -> Result<(), PluginError> {
    sandbox::windows::remove_profile(scope_plugin_identity)
}

/// Grant one provisioned `AppContainer` read/execute access to an immutable runtime snapshot.
///
/// # Errors
///
/// Returns an error when any snapshot ACL cannot be installed and verified exactly.
#[cfg(windows)]
pub fn authorize_windows_sandbox_path(
    authority: &WindowsSandboxAuthority,
    path: &Path,
) -> Result<(), PluginError> {
    sandbox::windows::authorize_path(authority, path)
}

/// Verify one runtime snapshot still has the exact current-user plus
/// provisioned `AppContainer` ACL installed by
/// [`authorize_windows_sandbox_path`].
///
/// # Errors
///
/// Returns an error for inherited, broadened, missing, or reparse-point ACLs.
#[cfg(windows)]
pub fn verify_windows_sandbox_path(
    authority: &WindowsSandboxAuthority,
    path: &Path,
) -> Result<(), PluginError> {
    sandbox::windows::verify_authorized_path(authority, path)
}

/// Atomically create one directory with a current-user-only protected DACL.
///
/// # Errors
///
/// Returns an error when creation or the exact owner/DACL verification fails.
#[cfg(windows)]
pub fn create_windows_plugin_store_directory(path: &Path) -> Result<(), PluginError> {
    sandbox::windows::create_private_directory(path)
}

/// Verify a plugin-store path remains owned and writable only by the current user.
///
/// # Errors
///
/// Returns an error when the path identity, owner, or DACL is not exact.
#[cfg(windows)]
pub fn verify_windows_plugin_store_path(path: &Path) -> Result<(), PluginError> {
    sandbox::windows::verify_private_path(path)
}

/// Verify a child inherited only the private plugin-store DACL.
///
/// # Errors
///
/// Returns an error when the child is a link or has unexpected ACL authority.
#[cfg(windows)]
pub fn verify_windows_plugin_store_child(path: &Path) -> Result<(), PluginError> {
    sandbox::windows::verify_private_child(path)
}

/// Verify an existing Windows user-data parent is owned by the current user
/// and grants mutation authority only to that user, SYSTEM, or Administrators.
///
/// # Errors
///
/// Returns an error when the parent identity, owner, or DACL is not trusted.
#[cfg(windows)]
pub fn verify_windows_plugin_trusted_parent(path: &Path) -> Result<(), PluginError> {
    sandbox::windows::verify_trusted_parent(path)
}

/// Atomically rename one sibling below a pinned directory without replacing an
/// existing destination, requesting write-through publication from Windows.
///
/// # Errors
///
/// Returns an error when either name is invalid or the no-replace rename fails.
#[cfg(windows)]
pub fn rename_windows_plugin_file_no_replace(
    directory: &std::fs::File,
    source: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
) -> Result<(), PluginError> {
    sandbox::windows::rename_sibling_no_replace(directory, source, destination)
}

/// Atomically replace one sibling below a pinned private directory, requesting
/// write-through publication from Windows.
///
/// # Errors
///
/// Returns an error when either name is invalid or the replacement fails.
#[cfg(windows)]
pub fn replace_windows_plugin_file(
    directory: &std::fs::File,
    source: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
) -> Result<(), PluginError> {
    sandbox::windows::replace_sibling(directory, source, destination)
}

/// Atomically move one child between two pinned private directories without replacement.
///
/// # Errors
///
/// Returns an error when a name is invalid, a directory identity is unavailable, the volumes
/// differ, or write-through no-replace publication fails.
#[cfg(windows)]
pub fn move_windows_plugin_file_no_replace(
    source_directory: &std::fs::File,
    source: &std::ffi::OsStr,
    destination_directory: &std::fs::File,
    destination: &std::ffi::OsStr,
) -> Result<(), PluginError> {
    sandbox::windows::move_between_no_replace(
        source_directory,
        source,
        destination_directory,
        destination,
    )
}

/// Host-enforced limits and the complete declared environment capability.
#[derive(Debug, Clone)]
pub struct RuntimePolicy {
    /// Maximum encoded JSON bytes in one frame.
    pub max_frame_bytes: u32,
    /// Maximum nested result JSON bytes.
    pub max_output_bytes: u64,
    /// Child physical/job memory ceiling.
    pub max_memory_bytes: u64,
    /// Optional virtual-address ceiling when sparse native mappings require
    /// more address space than the physical memory budget. `None` binds the
    /// address-space ceiling to `max_memory_bytes`.
    pub max_address_space_bytes: Option<u64>,
    /// Maximum size of a child-created file.
    pub max_file_bytes: u64,
    /// Maximum child file descriptors.
    pub max_open_files: u32,
    /// Maximum handshake duration independent of request timeout.
    pub handshake_timeout: Duration,
    /// Hard request duration even when the caller supplied no shorter context deadline.
    pub request_timeout: Duration,
    /// Grace after sending cancellation before forced tree termination.
    pub cancellation_grace: Duration,
    /// Complete explicit environment. No parent variable is inherited.
    pub environment: BTreeMap<OsString, OsString>,
    /// Canonical model directories exposed read-only to the worker.
    pub read_only_roots: Vec<PathBuf>,
    /// Permit provider-owned helpers below the authenticated runtime root.
    pub allow_child_processes: bool,
    /// Permit narrowly scoped macOS compatibility-child services. Ignored on other platforms.
    pub macos_compatibility_child: bool,
    /// Pre-provisioned no-capability `AppContainer` authority.
    #[cfg(windows)]
    pub windows: WindowsSandboxAuthority,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            max_frame_bytes: 16 * 1024 * 1024,
            max_output_bytes: 12 * 1024 * 1024,
            max_memory_bytes: 512 * 1024 * 1024,
            max_address_space_bytes: None,
            max_file_bytes: 64 * 1024 * 1024,
            max_open_files: 64,
            handshake_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_mins(1),
            cancellation_grace: Duration::from_millis(100),
            environment: BTreeMap::new(),
            read_only_roots: Vec::new(),
            allow_child_processes: false,
            macos_compatibility_child: false,
            #[cfg(windows)]
            windows: WindowsSandboxAuthority {
                profile_name: String::new(),
                sid: String::new(),
                storage_root: PathBuf::new(),
            },
        }
    }
}

/// One bounded conversion invocation.
#[derive(Debug, Clone, Copy)]
pub struct PluginRequest<'a> {
    /// Optional request-specific child-memory allowance, backed by a host lease.
    pub memory_limit: Option<u64>,
    /// Stable request ID unique within the host process.
    pub request_id: &'a str,
    /// Stable input-format wire name.
    pub input_format: &'a str,
    /// Display-only source name.
    pub source_name: Option<&'a str>,
    /// Optional bounded capability-specific JSON parameters.
    pub parameters_json: Option<&'a str>,
    /// Authenticated input bytes sent inline.
    pub source: &'a [u8],
}

/// Validated terminal result plus authenticated runtime identity.
#[derive(Debug)]
pub struct PluginExecution {
    /// Fully decoded and validated result DTO.
    pub result: ConversionResult,
    /// Manifest plugin ID matched during handshake.
    pub plugin_id: String,
    /// Executable SHA-256 revalidated immediately before launch.
    pub executable_sha256: String,
}

/// Authenticated raw terminal response for a typed capability adapter.
///
/// The caller must deserialize and validate `result_json` against its exact
/// capability DTO before exposing the result to converters.
#[derive(Debug)]
pub struct RawPluginExecution {
    /// Bounded nested JSON returned by the plugin.
    pub result_json: String,
    /// Independently decoded and validated streamed diagnostics.
    pub diagnostics: Vec<Diagnostic>,
    /// Manifest plugin ID matched during handshake.
    pub plugin_id: String,
    /// Executable SHA-256 revalidated immediately before launch.
    pub executable_sha256: String,
}

/// Reusable authenticated process-plugin runtime.
#[derive(Debug, Clone)]
pub struct ProcessPlugin {
    plugin: ValidatedPlugin,
    policy: RuntimePolicy,
    runtime_staging: RuntimeStaging,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeStaging {
    CopyBeforeLaunch,
    ManagerVerifiedPrivateSnapshot,
}

#[derive(Debug, Clone)]
struct ValidatedPlugin {
    plugin_id: String,
    executable: PathBuf,
    runtime_root: PathBuf,
    executable_sha256: String,
    protocol_versions: Vec<u32>,
}

impl ProcessPlugin {
    /// Validate static authority. Disk identity is checked again for every launch.
    ///
    /// # Errors
    ///
    /// Returns [`PluginErrorCode::Authority`] when manifest or policy authority is invalid.
    pub fn new(manifest: PluginManifest, policy: RuntimePolicy) -> Result<Self, PluginError> {
        validate_policy(&policy)?;
        let plugin = validate_manifest(manifest, policy.max_file_bytes)?;
        Ok(Self { plugin, policy, runtime_staging: RuntimeStaging::CopyBeforeLaunch })
    }

    /// Bind a plugin-manager-verified private runtime snapshot.
    ///
    /// The caller must retain exclusive ownership of the immutable snapshot for
    /// this value's full lifetime. The executable is still revalidated before
    /// every launch and every request receives a fresh writable working
    /// directory and sandbox. This path avoids copying large, already-private
    /// model runtimes a second time immediately before execution.
    ///
    /// # Errors
    ///
    /// Returns [`PluginErrorCode::Authority`] when manifest or policy authority is invalid.
    pub fn from_manager_verified_private_snapshot(
        manifest: PluginManifest,
        policy: RuntimePolicy,
    ) -> Result<Self, PluginError> {
        validate_policy(&policy)?;
        let plugin = validate_manifest(manifest, policy.max_file_bytes)?;
        Ok(Self { plugin, policy, runtime_staging: RuntimeStaging::ManagerVerifiedPrivateSnapshot })
    }

    /// Bind a complete authenticated runtime that was materialized into an
    /// owner-private, replacement-protected, read-only application cache.
    ///
    /// The caller must verify every file against application-embedded
    /// authority immediately before constructing this value and must retain
    /// the runtime root for this value's lifetime. The executable is rehashed
    /// on every request and each child still receives a private working
    /// directory and the normal process sandbox. This keeps large embedded
    /// model runtimes out of the per-request copy path.
    ///
    /// # Errors
    ///
    /// Returns [`PluginErrorCode::Authority`] when manifest or policy authority is invalid.
    pub fn from_authenticated_read_only_runtime(
        manifest: PluginManifest,
        policy: RuntimePolicy,
    ) -> Result<Self, PluginError> {
        Self::from_manager_verified_private_snapshot(manifest, policy)
    }

    /// Execute one plugin request, forwarding progress and respecting cancellation/deadlines.
    ///
    /// # Errors
    ///
    /// Returns a stable [`PluginError`] for authority, sandbox, protocol, lifecycle, resource,
    /// cancellation, timeout, plugin, or result-validation failures.
    #[allow(clippy::too_many_lines)]
    pub fn execute(
        &self,
        request: PluginRequest<'_>,
        context: &ExecutionContext,
    ) -> Result<PluginExecution, PluginError> {
        let raw = self.execute_raw(request, context)?;
        let dto = ResultDto::from_json(&raw.result_json).map_err(|_| {
            PluginError::new(PluginErrorCode::InvalidResult, "returned result DTO is invalid")
        })?;
        let mut result = ConversionResult::try_from(dto).map_err(|_| {
            PluginError::new(PluginErrorCode::InvalidResult, "returned result conversion failed")
        })?;
        result.diagnostics.splice(0..0, raw.diagnostics);
        Ok(PluginExecution {
            result,
            plugin_id: raw.plugin_id,
            executable_sha256: raw.executable_sha256,
        })
    }

    /// Execute one plugin request and return its authenticated raw JSON response.
    ///
    /// This shares the same sandbox, framing, event validation, cancellation,
    /// deadline, and process lifecycle as [`Self::execute`]. It exists so OCR
    /// and media adapters can use their own strict DTOs without pretending to
    /// return a complete document conversion.
    ///
    /// # Errors
    ///
    /// Returns a stable [`PluginError`] for authority, sandbox, protocol,
    /// lifecycle, resource, cancellation, timeout, or plugin failures.
    #[allow(clippy::too_many_lines)]
    pub fn execute_raw(
        &self,
        request: PluginRequest<'_>,
        context: &ExecutionContext,
    ) -> Result<RawPluginExecution, PluginError> {
        let (policy, _worker_memory) = self.memory_policy(request.memory_limit, context)?;
        validate_request(&request, &policy)?;
        let encoded_bound = u64::from(policy.max_frame_bytes)
            .checked_mul(2)
            .and_then(|value| value.checked_add(policy.max_output_bytes))
            .and_then(|value| value.checked_add(HASH_BUFFER_BYTES))
            .ok_or_else(|| {
                PluginError::new(PluginErrorCode::ResourceLimit, "frame reservation overflow")
            })?;
        let _wire_memory =
            context.reserve_memory(encoded_bound).map_err(|error| map_execution_error(&error))?;
        verify_executable(&self.plugin, policy.max_file_bytes)?;
        let directory_guard = sandbox::working_directory(&policy)?;
        let staged = match self.runtime_staging {
            RuntimeStaging::CopyBeforeLaunch => {
                stage_plugin(&self.plugin, &policy, directory_guard, context)?
            }
            RuntimeStaging::ManagerVerifiedPrivateSnapshot => {
                stage_private_snapshot(&self.plugin, directory_guard, context)?
            }
        };
        verify_executable(&staged.plugin, policy.max_file_bytes)?;
        let mut child = sandbox::spawn(&staged.plugin, &policy, &staged.working_directory)?;
        let mut stdin = child
            .take_stdin()
            .ok_or_else(|| PluginError::new(PluginErrorCode::Launch, "plugin stdin missing"))?;
        let mut stdout = child
            .take_stdout()
            .ok_or_else(|| PluginError::new(PluginErrorCode::Launch, "plugin stdout missing"))?;
        let stderr = child
            .take_stderr()
            .ok_or_else(|| PluginError::new(PluginErrorCode::Launch, "plugin stderr missing"))?;
        let maximum = policy.max_frame_bytes;
        // One queued frame plus the reader's current frame is covered by the two-frame execution
        // context reservation above. Backpressure remains cancellable during tree teardown.
        let (frames_tx, frames_rx) = mpsc::sync_channel(1);
        let reader_stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&reader_stop);
        let reader_handle = std::thread::Builder::new()
            .name("process-plugin-protocol".into())
            .spawn(move || {
                loop {
                    let mut frame = protocol::read_frame::<PluginMessage>(&mut stdout, maximum);
                    let terminal = frame.is_err();
                    loop {
                        if thread_stop.load(Ordering::Acquire) {
                            return;
                        }
                        match frames_tx.try_send(frame) {
                            Ok(()) => break,
                            Err(mpsc::TrySendError::Full(returned)) => {
                                frame = returned;
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => return,
                        }
                    }
                    if terminal {
                        break;
                    }
                }
            })
            .map_err(|_| {
                PluginError::new(PluginErrorCode::Launch, "protocol reader unavailable")
            })?;
        let reader = ProtocolReader { stop: reader_stop, handle: reader_handle };
        let stderr_reader = std::thread::Builder::new()
            .name("process-plugin-stderr".into())
            .spawn(move || drain_stderr(stderr))
            .map_err(|_| PluginError::new(PluginErrorCode::Launch, "stderr reader unavailable"))?;
        let nonce = request_nonce(request.request_id);
        if let Err(error) = protocol::write_frame(
            &mut stdin,
            &HostMessage::Hello {
                supported_versions: self.plugin.protocol_versions.clone(),
                plugin_id: self.plugin.plugin_id.clone(),
                nonce: nonce.clone(),
            },
            maximum,
        ) {
            return finish_error(child, reader, stderr_reader, map_write_error(&error));
        }
        let handshake_deadline = Instant::now().checked_add(policy.handshake_timeout);
        let hello = match receive_until(
            &frames_rx,
            &mut child,
            context,
            handshake_deadline,
            false,
            policy.max_memory_bytes,
        ) {
            Ok(hello) => hello,
            Err(error) => return finish_error(child, reader, stderr_reader, error),
        };
        match hello {
            PluginMessage::Hello { selected_version, plugin_id, nonce: echoed }
                if selected_version == PROTOCOL_V1
                    && self.plugin.protocol_versions.contains(&selected_version)
                    && plugin_id == self.plugin.plugin_id
                    && echoed == nonce => {}
            _ => {
                return finish_error(
                    child,
                    reader,
                    stderr_reader,
                    PluginError::new(PluginErrorCode::Protocol, "invalid handshake response"),
                );
            }
        }
        let inline_source = base64::engine::general_purpose::STANDARD.encode(request.source);
        let inline_request_bytes = inline_source
            .len()
            .saturating_add(request.parameters_json.map_or(0, str::len).saturating_add(4096));
        let (source_base64, source_path, _source_temporary) = if inline_request_bytes
            <= policy.max_frame_bytes as usize
        {
            (Some(inline_source), None, None)
        } else {
            let source_bytes = u64::try_from(request.source.len()).map_err(|_| {
                PluginError::new(PluginErrorCode::ResourceLimit, "source byte count overflow")
            })?;
            if source_bytes > policy.max_file_bytes {
                return finish_error(
                    child,
                    reader,
                    stderr_reader,
                    PluginError::new(PluginErrorCode::ResourceLimit, "source exceeds file limit"),
                );
            }
            let reservation = context
                .reserve_temporary(source_bytes)
                .map_err(|error| map_execution_error(&error));
            let reservation = match reservation {
                Ok(value) => value,
                Err(error) => return finish_error(child, reader, stderr_reader, error),
            };
            let staged_source = staged.working_directory.join("source.bin");
            let write_result = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged_source)
                .and_then(|mut file| {
                    std::io::Write::write_all(&mut file, request.source)?;
                    file.sync_all()
                });
            if write_result.is_err() || make_staged_file_private(&staged_source, false).is_err() {
                return finish_error(
                    child,
                    reader,
                    stderr_reader,
                    PluginError::new(PluginErrorCode::Launch, "request source staging failed"),
                );
            }
            if let Err(error) = sandbox::authorize_request_source(&policy, &staged_source) {
                return finish_error(child, reader, stderr_reader, error);
            }
            (None, Some("source.bin".to_owned()), Some(reservation))
        };
        let request_deadline = Instant::now().checked_add(policy.request_timeout);
        stdin = match write_request_bounded(
            stdin,
            HostMessage::Request {
                protocol_version: PROTOCOL_V1,
                request_id: request.request_id.to_owned(),
                input_format: request.input_format.to_owned(),
                source_name: request.source_name.map(str::to_owned),
                parameters_json: request.parameters_json.map(str::to_owned),
                source_base64,
                source_path,
                maximum_output_bytes: policy.max_output_bytes,
            },
            maximum,
            &mut child,
            context,
            request_deadline,
            policy.max_memory_bytes,
        ) {
            Ok(stdin) => stdin,
            Err(error) => return finish_error(child, reader, stderr_reader, error),
        };
        let mut last_sequence = 0_u64;
        let mut event_count = 0_u32;
        let mut streamed_diagnostics = Vec::new();
        let mut streamed_diagnostic_bytes = 0_u64;
        macro_rules! finish_on_error {
            ($expression:expr) => {
                match $expression {
                    Ok(value) => value,
                    Err(error) => return finish_error(child, reader, stderr_reader, error),
                }
            };
        }
        loop {
            let frame = match receive_until(
                &frames_rx,
                &mut child,
                context,
                request_deadline,
                true,
                policy.max_memory_bytes,
            ) {
                Ok(frame) => frame,
                Err(error)
                    if matches!(
                        error.code,
                        PluginErrorCode::Cancelled | PluginErrorCode::Timeout
                    ) =>
                {
                    let _ = protocol::write_frame(
                        &mut stdin,
                        &HostMessage::Cancel {
                            protocol_version: PROTOCOL_V1,
                            request_id: request.request_id.to_owned(),
                        },
                        maximum,
                    );
                    let grace = Instant::now().checked_add(policy.cancellation_grace);
                    while grace.is_some_and(|deadline| Instant::now() < deadline) {
                        if child.try_wait().ok().flatten().is_some() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    terminate_tree(&mut child);
                    let _ = reader.join();
                    let _ = stderr_reader.join();
                    return Err(error);
                }
                Err(error) => return finish_error(child, reader, stderr_reader, error),
            };
            // Cancellation can race the final millisecond of recv_timeout. Re-check after a frame
            // becomes readable so a cooperative terminal/error cannot overwrite an already-set
            // caller cancellation state.
            if let Err(error) = context.checkpoint().map_err(|error| map_execution_error(&error)) {
                let _ = protocol::write_frame(
                    &mut stdin,
                    &HostMessage::Cancel {
                        protocol_version: PROTOCOL_V1,
                        request_id: request.request_id.to_owned(),
                    },
                    maximum,
                );
                let grace = Instant::now().checked_add(policy.cancellation_grace);
                while grace.is_some_and(|deadline| Instant::now() < deadline) {
                    if child.try_wait().ok().flatten().is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                terminate_tree(&mut child);
                let _ = reader.join();
                let _ = stderr_reader.join();
                return Err(error);
            }
            match frame {
                PluginMessage::Progress {
                    protocol_version,
                    request_id,
                    sequence,
                    stage,
                    completed_units,
                    total_units,
                    message,
                } => {
                    event_count = event_count.saturating_add(1);
                    if event_count > 10_000
                        || message.as_ref().is_some_and(|value| value.len() > 4096)
                    {
                        return finish_error(
                            child,
                            reader,
                            stderr_reader,
                            PluginError::new(
                                PluginErrorCode::ResourceLimit,
                                "plugin event limit exceeded",
                            ),
                        );
                    }
                    finish_on_error!(validate_event(
                        protocol_version,
                        &request_id,
                        request.request_id,
                        sequence,
                        &mut last_sequence,
                    ));
                    let stage = finish_on_error!(parse_stage(&stage));
                    finish_on_error!(
                        context
                            .report(stage, completed_units, total_units, message)
                            .map_err(|error| map_execution_error(&error))
                    );
                }
                PluginMessage::Diagnostic {
                    protocol_version,
                    request_id,
                    sequence,
                    diagnostic_json,
                } => {
                    event_count = event_count.saturating_add(1);
                    streamed_diagnostic_bytes =
                        streamed_diagnostic_bytes.saturating_add(diagnostic_json.len() as u64);
                    if event_count > 10_000 || streamed_diagnostic_bytes > policy.max_output_bytes {
                        return finish_error(
                            child,
                            reader,
                            stderr_reader,
                            PluginError::new(
                                PluginErrorCode::ResourceLimit,
                                "plugin event limit exceeded",
                            ),
                        );
                    }
                    finish_on_error!(validate_event(
                        protocol_version,
                        &request_id,
                        request.request_id,
                        sequence,
                        &mut last_sequence,
                    ));
                    let envelope = finish_on_error!(
                        DiagnosticsDto::from_json(&diagnostic_json).map_err(|_| {
                            PluginError::new(PluginErrorCode::Protocol, "invalid diagnostic event")
                        })
                    );
                    if envelope.diagnostics.len() != 1 {
                        return finish_error(
                            child,
                            reader,
                            stderr_reader,
                            PluginError::new(
                                PluginErrorCode::Protocol,
                                "diagnostic event must contain exactly one record",
                            ),
                        );
                    }
                    let diagnostic = finish_on_error!(
                        envelope.diagnostics.into_iter().next().ok_or_else(|| PluginError::new(
                            PluginErrorCode::Protocol,
                            "diagnostic event is empty",
                        ))
                    );
                    streamed_diagnostics.push(finish_on_error!(
                        Diagnostic::try_from(diagnostic).map_err(|_| {
                            PluginError::new(PluginErrorCode::Protocol, "invalid diagnostic event")
                        })
                    ));
                }
                PluginMessage::Response { protocol_version, request_id, result_json } => {
                    if protocol_version != PROTOCOL_V1 || request_id != request.request_id {
                        return finish_error(
                            child,
                            reader,
                            stderr_reader,
                            PluginError::new(
                                PluginErrorCode::Protocol,
                                "terminal response identity mismatch",
                            ),
                        );
                    }
                    if result_json.len() as u64 > policy.max_output_bytes {
                        return finish_error(
                            child,
                            reader,
                            stderr_reader,
                            PluginError::new(
                                PluginErrorCode::FrameTooLarge,
                                "nested result exceeds output limit",
                            ),
                        );
                    }
                    if (result_json.len() as u64)
                        .checked_add(streamed_diagnostic_bytes)
                        .is_none_or(|bytes| bytes > policy.max_output_bytes)
                    {
                        return finish_error(
                            child,
                            reader,
                            stderr_reader,
                            PluginError::new(
                                PluginErrorCode::FrameTooLarge,
                                "combined result and diagnostics exceed output limit",
                            ),
                        );
                    }
                    drop(stdin);
                    let status = wait_bounded(&mut child, Duration::from_millis(250));
                    if status.is_none_or(|exit| !exit.success()) {
                        terminate_tree(&mut child);
                        let _ = reader.join();
                        let _ = stderr_reader.join();
                        return Err(PluginError::new(
                            PluginErrorCode::Crashed,
                            "plugin did not exit successfully after response",
                        ));
                    }
                    if reader.join_after_drain(&frames_rx) {
                        let _ = stderr_reader.join();
                        return Err(PluginError::new(
                            PluginErrorCode::Protocol,
                            "frame followed terminal response",
                        ));
                    }
                    let _ = stderr_reader.join();
                    return Ok(RawPluginExecution {
                        result_json,
                        diagnostics: streamed_diagnostics,
                        plugin_id: self.plugin.plugin_id.clone(),
                        executable_sha256: self.plugin.executable_sha256.clone(),
                    });
                }
                PluginMessage::Error { protocol_version, request_id, code, message } => {
                    if protocol_version != PROTOCOL_V1
                        || request_id.as_deref() != Some(request.request_id)
                        || !valid_token(&code, 64)
                        || message.len() > 4096
                    {
                        return finish_error(
                            child,
                            reader,
                            stderr_reader,
                            PluginError::new(
                                PluginErrorCode::Protocol,
                                "invalid plugin error frame",
                            ),
                        );
                    }
                    return finish_error(
                        child,
                        reader,
                        stderr_reader,
                        PluginError::new(
                            terminal_error_code(&code),
                            format!(
                                "plugin returned {code}: {}",
                                message
                                    .chars()
                                    .map(|character| {
                                        if character.is_control() { ' ' } else { character }
                                    })
                                    .collect::<String>()
                            ),
                        ),
                    );
                }
                PluginMessage::Hello { .. } => {
                    return finish_error(
                        child,
                        reader,
                        stderr_reader,
                        PluginError::new(PluginErrorCode::Protocol, "duplicate handshake"),
                    );
                }
            }
        }
    }
}

fn write_request_bounded(
    mut stdin: Box<dyn std::io::Write + Send>,
    request: HostMessage,
    maximum: u32,
    child: &mut sandbox::SandboxChild,
    context: &ExecutionContext,
    deadline: Option<Instant>,
    max_memory_bytes: u64,
) -> Result<Box<dyn std::io::Write + Send>, PluginError> {
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let writer = std::thread::Builder::new()
        .name("process-plugin-request".into())
        .spawn(move || {
            let result = protocol::write_frame(&mut stdin, &request, maximum);
            let _ = finished_tx.send((stdin, result));
        })
        .map_err(|_| PluginError::new(PluginErrorCode::Launch, "request writer unavailable"))?;
    loop {
        match finished_rx.recv_timeout(Duration::from_millis(5)) {
            Ok((stdin, result)) => {
                let _ = writer.join();
                return result.map(|()| stdin).map_err(|error| map_write_error(&error));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = writer.join();
                return Err(PluginError::new(
                    PluginErrorCode::Protocol,
                    "request writer ended unexpectedly",
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let context_error = context.checkpoint().err().map(|error| map_execution_error(&error));
        let policy_timeout = deadline.is_some_and(|value| Instant::now() >= value);
        let memory_exceeded = child.memory_exceeded(max_memory_bytes).unwrap_or(true);
        let child_ended = child.try_wait().ok().flatten().is_some();
        if context_error.is_some() || policy_timeout || memory_exceeded || child_ended {
            child.terminate();
            let _ = writer.join();
            return Err(context_error.unwrap_or_else(|| {
                if policy_timeout {
                    PluginError::new(PluginErrorCode::Timeout, "plugin request write timed out")
                } else if memory_exceeded {
                    PluginError::new(PluginErrorCode::ResourceLimit, "plugin memory limit exceeded")
                } else {
                    PluginError::new(
                        PluginErrorCode::Crashed,
                        "plugin exited while receiving request",
                    )
                }
            }));
        }
    }
}

fn receive_until(
    frames: &mpsc::Receiver<std::io::Result<PluginMessage>>,
    child: &mut sandbox::SandboxChild,
    context: &ExecutionContext,
    deadline: Option<Instant>,
    request_active: bool,
    max_memory_bytes: u64,
) -> Result<PluginMessage, PluginError> {
    loop {
        if request_active {
            context.checkpoint().map_err(|error| map_execution_error(&error))?;
        }
        if deadline.is_some_and(|value| Instant::now() >= value) {
            return Err(PluginError::new(
                PluginErrorCode::Timeout,
                if request_active {
                    "plugin request timed out"
                } else {
                    "plugin handshake timed out"
                },
            ));
        }
        match frames.recv_timeout(Duration::from_millis(5)) {
            Ok(frame) => return classify_received_frame(frame),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(PluginError::new(
                    PluginErrorCode::Crashed,
                    "plugin protocol stream ended",
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child.memory_exceeded(max_memory_bytes).unwrap_or(true) {
                    return Err(PluginError::new(
                        PluginErrorCode::ResourceLimit,
                        "plugin memory limit exceeded",
                    ));
                }
                if child
                    .try_wait()
                    .map_err(|()| PluginError::new(PluginErrorCode::Crashed, "plugin wait failed"))?
                    .is_some()
                {
                    // Process exit and the protocol-reader thread are observed
                    // independently. The provider may have already written a
                    // complete buffered frame before exiting, while the reader
                    // has not enqueued it yet. Give the closed pipe a bounded
                    // drain window so protocol evidence wins that race. The
                    // bound also prevents a descendant that inherited stdout
                    // from holding this path open indefinitely.
                    if let Ok(frame) = frames.recv_timeout(Duration::from_millis(250)) {
                        return classify_received_frame(frame);
                    }
                    return Err(PluginError::new(
                        PluginErrorCode::Crashed,
                        "plugin exited before terminal response",
                    ));
                }
            }
        }
    }
}

fn classify_received_frame(
    frame: std::io::Result<PluginMessage>,
) -> Result<PluginMessage, PluginError> {
    frame.map_err(|error| {
        let detail = error.to_string();
        // A completed frame read is stronger evidence than a child-exit or
        // resource-sampling race. Preserve the protocol/frame classification
        // even when the provider exits immediately after emitting invalid data.
        let code = if detail.contains("stream ended before frame") {
            PluginErrorCode::Crashed
        } else if detail.contains("exceeds limit") {
            PluginErrorCode::FrameTooLarge
        } else {
            PluginErrorCode::Protocol
        };
        let stable_detail = if detail.contains("stream ended before frame") {
            "plugin protocol stream ended"
        } else if detail.contains("truncated frame") {
            "plugin protocol frame was truncated"
        } else if detail.contains("exceeds limit") {
            "plugin protocol frame exceeds its limit"
        } else {
            "plugin emitted invalid protocol data"
        };
        PluginError::new(code, stable_detail)
    })
}

fn finish_error<T>(
    mut child: sandbox::SandboxChild,
    reader: ProtocolReader,
    stderr: std::thread::JoinHandle<()>,
    error: PluginError,
) -> Result<T, PluginError> {
    terminate_tree(&mut child);
    let _ = reader.join();
    let _ = stderr.join();
    Err(error)
}

struct ProtocolReader {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl ProtocolReader {
    fn join(self) -> std::thread::Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle.join()
    }

    fn join_after_drain(self, frames: &mpsc::Receiver<std::io::Result<PluginMessage>>) -> bool {
        let mut extra = false;
        for frame in frames {
            extra |= frame.is_ok();
        }
        let _ = self.handle.join();
        extra
    }
}

fn wait_bounded(
    child: &mut sandbox::SandboxChild,
    duration: Duration,
) -> Option<sandbox::ChildExit> {
    let deadline = Instant::now().checked_add(duration)?;
    loop {
        if let Ok(Some(exit)) = child.try_wait() {
            return Some(exit);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn terminate_tree(child: &mut sandbox::SandboxChild) {
    child.terminate();
}

fn drain_stderr(mut stderr: impl std::io::Read) {
    let mut buffer = [0_u8; 4096];
    let mut total = 0_usize;
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                total = total.saturating_add(read).min(16 * 1024);
            }
        }
    }
    let _ = total;
}

fn validate_policy(policy: &RuntimePolicy) -> Result<(), PluginError> {
    if !(1024..=protocol::ABSOLUTE_MAX_FRAME_BYTES).contains(&policy.max_frame_bytes)
        || policy.max_output_bytes == 0
        || policy.max_output_bytes > u64::from(policy.max_frame_bytes)
        || policy.max_memory_bytes < 32 * 1024 * 1024
        || policy.max_address_space_bytes.is_some_and(|bytes| {
            bytes < policy.max_memory_bytes || bytes > ABSOLUTE_ADDRESS_SPACE_BYTES
        })
        || policy.max_file_bytes == 0
        || !(16..=4096).contains(&policy.max_open_files)
        || policy.handshake_timeout.is_zero()
        || policy.request_timeout.is_zero()
        || policy.request_timeout > Duration::from_hours(2)
        || policy.cancellation_grace > Duration::from_secs(5)
        || policy.macos_compatibility_child && !policy.allow_child_processes
        || policy.environment.len() > 64
        || policy.read_only_roots.len() > 16
    {
        return Err(PluginError::new(PluginErrorCode::Authority, "invalid runtime policy"));
    }
    for (name, value) in &policy.environment {
        let name = name.to_str().ok_or_else(|| {
            PluginError::new(PluginErrorCode::Authority, "environment name is not UTF-8")
        })?;
        let value = value.to_str().ok_or_else(|| {
            PluginError::new(PluginErrorCode::Authority, "environment value is not UTF-8")
        })?;
        if !valid_environment_name(name)
            || reserved_environment_name(name)
            || value.len() > 4096
            || value.contains('\0')
        {
            return Err(PluginError::new(
                PluginErrorCode::Authority,
                "invalid declared environment",
            ));
        }
    }
    for root in &policy.read_only_roots {
        let canonical = canonical_directory(root)?;
        if &canonical != root {
            return Err(PluginError::new(
                PluginErrorCode::Authority,
                "read-only root is not canonical",
            ));
        }
    }
    Ok(())
}

fn validate_manifest(
    manifest: PluginManifest,
    maximum_file_bytes: u64,
) -> Result<ValidatedPlugin, PluginError> {
    if !valid_token(&manifest.plugin_id, 128)
        || manifest.protocol_versions.is_empty()
        || manifest.protocol_versions.len() > 8
        || !manifest.protocol_versions.contains(&PROTOCOL_V1)
        || manifest.protocol_versions.iter().copied().collect::<BTreeSet<_>>().len()
            != manifest.protocol_versions.len()
        || !valid_sha256(&manifest.executable_sha256)
    {
        return Err(PluginError::new(PluginErrorCode::Authority, "invalid plugin manifest"));
    }
    let executable = canonical_regular_file(&manifest.executable)?;
    let runtime_root = canonical_directory(&manifest.runtime_root)?;
    if !executable.starts_with(&runtime_root) {
        return Err(PluginError::new(
            PluginErrorCode::Authority,
            "executable escapes runtime root",
        ));
    }
    let plugin = ValidatedPlugin {
        plugin_id: manifest.plugin_id,
        executable,
        runtime_root,
        executable_sha256: manifest.executable_sha256,
        protocol_versions: manifest.protocol_versions,
    };
    verify_executable(&plugin, maximum_file_bytes)?;
    Ok(plugin)
}

fn verify_executable(plugin: &ValidatedPlugin, maximum_file_bytes: u64) -> Result<(), PluginError> {
    if canonical_regular_file(&plugin.executable)? != plugin.executable {
        return Err(PluginError::new(PluginErrorCode::Authority, "executable identity changed"));
    }
    let metadata = std::fs::metadata(&plugin.executable).map_err(|_| {
        PluginError::new(PluginErrorCode::Authority, "executable metadata unavailable")
    })?;
    let maximum = maximum_file_bytes.min(ABSOLUTE_EXECUTABLE_BYTES);
    if metadata.len() == 0 || metadata.len() > maximum {
        return Err(PluginError::new(
            PluginErrorCode::Authority,
            "executable exceeds authenticated file limit",
        ));
    }
    let mut file = std::fs::File::open(&plugin.executable)
        .map_err(|_| PluginError::new(PluginErrorCode::Authority, "executable cannot be read"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE].into_boxed_slice();
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| PluginError::new(PluginErrorCode::Authority, "executable read failed"))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > maximum {
            return Err(PluginError::new(
                PluginErrorCode::Authority,
                "executable exceeds authenticated file limit",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if total != metadata.len() {
        return Err(PluginError::new(PluginErrorCode::Authority, "executable size changed"));
    }
    let digest = format!("{:x}", hasher.finalize());
    if digest != plugin.executable_sha256 {
        return Err(PluginError::new(PluginErrorCode::Authority, "executable digest mismatch"));
    }
    Ok(())
}

fn stage_plugin(
    plugin: &ValidatedPlugin,
    policy: &RuntimePolicy,
    directory_guard: tempfile::TempDir,
    context: &ExecutionContext,
) -> Result<StagedPlugin, PluginError> {
    let private_root = directory_guard.path().canonicalize().map_err(|_| {
        PluginError::new(PluginErrorCode::Launch, "private working directory identity failed")
    })?;
    let runtime = private_root.join("runtime");
    let working = private_root.join("work");
    std::fs::create_dir(&runtime)
        .and_then(|()| std::fs::create_dir(&working))
        .map_err(|_| PluginError::new(PluginErrorCode::Launch, "private runtime unavailable"))?;
    let executable_relative =
        plugin.executable.strip_prefix(&plugin.runtime_root).map_err(|_| {
            PluginError::new(PluginErrorCode::Authority, "executable runtime identity changed")
        })?;
    let temporary =
        copy_runtime_tree(&plugin.runtime_root, &runtime, policy.max_file_bytes, context)?;
    let executable = runtime.join(executable_relative).canonicalize().map_err(|_| {
        PluginError::new(PluginErrorCode::Authority, "staged executable unavailable")
    })?;
    make_staged_file_private(&executable, true)?;
    let runtime_root = runtime
        .canonicalize()
        .map_err(|_| PluginError::new(PluginErrorCode::Authority, "staged runtime unavailable"))?;
    let working = working.canonicalize().map_err(|_| {
        PluginError::new(PluginErrorCode::Launch, "private work directory unavailable")
    })?;
    Ok(StagedPlugin {
        plugin: ValidatedPlugin {
            plugin_id: plugin.plugin_id.clone(),
            executable,
            runtime_root,
            executable_sha256: plugin.executable_sha256.clone(),
            protocol_versions: plugin.protocol_versions.clone(),
        },
        working_directory: working,
        _directory: directory_guard,
        _temporary: temporary,
    })
}

fn stage_private_snapshot(
    plugin: &ValidatedPlugin,
    directory_guard: tempfile::TempDir,
    context: &ExecutionContext,
) -> Result<StagedPlugin, PluginError> {
    context.checkpoint().map_err(|error| map_execution_error(&error))?;
    let private_root = directory_guard.path().canonicalize().map_err(|_| {
        PluginError::new(PluginErrorCode::Launch, "private working directory identity failed")
    })?;
    let working = private_root.join("work");
    std::fs::create_dir(&working)
        .map_err(|_| PluginError::new(PluginErrorCode::Launch, "private work directory failed"))?;
    let working = working.canonicalize().map_err(|_| {
        PluginError::new(PluginErrorCode::Launch, "private work directory unavailable")
    })?;
    Ok(StagedPlugin {
        plugin: plugin.clone(),
        working_directory: working,
        _directory: directory_guard,
        _temporary: context.reserve_temporary(0).map_err(|error| map_execution_error(&error))?,
    })
}

struct StagedPlugin {
    plugin: ValidatedPlugin,
    working_directory: PathBuf,
    // Fields drop in declaration order: remove files before releasing their accounting guard.
    _directory: tempfile::TempDir,
    _temporary: ResourceReservation,
}

#[allow(clippy::too_many_lines)]
fn copy_runtime_tree(
    source: &Path,
    destination: &Path,
    maximum_file: u64,
    context: &ExecutionContext,
) -> Result<ResourceReservation, PluginError> {
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    let mut entries = 0_usize;
    let mut total = 0_u64;
    let mut temporary =
        context.reserve_temporary(0).map_err(|error| map_execution_error(&error))?;
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE].into_boxed_slice();
    while let Some((source_directory, destination_directory)) = pending.pop() {
        let children = std::fs::read_dir(&source_directory)
            .map_err(|_| PluginError::new(PluginErrorCode::Authority, "runtime tree unreadable"))?;
        for child in children {
            let child = child.map_err(|_| {
                PluginError::new(PluginErrorCode::Authority, "runtime entry unreadable")
            })?;
            entries = entries.saturating_add(1);
            if entries > ABSOLUTE_RUNTIME_ENTRIES {
                return Err(PluginError::new(
                    PluginErrorCode::Authority,
                    "runtime tree entry limit exceeded",
                ));
            }
            let source_path = child.path();
            let destination_path = destination_directory.join(child.file_name());
            let metadata = std::fs::symlink_metadata(&source_path).map_err(|_| {
                PluginError::new(PluginErrorCode::Authority, "runtime entry metadata unavailable")
            })?;
            if metadata.file_type().is_symlink() {
                return Err(PluginError::new(
                    PluginErrorCode::Authority,
                    "runtime tree contains a symbolic link",
                ));
            }
            if metadata.is_dir() {
                std::fs::create_dir(&destination_path).map_err(|_| {
                    PluginError::new(PluginErrorCode::Launch, "staged runtime directory failed")
                })?;
                pending.push((source_path, destination_path));
            } else if metadata.is_file() {
                let maximum = maximum_file.min(ABSOLUTE_EXECUTABLE_BYTES);
                if metadata.len() > maximum {
                    return Err(PluginError::new(
                        PluginErrorCode::Authority,
                        "runtime file limit exceeded",
                    ));
                }
                let mut source_file = std::fs::File::open(&source_path).map_err(|_| {
                    PluginError::new(PluginErrorCode::Authority, "runtime file unreadable")
                })?;
                let mut destination_file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination_path)
                    .map_err(|_| {
                        PluginError::new(PluginErrorCode::Launch, "staged runtime create failed")
                    })?;
                let mut copied = 0_u64;
                loop {
                    context.checkpoint().map_err(|error| map_execution_error(&error))?;
                    let read = source_file.read(&mut buffer).map_err(|_| {
                        PluginError::new(PluginErrorCode::Authority, "runtime file read failed")
                    })?;
                    if read == 0 {
                        break;
                    }
                    copied = copied.saturating_add(read as u64);
                    if copied > metadata.len() || copied > maximum {
                        return Err(PluginError::new(
                            PluginErrorCode::Authority,
                            "runtime file changed while staging",
                        ));
                    }
                    temporary.grow(read as u64).map_err(|error| map_execution_error(&error))?;
                    std::io::Write::write_all(&mut destination_file, &buffer[..read]).map_err(
                        |_| {
                            PluginError::new(PluginErrorCode::Launch, "staged runtime write failed")
                        },
                    )?;
                }
                drop(destination_file);
                let copied = std::fs::metadata(&destination_path)
                    .map_err(|_| {
                        PluginError::new(PluginErrorCode::Launch, "staged runtime metadata failed")
                    })?
                    .len();
                if copied != metadata.len() || copied > maximum {
                    return Err(PluginError::new(
                        PluginErrorCode::Authority,
                        "runtime file changed while staging",
                    ));
                }
                total = total.saturating_add(copied);
                if total > ABSOLUTE_RUNTIME_BYTES {
                    return Err(PluginError::new(
                        PluginErrorCode::Authority,
                        "runtime tree byte limit exceeded",
                    ));
                }
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt as _;
                    metadata.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                make_staged_file_private(&destination_path, executable)?;
            } else {
                return Err(PluginError::new(
                    PluginErrorCode::Authority,
                    "runtime tree contains a special file",
                ));
            }
        }
    }
    Ok(temporary)
}

#[allow(clippy::unnecessary_wraps)] // Windows needs no chmod; Unix reports permission failures.
fn make_staged_file_private(path: &Path, executable: bool) -> Result<(), PluginError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if executable { 0o500 } else { 0o400 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|_| {
            PluginError::new(PluginErrorCode::Launch, "staged runtime permissions failed")
        })?;
    }
    #[cfg(windows)]
    {
        let _ = (path, executable);
    }
    Ok(())
}

fn canonical_regular_file(path: &Path) -> Result<PathBuf, PluginError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        PluginError::new(PluginErrorCode::Authority, "executable metadata unavailable")
    })?;
    if !path.is_absolute() || metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PluginError::new(
            PluginErrorCode::Authority,
            "executable is not a private regular file",
        ));
    }
    path.canonicalize().map_err(|_| {
        PluginError::new(PluginErrorCode::Authority, "executable canonicalization failed")
    })
}

fn canonical_directory(path: &Path) -> Result<PathBuf, PluginError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        PluginError::new(PluginErrorCode::Authority, "runtime root metadata unavailable")
    })?;
    if !path.is_absolute() || metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PluginError::new(
            PluginErrorCode::Authority,
            "runtime root is not a canonical directory",
        ));
    }
    path.canonicalize().map_err(|_| {
        PluginError::new(PluginErrorCode::Authority, "runtime root canonicalization failed")
    })
}

fn validate_request(
    request: &PluginRequest<'_>,
    policy: &RuntimePolicy,
) -> Result<(), PluginError> {
    let source_bytes = u64::try_from(request.source.len()).map_err(|_| {
        PluginError::new(PluginErrorCode::ResourceLimit, "source byte count overflow")
    })?;
    if !valid_token(request.request_id, 128)
        || !valid_token(request.input_format, 64)
        || request.source.is_empty()
        || source_bytes > policy.max_file_bytes
        || request.source_name.is_some_and(|name| name.len() > 1024 || name.contains('\0'))
        || request
            .parameters_json
            .is_some_and(|value| value.len() > 64 * 1024 || value.contains('\0'))
    {
        return Err(PluginError::new(
            PluginErrorCode::ResourceLimit,
            "request exceeds process protocol limits",
        ));
    }
    Ok(())
}

fn validate_event(
    version: u32,
    id: &str,
    expected: &str,
    sequence: u64,
    last: &mut u64,
) -> Result<(), PluginError> {
    if version != PROTOCOL_V1 || id != expected || sequence == 0 || sequence <= *last {
        return Err(PluginError::new(
            PluginErrorCode::Protocol,
            "invalid event identity or ordering",
        ));
    }
    *last = sequence;
    Ok(())
}

fn parse_stage(stage: &str) -> Result<ExecutionStage, PluginError> {
    match stage {
        "resolving" => Ok(ExecutionStage::Resolving),
        "detecting" => Ok(ExecutionStage::Detecting),
        "probing" => Ok(ExecutionStage::Probing),
        "converting" => Ok(ExecutionStage::Converting),
        "ai" => Ok(ExecutionStage::Ai),
        "ocr" => Ok(ExecutionStage::Ocr),
        "rendering" => Ok(ExecutionStage::Rendering),
        "completed" => Ok(ExecutionStage::Completed),
        _ => Err(PluginError::new(PluginErrorCode::Protocol, "unknown progress stage")),
    }
}

fn map_execution_error(error: &into_markdown_core::ConversionError) -> PluginError {
    match error {
        into_markdown_core::ConversionError::Cancelled => {
            PluginError::new(PluginErrorCode::Cancelled, "plugin request cancelled")
        }
        into_markdown_core::ConversionError::Timeout => {
            PluginError::new(PluginErrorCode::Timeout, "plugin request timed out")
        }
        _ => PluginError::new(
            PluginErrorCode::ResourceLimit,
            "execution context rejected plugin event",
        ),
    }
}

fn map_write_error(error: &std::io::Error) -> PluginError {
    let code = if error.to_string().contains("exceeds limit") {
        PluginErrorCode::FrameTooLarge
    } else {
        PluginErrorCode::Protocol
    };
    PluginError::new(code, "host could not write protocol frame")
}

fn valid_token(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('=')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn reserved_environment_name(value: &str) -> bool {
    matches!(
        value,
        "INTO_MARKDOWN_PLUGIN_PROTOCOL"
            | "INTO_MARKDOWN_INHERITED_SANDBOX"
            | "INTO_MARKDOWN_PRIVATE_TEMP"
            | "SYSTEMROOT"
            | "WINDIR"
            | "SYSTEMDRIVE"
            | "USERPROFILE"
            | "LOCALAPPDATA"
            | "APPDATA"
            | "TEMP"
            | "TMP"
    )
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn request_nonce(request_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"into-markdown/process-v1/nonce\0");
    hasher.update(request_id.as_bytes());
    hasher.update(std::process::id().to_le_bytes());
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    hasher.update(time.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_explicit_terminal_code_classifies_a_recognition_refusal() {
        use super::{PluginErrorCode, terminal_error_code};
        assert_eq!(
            terminal_error_code("ocrRecognitionMemory"),
            PluginErrorCode::OcrRecognitionMemory
        );
        assert_eq!(terminal_error_code("resourceLimit"), PluginErrorCode::ResourceLimit);
        assert_eq!(terminal_error_code("recognition bound exceeded"), PluginErrorCode::Plugin);
        assert_eq!(terminal_error_code("timeout"), PluginErrorCode::Timeout);
        assert_eq!(terminal_error_code("cancelled"), PluginErrorCode::Cancelled);
    }
    use super::*;

    #[test]
    fn policy_rejects_host_owned_environment_names() {
        for name in [
            "INTO_MARKDOWN_PLUGIN_PROTOCOL",
            "INTO_MARKDOWN_INHERITED_SANDBOX",
            "INTO_MARKDOWN_PRIVATE_TEMP",
            "SYSTEMROOT",
            "WINDIR",
            "SYSTEMDRIVE",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
            "TEMP",
            "TMP",
        ] {
            let mut policy = RuntimePolicy::default();
            policy.environment.insert(name.into(), "attacker-controlled".into());
            assert_eq!(validate_policy(&policy).unwrap_err().code, PluginErrorCode::Authority);
        }
        let mut policy = RuntimePolicy::default();
        policy.environment.insert("PLUGIN_LOCALE".into(), "en-US".into());
        validate_policy(&policy).unwrap();
    }

    #[test]
    fn policy_accepts_the_declared_media_timeout_and_rejects_larger_values() {
        let mut policy =
            RuntimePolicy { request_timeout: Duration::from_hours(2), ..RuntimePolicy::default() };
        validate_policy(&policy).unwrap();
        policy.request_timeout += Duration::from_millis(1);
        assert_eq!(validate_policy(&policy).unwrap_err().code, PluginErrorCode::Authority);
    }

    #[test]
    fn policy_keeps_physical_and_sparse_address_space_limits_ordered_and_finite() {
        let physical = 512 * 1024 * 1024;
        let mut policy = RuntimePolicy {
            max_memory_bytes: physical,
            max_address_space_bytes: Some(2 * 1024 * 1024 * 1024 * 1024),
            ..RuntimePolicy::default()
        };
        validate_policy(&policy).unwrap();
        policy.max_address_space_bytes = Some(physical - 1);
        assert_eq!(validate_policy(&policy).unwrap_err().code, PluginErrorCode::Authority);
        policy.max_address_space_bytes = Some(ABSOLUTE_ADDRESS_SPACE_BYTES + 1);
        assert_eq!(validate_policy(&policy).unwrap_err().code, PluginErrorCode::Authority);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_staging_preserves_private_helper_execute_authority() {
        use std::os::unix::fs::PermissionsExt as _;

        let source = tempfile::tempdir().unwrap();
        let helper = source.path().join("helper");
        let data = source.path().join("data");
        std::fs::write(&helper, b"helper").unwrap();
        std::fs::write(&data, b"data").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600)).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("runtime");
        std::fs::create_dir(&destination).unwrap();
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        copy_runtime_tree(source.path(), &destination, 1024, &context).unwrap();
        assert_eq!(
            std::fs::metadata(destination.join("helper")).unwrap().permissions().mode() & 0o777,
            0o500
        );
        assert_eq!(
            std::fs::metadata(destination.join("data")).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }
}
