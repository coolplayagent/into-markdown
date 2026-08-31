//! Shared, bounded administration DTOs used by the local Web console.
//!
//! This module deliberately calls the same catalog, configuration and capability
//! services as the CLI.  It never shells out to `into-md`, reads API-key
//! values, or returns an unredacted configuration value.

mod config_keys;
use config_keys::admin_config_key_allowed;

use crate::args::Scope;
use crate::config::{self, LoadedConfig};
use crate::error::{CliError, ExitClass};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const MAX_ACTION_TEXT: usize = 4096;
/// Shared wire limit for administration snapshots and successful action results.
pub(crate) const MAX_ADMIN_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct AdminConfigContext {
    pub explicit: Vec<PathBuf>,
    pub no_automatic: bool,
    pub profile: Option<String>,
    pub language: Option<crate::args::Language>,
}

impl AdminConfigContext {
    fn selected_profile(&self) -> Option<String> {
        self.profile
            .clone()
            .or_else(|| std::env::var("INTO_MD_PROFILE").ok().filter(|value| !value.is_empty()))
    }

    pub(crate) fn is_default(&self) -> bool {
        !self.no_automatic && self.explicit.is_empty() && self.selected_profile().is_none()
    }

    fn load(&self, cwd: &Path) -> Result<LoadedConfig, CliError> {
        config::load(cwd, &self.explicit, self.no_automatic, self.profile.as_deref(), self.language)
    }

    fn cli_arguments(&self, mut command: Vec<OsString>) -> Vec<OsString> {
        let mut arguments = Vec::new();
        for path in &self.explicit {
            arguments.extend([OsString::from("--config"), path.as_os_str().to_owned()]);
        }
        if self.no_automatic {
            arguments.push(OsString::from("--no-config"));
        }
        if let Some(profile) = &self.profile {
            arguments.extend([OsString::from("--profile"), OsString::from(profile)]);
        }
        if let Some(language) = self.language {
            arguments.extend([
                OsString::from("--language"),
                OsString::from(match language {
                    crate::args::Language::En => "en",
                    crate::args::Language::ZhCn => "zh-CN",
                }),
            ]);
        }
        arguments.append(&mut command);
        arguments
    }

    fn ensure_mutation_allowed(&self) -> Result<(), CliError> {
        if self.is_default() {
            Ok::<(), CliError>(())
        } else {
            Err(CliError::new(
                ExitClass::Policy,
                "adminConfigContextReadOnly",
                "administration mutations require automatic unprofiled configuration authority",
            ))
        }
    }
}

fn run_admin_cli(
    context: &AdminConfigContext,
    cwd: &Path,
    command: Vec<OsString>,
    test_user_data_anchor: Option<&Path>,
) -> Result<String, CliError> {
    crate::app::run_admin_cli_arguments(cwd, context.cli_arguments(command), test_user_data_anchor)
}

fn run_plugin_command(
    context: &AdminConfigContext,
    cwd: &Path,
    scope: Scope,
    operation: &str,
    id: &str,
    test_user_data_anchor: Option<&Path>,
) -> Result<String, CliError> {
    let mut command = vec![OsString::from("plugins"), OsString::from(operation)];
    if !id.is_empty() {
        command.push(OsString::from(id));
    }
    command.extend([
        OsString::from("--scope"),
        OsString::from(if scope == Scope::Global { "global" } else { "project" }),
    ]);
    if operation == "verify" {
        command.push(OsString::from("--json"));
    }
    run_admin_cli(context, cwd, command, test_user_data_anchor)
}

fn effective_plugin_scope(loaded: &LoadedConfig, cwd: &Path, id: &str) -> Result<Scope, CliError> {
    crate::app::admin_effective_plugin_scope(loaded, cwd, id)
}

fn verify_effective_plugin(
    loaded: &LoadedConfig,
    cwd: &Path,
    id: &str,
) -> Result<into_markdown_plugin_manager::InstalledPlugin, CliError> {
    crate::app::verify_admin_effective_plugin_from_loaded(loaded, cwd, id)
}

fn inspect_effective_plugin(
    loaded: &LoadedConfig,
    cwd: &Path,
    id: &str,
) -> Result<into_markdown_plugin_manager::InstalledPlugin, CliError> {
    crate::app::inspect_admin_effective_plugin_from_loaded(loaded, cwd, id)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSnapshot {
    pub schema_version: u32,
    pub formats: Vec<FormatDto>,
    pub capabilities: Vec<crate::app::CapabilityView>,
    pub providers: Vec<ProviderDto>,
    pub plugins: Vec<PluginDto>,
    pub configuration: serde_json::Value,
    pub profiles: Vec<ProfileDto>,
    pub doctor: Vec<DoctorDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_result: Option<AdminOperationResult>,
    pub configuration_read_only: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatDto {
    pub format: String,
    pub family: String,
    pub status: String,
    pub source: String,
    pub extensions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDto {
    pub name: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub models: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_set: Option<bool>,
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub allowed_hosts: Vec<String>,
    pub allow_private_network: bool,
    pub default: bool,
    pub effective: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDto {
    pub id: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub effective: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_key_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorDto {
    pub id: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDto {
    pub name: String,
    pub scope: String,
    pub effective: bool,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AdminOperationResult {
    Detection {
        source_name: Option<String>,
        source_size: u64,
        candidates: Vec<DetectionCandidateDto>,
    },
    Profile {
        name: String,
        value: serde_json::Map<String, serde_json::Value>,
    },
    Config {
        operation: String,
        value: serde_json::Value,
    },
    Doctor {
        checks: Vec<DoctorDto>,
    },
    ProviderTest {
        configured_model_available: bool,
        model_count: usize,
        capabilities: Vec<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DetectionCandidateDto {
    format: String,
    confidence: f32,
    explicit: bool,
    detector_id: String,
    reason: String,
    diagnostics: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DetectionOutput {
    source_name: Option<String>,
    source_size: u64,
    candidates: Vec<DetectionCandidateDto>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
// These independent booleans are the bounded, versioned HTTP wire schema. Collapsing them into
// enums would make valid one-shot authority combinations mutually exclusive.
#[allow(clippy::struct_excessive_bools)]
pub struct AdminAction {
    pub schema_version: u32,
    pub action: String,
    #[serde(default)]
    pub scope: ActionScope,
    pub target: Option<String>,
    pub value: Option<String>,
    pub source: Option<String>,
    pub sha256: Option<String>,
    pub signing_key_id: Option<String>,
    pub signing_key_sha256: Option<String>,
    pub provider_type: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub timeout_ms: Option<u64>,
    pub charset: Option<String>,
    pub format_hint: Option<String>,
    pub extension: Option<String>,
    pub mime_type: Option<String>,
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    #[serde(default)]
    pub allow_private_network: bool,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub resolved: bool,
    pub from: Option<String>,
    pub authorization_grant: Option<String>,
    #[serde(default)]
    pub authorize_dangerous: bool,
    #[serde(default)]
    pub authorize_network: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ActionScope {
    #[default]
    Global,
    Project,
}

impl From<ActionScope> for Scope {
    fn from(value: ActionScope) -> Self {
        match value {
            ActionScope::Global => Self::Global,
            ActionScope::Project => Self::Project,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminActionResult {
    pub schema_version: u32,
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_result: Option<AdminOperationResult>,
}

#[cfg(test)]
fn snapshot(
    cwd: &Path,
    context: &AdminConfigContext,
    test_user_data_anchor: Option<&Path>,
) -> Result<AdminSnapshot, CliError> {
    snapshot_with_doctor(cwd, context, test_user_data_anchor, true)
}

pub fn snapshot_with_doctor(
    cwd: &Path,
    context: &AdminConfigContext,
    test_user_data_anchor: Option<&Path>,
    include_doctor: bool,
) -> Result<AdminSnapshot, CliError> {
    crate::app::with_admin_authority(test_user_data_anchor, || {
        if context.is_default() {
            crate::app::recover_plugins_before_config_load(cwd, false)?;
        }
        snapshot_consistent(cwd, context, include_doctor)
    })
}

fn snapshot_consistent(
    cwd: &Path,
    context: &AdminConfigContext,
    include_doctor: bool,
) -> Result<AdminSnapshot, CliError> {
    for _ in 0..2 {
        let exact_automatic = context.is_default();
        let global_generation =
            exact_automatic.then(|| config::scope_snapshot(Scope::Global, cwd)).transpose()?;
        let project_generation =
            exact_automatic.then(|| config::scope_snapshot(Scope::Project, cwd)).transpose()?;
        let loaded = context.load(cwd)?;
        let loaded_paths = config::loaded_paths_snapshot(&loaded.paths.loaded)?;
        let global_document = global_generation.as_ref().map_or_else(
            || toml::Value::Table(toml::map::Map::new()),
            |value| value.document.clone(),
        );
        let project_document = project_generation.as_ref().map_or_else(
            || toml::Value::Table(toml::map::Map::new()),
            |value| value.document.clone(),
        );
        let global_raw: config::RawConfig = global_document.try_into().map_err(|error| {
            CliError::config(format!("global configuration is invalid: {error}"))
        })?;
        let project_raw: config::RawConfig = project_document.try_into().map_err(|error| {
            CliError::config(format!("project configuration is invalid: {error}"))
        })?;
        let global_profile_names = global_raw.profiles.keys().cloned().collect();
        let project_profile_names = project_raw.profiles.keys().cloned().collect();
        let result = snapshot_loaded(
            cwd,
            context,
            &loaded,
            &SnapshotLayers {
                global_plugins: &global_raw.plugins,
                project_plugins: &project_raw.plugins,
                global_providers: &global_raw.providers,
                project_providers: &project_raw.providers,
                global_profiles: &global_profile_names,
                project_profiles: &project_profile_names,
                synthetic_context: !exact_automatic,
            },
            include_doctor,
        )?;
        let loaded_after = context.load(cwd)?;
        let exact_unchanged = if exact_automatic {
            global_generation.as_ref() == Some(&config::scope_snapshot(Scope::Global, cwd)?)
                && project_generation.as_ref()
                    == Some(&config::scope_snapshot(Scope::Project, cwd)?)
        } else {
            true
        };
        if exact_unchanged
            && loaded.paths.loaded == loaded_after.paths.loaded
            && loaded_paths == config::loaded_paths_snapshot(&loaded.paths.loaded)?
            && loaded.merged == loaded_after.merged
        {
            return Ok(result);
        }
    }
    Err(CliError::new(
        ExitClass::Policy,
        "storeChanged",
        "plugin configuration changed while the administration snapshot was verified",
    ))
}

struct SnapshotLayers<'a> {
    global_plugins: &'a std::collections::BTreeMap<String, config::PluginConfig>,
    project_plugins: &'a std::collections::BTreeMap<String, config::PluginConfig>,
    global_providers: &'a std::collections::BTreeMap<String, config::ProviderConfig>,
    project_providers: &'a std::collections::BTreeMap<String, config::ProviderConfig>,
    global_profiles: &'a std::collections::BTreeSet<String>,
    project_profiles: &'a std::collections::BTreeSet<String>,
    synthetic_context: bool,
}

// Keeping construction in one pass makes the returned DTO use one captured layer authority;
// helpers perform the security-sensitive loading and verification outside this assembler.
#[allow(clippy::too_many_lines)]
fn snapshot_loaded(
    cwd: &Path,
    context: &AdminConfigContext,
    loaded: &LoadedConfig,
    layers: &SnapshotLayers<'_>,
    include_doctor: bool,
) -> Result<AdminSnapshot, CliError> {
    let global_plugins = layers.global_plugins;
    let project_plugins = layers.project_plugins;
    let global_providers = layers.global_providers;
    let project_providers = layers.project_providers;
    let global_profiles = layers.global_profiles;
    let project_profiles = layers.project_profiles;
    let synthetic_context = layers.synthetic_context;
    let formats = into_markdown::format_catalog()
        .iter()
        .map(|entry| FormatDto {
            format: entry.descriptor.format.as_str().into(),
            family: entry.descriptor.family.into(),
            status: entry.descriptor.status.as_str().into(),
            source: entry.source.as_str().into(),
            extensions: entry.descriptor.extensions.iter().map(|value| (*value).into()).collect(),
            runtime_component: entry.runtime.map(|value| value.component.into()),
            install_hint: entry.runtime.map(|value| value.install_hint.into()),
        })
        .collect();
    let capabilities = crate::app::capability_views(loaded, cwd)?;
    let mut providers = Vec::new();
    for (scope_name, scoped_providers) in
        [("global", global_providers), ("project", project_providers)]
    {
        for (name, provider) in scoped_providers {
            providers.push(ProviderDto {
                name: name.clone(),
                scope: scope_name.into(),
                action_scope: Some(scope_name.into()),
                provider_type: (!provider.provider_type.is_empty())
                    .then(|| provider.provider_type.clone()),
                base_url: (!provider.base_url.is_empty()).then(|| redact_url(&provider.base_url)),
                model: (!provider.model.is_empty()).then(|| provider.model.clone()),
                models: provider.models.clone(),
                api_key_env: (!provider.api_key_env.is_empty())
                    .then(|| provider.api_key_env.clone()),
                environment_set: (!provider.api_key_env.is_empty())
                    .then(|| std::env::var_os(&provider.api_key_env).is_some()),
                capabilities: provider.capabilities.clone(),
                timeout_ms: provider.timeout_ms,
                allowed_hosts: provider.allowed_hosts.clone(),
                allow_private_network: provider.allow_private_network,
                default: false,
                effective: false,
                shadowed_by: Some("effective".into()),
            });
        }
    }
    for (name, provider) in &loaded.effective.providers {
        providers.push(ProviderDto {
            name: name.clone(),
            scope: "effective".into(),
            action_scope: (!synthetic_context).then(|| "project".into()),
            provider_type: Some(provider.provider_type.clone()),
            base_url: Some(redact_url(&provider.base_url)),
            model: Some(provider.model.clone()),
            models: provider.models.clone(),
            api_key_env: Some(provider.api_key_env.clone()),
            environment_set: Some(std::env::var_os(&provider.api_key_env).is_some()),
            capabilities: provider.capabilities.clone(),
            timeout_ms: provider.timeout_ms,
            allowed_hosts: provider.allowed_hosts.clone(),
            allow_private_network: provider.allow_private_network,
            default: loaded.effective.default_provider.as_deref() == Some(name),
            effective: true,
            shadowed_by: None,
        });
    }
    let mut plugins = Vec::new();
    for (scope_name, scoped_plugins) in [("global", global_plugins), ("project", project_plugins)] {
        for (id, plugin) in scoped_plugins {
            if id == "official.ocr.ppocrv6" && crate::embedded_runtime::enabled() {
                continue;
            }
            let package_scope = effective_plugin_scope(loaded, cwd, id)?;
            plugins.push(PluginDto {
                id: id.clone(),
                scope: scope_name.into(),
                action_scope: Some(scope_name.into()),
                package_scope: Some(
                    if package_scope == Scope::Global { "global" } else { "project" }.into(),
                ),
                source: (!plugin.source.is_empty()).then(|| redact_url(&plugin.source)),
                sha256: plugin.sha256.clone(),
                protocol: (!plugin.protocol.is_empty()).then(|| plugin.protocol.clone()),
                enabled: Some(plugin.enabled),
                effective: false,
                shadowed_by: Some("effective".into()),
                verification: None,
                version: None,
                signing_key_id: (!plugin.signing_key_id.is_empty())
                    .then(|| plugin.signing_key_id.clone()),
                signing_key_sha256: (!plugin.signing_key_sha256.is_empty())
                    .then(|| plugin.signing_key_sha256.clone()),
                target: None,
            });
        }
    }
    for (id, plugin) in &loaded.effective.plugins {
        if id == "official.ocr.ppocrv6" && crate::embedded_runtime::enabled() {
            continue;
        }
        let package_scope =
            (!synthetic_context).then(|| effective_plugin_scope(loaded, cwd, id)).transpose()?;
        let official_capability = match id.as_str() {
            "official.ocr.ppocrv6" => "ocr",
            "official.media.whisper" => "transcription",
            _ => "",
        };
        let cached_official = capabilities.iter().find(|entry| entry.id == official_capability);
        let (verification, version) = if synthetic_context {
            ("adminConfigContextReadOnly".to_owned(), None)
        } else if let Some(capability) = cached_official {
            (
                match capability.local_status.as_str() {
                    "ready" => "metadataAuthenticated",
                    "disabled" => "disabled",
                    "incompatible" => "pluginUnsupportedTarget",
                    "not-installed" => "pluginNotInstalled",
                    _ => "pluginIntegrity",
                }
                .to_owned(),
                capability.local_version.clone(),
            )
        } else {
            match inspect_effective_plugin(loaded, cwd, id) {
                Ok(installed) => ("metadataAuthenticated".to_owned(), Some(installed.version)),
                Err(error) => (error.code().to_owned(), None),
            }
        };
        plugins.push(PluginDto {
            id: id.clone(),
            scope: "effective".into(),
            action_scope: package_scope
                .map(|scope| if scope == Scope::Global { "global" } else { "project" }.into()),
            package_scope: package_scope
                .map(|scope| if scope == Scope::Global { "global" } else { "project" }.into()),
            source: Some(redact_url(&plugin.source)),
            sha256: plugin.sha256.clone(),
            protocol: Some(plugin.protocol.clone()),
            enabled: Some(plugin.enabled),
            effective: true,
            shadowed_by: None,
            verification: Some(verification),
            version,
            signing_key_id: Some(plugin.signing_key_id.clone()),
            signing_key_sha256: Some(plugin.signing_key_sha256.clone()),
            target: Some(crate::app::admin_plugin_target().into()),
        });
    }
    if providers.len() > 128 || plugins.len() > 256 {
        return Err(CliError::new(
            ExitClass::Policy,
            "resourceLimit",
            "administration snapshot exceeds its bounded collection limit",
        ));
    }
    let configuration = serde_json::to_value(loaded.display_value(true)?).map_err(|error| {
        CliError::internal(format!("serialize redacted configuration: {error}"))
    })?;
    let mut profiles = Vec::new();
    for (scope, scope_name, scoped_profiles) in
        [(Scope::Global, "global", global_profiles), (Scope::Project, "project", project_profiles)]
    {
        for name in scoped_profiles {
            let effective = scope == Scope::Project || !project_profiles.contains(name);
            profiles.push(ProfileDto {
                name: name.clone(),
                scope: scope_name.into(),
                effective,
                active: effective && context.profile.as_deref() == Some(name),
                shadowed_by: (!effective).then(|| "project".into()),
            });
        }
    }
    if let Some(name) = context.selected_profile()
        && !profiles.iter().any(|profile| profile.name == *name && profile.active)
    {
        profiles.push(ProfileDto {
            name,
            scope: "effective".into(),
            effective: true,
            active: true,
            shadowed_by: None,
        });
    }
    if profiles.len() > 128 {
        return Err(CliError::new(
            ExitClass::Policy,
            "resourceLimit",
            "administration snapshot exceeds its profile limit",
        ));
    }
    // Initial navigation authenticates lightweight package metadata. The
    // explicit doctor and verify actions perform full payload hashing.
    let doctor = if include_doctor {
        doctor_checks(cwd, loaded, false, synthetic_context)
    } else {
        Vec::new()
    };
    let snapshot = AdminSnapshot {
        schema_version: 1,
        formats,
        capabilities,
        providers,
        plugins,
        configuration,
        profiles,
        doctor,
        operation_result: None,
        configuration_read_only: !context.is_default(),
    };
    ensure_admin_response_size(&snapshot, "administration snapshot")?;
    Ok(snapshot)
}

pub fn apply(
    cwd: &Path,
    context: &AdminConfigContext,
    action: &AdminAction,
    test_user_data_anchor: Option<&Path>,
) -> Result<AdminActionResult, CliError> {
    crate::app::with_admin_authority(test_user_data_anchor, || {
        if context.is_default() {
            crate::app::recover_plugins_before_config_load(cwd, false)?;
        }
        apply_inner(cwd, context, action, test_user_data_anchor)
    })
}

#[allow(clippy::too_many_lines)]
fn apply_inner(
    cwd: &Path,
    context: &AdminConfigContext,
    action: &AdminAction,
    test_user_data_anchor: Option<&Path>,
) -> Result<AdminActionResult, CliError> {
    if action.schema_version != 1 {
        return Err(CliError::usage("unsupported administration request schema"));
    }
    for text in [
        action.target.as_deref(),
        action.value.as_deref(),
        action.source.as_deref(),
        action.model.as_deref(),
        action.api_key_env.as_deref(),
        action.from.as_deref(),
        action.charset.as_deref(),
        action.format_hint.as_deref(),
        action.extension.as_deref(),
        action.mime_type.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if text.len() > MAX_ACTION_TEXT || text.contains('\0') {
            return Err(CliError::usage("administration request value is invalid or oversized"));
        }
    }
    if action.capabilities.len() > 64
        || action.capabilities.iter().any(|value| value.len() > 64 || value.contains('\0'))
        || action.allow_hosts.len() > 64
        || action.allow_hosts.iter().any(|value| value.len() > 253 || value.contains('\0'))
    {
        return Err(CliError::usage("administration capability list is invalid or oversized"));
    }
    let target = action.target.as_deref().unwrap_or("");
    let normalized_target = target.to_ascii_lowercase().replace('-', "_");
    if action.action == "config.set"
        && ["api_key", "secret", "password", "token"].iter().any(|needle| {
            normalized_target.contains(needle) && !normalized_target.ends_with("api_key_env")
        })
    {
        return Err(policy("plaintextSecretRejected"));
    }
    let scope = action.scope.into();
    if action.action.starts_with("plugin.")
        || action.action.starts_with("provider.")
        || action.action.starts_with("config.")
        || action.action.starts_with("profile.")
        || action.action.starts_with("capability.")
    {
        context.ensure_mutation_allowed()?;
    }
    let mut operation_result = None;
    match action.action.as_str() {
        "format.detect" => {
            if (!action.allow_hosts.is_empty() || action.allow_private_network)
                && !action.authorize_network
            {
                return Err(policy("networkAuthorizationRequired"));
            }
            let source = action
                .source
                .as_deref()
                .ok_or_else(|| CliError::usage("format.detect requires a local source path"))?;
            let mut arguments = vec![
                OsString::from("formats"),
                OsString::from("detect"),
                OsString::from(source),
                OsString::from("--json"),
            ];
            if let Some(charset) = &action.charset {
                arguments.extend([OsString::from("--charset"), OsString::from(charset)]);
            }
            for (flag, value) in [
                ("--format", action.format_hint.as_deref()),
                ("--extension", action.extension.as_deref()),
                ("--mime-type", action.mime_type.as_deref()),
            ] {
                if let Some(value) = value {
                    arguments.extend([OsString::from(flag), OsString::from(value)]);
                }
            }
            if action.authorize_network {
                arguments.push(OsString::from("--allow-network"));
            }
            if action.allow_private_network {
                require_dangerous(action)?;
                arguments.push(OsString::from("--allow-private-network"));
            }
            for host in &action.allow_hosts {
                arguments.extend([OsString::from("--allow-host"), OsString::from(host)]);
            }
            let output = run_admin_cli(context, cwd, arguments, test_user_data_anchor)?;
            let detected: DetectionOutput = serde_json::from_str(&output).map_err(|error| {
                CliError::internal(format!("parse format detection result: {error}"))
            })?;
            validate_detection(&detected)?;
            operation_result = Some(AdminOperationResult::Detection {
                source_name: detected.source_name,
                source_size: detected.source_size,
                candidates: detected.candidates,
            });
        }
        "capability.install" => {
            if target == "ocr" && crate::embedded_runtime::enabled() {
                return Err(built_in_ocr_action_error("installed"));
            } else {
                if !action.authorize_network {
                    return Err(policy("networkAuthorizationRequired"));
                }
                require_dangerous(action)?;
                let setup_target = match target {
                    "ocr" => "ocr",
                    "transcription" | "diarization" | "media" => "media",
                    _ => return Err(CliError::usage("unknown capability")),
                };
                let command = vec![OsString::from("setup"), OsString::from(setup_target)];
                run_admin_cli(context, cwd, command, test_user_data_anchor)?;
            }
        }
        "capability.use" => {
            require_dangerous(action)?;
            let source = action
                .source
                .as_deref()
                .or(action.value.as_deref())
                .ok_or_else(|| CliError::usage("capability.use requires a source"))?;
            let loaded = context.load(cwd)?;
            let config_source = crate::app::capability_config_source(target, source, &loaded)?;
            config::set_capability_source(scope, cwd, target, &config_source)?;
        }
        "capability.verify" => {
            if target == "ocr" && crate::embedded_runtime::enabled() {
                return Err(built_in_ocr_action_error("verified as a plugin"));
            } else {
                let loaded = context.load(cwd)?;
                let plugin_id = capability_plugin_id(target)?;
                verify_effective_plugin(&loaded, cwd, plugin_id)?;
            }
        }
        "capability.remove" => {
            require_dangerous(action)?;
            if target == "ocr" && crate::embedded_runtime::enabled() {
                return Err(built_in_ocr_action_error("removed"));
            }
            let plugin_id = capability_plugin_id(target)?;
            run_plugin_command(context, cwd, scope, "remove", plugin_id, test_user_data_anchor)?;
        }
        "plugin.enable" | "plugin.disable" => {
            if target == "official.ocr.ppocrv6" && crate::embedded_runtime::enabled() {
                return Err(built_in_ocr_action_error("managed as a plugin"));
            }
            if action.action == "plugin.enable" {
                require_dangerous(action)?;
            }
            run_plugin_command(
                context,
                cwd,
                scope,
                if action.action == "plugin.enable" { "enable" } else { "disable" },
                target,
                test_user_data_anchor,
            )?;
        }
        "plugin.install" => {
            require_dangerous(action)?;
            let source = action
                .source
                .as_deref()
                .ok_or_else(|| CliError::usage("plugin.install requires a package source"))?;
            if source.starts_with("https://") && !action.authorize_network {
                return Err(policy("networkAuthorizationRequired"));
            }
            let mut command =
                vec![OsString::from("plugins"), OsString::from("install"), OsString::from(source)];
            for (flag, value) in [
                ("--sha256", action.sha256.as_deref()),
                ("--signing-key-id", action.signing_key_id.as_deref()),
                ("--signing-key-sha256", action.signing_key_sha256.as_deref()),
            ] {
                if let Some(value) = value {
                    command.extend([OsString::from(flag), OsString::from(value)]);
                }
            }
            command.extend([
                OsString::from("--scope"),
                OsString::from(if scope == Scope::Global { "global" } else { "project" }),
            ]);
            run_admin_cli(context, cwd, command, test_user_data_anchor)?;
        }
        "plugin.verify" => {
            if target == "official.ocr.ppocrv6" && crate::embedded_runtime::enabled() {
                return Err(built_in_ocr_action_error("verified as a plugin"));
            }
            let global_before = config::scope_snapshot(Scope::Global, cwd)?;
            let project_before = config::scope_snapshot(Scope::Project, cwd)?;
            let loaded = context.load(cwd)?;
            verify_effective_plugin(&loaded, cwd, target)?;
            if global_before != config::scope_snapshot(Scope::Global, cwd)?
                || project_before != config::scope_snapshot(Scope::Project, cwd)?
            {
                return Err(CliError::new(
                    ExitClass::Policy,
                    "storeChanged",
                    "plugin configuration changed while the effective package was verified",
                ));
            }
        }
        "plugin.remove" => {
            require_dangerous(action)?;
            if target == "official.ocr.ppocrv6" && crate::embedded_runtime::enabled() {
                return Err(built_in_ocr_action_error("removed"));
            }
            run_plugin_command(context, cwd, scope, "remove", target, test_user_data_anchor)?;
        }
        "config.set" => {
            require_dangerous(action)?;
            if !admin_config_key_allowed(target) {
                return Err(policy("adminConfigKeyDenied"));
            }
            config::set(scope, cwd, target, action.value.as_deref().unwrap_or(""))?;
        }
        "config.paths" => {
            let output = run_admin_cli(
                context,
                cwd,
                vec![OsString::from("config"), OsString::from("paths"), OsString::from("--json")],
                test_user_data_anchor,
            )?;
            let value = serde_json::from_str(&output)
                .map_err(|error| CliError::internal(format!("parse config paths: {error}")))?;
            operation_result =
                Some(AdminOperationResult::Config { operation: "paths".into(), value });
        }
        "config.get" => {
            let loaded = context.load(cwd)?;
            let redacted = loaded.display_value(false)?;
            let value = target.split('.').try_fold(&redacted, |current, segment| {
                current.get(segment).ok_or_else(|| CliError::usage("configuration key not found"))
            })?;
            operation_result = Some(AdminOperationResult::Config {
                operation: "get".into(),
                value: serde_json::to_value(value).map_err(|error| {
                    CliError::internal(format!("serialize redacted config value: {error}"))
                })?,
            });
        }
        "config.show" => {
            let mut command = vec![
                OsString::from("config"),
                OsString::from("show"),
                OsString::from("--format"),
                OsString::from("json"),
            ];
            if action.resolved {
                command.push(OsString::from("--resolved"));
            }
            let output = run_admin_cli(context, cwd, command, test_user_data_anchor)?;
            let value = serde_json::from_str(&output)
                .map_err(|error| CliError::internal(format!("parse config show: {error}")))?;
            operation_result = Some(AdminOperationResult::Config {
                operation: if action.resolved { "showResolved" } else { "showMerged" }.into(),
                value,
            });
        }
        "config.validate" => {
            let mut command = vec![OsString::from("config"), OsString::from("validate")];
            if let Some(path) = &action.source {
                command.push(OsString::from(path));
            }
            run_admin_cli(context, cwd, command, test_user_data_anchor)?;
        }
        "config.init" => {
            require_dangerous(action)?;
            let mut command = vec![
                OsString::from("config"),
                OsString::from("init"),
                OsString::from("--scope"),
                OsString::from(if scope == Scope::Global { "global" } else { "project" }),
            ];
            if action.force {
                command.push(OsString::from("--force"));
            }
            run_admin_cli(context, cwd, command, test_user_data_anchor)?;
        }
        "config.unset" => {
            require_dangerous(action)?;
            if !admin_config_key_allowed(target) {
                return Err(policy("adminConfigKeyDenied"));
            }
            config::unset(scope, cwd, target)?;
        }
        "profile.create" => {
            require_dangerous(action)?;
            let loaded = context.load(cwd)?;
            config::create_profile(scope, cwd, target, action.from.as_deref(), &loaded.merged)?;
        }
        "profile.remove" => {
            require_dangerous(action)?;
            config::remove_profile(scope, cwd, target)?;
        }
        "provider.test" => {
            if !action.authorize_network {
                return Err(policy("networkAuthorizationRequired"));
            }
            let loaded = context.load(cwd)?;
            let provider = loaded
                .effective
                .providers
                .get(target)
                .ok_or_else(|| CliError::usage(format!("unknown provider '{target}'")))?;
            if provider.allow_private_network {
                require_dangerous(action)?;
            }
            let result = crate::app::test_provider(
                &loaded,
                &crate::args::ProviderTestArgs {
                    name: target.into(),
                    allow_network: true,
                    allow_private_network: provider.allow_private_network,
                    allow_host: Vec::new(),
                },
            )?;
            if result.model_count > 10_000
                || result.capabilities.len() > 64
                || result.capabilities.iter().any(|value| value.len() > 64)
            {
                return Err(CliError::new(
                    ExitClass::Policy,
                    "resourceLimit",
                    "provider test result exceeds its bounded DTO limits",
                ));
            }
            operation_result = Some(AdminOperationResult::ProviderTest {
                configured_model_available: result.configured_model_available,
                model_count: result.model_count,
                capabilities: result.capabilities,
            });
        }
        "provider.add" => {
            require_dangerous(action)?;
            if action.timeout_ms.is_some_and(|value| value == 0 || value > 86_400_000) {
                return Err(CliError::usage("provider timeout must be 1..=86400000 milliseconds"));
            }
            let base_url = action
                .source
                .as_deref()
                .ok_or_else(|| CliError::usage("provider.add requires a base URL"))?;
            let model = action
                .model
                .as_deref()
                .ok_or_else(|| CliError::usage("provider.add requires a model"))?;
            let api_key_env = action
                .api_key_env
                .as_deref()
                .ok_or_else(|| CliError::usage("provider.add requires an environment name"))?;
            let provider_type = action.provider_type.as_deref().unwrap_or("openai-compatible");
            let mut arguments = vec![
                OsString::from("providers"),
                OsString::from("add"),
                OsString::from(target),
                OsString::from("--type"),
                OsString::from(provider_type),
                OsString::from("--base-url"),
                OsString::from(base_url),
                OsString::from("--model"),
                OsString::from(model),
                OsString::from("--api-key-env"),
                OsString::from(api_key_env),
            ];
            for capability in &action.capabilities {
                arguments.push(OsString::from("--capability"));
                arguments.push(OsString::from(capability));
            }
            for (capability, model) in &action.models {
                arguments.push(OsString::from("--model-map"));
                arguments.push(OsString::from(format!("{capability}={model}")));
            }
            if let Some(timeout_ms) = action.timeout_ms {
                arguments.extend([
                    OsString::from("--timeout"),
                    OsString::from(format!("{timeout_ms}ms")),
                ]);
            }
            for host in &action.allow_hosts {
                arguments.push(OsString::from("--allow-host"));
                arguments.push(OsString::from(host));
            }
            if action.allow_private_network {
                arguments.push(OsString::from("--allow-private-network"));
            }
            arguments.extend([
                OsString::from("--scope"),
                OsString::from(if scope == Scope::Global { "global" } else { "project" }),
            ]);
            run_admin_cli(context, cwd, arguments, test_user_data_anchor)?;
        }
        "provider.remove" => {
            require_dangerous(action)?;
            config::remove_provider(scope, cwd, target)?;
        }
        "provider.set-default" => {
            require_dangerous(action)?;
            run_admin_cli(
                context,
                cwd,
                vec![
                    OsString::from("providers"),
                    OsString::from("set-default"),
                    OsString::from(target),
                    OsString::from("--scope"),
                    OsString::from(if scope == Scope::Global { "global" } else { "project" }),
                ],
                test_user_data_anchor,
            )?;
        }
        "profile.show" => {
            let profile = config::profiles_in_scope(scope, cwd)?
                .remove(target)
                .ok_or_else(|| CliError::usage("unknown configuration profile"))?;
            operation_result = Some(profile_operation_result(target, &profile)?);
        }
        "doctor.run" => {
            if action.allow_private_network && !action.authorize_network {
                return Err(policy("networkAuthorizationRequired"));
            }
            if action.allow_private_network {
                require_dangerous(action)?;
            }
            let loaded = context.load(cwd)?;
            let checks = crate::app::collect_doctor_checks(
                &crate::args::DoctorArgs {
                    json: true,
                    deep: true,
                    allow_network: action.authorize_network,
                    allow_private_network: action.allow_private_network,
                },
                &loaded,
                cwd,
                context.is_default(),
            )
            .into_iter()
            .map(|check| DoctorDto { id: check.id, status: check.status, detail: check.detail })
            .collect();
            operation_result = Some(AdminOperationResult::Doctor { checks });
        }
        _ => return Err(CliError::usage("unknown administration action")),
    }
    let result = AdminActionResult { schema_version: 1, code: "ok", operation_result };
    ensure_admin_response_size(&result, "administration action result")?;
    Ok(result)
}

fn profile_operation_result(
    name: &str,
    profile: &toml::Value,
) -> Result<AdminOperationResult, CliError> {
    let profile = config::redacted_value(profile);
    let value = serde_json::to_value(&profile)
        .map_err(|error| CliError::internal(format!("serialize profile: {error}")))?;
    let value = value
        .as_object()
        .cloned()
        .ok_or_else(|| CliError::internal("profile is not a redacted object"))?;
    if serde_json::to_vec(&value)
        .map_err(|error| CliError::internal(format!("serialize profile: {error}")))?
        .len()
        > 64 * 1024
    {
        return Err(CliError::new(
            ExitClass::Policy,
            "resourceLimit",
            "profile display exceeds its bounded output limit",
        ));
    }
    Ok(AdminOperationResult::Profile { name: name.into(), value })
}

fn ensure_admin_response_size<T: Serialize>(value: &T, description: &str) -> Result<(), CliError> {
    let size = serde_json::to_vec(value)
        .map_err(|error| CliError::internal(format!("serialize {description}: {error}")))?
        .len();
    if size > MAX_ADMIN_RESPONSE_BYTES {
        return Err(CliError::new(
            ExitClass::Policy,
            "resourceLimit",
            format!("{description} exceeds its one MiB response limit"),
        ));
    }
    Ok(())
}

fn require_dangerous(action: &AdminAction) -> Result<(), CliError> {
    if action.authorize_dangerous {
        Ok(())
    } else {
        Err(policy("dangerousActionConfirmationRequired"))
    }
}

fn validate_detection(output: &DetectionOutput) -> Result<(), CliError> {
    let text_ok = |value: &str, limit: usize| {
        !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
    };
    if output.source_name.as_deref().is_some_and(|value| !text_ok(value, 512))
        || output.candidates.len() > 64
        || output.candidates.iter().any(|candidate| {
            !text_ok(&candidate.format, 64)
                || !candidate.confidence.is_finite()
                || !(0.0..=1.0).contains(&candidate.confidence)
                || !text_ok(&candidate.detector_id, 128)
                || !text_ok(&candidate.reason, 512)
                || candidate.diagnostics.len() > 64
                || candidate.diagnostics.iter().any(|value| !text_ok(value, 512))
        })
    {
        return Err(CliError::new(
            ExitClass::Policy,
            "resourceLimit",
            "format detection result exceeds its bounded administration contract",
        ));
    }
    Ok(())
}

fn policy(code: &'static str) -> CliError {
    CliError::new(ExitClass::Policy, code, "explicit one-time authorization is required")
}

fn built_in_ocr_action_error(action: &str) -> CliError {
    CliError::new(
        ExitClass::Policy,
        "capabilityBuiltIn",
        format!("OCR is built into into-md Core and cannot be {action}"),
    )
}

fn capability_plugin_id(capability: &str) -> Result<&'static str, CliError> {
    match capability {
        "ocr" => Ok("official.ocr.ppocrv6"),
        "transcription" | "diarization" | "media" => Ok("official.media.whisper"),
        _ => Err(CliError::usage("unknown capability")),
    }
}

fn doctor_checks(
    cwd: &Path,
    loaded: &LoadedConfig,
    verify_plugins: bool,
    read_only_context: bool,
) -> Vec<DoctorDto> {
    let mut diagnostic_config = loaded.clone();
    if read_only_context {
        diagnostic_config.effective.plugins.clear();
    }
    let mut checks = crate::app::collect_doctor_checks(
        &crate::args::DoctorArgs {
            json: true,
            deep: verify_plugins,
            allow_network: false,
            allow_private_network: false,
        },
        &diagnostic_config,
        cwd,
        verify_plugins,
    );
    if read_only_context {
        checks.extend(loaded.effective.plugins.keys().map(|id| crate::app::DoctorCheck {
            id: format!("plugin:{id}"),
            status: "adminConfigContextReadOnly".into(),
            detail: "此配置上下文只读，未访问插件存储".into(),
        }));
    }
    checks
        .into_iter()
        .map(|check| DoctorDto { id: check.id, status: check.status, detail: check.detail })
        .collect()
}

fn redact_url(value: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(value) else { return "<invalid-url>".into() };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use into_markdown_plugin_manager::{
        PackageFile, PackageManifest, PackageSignature, canonical_signed_payload,
    };
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use sha2::{Digest as _, Sha256};
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{Cursor, Write as _};
    use zip::write::SimpleFileOptions;

    fn action(name: &str, target: Option<&str>) -> AdminAction {
        AdminAction {
            schema_version: 1,
            action: name.into(),
            scope: ActionScope::Project,
            target: target.map(str::to_owned),
            value: None,
            source: None,
            sha256: None,
            signing_key_id: None,
            signing_key_sha256: None,
            provider_type: None,
            model: None,
            models: BTreeMap::new(),
            api_key_env: None,
            capabilities: Vec::new(),
            timeout_ms: None,
            charset: None,
            format_hint: None,
            extension: None,
            mime_type: None,
            allow_hosts: Vec::new(),
            allow_private_network: false,
            insecure: false,
            force: false,
            resolved: false,
            from: None,
            authorization_grant: None,
            authorize_dangerous: false,
            authorize_network: false,
        }
    }

    fn signed_process_package(id: &str) -> (Vec<u8>, String) {
        let worker = b"fixture process authority".to_vec();
        let key = Ed25519KeyPair::from_seed_unchecked(&[31; 32]).unwrap();
        let public = key.public_key().as_ref();
        let fingerprint = format!("{:x}", Sha256::digest(public));
        let mut manifest = PackageManifest {
            schema_version: 1,
            id: id.into(),
            version: "1.0.0".into(),
            protocol: "process-v1".into(),
            supported_targets: BTreeSet::from([crate::app::admin_plugin_target().into()]),
            entrypoints: BTreeMap::from([(
                crate::app::admin_plugin_target().into(),
                "worker.bin".into(),
            )]),
            runtime_manifest: None,
            files: vec![PackageFile {
                path: "worker.bin".into(),
                bytes: worker.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&worker)),
                executable: true,
            }],
            signature: PackageSignature {
                signed_payload_version: 1,
                algorithm: "ed25519".into(),
                key_id: "publisher.admin-test".into(),
                public_key_base64: base64::engine::general_purpose::STANDARD.encode(public),
                public_key_sha256: fingerprint.clone(),
                signed_payload_sha256: String::new(),
                signature_base64: String::new(),
            },
        };
        let payload = canonical_signed_payload(&manifest).unwrap();
        manifest.signature.signed_payload_sha256 = format!("{:x}", Sha256::digest(&payload));
        manifest.signature.signature_base64 =
            base64::engine::general_purpose::STANDARD.encode(key.sign(&payload).as_ref());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o700);
        writer.start_file("plugin.json", options).unwrap();
        writer.write_all(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        writer.start_file("worker.bin", options).unwrap();
        writer.write_all(&worker).unwrap();
        (writer.finish().unwrap().into_inner(), fingerprint)
    }

    #[test]
    fn url_redaction_drops_every_secret_bearing_component() {
        assert_eq!(
            redact_url("https://user:pass@example.com/v1?key=secret#token"),
            "https://example.com/v1"
        );
    }

    #[test]
    fn profile_result_redacts_signed_urls_and_secret_values() {
        let profile: toml::Value = toml::from_str(
            r#"token = "leak-token-marker"
[providers.publisher]
type = "openai-compatible"
base_url = "https://leak-user:leak-pass@example.invalid/v1?signature=leak-query#leak-fragment"
model = "fixture"
api_key_env = "PUBLISHER_API_KEY"
"#,
        )
        .unwrap();
        let result = profile_operation_result("safe", &profile).unwrap();
        let output = serde_json::to_string(&result).unwrap();
        for marker in ["leak-token-marker", "leak-user", "leak-pass", "leak-query", "leak-fragment"]
        {
            assert!(!output.contains(marker));
        }
        assert!(output.contains("https://example.invalid/v1"));
        assert!(output.contains("PUBLISHER_API_KEY"));
    }

    #[test]
    fn dangerous_and_network_actions_require_per_request_confirmation() {
        let action = AdminAction {
            schema_version: 1,
            action: "capability.remove".into(),
            scope: ActionScope::Global,
            target: Some("ocr".into()),
            value: None,
            source: None,
            sha256: None,
            signing_key_id: None,
            signing_key_sha256: None,
            provider_type: None,
            model: None,
            models: BTreeMap::new(),
            api_key_env: None,
            capabilities: Vec::new(),
            timeout_ms: None,
            charset: None,
            format_hint: None,
            extension: None,
            mime_type: None,
            allow_hosts: Vec::new(),
            allow_private_network: false,
            insecure: false,
            force: false,
            resolved: false,
            from: None,
            authorization_grant: None,
            authorize_dangerous: false,
            authorize_network: false,
        };
        assert_eq!(
            require_dangerous(&action).unwrap_err().code(),
            "dangerousActionConfirmationRequired"
        );
    }

    #[test]
    fn plugin_verify_uses_scoped_manager_authority_instead_of_a_facade() {
        let directory = tempfile::tempdir().unwrap();
        let error = apply(
            directory.path(),
            &AdminConfigContext::default(),
            &AdminAction {
                schema_version: 1,
                action: "plugin.verify".into(),
                scope: ActionScope::Project,
                target: Some("missing-plugin".into()),
                value: None,
                source: None,
                sha256: None,
                signing_key_id: None,
                signing_key_sha256: None,
                provider_type: None,
                model: None,
                models: BTreeMap::new(),
                api_key_env: None,
                capabilities: Vec::new(),
                timeout_ms: None,
                charset: None,
                format_hint: None,
                extension: None,
                mime_type: None,
                allow_hosts: Vec::new(),
                allow_private_network: false,
                insecure: false,
                force: false,
                resolved: false,
                from: None,
                authorization_grant: None,
                authorize_dangerous: false,
                authorize_network: false,
            },
            Some(&directory.path().join("user-data")),
        )
        .unwrap_err();
        assert_eq!(error.code(), "usage");
    }

    #[test]
    fn signed_global_package_and_project_partial_pin_share_one_effective_authority() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("project#authority");
        std::fs::create_dir(&cwd).unwrap();
        let anchor = directory.path().join("user-data");
        let _global_config =
            config::TestGlobalConfigGuard::set(Some(anchor.join("config/config.toml")));
        let id = "fixture-admin-process";
        let (archive, fingerprint) = signed_process_package(id);
        let package_sha = format!("{:x}", Sha256::digest(&archive));
        let package = directory.path().join("fixture.impkg");
        std::fs::write(&package, archive).unwrap();
        let context = AdminConfigContext::default();
        let mut install = action("plugin.install", Some(id));
        install.scope = ActionScope::Global;
        install.source = Some(package.display().to_string());
        install.sha256 = Some(package_sha.clone());
        install.signing_key_id = Some("publisher.admin-test".into());
        install.signing_key_sha256 = Some(fingerprint);
        install.authorize_dangerous = true;
        apply(&cwd, &context, &install, Some(&anchor)).unwrap();

        let initial = snapshot(&cwd, &context, Some(&anchor)).unwrap();
        let installed =
            initial.plugins.iter().find(|plugin| plugin.id == id && plugin.effective).unwrap();
        assert_eq!(installed.package_scope.as_deref(), Some("global"));
        assert_eq!(installed.verification.as_deref(), Some("metadataAuthenticated"));

        std::fs::write(
            cwd.join(".into-markdown.toml"),
            format!("schema_version = 1\n\n[plugins.{id}]\nsha256 = \"{}\"\n", "0".repeat(64)),
        )
        .unwrap();
        let alias = std::fs::canonicalize(cwd.join(".")).unwrap();
        let mismatched = snapshot(&alias, &context, Some(&anchor)).unwrap();
        let mismatched_plugin =
            mismatched.plugins.iter().find(|plugin| plugin.id == id && plugin.effective).unwrap();
        let mismatch_code = mismatched_plugin.verification.as_deref().unwrap();
        assert_ne!(mismatch_code, "metadataAuthenticated");
        assert_eq!(mismatched_plugin.package_scope.as_deref(), Some("global"));
        assert!(mismatched.plugins.iter().any(|plugin| {
            plugin.id == id
                && plugin.scope == "project"
                && plugin.package_scope.as_deref() == Some("global")
        }));
        let verify_error =
            apply(&alias, &context, &action("plugin.verify", Some(id)), Some(&anchor)).unwrap_err();
        assert_eq!(verify_error.code(), mismatch_code);
        let doctor =
            mismatched.doctor.iter().find(|check| check.id == format!("plugin:{id}")).unwrap();
        assert_eq!(doctor.status, "error");
        let doctor_result =
            apply(&alias, &context, &action("doctor.run", None), Some(&anchor)).unwrap();
        let Some(AdminOperationResult::Doctor { checks }) = doctor_result.operation_result else {
            panic!("doctor action must return checks")
        };
        let deep_check = checks.iter().find(|check| check.id == format!("plugin:{id}")).unwrap();
        assert_ne!(deep_check.status, "ok");
        assert!(deep_check.detail.contains(mismatch_code));

        std::fs::write(
            alias.join(".into-markdown.toml"),
            format!("schema_version = 1\n\n[plugins.{id}]\nsha256 = \"{package_sha}\"\n"),
        )
        .unwrap();
        let repaired = snapshot(&alias, &context, Some(&anchor)).unwrap();
        let repaired_plugin =
            repaired.plugins.iter().find(|plugin| plugin.id == id && plugin.effective).unwrap();
        assert_eq!(repaired_plugin.verification.as_deref(), Some("metadataAuthenticated"));
        apply(&alias, &context, &action("plugin.verify", Some(id)), Some(&anchor)).unwrap();
        assert_eq!(
            repaired.doctor.iter().find(|check| check.id == format!("plugin:{id}")).unwrap().status,
            "ok"
        );
    }

    #[test]
    fn management_actions_share_cli_validation_scope_and_bounded_results() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let anchor = cwd.join("user-data");
        let _global_config =
            config::TestGlobalConfigGuard::set(Some(anchor.join("config/config.toml")));
        let input = cwd.join("sample.txt");
        std::fs::write(&input, b"hello\n").unwrap();

        let mut detect = action("format.detect", None);
        detect.source = Some(input.display().to_string());
        let context = AdminConfigContext::default();
        let detected = apply(cwd, &context, &detect, Some(&anchor)).unwrap();
        assert!(matches!(
            detected.operation_result,
            Some(AdminOperationResult::Detection { ref candidates, .. }) if !candidates.is_empty()
        ));

        let shown = apply(cwd, &context, &action("config.show", None), Some(&anchor)).unwrap();
        assert!(matches!(
            shown.operation_result,
            Some(AdminOperationResult::Config { ref operation, ref value })
                if operation == "showMerged" && value.is_object()
        ));
        let mut resolved_show = action("config.show", None);
        resolved_show.resolved = true;
        let shown = apply(cwd, &context, &resolved_show, Some(&anchor)).unwrap();
        assert!(matches!(
            shown.operation_result,
            Some(AdminOperationResult::Config { ref operation, ref value })
                if operation == "showResolved" && value.is_object()
        ));

        let mut add = action("provider.add", Some("fixture"));
        add.source = Some("https://example.invalid/v1".into());
        add.provider_type = Some("openai-compatible".into());
        add.model = Some("fixture-model".into());
        add.api_key_env = Some("FIXTURE_API_KEY".into());
        add.capabilities = vec!["image-description".into()];
        add.allow_hosts = vec!["example.invalid".into()];
        add.allow_private_network = true;
        add.authorize_dangerous = true;
        apply(cwd, &context, &add, Some(&anchor)).unwrap();
        let global_before = config::scope_snapshot(Scope::Global, cwd).unwrap();
        let mut invalid_global_default = action("provider.set-default", Some("fixture"));
        invalid_global_default.scope = ActionScope::Global;
        invalid_global_default.authorize_dangerous = true;
        assert_eq!(
            apply(cwd, &context, &invalid_global_default, Some(&anchor)).unwrap_err().code(),
            "usage"
        );
        assert_eq!(global_before, config::scope_snapshot(Scope::Global, cwd).unwrap());
        add.scope = ActionScope::Global;
        apply(cwd, &context, &add, Some(&anchor)).unwrap();
        let providers = snapshot(cwd, &context, Some(&anchor)).unwrap().providers;
        assert_eq!(providers.iter().filter(|item| item.name == "fixture").count(), 3);
        assert!(providers.iter().any(|item| item.scope == "effective" && item.effective));
        assert!(providers.iter().any(|item| {
            item.name == "fixture"
                && item.scope == "effective"
                && item.allowed_hosts == ["example.invalid"]
                && item.allow_private_network
        }));
        assert!(providers.iter().any(|item| {
            item.scope == "global"
                && !item.effective
                && item.shadowed_by.as_deref() == Some("effective")
        }));

        let before = config::scope_snapshot(Scope::Project, cwd).unwrap();
        let mut missing_default = action("provider.set-default", Some("missing"));
        missing_default.authorize_dangerous = true;
        let error = apply(cwd, &context, &missing_default, Some(&anchor)).unwrap_err();
        assert_eq!(error.code(), "usage");
        assert_eq!(before, config::scope_snapshot(Scope::Project, cwd).unwrap());
        let mut set_default = action("provider.set-default", Some("fixture"));
        set_default.authorize_dangerous = true;
        apply(cwd, &context, &set_default, Some(&anchor)).unwrap();

        let mut set = action("config.set", Some("conversion.ocr.policy"));
        set.value = Some("always".into());
        set.authorize_dangerous = true;
        apply(cwd, &context, &set, Some(&anchor)).unwrap();
        let mut unset = action("config.unset", Some("conversion.ocr.policy"));
        unset.authorize_dangerous = true;
        apply(cwd, &context, &unset, Some(&anchor)).unwrap();

        config::set(Scope::Project, cwd, "profiles.base.conversion.ocr.policy", "auto").unwrap();
        let mut create = action("profile.create", Some("copy"));
        create.from = Some("base".into());
        create.authorize_dangerous = true;
        apply(cwd, &context, &create, Some(&anchor)).unwrap();
        let shown =
            apply(cwd, &context, &action("profile.show", Some("copy")), Some(&anchor)).unwrap();
        assert!(matches!(shown.operation_result, Some(AdminOperationResult::Profile { .. })));
        let mut remove_profile = action("profile.remove", Some("copy"));
        remove_profile.authorize_dangerous = true;
        apply(cwd, &context, &remove_profile, Some(&anchor)).unwrap();

        let mut remove_provider = action("provider.remove", Some("fixture"));
        remove_provider.authorize_dangerous = true;
        apply(cwd, &context, &remove_provider, Some(&anchor)).unwrap();
    }

    #[test]
    fn generic_config_mutation_cannot_bypass_managed_namespaces() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let anchor = cwd.join("user-data");
        for key in [
            "default_provider",
            "providers.escape.model",
            "plugins.escape.source",
            "profiles.escape.cli.language",
            "schema_version",
            "conversion.ocr.policy.extra",
        ] {
            let before = config::scope_snapshot(Scope::Project, cwd).unwrap();
            let mut mutation = action("config.set", Some(key));
            mutation.value = Some("x".into());
            mutation.authorize_dangerous = true;
            let error =
                apply(cwd, &AdminConfigContext::default(), &mutation, Some(&anchor)).unwrap_err();
            assert_eq!(error.code(), "adminConfigKeyDenied");
            assert_eq!(before, config::scope_snapshot(Scope::Project, cwd).unwrap());
        }
    }

    #[test]
    fn partial_layers_are_raw_overrides_plus_one_complete_effective_record() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let anchor = cwd.join("user-data");
        let mut add = action("provider.add", Some("split"));
        add.scope = ActionScope::Global;
        add.source = Some("https://example.invalid/v1".into());
        add.provider_type = Some("openai-compatible".into());
        add.model = Some("discarded-model".into());
        add.api_key_env = Some("SPLIT_API_KEY".into());
        add.authorize_dangerous = true;
        apply(cwd, &AdminConfigContext::default(), &add, Some(&anchor)).unwrap();
        crate::app::with_admin_authority(Some(&anchor), || {
            config::unset(Scope::Global, cwd, "providers.split.model")
        })
        .unwrap();
        config::set(Scope::Project, cwd, "providers.split.model", "merged-model").unwrap();
        let snapshot = snapshot(cwd, &AdminConfigContext::default(), Some(&anchor)).unwrap();
        let raw_global = snapshot
            .providers
            .iter()
            .find(|item| item.name == "split" && item.scope == "global")
            .unwrap();
        assert!(raw_global.model.is_none());
        let raw_project = snapshot
            .providers
            .iter()
            .find(|item| item.name == "split" && item.scope == "project")
            .unwrap();
        assert!(raw_project.base_url.is_none());
        let effective = snapshot
            .providers
            .iter()
            .find(|item| item.name == "split" && item.scope == "effective")
            .unwrap();
        assert_eq!(effective.base_url.as_deref(), Some("https://example.invalid/v1"));
        assert_eq!(effective.model.as_deref(), Some("merged-model"));
    }

    #[test]
    fn administration_snapshot_uses_the_shallow_doctor_service_dto() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let anchor = cwd.join("user-data");
        let context = AdminConfigContext::default();
        let loaded = crate::app::with_admin_authority(Some(&anchor), || context.load(cwd)).unwrap();
        let expected = crate::app::collect_doctor_checks(
            &crate::args::DoctorArgs {
                json: true,
                deep: false,
                allow_network: false,
                allow_private_network: false,
            },
            &loaded,
            cwd,
            false,
        );
        let actual = snapshot(cwd, &context, Some(&anchor)).unwrap().doctor;
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.status, expected.status);
            assert_eq!(actual.detail, expected.detail);
        }
    }

    #[test]
    fn config_get_returns_the_recursive_redacted_view() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let anchor = cwd.join("user-data");
        std::fs::write(
            cwd.join(".into-markdown.toml"),
            concat!(
                "schema_version = 1\n",
                "[plugins.remote]\n",
                "source = \"https://leak-user:leak-pass@example.invalid/plugin.zip?leak-query=yes#leak-fragment\"\n",
                "protocol = \"process-v1\"\n",
                "enabled = true\n",
            ),
        )
        .unwrap();
        let result = apply(
            cwd,
            &AdminConfigContext::default(),
            &action("config.get", Some("plugins.remote.source")),
            Some(&anchor),
        )
        .unwrap();
        let output = serde_json::to_string(&result).unwrap();
        for marker in ["leak-user", "leak-pass", "leak-query", "leak-fragment"] {
            assert!(!output.contains(marker));
        }
        assert!(output.contains("https://example.invalid/plugin.zip"));
    }

    #[test]
    fn administration_wire_limit_accepts_exact_boundary_and_rejects_one_more_byte() {
        #[derive(Serialize)]
        #[serde(transparent)]
        struct Payload(String);

        let exact = Payload("x".repeat(MAX_ADMIN_RESPONSE_BYTES - 2));
        assert_eq!(serde_json::to_vec(&exact).unwrap().len(), MAX_ADMIN_RESPONSE_BYTES);
        ensure_admin_response_size(&exact, "fixture").unwrap();

        let over = Payload("x".repeat(MAX_ADMIN_RESPONSE_BYTES - 1));
        assert_eq!(serde_json::to_vec(&over).unwrap().len(), MAX_ADMIN_RESPONSE_BYTES + 1);
        let error = ensure_admin_response_size(&over, "fixture").unwrap_err();
        assert_eq!(error.code(), "resourceLimit");
    }

    fn write_non_default_context_fixture(cwd: &Path) -> PathBuf {
        let explicit = cwd.join("explicit.toml");
        std::fs::write(
            &explicit,
            concat!(
                "schema_version = 1\n",
                "[providers.fixture]\n",
                "type = \"openai-compatible\"\n",
                "base_url = \"https://example.invalid/v1\"\n",
                "model = \"fixture\"\n",
                "api_key_env = \"FIXTURE_API_KEY\"\n",
                "[profiles.secure.providers.fixture]\n",
                "model = \"profile-fixture\"\n",
                "[plugins.uninstalled]\n",
                "source = \"https://example.invalid/plugin.zip\"\n",
                "sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n",
                "protocol = \"process-v1\"\n",
                "enabled = true\n",
                "signing_key_id = \"release-key\"\n",
                "signing_key_sha256 = \"1111111111111111111111111111111111111111111111111111111111111111\"\n",
            ),
        )
        .unwrap();
        explicit
    }

    fn assert_explicit_snapshot_is_read_only(snapshot: &AdminSnapshot) {
        assert!(snapshot.configuration_read_only);
        assert!(
            snapshot
                .providers
                .iter()
                .any(|provider| provider.name == "fixture" && provider.scope == "effective")
        );
        assert!(snapshot.plugins.iter().any(|plugin| {
            plugin.id == "uninstalled"
                && plugin.scope == "effective"
                && plugin.action_scope.is_none()
                && plugin.package_scope.is_none()
                && plugin.verification.as_deref() == Some("adminConfigContextReadOnly")
        }));
        assert!(
            snapshot.providers.iter().all(|provider| {
                provider.scope == "effective" && provider.action_scope.is_none()
            })
        );
        assert!(snapshot.doctor.iter().any(|check| {
            check.id == "plugin:uninstalled" && check.status == "adminConfigContextReadOnly"
        }));
    }

    #[test]
    fn non_default_configuration_context_is_visible_but_strictly_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let anchor = cwd.join("user-data");
        let explicit = write_non_default_context_fixture(cwd);
        let explicit_context = AdminConfigContext {
            explicit: vec![explicit.clone()],
            ..AdminConfigContext::default()
        };
        let explicit_snapshot = snapshot(cwd, &explicit_context, Some(&anchor)).unwrap();
        assert!(!anchor.exists(), "special-context snapshot must not create or recover a store");
        assert_explicit_snapshot_is_read_only(&explicit_snapshot);

        for action_name in ["plugin.verify", "provider.test", "profile.show"] {
            let error =
                apply(cwd, &explicit_context, &action(action_name, Some("fixture")), Some(&anchor))
                    .unwrap_err();
            assert_eq!(error.code(), "adminConfigContextReadOnly");
            assert!(!anchor.exists());
        }
        let capability_error =
            apply(cwd, &explicit_context, &action("capability.verify", Some("ocr")), Some(&anchor))
                .unwrap_err();
        assert_eq!(capability_error.code(), "adminConfigContextReadOnly");

        let profile_context = AdminConfigContext {
            explicit: vec![explicit.clone()],
            no_automatic: true,
            profile: Some("secure".into()),
            ..AdminConfigContext::default()
        };
        let profile_snapshot = snapshot(cwd, &profile_context, Some(&anchor)).unwrap();
        assert!(profile_snapshot.configuration_read_only);
        assert!(profile_snapshot.profiles.iter().any(|profile| {
            profile.name == "secure" && profile.scope == "effective" && profile.active
        }));

        let input = cwd.join("sample.txt");
        std::fs::write(&input, b"hello\n").unwrap();
        let mut detect = action("format.detect", None);
        detect.source = Some(input.display().to_string());
        apply(cwd, &explicit_context, &detect, Some(&anchor)).unwrap();

        let no_config = AdminConfigContext { no_automatic: true, ..AdminConfigContext::default() };
        let no_config_snapshot = snapshot(cwd, &no_config, Some(&anchor)).unwrap();
        assert!(no_config_snapshot.configuration_read_only);
        assert!(no_config_snapshot.providers.is_empty());
        assert!(no_config_snapshot.plugins.is_empty());
        assert!(no_config_snapshot.profiles.is_empty());

        let no_config_explicit = AdminConfigContext {
            explicit: vec![explicit.clone()],
            no_automatic: true,
            ..AdminConfigContext::default()
        };
        std::fs::create_dir(cwd.join(".into-markdown.toml")).unwrap();
        let snapshot = snapshot(cwd, &no_config_explicit, Some(&anchor)).unwrap();
        assert!(snapshot.configuration_read_only);
        assert!(
            snapshot.providers.iter().all(|provider| {
                provider.scope == "effective" && provider.action_scope.is_none()
            })
        );
        assert!(snapshot.plugins.iter().all(|plugin| {
            plugin.scope == "effective"
                && plugin.action_scope.is_none()
                && plugin.package_scope.is_none()
        }));

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "admin::tests::environment_selected_profile_child", "--nocapture"])
            .env("INTO_MD_ADMIN_ENV_PROFILE_CHILD", "1")
            .env("INTO_MD_ADMIN_ENV_PROFILE_CWD", cwd)
            .env("INTO_MD_ADMIN_ENV_PROFILE_CONFIG", &explicit)
            .env("INTO_MD_ADMIN_ENV_PROFILE_ANCHOR", &anchor)
            .env("INTO_MD_PROFILE", "secure")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn environment_selected_profile_child() {
        if std::env::var_os("INTO_MD_ADMIN_ENV_PROFILE_CHILD").is_none() {
            return;
        }
        let cwd = PathBuf::from(std::env::var_os("INTO_MD_ADMIN_ENV_PROFILE_CWD").unwrap());
        let explicit = PathBuf::from(std::env::var_os("INTO_MD_ADMIN_ENV_PROFILE_CONFIG").unwrap());
        let anchor = PathBuf::from(std::env::var_os("INTO_MD_ADMIN_ENV_PROFILE_ANCHOR").unwrap());
        let context = AdminConfigContext {
            explicit: vec![explicit],
            no_automatic: true,
            ..AdminConfigContext::default()
        };
        assert_eq!(context.selected_profile().as_deref(), Some("secure"));
        let snapshot = snapshot(&cwd, &context, Some(&anchor)).unwrap();
        assert!(snapshot.configuration_read_only);
        assert!(snapshot.profiles.iter().any(|profile| {
            profile.name == "secure" && profile.scope == "effective" && profile.active
        }));
        assert!(snapshot.providers.iter().any(|provider| {
            provider.name == "fixture"
                && provider.scope == "effective"
                && provider.model.as_deref() == Some("profile-fixture")
        }));
    }
}
