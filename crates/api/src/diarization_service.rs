//! Explicit assembly of installed local anonymous speaker diarization.

use crate::{ConversionError, ConversionOptions, Diarizer, ExecutionContext};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

/// Explicit verified paths required for speaker diarization.
#[derive(Debug, Clone)]
pub struct InstalledDiarizationConfig {
    /// Writable model root managed by `into-md models`.
    pub writable_model_root: PathBuf,
    /// Optional read-only model root included in a full offline package.
    pub bundled_model_root: Option<PathBuf>,
    /// Trusted root containing the audited ONNX Runtime distribution.
    pub runtime_trusted_root: PathBuf,
    /// Exact native ONNX Runtime library.
    pub runtime_library: PathBuf,
    /// Exact isolated ONNX worker executable.
    pub worker_executable: PathBuf,
    /// Trusted root containing the audited `FFmpeg` distribution.
    pub ffmpeg_trusted_root: PathBuf,
    /// Exact audited `FFmpeg` executable.
    pub ffmpeg_executable: PathBuf,
    /// Generated `FFmpeg` artifact authority JSON.
    pub ffmpeg_authority: PathBuf,
    /// Selected embedded diarization bundle.
    pub model_bundle: String,
}

/// Assemble local Silero VAD plus 3D-Speaker embedding diarization.
///
/// This function performs verification only and never downloads components.
///
/// # Errors
///
/// Returns an error when diarization is disabled, the selected bundle is not authoritative,
/// or any model or native runtime artifact fails validation.
pub fn installed_diarization_service(
    config: &InstalledDiarizationConfig,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Arc<dyn Diarizer>, ConversionError> {
    context.checkpoint()?;
    if !options.diarization.enabled {
        return Err(ConversionError::ComponentUnavailable {
            component: "speaker-diarization".into(),
            detail: "speaker diarization is not enabled for this request".into(),
        });
    }
    let manager = Arc::new(into_markdown_ocr::ModelManager::embedded(
        config.writable_model_root.clone(),
        config.bundled_model_root.clone(),
    )?);
    if manager.manifest().default_diarization_bundle.as_deref() != Some(&config.model_bundle) {
        return Err(ConversionError::ComponentUnavailable {
            component: config.model_bundle.clone(),
            detail: "selected diarization bundle is not the reviewed default".into(),
        });
    }
    manager.verify_with_context(&config.model_bundle, context).map_err(|error| match error {
        into_markdown_ocr::ModelManagerError::Execution(error) => error,
        error => ConversionError::ComponentUnavailable {
            component: config.model_bundle.clone(),
            detail: format!(
                "installed diarization model verification failed ({error}); install it with `into-md models install {}`",
                config.model_bundle
            ),
        },
    })?;
    let vad =
        manager.verified_runtime_path(&config.model_bundle, "vad", context).map_err(|error| {
            ConversionError::ComponentUnavailable {
                component: config.model_bundle.clone(),
                detail: format!("installed VAD model identity is unavailable: {error}"),
            }
        })?;
    let embedding = manager
        .verified_runtime_path(&config.model_bundle, "speaker-embedding", context)
        .map_err(|error| ConversionError::ComponentUnavailable {
            component: config.model_bundle.clone(),
            detail: format!("installed speaker model identity is unavailable: {error}"),
        })?;
    let model_identity = format!(
        "{}@vad-sha256:{}+speaker-sha256:{}",
        config.model_bundle, vad.sha256, embedding.sha256
    );
    let library = into_markdown_onnxruntime::RuntimeLibrary::load(
        &config.runtime_trusted_root,
        &config.runtime_library,
    )
    .map_err(|error| ConversionError::ComponentUnavailable {
        component: "onnxruntime".into(),
        detail: format!("installed ONNX Runtime is unavailable: {error}"),
    })?;
    let runtime_version = library.version().to_owned();
    let factory = into_markdown_onnxruntime::OrtSessionFactory::new(
        Arc::new(library),
        config.worker_executable.clone(),
    )
    .map_err(|error| ConversionError::ComponentUnavailable {
        component: "onnxruntime-worker".into(),
        detail: format!("installed ONNX worker is unavailable: {error}"),
    })?;
    let resolver = into_markdown_asr::DiarizationModelResolver::new(Arc::clone(&manager));
    let runtime = into_markdown_ocr::OnnxRuntime::new(
        Arc::new(resolver),
        Arc::new(factory),
        into_markdown_ocr::RuntimeConfig {
            runtime_version,
            ..into_markdown_ocr::RuntimeConfig::default()
        },
    )?;
    let authority = fs::read(&config.ffmpeg_authority).map_err(|error| {
        ConversionError::ComponentUnavailable {
            component: "ffmpeg-lgpl".into(),
            detail: format!("FFmpeg artifact authority is unavailable: {error}"),
        }
    })?;
    let ffmpeg = into_markdown_ffmpeg::FfmpegRuntime::load(
        &config.ffmpeg_trusted_root,
        &config.ffmpeg_executable,
        &authority,
    )
    .map_err(|error| ConversionError::ComponentUnavailable {
        component: "ffmpeg-lgpl".into(),
        detail: format!("installed FFmpeg runtime is unavailable: {error}"),
    })?;
    Ok(Arc::new(into_markdown_asr::LocalSpeakerDiarizer::new(
        Arc::new(runtime),
        Arc::new(ffmpeg),
        model_identity,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCode, ExecutionOptions, ResourceLimits};

    #[test]
    fn missing_bundle_fails_before_native_runtime_lookup() {
        let root = tempfile::tempdir().unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let mut options = ConversionOptions::default();
        options.diarization.enabled = true;
        let config = InstalledDiarizationConfig {
            writable_model_root: root.path().join("models"),
            bundled_model_root: None,
            runtime_trusted_root: root.path().join("runtime"),
            runtime_library: root.path().join("runtime/missing"),
            worker_executable: root.path().join("missing-worker"),
            ffmpeg_trusted_root: root.path().join("ffmpeg"),
            ffmpeg_executable: root.path().join("ffmpeg/missing"),
            ffmpeg_authority: root.path().join("ffmpeg/missing-authority"),
            model_bundle: "silero-vad-3dspeaker-eres2net".into(),
        };
        let error = match installed_diarization_service(&config, &options, &context) {
            Ok(_) => panic!("missing diarization model unexpectedly assembled"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::ComponentUnavailable);
        assert!(error.to_string().contains("models install silero-vad-3dspeaker-eres2net"));
        assert!(!error.to_string().contains("missing-authority"));
    }
}
