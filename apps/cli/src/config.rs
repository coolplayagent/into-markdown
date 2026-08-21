//! Layered, versioned TOML configuration and atomic mutation helpers.

use crate::args::{AssetModeArg, ConflictPolicy, EmitKind, Language, Scope};
use crate::error::CliError;
#[cfg(not(test))]
use directories::ProjectDirs;
use into_markdown::{
    AiMode, AssetMode, ChineseScript, ConversionOptions, OcrPolicy, RaggedRowsMode,
    TableHeaderMode, TextDecodingMode,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    static TEST_GLOBAL_CONFIG_PATH: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(crate) struct TestGlobalConfigGuard(Option<PathBuf>);

#[cfg(test)]
impl TestGlobalConfigGuard {
    pub(crate) fn set(path: Option<PathBuf>) -> Self {
        Self(TEST_GLOBAL_CONFIG_PATH.with(|slot| slot.replace(path)))
    }
}

#[cfg(test)]
impl Drop for TestGlobalConfigGuard {
    fn drop(&mut self) {
        TEST_GLOBAL_CONFIG_PATH.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

const CONFIG_FILENAME: &str = ".into-markdown.toml";
const MAX_PROVIDER_BASE_URL_BYTES: usize = 4 * 1024;
const MAX_PROVIDER_MODEL_BYTES: usize = 512;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 256;

/// Paths participating in configuration discovery.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPaths {
    pub global: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub explicit: Vec<PathBuf>,
    pub loaded: Vec<PathBuf>,
}

/// One configured OpenAI-compatible provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub timeout_ms: Option<u64>,
    pub capabilities: Vec<String>,
}

/// One configured process or WASI plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginConfig {
    pub source: String,
    pub sha256: Option<String>,
    pub protocol: String,
    pub enabled: bool,
    pub signing_key_id: String,
    pub signing_key_sha256: String,
}

/// Versioned top-level configuration document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawConfig {
    pub schema_version: Option<u32>,
    pub cli: CliConfig,
    pub conversion: ConversionConfig,
    pub default_provider: Option<String>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub plugins: BTreeMap<String, PluginConfig>,
    pub capability_routes: CapabilityRoutesConfig,
    pub profiles: BTreeMap<String, ProfileConfig>,
}

/// Human interface defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CliConfig {
    pub language: Option<String>,
    pub jobs: Option<usize>,
    pub color: Option<String>,
    pub progress: Option<String>,
    pub log_format: Option<String>,
}

/// Partial conversion policy stored in TOML.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConversionConfig {
    pub timeout_ms: Option<u64>,
    pub text: TextConfig,
    pub delimited_text: DelimitedTextConfig,
    pub ocr: OcrConfig,
    pub asr: AsrConfig,
    pub ai: AiConfig,
    pub network: NetworkConfig,
    pub limits: LimitsConfig,
    pub output: OutputConfig,
}

/// Partial plain-text decoding configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TextConfig {
    pub decoding_mode: Option<TextDecodingMode>,
}

/// Partial CSV and TSV policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DelimitedTextConfig {
    pub header: Option<TableHeaderMode>,
    pub ragged_rows: Option<RaggedRowsMode>,
}

/// Partial local OCR configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OcrConfig {
    pub policy: Option<OcrPolicy>,
    pub model_bundle: Option<String>,
    pub languages: Vec<String>,
    pub minimum_confidence: Option<f32>,
}

/// Partial local ASR configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    pub model_bundle: Option<String>,
    pub language: Option<String>,
    pub chinese_script: Option<ChineseScript>,
    pub max_threads: Option<u16>,
    pub max_duration_ms: Option<u64>,
    pub max_segments: Option<u32>,
    pub max_native_memory_bytes: Option<u64>,
}

/// Primary and ordered readiness fallbacks for isolated local capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityRoutesConfig {
    pub ocr: CapabilityRouteConfig,
    pub transcription: CapabilityRouteConfig,
    pub diarization: CapabilityRouteConfig,
}

/// One capability route using `plugin-id/capability-id` references.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityRouteConfig {
    pub primary: Option<String>,
    pub fallbacks: Vec<String>,
}

/// Partial AI routing configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    pub vision_ocr: Option<AiMode>,
    pub image_description: Option<AiMode>,
    pub layout_repair: Option<AiMode>,
    pub table_repair: Option<AiMode>,
    pub formula_repair: Option<AiMode>,
    pub audio_transcription: Option<AiMode>,
    pub markdown_postprocess: Option<AiMode>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub prompts: BTreeMap<String, PathBuf>,
}

/// Network restrictions that do not grant network permission.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub max_redirects: Option<u8>,
    pub allowed_hosts: Vec<String>,
    pub deny_private_networks: Option<bool>,
}

/// Partial resource budgets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
pub struct LimitsConfig {
    pub max_input_bytes: Option<u64>,
    pub max_decompressed_bytes: Option<u64>,
    pub max_archive_entries: Option<u32>,
    pub max_archive_depth: Option<u16>,
    pub max_archive_entry_bytes: Option<u64>,
    pub max_archive_compression_ratio: Option<u32>,
    pub max_nesting_depth: Option<u16>,
    pub max_pages: Option<u32>,
    pub max_asset_bytes: Option<u64>,
    pub max_total_asset_bytes: Option<u64>,
    pub max_memory_bytes: Option<u64>,
    pub max_temporary_bytes: Option<u64>,
    pub max_table_rows: Option<u64>,
    pub max_table_columns: Option<u64>,
    pub max_table_cells: Option<u64>,
    pub max_field_bytes: Option<u64>,
}

/// Partial artifact policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub emit: Option<String>,
    pub asset_mode: Option<AssetMode>,
    pub conflict: Option<String>,
    pub asset_directory_suffix: Option<String>,
    pub include_provenance: Option<bool>,
}

/// Profile overlay. Profiles cannot grant network access either.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    pub cli: CliConfig,
    pub conversion: ConversionConfig,
    pub default_provider: Option<String>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub plugins: BTreeMap<String, PluginConfig>,
    pub capability_routes: CapabilityRoutesConfig,
}

/// Fully loaded configuration plus source-path information.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub paths: ConfigPaths,
    pub merged: toml::Value,
    pub effective: RawConfig,
    pub options: ConversionOptions,
    pub language: Language,
    pub jobs: usize,
    pub emit: EmitKind,
    pub asset_mode: AssetModeArg,
    pub conflict: ConflictPolicy,
    pub ai_provider: Option<String>,
    pub ai_model: Option<String>,
    pub ocr_languages: Vec<String>,
    pub prompts: BTreeMap<String, PathBuf>,
    /// Dotted effective keys mapped to the layer that supplied their final value.
    pub sources: BTreeMap<String, String>,
    pub timeout_ms: Option<u64>,
}

impl LoadedConfig {
    /// Redacted TOML value suitable for display.
    pub fn display_value(&self, resolved: bool) -> Result<toml::Value, CliError> {
        let mut value = if resolved {
            toml::Value::try_from(&self.effective)
                .map_err(|error| CliError::internal(format!("serialize config: {error}")))?
        } else {
            self.merged.clone()
        };
        redact_value(&mut value, None);
        if resolved {
            let sources = toml::Value::try_from(&self.sources).map_err(|error| {
                CliError::internal(format!("serialize config sources: {error}"))
            })?;
            value
                .as_table_mut()
                .ok_or_else(|| CliError::internal("resolved configuration is not a table"))?
                .insert("_sources".into(), sources);
        }
        Ok(value)
    }
}

/// Load automatic, explicit, and profile layers.
pub fn load(
    cwd: &Path,
    explicit: &[PathBuf],
    no_automatic: bool,
    selected_profile: Option<&str>,
    language_override: Option<Language>,
) -> Result<LoadedConfig, CliError> {
    load_with_global(
        cwd,
        if no_automatic { None } else { Some(global_config_path()?) },
        explicit,
        no_automatic,
        selected_profile,
        language_override,
    )
}

fn load_with_global(
    cwd: &Path,
    global: Option<PathBuf>,
    explicit: &[PathBuf],
    no_automatic: bool,
    selected_profile: Option<&str>,
    language_override: Option<Language>,
) -> Result<LoadedConfig, CliError> {
    let project = find_project_config(cwd);
    let mut candidates = Vec::new();
    if !no_automatic {
        candidates.push((global.clone().expect("automatic global path"), false));
        if let Some(path) = &project {
            candidates.push((path.clone(), false));
        }
    }
    let explicit = explicit.iter().map(|path| resolve_path(cwd, path)).collect::<Vec<_>>();
    candidates.extend(explicit.iter().cloned().map(|path| (path, true)));

    let profile = selected_profile
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("INTO_MD_PROFILE").ok().filter(|value| !value.is_empty()));
    let mut merged = empty_table();
    let mut effective = empty_table();
    let mut loaded = Vec::new();
    let mut profile_found = false;
    let mut sources = BTreeMap::new();
    for (path, required) in candidates {
        if !path.exists() {
            if required {
                return Err(CliError::config(format!(
                    "configuration file does not exist: {}",
                    path.display()
                )));
            }
            continue;
        }
        let value = read_layer_value(&path)?;
        profile_found |= merge_layer(
            value,
            &path,
            profile.as_deref(),
            &mut merged,
            &mut effective,
            &mut sources,
        );
        loaded.push(path);
    }
    if let Some(name) = &profile
        && !profile_found
    {
        return Err(CliError::config(format!("configuration profile '{name}' was not found")));
    }

    let mut parsed: RawConfig = effective
        .clone()
        .try_into()
        .map_err(|error| CliError::config(format!("merged configuration is invalid: {error}")))?;
    parsed.profiles.clear();
    parsed.schema_version = Some(1);
    validate_raw(&parsed)?;
    if let Some(name) = &parsed.default_provider
        && !parsed.providers.contains_key(name)
    {
        return Err(CliError::config(format!(
            "default provider '{name}' is not configured after merging all layers"
        )));
    }
    let options = resolve_conversion_options(&parsed.conversion)?;
    let language = resolve_language(&mut parsed, language_override, &mut sources);
    let jobs = parsed.cli.jobs.unwrap_or_else(default_jobs);
    if jobs == 0 {
        return Err(CliError::config("cli.jobs must be greater than zero"));
    }
    let emit = parse_emit(parsed.conversion.output.emit.as_deref())?;
    let asset_mode = parse_asset_mode(parsed.conversion.output.asset_mode);
    let conflict = parse_conflict(parsed.conversion.output.conflict.as_deref())?;
    let sources = complete_sources(&parsed, sources)?;
    let timeout_ms = parsed.conversion.timeout_ms;
    Ok(LoadedConfig {
        paths: ConfigPaths { global, project, explicit, loaded },
        merged,
        ai_provider: parsed
            .conversion
            .ai
            .provider
            .clone()
            .or_else(|| parsed.default_provider.clone()),
        ai_model: parsed.conversion.ai.model.clone(),
        ocr_languages: parsed.conversion.ocr.languages.clone(),
        prompts: parsed.conversion.ai.prompts.clone(),
        timeout_ms,
        effective: parsed,
        options,
        language,
        jobs,
        emit,
        asset_mode,
        conflict,
        sources,
    })
}

fn merge_layer(
    value: toml::Value,
    path: &Path,
    profile: Option<&str>,
    merged: &mut toml::Value,
    effective: &mut toml::Value,
    sources: &mut BTreeMap<String, String>,
) -> bool {
    merge_value(merged, value.clone());
    let mut base = value;
    let profile_overlay = base
        .as_table_mut()
        .and_then(|table| table.remove("profiles"))
        .and_then(|profiles| profile.and_then(|name| profiles.get(name).cloned()));
    record_sources(&base, "", &path.display().to_string(), sources);
    merge_value(effective, base);
    if let Some(overlay) = profile_overlay {
        record_sources(
            &overlay,
            "",
            &format!("{}#profile:{}", path.display(), profile.unwrap_or_default()),
            sources,
        );
        merge_value(effective, overlay);
        true
    } else {
        false
    }
}

fn resolve_language(
    parsed: &mut RawConfig,
    language_override: Option<Language>,
    sources: &mut BTreeMap<String, String>,
) -> Language {
    let environment = std::env::var("INTO_MD_LANGUAGE").ok();
    let environment = environment.as_deref().and_then(parse_language);
    let language = language_override
        .or(environment)
        .or_else(|| parsed.cli.language.as_deref().and_then(parse_language))
        .unwrap_or_default();
    if language_override.is_some() {
        sources.insert("cli.language".into(), "command line: --language".into());
    } else if environment.is_some() {
        sources.insert("cli.language".into(), "environment: INTO_MD_LANGUAGE".into());
    }
    parsed.cli.language = Some(
        match language {
            Language::En => "en",
            Language::ZhCn => "zh-CN",
        }
        .into(),
    );
    language
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() { path.to_owned() } else { cwd.join(path) }
}

fn record_sources(
    value: &toml::Value,
    prefix: &str,
    source: &str,
    sources: &mut BTreeMap<String, String>,
) {
    if let toml::Value::Table(table) = value {
        for (key, child) in table {
            let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
            if child.is_table() {
                record_sources(child, &path, source, sources);
            } else {
                sources.insert(path, source.to_owned());
            }
        }
    }
}

fn complete_sources(
    config: &RawConfig,
    mut sources: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, CliError> {
    let value = toml::Value::try_from(config)
        .map_err(|error| CliError::internal(format!("serialize resolved config: {error}")))?;
    let mut defaults = BTreeMap::new();
    record_sources(&value, "", "built-in default", &mut defaults);
    for (key, source) in defaults {
        sources.entry(key).or_insert(source);
    }
    Ok(sources)
}

fn resolve_conversion_options(config: &ConversionConfig) -> Result<ConversionOptions, CliError> {
    let mut options = ConversionOptions::default();
    if let Some(mode) = config.text.decoding_mode {
        options.text.decoding_mode = mode;
    }
    if let Some(mode) = config.delimited_text.header {
        options.delimited_text.header = mode;
    }
    if let Some(mode) = config.delimited_text.ragged_rows {
        options.delimited_text.ragged_rows = mode;
    }
    if let Some(policy) = config.ocr.policy {
        options.ocr.policy = policy;
    }
    if let Some(bundle) = &config.ocr.model_bundle {
        options.ocr.model_bundle = Some(bundle.clone());
    }
    if let Some(confidence) = config.ocr.minimum_confidence {
        validate_confidence(confidence)?;
        options.ocr.minimum_confidence = confidence;
    }
    if let Some(value) = &config.asr.model_bundle {
        options.asr.model_bundle.clone_from(value);
    }
    if let Some(value) = &config.asr.language {
        options.asr.language = Some(value.clone());
    }
    if let Some(value) = config.asr.chinese_script {
        options.asr.chinese_script = value;
    }
    if let Some(value) = config.asr.max_threads {
        options.asr.max_threads = value;
    }
    if let Some(value) = config.asr.max_duration_ms {
        options.asr.max_duration_ms = Some(value);
    }
    if let Some(value) = config.asr.max_segments {
        options.asr.max_segments = value;
    }
    if let Some(value) = config.asr.max_native_memory_bytes {
        options.asr.max_native_memory_bytes = value;
    }
    let ai = &config.ai;
    if let Some(mode) = ai.vision_ocr {
        options.ai.vision_ocr = mode;
    }
    if let Some(mode) = ai.image_description {
        options.ai.image_description = mode;
    }
    if let Some(mode) = ai.layout_repair {
        options.ai.layout_repair = mode;
    }
    if let Some(mode) = ai.table_repair {
        options.ai.table_repair = mode;
    }
    if let Some(mode) = ai.formula_repair {
        options.ai.formula_repair = mode;
    }
    if let Some(mode) = ai.audio_transcription {
        options.ai.audio_transcription = mode;
    }
    if let Some(mode) = ai.markdown_postprocess {
        options.ai.markdown_postprocess = mode;
    }
    let network = &config.network;
    if let Some(value) = network.max_redirects {
        options.network.max_redirects = value;
    }
    if !network.allowed_hosts.is_empty() {
        options.network.allowed_hosts = normalize_allowed_hosts(&network.allowed_hosts)?;
    }
    if let Some(value) = network.deny_private_networks {
        options.network.deny_private_networks = value;
    }
    let limits = &config.limits;
    macro_rules! assign {
        ($field:ident) => {
            if let Some(value) = limits.$field {
                options.limits.$field = value;
            }
        };
    }
    assign!(max_input_bytes);
    assign!(max_decompressed_bytes);
    assign!(max_archive_entries);
    assign!(max_archive_depth);
    assign!(max_archive_entry_bytes);
    assign!(max_archive_compression_ratio);
    assign!(max_nesting_depth);
    assign!(max_pages);
    assign!(max_asset_bytes);
    assign!(max_total_asset_bytes);
    assign!(max_memory_bytes);
    assign!(max_temporary_bytes);
    assign!(max_table_rows);
    assign!(max_table_columns);
    assign!(max_table_cells);
    assign!(max_field_bytes);
    if let Some(value) = &config.output.asset_directory_suffix {
        options.output.asset_directory_suffix.clone_from(value);
    }
    if let Some(value) = config.output.include_provenance {
        options.output.include_provenance = value;
    }
    if let Some(value) = config.output.asset_mode {
        options.output.asset_mode = value;
    }
    Ok(options)
}

fn validate_common(config: &RawConfig) -> Result<(), CliError> {
    if let Some(version) = config.schema_version
        && version != 1
    {
        return Err(CliError::config(format!(
            "unsupported configuration schema version {version}"
        )));
    }
    if let Some(language) = &config.cli.language
        && parse_language(language).is_none()
    {
        return Err(CliError::config(format!("unsupported language '{language}'")));
    }
    for value in [&config.cli.color, &config.cli.progress].into_iter().flatten() {
        if !matches!(value.as_str(), "auto" | "always" | "never") {
            return Err(CliError::config(format!("invalid terminal policy '{value}'")));
        }
    }
    if let Some(value) = &config.cli.log_format
        && !matches!(value.as_str(), "text" | "json")
    {
        return Err(CliError::config(format!("invalid log format '{value}'")));
    }
    if let Some(confidence) = config.conversion.ocr.minimum_confidence {
        validate_confidence(confidence)?;
    }
    if config.conversion.timeout_ms == Some(0) {
        return Err(CliError::config("conversion.timeout_ms must be greater than zero"));
    }
    if config.conversion.network.deny_private_networks == Some(false) {
        return Err(CliError::config(
            "conversion.network.deny_private_networks cannot be false; private-network access requires --allow-private-network on the current invocation",
        ));
    }
    normalize_allowed_hosts(&config.conversion.network.allowed_hosts)?;
    parse_emit(config.conversion.output.emit.as_deref())?;
    parse_conflict(config.conversion.output.conflict.as_deref())?;
    for (id, plugin) in &config.plugins {
        validate_id("plugin", id)?;
        if !plugin.protocol.is_empty()
            && !matches!(plugin.protocol.as_str(), "process-v1" | "wasi-v1")
        {
            return Err(CliError::config(format!(
                "plugin '{id}' protocol must be process-v1 or wasi-v1"
            )));
        }
        if let Some(hash) = &plugin.sha256 {
            validate_sha256(hash)?;
        }
        if !plugin.signing_key_sha256.is_empty() {
            validate_sha256(&plugin.signing_key_sha256)?;
        }
    }
    validate_capability_routes(&config.capability_routes)?;
    Ok(())
}

fn validate_capability_routes(routes: &CapabilityRoutesConfig) -> Result<(), CliError> {
    for (name, route) in [
        ("ocr", &routes.ocr),
        ("transcription", &routes.transcription),
        ("diarization", &routes.diarization),
    ] {
        for reference in route.primary.iter().chain(&route.fallbacks) {
            let Some((plugin, capability)) = reference.split_once('/') else {
                return Err(CliError::config(format!(
                    "capability route '{name}' must use plugin-id/capability-id"
                )));
            };
            validate_id("plugin", plugin)?;
            validate_id("capability", capability)?;
            if capability.contains('/') {
                return Err(CliError::config(format!(
                    "capability route '{name}' contains an invalid capability ID"
                )));
            }
        }
    }
    Ok(())
}

fn validate_raw(config: &RawConfig) -> Result<(), CliError> {
    validate_common(config)?;
    for (name, provider) in &config.providers {
        validate_provider(name, provider, true)?;
    }
    for (id, plugin) in &config.plugins {
        if !matches!(plugin.protocol.as_str(), "process-v1" | "wasi-v1") {
            return Err(CliError::config(format!(
                "plugin '{id}' protocol must be process-v1 or wasi-v1"
            )));
        }
        if plugin.signing_key_id.is_empty() != plugin.signing_key_sha256.is_empty() {
            return Err(CliError::config(format!(
                "plugin '{id}' signing key id and fingerprint must be configured together"
            )));
        }
    }
    Ok(())
}

fn validate_layer(config: &RawConfig) -> Result<(), CliError> {
    validate_common(config)?;
    for (name, provider) in &config.providers {
        validate_provider(name, provider, false)?;
    }
    Ok(())
}

fn validate_provider(
    name: &str,
    provider: &ProviderConfig,
    require_complete: bool,
) -> Result<(), CliError> {
    validate_id("provider", name)?;
    if !provider.provider_type.is_empty() && provider.provider_type != "openai-compatible" {
        return Err(CliError::config(format!(
            "provider '{name}' has unsupported type '{}'",
            provider.provider_type
        )));
    }
    if !provider.base_url.is_empty() {
        validate_provider_url(name, &provider.base_url)?;
    }
    if !provider.model.is_empty()
        && (provider.model.len() > MAX_PROVIDER_MODEL_BYTES
            || provider.model.chars().any(char::is_control))
    {
        return Err(CliError::config(format!("provider '{name}' model is invalid")));
    }
    if !provider.api_key_env.is_empty() {
        validate_environment_name(&provider.api_key_env)?;
    }
    for capability in &provider.capabilities {
        validate_capability(capability)?;
    }
    if require_complete {
        let missing = [
            ("type", provider.provider_type.is_empty()),
            ("base_url", provider.base_url.is_empty()),
            ("model", provider.model.is_empty()),
            ("api_key_env", provider.api_key_env.is_empty()),
        ]
        .into_iter()
        .filter_map(|(field, missing)| missing.then_some(field))
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(CliError::config(format!(
                "provider '{name}' is incomplete after merging: missing {}",
                missing.join(", ")
            )));
        }
    }
    Ok(())
}

fn validate_provider_url(name: &str, value: &str) -> Result<(), CliError> {
    if value.len() > MAX_PROVIDER_BASE_URL_BYTES {
        return Err(CliError::config(format!("provider '{name}' URL exceeds its size limit")));
    }
    let url = url::Url::parse(value)
        .map_err(|error| CliError::config(format!("provider '{name}' URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CliError::config(format!("provider '{name}' URL must use http or https")));
    }
    if url.host_str().is_none() {
        return Err(CliError::config(format!("provider '{name}' URL must include a hostname")));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CliError::config(format!(
            "provider '{name}' URL must not include user information"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() || url.as_str() != value {
        return Err(CliError::config(format!(
            "provider '{name}' URL must be canonical and must not include a query or fragment"
        )));
    }
    Ok(())
}

pub fn validate_file(path: &Path) -> Result<(), CliError> {
    read_config_value(path, true).map(|_| ())
}

fn read_layer_value(path: &Path) -> Result<toml::Value, CliError> {
    read_config_value(path, false)
}

fn read_config_value(path: &Path, require_complete: bool) -> Result<toml::Value, CliError> {
    let text = fs::read_to_string(path).map_err(|error| {
        CliError::config(format!("read configuration {}: {error}", path.display()))
    })?;
    let value: toml::Value = toml::from_str(&text).map_err(|error| {
        CliError::config(format!("parse configuration {}: {error}", path.display()))
    })?;
    validate_explicit_provider_values(&value)?;
    let parsed: RawConfig = value.clone().try_into().map_err(|error| {
        CliError::config(format!("validate configuration {}: {error}", path.display()))
    })?;
    validate_document(&parsed, true, require_complete)
        .map_err(|error| CliError::config(format!("configuration {}: {error}", path.display())))?;
    Ok(value)
}

fn validate_explicit_provider_values(value: &toml::Value) -> Result<(), CliError> {
    if let Some(providers) = value.get("providers").and_then(toml::Value::as_table) {
        validate_explicit_provider_table(providers, "provider")?;
    }
    if let Some(profiles) = value.get("profiles").and_then(toml::Value::as_table) {
        for (profile, value) in profiles {
            if let Some(providers) = value.get("providers").and_then(toml::Value::as_table) {
                validate_explicit_provider_table(
                    providers,
                    &format!("profile '{profile}' provider"),
                )?;
            }
        }
    }
    Ok(())
}

fn validate_explicit_provider_table(
    providers: &toml::map::Map<String, toml::Value>,
    context: &str,
) -> Result<(), CliError> {
    for (name, provider) in providers {
        let Some(table) = provider.as_table() else {
            continue;
        };
        for field in ["type", "base_url", "model", "api_key_env"] {
            if table.get(field).and_then(toml::Value::as_str) == Some("") {
                return Err(CliError::config(format!(
                    "{context} '{name}' field '{field}' must not be empty"
                )));
            }
        }
    }
    Ok(())
}

fn validate_document(
    parsed: &RawConfig,
    require_schema: bool,
    require_complete: bool,
) -> Result<(), CliError> {
    if require_schema && parsed.schema_version != Some(1) {
        return Err(CliError::config("must declare schema_version = 1"));
    }
    if require_complete {
        validate_raw(parsed)?;
    } else {
        validate_layer(parsed)?;
    }
    for (name, profile) in &parsed.profiles {
        validate_id("profile", name)?;
        validate_profile(name, profile)?;
    }
    Ok(())
}

fn validate_profile(name: &str, profile: &ProfileConfig) -> Result<(), CliError> {
    let partial = RawConfig {
        cli: profile.cli.clone(),
        conversion: profile.conversion.clone(),
        capability_routes: profile.capability_routes.clone(),
        ..RawConfig::default()
    };
    validate_layer(&partial)?;
    for (provider_name, provider) in &profile.providers {
        validate_provider(provider_name, provider, false)
            .map_err(|error| CliError::config(format!("profile '{name}': {error}")))?;
    }
    for (plugin_id, plugin) in &profile.plugins {
        validate_id("plugin", plugin_id)?;
        if !plugin.protocol.is_empty()
            && !matches!(plugin.protocol.as_str(), "process-v1" | "wasi-v1")
        {
            return Err(CliError::config(format!(
                "profile '{name}' plugin '{plugin_id}' protocol must be process-v1 or wasi-v1"
            )));
        }
        if let Some(hash) = &plugin.sha256 {
            validate_sha256(hash)?;
        }
        if !plugin.signing_key_sha256.is_empty() {
            validate_sha256(&plugin.signing_key_sha256)?;
        }
    }
    Ok(())
}

pub fn global_config_path() -> Result<PathBuf, CliError> {
    #[cfg(test)]
    {
        return TEST_GLOBAL_CONFIG_PATH
            .with(|slot| slot.borrow().clone())
            .ok_or_else(|| CliError::internal("test global config path was not injected"));
    }
    #[cfg(not(test))]
    {
        if let Some(configured) = std::env::var_os("INTO_MARKDOWN_USER_DATA_HOME") {
            let requested = PathBuf::from(configured);
            if !requested.is_absolute() || !requested.is_dir() {
                return Err(CliError::config(
                    "INTO_MARKDOWN_USER_DATA_HOME must name an existing absolute directory",
                ));
            }
            let metadata = fs::symlink_metadata(&requested)?;
            if metadata.file_type().is_symlink() {
                return Err(CliError::config("user-data anchor link rejected"));
            }
            let identity = user_data_directory_identity(&requested)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                if metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.mode() & 0o077 != 0
                {
                    return Err(CliError::config("user-data anchor is not owner-private"));
                }
            }
            #[cfg(windows)]
            into_markdown_process_plugin::verify_windows_plugin_store_path(&requested)
                .map_err(|error| CliError::config(error.to_string()))?;
            let canonical = fs::canonicalize(&requested)?;
            if canonical != requested
                || user_data_directory_identity(&requested)? != identity
                || user_data_directory_identity(&canonical)? != identity
            {
                return Err(CliError::config("user-data anchor must be canonical"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                let final_metadata = fs::symlink_metadata(&canonical)?;
                if final_metadata.uid() != rustix::process::geteuid().as_raw()
                    || final_metadata.mode() & 0o077 != 0
                {
                    return Err(CliError::config("user-data anchor is not owner-private"));
                }
            }
            #[cfg(windows)]
            into_markdown_process_plugin::verify_windows_plugin_store_path(&canonical)
                .map_err(|error| CliError::config(error.to_string()))?;
            return Ok(canonical.join("into-markdown").join("config.toml"));
        }
        let dirs = ProjectDirs::from("", "", "into-markdown").ok_or_else(|| {
            CliError::config("operating-system configuration directory is unavailable")
        })?;
        Ok(dirs.config_dir().join("config.toml"))
    }
}

#[cfg(not(test))]
fn user_data_directory_identity(path: &Path) -> Result<(u64, u64), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(CliError::config("user-data anchor identity rejected"));
        }
        return Ok((metadata.dev(), metadata.ino()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let information = winapi_util::file::information(&directory)?;
        if information.file_attributes() & 0x400 != 0 {
            return Err(CliError::config("user-data anchor reparse point rejected"));
        }
        return Ok((information.volume_serial_number(), information.file_index()));
    }
    #[allow(unreachable_code)]
    Err(CliError::config("user-data anchor identity unavailable"))
}

pub fn project_config_path(cwd: &Path) -> Result<PathBuf, CliError> {
    Ok(project_scope_root(cwd)?.join(CONFIG_FILENAME))
}

/// Root directory that owns the nearest project configuration scope.
pub fn project_scope_root(cwd: &Path) -> Result<PathBuf, CliError> {
    let canonical = fs::canonicalize(cwd)?;
    Ok(find_project_config(&canonical)
        .and_then(|path| path.parent().map(Path::to_owned))
        .unwrap_or(canonical))
}

fn find_project_config(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().map(|ancestor| ancestor.join(CONFIG_FILENAME)).find(|path| path.is_file())
}

pub fn scope_path(scope: Scope, cwd: &Path) -> Result<PathBuf, CliError> {
    match scope {
        Scope::Global => global_config_path(),
        Scope::Project => project_config_path(cwd),
    }
}

pub fn init(scope: Scope, cwd: &Path, force: bool) -> Result<PathBuf, CliError> {
    let path = scope_path(scope, cwd)?;
    let lock = lock_scope(scope, cwd)?;
    if path.exists() && !force {
        return Err(CliError::config(format!(
            "configuration already exists: {} (use --force to replace it)",
            path.display()
        )));
    }
    atomic_write_locked(&lock, &path, CONFIG_TEMPLATE.as_bytes(), force, None)?;
    Ok(path)
}

pub fn get_value<'a>(value: &'a toml::Value, key: &str) -> Result<&'a toml::Value, CliError> {
    let segments = key_segments(key)?;
    let mut current = value;
    for segment in segments {
        current = current
            .get(segment)
            .ok_or_else(|| CliError::config(format!("configuration key '{key}' was not found")))?;
    }
    Ok(current)
}

pub fn set(scope: Scope, cwd: &Path, key: &str, input: &str) -> Result<PathBuf, CliError> {
    let value = parse_toml_value(input);
    mutate_scope(scope, cwd, |root| set_nested(root, key, value))
}

pub fn unset(scope: Scope, cwd: &Path, key: &str) -> Result<PathBuf, CliError> {
    mutate_scope(scope, cwd, |root| remove_nested(root, key))
}

pub fn create_profile(
    scope: Scope,
    cwd: &Path,
    name: &str,
    from: Option<&str>,
    merged: &toml::Value,
) -> Result<PathBuf, CliError> {
    validate_id("profile", name)?;
    let value = if let Some(source) = from {
        get_value(merged, &format!("profiles.{source}"))?.clone()
    } else {
        empty_table()
    };
    mutate_scope(scope, cwd, |root| set_nested(root, &format!("profiles.{name}"), value))
}

pub fn remove_profile(scope: Scope, cwd: &Path, name: &str) -> Result<PathBuf, CliError> {
    mutate_scope(scope, cwd, |root| remove_nested(root, &format!("profiles.{name}")))
}

pub fn profile_names(value: &toml::Value) -> Vec<String> {
    value
        .get("profiles")
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn add_provider(
    scope: Scope,
    cwd: &Path,
    name: &str,
    provider: &ProviderConfig,
) -> Result<PathBuf, CliError> {
    validate_id("provider", name)?;
    let value = toml::Value::try_from(provider)
        .map_err(|error| CliError::internal(format!("serialize provider: {error}")))?;
    mutate_scope(scope, cwd, |root| set_nested(root, &format!("providers.{name}"), value))
}

pub fn remove_provider(scope: Scope, cwd: &Path, name: &str) -> Result<PathBuf, CliError> {
    validate_id("provider", name)?;
    mutate_scope(scope, cwd, |root| remove_nested(root, &format!("providers.{name}")))
}

pub fn set_default_provider(scope: Scope, cwd: &Path, name: &str) -> Result<PathBuf, CliError> {
    validate_id("provider", name)?;
    mutate_scope(scope, cwd, |root| {
        set_nested(root, "default_provider", toml::Value::String(name.to_owned()))
    })
}

/// Check that an exact scope contains a self-contained provider definition.
/// Global defaults must remain usable when no project overlay is present.
pub(crate) fn has_complete_provider_in_scope(
    scope: Scope,
    cwd: &Path,
    name: &str,
) -> Result<bool, CliError> {
    let snapshot = scope_snapshot(scope, cwd)?;
    let raw: RawConfig = snapshot
        .document
        .try_into()
        .map_err(|error| CliError::config(format!("scope configuration is invalid: {error}")))?;
    let Some(provider) = raw.providers.get(name) else { return Ok(false) };
    validate_provider(name, provider, true)?;
    Ok(true)
}

pub(crate) fn compare_and_set_plugin_exact_locked(
    lock: &ConfigMutationGuard,
    path: &Path,
    id: &str,
    expected: Option<&PluginConfig>,
    replacement: Option<&PluginConfig>,
) -> Result<PathBuf, CliError> {
    validate_id("plugin", id)?;
    mutate_path_locked(lock, path, |root| {
        let current = root
            .get("plugins")
            .and_then(toml::Value::as_table)
            .and_then(|plugins| plugins.get(id))
            .map(|value| value.clone().try_into::<PluginConfig>())
            .transpose()
            .map_err(|error| CliError::config(format!("plugin config is invalid: {error}")))?;
        if current.as_ref() != expected {
            return Err(CliError::config(format!("plugin '{id}' changed during the transaction")));
        }
        if let Some(replacement) = replacement {
            let value = toml::Value::try_from(replacement)
                .map_err(|error| CliError::internal(format!("serialize plugin: {error}")))?;
            let table = root
                .as_table_mut()
                .ok_or_else(|| CliError::config("configuration root must be a table"))?;
            let plugins = table
                .entry("plugins")
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                .as_table_mut()
                .ok_or_else(|| CliError::config("plugins must be a table"))?;
            plugins.insert(id.to_owned(), value);
            Ok(())
        } else {
            if let Some(plugins) = root.get_mut("plugins").and_then(toml::Value::as_table_mut) {
                plugins.remove(id);
            }
            Ok(())
        }
    })
}

/// Read plugins from exactly one scope without applying project/profile shadowing.
#[cfg(test)]
pub fn plugins_in_scope(
    scope: Scope,
    cwd: &Path,
) -> Result<BTreeMap<String, PluginConfig>, CliError> {
    let path = scope_path(scope, cwd)?;
    plugins_at_path(&path)
}

/// Read profile objects from exactly one configuration scope.
pub fn profiles_in_scope(
    scope: Scope,
    cwd: &Path,
) -> Result<BTreeMap<String, toml::Value>, CliError> {
    let path = scope_path(scope, cwd)?;
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let value = read_layer_value(&path)?;
    Ok(value
        .get("profiles")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect())
}

#[derive(Debug, PartialEq)]
pub(crate) struct ScopeSnapshot {
    pub(crate) document: toml::Value,
    authority: crate::transaction::ConfigExpectedAuthority,
}

/// Capture the physical identity and bytes of every configuration file used by one load.
/// Administration compares this authority after plugin verification so changes in unrelated
/// project scopes cannot invalidate the snapshot while replacements of an actual input still do.
pub(crate) fn loaded_paths_snapshot(
    paths: &[PathBuf],
) -> Result<Vec<(PathBuf, crate::transaction::ConfigExpectedAuthority)>, CliError> {
    paths
        .iter()
        .map(|path| {
            let mut file = File::open(path)?;
            let initial_identity = config_file_identity(&file)?;
            let initial_metadata = file.metadata()?;
            if !initial_metadata.is_file() {
                return Err(CliError::config("configuration identity rejected"));
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let final_identity = config_file_identity(&file)?;
            let final_metadata = file.metadata()?;
            if initial_identity != final_identity
                || initial_metadata.len() != final_metadata.len()
                || final_metadata.len() != bytes.len() as u64
            {
                return Err(CliError::config("configuration changed while reading"));
            }
            Ok((
                path.clone(),
                crate::transaction::ConfigExpectedAuthority {
                    identity: Some(final_identity),
                    sha256: Some(format!("{:x}", Sha256::digest(&bytes))),
                },
            ))
        })
        .collect()
}

/// Read the complete semantic document plus physical file identity for one
/// scope. Atomic A→B→A replacement changes identity even when bytes return to
/// their original value, so administration cannot accept a mixed generation.
pub(crate) fn scope_snapshot(scope: Scope, cwd: &Path) -> Result<ScopeSnapshot, CliError> {
    let lock = lock_scope(scope, cwd)?;
    let target =
        lock.config.file_name().ok_or_else(|| CliError::config("configuration has no name"))?;
    match lock.directory.symlink_metadata(target) {
        Ok(_) => {
            let (document, authority) = read_layer_value_locked_with_authority(&lock)?;
            Ok(ScopeSnapshot { document, authority })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ScopeSnapshot {
            document: empty_table(),
            authority: crate::transaction::ConfigExpectedAuthority { identity: None, sha256: None },
        }),
        Err(error) => Err(error.into()),
    }
}

/// Return whether a recorded, unprofiled configuration source is the same
/// physical file as an exact scope. Comparing open-file identities avoids
/// path spelling, symlink-alias, Windows case and `#` delimiter ambiguity.
pub(crate) fn source_is_exact_scope(
    source: &str,
    scope: Scope,
    cwd: &Path,
) -> Result<bool, CliError> {
    let scope_path = scope_path(scope, cwd)?;
    let source_file = match File::open(Path::new(source)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let scope_file = match File::open(scope_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !source_file.metadata()?.is_file() || !scope_file.metadata()?.is_file() {
        return Ok(false);
    }
    Ok(config_file_identity(&source_file)? == config_file_identity(&scope_file)?)
}

pub(crate) fn plugins_in_exact_locked(
    lock: &ConfigMutationGuard,
    path: &Path,
) -> Result<BTreeMap<String, PluginConfig>, CliError> {
    if lock.config != path {
        return Err(CliError::config("configuration lock scope mismatch"));
    }
    match lock.directory.symlink_metadata(
        path.file_name().ok_or_else(|| CliError::config("configuration has no name"))?,
    ) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    }
    let value = read_layer_value_locked(lock)?;
    let parsed: RawConfig = value
        .clone()
        .try_into()
        .map_err(|error| CliError::config(format!("scope configuration is invalid: {error}")))?;
    validate_explicit_provider_values(&value)?;
    validate_document(&parsed, false, false)?;
    Ok(parsed.plugins)
}

#[cfg(test)]
fn plugins_at_path(path: &Path) -> Result<BTreeMap<String, PluginConfig>, CliError> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let value = read_layer_value(path)?;
    let parsed: RawConfig = value
        .clone()
        .try_into()
        .map_err(|error| CliError::config(format!("scope configuration is invalid: {error}")))?;
    validate_explicit_provider_values(&value)?;
    validate_document(&parsed, false, false)?;
    Ok(parsed.plugins)
}

fn mutate_scope(
    scope: Scope,
    cwd: &Path,
    operation: impl FnOnce(&mut toml::Value) -> Result<(), CliError>,
) -> Result<PathBuf, CliError> {
    let lock = lock_scope(scope, cwd)?;
    mutate_scope_locked(&lock, scope, cwd, operation)
}

fn mutate_scope_locked(
    lock: &ConfigMutationGuard,
    scope: Scope,
    cwd: &Path,
    operation: impl FnOnce(&mut toml::Value) -> Result<(), CliError>,
) -> Result<PathBuf, CliError> {
    let path = scope_path(scope, cwd)?;
    mutate_path_locked(lock, &path, operation)
}

fn mutate_path_locked(
    lock: &ConfigMutationGuard,
    path: &Path,
    operation: impl FnOnce(&mut toml::Value) -> Result<(), CliError>,
) -> Result<PathBuf, CliError> {
    if lock.config != path {
        return Err(CliError::config("configuration lock scope mismatch"));
    }
    let target = path.file_name().ok_or_else(|| CliError::config("configuration has no name"))?;
    let (mut value, expected) = match lock.directory.symlink_metadata(target) {
        Ok(_) => read_layer_value_locked_with_authority(lock)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
            empty_table(),
            crate::transaction::ConfigExpectedAuthority { identity: None, sha256: None },
        ),
        Err(error) => return Err(error.into()),
    };
    operation(&mut value)?;
    if value.get("schema_version").is_none() {
        value
            .as_table_mut()
            .ok_or_else(|| CliError::config("configuration root is not a table"))?
            .insert("schema_version".into(), toml::Value::Integer(1));
    }
    let parsed: RawConfig = value
        .clone()
        .try_into()
        .map_err(|error| CliError::config(format!("updated configuration is invalid: {error}")))?;
    validate_explicit_provider_values(&value)?;
    validate_document(&parsed, false, false)?;
    let text = toml::to_string_pretty(&value)
        .map_err(|error| CliError::internal(format!("serialize configuration: {error}")))?;
    #[cfg(test)]
    {
        let marker = Path::new(".test-config-mutate-after-locked-read");
        if let Ok(racer) = lock.directory.read(marker) {
            lock.directory.remove_file(marker)?;
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).truncate(true);
            let mut target_file = lock.directory.open_with(target, &options)?.into_std();
            use std::io::Write as _;
            target_file.write_all(&racer)?;
            target_file.sync_all()?;
        }
    }
    atomic_write_locked(lock, &path, text.as_bytes(), true, Some(&expected))?;
    Ok(path.to_owned())
}

/// Exclusive mutation token for one exact configuration scope.
pub struct ConfigMutationGuard {
    file: File,
    config: PathBuf,
    #[cfg_attr(not(windows), allow(dead_code))]
    directory: cap_std::fs::Dir,
}

impl ConfigMutationGuard {
    fn acquire(config: &Path) -> Result<Self, CliError> {
        let parent = config
            .parent()
            .ok_or_else(|| CliError::config("configuration has no parent directory"))?;
        fs::create_dir_all(parent)?;
        let directory = cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority())?;
        let name = Path::new(".into-md-config.lock");
        let mut options = cap_std::fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            options.custom_flags(0x0020_0000);
        }
        let file = directory.open_with(name, &options)?.into_std();
        let metadata = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != rustix::process::geteuid().as_raw()
            {
                return Err(CliError::config("configuration lock identity rejected"));
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;
            let information = winapi_util::file::information(&file)?;
            if !metadata.is_file()
                || metadata.file_attributes() & 0x400 != 0
                || information.number_of_links() != 1
            {
                return Err(CliError::config("configuration lock identity rejected"));
            }
        }
        file.try_lock()
            .map_err(|_| CliError::config("another configuration mutation is active"))?;
        let target =
            config.file_name().ok_or_else(|| CliError::config("configuration has no file name"))?;
        crate::transaction::recover_config_in_dir(&directory, target)?;
        Ok(Self { file, config: config.to_owned(), directory })
    }

    pub(crate) fn directory_identity(&self) -> Result<(u64, u64), CliError> {
        let directory = self.directory.try_clone()?.into_std_file();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = directory.metadata()?;
            return Ok((metadata.dev(), metadata.ino()));
        }
        #[cfg(windows)]
        {
            let information = winapi_util::file::information(&directory)?;
            return Ok((information.volume_serial_number(), information.file_index()));
        }
        #[allow(unreachable_code)]
        Err(CliError::config("configuration directory identity unavailable"))
    }
}

fn read_layer_value_locked(lock: &ConfigMutationGuard) -> Result<toml::Value, CliError> {
    read_layer_value_locked_with_authority(lock).map(|(value, _)| value)
}

fn read_layer_value_locked_with_authority(
    lock: &ConfigMutationGuard,
) -> Result<(toml::Value, crate::transaction::ConfigExpectedAuthority), CliError> {
    let name = lock
        .config
        .file_name()
        .ok_or_else(|| CliError::config("configuration has no file name"))?;
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let mut file = lock.directory.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    let initial_identity = config_file_identity(&file)?;
    if !metadata.is_file() {
        return Err(CliError::config("configuration identity rejected"));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let final_metadata = file.metadata()?;
    let identity = config_file_identity(&file)?;
    if metadata.len() != final_metadata.len()
        || metadata.len() != text.len() as u64
        || identity != initial_identity
    {
        return Err(CliError::config("configuration changed while reading"));
    }
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| CliError::config(format!("parse configuration: {error}")))?;
    validate_explicit_provider_values(&value)?;
    let parsed: RawConfig = value
        .clone()
        .try_into()
        .map_err(|error| CliError::config(format!("validate configuration: {error}")))?;
    validate_document(&parsed, true, false)?;
    Ok((
        value,
        crate::transaction::ConfigExpectedAuthority {
            identity: Some(identity),
            sha256: Some(format!("{:x}", Sha256::digest(text.as_bytes()))),
        },
    ))
}

fn config_file_identity(file: &File) -> Result<(u64, u64), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.metadata()?;
        return Ok((metadata.dev(), metadata.ino()));
    }
    #[cfg(windows)]
    {
        let information = winapi_util::file::information(file)?;
        return Ok((information.volume_serial_number(), information.file_index()));
    }
    #[allow(unreachable_code)]
    Err(CliError::config("configuration identity unavailable"))
}

impl Drop for ConfigMutationGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Acquire the transaction-wide mutation lock for one exact scope.
pub fn lock_scope(scope: Scope, cwd: &Path) -> Result<ConfigMutationGuard, CliError> {
    ConfigMutationGuard::acquire(&scope_path(scope, cwd)?)
}

pub(crate) fn lock_exact(path: &Path) -> Result<ConfigMutationGuard, CliError> {
    ConfigMutationGuard::acquire(path)
}

/// Whether recovery must run before reading this scope's configuration.
pub fn scope_has_pending_transaction(scope: Scope, cwd: &Path) -> Result<bool, CliError> {
    let path = scope_path(scope, cwd)?;
    let Some(parent) = path.parent() else { return Ok(false) };
    if !parent.is_dir() {
        return Ok(false);
    }
    #[cfg(windows)]
    {
        return Ok(fs::read_dir(parent)?.filter_map(Result::ok).any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".into-md-config-") && name.ends_with(".journal")
        }));
    }
    #[cfg(unix)]
    {
        return Ok(parent.join(".into-md-output-transactions-01").is_dir());
    }
    #[allow(unreachable_code)]
    Ok(false)
}

fn set_nested(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<(), CliError> {
    let segments = key_segments(key)?;
    let Some((last, parents)) = segments.split_last() else {
        return Err(CliError::config("configuration key is empty"));
    };
    let mut current = root;
    for segment in parents {
        if current.get(*segment).is_none() {
            current
                .as_table_mut()
                .ok_or_else(|| CliError::config(format!("'{segment}' is not a table")))?
                .insert((*segment).to_owned(), empty_table());
        }
        current = current
            .get_mut(*segment)
            .ok_or_else(|| CliError::internal("new configuration table disappeared"))?;
        if !current.is_table() {
            return Err(CliError::config(format!("'{segment}' is not a table")));
        }
    }
    current
        .as_table_mut()
        .ok_or_else(|| CliError::config("configuration root is not a table"))?
        .insert((*last).to_owned(), value);
    Ok(())
}

fn remove_nested(root: &mut toml::Value, key: &str) -> Result<(), CliError> {
    let segments = key_segments(key)?;
    let Some((last, parents)) = segments.split_last() else {
        return Err(CliError::config("configuration key is empty"));
    };
    let mut current = root;
    for segment in parents {
        current = current
            .get_mut(*segment)
            .ok_or_else(|| CliError::config(format!("configuration key '{key}' was not found")))?;
    }
    let removed = current
        .as_table_mut()
        .and_then(|table| table.remove(*last))
        .ok_or_else(|| CliError::config(format!("configuration key '{key}' was not found")))?;
    drop(removed);
    Ok(())
}

fn key_segments(key: &str) -> Result<Vec<&str>, CliError> {
    let segments = key.split('.').collect::<Vec<_>>();
    if segments.is_empty()
        || segments.iter().any(|segment| {
            segment.is_empty()
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return Err(CliError::config(format!("invalid dotted configuration key '{key}'")));
    }
    Ok(segments)
}

fn parse_toml_value(input: &str) -> toml::Value {
    let wrapped = format!("value = {input}");
    toml::from_str::<toml::Value>(&wrapped)
        .ok()
        .and_then(|mut value| value.as_table_mut()?.remove("value"))
        .unwrap_or_else(|| toml::Value::String(input.to_owned()))
}

fn atomic_write_locked(
    lock: &ConfigMutationGuard,
    path: &Path,
    bytes: &[u8],
    replace: bool,
    expected: Option<&crate::transaction::ConfigExpectedAuthority>,
) -> Result<(), CliError> {
    if lock.config != path {
        return Err(CliError::config("configuration lock scope mismatch"));
    }
    let name =
        path.file_name().ok_or_else(|| CliError::config("configuration has no file name"))?;
    crate::transaction::atomic_replace_config_in_dir(
        &lock.directory,
        name,
        bytes,
        replace,
        expected,
    )
}

fn merge_value(target: &mut toml::Value, overlay: toml::Value) {
    match (target, overlay) {
        (toml::Value::Table(target), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(existing) = target.get_mut(&key) {
                    merge_value(existing, value);
                } else {
                    target.insert(key, value);
                }
            }
        }
        (target, overlay) => *target = overlay,
    }
}

fn empty_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

fn parse_language(value: &str) -> Option<Language> {
    match value {
        "en" => Some(Language::En),
        "zh-CN" => Some(Language::ZhCn),
        _ => None,
    }
}

fn parse_emit(value: Option<&str>) -> Result<EmitKind, CliError> {
    match value.unwrap_or("markdown") {
        "markdown" => Ok(EmitKind::Markdown),
        "ir-json" => Ok(EmitKind::IrJson),
        "result-json" => Ok(EmitKind::ResultJson),
        "bundle" => Ok(EmitKind::Bundle),
        value => Err(CliError::config(format!("invalid output emit kind '{value}'"))),
    }
}

fn parse_asset_mode(value: Option<AssetMode>) -> AssetModeArg {
    match value.unwrap_or(AssetMode::Extract) {
        AssetMode::Extract => AssetModeArg::Extract,
        AssetMode::Embed => AssetModeArg::Embed,
        AssetMode::Omit => AssetModeArg::Omit,
    }
}

fn parse_conflict(value: Option<&str>) -> Result<ConflictPolicy, CliError> {
    match value.unwrap_or("rename") {
        "rename" => Ok(ConflictPolicy::Rename),
        "error" => Ok(ConflictPolicy::Error),
        "overwrite" => Ok(ConflictPolicy::Overwrite),
        value => Err(CliError::config(format!("invalid output conflict policy '{value}'"))),
    }
}

pub fn validate_confidence(value: f32) -> Result<(), CliError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(CliError::usage("OCR minimum confidence must be in the inclusive range 0..1"))
    }
}

pub fn validate_capability(value: &str) -> Result<(), CliError> {
    if matches!(
        value,
        "vision-ocr"
            | "image-description"
            | "layout-repair"
            | "table-repair"
            | "formula-repair"
            | "audio-transcription"
            | "markdown-postprocess"
    ) {
        Ok(())
    } else {
        Err(CliError::usage(format!("unknown AI capability '{value}'")))
    }
}

pub fn validate_sha256(value: &str) -> Result<(), CliError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(CliError::usage("SHA-256 must contain exactly 64 hexadecimal characters"))
    }
}

pub fn validate_environment_name(value: &str) -> Result<(), CliError> {
    if value.len() > MAX_ENVIRONMENT_NAME_BYTES {
        return Err(CliError::usage("environment variable name exceeds its size limit"));
    }
    let mut bytes = value.bytes();
    let first = bytes.next();
    if first.is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        Ok(())
    } else {
        Err(CliError::usage(format!("invalid environment variable name '{value}'")))
    }
}

/// Normalize a hostname allowlist for exact, port-independent comparisons.
pub fn normalize_allowed_hosts(values: &[String]) -> Result<Vec<String>, CliError> {
    values
        .iter()
        .map(|value| normalize_allowed_host(value))
        .collect::<Result<BTreeSet<_>, _>>()
        .map(|hosts| hosts.into_iter().collect())
}

/// Normalize one DNS name or IP address to the URL parser's ASCII host representation.
pub fn normalize_allowed_host(value: &str) -> Result<String, CliError> {
    if value.is_empty() || value.trim() != value {
        return Err(CliError::config(format!(
            "invalid allowed host '{value}': expected a hostname without whitespace"
        )));
    }
    let without_root_dot = value.strip_suffix('.').unwrap_or(value);
    if without_root_dot.is_empty() || without_root_dot.ends_with('.') {
        return Err(CliError::config(format!(
            "invalid allowed host '{value}': expected at most one trailing dot"
        )));
    }
    let host = url::Host::parse(without_root_dot).map_err(|error| {
        CliError::config(format!(
            "invalid allowed host '{value}': use a hostname or IP address without a scheme or port ({error})"
        ))
    })?;
    Ok(host.to_string().to_ascii_lowercase())
}

fn validate_id(kind: &str, value: &str) -> Result<(), CliError> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(CliError::config(format!("invalid {kind} ID '{value}'")))
    }
}

fn redact_value(value: &mut toml::Value, key: Option<&str>) {
    if key.is_some_and(is_secret_key) {
        *value = toml::Value::String("<redacted>".into());
        return;
    }
    match value {
        toml::Value::String(text) => {
            if let Ok(mut parsed) = url::Url::parse(text) {
                let mut changed = parsed.query().is_some() || parsed.fragment().is_some();
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    let _ = parsed.set_username("");
                    let _ = parsed.set_password(None);
                    changed = true;
                }
                if changed {
                    parsed.set_query(None);
                    parsed.set_fragment(None);
                    *text = parsed.to_string();
                }
            }
        }
        toml::Value::Table(table) => {
            for (child_key, child) in table {
                redact_value(child, Some(child_key));
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                redact_value(item, key);
            }
        }
        _ => {}
    }
}

pub(crate) fn redacted_value(value: &toml::Value) -> toml::Value {
    let mut value = value.clone();
    redact_value(&mut value, None);
    value
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    !normalized.ends_with("_env")
        && ["api_key", "apikey", "access_key", "token", "password", "secret"]
            .iter()
            .any(|part| normalized == *part || normalized.ends_with(&format!("_{part}")))
}

fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

const CONFIG_TEMPLATE: &str = r#"schema_version = 1

[cli]
language = "en"

[conversion.ocr]
policy = "auto"
model_bundle = "pp-ocrv6-tiny-zh-en"
languages = ["zh-Hans", "zh-Hant", "en"]
minimum_confidence = 0.70

[conversion.ai]
vision_ocr = "off"
image_description = "off"
layout_repair = "off"
table_repair = "off"
formula_repair = "off"
audio_transcription = "off"
markdown_postprocess = "off"

[conversion.network]
max_redirects = 3
deny_private_networks = true

[conversion.output]
emit = "markdown"
asset_mode = "extract"
conflict = "rename"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn temporary_directory(name: &str) -> PathBuf {
        let thread_name = std::thread::current()
            .name()
            .unwrap_or("test")
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() { character } else { '-' })
            .collect::<String>();
        let path = std::env::temp_dir().join(format!(
            "into-md-config-{name}-{}-{}",
            std::process::id(),
            thread_name
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn explicit_config_and_profile_merge_with_safe_network_default() {
        let root = temporary_directory("merge");
        let path = root.join("custom.toml");
        fs::write(
            &path,
            r#"schema_version = 1
[conversion.network]
max_redirects = 1
[profiles.quality.conversion.ocr]
policy = "always"
"#,
        )
        .unwrap();
        let loaded = load(&root, &[path], true, Some("quality"), None).unwrap();
        assert_eq!(loaded.options.ocr.policy, OcrPolicy::Always);
        assert_eq!(loaded.options.network.max_redirects, 1);
        assert!(!loaded.options.network.enabled);
        assert!(loaded.sources["conversion.ocr.policy"].ends_with("custom.toml#profile:quality"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_layers_and_profiles_follow_command_line_order() {
        let root = temporary_directory("layer-order");
        fs::write(
            root.join("one.toml"),
            r"schema_version = 1
[cli]
jobs = 2
[profiles.ci.cli]
jobs = 3
",
        )
        .unwrap();
        fs::write(
            root.join("two.toml"),
            r"schema_version = 1
[cli]
jobs = 4
[profiles.ci.cli]
jobs = 5
",
        )
        .unwrap();
        let loaded = load(
            &root,
            &[PathBuf::from("one.toml"), PathBuf::from("two.toml")],
            true,
            Some("ci"),
            None,
        )
        .unwrap();
        assert_eq!(loaded.jobs, 5);
        assert_eq!(loaded.paths.explicit, vec![root.join("one.toml"), root.join("two.toml")]);
        assert_eq!(
            loaded.sources["cli.jobs"],
            format!("{}#profile:ci", root.join("two.toml").display())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_can_partially_override_provider_from_a_lower_layer() {
        let root = temporary_directory("partial-provider");
        fs::write(
            root.join("base.toml"),
            r#"schema_version = 1
[providers.vision]
type = "openai-compatible"
base_url = "https://example.com/v1"
model = "base"
api_key_env = "VISION_API_KEY"
"#,
        )
        .unwrap();
        fs::write(
            root.join("overlay.toml"),
            r#"schema_version = 1
[profiles.quality.providers.vision]
model = "quality"
"#,
        )
        .unwrap();
        let loaded = load(
            &root,
            &[PathBuf::from("base.toml"), PathBuf::from("overlay.toml")],
            true,
            Some("quality"),
            None,
        )
        .unwrap();
        assert_eq!(loaded.effective.providers["vision"].model, "quality");
        assert_eq!(
            loaded.sources["providers.vision.model"],
            format!("{}#profile:quality", root.join("overlay.toml").display())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_layers_can_partially_override_provider_fields() {
        let root = temporary_directory("partial-provider-layers");
        let global = root.join("global/config.toml");
        let project = root.join("project");
        let cwd = project.join("nested");
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::write(
            &global,
            r#"schema_version = 1
[providers.vision]
type = "openai-compatible"
base_url = "https://global.example/v1"
model = "global"
api_key_env = "GLOBAL_KEY"
"#,
        )
        .unwrap();
        fs::write(
            project.join(CONFIG_FILENAME),
            r#"schema_version = 1
[providers.vision]
model = "project"
"#,
        )
        .unwrap();
        let explicit_one = root.join("explicit-one.toml");
        fs::write(
            &explicit_one,
            r#"schema_version = 1
[providers.vision]
base_url = "https://explicit.example/v1"
"#,
        )
        .unwrap();
        let explicit_two = root.join("explicit-two.toml");
        fs::write(
            &explicit_two,
            r#"schema_version = 1
[providers.vision]
api_key_env = "EXPLICIT_KEY"
"#,
        )
        .unwrap();
        let loaded =
            load_with_global(&cwd, Some(global), &[explicit_one, explicit_two], false, None, None)
                .unwrap();
        let provider = &loaded.effective.providers["vision"];
        assert_eq!(provider.provider_type, "openai-compatible");
        assert_eq!(provider.base_url, "https://explicit.example/v1");
        assert_eq!(provider.model, "project");
        assert_eq!(provider.api_key_env, "EXPLICIT_KEY");
        assert!(loaded.sources["providers.vision.model"].ends_with(".into-markdown.toml"));
        assert!(loaded.sources["providers.vision.api_key_env"].ends_with("explicit-two.toml"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_automatic_config_has_no_global_authority_but_loads_explicit_files() {
        let root = temporary_directory("no-automatic-explicit");
        let explicit = root.join("explicit.toml");
        fs::write(&explicit, "schema_version = 1\n").unwrap();

        let loaded = load_with_global(&root, None, &[explicit.clone()], true, None, None).unwrap();

        assert_eq!(loaded.paths.global, None);
        assert_eq!(loaded.paths.explicit, vec![explicit.clone()]);
        assert_eq!(loaded.paths.loaded, vec![explicit]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_provider_is_rejected_after_final_merge() {
        let root = temporary_directory("incomplete-provider");
        let path = root.join("partial.toml");
        fs::write(&path, "schema_version = 1\n[providers.vision]\nmodel = \"override\"\n").unwrap();
        let validation = validate_file(&path).unwrap_err();
        assert!(validation.to_string().contains("incomplete after merging"));
        let loading = load(&root, &[path], true, None, None).unwrap_err();
        assert!(loading.to_string().contains("incomplete after merging"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_provider_layers_still_validate_present_fields() {
        let root = temporary_directory("invalid-partial-provider");
        let path = root.join("partial.toml");
        fs::write(
            &path,
            "schema_version = 1\n[providers.vision]\nbase_url = \"https://user@example.com/v1\"\n",
        )
        .unwrap();
        let error = read_layer_value(&path).unwrap_err();
        assert!(error.to_string().contains("must not include user information"));
        fs::write(&path, "schema_version = 1\n[providers.vision]\nmodel = \"\"\n").unwrap();
        let error = read_layer_value(&path).unwrap_err();
        assert!(error.to_string().contains("field 'model' must not be empty"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_and_network_grant_fields_are_rejected() {
        let root = temporary_directory("unknown");
        let path = root.join("bad.toml");
        fs::write(&path, "[conversion.network]\nenabled = true\n").unwrap();
        let error = load(&root, &[path], true, None, None).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_profile_fields_and_private_network_relaxation_are_rejected() {
        let root = temporary_directory("profile-security");
        let unknown = root.join("unknown.toml");
        fs::write(
            &unknown,
            "schema_version = 1\n[profiles.bad.conversion.network]\nallow_network = true\n",
        )
        .unwrap();
        assert!(validate_file(&unknown).unwrap_err().to_string().contains("unknown field"));

        let relaxed = root.join("relaxed.toml");
        fs::write(
            &relaxed,
            "schema_version = 1\n[conversion.network]\ndeny_private_networks = false\n",
        )
        .unwrap();
        assert!(validate_file(&relaxed).unwrap_err().to_string().contains("cannot be false"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn redaction_removes_signed_url_query() {
        let mut value: toml::Value = toml::from_str(
            r#"[providers.example]
type = "openai-compatible"
base_url = "https://example.com/v1?sig=secret"
model = "model"
api_key_env = "API_KEY"
"#,
        )
        .unwrap();
        redact_value(&mut value, None);
        assert!(!value.to_string().contains("sig=secret"));
        assert!(value.to_string().contains("API_KEY"));
    }

    #[test]
    fn redaction_covers_signed_source_urls_credentials_and_secret_keys() {
        let mut value: toml::Value = toml::from_str(
            r#"token = "top-secret"
[plugins.remote]
source = "https://user:password@example.com/plugin.wasm?X-Amz-Signature=secret#fragment"
"#,
        )
        .unwrap();
        redact_value(&mut value, None);
        let text = value.to_string();
        assert!(!text.contains("top-secret"));
        assert!(!text.contains("password"));
        assert!(!text.contains("Signature"));
        assert!(!text.contains("fragment"));
        assert!(text.contains("<redacted>"));
    }

    #[test]
    fn platform_specific_paths_round_trip_without_interpretation() {
        let value: RawConfig = toml::from_str(
            r#"schema_version = 1
[conversion.ai.prompts]
windows = 'C:\models\prompt.txt'
posix = '/opt/models/prompt.txt'
[plugins.local]
source = 'C:\plugins\parser.wasm'
protocol = "wasi-v1"
enabled = true
"#,
        )
        .unwrap();
        assert_eq!(value.conversion.ai.prompts["windows"], PathBuf::from(r"C:\models\prompt.txt"));
        assert_eq!(value.conversion.ai.prompts["posix"], PathBuf::from("/opt/models/prompt.txt"));
        assert_eq!(value.plugins["local"].source, r"C:\plugins\parser.wasm");
    }

    #[test]
    fn resolved_display_includes_canonical_provider_value_sources() {
        let root = temporary_directory("resolved-sources");
        fs::write(
            root.join("config.toml"),
            r#"schema_version = 1
[providers.remote]
type = "openai-compatible"
base_url = "https://example.com/v1"
model = "vision"
api_key_env = "VISION_API_KEY"
"#,
        )
        .unwrap();
        let loaded = load(&root, &[PathBuf::from("config.toml")], true, None, None).unwrap();
        let display = loaded.display_value(true).unwrap();
        let text = display.to_string();
        assert!(display.get("_sources").is_some());
        assert!(!text.contains("Authorization"));
        assert!(text.contains("config.toml"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dotted_keys_are_typed_and_removable() {
        let mut value = empty_table();
        set_nested(&mut value, "conversion.output.emit", parse_toml_value("\"bundle\"")).unwrap();
        assert_eq!(get_value(&value, "conversion.output.emit").unwrap().as_str(), Some("bundle"));
        remove_nested(&mut value, "conversion.output.emit").unwrap();
        assert!(get_value(&value, "conversion.output.emit").is_err());
    }

    #[test]
    fn known_capabilities_and_hashes_validate() {
        assert!(validate_capability("vision-ocr").is_ok());
        assert!(validate_capability("magic").is_err());
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn allowed_hosts_use_url_host_normalization_and_reject_ports() {
        let normalized = normalize_allowed_hosts(&[
            "EXAMPLE.COM.".into(),
            "bücher.example".into(),
            "xn--bcher-kva.example".into(),
            "[2001:0DB8::1]".into(),
        ])
        .unwrap();
        assert_eq!(normalized, vec!["[2001:db8::1]", "example.com", "xn--bcher-kva.example"]);
        assert!(normalize_allowed_host("example.com:443").is_err());
        assert!(normalize_allowed_host("https://example.com").is_err());
        assert!(normalize_allowed_host("example.com..").is_err());
    }

    #[test]
    fn profile_names_are_deterministic() {
        let value: toml::Value = toml::from_str("[profiles.z]\n[profiles.a]\n").unwrap();
        assert_eq!(profile_names(&value), vec!["a", "z"]);
    }

    #[test]
    fn no_secret_config_field_exists() {
        let fields = BTreeSet::from(["api_key_env"]);
        assert!(!fields.contains("api_key"));
    }

    #[test]
    fn zero_conversion_timeout_is_rejected() {
        let config: RawConfig =
            toml::from_str("schema_version = 1\n[conversion]\ntimeout_ms = 0\n").unwrap();
        assert!(validate_raw(&config).is_err());
    }
}
#[test]
fn dotted_plugin_ids_remain_single_toml_keys() {
    let temporary = tempfile::tempdir().unwrap();
    let plugin = PluginConfig {
        source: "fixture.zip".to_owned(),
        sha256: Some("0".repeat(64)),
        protocol: "process-v1".to_owned(),
        enabled: true,
        signing_key_id: "publisher.test".to_owned(),
        signing_key_sha256: "1".repeat(64),
    };
    let path = scope_path(Scope::Project, temporary.path()).unwrap();
    let lock = lock_exact(&path).unwrap();
    compare_and_set_plugin_exact_locked(&lock, &path, "publisher.converter", None, Some(&plugin))
        .unwrap();
    assert_eq!(
        plugins_in_exact_locked(&lock, &path).unwrap().get("publisher.converter"),
        Some(&plugin)
    );
}

#[test]
fn pinned_config_cas_rejects_content_mutation_before_publish() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join(CONFIG_FILENAME);
    let original = PluginConfig {
        source: "old.zip".to_owned(),
        sha256: Some("0".repeat(64)),
        protocol: "process-v1".to_owned(),
        enabled: true,
        signing_key_id: "publisher.test".to_owned(),
        signing_key_sha256: "1".repeat(64),
    };
    let lock = lock_exact(&path).unwrap();
    compare_and_set_plugin_exact_locked(&lock, &path, "publisher.converter", None, Some(&original))
        .unwrap();
    let mut replacement = original.clone();
    replacement.enabled = false;
    let racer = b"schema_version = 1\n[plugins.racer]\nsource = \"racer.zip\"\nprotocol = \"process-v1\"\nenabled = true\n";
    fs::write(temporary.path().join(".test-config-mutate-after-locked-read"), racer).unwrap();

    assert!(
        compare_and_set_plugin_exact_locked(
            &lock,
            &path,
            "publisher.converter",
            Some(&original),
            Some(&replacement),
        )
        .is_err()
    );
    assert_eq!(fs::read(path).unwrap(), racer, "racer content was overwritten");
}

#[test]
fn physical_scope_source_does_not_treat_hash_as_a_profile_delimiter() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project#literal");
    fs::create_dir(&project).unwrap();
    set(Scope::Project, &project, "cli.jobs", "1").unwrap();
    let source = project_config_path(&project).unwrap();
    assert!(source_is_exact_scope(&source.to_string_lossy(), Scope::Project, &project).unwrap());
}
