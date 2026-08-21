//! Official offline OCR and media capability workers.

use futures::executor::block_on;
use into_markdown::{
    CancellationToken, ConversionError, DiarizationRequest, ExecutionContext, ExecutionOptions,
    InstalledAsrConfig, InstalledDiarizationConfig, InstalledOcrConfig, OcrRequest, ResourceLimits,
    TranscriptionRequest,
};
use into_markdown_process_plugin::worker::{
    WorkerError, WorkerEvents, WorkerRequest, serve_raw_with_isolated_stdout,
};
use into_markdown_provider_plugin::{
    DiarizationParameters, OcrCapabilityResponse, OcrParameters, ReadinessParameters,
    TranscriptionParameters,
};
use std::fs;
use std::path::{Path, PathBuf};

const OCR_PLUGIN_ID: &str = "official.ocr.ppocrv6";
const MEDIA_PLUGIN_ID: &str = "official.media.whisper";
const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Serve one OCR request on the authenticated process protocol.
pub fn serve_ocr() -> std::io::Result<()> {
    serve_raw_with_isolated_stdout(
        OCR_PLUGIN_ID,
        MAX_FRAME_BYTES,
        |request, events, cancellation| {
            if request.input_format == "readiness" {
                let parameters: ReadinessParameters = parameters(&request)?;
                if parameters.schema_version != 1 || parameters.capability_id != "ocr" {
                    return Err(WorkerError::new(
                        "invalidRequest",
                        "OCR readiness identity is invalid",
                    ));
                }
                let context = execution_context(cancellation, &parameters.options.limits);
                let root = distribution_root()?;
                ocr_service(
                    &root,
                    &parameters.model_roots,
                    parameters.model_bundle,
                    &parameters.options,
                    &context,
                )?;
                return Ok("{\"ready\":true}".into());
            }
            if request.input_format != "ocr" {
                return Err(WorkerError::new("unsupportedOperation", "expected OCR operation"));
            }
            let parameters: OcrParameters = parameters(&request)?;
            if parameters.schema_version != 1 || parameters.capability_id != "ocr" {
                return Err(WorkerError::new(
                    "invalidRequest",
                    "OCR capability identity is invalid",
                ));
            }
            let context = execution_context(cancellation, &parameters.options.limits);
            events.progress("ocr", Some(0), Some(1), Some("provider.ocr.start".into())).ok();
            let source = source_bytes(&request, &context)?;
            let root = distribution_root()?;
            let model_bundle =
                parameters.model_bundle.clone().unwrap_or_else(|| "pp-ocrv6-tiny-zh-en".into());
            let engine = ocr_service(
                &root,
                &parameters.model_roots,
                Some(model_bundle.clone()),
                &parameters.options,
                &context,
            )?;
            let languages = parameters.languages.iter().map(String::as_str).collect::<Vec<_>>();
            let recognition = block_on(engine.recognize_bound(
                OcrRequest {
                    image: &source.bytes,
                    media_type: &parameters.media_type,
                    languages: &languages,
                },
                &context,
            ))
            .map_err(worker_error)?;
            let into_markdown::OcrRecognition::Bound(result) = recognition else {
                return Err(WorkerError::new("invalidResult", "official OCR result is not bound"));
            };
            events.progress("ocr", Some(1), Some(1), Some("provider.ocr.complete".into())).ok();
            serde_json::to_string(&OcrCapabilityResponse {
                schema_version: 1,
                capability_id: "ocr".into(),
                provider_id: engine.id().into(),
                model_bundle: Some(model_bundle),
                result: result.to_dto(),
            })
            .map_err(|_| WorkerError::new("invalidResult", "OCR result serialization failed"))
        },
    )
}

/// Serve one speech-transcription or speaker-diarization request.
pub fn serve_media() -> std::io::Result<()> {
    serve_raw_with_isolated_stdout(
        MEDIA_PLUGIN_ID,
        MAX_FRAME_BYTES,
        |request, events, cancellation| match request.input_format.as_str() {
            "readiness" => media_readiness(request, cancellation),
            "transcription" => transcribe(request, events, cancellation),
            "diarization" => diarize(request, events, cancellation),
            _ => Err(WorkerError::new(
                "unsupportedOperation",
                "expected transcription or diarization operation",
            )),
        },
    )
}

fn media_readiness(
    request: WorkerRequest,
    cancellation: CancellationToken,
) -> Result<String, WorkerError> {
    let mut parameters: ReadinessParameters = parameters(&request)?;
    if parameters.schema_version != 1 {
        return Err(WorkerError::new("invalidRequest", "media readiness version is invalid"));
    }
    // Readiness verifies that the declared capability can be constructed even
    // when the caller's conversion defaults do not request that capability.
    if parameters.capability_id == "diarization" {
        parameters.options.diarization.enabled = true;
    }
    let context = execution_context(cancellation, &parameters.options.limits);
    let root = distribution_root()?;
    match parameters.capability_id.as_str() {
        "transcription" => {
            asr_service(
                &root,
                &parameters.model_roots,
                parameters.model_bundle,
                &parameters.options,
                &context,
            )?;
        }
        "diarization" => {
            diarization_service(
                &root,
                &parameters.model_roots,
                parameters.model_bundle,
                &parameters.options,
                &context,
            )?;
        }
        _ => {
            return Err(WorkerError::new(
                "invalidRequest",
                "media readiness capability is invalid",
            ));
        }
    }
    Ok("{\"ready\":true}".into())
}

fn transcribe(
    request: WorkerRequest,
    events: WorkerEvents,
    cancellation: CancellationToken,
) -> Result<String, WorkerError> {
    let parameters: TranscriptionParameters = parameters(&request)?;
    if parameters.schema_version != 1 || parameters.capability_id != "transcription" {
        return Err(WorkerError::new(
            "invalidRequest",
            "transcription capability identity is invalid",
        ));
    }
    let context = execution_context(cancellation, &parameters.options.limits);
    events.progress("ai", Some(0), Some(1), Some("provider.transcription.start".into())).ok();
    let source = source_bytes(&request, &context)?;
    let root = distribution_root()?;
    let transcriber = asr_service(
        &root,
        &parameters.model_roots,
        parameters.model_bundle,
        &parameters.options,
        &context,
    )?;
    let result = block_on(transcriber.transcribe(
        TranscriptionRequest {
            media: &source.bytes,
            media_type: &parameters.media_type,
            language: parameters.language.as_deref(),
        },
        &context,
    ))
    .map_err(worker_error)?;
    events.progress("ai", Some(1), Some(1), Some("provider.transcription.complete".into())).ok();
    serde_json::to_string(&result)
        .map_err(|_| WorkerError::new("invalidResult", "transcription result serialization failed"))
}

fn diarize(
    request: WorkerRequest,
    events: WorkerEvents,
    cancellation: CancellationToken,
) -> Result<String, WorkerError> {
    let parameters: DiarizationParameters = parameters(&request)?;
    if parameters.schema_version != 1 || parameters.capability_id != "diarization" {
        return Err(WorkerError::new(
            "invalidRequest",
            "diarization capability identity is invalid",
        ));
    }
    let context = execution_context(cancellation, &parameters.options.limits);
    events.progress("ai", Some(0), Some(1), Some("provider.diarization.start".into())).ok();
    let source = source_bytes(&request, &context)?;
    let root = distribution_root()?;
    let diarizer = diarization_service(
        &root,
        &parameters.model_roots,
        parameters.model_bundle,
        &parameters.options,
        &context,
    )?;
    let result = block_on(diarizer.diarize(
        DiarizationRequest {
            media: &source.bytes,
            media_type: &parameters.media_type,
            segments: &parameters.segments,
            expected_speakers: parameters.expected_speakers,
            max_speakers: parameters.max_speakers,
        },
        &context,
    ))
    .map_err(worker_error)?;
    events.progress("ai", Some(1), Some(1), Some("provider.diarization.complete".into())).ok();
    serde_json::to_string(&result)
        .map_err(|_| WorkerError::new("invalidResult", "diarization result serialization failed"))
}

fn ocr_service(
    root: &Path,
    roots: &[PathBuf],
    model_bundle: Option<String>,
    options: &into_markdown::ConversionOptions,
    context: &ExecutionContext,
) -> Result<std::sync::Arc<dyn into_markdown::OcrEngine>, WorkerError> {
    let (writable_model_root, bundled_model_root) = model_roots(root, roots);
    let runtime_root = root.join("onnxruntime");
    into_markdown::installed_ocr_service(
        &InstalledOcrConfig {
            writable_model_root,
            bundled_model_root,
            runtime_library: into_markdown::expected_ocr_runtime_library(&runtime_root)
                .map_err(worker_error)?,
            runtime_trusted_root: runtime_root,
            worker_executable: root.join("bin").join(worker_name()),
            model_bundle: model_bundle.unwrap_or_else(|| "pp-ocrv6-tiny-zh-en".into()),
        },
        options,
        context,
    )
    .map_err(worker_error)
}

fn asr_service(
    root: &Path,
    roots: &[PathBuf],
    model_bundle: Option<String>,
    options: &into_markdown::ConversionOptions,
    context: &ExecutionContext,
) -> Result<std::sync::Arc<dyn into_markdown::Transcriber>, WorkerError> {
    let (writable_model_root, bundled_model_root) = model_roots(root, roots);
    let ffmpeg_root = root.join("ffmpeg");
    into_markdown::installed_asr_service_in_read_only_sandbox(
        &InstalledAsrConfig {
            writable_model_root,
            bundled_model_root,
            ffmpeg_executable: ffmpeg_root.join(ffmpeg_name()),
            ffmpeg_authority: ffmpeg_root.join("authority.json"),
            ffmpeg_trusted_root: ffmpeg_root,
            model_bundle: model_bundle.unwrap_or_else(|| "whisper-small-multilingual".into()),
        },
        options,
        context,
    )
    .map_err(worker_error)
}

fn diarization_service(
    root: &Path,
    roots: &[PathBuf],
    model_bundle: Option<String>,
    options: &into_markdown::ConversionOptions,
    context: &ExecutionContext,
) -> Result<std::sync::Arc<dyn into_markdown::Diarizer>, WorkerError> {
    let (writable_model_root, bundled_model_root) = model_roots(root, roots);
    let runtime_root = root.join("onnxruntime");
    let ffmpeg_root = root.join("ffmpeg");
    into_markdown::installed_diarization_service_in_read_only_sandbox(
        &InstalledDiarizationConfig {
            writable_model_root,
            bundled_model_root,
            runtime_library: into_markdown::expected_ocr_runtime_library(&runtime_root)
                .map_err(worker_error)?,
            runtime_trusted_root: runtime_root,
            worker_executable: root.join("bin").join(worker_name()),
            ffmpeg_executable: ffmpeg_root.join(ffmpeg_name()),
            ffmpeg_authority: ffmpeg_root.join("authority.json"),
            ffmpeg_trusted_root: ffmpeg_root,
            model_bundle: model_bundle.unwrap_or_else(|| "silero-vad-3dspeaker-eres2net".into()),
        },
        options,
        context,
    )
    .map_err(worker_error)
}

fn parameters<T: serde::de::DeserializeOwned>(request: &WorkerRequest) -> Result<T, WorkerError> {
    let json = request
        .parameters_json
        .as_deref()
        .ok_or_else(|| WorkerError::new("invalidRequest", "capability parameters are missing"))?;
    serde_json::from_str(json)
        .map_err(|_| WorkerError::new("invalidRequest", "capability parameters are invalid"))
}

struct SourceBytes {
    bytes: Vec<u8>,
    _memory: Option<into_markdown::ResourceReservation>,
}

fn source_bytes(
    request: &WorkerRequest,
    context: &ExecutionContext,
) -> Result<SourceBytes, WorkerError> {
    if let Some(path) = &request.source_path {
        let size = fs::metadata(path)
            .map_err(|_| WorkerError::new("invalidRequest", "staged source is unavailable"))?
            .len();
        let memory = context.reserve_memory(size).map_err(worker_error)?;
        let bytes = fs::read(path)
            .map_err(|_| WorkerError::new("invalidRequest", "staged source cannot be read"))?;
        if bytes.len() as u64 != size {
            return Err(WorkerError::new("invalidRequest", "staged source changed while reading"));
        }
        Ok(SourceBytes { bytes, _memory: Some(memory) })
    } else if request.source.is_empty() {
        Err(WorkerError::new("invalidRequest", "source is empty"))
    } else {
        Ok(SourceBytes { bytes: request.source.clone(), _memory: None })
    }
}

fn execution_context(cancellation: CancellationToken, limits: &ResourceLimits) -> ExecutionContext {
    ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        limits.clone(),
    )
}

fn distribution_root() -> Result<PathBuf, WorkerError> {
    let executable =
        std::env::current_exe().and_then(|path| path.canonicalize()).map_err(|_| {
            WorkerError::new("componentUnavailable", "provider executable is unavailable")
        })?;
    let parent = executable.parent().ok_or_else(|| {
        WorkerError::new("componentUnavailable", "provider runtime root is unavailable")
    })?;
    Ok(if parent.file_name().is_some_and(|name| name == "bin") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    })
}

fn model_roots(root: &Path, configured: &[PathBuf]) -> (PathBuf, Option<PathBuf>) {
    let packaged = root.join("models");
    let writable = configured.first().cloned().unwrap_or_else(|| packaged.clone());
    let bundled = configured.get(1).cloned().or_else(|| packaged.is_dir().then_some(packaged));
    (writable, bundled)
}

fn worker_error(error: ConversionError) -> WorkerError {
    WorkerError::new(error.code().as_str(), error.to_string())
}

const fn worker_name() -> &'static str {
    if cfg!(windows) { "onnxruntime-worker.exe" } else { "onnxruntime-worker" }
}

const fn ffmpeg_name() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown::OcrPolicy;

    #[test]
    #[ignore = "requires INTO_MD_PROVIDER_RUNTIME, INTO_MD_PROVIDER_MODELS, and INTO_MD_OCR_FIXTURE"]
    fn explicit_installed_ocr_runtime_smoke() {
        let runtime = PathBuf::from(std::env::var_os("INTO_MD_PROVIDER_RUNTIME").unwrap());
        let models = PathBuf::from(std::env::var_os("INTO_MD_PROVIDER_MODELS").unwrap());
        let fixture = fs::read(std::env::var_os("INTO_MD_OCR_FIXTURE").unwrap()).unwrap();
        let limits = ResourceLimits {
            max_memory_bytes: 8 * 1024 * 1024 * 1024,
            max_temporary_bytes: 4 * 1024 * 1024 * 1024,
            ..ResourceLimits::default()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        let mut options = into_markdown::ConversionOptions { limits, ..Default::default() };
        options.ocr.policy = OcrPolicy::Always;
        let engine = ocr_service(
            &runtime,
            &[models],
            Some("pp-ocrv6-tiny-zh-en".into()),
            &options,
            &context,
        )
        .unwrap();
        let result = block_on(engine.recognize_bound(
            OcrRequest { image: &fixture, media_type: "image/png", languages: &[] },
            &context,
        ))
        .unwrap();
        assert!(matches!(result, into_markdown::OcrRecognition::Bound(_)));
    }
}
