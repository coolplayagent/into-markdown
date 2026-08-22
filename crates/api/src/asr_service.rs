//! Explicit assembly of the installed offline Whisper-small pipeline.

use crate::{ConversionError, ConversionOptions, ExecutionContext, Transcriber};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

/// Explicit local paths required to assemble production ASR.
#[derive(Debug, Clone)]
pub struct InstalledAsrConfig {
    /// Plugin-private model root.
    pub writable_model_root: PathBuf,
    /// Optional read-only model root included by the Speech plugin.
    pub bundled_model_root: Option<PathBuf>,
    /// Trusted directory containing the audited FFmpeg artifact.
    pub ffmpeg_trusted_root: PathBuf,
    /// Exact FFmpeg executable below the trusted root.
    pub ffmpeg_executable: PathBuf,
    /// Generated FFmpeg artifact authority JSON.
    pub ffmpeg_authority: PathBuf,
    /// Selected embedded ASR model bundle.
    pub model_bundle: String,
}

/// Assemble an installed Whisper-small service without networking.
/// macOS release builds prefer Metal and retain an automatic CPU fallback.
pub fn installed_asr_service(
    config: &InstalledAsrConfig,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Arc<dyn Transcriber>, ConversionError> {
    installed_asr_service_inner(config, options, context, false)
}

/// Assemble local ASR when the caller already enforces a read-only,
/// replacement-protected native-runtime sandbox.
pub fn installed_asr_service_in_read_only_sandbox(
    config: &InstalledAsrConfig,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Arc<dyn Transcriber>, ConversionError> {
    installed_asr_service_inner(config, options, context, true)
}

fn installed_asr_service_inner(
    config: &InstalledAsrConfig,
    options: &ConversionOptions,
    context: &ExecutionContext,
    read_only_sandbox: bool,
) -> Result<Arc<dyn Transcriber>, ConversionError> {
    context.checkpoint()?;
    let manager = Arc::new(if read_only_sandbox {
        into_markdown_ocr::ModelManager::embedded_authenticated_read_only_snapshot(
            config.writable_model_root.clone(),
            config.bundled_model_root.clone(),
        )?
    } else {
        into_markdown_ocr::ModelManager::embedded(
            config.writable_model_root.clone(),
            config.bundled_model_root.clone(),
        )?
    });
    if manager.manifest().default_asr_bundle.as_deref() != Some(&config.model_bundle) {
        return Err(ConversionError::ComponentUnavailable {
            component: config.model_bundle.clone(),
            detail: "selected ASR bundle is not the reviewed default".into(),
        });
    }
    manager.verify_with_context(&config.model_bundle, context).map_err(|error| match error {
        into_markdown_ocr::ModelManagerError::Execution(error) => error,
        error => ConversionError::ComponentUnavailable {
            component: config.model_bundle.clone(),
            detail: format!(
                "installed Speech capability verification failed ({error}); repair or reinstall the Speech plugin"
            ),
        },
    })?;
    let authority = fs::read(&config.ffmpeg_authority).map_err(|error| {
        ConversionError::ComponentUnavailable {
            component: "ffmpeg-lgpl".into(),
            detail: format!("FFmpeg artifact authority is unavailable: {error}"),
        }
    })?;
    let runtime = if read_only_sandbox {
        into_markdown_ffmpeg::FfmpegRuntime::load_read_only_sandbox(
            &config.ffmpeg_trusted_root,
            &config.ffmpeg_executable,
            &authority,
        )
    } else {
        into_markdown_ffmpeg::FfmpegRuntime::load(
            &config.ffmpeg_trusted_root,
            &config.ffmpeg_executable,
            &authority,
        )
    }
    .map_err(|error| ConversionError::ComponentUnavailable {
        component: "ffmpeg-lgpl".into(),
        detail: format!("installed FFmpeg runtime is unavailable: {error}"),
    })?;
    let asr_options = options.asr.clone();
    let whisper_config = into_markdown_asr::WhisperConfig::try_from(&asr_options)?;
    let transcriber = into_markdown_asr::WhisperSmallTranscriber::new(
        manager,
        Arc::new(runtime),
        whisper_config,
    )?;
    Ok(Arc::new(transcriber))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCode, ExecutionOptions, ResourceLimits};

    #[test]
    fn missing_model_fails_before_ffmpeg_authority_lookup() {
        let root = tempfile::tempdir().unwrap();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let config = InstalledAsrConfig {
            writable_model_root: root.path().join("models"),
            bundled_model_root: None,
            ffmpeg_trusted_root: root.path().join("ffmpeg"),
            ffmpeg_executable: root.path().join("ffmpeg/missing"),
            ffmpeg_authority: root.path().join("ffmpeg/missing-authority.json"),
            model_bundle: "whisper-small-multilingual".into(),
        };
        let error = match installed_asr_service(&config, &ConversionOptions::default(), &context) {
            Ok(_) => panic!("missing model unexpectedly assembled"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::ComponentUnavailable);
        assert!(error.to_string().contains("repair or reinstall the Speech plugin"));
        assert!(!error.to_string().contains("missing-authority"));
    }
}
