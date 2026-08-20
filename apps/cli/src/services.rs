//! Per-invocation optional-service assembly with no implicit discovery or download.

use crate::config::LoadedConfig;
use crate::error::CliError;
use into_markdown::{
    AiMode, ConversionError, ConversionOptions, ExecutionContext, ExecutionOptions,
    InstalledAsrConfig, InstalledDiarizationConfig, InstalledOcrConfig, OcrPolicy,
    OpenAiCompatibleClient, OpenAiImageDescriptionProvider,
    ProviderConfig as TransportProviderConfig, ProviderNetworkPolicy, Services,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

pub(crate) fn assemble(
    loaded: &LoadedConfig,
    execution: &ExecutionOptions,
) -> Result<Services, CliError> {
    let executable = canonical_executable().map_err(CliError::component)?;
    assemble_at(loaded, execution, &executable)
}

/// Assemble the exact local media services required by one durable Web meeting
/// request. The helper verifies installed components and never downloads them.
pub(crate) fn assemble_web_media(
    options: &ConversionOptions,
    execution: &ExecutionOptions,
) -> Result<Services, CliError> {
    let executable = canonical_executable().map_err(CliError::component)?;
    let context = ExecutionContext::new(execution.clone(), options.limits.clone());
    let mut services = Services {
        transcriber: Some(assemble_asr_options(options, &context, &executable)?),
        ..Services::default()
    };
    if options.diarization.enabled {
        let directory = executable.parent().ok_or_else(|| {
            CliError::component("current executable has no distribution directory")
        })?;
        services.diarizer = Some(
            assemble_diarization_config(options, directory, &context).map_err(CliError::from)?,
        );
    }
    Ok(services)
}

/// Verify the exact local OCR distribution used by conversion without any
/// download or network access.
pub(crate) fn verify_ocr_runtime(loaded: &LoadedConfig) -> Result<(), ConversionError> {
    let executable = canonical_executable().map_err(|detail| {
        ConversionError::ComponentUnavailable { component: "onnxruntime".into(), detail }
    })?;
    let directory = executable.parent().ok_or_else(|| ConversionError::ComponentUnavailable {
        component: "onnxruntime-worker".into(),
        detail: "current executable has no distribution directory".into(),
    })?;
    into_markdown::verify_ocr_worker_executable(&directory.join(worker_name())).map_err(
        |error| ConversionError::ComponentUnavailable {
            component: "onnxruntime-worker".into(),
            detail: format!("installed ONNX worker is unavailable: {error}"),
        },
    )?;
    let context = ExecutionContext::new(ExecutionOptions::default(), loaded.options.limits.clone());
    assemble_ocr(loaded, &context, &executable).map(drop)
}

/// Verify the exact offline ASR distribution used by the Web workbench.
pub(crate) fn verify_asr_runtime() -> Result<(), ConversionError> {
    let executable = canonical_executable().map_err(|detail| {
        ConversionError::ComponentUnavailable { component: "whisper-small".into(), detail }
    })?;
    let directory = executable.parent().ok_or_else(|| ConversionError::ComponentUnavailable {
        component: "whisper-small".into(),
        detail: "current executable has no distribution directory".into(),
    })?;
    let context = ExecutionContext::new(
        ExecutionOptions::default(),
        into_markdown::ResourceLimits::default(),
    );
    let ffmpeg_root = directory.join("ffmpeg");
    into_markdown::installed_asr_service(
        &InstalledAsrConfig {
            writable_model_root: writable_model_root()?,
            bundled_model_root: bundled_model_root(directory),
            ffmpeg_trusted_root: ffmpeg_root.clone(),
            ffmpeg_executable: ffmpeg_root.join(ffmpeg_name()),
            ffmpeg_authority: ffmpeg_root.join("authority.json"),
            model_bundle: "whisper-small-multilingual".into(),
        },
        &ConversionOptions::default(),
        &context,
    )
    .map(drop)
}

/// Verify the exact offline diarization distribution used by the meeting page.
pub(crate) fn verify_diarization_runtime() -> Result<(), ConversionError> {
    let executable = canonical_executable().map_err(|detail| {
        ConversionError::ComponentUnavailable { component: "speaker-diarization".into(), detail }
    })?;
    let directory = executable.parent().ok_or_else(|| ConversionError::ComponentUnavailable {
        component: "speaker-diarization".into(),
        detail: "current executable has no distribution directory".into(),
    })?;
    let mut options = ConversionOptions::default();
    options.diarization.enabled = true;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assemble_diarization_config(&options, directory, &context).map(drop)
}

/// Revision of the writable media-model root used to invalidate a cached
/// unavailable status after an explicit `setup media` installation.
pub(crate) fn media_model_revision() -> u128 {
    let Ok(root) = writable_model_root() else { return 0 };
    let Ok(metadata) = std::fs::metadata(root) else { return 0 };
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(1, |duration| duration.as_nanos().saturating_add(1))
}

fn assemble_at(
    loaded: &LoadedConfig,
    execution: &ExecutionOptions,
    executable: &Path,
) -> Result<Services, CliError> {
    let mut services = Services::default();
    let context = ExecutionContext::new(execution.clone(), loaded.options.limits.clone());
    if loaded.options.ocr.policy != OcrPolicy::Off {
        match assemble_ocr(loaded, &context, executable) {
            Ok(engine) => services.ocr = Some(engine),
            Err(error) if can_degrade_ocr(loaded.options.ocr.policy, &error) => {}
            Err(error) => return Err(CliError::from(error)),
        }
    }
    if loaded.options.ai.image_description != AiMode::Off {
        services.ai = assemble_image_description(loaded)?;
    }
    if loaded.options.ai.audio_transcription != AiMode::Off {
        services.transcriber = Some(assemble_asr(loaded, &context, executable)?);
    }
    if loaded.options.diarization.enabled {
        let directory = executable.parent().ok_or_else(|| {
            CliError::component("current executable has no distribution directory")
        })?;
        services.diarizer = Some(
            assemble_diarization_config(&loaded.options, directory, &context)
                .map_err(CliError::from)?,
        );
    }
    Ok(services)
}

fn assemble_diarization_config(
    options: &ConversionOptions,
    directory: &Path,
    context: &ExecutionContext,
) -> Result<Arc<dyn into_markdown::Diarizer>, ConversionError> {
    let runtime_root = directory.join("onnxruntime");
    let runtime_library = into_markdown::expected_ocr_runtime_library(&runtime_root)?;
    let ffmpeg_root = directory.join("ffmpeg");
    into_markdown::installed_diarization_service(
        &InstalledDiarizationConfig {
            writable_model_root: writable_model_root()?,
            bundled_model_root: bundled_model_root(directory),
            runtime_trusted_root: runtime_root,
            runtime_library,
            worker_executable: directory.join(worker_name()),
            ffmpeg_trusted_root: ffmpeg_root.clone(),
            ffmpeg_executable: ffmpeg_root.join(ffmpeg_name()),
            ffmpeg_authority: ffmpeg_root.join("authority.json"),
            model_bundle: options.diarization.model_bundle.clone(),
        },
        options,
        context,
    )
}

fn assemble_asr(
    loaded: &LoadedConfig,
    context: &ExecutionContext,
    executable: &Path,
) -> Result<Arc<dyn into_markdown::Transcriber>, CliError> {
    assemble_asr_options(&loaded.options, context, executable)
}

fn assemble_asr_options(
    options: &ConversionOptions,
    context: &ExecutionContext,
    executable: &Path,
) -> Result<Arc<dyn into_markdown::Transcriber>, CliError> {
    let directory = executable
        .parent()
        .ok_or_else(|| CliError::component("current executable has no distribution directory"))?;
    let ffmpeg_root = directory.join("ffmpeg");
    into_markdown::installed_asr_service(
        &InstalledAsrConfig {
            writable_model_root: writable_model_root()?,
            bundled_model_root: bundled_model_root(directory),
            ffmpeg_trusted_root: ffmpeg_root.clone(),
            ffmpeg_executable: ffmpeg_root.join(ffmpeg_name()),
            ffmpeg_authority: ffmpeg_root.join("authority.json"),
            model_bundle: options.asr.model_bundle.clone(),
        },
        options,
        context,
    )
    .map_err(CliError::from)
}

fn can_degrade_ocr(policy: OcrPolicy, error: &into_markdown::ConversionError) -> bool {
    policy == OcrPolicy::Auto
        && matches!(error, into_markdown::ConversionError::ComponentUnavailable { .. })
}

fn assemble_ocr(
    loaded: &LoadedConfig,
    context: &ExecutionContext,
    executable: &Path,
) -> Result<Arc<dyn into_markdown::OcrEngine>, into_markdown::ConversionError> {
    let directory = executable.parent().ok_or_else(|| {
        into_markdown::ConversionError::ComponentUnavailable {
            component: "onnxruntime-worker".into(),
            detail: "current executable has no distribution directory".into(),
        }
    })?;
    let runtime_root = directory.join("onnxruntime");
    let runtime_library = into_markdown::expected_ocr_runtime_library(&runtime_root)?;
    let model_bundle =
        loaded.options.ocr.model_bundle.clone().unwrap_or_else(|| "pp-ocrv6-tiny-zh-en".into());
    into_markdown::installed_ocr_service(
        &InstalledOcrConfig {
            writable_model_root: writable_model_root()?,
            bundled_model_root: bundled_model_root(directory),
            runtime_trusted_root: runtime_root,
            runtime_library,
            worker_executable: directory.join(worker_name()),
            model_bundle,
        },
        &loaded.options,
        context,
    )
}

fn assemble_image_description(
    loaded: &LoadedConfig,
) -> Result<Option<Arc<dyn into_markdown::AiProvider>>, CliError> {
    let Some(name) = loaded.ai_provider.as_deref() else {
        return Ok(None);
    };
    let Some(configured) = loaded.effective.providers.get(name) else {
        return Ok(None);
    };
    if !configured.capabilities.iter().any(|value| value == "image-description") {
        return Ok(None);
    }
    let model = loaded.ai_model.as_deref().unwrap_or(&configured.model);
    let timeout = std::time::Duration::from_millis(
        configured.timeout_ms.or(loaded.timeout_ms).unwrap_or(30_000),
    );
    let config = TransportProviderConfig::parse(
        &configured.base_url,
        model,
        &configured.api_key_env,
        timeout,
        configured.capabilities.clone(),
    )?;
    let network = ProviderNetworkPolicy {
        allow_network: loaded.options.network.enabled,
        allow_private_network: !loaded.options.network.deny_private_networks,
        allowed_hosts: loaded.options.network.allowed_hosts.clone(),
    };
    let client = OpenAiCompatibleClient::new(config, network.clone());
    Ok(Some(Arc::new(OpenAiImageDescriptionProvider::new(client, network))))
}

fn writable_model_root() -> Result<PathBuf, into_markdown::ConversionError> {
    directories::ProjectDirs::from("", "", "into-markdown")
        .map(|directories| directories.data_dir().join("models"))
        .ok_or_else(|| into_markdown::ConversionError::ComponentUnavailable {
            component: "ocr-models".into(),
            detail: "platform model data directory is unavailable".into(),
        })
}

fn bundled_model_root(directory: &Path) -> Option<PathBuf> {
    let path = directory.join("models");
    path.is_dir().then_some(path)
}

fn canonical_executable() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot resolve the current executable: {error}"))?;
    executable
        .canonicalize()
        .map_err(|error| format!("cannot resolve the installed executable: {error}"))
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

    #[test]
    fn auto_degrades_only_component_absence_and_preserves_execution_failures() {
        let unavailable = into_markdown::ConversionError::ComponentUnavailable {
            component: "ocr".into(),
            detail: "missing".into(),
        };
        assert!(can_degrade_ocr(OcrPolicy::Auto, &unavailable));
        assert!(!can_degrade_ocr(OcrPolicy::Always, &unavailable));
        assert!(!can_degrade_ocr(OcrPolicy::Auto, &into_markdown::ConversionError::Cancelled));
        assert!(!can_degrade_ocr(OcrPolicy::Auto, &into_markdown::ConversionError::Timeout));
        assert!(!can_degrade_ocr(
            OcrPolicy::Auto,
            &into_markdown::ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "low memory".into(),
            },
        ));
    }
}
