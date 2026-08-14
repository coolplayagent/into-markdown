//! Explicit assembly of the installed, offline production OCR pipeline.

use crate::{ConversionError, ConversionOptions, ExecutionContext, OcrEngine};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Explicit local paths required to assemble the production OCR service.
#[derive(Debug, Clone)]
pub struct InstalledOcrConfig {
    /// Writable model-component root managed by `into-md models`.
    pub writable_model_root: PathBuf,
    /// Optional read-only packaged model-component root.
    pub bundled_model_root: Option<PathBuf>,
    /// Trusted root containing the audited ONNX Runtime distribution.
    pub runtime_trusted_root: PathBuf,
    /// Exact native runtime library below `runtime_trusted_root`.
    pub runtime_library: PathBuf,
    /// Exact absolute `onnxruntime-worker` executable path.
    pub worker_executable: PathBuf,
    /// Selected OCR bundle. Only an embedded available pipeline is accepted.
    pub model_bundle: String,
}

/// Assemble the real offline OCR service from explicitly installed components.
///
/// This operation performs local verification only. It never downloads a model,
/// searches `PATH`, reads a secret, or enables network access.
pub fn installed_ocr_service(
    config: &InstalledOcrConfig,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Arc<dyn OcrEngine>, ConversionError> {
    context.checkpoint()?;
    let manager = Arc::new(into_markdown_ocr::ModelManager::embedded(
        config.writable_model_root.clone(),
        config.bundled_model_root.clone(),
    )?);
    if config.model_bundle != manager.manifest().default_bundle {
        return Err(ConversionError::ComponentUnavailable {
            component: config.model_bundle.clone(),
            detail: "the selected OCR bundle is not an installed production pipeline".into(),
        });
    }
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
    let resolver = into_markdown_ocr::ManifestModelResolver::new(Arc::clone(&manager));
    let runtime = into_markdown_ocr::OnnxRuntime::new(
        Arc::new(resolver),
        Arc::new(factory),
        into_markdown_ocr::RuntimeConfig {
            runtime_version,
            ..into_markdown_ocr::RuntimeConfig::default()
        },
    )?;
    let engine = into_markdown_ocr::PpOcrImageEngine::from_installed(
        Arc::new(runtime),
        manager,
        options.limits.clone(),
        context,
    )?;
    Ok(Arc::new(engine))
}

/// Resolve the exact embedded-authority runtime-library location below a distribution root.
pub fn expected_ocr_runtime_library(trusted_root: &Path) -> Result<PathBuf, ConversionError> {
    into_markdown_onnxruntime::RuntimeLibrary::expected_path(trusted_root).map_err(|error| {
        ConversionError::ComponentUnavailable {
            component: "onnxruntime".into(),
            detail: format!("ONNX Runtime distribution path is unavailable: {error}"),
        }
    })
}
