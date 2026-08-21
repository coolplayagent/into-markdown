use crate::{CapabilityKind, PluginCapabilityDescriptor, PluginManifest};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// One configured provider and optional provider-owned model bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReference {
    /// Plugin package ID.
    pub plugin_id: String,
    /// Package-local capability ID.
    pub capability_id: String,
    /// Optional package-local model bundle ID.
    pub model_bundle: Option<String>,
}

/// Deterministic primary and ordered readiness fallback configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRoute {
    /// Required preferred provider.
    pub primary: ProviderReference,
    /// Providers considered in this exact order only before execution starts.
    pub fallbacks: Vec<ProviderReference>,
}

/// Whether readiness fallback is permitted for this invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMode {
    /// Resolve only the configured primary and fail closed if unavailable.
    RequiredPrimary,
    /// Select the first ready provider from primary followed by fallbacks.
    ReadinessFallback,
}

/// Exact immutable provider selected for a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderBinding {
    /// Package ID.
    pub plugin_id: String,
    /// Installed package version.
    pub plugin_version: String,
    /// Digest of the signed manifest bytes.
    pub manifest_sha256: String,
    /// Package-local capability ID.
    pub capability_id: String,
    /// Stable provenance provider ID.
    pub provider_id: String,
    /// Selected package-local model bundle.
    pub model_bundle: Option<String>,
    /// Immutable installed package root.
    pub install_root: PathBuf,
}

#[derive(Debug, Clone)]
struct RegisteredPlugin {
    manifest: Arc<PluginManifest>,
    manifest_sha256: String,
    install_root: PathBuf,
    enabled: bool,
}

/// Installed capability inventory used to resolve exact provider bindings.
#[derive(Debug, Clone, Default)]
pub struct CapabilityRegistry {
    plugins: BTreeMap<String, RegisteredPlugin>,
}

/// Stable route construction or resolution failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RouteError {
    /// Package registration conflicts with an existing identity.
    #[error("duplicate plugin identity: {0}")]
    DuplicatePlugin(String),
    /// Registered package authority is malformed.
    #[error("invalid plugin authority: {0}")]
    InvalidPlugin(String),
    /// No acceptable provider is ready.
    #[error("provider unavailable: {0}")]
    Unavailable(String),
}

impl CapabilityRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one already authenticated immutable package.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::InvalidPlugin`] for invalid authority and
    /// [`RouteError::DuplicatePlugin`] for duplicate IDs.
    pub fn register(
        &mut self,
        manifest: PluginManifest,
        manifest_sha256: String,
        install_root: PathBuf,
        enabled: bool,
    ) -> Result<(), RouteError> {
        manifest.validate().map_err(RouteError::InvalidPlugin)?;
        if !valid_sha256(&manifest_sha256) || !install_root.is_absolute() {
            return Err(RouteError::InvalidPlugin(
                "manifest digest or installation root is invalid".into(),
            ));
        }
        let id = manifest.id.clone();
        if self
            .plugins
            .insert(
                id.clone(),
                RegisteredPlugin {
                    manifest: Arc::new(manifest),
                    manifest_sha256,
                    install_root,
                    enabled,
                },
            )
            .is_some()
        {
            return Err(RouteError::DuplicatePlugin(id));
        }
        Ok(())
    }

    /// Resolve a route without invoking any provider.
    ///
    /// `ready` performs local verification of executable, model, and platform
    /// state. A provider that becomes invalid after this method returns must
    /// fail authoritatively during execution; callers must not re-enter the
    /// fallback chain.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::Unavailable`] when no permitted provider is ready.
    pub fn resolve(
        &self,
        kind: CapabilityKind,
        route: &CapabilityRoute,
        mode: ResolutionMode,
        mut ready: impl FnMut(&ProviderBinding) -> bool,
    ) -> Result<ProviderBinding, RouteError> {
        let references = std::iter::once(&route.primary).chain(match mode {
            ResolutionMode::RequiredPrimary => [].iter(),
            ResolutionMode::ReadinessFallback => route.fallbacks.iter(),
        });
        let mut seen = BTreeSet::new();
        let mut details = Vec::new();
        for reference in references {
            let key = format!("{}/{}", reference.plugin_id, reference.capability_id);
            if !seen.insert(key.clone()) {
                details.push(format!("{key}:duplicate"));
                continue;
            }
            match self.bind(kind, reference) {
                Ok(binding) if ready(&binding) => return Ok(binding),
                Ok(_) => details.push(format!("{key}:not-ready")),
                Err(error) => details.push(format!("{key}:{error}")),
            }
        }
        Err(RouteError::Unavailable(details.join(",")))
    }

    fn bind(
        &self,
        kind: CapabilityKind,
        reference: &ProviderReference,
    ) -> Result<ProviderBinding, &'static str> {
        let plugin = self.plugins.get(&reference.plugin_id).ok_or("not-installed")?;
        if !plugin.enabled {
            return Err("disabled");
        }
        let capability = plugin
            .manifest
            .capabilities
            .iter()
            .find(|capability| capability.id == reference.capability_id)
            .ok_or("capability-missing")?;
        if capability.kind != kind {
            return Err("capability-kind-mismatch");
        }
        validate_model(capability, reference.model_bundle.as_deref())?;
        Ok(ProviderBinding {
            plugin_id: plugin.manifest.id.clone(),
            plugin_version: plugin.manifest.version.clone(),
            manifest_sha256: plugin.manifest_sha256.clone(),
            capability_id: capability.id.clone(),
            provider_id: capability.provider_id.clone(),
            model_bundle: reference.model_bundle.clone(),
            install_root: plugin.install_root.clone(),
        })
    }
}

fn validate_model(
    capability: &PluginCapabilityDescriptor,
    selected: Option<&str>,
) -> Result<(), &'static str> {
    match selected {
        Some(model) if capability.model_bundles.iter().any(|candidate| candidate == model) => {
            Ok(())
        }
        Some(_) => Err("model-unsupported"),
        None if capability.model_bundles.len() <= 1 => Ok(()),
        None => Err("model-selection-required"),
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CAPABILITY_PROTOCOL, HostApiRange, PluginFileDescriptor, PluginPermissions,
        PluginTargetDescriptor, ResourceEnvelope,
    };

    fn plugin(id: &str, capability: &str, provider: &str) -> PluginManifest {
        PluginManifest {
            schema_version: 1,
            id: id.into(),
            version: "1.2.3".into(),
            publisher: "publisher".into(),
            host_api: HostApiRange { minimum: 1, maximum: 1 },
            protocol: CAPABILITY_PROTOCOL.into(),
            targets: vec![PluginTargetDescriptor {
                triple: "aarch64-apple-darwin".into(),
                entrypoint: "bin/provider".into(),
                files: vec![PluginFileDescriptor {
                    path: "bin/provider".into(),
                    bytes: 1,
                    sha256: "a".repeat(64),
                    executable: true,
                }],
            }],
            capabilities: vec![PluginCapabilityDescriptor {
                id: capability.into(),
                kind: CapabilityKind::Ocr,
                provider_id: provider.into(),
                languages: vec![],
                media_types: vec!["image/png".into()],
                model_bundles: vec![],
                resources: ResourceEnvelope {
                    max_input_bytes: 1024,
                    max_output_bytes: 1024,
                    max_memory_bytes: 32 * 1024 * 1024,
                    max_temporary_bytes: 1024,
                    timeout_ms: 1000,
                },
            }],
            models: vec![],
            permissions: PluginPermissions::default(),
            licenses: vec!["Apache-2.0".into()],
        }
    }

    #[test]
    fn fallback_is_readiness_only_and_ordered() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let mut registry = CapabilityRegistry::new();
        registry
            .register(
                plugin("primary.plugin", "ocr", "primary.provider"),
                "a".repeat(64),
                root.clone(),
                true,
            )
            .unwrap();
        registry
            .register(
                plugin("fallback.plugin", "ocr", "fallback.provider"),
                "b".repeat(64),
                root,
                true,
            )
            .unwrap();
        let route = CapabilityRoute {
            primary: ProviderReference {
                plugin_id: "primary.plugin".into(),
                capability_id: "ocr".into(),
                model_bundle: None,
            },
            fallbacks: vec![ProviderReference {
                plugin_id: "fallback.plugin".into(),
                capability_id: "ocr".into(),
                model_bundle: None,
            }],
        };
        assert!(
            registry
                .resolve(CapabilityKind::Ocr, &route, ResolutionMode::RequiredPrimary, |_| false)
                .is_err()
        );
        let binding = registry
            .resolve(CapabilityKind::Ocr, &route, ResolutionMode::ReadinessFallback, |binding| {
                binding.plugin_id == "fallback.plugin"
            })
            .unwrap();
        assert_eq!(binding.provider_id, "fallback.provider");
    }
}
