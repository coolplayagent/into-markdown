//! Per-invocation optional-service assembly with no implicit discovery or download.

use crate::config::{CapabilityRouteConfig, LoadedConfig};
use crate::error::CliError;
use into_markdown::{
    AiCapability, AiMode, BoxFuture, CompositeAiProvider, ConversionError, ConversionOptions,
    ExecutionContext, ExecutionOptions, OcrEngine, OcrOutputPlan, OcrPolicy, OcrRecognition,
    OcrRequest, OcrResult, OpenAiCompatibleClient, OpenAiDocumentPatchProvider,
    OpenAiImageDescriptionProvider, OpenAiRemoteOcr, OpenAiRemoteTranscriber,
    ProviderConfig as TransportProviderConfig, ProviderNetworkPolicy, Services, Transcriber,
    TranscriptionRequest, TranscriptionResult,
};
use into_markdown_provider_plugin::{
    CapabilityId, CapabilityRouteMode, CapabilitySourceRef, UnifiedCapabilityRoute,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

struct RoutedOcrEngine {
    id: String,
    mode: CapabilityRouteMode,
    sources: Vec<Arc<dyn OcrEngine>>,
}

impl OcrEngine for RoutedOcrEngine {
    fn id(&self) -> &str {
        &self.id
    }

    fn planned_bound_output(
        &self,
        request: OcrRequest<'_>,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        let mut plans = Vec::new();
        let mut last_error = None;
        for source in &self.sources {
            match source.planned_bound_output(request, options, context) {
                Ok(plan) => plans.push(plan),
                Err(error) if can_route_fallback(self.mode, &error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        if plans.is_empty() {
            return Err(last_error.unwrap_or_else(|| ConversionError::ComponentUnavailable {
                component: self.id.clone(),
                detail: "no OCR capability source is ready".into(),
            }));
        }
        aggregate_ocr_plans(&plans)
    }

    fn planned_normalized_png_output(
        &self,
        width: u32,
        height: u32,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<OcrOutputPlan, ConversionError> {
        let mut plans = Vec::new();
        let mut last_error = None;
        for source in &self.sources {
            match source.planned_normalized_png_output(width, height, options, context) {
                Ok(plan) => plans.push(plan),
                Err(error) if can_route_fallback(self.mode, &error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        if plans.is_empty() {
            return Err(last_error.unwrap_or_else(|| ConversionError::ComponentUnavailable {
                component: self.id.clone(),
                detail: "no OCR capability source can accept a normalized PNG".into(),
            }));
        }
        aggregate_ocr_plans(&plans)
    }

    fn recognize<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async move {
            let mut last_error = None;
            for source in &self.sources {
                match source.recognize(request, context).await {
                    Ok(result) => return Ok(result),
                    Err(error) if can_route_fallback(self.mode, &error) => {
                        last_error = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last_error.unwrap_or_else(|| ConversionError::ComponentUnavailable {
                component: self.id.clone(),
                detail: "no OCR capability source is ready".into(),
            }))
        })
    }

    fn recognize_bound<'a>(
        &'a self,
        request: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
        Box::pin(async move {
            let mut last_error = None;
            for source in &self.sources {
                match source.recognize_bound(request, context).await {
                    Ok(OcrRecognition::Remote(result)) if result.provider != source.id() => {
                        return Err(ConversionError::Ocr {
                            provider: source.id().into(),
                            detail:
                                "remote OCR result does not match the selected capability source"
                                    .into(),
                        });
                    }
                    Ok(result) => return Ok(result),
                    Err(error) if can_route_fallback(self.mode, &error) => {
                        last_error = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last_error.unwrap_or_else(|| ConversionError::ComponentUnavailable {
                component: self.id.clone(),
                detail: "no OCR capability source is ready".into(),
            }))
        })
    }
}

fn aggregate_ocr_plans(plans: &[OcrOutputPlan]) -> Result<OcrOutputPlan, ConversionError> {
    let working = plans.iter().map(|plan| plan.max_working_bytes()).max().unwrap_or(0);
    let regions = plans.iter().map(|plan| plan.max_regions()).max().unwrap_or(0);
    let text = plans.iter().map(|plan| plan.max_text_bytes()).max().unwrap_or(0);
    let structural = u64::from(regions)
        .checked_mul(256)
        .and_then(|bytes| bytes.checked_add(text))
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "OCR route output plan overflow".into(),
        })?;
    let retained =
        plans.iter().map(|plan| plan.max_retained_bytes()).max().unwrap_or(0).max(structural);
    OcrOutputPlan::try_new_with_working(retained, working, regions, text)
}

struct RoutedTranscriber {
    id: String,
    mode: CapabilityRouteMode,
    sources: Vec<Arc<dyn Transcriber>>,
}

impl Transcriber for RoutedTranscriber {
    fn id(&self) -> &str {
        &self.id
    }

    fn transcribe<'a>(
        &'a self,
        request: TranscriptionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
        Box::pin(async move {
            let mut last_error = None;
            for source in &self.sources {
                match source.transcribe(request, context).await {
                    Ok(result) if result.provider != source.id() => {
                        return Err(ConversionError::Ai {
                            provider: source.id().into(),
                            detail:
                                "transcription result does not match the selected capability source"
                                    .into(),
                        });
                    }
                    Ok(result) => return Ok(result),
                    Err(error) if can_route_fallback(self.mode, &error) => {
                        last_error = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last_error.unwrap_or_else(|| ConversionError::ComponentUnavailable {
                component: self.id.clone(),
                detail: "no transcription capability source is ready".into(),
            }))
        })
    }
}

fn can_route_fallback(mode: CapabilityRouteMode, error: &ConversionError) -> bool {
    match mode {
        CapabilityRouteMode::Off | CapabilityRouteMode::Only => false,
        CapabilityRouteMode::Fallback => {
            matches!(error, ConversionError::ComponentUnavailable { .. })
        }
        CapabilityRouteMode::Prefer => matches!(
            error,
            ConversionError::ComponentUnavailable { .. }
                | ConversionError::Network { .. }
                | ConversionError::Timeout
                | ConversionError::Ai { .. }
                | ConversionError::Ocr { .. }
        ),
    }
}

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

/// Verify construction of the effective OCR route without downloading models
/// or issuing a Provider request.
pub(crate) fn verify_ocr_runtime(loaded: &LoadedConfig, cwd: &Path) -> Result<(), ConversionError> {
    let context = ExecutionContext::new(ExecutionOptions::default(), loaded.options.limits.clone());
    assemble_ocr(loaded, &context, cwd).map(drop)
}

/// Verify construction of the effective transcription route without issuing a
/// Provider request.
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
    if loaded.options.ocr.policy != OcrPolicy::Off
        || loaded.options.ai.vision_ocr != AiMode::Off
        || configured_route_is_active(&loaded.effective.capability_routes.ocr)
    {
        match assemble_ocr(loaded, &context, cwd) {
            Ok(engine) => services.ocr = Some(engine),
            Err(error) if can_degrade_ocr(loaded.options.ocr.policy, &error) => {}
            Err(error) => return Err(CliError::from(error)),
        }
    }
    if ai_provider_service_enabled(&loaded.options) {
        services.ai = assemble_ai_provider(loaded)?;
    }
    if loaded.options.ai.audio_transcription != AiMode::Off
        || configured_route_is_active(&loaded.effective.capability_routes.transcription)
    {
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

fn configured_route_is_active(route: &CapabilityRouteConfig) -> bool {
    route.mode.is_some_and(|mode| mode != AiMode::Off)
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
        }
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
    let route = unified_route(
        CapabilityId::Transcription,
        &loaded.effective.capability_routes.transcription,
        "plugin:official.media.whisper/transcription",
        options.ai.audio_transcription,
    )
    .map_err(CliError::from)?;
    let eligible = route.eligible_sources().map_err(|error| {
        CliError::component(format!("invalid transcription capability route: {error}"))
    })?;
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    for source in eligible {
        let built = match source {
            CapabilitySourceRef::Plugin { plugin_id, capability_id } => {
                let configured = single_plugin_route(plugin_id, capability_id);
                resolve_process_capability(
                    loaded,
                    cwd,
                    into_markdown_provider_plugin::CapabilityKind::Transcription,
                    &configured,
                    &format!("{plugin_id}/{capability_id}"),
                    Some(options.asr.model_bundle.clone()),
                    into_markdown_provider_plugin::ResolutionMode::RequiredPrimary,
                    context,
                )
                .and_then(|capability| capability.transcriber(options.clone()))
                .map(|provider| Arc::new(provider) as Arc<dyn Transcriber>)
            }
            CapabilitySourceRef::Provider { provider_id, capability_id } => {
                build_remote_transcriber(loaded, provider_id, capability_id)
            }
            CapabilitySourceRef::Off => continue,
        };
        match built {
            Ok(source) => sources.push(source),
            Err(error) if route.mode != CapabilityRouteMode::Only => errors.push(error.to_string()),
            Err(error) => return Err(CliError::from(error)),
        }
    }
    if sources.is_empty() {
        return Err(CliError::component(if errors.is_empty() {
            "transcription capability is disabled".into()
        } else {
            errors.join("; ")
        }));
    }
    if sources.len() == 1 {
        return Ok(sources.remove(0));
    }
    Ok(Arc::new(RoutedTranscriber { id: "route.transcription".into(), mode: route.mode, sources }))
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
    let default_mode = if loaded.options.ai.vision_ocr == AiMode::Off {
        AiMode::Only
    } else {
        loaded.options.ai.vision_ocr
    };
    let route = unified_route(
        CapabilityId::Ocr,
        &loaded.effective.capability_routes.ocr,
        "plugin:official.ocr.ppocrv6/ocr",
        default_mode,
    )?;
    let eligible =
        route.eligible_sources().map_err(|error| ConversionError::ComponentUnavailable {
            component: "capability-routing".into(),
            detail: error.to_string(),
        })?;
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    for source in eligible {
        let built = match source {
            CapabilitySourceRef::Plugin { plugin_id, capability_id } => {
                let configured = single_plugin_route(plugin_id, capability_id);
                resolve_process_capability(
                    loaded,
                    cwd,
                    into_markdown_provider_plugin::CapabilityKind::Ocr,
                    &configured,
                    &format!("{plugin_id}/{capability_id}"),
                    Some(model_bundle.clone()),
                    into_markdown_provider_plugin::ResolutionMode::RequiredPrimary,
                    context,
                )
                .and_then(|capability| capability.ocr(loaded.options.clone()))
                .map(|provider| Arc::new(provider) as Arc<dyn OcrEngine>)
            }
            CapabilitySourceRef::Provider { provider_id, capability_id } => {
                build_remote_ocr(loaded, provider_id, capability_id)
            }
            CapabilitySourceRef::Off => continue,
        };
        match built {
            Ok(source) => sources.push(source),
            Err(error) if route.mode != CapabilityRouteMode::Only => errors.push(error.to_string()),
            Err(error) => return Err(error),
        }
    }
    if sources.is_empty() {
        return Err(ConversionError::ComponentUnavailable {
            component: "ocr".into(),
            detail: if errors.is_empty() {
                "OCR capability is disabled".into()
            } else {
                errors.join("; ")
            },
        });
    }
    if sources.len() == 1 {
        return Ok(sources.remove(0));
    }
    Ok(Arc::new(RoutedOcrEngine { id: "route.ocr".into(), mode: route.mode, sources }))
}

fn unified_route(
    capability: CapabilityId,
    configured: &CapabilityRouteConfig,
    default_primary: &str,
    default_mode: AiMode,
) -> Result<UnifiedCapabilityRoute, ConversionError> {
    let mode =
        configured.mode.map_or_else(|| capability_route_mode(default_mode), capability_route_mode);
    let primary = configured
        .primary
        .as_deref()
        .unwrap_or(if mode == CapabilityRouteMode::Off { "off" } else { default_primary })
        .parse::<CapabilitySourceRef>()
        .map_err(|error| ConversionError::ComponentUnavailable {
            component: "capability-routing".into(),
            detail: error.to_string(),
        })?;
    let fallbacks = configured
        .fallbacks
        .iter()
        .map(|source| source.parse::<CapabilitySourceRef>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ConversionError::ComponentUnavailable {
            component: "capability-routing".into(),
            detail: error.to_string(),
        })?;
    let route = UnifiedCapabilityRoute { capability, mode, primary, fallbacks };
    route.validate().map_err(|error| ConversionError::ComponentUnavailable {
        component: "capability-routing".into(),
        detail: error.to_string(),
    })?;
    Ok(route)
}

const fn capability_route_mode(mode: AiMode) -> CapabilityRouteMode {
    match mode {
        AiMode::Off => CapabilityRouteMode::Off,
        AiMode::Fallback => CapabilityRouteMode::Fallback,
        AiMode::Prefer => CapabilityRouteMode::Prefer,
        AiMode::Only => CapabilityRouteMode::Only,
    }
}

fn single_plugin_route(plugin_id: &str, capability_id: &str) -> CapabilityRouteConfig {
    CapabilityRouteConfig {
        mode: Some(AiMode::Only),
        primary: Some(format!("{plugin_id}/{capability_id}")),
        fallbacks: Vec::new(),
    }
}

fn build_remote_ocr(
    loaded: &LoadedConfig,
    provider_name: &str,
    capability_id: &str,
) -> Result<Arc<dyn OcrEngine>, ConversionError> {
    if capability_id != "vision-ocr" && capability_id != "ocr" {
        return Err(remote_route_error(provider_name, capability_id));
    }
    let (client, network, _) = remote_client(loaded, provider_name, "vision-ocr")?;
    OpenAiRemoteOcr::new(client, network, format!("provider.{provider_name}.vision-ocr"))
        .map(|provider| Arc::new(provider) as Arc<dyn OcrEngine>)
}

fn build_remote_transcriber(
    loaded: &LoadedConfig,
    provider_name: &str,
    capability_id: &str,
) -> Result<Arc<dyn Transcriber>, ConversionError> {
    if capability_id != "audio-transcription" && capability_id != "transcription" {
        return Err(remote_route_error(provider_name, capability_id));
    }
    let (client, _, model) = remote_client(loaded, provider_name, "audio-transcription")?;
    OpenAiRemoteTranscriber::new(
        client,
        format!("provider.{provider_name}.audio-transcription"),
        model,
    )
    .map(|provider| Arc::new(provider) as Arc<dyn Transcriber>)
}

fn remote_client(
    loaded: &LoadedConfig,
    provider_name: &str,
    capability: &str,
) -> Result<(OpenAiCompatibleClient, ProviderNetworkPolicy, String), ConversionError> {
    let configured = loaded.effective.providers.get(provider_name).ok_or_else(|| {
        ConversionError::ComponentUnavailable {
            component: format!("provider.{provider_name}"),
            detail: "configured provider does not exist".into(),
        }
    })?;
    if !configured.capabilities.iter().any(|value| value == capability) {
        return Err(ConversionError::ComponentUnavailable {
            component: format!("provider.{provider_name}"),
            detail: format!("provider does not declare {capability}"),
        });
    }
    let model = configured.model.clone();
    let timeout = std::time::Duration::from_millis(
        configured.timeout_ms.or(loaded.timeout_ms).unwrap_or(30_000),
    );
    let config = TransportProviderConfig::parse(
        &configured.base_url,
        &model,
        &configured.api_key_env,
        timeout,
        configured.capabilities.clone(),
    )
    .map_err(|error| ConversionError::ComponentUnavailable {
        component: format!("provider.{provider_name}"),
        detail: error.code_str().into(),
    })?;
    let network = ProviderNetworkPolicy {
        allow_network: loaded.options.network.enabled,
        allow_private_network: !loaded.options.network.deny_private_networks,
        allowed_hosts: loaded.options.network.allowed_hosts.clone(),
    };
    Ok((OpenAiCompatibleClient::new(config, network.clone()), network, model))
}

fn remote_route_error(provider: &str, capability: &str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: format!("provider.{provider}"),
        detail: format!("provider route uses incompatible capability {capability}"),
    }
}

fn ai_provider_service_enabled(options: &ConversionOptions) -> bool {
    [
        options.ai.image_description,
        options.ai.layout_repair,
        options.ai.table_repair,
        options.ai.formula_repair,
        options.ai.markdown_postprocess,
    ]
    .into_iter()
    .any(|mode| mode != AiMode::Off)
}

fn assemble_ai_provider(
    loaded: &LoadedConfig,
) -> Result<Option<Arc<dyn into_markdown::AiProvider>>, CliError> {
    let Some(name) = loaded.ai_provider.as_deref() else {
        return Ok(None);
    };
    let Some(configured) = loaded.effective.providers.get(name) else {
        return Ok(None);
    };
    let model = loaded.ai_model.as_deref().unwrap_or(&configured.model);
    let timeout = std::time::Duration::from_millis(
        configured.timeout_ms.or(loaded.timeout_ms).unwrap_or(30_000),
    );
    let network = ProviderNetworkPolicy {
        allow_network: loaded.options.network.enabled,
        allow_private_network: !loaded.options.network.deny_private_networks,
        allowed_hosts: loaded.options.network.allowed_hosts.clone(),
    };
    let provider_id = format!("provider.{name}");
    let client = || {
        TransportProviderConfig::parse(
            &configured.base_url,
            model,
            &configured.api_key_env,
            timeout,
            configured.capabilities.clone(),
        )
        .map(|config| OpenAiCompatibleClient::new(config, network.clone()))
        .map_err(CliError::from)
    };
    let mut adapters = Vec::<Arc<dyn into_markdown::AiProvider>>::new();
    if loaded.options.ai.image_description != AiMode::Off
        && configured.capabilities.iter().any(|value| value == "image-description")
    {
        adapters.push(Arc::new(OpenAiImageDescriptionProvider::new_with_id(
            client()?,
            network.clone(),
            provider_id.clone(),
        )?));
    }
    let patch_capabilities = [
        (AiCapability::LayoutRepair, "layout-repair", loaded.options.ai.layout_repair),
        (AiCapability::TableRepair, "table-repair", loaded.options.ai.table_repair),
        (AiCapability::FormulaRepair, "formula-repair", loaded.options.ai.formula_repair),
        (
            AiCapability::MarkdownPostprocess,
            "markdown-postprocess",
            loaded.options.ai.markdown_postprocess,
        ),
    ]
    .into_iter()
    .filter(|(_, capability, mode)| {
        *mode != AiMode::Off
            && configured.capabilities.iter().any(|declared| declared == capability)
    })
    .map(|(capability, _, _)| capability)
    .collect::<Vec<_>>();
    if !patch_capabilities.is_empty() {
        adapters.push(Arc::new(OpenAiDocumentPatchProvider::new(
            client()?,
            network,
            provider_id.clone(),
            patch_capabilities,
        )?));
    }
    match adapters.len() {
        0 => Ok(None),
        1 => Ok(adapters.pop()),
        _ => Ok(Some(Arc::new(CompositeAiProvider::new(provider_id, adapters)?))),
    }
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
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn loaded_config(name: &str, contents: &str) -> (PathBuf, LoadedConfig) {
        let thread = std::thread::current()
            .name()
            .unwrap_or("test")
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() { character } else { '-' })
            .collect::<String>();
        let root = std::env::temp_dir().join(format!(
            "into-md-services-{name}-{}-{}",
            std::process::id(),
            thread
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        let loaded = crate::config::load(&root, &[path], true, None, None).unwrap();
        (root, loaded)
    }

    struct FixtureOcr {
        id: &'static str,
        outcome: Result<OcrResult, ConversionError>,
        calls: AtomicUsize,
    }

    impl OcrEngine for FixtureOcr {
        fn id(&self) -> &str {
            self.id
        }

        fn planned_bound_output(
            &self,
            _request: OcrRequest<'_>,
            _options: &ConversionOptions,
            _context: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            OcrOutputPlan::try_new_with_working(1024, 1024, 1, 256)
        }

        fn planned_normalized_png_output(
            &self,
            _width: u32,
            _height: u32,
            _options: &ConversionOptions,
            _context: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            OcrOutputPlan::try_new_with_working(1024, 1024, 1, 256)
        }

        fn recognize<'a>(
            &'a self,
            _request: OcrRequest<'a>,
            _context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self.outcome.clone();
            Box::pin(async move { outcome })
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(value) => return value,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

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
    fn runtime_route_modes_have_fail_closed_and_recovery_contracts() {
        let unavailable = ConversionError::ComponentUnavailable {
            component: "fixture".into(),
            detail: "missing".into(),
        };
        let network = ConversionError::Network { detail: "offline".into() };
        let cancelled = ConversionError::Cancelled;
        assert!(!can_route_fallback(CapabilityRouteMode::Off, &unavailable));
        assert!(!can_route_fallback(CapabilityRouteMode::Only, &unavailable));
        assert!(can_route_fallback(CapabilityRouteMode::Fallback, &unavailable));
        assert!(!can_route_fallback(CapabilityRouteMode::Fallback, &network));
        assert!(can_route_fallback(CapabilityRouteMode::Prefer, &unavailable));
        assert!(can_route_fallback(CapabilityRouteMode::Prefer, &network));
        assert!(!can_route_fallback(CapabilityRouteMode::Prefer, &cancelled));
        assert!(!can_route_fallback(
            CapabilityRouteMode::Prefer,
            &ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: "fixture".into() },
        ));
    }

    #[test]
    fn routed_ocr_uses_ordered_fallback_without_swallowing_disallowed_failures() {
        let successful = || {
            Arc::new(FixtureOcr {
                id: "fallback",
                outcome: Ok(OcrResult { regions: Vec::new(), provider: "fallback".into() }),
                calls: AtomicUsize::new(0),
            })
        };
        let request = OcrRequest { image: b"fixture", media_type: "image/png", languages: &[] };
        let context = ExecutionContext::new(
            ExecutionOptions::default(),
            into_markdown::ResourceLimits::default(),
        );

        let primary = Arc::new(FixtureOcr {
            id: "primary",
            outcome: Err(ConversionError::Network { detail: "offline".into() }),
            calls: AtomicUsize::new(0),
        });
        let fallback = successful();
        let route = RoutedOcrEngine {
            id: "route".into(),
            mode: CapabilityRouteMode::Prefer,
            sources: vec![primary.clone(), fallback.clone()],
        };
        assert!(block_on(route.recognize_bound(request, &context)).is_ok());
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback.calls.load(Ordering::SeqCst), 1);

        let primary = Arc::new(FixtureOcr {
            id: "primary",
            outcome: Err(ConversionError::Network { detail: "offline".into() }),
            calls: AtomicUsize::new(0),
        });
        let fallback = successful();
        let route = RoutedOcrEngine {
            id: "route".into(),
            mode: CapabilityRouteMode::Fallback,
            sources: vec![primary, fallback.clone()],
        };
        assert!(matches!(
            block_on(route.recognize_bound(request, &context)),
            Err(ConversionError::Network { .. })
        ));
        assert_eq!(fallback.calls.load(Ordering::SeqCst), 0);
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

    #[test]
    fn configured_provider_composes_image_and_structured_patch_adapters() {
        let (root, loaded) = loaded_config(
            "composite-provider",
            r#"
schema_version = 1
default_provider = "bailian"

[conversion.ai]
image_description = "prefer"
table_repair = "only"

[providers.bailian]
type = "openai-compatible"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
model = "fixture-model"
api_key_env = "DASHSCOPE_API_KEY"
capabilities = ["image-description", "table-repair"]
"#,
        );
        let provider = assemble_ai_provider(&loaded).unwrap().unwrap();
        assert_eq!(provider.id(), "provider.bailian");
        assert_eq!(
            provider.capabilities(),
            std::collections::BTreeSet::from([
                AiCapability::ImageDescription,
                AiCapability::TableRepair,
            ])
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_remote_transcription_route_activates_with_legacy_ai_mode_off() {
        let (root, loaded) = loaded_config(
            "remote-transcription-route",
            r#"
schema_version = 1

[conversion.ocr]
policy = "off"

[providers.bailian]
type = "openai-compatible"
base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
model = "qwen3-asr-flash"
api_key_env = "DASHSCOPE_API_KEY"
capabilities = ["audio-transcription"]

[capability_routes.transcription]
mode = "only"
primary = "provider:bailian/audio-transcription"
fallbacks = []
"#,
        );
        let services = assemble(&loaded, &ExecutionOptions::default(), &root).unwrap();
        assert_eq!(
            services.transcriber.as_deref().map(Transcriber::id),
            Some("provider.bailian.audio-transcription")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
