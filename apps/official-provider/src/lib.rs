//! Official offline OCR and media capability workers.

use base64::Engine as _;
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
    DiarizationParameters, LegacyOfficeCapabilityResponse, LegacyOfficeParameters,
    OcrCapabilityResponse, OcrParameters, ReadinessParameters, TranscriptionParameters,
};
use std::fs;
use std::path::{Path, PathBuf};

const OCR_PLUGIN_ID: &str = "official.ocr.ppocrv6";
const MEDIA_PLUGIN_ID: &str = "official.media.whisper";
const LEGACY_OFFICE_PLUGIN_ID: &str = "official.legacy-office.libreoffice";
const LEGACY_OFFICE_PROVIDER_ID: &str = "builtin.legacy-office.libreoffice";
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
                ocr_service(&root, &parameters.options, &context)?;
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
            let engine = ocr_service(&root, &parameters.options, &context)?;
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

/// Serve legacy Office normalization using the plugin-owned LibreOffice runtime.
pub fn serve_legacy_office() -> std::io::Result<()> {
    serve_raw_with_isolated_stdout(
        LEGACY_OFFICE_PLUGIN_ID,
        MAX_FRAME_BYTES,
        |request, events, cancellation| {
            let context = execution_context(cancellation, &ResourceLimits::default());
            let root = distribution_root()?.join("legacy-office-runtime");
            let mut runtime_config = into_markdown_legacy_office::RuntimeConfig::new(
                root.join("authority.json"),
                root.clone(),
                root.join(legacy_worker_name()),
            );
            #[cfg(target_os = "macos")]
            if std::env::var_os("INTO_MARKDOWN_INHERITED_SANDBOX").as_deref()
                == Some(std::ffi::OsStr::new("process-v1"))
            {
                runtime_config = runtime_config.with_inherited_process_sandbox();
            }
            let runtime = into_markdown_legacy_office::LegacyOfficeRuntime::new(runtime_config);
            if request.input_format == "readiness" {
                let parameters: ReadinessParameters = parameters(&request)?;
                if parameters.schema_version != 1 || parameters.capability_id != "legacy-office" {
                    return Err(WorkerError::new(
                        "invalidRequest",
                        "legacy Office readiness identity is invalid",
                    ));
                }
                runtime.verify(&context).map_err(worker_error)?;
                return Ok("{\"ready\":true}".into());
            }
            if request.input_format != "legacy-office" {
                return Err(WorkerError::new(
                    "unsupportedOperation",
                    "expected legacy Office operation",
                ));
            }
            let parameters: LegacyOfficeParameters = parameters(&request)?;
            if parameters.schema_version != 1 || parameters.capability_id != "legacy-office" {
                return Err(WorkerError::new(
                    "invalidRequest",
                    "legacy Office capability identity is invalid",
                ));
            }
            let source = source_bytes(&request, &context)?;
            events
                .progress(
                    "converting",
                    Some(0),
                    Some(1),
                    Some("provider.legacy-office.start".into()),
                )
                .ok();
            let package = runtime
                .convert(
                    &source.bytes,
                    parameters.source_format,
                    parameters.maximum_output_bytes,
                    &context,
                )
                .map_err(worker_error)?;
            events
                .progress(
                    "converting",
                    Some(1),
                    Some(1),
                    Some("provider.legacy-office.complete".into()),
                )
                .ok();
            let format = match package.format {
                into_markdown_legacy_office::NormalizedFormat::Docx => {
                    into_markdown::InputFormat::Docx
                }
                into_markdown_legacy_office::NormalizedFormat::Pptx => {
                    into_markdown::InputFormat::Pptx
                }
                into_markdown_legacy_office::NormalizedFormat::Xlsx => {
                    into_markdown::InputFormat::Xlsx
                }
            };
            serde_json::to_string(&LegacyOfficeCapabilityResponse {
                schema_version: 1,
                capability_id: "legacy-office".into(),
                provider_id: LEGACY_OFFICE_PROVIDER_ID.into(),
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(&package.bytes),
                format,
                version: package.runtime.version().into(),
                artifact_sha256: package.runtime.artifact_sha256().into(),
                target: package.runtime.target().into(),
            })
            .map_err(|_| {
                WorkerError::new("invalidResult", "legacy Office result serialization failed")
            })
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
            asr_service(&root, &parameters.options, &context)?;
        }
        "diarization" => {
            diarization_service(&root, &parameters.options, &context)?;
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
    let transcriber = asr_service(&root, &parameters.options, &context)?;
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
    let diarizer = diarization_service(&root, &parameters.options, &context)?;
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
    options: &into_markdown::ConversionOptions,
    context: &ExecutionContext,
) -> Result<std::sync::Arc<dyn into_markdown::OcrEngine>, WorkerError> {
    let model_root = root.join("models");
    let runtime_root = root.join("onnxruntime");
    into_markdown::installed_ocr_service(
        &InstalledOcrConfig {
            writable_model_root: model_root.clone(),
            bundled_model_root: Some(model_root),
            runtime_library: into_markdown::expected_ocr_runtime_library(&runtime_root)
                .map_err(worker_error)?,
            runtime_trusted_root: runtime_root,
            worker_executable: root.join("bin").join(worker_name()),
            model_bundle: "pp-ocrv6-tiny-zh-en".into(),
        },
        options,
        context,
    )
    .map_err(worker_error)
}

fn asr_service(
    root: &Path,
    options: &into_markdown::ConversionOptions,
    context: &ExecutionContext,
) -> Result<std::sync::Arc<dyn into_markdown::Transcriber>, WorkerError> {
    let model_root = root.join("models");
    let ffmpeg_root = root.join("ffmpeg");
    into_markdown::installed_asr_service_in_read_only_sandbox(
        &InstalledAsrConfig {
            writable_model_root: model_root.clone(),
            bundled_model_root: Some(model_root),
            ffmpeg_executable: ffmpeg_root.join(ffmpeg_name()),
            ffmpeg_authority: ffmpeg_root.join("authority.json"),
            ffmpeg_trusted_root: ffmpeg_root,
            model_bundle: "whisper-small-multilingual".into(),
        },
        options,
        context,
    )
    .map_err(worker_error)
}

fn diarization_service(
    root: &Path,
    options: &into_markdown::ConversionOptions,
    context: &ExecutionContext,
) -> Result<std::sync::Arc<dyn into_markdown::Diarizer>, WorkerError> {
    let model_root = root.join("models");
    let runtime_root = root.join("onnxruntime");
    let ffmpeg_root = root.join("ffmpeg");
    into_markdown::installed_diarization_service_in_read_only_sandbox(
        &InstalledDiarizationConfig {
            writable_model_root: model_root.clone(),
            bundled_model_root: Some(model_root),
            runtime_library: into_markdown::expected_ocr_runtime_library(&runtime_root)
                .map_err(worker_error)?,
            runtime_trusted_root: runtime_root,
            worker_executable: root.join("bin").join(worker_name()),
            ffmpeg_executable: ffmpeg_root.join(ffmpeg_name()),
            ffmpeg_authority: ffmpeg_root.join("authority.json"),
            ffmpeg_trusted_root: ffmpeg_root,
            model_bundle: "silero-vad-3dspeaker-eres2net".into(),
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

fn worker_error(error: ConversionError) -> WorkerError {
    WorkerError::new(error.code().as_str(), error.to_string())
}

const fn worker_name() -> &'static str {
    if cfg!(windows) { "onnxruntime-worker.exe" } else { "onnxruntime-worker" }
}

const fn ffmpeg_name() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

const fn legacy_worker_name() -> &'static str {
    if cfg!(windows) { "legacy-office-worker.exe" } else { "legacy-office-worker" }
}

#[cfg(test)]
mod speech_tests {
    use super::*;
    use into_markdown_process_plugin::{
        PluginManifest, PluginRequest, ProcessPlugin, RuntimePolicy,
    };
    use sha2::{Digest as _, Sha256};
    use std::time::Duration;

    #[test]
    #[ignore = "requires an installed Speech plugin and a real audio fixture"]
    fn installed_speech_runtime_transcribes_real_audio_outside_process_sandbox() {
        let root = PathBuf::from(
            std::env::var_os("INTO_MD_TEST_SPEECH_PLUGIN_ROOT")
                .expect("INTO_MD_TEST_SPEECH_PLUGIN_ROOT is required"),
        );
        let input = PathBuf::from(
            std::env::var_os("INTO_MD_TEST_AUDIO").expect("INTO_MD_TEST_AUDIO is required"),
        );
        let bytes = fs::read(input).expect("real audio fixture must be readable");
        let mut options = into_markdown::ConversionOptions::default();
        options.asr.language = Some("zh".into());
        let context = execution_context(CancellationToken::new(), &options.limits);
        let transcriber = asr_service(&root, &options, &context).expect("speech runtime assembles");
        let result = block_on(transcriber.transcribe(
            TranscriptionRequest { media: &bytes, media_type: "audio/webm", language: Some("zh") },
            &context,
        ))
        .expect("real audio transcribes");
        assert!(!result.segments.is_empty(), "transcript must contain timed segments");
    }

    #[test]
    #[ignore = "requires an installed Speech runtime, built provider, and real audio fixture"]
    fn installed_speech_runtime_transcribes_real_audio_inside_process_sandbox() {
        let installed_root = PathBuf::from(
            std::env::var_os("INTO_MD_TEST_SPEECH_PLUGIN_ROOT")
                .expect("INTO_MD_TEST_SPEECH_PLUGIN_ROOT is required"),
        );
        let provider = PathBuf::from(
            std::env::var_os("INTO_MD_TEST_MEDIA_PROVIDER")
                .expect("INTO_MD_TEST_MEDIA_PROVIDER is required"),
        )
        .canonicalize()
        .expect("built provider path is valid");
        let input = fs::read(
            std::env::var_os("INTO_MD_TEST_AUDIO").expect("INTO_MD_TEST_AUDIO is required"),
        )
        .expect("real audio fixture must be readable");
        let staged = tempfile::Builder::new()
            .prefix("into-md-speech-sandbox-test-")
            .tempdir()
            .expect("private runtime staging directory is available");
        link_runtime_tree(&installed_root, staged.path());
        let staged_provider = staged.path().join("bin/into-md-media-provider");
        if staged_provider.exists() {
            fs::remove_file(&staged_provider).expect("old provider staging link is removable");
        }
        fs::hard_link(&provider, &staged_provider).expect("built provider is staged");
        let digest = format!("{:x}", Sha256::digest(fs::read(&provider).unwrap()));
        let mut policy = RuntimePolicy {
            max_frame_bytes: 64 * 1024 * 1024,
            max_output_bytes: 24 * 1024 * 1024,
            max_memory_bytes: 1536 * 1024 * 1024,
            max_file_bytes: 4 * 1024 * 1024 * 1024,
            max_open_files: 1024,
            request_timeout: Duration::from_secs(10 * 60),
            allow_child_processes: true,
            ..RuntimePolicy::default()
        };
        policy.macos_compatibility_child = false;
        let process = ProcessPlugin::new(
            PluginManifest {
                plugin_id: MEDIA_PLUGIN_ID.into(),
                executable: staged_provider.canonicalize().unwrap(),
                runtime_root: staged.path().canonicalize().unwrap(),
                executable_sha256: digest,
                protocol_versions: vec![1],
            },
            policy,
        )
        .expect("process authority is valid");
        let mut options = into_markdown::ConversionOptions::default();
        options.asr.language = Some("zh".into());
        let parameters = serde_json::to_string(&TranscriptionParameters {
            schema_version: 1,
            capability_id: "transcription".into(),
            media_type: "audio/webm".into(),
            language: Some("zh".into()),
            options,
        })
        .unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let result = process
            .execute_raw(
                PluginRequest {
                    request_id: "real-speech-sandbox",
                    input_format: "transcription",
                    source_name: Some("recording.webm"),
                    parameters_json: Some(&parameters),
                    source: &input,
                },
                &context,
            )
            .expect("real audio transcribes inside the process sandbox");
        let result: into_markdown::TranscriptionResult =
            serde_json::from_str(&result.result_json).expect("transcription result is typed");
        assert!(!result.segments.is_empty(), "transcript must contain timed segments");
    }

    fn link_runtime_tree(source: &Path, destination: &Path) {
        for entry in fs::read_dir(source).expect("runtime directory is readable") {
            let entry = entry.expect("runtime entry is readable");
            let source = entry.path();
            let destination = destination.join(entry.file_name());
            let metadata = entry.metadata().expect("runtime metadata is readable");
            if metadata.is_dir() {
                fs::create_dir(&destination).expect("runtime directory is staged");
                link_runtime_tree(&source, &destination);
            } else if metadata.is_file()
                && !matches!(entry.file_name().to_str(), Some(".package.zip" | ".installed.json"))
            {
                fs::hard_link(&source, &destination).expect("runtime file is staged");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown::OcrPolicy;

    #[test]
    #[ignore = "requires a self-contained INTO_MD_PROVIDER_RUNTIME and INTO_MD_OCR_FIXTURE"]
    fn explicit_installed_ocr_runtime_smoke() {
        let runtime = PathBuf::from(std::env::var_os("INTO_MD_PROVIDER_RUNTIME").unwrap());
        let fixture = fs::read(std::env::var_os("INTO_MD_OCR_FIXTURE").unwrap()).unwrap();
        let limits = ResourceLimits {
            max_memory_bytes: 8 * 1024 * 1024 * 1024,
            max_temporary_bytes: 4 * 1024 * 1024 * 1024,
            ..ResourceLimits::default()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        let mut options = into_markdown::ConversionOptions { limits, ..Default::default() };
        options.ocr.policy = OcrPolicy::Always;
        let engine = ocr_service(&runtime, &options, &context).unwrap();
        let result = block_on(engine.recognize_bound(
            OcrRequest { image: &fixture, media_type: "image/png", languages: &[] },
            &context,
        ))
        .unwrap();
        assert!(matches!(result, into_markdown::OcrRecognition::Bound(_)));
    }
}
