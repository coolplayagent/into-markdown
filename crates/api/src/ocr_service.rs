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
///
/// # Errors
///
/// Returns cancellation, timeout, or resource errors from the execution
/// context; [`ConversionError::ComponentUnavailable`] when any selected model
/// component or runtime artifact is missing or corrupt; and a stable OCR error
/// when the verified runtime cannot construct the bounded pipeline.
pub fn installed_ocr_service(
    config: &InstalledOcrConfig,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Arc<dyn OcrEngine>, ConversionError> {
    context.checkpoint()?;
    into_markdown_ocr::PpOcrImageEngine::validate_service_limits(&options.limits, context)?;
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
    manager.verify_with_context(&config.model_bundle, context).map_err(|error| match error {
        into_markdown_ocr::ModelManagerError::Execution(error) => error,
        error => ConversionError::ComponentUnavailable {
            component: config.model_bundle.clone(),
            detail: format!(
                "installed OCR pipeline verification failed ({error}); install it with `into-md \
                 models install {}`",
                config.model_bundle
            ),
        },
    })?;
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
///
/// # Errors
///
/// Returns [`ConversionError::ComponentUnavailable`] when the trusted root is
/// unsafe or the current platform has no pinned runtime artifact.
pub fn expected_ocr_runtime_library(trusted_root: &Path) -> Result<PathBuf, ConversionError> {
    into_markdown_onnxruntime::RuntimeLibrary::expected_path(trusted_root).map_err(|error| {
        ConversionError::ComponentUnavailable {
            component: "onnxruntime".into(),
            detail: format!("ONNX Runtime distribution path is unavailable: {error}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CancellationToken, ErrorCode, ExecutionOptions, OcrPolicy, ResourceLimits};
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct TemporaryRoot(PathBuf);

    impl TemporaryRoot {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir()
                .join(format!("into-markdown-api-ocr-{label}-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TemporaryRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config(root: &Path) -> InstalledOcrConfig {
        InstalledOcrConfig {
            writable_model_root: root.join("models"),
            bundled_model_root: None,
            runtime_trusted_root: root.join("runtime"),
            runtime_library: root.join("runtime/missing-library"),
            worker_executable: root.join("missing-worker"),
            model_bundle: "pp-ocrv6-tiny-zh-en".into(),
        }
    }

    fn options() -> ConversionOptions {
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        options
    }

    fn failure(
        config: &InstalledOcrConfig,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> ConversionError {
        match installed_ocr_service(config, options, context) {
            Ok(_) => panic!("OCR service unexpectedly assembled"),
            Err(error) => error,
        }
    }

    #[test]
    fn missing_and_corrupt_components_fail_before_native_runtime_with_install_hint() {
        let root = TemporaryRoot::new("missing-corrupt");
        let options = options();
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let missing = failure(&config(&root.0), &options, &context);
        assert_eq!(missing.code(), ErrorCode::ComponentUnavailable);
        assert!(missing.to_string().contains("models install pp-ocrv6-tiny-zh-en"));
        assert!(!missing.to_string().contains("missing-library"));
        assert_eq!(context.reserved_memory_bytes(), 0);

        fs::create_dir_all(root.0.join("models/pp-ocrv6-tiny-detector-onnx")).unwrap();
        let corrupt = failure(&config(&root.0), &options, &context);
        assert_eq!(corrupt.code(), ErrorCode::ComponentUnavailable);
        assert!(corrupt.to_string().contains("models install pp-ocrv6-tiny-zh-en"));
        assert!(!corrupt.to_string().contains("missing-library"));
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn service_preflight_preserves_precancel_and_tiny_deadline_without_a_lease() {
        let root = TemporaryRoot::new("execution");
        let options = options();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let error = failure(&config(&root.0), &options, &cancelled);
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(cancelled.reserved_memory_bytes(), 0);

        let timed_out = ExecutionContext::new(
            ExecutionOptions { timeout: Some(Duration::ZERO), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let error = failure(&config(&root.0), &options, &timed_out);
        assert_eq!(error.code(), ErrorCode::Timeout);
        assert_eq!(timed_out.reserved_memory_bytes(), 0);
    }
}
