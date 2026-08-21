//! Per-invocation optional-service assembly with no implicit discovery or download.

use crate::config::{CapabilityRouteConfig, LoadedConfig};
use crate::error::CliError;
use into_markdown::{
    AiMode, ConversionError, ConversionOptions, ExecutionContext, ExecutionOptions, OcrPolicy,
    OpenAiCompatibleClient, OpenAiImageDescriptionProvider,
    ProviderConfig as TransportProviderConfig, ProviderNetworkPolicy, Services,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

pub(crate) fn assemble(
    loaded: &LoadedConfig,
    execution: &ExecutionOptions,
    cwd: &Path,
) -> Result<Services, CliError> {
    assemble_at(loaded, execution, cwd)
}

/// Assemble the exact local media services required by one durable Web meeting
/// request. The helper verifies installed components and never downloads them.
#[derive(Clone, PartialEq, Eq)]
struct WebMediaKey {
    model_revision: u128,
    asr: into_markdown::AsrOptions,
    diarization_bundle: Option<String>,
}

struct SingleEntryCache<K, V> {
    entry: Mutex<Option<(K, V)>>,
}

impl<K: PartialEq, V: Clone> SingleEntryCache<K, V> {
    fn get_or_try_insert_with<E>(
        &self,
        key: K,
        build: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E> {
        let mut entry = self.entry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_key, value)) = entry.as_ref()
            && *cached_key == key
        {
            return Ok(value.clone());
        }
        let value = build()?;
        *entry = Some((key, value.clone()));
        Ok(value)
    }

    fn clear(&self) {
        *self.entry.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl<K, V> Default for SingleEntryCache<K, V> {
    fn default() -> Self {
        Self { entry: Mutex::new(None) }
    }
}

/// Process-local, bounded cache for the native services used by Web meetings.
/// A configuration or installed-model revision change atomically replaces the
/// previous entry; construction failures are never cached.
#[derive(Default)]
pub(crate) struct WebMediaServiceCache {
    services: SingleEntryCache<WebMediaKey, Services>,
    loaded: Mutex<Option<LoadedConfig>>,
    cwd: Mutex<Option<PathBuf>>,
}

impl WebMediaServiceCache {
    pub(crate) fn assemble(&self, options: &ConversionOptions) -> Result<Services, CliError> {
        let key = WebMediaKey {
            model_revision: media_model_revision(),
            asr: options.asr.clone(),
            diarization_bundle: options
                .diarization
                .enabled
                .then(|| options.diarization.model_bundle.clone()),
        };
        self.services.get_or_try_insert_with(key, || {
            let loaded = self
                .loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .ok_or_else(|| {
                    CliError::component("Web media capability routing is unavailable")
                })?;
            let cwd = self
                .cwd
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .ok_or_else(|| CliError::component("Web capability scope is unavailable"))?;
            // Cached native services outlive any one request, so their verified
            // model leases must not retain a request cancellation/progress sink.
            let context = ExecutionContext::new(
                ExecutionOptions::default(),
                into_markdown::ResourceLimits::default(),
            );
            let mut services = Services {
                transcriber: Some(assemble_asr_options(&loaded, options, &context, &cwd)?),
                ..Services::default()
            };
            if options.diarization.enabled {
                services.diarizer = Some(
                    assemble_diarization_config(&loaded, options, &context, &cwd)
                        .map_err(CliError::from)?,
                );
            }
            Ok(services)
        })
    }

    pub(crate) fn with_config(loaded: LoadedConfig, cwd: PathBuf) -> Self {
        Self {
            services: SingleEntryCache::default(),
            loaded: Mutex::new(Some(loaded)),
            cwd: Mutex::new(Some(cwd)),
        }
    }

    pub(crate) fn update_config(&self, loaded: LoadedConfig) {
        *self.loaded.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(loaded);
        self.services.clear();
    }
}

/// Verify the exact local OCR distribution used by conversion without any
/// download or network access.
pub(crate) fn verify_ocr_runtime(loaded: &LoadedConfig, cwd: &Path) -> Result<(), ConversionError> {
    let context = ExecutionContext::new(ExecutionOptions::default(), loaded.options.limits.clone());
    assemble_ocr(loaded, &context, cwd).map(drop)
}

/// Verify the exact offline ASR distribution used by the Web workbench.
pub(crate) fn verify_asr_runtime(loaded: &LoadedConfig, cwd: &Path) -> Result<(), ConversionError> {
    let context = ExecutionContext::new(
        ExecutionOptions::default(),
        into_markdown::ResourceLimits::default(),
    );
    assemble_asr_options(loaded, &loaded.options, &context, cwd).map(drop).map_err(|error| {
        ConversionError::ComponentUnavailable {
            component: "transcription-plugin".into(),
            detail: error.to_string(),
        }
    })
}

/// Verify the exact offline diarization distribution used by the meeting page.
pub(crate) fn verify_diarization_runtime(
    loaded: &LoadedConfig,
    cwd: &Path,
) -> Result<(), ConversionError> {
    let mut options = ConversionOptions::default();
    options.diarization.enabled = true;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assemble_diarization_config(loaded, &options, &context, cwd).map(drop)
}

/// Revision of model, plugin, and routing authority used to invalidate a
/// cached unavailable status after an explicit capability installation.
pub(crate) fn media_model_revision() -> u128 {
    let mut revision = 1_u128;
    let paths = [
        writable_model_root().ok(),
        directories::ProjectDirs::from("", "", "into-markdown")
            .map(|directories| directories.data_dir().join("plugins")),
        crate::config::global_config_path().ok(),
    ];
    for path in paths.into_iter().flatten() {
        let Ok(metadata) = std::fs::metadata(path) else { continue };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |value| value.as_nanos());
        revision = revision.rotate_left(17) ^ modified ^ u128::from(metadata.len());
    }
    revision
}

fn assemble_at(
    loaded: &LoadedConfig,
    execution: &ExecutionOptions,
    cwd: &Path,
) -> Result<Services, CliError> {
    let mut services = Services::default();
    let context = ExecutionContext::new(execution.clone(), loaded.options.limits.clone());
    if loaded.options.ocr.policy != OcrPolicy::Off {
        match assemble_ocr(loaded, &context, cwd) {
            Ok(engine) => services.ocr = Some(engine),
            Err(error) if can_degrade_ocr(loaded.options.ocr.policy, &error) => {}
            Err(error) => return Err(CliError::from(error)),
        }
    }
    if loaded.options.ai.image_description != AiMode::Off {
        services.ai = assemble_image_description(loaded)?;
    }
    if loaded.options.ai.audio_transcription != AiMode::Off {
        services.transcriber = Some(assemble_asr(loaded, &context, cwd)?);
    }
    if loaded.options.diarization.enabled {
        services.diarizer = Some(
            assemble_diarization_config(loaded, &loaded.options, &context, cwd)
                .map_err(CliError::from)?,
        );
    }
    Ok(services)
}

fn strict_media_mode(options: &ConversionOptions) -> into_markdown_provider_plugin::ResolutionMode {
    if options.ai.audio_transcription == AiMode::Only {
        into_markdown_provider_plugin::ResolutionMode::RequiredPrimary
    } else {
        into_markdown_provider_plugin::ResolutionMode::ReadinessFallback
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_process_capability(
    loaded: &LoadedConfig,
    cwd: &Path,
    kind: into_markdown_provider_plugin::CapabilityKind,
    configured: &CapabilityRouteConfig,
    default_primary: &str,
    model_bundle: Option<String>,
    mode: into_markdown_provider_plugin::ResolutionMode,
    context: &ExecutionContext,
) -> Result<into_markdown_provider_plugin::ProcessCapability, ConversionError> {
    use into_markdown_provider_plugin::{CapabilityRegistry, CapabilityRoute, ProcessCapability};

    let primary = parse_provider_reference(
        configured.primary.as_deref().unwrap_or(default_primary),
        model_bundle.as_deref(),
    )?;
    let fallbacks = configured
        .fallbacks
        .iter()
        .map(|reference| parse_provider_reference(reference, model_bundle.as_deref()))
        .collect::<Result<Vec<_>, _>>()?;
    let route = CapabilityRoute { primary, fallbacks };
    let mut registry = CapabilityRegistry::new();
    let mut packages = BTreeMap::new();
    let mut registration_errors = BTreeMap::new();
    let references = std::iter::once(&route.primary).chain(&route.fallbacks);
    for reference in references {
        if packages.contains_key(&reference.plugin_id) {
            continue;
        }
        let key = format!("{}/{}", reference.plugin_id, reference.capability_id);
        let Some(config) = loaded.effective.plugins.get(&reference.plugin_id) else {
            registration_errors.insert(key, "plugin is not configured".to_owned());
            continue;
        };
        if !config.enabled || config.protocol != "process-v1" {
            registration_errors
                .insert(key, "plugin is disabled or does not use process-v1".to_owned());
            continue;
        }
        let installed = match crate::app::verify_admin_effective_plugin_from_loaded(
            loaded,
            cwd,
            &reference.plugin_id,
        ) {
            Ok(installed) => installed,
            Err(error) => {
                registration_errors.insert(key, error.to_string());
                continue;
            }
        };
        let (manifest, descriptor_sha256) =
            match into_markdown_provider_plugin::load_installed_manifest(&installed) {
                Ok(authority) => authority,
                Err(error) => {
                    registration_errors.insert(key, error);
                    continue;
                }
            };
        if let Err(error) =
            registry.register(manifest.clone(), descriptor_sha256, installed.root.clone(), true)
        {
            registration_errors.insert(key, error.to_string());
            continue;
        };
        packages.insert(reference.plugin_id.clone(), (installed, manifest));
    }
    let roots = provider_model_roots()?;
    let mut readiness_errors = BTreeMap::new();
    let mut ready = BTreeMap::new();
    let binding = registry.resolve(kind, &route, mode, |binding| {
        let Some((_installed, manifest)) = packages.get(&binding.plugin_id) else {
            return false;
        };
        let result = ProcessCapability::runtime_policy(manifest, binding, roots.clone())
            .map_err(CliError::from)
            .and_then(|(policy, model_roots)| {
                crate::app::prepare_admin_effective_process_plugin_from_loaded(
                    loaded,
                    cwd,
                    &binding.plugin_id,
                    policy,
                    context,
                )
                .and_then(|process| {
                    ProcessCapability::new(process, manifest, binding.clone(), model_roots)
                        .map_err(CliError::from)
                })
            })
            .and_then(|capability| {
                capability.verify_ready(&loaded.options, context).map_err(CliError::from)?;
                ready
                    .insert(format!("{}/{}", binding.plugin_id, binding.capability_id), capability);
                Ok(())
            });
        match result {
            Ok(()) => true,
            Err(error) => {
                readiness_errors.insert(
                    format!("{}/{}", binding.plugin_id, binding.capability_id),
                    error.to_string(),
                );
                false
            }
        }
    });
    let binding = binding.map_err(|error| {
        let mut details = registration_errors.into_values().collect::<Vec<_>>();
        details.extend(readiness_errors.into_values());
        ConversionError::ComponentUnavailable {
            component: capability_name(kind).into(),
            detail: if details.is_empty() {
                format!("{error}; {}", capability_setup_hint(kind))
            } else {
                format!("{error}; {}", details.join("; "))
            },
        }
    })?;
    ready.remove(&format!("{}/{}", binding.plugin_id, binding.capability_id)).ok_or_else(|| {
        ConversionError::ComponentUnavailable {
            component: capability_name(kind).into(),
            detail: "resolved plugin capability disappeared".into(),
        }
    })
}

fn parse_provider_reference(
    value: &str,
    model_bundle: Option<&str>,
) -> Result<into_markdown_provider_plugin::ProviderReference, ConversionError> {
    let Some((plugin_id, capability_id)) = value.split_once('/') else {
        return Err(ConversionError::ComponentUnavailable {
            component: "capability-routing".into(),
            detail: format!("invalid provider reference '{value}'"),
        });
    };
    Ok(into_markdown_provider_plugin::ProviderReference {
        plugin_id: plugin_id.into(),
        capability_id: capability_id.into(),
        model_bundle: model_bundle.map(str::to_owned),
    })
}

fn provider_model_roots() -> Result<Vec<PathBuf>, ConversionError> {
    let mut roots = vec![writable_model_root()?];
    if let Ok(executable) = canonical_executable()
        && let Some(directory) = executable.parent()
        && let Some(bundled) = bundled_model_root(directory)
    {
        roots.push(bundled);
    }
    Ok(roots)
}

const fn capability_name(kind: into_markdown_provider_plugin::CapabilityKind) -> &'static str {
    match kind {
        into_markdown_provider_plugin::CapabilityKind::Ocr => "ocr-plugin",
        into_markdown_provider_plugin::CapabilityKind::Transcription => "transcription-plugin",
        into_markdown_provider_plugin::CapabilityKind::Diarization => "diarization-plugin",
    }
}

const fn capability_setup_hint(
    kind: into_markdown_provider_plugin::CapabilityKind,
) -> &'static str {
    match kind {
        into_markdown_provider_plugin::CapabilityKind::Ocr => "run `into-md setup ocr`",
        into_markdown_provider_plugin::CapabilityKind::Transcription
        | into_markdown_provider_plugin::CapabilityKind::Diarization => "run `into-md setup media`",
    }
}

fn assemble_diarization_config(
    loaded: &LoadedConfig,
    options: &ConversionOptions,
    context: &ExecutionContext,
    cwd: &Path,
) -> Result<Arc<dyn into_markdown::Diarizer>, ConversionError> {
    resolve_process_capability(
        loaded,
        cwd,
        into_markdown_provider_plugin::CapabilityKind::Diarization,
        &loaded.effective.capability_routes.diarization,
        "official.media.whisper/diarization",
        Some(options.diarization.model_bundle.clone()),
        strict_media_mode(options),
        context,
    )?
    .diarizer(options.clone())
    .map(|provider| Arc::new(provider) as Arc<dyn into_markdown::Diarizer>)
}

fn assemble_asr(
    loaded: &LoadedConfig,
    context: &ExecutionContext,
    cwd: &Path,
) -> Result<Arc<dyn into_markdown::Transcriber>, CliError> {
    assemble_asr_options(loaded, &loaded.options, context, cwd)
}

fn assemble_asr_options(
    loaded: &LoadedConfig,
    options: &ConversionOptions,
    context: &ExecutionContext,
    cwd: &Path,
) -> Result<Arc<dyn into_markdown::Transcriber>, CliError> {
    resolve_process_capability(
        loaded,
        cwd,
        into_markdown_provider_plugin::CapabilityKind::Transcription,
        &loaded.effective.capability_routes.transcription,
        "official.media.whisper/transcription",
        Some(options.asr.model_bundle.clone()),
        strict_media_mode(options),
        context,
    )
    .and_then(|capability| capability.transcriber(options.clone()))
    .map(|provider| Arc::new(provider) as Arc<dyn into_markdown::Transcriber>)
    .map_err(CliError::from)
}

fn can_degrade_ocr(policy: OcrPolicy, error: &into_markdown::ConversionError) -> bool {
    policy == OcrPolicy::Auto
        && matches!(error, into_markdown::ConversionError::ComponentUnavailable { .. })
}

fn assemble_ocr(
    loaded: &LoadedConfig,
    context: &ExecutionContext,
    cwd: &Path,
) -> Result<Arc<dyn into_markdown::OcrEngine>, into_markdown::ConversionError> {
    let model_bundle =
        loaded.options.ocr.model_bundle.clone().unwrap_or_else(|| "pp-ocrv6-tiny-zh-en".into());
    let mode = if loaded.options.ocr.policy == OcrPolicy::Always {
        into_markdown_provider_plugin::ResolutionMode::RequiredPrimary
    } else {
        into_markdown_provider_plugin::ResolutionMode::ReadinessFallback
    };
    resolve_process_capability(
        loaded,
        cwd,
        into_markdown_provider_plugin::CapabilityKind::Ocr,
        &loaded.effective.capability_routes.ocr,
        "official.ocr.ppocrv6/ocr",
        Some(model_bundle),
        mode,
        context,
    )?
    .ocr(loaded.options.clone())
    .map(|provider| Arc::new(provider) as Arc<dyn into_markdown::OcrEngine>)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[test]
    fn single_entry_cache_reuses_replaces_and_does_not_cache_errors() {
        let cache = SingleEntryCache::<u8, String>::default();
        let builds = AtomicUsize::new(0);
        let first = cache
            .get_or_try_insert_with(1, || {
                builds.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>("one".to_owned())
            })
            .unwrap();
        let reused = cache
            .get_or_try_insert_with(1, || {
                builds.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>("unexpected".to_owned())
            })
            .unwrap();
        assert_eq!(
            (first.as_str(), reused.as_str(), builds.load(Ordering::Relaxed)),
            ("one", "one", 1)
        );

        let failure = cache.get_or_try_insert_with(2, || Err::<String, _>("failed"));
        assert_eq!(failure.unwrap_err(), "failed");
        let replacement = cache
            .get_or_try_insert_with(2, || {
                builds.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>("two".to_owned())
            })
            .unwrap();
        assert_eq!(replacement, "two");
        assert_eq!(builds.load(Ordering::Relaxed), 2);
    }
}
