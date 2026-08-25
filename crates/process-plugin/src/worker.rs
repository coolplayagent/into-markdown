//! Small SDK for implementing a `process-v1` plugin executable.

use crate::protocol::{self, HostMessage, PROTOCOL_V1, PluginMessage};
use base64::Engine as _;
use into_markdown_core::{
    CancellationToken, ConversionResult, Diagnostic, DiagnosticsDto, DtoJsonStyle, ResultDto,
};
use std::io;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Decoded, bounded request delivered to a plugin implementation.
#[derive(Debug)]
pub struct WorkerRequest {
    /// Host request identifier.
    pub request_id: String,
    /// Input format wire name.
    pub input_format: String,
    /// Display-only source name.
    pub source_name: Option<String>,
    /// Optional bounded capability-specific JSON parameters.
    pub parameters_json: Option<String>,
    /// Inline source bytes.
    pub source: Vec<u8>,
    /// Host-staged, request-private source file for payloads that do not fit a frame.
    pub source_path: Option<PathBuf>,
    /// Maximum nested result JSON bytes accepted by the host.
    pub maximum_output_bytes: u64,
}

/// Controlled worker failure sent as a terminal protocol error.
#[derive(Debug, Clone)]
pub struct WorkerError {
    /// Stable bounded ASCII token.
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

impl WorkerError {
    /// Construct a bounded controlled error.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

/// Ordered event writer shared with plugin conversion code.
#[derive(Clone)]
pub struct WorkerEvents {
    output: Arc<Mutex<WorkerOutput>>,
    request_id: String,
    maximum: u32,
}

struct WorkerOutput {
    writer: Box<dyn io::Write + Send>,
    next_sequence: u64,
    accepting_events: bool,
}

impl WorkerEvents {
    /// Emit one ordered progress event.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the host pipe closes or the event exceeds the frame limit.
    pub fn progress(
        &self,
        stage: &str,
        completed_units: Option<u64>,
        total_units: Option<u64>,
        message: Option<String>,
    ) -> io::Result<()> {
        self.write_event(|sequence| PluginMessage::Progress {
            protocol_version: PROTOCOL_V1,
            request_id: self.request_id.clone(),
            sequence,
            stage: stage.to_owned(),
            completed_units,
            total_units,
            message,
        })
    }

    /// Emit one validated non-fatal diagnostic.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for an invalid diagnostic, closed pipe, or oversized frame.
    pub fn diagnostic(&self, diagnostic: Diagnostic) -> io::Result<()> {
        let envelope = DiagnosticsDto::try_from_diagnostics(&[diagnostic])
            .and_then(|value| value.to_json())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.write_event(|sequence| PluginMessage::Diagnostic {
            protocol_version: PROTOCOL_V1,
            request_id: self.request_id.clone(),
            sequence,
            diagnostic_json: envelope,
        })
    }

    fn write_event(&self, build: impl FnOnce(u64) -> PluginMessage) -> io::Result<()> {
        let mut output = self.output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !output.accepting_events {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "terminal already written"));
        }
        let sequence = output.next_sequence;
        output.next_sequence = output.next_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "event sequence exhausted")
        })?;
        let message = build(sequence);
        protocol::write_frame(&mut output.writer, &message, self.maximum)
    }
}

/// Run one handshake and one request on stdin/stdout.
///
/// The reader thread consumes cancellation concurrently while conversion emits progress, so a
/// full stdout pipe cannot prevent the host from delivering cancellation.
///
/// # Errors
///
/// Returns an I/O error for a malformed/unsupported host sequence or a failed protocol write.
pub fn serve(
    plugin_id: &str,
    maximum_frame_bytes: u32,
    handler: impl FnOnce(
        WorkerRequest,
        WorkerEvents,
        CancellationToken,
    ) -> Result<ConversionResult, WorkerError>,
) -> io::Result<()> {
    serve_raw(plugin_id, maximum_frame_bytes, conversion_handler(handler))
}

/// Run one conversion worker while reserving the inherited stdout pipe exclusively for protocol
/// frames and redirecting ambient/native stdout writes to the separately drained stderr pipe.
///
/// Use this for plugins that call native libraries which cannot guarantee stdout silence. The
/// protocol pipe is duplicated before descriptor 1 is redirected, so Rust/C dependencies cannot
/// corrupt frame boundaries. Stderr remains separately drained by the host.
///
/// # Errors
///
/// Returns an I/O error when stdout isolation or the worker protocol fails.
pub fn serve_with_isolated_stdout(
    plugin_id: &str,
    maximum_frame_bytes: u32,
    handler: impl FnOnce(
        WorkerRequest,
        WorkerEvents,
        CancellationToken,
    ) -> Result<ConversionResult, WorkerError>,
) -> io::Result<()> {
    serve_raw_with_isolated_stdout(plugin_id, maximum_frame_bytes, conversion_handler(handler))
}

fn conversion_handler(
    handler: impl FnOnce(
        WorkerRequest,
        WorkerEvents,
        CancellationToken,
    ) -> Result<ConversionResult, WorkerError>,
) -> impl FnOnce(WorkerRequest, WorkerEvents, CancellationToken) -> Result<String, WorkerError> {
    move |mut request, events, cancellation| {
        let maximum = request.maximum_output_bytes;
        if request.source.is_empty()
            && let Some(path) = request.source_path.take()
        {
            request.source = std::fs::read(path)
                .map_err(|_| WorkerError::new("invalidSource", "staged source cannot be read"))?;
        }
        let result = handler(request, events, cancellation)?;
        let result_json = ResultDto::json_from_result(&result, DtoJsonStyle::Compact)
            .map_err(|_| WorkerError::new("invalidResult", "result serialization failed"))?;
        if result_json.len() as u64 > maximum {
            Err(WorkerError::new("outputLimit", "result exceeds declared output limit"))
        } else {
            Ok(result_json)
        }
    }
}

/// Run one handshake and return an already serialized capability DTO.
///
/// The raw JSON remains bounded by the host-declared output limit. The host
/// revalidates it against the exact OCR or media DTO before use.
///
/// # Errors
///
/// Returns an I/O error for a malformed/unsupported host sequence or a failed
/// protocol write.
pub fn serve_raw(
    plugin_id: &str,
    maximum_frame_bytes: u32,
    handler: impl FnOnce(WorkerRequest, WorkerEvents, CancellationToken) -> Result<String, WorkerError>,
) -> io::Result<()> {
    serve_raw_with_writer(plugin_id, maximum_frame_bytes, Box::new(io::stdout()), handler)
}

/// Run one raw-JSON worker with an isolated protocol stdout pipe.
///
/// # Errors
///
/// Returns an I/O error when stdout isolation or the worker protocol fails.
pub fn serve_raw_with_isolated_stdout(
    plugin_id: &str,
    maximum_frame_bytes: u32,
    handler: impl FnOnce(WorkerRequest, WorkerEvents, CancellationToken) -> Result<String, WorkerError>,
) -> io::Result<()> {
    let writer = isolate_protocol_stdout()?;
    serve_raw_with_writer(plugin_id, maximum_frame_bytes, Box::new(writer), handler)
}

// Keeping the complete handshake, request validation, cancellation wiring, and terminal frame
// transition in one function makes the protocol state machine auditable as a single sequence.
#[allow(clippy::too_many_lines)]
fn serve_raw_with_writer(
    plugin_id: &str,
    maximum_frame_bytes: u32,
    writer: Box<dyn io::Write + Send>,
    handler: impl FnOnce(WorkerRequest, WorkerEvents, CancellationToken) -> Result<String, WorkerError>,
) -> io::Result<()> {
    let mut input = io::stdin();
    let output =
        Arc::new(Mutex::new(WorkerOutput { writer, next_sequence: 1, accepting_events: true }));
    let hello = protocol::read_frame::<HostMessage>(&mut input, maximum_frame_bytes)?;
    let HostMessage::Hello { supported_versions: versions, plugin_id: expected_id, nonce } = hello
    else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "hello must be first"));
    };
    if expected_id != plugin_id || !versions.contains(&PROTOCOL_V1) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "no compatible identity/version"));
    }
    {
        let mut writer = output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        protocol::write_frame(
            &mut writer.writer,
            &PluginMessage::Hello {
                selected_version: PROTOCOL_V1,
                plugin_id: plugin_id.to_owned(),
                nonce,
            },
            maximum_frame_bytes,
        )?;
    }
    let request = protocol::read_frame::<HostMessage>(&mut input, maximum_frame_bytes)?;
    let HostMessage::Request {
        protocol_version: PROTOCOL_V1,
        request_id,
        input_format,
        source_name,
        parameters_json,
        source_base64,
        source_path,
        maximum_output_bytes,
    } = request
    else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request must follow hello"));
    };
    let (source, source_path) = match (source_base64, source_path) {
        (Some(encoded), None) => {
            let source =
                base64::engine::general_purpose::STANDARD.decode(&encoded).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "source is not canonical base64")
                })?;
            if base64::engine::general_purpose::STANDARD.encode(&source) != encoded {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "source is not canonical base64",
                ));
            }
            (source, None)
        }
        (None, Some(path)) if path == "source.bin" => {
            let path = std::env::current_dir()?.join(path);
            let metadata = std::fs::symlink_metadata(&path)?;
            #[cfg(windows)]
            let reparse = {
                use std::os::windows::fs::MetadataExt as _;
                metadata.file_attributes() & 0x0000_0400 != 0
            };
            #[cfg(not(windows))]
            let reparse = metadata.file_type().is_symlink();
            // The wire contract accepts only this literal one-component name. The host created
            // it with create-new semantics in the sandbox-owned working directory, and the
            // AppContainer has read-only access, so no path canonicalization is needed here.
            if reparse || !metadata.is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "source path is invalid"));
            }
            (Vec::new(), Some(path))
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request must contain exactly one source transport",
            ));
        }
    };
    let cancellation = CancellationToken::new();
    let reader_cancellation = cancellation.clone();
    let expected_request_id = request_id.clone();
    std::thread::Builder::new().name("process-plugin-cancel".into()).spawn(move || {
        if let Ok(HostMessage::Cancel { protocol_version: PROTOCOL_V1, request_id }) =
            protocol::read_frame::<HostMessage>(&mut input, maximum_frame_bytes)
            && request_id == expected_request_id
        {
            reader_cancellation.cancel();
        }
    })?;
    let events = WorkerEvents {
        output: Arc::clone(&output),
        request_id: request_id.clone(),
        maximum: maximum_frame_bytes,
    };
    let request = WorkerRequest {
        request_id: request_id.clone(),
        input_format,
        source_name,
        parameters_json,
        source,
        source_path,
        maximum_output_bytes,
    };
    let terminal = match handler(request, events, cancellation) {
        Ok(result_json) => {
            if result_json.len() as u64 > maximum_output_bytes {
                PluginMessage::Error {
                    protocol_version: PROTOCOL_V1,
                    request_id: Some(request_id),
                    code: "outputLimit".into(),
                    message: "result exceeds declared output limit".into(),
                }
            } else {
                PluginMessage::Response { protocol_version: PROTOCOL_V1, request_id, result_json }
            }
        }
        Err(error) => PluginMessage::Error {
            protocol_version: PROTOCOL_V1,
            request_id: Some(request_id),
            code: error.code,
            message: error.message,
        },
    };
    let mut writer = output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    writer.accepting_events = false;
    protocol::write_frame(&mut writer.writer, &terminal, maximum_frame_bytes)
}

struct ProtocolWriter {
    descriptor: libc::c_int,
}

impl io::Write for ProtocolWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        let count = buffer.len();
        #[cfg(windows)]
        let count = u32::try_from(buffer.len().min(u32::MAX as usize)).unwrap_or(u32::MAX);
        // SAFETY: `descriptor` is an owned duplicate kept open for this writer, and `buffer`
        // remains valid for the duration of the synchronous OS write.
        let written = unsafe { libc::write(self.descriptor, buffer.as_ptr().cast(), count) };
        if written < 0 {
            Err(io::Error::last_os_error())
        } else {
            usize::try_from(written).map_err(|_| io::Error::other("protocol write overflow"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ProtocolWriter {
    fn drop(&mut self) {
        // SAFETY: this writer owns the duplicated descriptor and closes it exactly once.
        let _ = unsafe { libc::close(self.descriptor) };
    }
}

fn isolate_protocol_stdout() -> io::Result<ProtocolWriter> {
    const STDOUT_DESCRIPTOR: libc::c_int = 1;
    const STDERR_DESCRIPTOR: libc::c_int = 2;

    io::stdout().flush()?;
    // SAFETY: duplicating a valid inherited stdout descriptor does not alias Rust ownership;
    // `ProtocolWriter` assumes responsibility for closing the duplicate.
    let protocol = unsafe { libc::dup(STDOUT_DESCRIPTOR) };
    if protocol < 0 {
        return Err(io::Error::last_os_error());
    }
    if let Err(error) = make_protocol_descriptor_private(protocol) {
        // SAFETY: ownership has not yet been transferred to `ProtocolWriter`.
        let _ = unsafe { libc::close(protocol) };
        return Err(error);
    }
    // SAFETY: both inherited standard descriptors are open. `dup2` atomically makes ambient
    // stdout use the host's separately drained stderr pipe while the duplicated protocol
    // descriptor continues to reference the original stdout pipe.
    let redirected = unsafe { libc::dup2(STDERR_DESCRIPTOR, STDOUT_DESCRIPTOR) };
    if redirected < 0 {
        // SAFETY: ownership has not yet been transferred to `ProtocolWriter`.
        let _ = unsafe { libc::close(protocol) };
        return Err(io::Error::last_os_error());
    }
    #[cfg(windows)]
    if let Err(error) = set_binary_mode(protocol) {
        // SAFETY: ownership has not yet been transferred to `ProtocolWriter`.
        let _ = unsafe { libc::close(protocol) };
        return Err(error);
    }
    Ok(ProtocolWriter { descriptor: protocol })
}

#[cfg(unix)]
fn make_protocol_descriptor_private(descriptor: libc::c_int) -> io::Result<()> {
    // SAFETY: `descriptor` is open and F_SETFD only updates its close-on-exec flag.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn make_protocol_descriptor_private(descriptor: libc::c_int) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    // SAFETY: the CRT descriptor is valid and remains open while the derived handle is used.
    let handle = unsafe { libc::get_osfhandle(descriptor) };
    if handle == -1
        // SAFETY: the handle was obtained from the live descriptor; clearing inheritance does not
        // close or otherwise mutate its pipe identity.
        || unsafe { SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, 0) } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn set_binary_mode(descriptor: libc::c_int) -> io::Result<()> {
    unsafe extern "C" {
        #[link_name = "_setmode"]
        fn set_mode(descriptor: libc::c_int, mode: libc::c_int) -> libc::c_int;
    }
    // SAFETY: `descriptor` is a live CRT file descriptor and `_setmode` preserves ownership.
    if unsafe { set_mode(descriptor, libc::O_BINARY) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
