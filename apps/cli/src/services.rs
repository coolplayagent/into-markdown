//! Per-invocation optional-service assembly with no implicit discovery or download.

use crate::config::LoadedConfig;
use crate::error::CliError;
use into_markdown::{
    AiMode, ConversionError, ExecutionContext, ExecutionOptions, InstalledOcrConfig, OcrPolicy,
    OpenAiCompatibleClient, OpenAiImageDescriptionProvider,
    ProviderConfig as TransportProviderConfig, ProviderNetworkPolicy, Services,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn assemble(
    loaded: &LoadedConfig,
    execution: &ExecutionOptions,
) -> Result<Services, CliError> {
    let executable = canonical_executable().map_err(CliError::component)?;
    assemble_at(loaded, execution, &executable)
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
    Ok(services)
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
