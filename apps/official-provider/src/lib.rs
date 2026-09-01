//! Official offline OCR and media capability workers.

use futures::executor::block_on;
use into_markdown::{
    CancellationToken, ConversionError, ExecutionContext, ExecutionOptions, ResourceLimits,
};
#[cfg(feature = "media-runtime")]
use into_markdown::{
    DiarizationRequest, InstalledAsrConfig, InstalledDiarizationConfig, TranscriptionRequest,
};
#[cfg(feature = "ocr-runtime")]
use into_markdown::{InstalledOcrConfig, OcrRequest};
#[cfg(feature = "media-runtime")]
use into_markdown_process_plugin::worker::WorkerEvents;
use into_markdown_process_plugin::worker::{
    WorkerError, WorkerRequest, serve_raw_with_isolated_stdout,
};
use into_markdown_provider_plugin::ReadinessParameters;
#[cfg(feature = "media-runtime")]
use into_markdown_provider_plugin::{DiarizationParameters, TranscriptionParameters};
#[cfg(feature = "ocr-runtime")]
use into_markdown_provider_plugin::{OcrCapabilityResponse, OcrParameters};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "ocr-runtime")]
const OCR_PLUGIN_ID: &str = "official.ocr.ppocrv6";
#[cfg(feature = "media-runtime")]
const MEDIA_PLUGIN_ID: &str = "official.media.whisper";
const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Serve one OCR request on the authenticated process protocol.
///
/// # Errors
///
/// Returns an I/O error when the authenticated worker protocol cannot be served.
#[cfg(feature = "ocr-runtime")]
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
            .map_err(|error| worker_error(&error))?;
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
///
/// # Errors
///
/// Returns an I/O error when the authenticated worker protocol cannot be served.
#[cfg(feature = "media-runtime")]
pub fn serve_media() -> std::io::Result<()> {
    initialize_media_runtime()?;
    serve_raw_with_isolated_stdout(
        MEDIA_PLUGIN_ID,
        MAX_FRAME_BYTES,
        |request, events, cancellation| match request.input_format.as_str() {
            "readiness" => media_readiness(&request, cancellation),
            "transcription" => transcribe(&request, &events, cancellation),
            "diarization" => diarize(&request, &events, cancellation),
            _ => Err(WorkerError::new(
                "unsupportedOperation",
                "expected transcription or diarization operation",
            )),
        },
    )
}

#[cfg(feature = "media-runtime")]
fn initialize_media_runtime() -> std::io::Result<()> {
    let executable = current_provider_executable()
        .map_err(|_| std::io::Error::other("provider executable identity is invalid"))?;
    let runtime_directory = executable
        .parent()
        .ok_or_else(|| std::io::Error::other("provider runtime directory is unavailable"))?;
    into_markdown::initialize_cpu_runtime(runtime_directory)
}

#[cfg(feature = "media-runtime")]
fn media_readiness(
    request: &WorkerRequest,
    cancellation: CancellationToken,
) -> Result<String, WorkerError> {
    let mut parameters: ReadinessParameters = parameters(request)?;
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

#[cfg(feature = "media-runtime")]
fn transcribe(
    request: &WorkerRequest,
    events: &WorkerEvents,
    cancellation: CancellationToken,
) -> Result<String, WorkerError> {
    let parameters: TranscriptionParameters = parameters(request)?;
    if parameters.schema_version != 1 || parameters.capability_id != "transcription" {
        return Err(WorkerError::new(
            "invalidRequest",
            "transcription capability identity is invalid",
        ));
    }
    let context = execution_context(cancellation, &parameters.options.limits);
    events.progress("ai", Some(0), Some(1), Some("provider.transcription.start".into())).ok();
    let source = source_bytes(request, &context)?;
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
    .map_err(|error| worker_error(&error))?;
    events.progress("ai", Some(1), Some(1), Some("provider.transcription.complete".into())).ok();
    serde_json::to_string(&result)
        .map_err(|_| WorkerError::new("invalidResult", "transcription result serialization failed"))
}

#[cfg(feature = "media-runtime")]
fn diarize(
    request: &WorkerRequest,
    events: &WorkerEvents,
    cancellation: CancellationToken,
) -> Result<String, WorkerError> {
    let parameters: DiarizationParameters = parameters(request)?;
    if parameters.schema_version != 1 || parameters.capability_id != "diarization" {
        return Err(WorkerError::new(
            "invalidRequest",
            "diarization capability identity is invalid",
        ));
    }
    let context = execution_context(cancellation, &parameters.options.limits);
    events.progress("ai", Some(0), Some(1), Some("provider.diarization.start".into())).ok();
    let source = source_bytes(request, &context)?;
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
    .map_err(|error| worker_error(&error))?;
    events.progress("ai", Some(1), Some(1), Some("provider.diarization.complete".into())).ok();
    serde_json::to_string(&result)
        .map_err(|_| WorkerError::new("invalidResult", "diarization result serialization failed"))
}

#[cfg(feature = "ocr-runtime")]
fn ocr_service(
    root: &Path,
    options: &into_markdown::ConversionOptions,
    context: &ExecutionContext,
) -> Result<std::sync::Arc<dyn into_markdown::OcrEngine>, WorkerError> {
    let model_root = root.join("models");
    let runtime_root = root.join("onnxruntime");
    into_markdown::installed_ocr_service_in_read_only_sandbox(
        &InstalledOcrConfig {
            writable_model_root: model_root.clone(),
            bundled_model_root: Some(model_root),
            runtime_library: into_markdown::expected_ocr_runtime_library(&runtime_root)
                .map_err(|error| worker_error(&error))?,
            runtime_trusted_root: runtime_root,
            worker_executable: root.join("bin").join(worker_name()),
            model_bundle: "pp-ocrv6-tiny-zh-en".into(),
        },
        options,
        context,
    )
    .map_err(|error| worker_error(&error))
}

#[cfg(feature = "media-runtime")]
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
    .map_err(|error| worker_error(&error))
}

#[cfg(feature = "media-runtime")]
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
                .map_err(|error| worker_error(&error))?,
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
    .map_err(|error| worker_error(&error))
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
        let memory = context.reserve_memory(size).map_err(|error| worker_error(&error))?;
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
    let temporary_directory = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    ExecutionContext::new_with_temporary_directory(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        limits.clone(),
        temporary_directory,
    )
}

fn distribution_root() -> Result<PathBuf, WorkerError> {
    let executable = current_provider_executable()?;
    let parent = executable.parent().ok_or_else(|| {
        WorkerError::new("componentUnavailable", "provider runtime root is unavailable")
    })?;
    Ok(if parent.file_name().is_some_and(|name| name == "bin") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    })
}

fn current_provider_executable() -> Result<PathBuf, WorkerError> {
    let executable = std::env::current_exe().map_err(|_| {
        WorkerError::new("componentUnavailable", "provider executable is unavailable")
    })?;
    #[cfg(not(windows))]
    {
        return executable.canonicalize().map_err(|_| {
            WorkerError::new("componentUnavailable", "provider executable is unavailable")
        });
    }
    #[cfg(windows)]
    {
        // GetModuleFileNameW, which backs current_exe, returns the loaded image's absolute
        // kernel identity. Canonicalizing it reopens every ancestor and fails deliberately for
        // an AppContainer whose snapshot parent grants the worker no directory-list authority.
        // The manager already rejected reparse points, hash-verified the private snapshot, and
        // retained it for this process lifetime, so preserve that authenticated image path.
        if executable.is_absolute()
            && !executable
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            Ok(executable)
        } else {
            Err(WorkerError::new("componentUnavailable", "provider executable identity is invalid"))
        }
    }
}

fn worker_error(error: &ConversionError) -> WorkerError {
    let code = match error {
        ConversionError::ResourceLimit {
            limit:
                "ocrRecognitionMemory"
                | "recognitionMemory"
                | "recognitionCropMemory"
                | "recognitionOutputMemory",
            ..
        } => "ocrRecognitionMemory",
        ConversionError::ResourceLimit { limit: "recognitionWidth", .. } => "ocrWidthLimit",
        ConversionError::ResourceLimit { limit: "recognitionCropPixels", .. } => "ocrPixelLimit",
        ConversionError::ResourceLimit {
            limit: "recognitionTensorElements" | "recognitionOutputElements",
            ..
        } => "ocrTensorLimit",
        ConversionError::ResourceLimit {
            limit: "recognitionRegions" | "recognitionDecodedBytes",
            ..
        } => "ocrStructureLimit",
        _ => error.code().as_str(),
    };
    WorkerError::new(code, error.to_string())
}

const fn worker_name() -> &'static str {
    if cfg!(windows) { "onnxruntime-worker.exe" } else { "onnxruntime-worker" }
}

#[cfg(feature = "media-runtime")]
const fn ffmpeg_name() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

#[cfg(all(test, feature = "media-runtime"))]
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
        let assembly_started = std::time::Instant::now();
        let transcriber = asr_service(&root, &options, &context).expect("speech runtime assembles");
        let assembly_ms = assembly_started.elapsed().as_secs_f64() * 1000.0;
        let transcription_started = std::time::Instant::now();
        let result = block_on(transcriber.transcribe(
            TranscriptionRequest { media: &bytes, media_type: "audio/webm", language: Some("zh") },
            &context,
        ))
        .expect("real audio transcribes");
        let transcription_ms = transcription_started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "{}",
            serde_json::json!({
                "assemblyMs": (assembly_ms * 100.0).round() / 100.0,
                "transcriptionMs": (transcription_ms * 100.0).round() / 100.0,
            })
        );
        assert!(!result.segments.is_empty(), "transcript must contain timed segments");
        assert!(
            !serde_json::to_string(&result).unwrap().contains('\u{fffd}'),
            "transcript must not contain damaged UTF-8 replacement text"
        );
    }

    #[test]
    #[ignore = "requires an installed Speech plugin and a real two-speaker audio fixture"]
    fn installed_speech_runtime_diarizes_real_audio_outside_process_sandbox() {
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
        options.diarization.enabled = true;
        options.diarization.expected_speakers = Some(2);
        let context = execution_context(CancellationToken::new(), &options.limits);
        let started = std::time::Instant::now();
        let transcriber = asr_service(&root, &options, &context).expect("speech runtime assembles");
        let transcription = block_on(transcriber.transcribe(
            TranscriptionRequest { media: &bytes, media_type: "audio/wav", language: Some("zh") },
            &context,
        ))
        .expect("real audio transcribes");
        let diarizer =
            diarization_service(&root, &options, &context).expect("diarization runtime assembles");
        let result = block_on(diarizer.diarize(
            into_markdown::DiarizationRequest {
                media: &bytes,
                media_type: "audio/wav",
                segments: &transcription.segments,
                expected_speakers: Some(2),
                max_speakers: options.diarization.max_speakers,
            },
            &context,
        ))
        .expect("real audio diarizes");
        let document = into_markdown::Document {
            blocks: result.segments.clone(),
            ..into_markdown::Document::default()
        };
        document.validate().expect("diarization result must remain valid Document IR");
        let speakers = result
            .segments
            .iter()
            .filter_map(|node| match &node.block {
                into_markdown::Block::TimedSegment { speaker, .. } => speaker.as_deref(),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        eprintln!(
            "{}",
            serde_json::json!({
                "diarizationMs": (started.elapsed().as_secs_f64() * 100_000.0).round() / 100.0,
                "segments": result.segments.len(),
                "speakers": speakers.len(),
            })
        );
        assert_eq!(speakers.len(), 2, "the controlled two-speaker fixture must retain two labels");
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
        copy_runtime_tree(&installed_root, staged.path());
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
            request_timeout: Duration::from_mins(10),
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
        let execution_started = std::time::Instant::now();
        let result = process
            .execute_raw(
                PluginRequest {
                    memory_limit: None,
                    request_id: "real-speech-sandbox",
                    input_format: "transcription",
                    source_name: Some("recording.webm"),
                    parameters_json: Some(&parameters),
                    source: &input,
                },
                &context,
            )
            .expect("real audio transcribes inside the process sandbox");
        let execution_ms = execution_started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "{}",
            serde_json::json!({
                "sandboxExecutionMs": (execution_ms * 100.0).round() / 100.0,
            })
        );
        let result: into_markdown::TranscriptionResult =
            serde_json::from_str(&result.result_json).expect("transcription result is typed");
        assert!(!result.segments.is_empty(), "transcript must contain timed segments");
        assert!(
            !serde_json::to_string(&result).unwrap().contains('\u{fffd}'),
            "transcript must not contain damaged UTF-8 replacement text"
        );
    }

    fn copy_runtime_tree(source: &Path, destination: &Path) {
        for entry in fs::read_dir(source).expect("runtime directory is readable") {
            let entry = entry.expect("runtime entry is readable");
            let source = entry.path();
            let destination = destination.join(entry.file_name());
            let metadata = entry.metadata().expect("runtime metadata is readable");
            if metadata.is_dir() {
                fs::create_dir(&destination).expect("runtime directory is staged");
                copy_runtime_tree(&source, &destination);
            } else if metadata.is_file()
                && !matches!(entry.file_name().to_str(), Some(".package.zip" | ".installed.json"))
            {
                let mut input = fs::File::open(&source).expect("runtime source opens");
                let mut output = fs::File::options()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .expect("runtime destination opens");
                std::io::copy(&mut input, &mut output).expect("runtime file is copied");
                fs::set_permissions(&destination, metadata.permissions())
                    .expect("runtime permissions are copied");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn worker_terminal_classification_is_stage_specific() {
        use super::*;
        let controlled = ConversionError::ResourceLimit {
            limit: "ocrRecognitionMemory",
            detail: "private refusal".into(),
        };
        assert_eq!(worker_error(&controlled).code, "ocrRecognitionMemory");
        for (limit, expected) in [
            ("recognitionMemory", "ocrRecognitionMemory"),
            ("recognitionCropMemory", "ocrRecognitionMemory"),
            ("recognitionOutputMemory", "ocrRecognitionMemory"),
            ("recognitionWidth", "ocrWidthLimit"),
            ("recognitionCropPixels", "ocrPixelLimit"),
            ("recognitionTensorElements", "ocrTensorLimit"),
            ("recognitionOutputElements", "ocrTensorLimit"),
            ("recognitionRegions", "ocrStructureLimit"),
            ("recognitionDecodedBytes", "ocrStructureLimit"),
        ] {
            assert_eq!(
                worker_error(&ConversionError::ResourceLimit {
                    limit,
                    detail: "fixed bound".into()
                })
                .code,
                expected
            );
        }
        for limit in ["max_memory_bytes", "workerProtocol", "nativeWorkerMemory"] {
            let error = ConversionError::ResourceLimit { limit, detail: "private refusal".into() };
            assert_eq!(worker_error(&error).code, "resourceLimit");
        }
    }
    use super::*;
    use into_markdown::OcrPolicy;

    #[test]
    fn provider_image_identity_resolves_to_an_absolute_distribution_root() {
        let executable = current_provider_executable().unwrap();
        assert!(executable.is_absolute());
        assert!(distribution_root().unwrap().is_absolute());
    }

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
