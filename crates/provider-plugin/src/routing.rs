use crate::{CapabilityKind, PluginManifest};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;

/// Product-level capability routed across Core, local plugins, and remote providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityId {
    /// Legacy binary Office document normalization.
    LegacyOffice,
    /// Image or rendered-page OCR.
    Ocr,
    /// Audio/video transcription.
    Transcription,
    /// Anonymous speaker diarization.
    Diarization,
}

impl CapabilityId {
    /// Stable configuration and CLI spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyOffice => "legacy-office",
            Self::Ocr => "ocr",
            Self::Transcription => "transcription",
            Self::Diarization => "diarization",
        }
    }
}

impl FromStr for CapabilityId {
    type Err = RouteError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "legacy-office" => Ok(Self::LegacyOffice),
            "ocr" => Ok(Self::Ocr),
            "transcription" => Ok(Self::Transcription),
            "diarization" => Ok(Self::Diarization),
            _ => Err(RouteError::InvalidSource(value.into())),
        }
    }
}

/// One exact capability implementation source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapabilitySourceRef {
    /// Capability implemented by a signed local plugin.
    Plugin {
        /// Signed package identity.
        plugin_id: String,
        /// Package-local capability identity.
        capability_id: String,
    },
    /// Capability implemented by a configured remote provider.
    Provider {
        /// Provider configuration identity.
        provider_id: String,
        /// Provider capability identity.
        capability_id: String,
    },
    /// Capability is explicitly disabled.
    Off,
}

impl std::fmt::Display for CapabilitySourceRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plugin { plugin_id, capability_id } => {
                write!(formatter, "plugin:{plugin_id}/{capability_id}")
            }
            Self::Provider { provider_id, capability_id } => {
                write!(formatter, "provider:{provider_id}/{capability_id}")
            }
            Self::Off => formatter.write_str("off"),
        }
    }
}

impl FromStr for CapabilitySourceRef {
    type Err = RouteError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "off" {
            return Ok(Self::Off);
        }
        let (kind, qualified) =
            value.split_once(':').map_or(("plugin", value), |(kind, qualified)| (kind, qualified));
        let (source, capability) = qualified
            .split_once('/')
            .filter(|(source, capability)| {
                valid_id(source) && valid_id(capability) && !capability.contains('/')
            })
            .ok_or_else(|| RouteError::InvalidSource(value.into()))?;
        match kind {
            "plugin" => {
                Ok(Self::Plugin { plugin_id: source.into(), capability_id: capability.into() })
            }
            "provider" => {
                Ok(Self::Provider { provider_id: source.into(), capability_id: capability.into() })
            }
            _ => Err(RouteError::InvalidSource(value.into())),
        }
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Per-capability routing behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityRouteMode {
    /// Do not invoke any source.
    Off,
    /// Use the ordered fallback only when the primary cannot serve the request.
    Fallback,
    /// Prefer the primary and recover through ordered fallbacks.
    Prefer,
    /// Require the primary and fail closed.
    Only,
}

/// Ordered heterogeneous sources for one product capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedCapabilityRoute {
    /// Product capability being routed.
    pub capability: CapabilityId,
    /// Invocation behavior.
    pub mode: CapabilityRouteMode,
    /// Preferred exact source.
    pub primary: CapabilitySourceRef,
    /// Ordered recovery sources.
    pub fallbacks: Vec<CapabilitySourceRef>,
}

impl UnifiedCapabilityRoute {
    /// Validate route invariants before configuration is accepted.
    ///
    /// # Errors
    ///
    /// Rejects contradictory off sources and duplicate source identities.
    pub fn validate(&self) -> Result<(), RouteError> {
        if self.mode == CapabilityRouteMode::Off {
            if self.primary != CapabilitySourceRef::Off || !self.fallbacks.is_empty() {
                return Err(RouteError::InvalidSource(
                    "off mode must contain only the off source".into(),
                ));
            }
            return Ok(());
        }
        if self.primary == CapabilitySourceRef::Off
            || self.fallbacks.contains(&CapabilitySourceRef::Off)
        {
            return Err(RouteError::InvalidSource(
                "active routes cannot contain the off source".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for source in std::iter::once(&self.primary).chain(&self.fallbacks) {
            if !seen.insert(source) {
                return Err(RouteError::InvalidSource(format!("duplicate source {source}")));
            }
            let source_capability = match source {
                CapabilitySourceRef::Plugin { capability_id, .. }
                | CapabilitySourceRef::Provider { capability_id, .. } => capability_id.as_str(),
                CapabilitySourceRef::Off => continue,
            };
            if !self.capability.accepts_source_capability(source_capability) {
                return Err(RouteError::InvalidSource(format!(
                    "source {source} cannot implement {}",
                    self.capability.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Sources eligible during pre-execution readiness resolution.
    ///
    /// # Errors
    ///
    /// Returns route validation failures before exposing an iterator.
    pub fn eligible_sources(&self) -> Result<Vec<&CapabilitySourceRef>, RouteError> {
        self.validate()?;
        Ok(match self.mode {
            CapabilityRouteMode::Off => Vec::new(),
            CapabilityRouteMode::Only => vec![&self.primary],
            CapabilityRouteMode::Fallback | CapabilityRouteMode::Prefer => {
                std::iter::once(&self.primary).chain(&self.fallbacks).collect()
            }
        })
    }
}

impl CapabilityId {
    fn accepts_source_capability(self, source: &str) -> bool {
        match self {
            Self::LegacyOffice => source == "legacy-office",
            Self::Ocr => matches!(source, "ocr" | "vision-ocr"),
            Self::Transcription => matches!(source, "transcription" | "audio-transcription"),
            Self::Diarization => source == "diarization",
        }
    }
}

/// One configured self-contained local provider capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReference {
    /// Plugin package ID.
    pub plugin_id: String,
    /// Package-local capability ID.
    pub capability_id: String,
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
    /// Product capability source reference is malformed or contradictory.
    #[error("invalid capability source: {0}")]
    InvalidSource(String),
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
        Ok(ProviderBinding {
            plugin_id: plugin.manifest.id.clone(),
            plugin_version: plugin.manifest.version.clone(),
            manifest_sha256: plugin.manifest_sha256.clone(),
            capability_id: capability.id.clone(),
            provider_id: capability.provider_id.clone(),
            install_root: plugin.install_root.clone(),
        })
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
        CAPABILITY_PROTOCOL, HostApiRange, PluginCapabilityDescriptor, PluginFileDescriptor,
        PluginPermissions, PluginTargetDescriptor, ResourceEnvelope,
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
                resources: ResourceEnvelope {
                    max_input_bytes: 1024,
                    max_output_bytes: 1024,
                    max_memory_bytes: 32 * 1024 * 1024,
                    max_temporary_bytes: 1024,
                    timeout_ms: 1000,
                },
            }],
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
            },
            fallbacks: vec![ProviderReference {
                plugin_id: "fallback.plugin".into(),
                capability_id: "ocr".into(),
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

    #[test]
    fn heterogeneous_sources_have_one_strict_canonical_contract() {
        let plugin: CapabilitySourceRef = "plugin:official.ocr/ocr".parse().unwrap();
        let provider: CapabilitySourceRef = "provider:bailian/vision-ocr".parse().unwrap();
        assert_eq!(plugin.to_string(), "plugin:official.ocr/ocr");
        assert_eq!(provider.to_string(), "provider:bailian/vision-ocr");
        assert_eq!("official.ocr/ocr".parse::<CapabilitySourceRef>().unwrap(), plugin);
        for invalid in ["", "core:x/y", "provider:/ocr", "provider:x/ocr/extra"] {
            assert!(invalid.parse::<CapabilitySourceRef>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn unified_route_rejects_off_conflicts_and_duplicate_sources() {
        let source: CapabilitySourceRef = "provider:bailian/vision-ocr".parse().unwrap();
        let route = UnifiedCapabilityRoute {
            capability: CapabilityId::Ocr,
            mode: CapabilityRouteMode::Prefer,
            primary: source.clone(),
            fallbacks: vec![source],
        };
        assert!(route.validate().is_err());

        let off = UnifiedCapabilityRoute {
            capability: CapabilityId::Ocr,
            mode: CapabilityRouteMode::Off,
            primary: CapabilitySourceRef::Off,
            fallbacks: Vec::new(),
        };
        assert!(off.eligible_sources().unwrap().is_empty());

        let incompatible = UnifiedCapabilityRoute {
            capability: CapabilityId::Ocr,
            mode: CapabilityRouteMode::Only,
            primary: "provider:bailian/audio-transcription".parse().unwrap(),
            fallbacks: Vec::new(),
        };
        assert!(incompatible.validate().is_err());
    }
}
