//! Small SDK for implementing a `process-v1` plugin executable.

use crate::protocol::{self, HostMessage, PROTOCOL_V1, PluginMessage};
use base64::Engine as _;
use into_markdown_core::{
    CancellationToken, ConversionResult, Diagnostic, DiagnosticsDto, DtoJsonStyle, ResultDto,
};
use std::io;
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
    /// Inline source bytes.
    pub source: Vec<u8>,
    /// Maximum nested result JSON bytes accepted by the host.
    pub maximum_output_bytes: u64,
}

/// Controlled worker failure sent as a terminal protocol error.
#[derive(Debug, Clone)]
pub struct WorkerError {
    /// Stable lowercase token.
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
    writer: io::Stdout,
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
    let mut input = io::stdin();
    let output = Arc::new(Mutex::new(WorkerOutput {
        writer: io::stdout(),
        next_sequence: 1,
        accepting_events: true,
    }));
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
        source_base64,
        maximum_output_bytes,
    } = request
    else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request must follow hello"));
    };
    let source =
        base64::engine::general_purpose::STANDARD.decode(&source_base64).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "source is not canonical base64")
        })?;
    if base64::engine::general_purpose::STANDARD.encode(&source) != source_base64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "source is not canonical base64"));
    }
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
        source,
        maximum_output_bytes,
    };
    let terminal = match handler(request, events, cancellation) {
        Ok(result) => {
            let result_json = ResultDto::json_from_result(&result, DtoJsonStyle::Compact)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
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
