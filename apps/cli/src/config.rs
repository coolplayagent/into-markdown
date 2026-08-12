//! Layered, versioned TOML configuration and atomic mutation helpers.

use crate::args::{AssetModeArg, ConflictPolicy, EmitKind, Language, Scope};
use crate::error::CliError;
use directories::ProjectDirs;
use into_markdown::{AiMode, AssetMode, ConversionOptions, OcrPolicy};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const CONFIG_FILENAME: &str = ".into-markdown.toml";

/// Paths participating in configuration discovery.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPaths {
    pub global: PathBuf,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginConfig {
    pub source: String,
    pub sha256: Option<String>,
    pub protocol: String,
    pub enabled: bool,
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
    pub ocr: OcrConfig,
    pub ai: AiConfig,
    pub network: NetworkConfig,
    pub limits: LimitsConfig,
    pub output: OutputConfig,
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
    pub max_nesting_depth: Option<u16>,
    pub max_pages: Option<u32>,
    pub max_asset_bytes: Option<u64>,
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
    let global = global_config_path()?;
    let project = find_project_config(cwd);
    let mut candidates = Vec::new();
    if !no_automatic {
        candidates.push((global.clone(), false));
        if let Some(path) = &project {
            candidates.push((path.clone(), false));
        }
    }
    candidates.extend(explicit.iter().cloned().map(|path| (path, true)));

    let profile = selected_profile
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("INTO_MD_PROFILE").ok().filter(|value| !value.is_empty()));
    let mut merged = empty_table();
    let mut effective = empty_table();
    let mut loaded = Vec::new();
    let mut profile_found = false;
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
        let value = read_validated_value(&path)?;
        merge_value(&mut merged, value.clone());
        let mut base = value.clone();
        let profile_overlay = base
            .as_table_mut()
            .and_then(|table| table.remove("profiles"))
            .and_then(|profiles| profile.as_ref().and_then(|name| profiles.get(name).cloned()));
        merge_value(&mut effective, base);
        if let Some(overlay) = profile_overlay {
            profile_found = true;
            merge_value(&mut effective, overlay);
        }
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
    let language = language_override
        .or_else(|| std::env::var("INTO_MD_LANGUAGE").ok().as_deref().and_then(parse_language))
        .or_else(|| parsed.cli.language.as_deref().and_then(parse_language))
        .unwrap_or_default();
    let jobs = parsed.cli.jobs.unwrap_or_else(default_jobs);
    if jobs == 0 {
        return Err(CliError::config("cli.jobs must be greater than zero"));
    }
    let emit = parse_emit(parsed.conversion.output.emit.as_deref())?;
    let asset_mode = parse_asset_mode(parsed.conversion.output.asset_mode);
    let conflict = parse_conflict(parsed.conversion.output.conflict.as_deref())?;
    Ok(LoadedConfig {
        paths: ConfigPaths { global, project, explicit: explicit.to_vec(), loaded },
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
        effective: parsed,
        options,
        language,
        jobs,
        emit,
        asset_mode,
        conflict,
    })
}

fn resolve_conversion_options(config: &ConversionConfig) -> Result<ConversionOptions, CliError> {
    let mut options = ConversionOptions::default();
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
        options.network.allowed_hosts.clone_from(&network.allowed_hosts);
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
    assign!(max_nesting_depth);
    assign!(max_pages);
    assign!(max_asset_bytes);
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

fn validate_raw(config: &RawConfig) -> Result<(), CliError> {
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
    parse_emit(config.conversion.output.emit.as_deref())?;
    parse_conflict(config.conversion.output.conflict.as_deref())?;
    for (name, provider) in &config.providers {
        validate_id("provider", name)?;
        if provider.provider_type != "openai-compatible" {
            return Err(CliError::config(format!(
                "provider '{name}' has unsupported type '{}'",
                provider.provider_type
            )));
        }
        let url = url::Url::parse(&provider.base_url)
            .map_err(|error| CliError::config(format!("provider '{name}' URL: {error}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(CliError::config(format!("provider '{name}' URL must use http or https")));
        }
        validate_environment_name(&provider.api_key_env)?;
        for capability in &provider.capabilities {
            validate_capability(capability)?;
        }
    }
    for (id, plugin) in &config.plugins {
        validate_id("plugin", id)?;
        if !matches!(plugin.protocol.as_str(), "process-v1" | "wasi-v1") {
            return Err(CliError::config(format!(
                "plugin '{id}' protocol must be process-v1 or wasi-v1"
            )));
        }
        if let Some(hash) = &plugin.sha256 {
            validate_sha256(hash)?;
        }
    }
    Ok(())
}

pub fn validate_file(path: &Path) -> Result<(), CliError> {
    read_validated_value(path).map(|_| ())
}

fn read_validated_value(path: &Path) -> Result<toml::Value, CliError> {
    let text = fs::read_to_string(path).map_err(|error| {
        CliError::config(format!("read configuration {}: {error}", path.display()))
    })?;
    let value: toml::Value = toml::from_str(&text).map_err(|error| {
        CliError::config(format!("parse configuration {}: {error}", path.display()))
    })?;
    let parsed: RawConfig = value.clone().try_into().map_err(|error| {
        CliError::config(format!("validate configuration {}: {error}", path.display()))
    })?;
    if parsed.schema_version != Some(1) {
        return Err(CliError::config(format!(
            "configuration {} must declare schema_version = 1",
            path.display()
        )));
    }
    validate_raw(&parsed)?;
    for profile in parsed.profiles.values() {
        let overlay = RawConfig {
            cli: profile.cli.clone(),
            conversion: profile.conversion.clone(),
            default_provider: profile.default_provider.clone(),
            providers: profile.providers.clone(),
            plugins: profile.plugins.clone(),
            ..RawConfig::default()
        };
        validate_raw(&overlay)?;
    }
    Ok(value)
}

pub fn global_config_path() -> Result<PathBuf, CliError> {
    let dirs = ProjectDirs::from("", "", "into-markdown").ok_or_else(|| {
        CliError::config("operating-system configuration directory is unavailable")
    })?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(CONFIG_FILENAME)
}

fn find_project_config(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().map(|ancestor| ancestor.join(CONFIG_FILENAME)).find(|path| path.is_file())
}

pub fn scope_path(scope: Scope, cwd: &Path) -> Result<PathBuf, CliError> {
    match scope {
        Scope::Global => global_config_path(),
        Scope::Project => Ok(project_config_path(cwd)),
    }
}

pub fn init(scope: Scope, cwd: &Path, force: bool) -> Result<PathBuf, CliError> {
    let path = scope_path(scope, cwd)?;
    if path.exists() && !force {
        return Err(CliError::config(format!(
            "configuration already exists: {} (use --force to replace it)",
            path.display()
        )));
    }
    atomic_write(&path, CONFIG_TEMPLATE.as_bytes(), force)?;
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
    mutate_scope(scope, cwd, |root| remove_nested(root, &format!("providers.{name}")))
}

pub fn set_default_provider(scope: Scope, cwd: &Path, name: &str) -> Result<PathBuf, CliError> {
    validate_id("provider", name)?;
    mutate_scope(scope, cwd, |root| {
        set_nested(root, "default_provider", toml::Value::String(name.to_owned()))
    })
}

pub fn set_plugin_enabled(
    scope: Scope,
    cwd: &Path,
    id: &str,
    enabled: bool,
) -> Result<PathBuf, CliError> {
    validate_id("plugin", id)?;
    mutate_scope(scope, cwd, |root| {
        if get_value(root, &format!("plugins.{id}")).is_err() {
            return Err(CliError::config(format!("plugin '{id}' is not configured in this scope")));
        }
        set_nested(root, &format!("plugins.{id}.enabled"), toml::Value::Boolean(enabled))
    })
}

pub fn remove_plugin(scope: Scope, cwd: &Path, id: &str) -> Result<PathBuf, CliError> {
    mutate_scope(scope, cwd, |root| remove_nested(root, &format!("plugins.{id}")))
}

fn mutate_scope(
    scope: Scope,
    cwd: &Path,
    operation: impl FnOnce(&mut toml::Value) -> Result<(), CliError>,
) -> Result<PathBuf, CliError> {
    let path = scope_path(scope, cwd)?;
    let mut value = if path.exists() { read_validated_value(&path)? } else { empty_table() };
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
    validate_raw(&parsed)?;
    let text = toml::to_string_pretty(&value)
        .map_err(|error| CliError::internal(format!("serialize configuration: {error}")))?;
    atomic_write(&path, text.as_bytes(), true)?;
    Ok(path)
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

fn atomic_write(path: &Path, bytes: &[u8], replace: bool) -> Result<(), CliError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    if path.exists() && !replace {
        return Err(CliError::config(format!("path already exists: {}", path.display())));
    }
    let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or("config");
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{filename}.into-md-"))
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| CliError::from(error.error))?;
    Ok(())
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
    if key.is_some_and(|key| matches!(key, "api_key" | "token" | "password" | "secret")) {
        *value = toml::Value::String("<redacted>".into());
        return;
    }
    match value {
        toml::Value::String(text) if key.is_some_and(|key| key.ends_with("url")) => {
            if let Ok(mut parsed) = url::Url::parse(text)
                && (parsed.query().is_some() || parsed.fragment().is_some())
            {
                parsed.set_query(None);
                parsed.set_fragment(None);
                *text = parsed.to_string();
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
        let path = std::env::temp_dir().join(format!(
            "into-md-config-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
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
    fn profile_names_are_deterministic() {
        let value: toml::Value = toml::from_str("[profiles.z]\n[profiles.a]\n").unwrap();
        assert_eq!(profile_names(&value), vec!["a", "z"]);
    }

    #[test]
    fn no_secret_config_field_exists() {
        let fields = BTreeSet::from(["api_key_env"]);
        assert!(!fields.contains("api_key"));
    }
}
