//! Shared, bounded administration DTOs used by the local Web console.
//!
//! This module deliberately calls the same catalog, configuration and model
//! services as the CLI.  It never shells out to `into-md`, reads API-key
//! values, or returns an unredacted configuration value.

use crate::args::Scope;
use crate::config::{self, LoadedConfig};
use crate::error::{CliError, ExitClass};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_ACTION_TEXT: usize = 4096;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdminSnapshot {
    pub schema_version: u32,
    pub formats: Vec<FormatDto>,
    pub models: ModelsDto,
    pub providers: Vec<ProviderDto>,
    pub plugins: Vec<PluginDto>,
    pub configuration: serde_json::Value,
    pub profiles: Vec<String>,
    pub doctor: Vec<DoctorDto>,
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
pub struct ModelsDto {
    pub default_bundle: String,
    pub entries: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDto {
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub environment_set: bool,
    pub capabilities: Vec<String>,
    pub default: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDto {
    pub id: String,
    pub source: String,
    pub sha256: Option<String>,
    pub protocol: String,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorDto {
    pub id: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdminAction {
    pub schema_version: u32,
    pub action: String,
    #[serde(default)]
    pub scope: ActionScope,
    pub target: Option<String>,
    pub value: Option<String>,
    #[serde(default)]
    pub authorize_dangerous: bool,
    #[serde(default)]
    pub authorize_network: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
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
    pub snapshot: AdminSnapshot,
}

pub fn snapshot(cwd: &Path) -> Result<AdminSnapshot, CliError> {
    let loaded = config::load(cwd, &[], false, None, None)?;
    snapshot_loaded(cwd, &loaded)
}

fn snapshot_loaded(cwd: &Path, loaded: &LoadedConfig) -> Result<AdminSnapshot, CliError> {
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
    let manager = crate::app::model_manager()?;
    let statuses = manager.list().map_err(crate::app::model_error)?;
    let entries = manager
        .manifest()
        .bundles
        .iter()
        .zip(statuses)
        .map(|(bundle, status)| serde_json::json!({ "bundle": bundle, "status": status }))
        .collect();
    let providers = loaded
        .effective
        .providers
        .iter()
        .map(|(name, provider)| ProviderDto {
            name: name.clone(),
            provider_type: provider.provider_type.clone(),
            base_url: redact_url(&provider.base_url),
            model: provider.model.clone(),
            api_key_env: provider.api_key_env.clone(),
            environment_set: std::env::var_os(&provider.api_key_env).is_some(),
            capabilities: provider.capabilities.clone(),
            default: loaded.effective.default_provider.as_deref() == Some(name),
        })
        .collect();
    let plugins = loaded
        .effective
        .plugins
        .iter()
        .map(|(id, plugin)| PluginDto {
            id: id.clone(),
            source: redact_url(&plugin.source),
            sha256: plugin.sha256.clone(),
            protocol: plugin.protocol.clone(),
            enabled: plugin.enabled,
        })
        .collect();
    let configuration = serde_json::to_value(loaded.display_value(true)?).map_err(|error| {
        CliError::internal(format!("serialize redacted configuration: {error}"))
    })?;
    let profiles = config::profile_names(&loaded.merged);
    let doctor = doctor_checks(cwd, loaded);
    Ok(AdminSnapshot {
        schema_version: 1,
        formats,
        models: ModelsDto { default_bundle: manager.manifest().default_bundle.clone(), entries },
        providers,
        plugins,
        configuration,
        profiles,
        doctor,
    })
}

pub fn apply(cwd: &Path, action: AdminAction) -> Result<AdminActionResult, CliError> {
    if action.schema_version != 1 {
        return Err(CliError::usage("unsupported administration request schema"));
    }
    for text in [action.target.as_deref(), action.value.as_deref()].into_iter().flatten() {
        if text.len() > MAX_ACTION_TEXT || text.contains('\0') {
            return Err(CliError::usage("administration request value is invalid or oversized"));
        }
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
    match action.action.as_str() {
        "model.verify" => {
            let manager = crate::app::model_manager()?;
            let id =
                if target.is_empty() { manager.manifest().default_bundle.as_str() } else { target };
            manager.verify(id).map_err(crate::app::model_error)?;
        }
        "model.remove" => {
            require_dangerous(&action)?;
            crate::app::model_manager()?.remove(target).map_err(crate::app::model_error)?;
        }
        "model.install" => {
            if !action.authorize_network {
                return Err(policy("networkAuthorizationRequired"));
            }
            crate::app::ensure_model_parent()?;
            let loaded = config::load(cwd, &[], false, None, None)?;
            let manager = crate::app::model_manager()?;
            let id =
                if target.is_empty() { manager.manifest().default_bundle.as_str() } else { target };
            let execution = into_markdown::ExecutionContext::new(
                into_markdown::ExecutionOptions {
                    timeout: loaded.timeout_ms.map(std::time::Duration::from_millis),
                    ..into_markdown::ExecutionOptions::default()
                },
                loaded.options.limits,
            );
            manager.require_installable(id).map_err(crate::app::model_error)?;
            manager
                .install(id, &crate::model_fetch::PinnedModelFetcher::default(), &execution)
                .map_err(crate::app::model_error)?;
        }
        "plugin.enable" | "plugin.disable" => {
            config::set_plugin_enabled(scope, cwd, target, action.action == "plugin.enable")?;
        }
        "plugin.verify" => {
            let loaded = config::load(cwd, &[], false, None, None)?;
            if !target.is_empty() && !loaded.effective.plugins.contains_key(target) {
                return Err(CliError::usage(format!("unknown plugin '{target}'")));
            }
            return Err(CliError::component(format!(
                "plugins verify {}: unavailable",
                if target.is_empty() { "all" } else { target }
            )));
        }
        "plugin.remove" => {
            require_dangerous(&action)?;
            config::remove_plugin(scope, cwd, target)?;
        }
        "config.set" => {
            config::set(scope, cwd, target, action.value.as_deref().unwrap_or(""))?;
        }
        "config.unset" => {
            require_dangerous(&action)?;
            config::unset(scope, cwd, target)?;
        }
        "profile.create" => {
            let loaded = config::load(cwd, &[], false, None, None)?;
            config::create_profile(scope, cwd, target, action.value.as_deref(), &loaded.merged)?;
        }
        "profile.remove" => {
            require_dangerous(&action)?;
            config::remove_profile(scope, cwd, target)?;
        }
        "provider.test" => {
            if !action.authorize_network {
                return Err(policy("networkAuthorizationRequired"));
            }
            let loaded = config::load(cwd, &[], false, None, None)?;
            crate::app::test_provider(
                &loaded,
                &crate::args::ProviderTestArgs {
                    name: target.into(),
                    allow_network: true,
                    allow_private_network: false,
                    allow_host: Vec::new(),
                },
            )?;
        }
        _ => return Err(CliError::usage("unknown administration action")),
    }
    Ok(AdminActionResult { schema_version: 1, code: "ok", snapshot: snapshot(cwd)? })
}

fn require_dangerous(action: &AdminAction) -> Result<(), CliError> {
    if action.authorize_dangerous {
        Ok(())
    } else {
        Err(policy("dangerousActionConfirmationRequired"))
    }
}

fn policy(code: &'static str) -> CliError {
    CliError::new(ExitClass::Policy, code, "explicit one-time authorization is required")
}

fn doctor_checks(cwd: &Path, loaded: &LoadedConfig) -> Vec<DoctorDto> {
    let mut checks = vec![
        DoctorDto {
            id: "configuration".into(),
            status: "ok".into(),
            detail: format!("{} layer(s) loaded", loaded.paths.loaded.len()),
        },
        DoctorDto {
            id: "workingDirectory".into(),
            status: if cwd.is_dir() { "ok" } else { "error" }.into(),
            detail: cwd.display().to_string(),
        },
        DoctorDto {
            id: "modelManifest".into(),
            status: if into_markdown::model_manifest().is_ok() { "ok" } else { "error" }.into(),
            detail: "embedded authority checked".into(),
        },
        DoctorDto {
            id: "networkProbe".into(),
            status: "skipped".into(),
            detail: "offline by default; no network request was made".into(),
        },
    ];
    checks.extend(loaded.effective.providers.iter().map(|(name, provider)| DoctorDto {
        id: format!("providerEnvironment:{name}"),
        status:
            if std::env::var_os(&provider.api_key_env).is_some() { "ok" } else { "missing" }.into(),
        detail: provider.api_key_env.clone(),
    }));
    checks.extend(loaded.effective.plugins.iter().map(|(id, plugin)| {
        DoctorDto {
            id: format!("plugin:{id}"),
            status: if !plugin.enabled {
                "disabled"
            } else if plugin.source.starts_with("https://") {
                "unavailable"
            } else if Path::new(&plugin.source).is_file() {
                "ok"
            } else {
                "missing"
            }
            .into(),
            detail: if plugin.enabled {
                "local package presence checked without execution"
            } else {
                "plugin is disabled"
            }
            .into(),
        }
    }));
    checks
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

    #[test]
    fn url_redaction_drops_every_secret_bearing_component() {
        assert_eq!(
            redact_url("https://user:pass@example.com/v1?key=secret#token"),
            "https://example.com/v1"
        );
    }

    #[test]
    fn dangerous_and_network_actions_require_per_request_confirmation() {
        let action = AdminAction {
            schema_version: 1,
            action: "model.remove".into(),
            scope: ActionScope::Global,
            target: Some("x".into()),
            value: None,
            authorize_dangerous: false,
            authorize_network: false,
        };
        assert_eq!(
            require_dangerous(&action).unwrap_err().code(),
            "dangerousActionConfirmationRequired"
        );
    }
}
