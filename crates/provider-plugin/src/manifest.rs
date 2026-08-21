use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read as _;
use std::path::{Component, Path};
use url::Url;

/// Exact manifest schema understood by this host.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
/// Stable capability-provider protocol name.
pub const CAPABILITY_PROTOCOL: &str = "capability-provider";
/// Hash-bound provider descriptor stored inside a generic signed process package.
pub const PROVIDER_MANIFEST_NAME: &str = "provider.json";
const MAX_PLUGIN_FILES: usize = 10_000;
const MAX_CAPABILITIES: usize = 64;
const MAX_MODELS: usize = 128;
const MAX_MODEL_ARTIFACTS: usize = 128;
const MAX_DECLARED_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Host API range accepted by a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostApiRange {
    /// Lowest compatible host API revision.
    pub minimum: u32,
    /// Highest compatible host API revision.
    pub maximum: u32,
}

/// Capability implemented by an isolated provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityKind {
    /// Image or rendered-page OCR.
    Ocr,
    /// Time-aligned speech transcription.
    Transcription,
    /// Anonymous speaker diarization over a transcript.
    Diarization,
}

/// Complete host-enforced resource envelope for one capability request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceEnvelope {
    /// Maximum source bytes.
    pub max_input_bytes: u64,
    /// Maximum encoded response bytes.
    pub max_output_bytes: u64,
    /// Maximum worker address space.
    pub max_memory_bytes: u64,
    /// Maximum request-private temporary bytes.
    pub max_temporary_bytes: u64,
    /// Hard request duration.
    pub timeout_ms: u64,
}

/// One capability exported by a plugin package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginCapabilityDescriptor {
    /// Stable package-local capability ID.
    pub id: String,
    /// Typed provider contract.
    pub kind: CapabilityKind,
    /// Stable provenance provider ID.
    pub provider_id: String,
    /// Supported BCP-47 language tags or `multilingual`.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Accepted MIME media types.
    #[serde(default)]
    pub media_types: Vec<String>,
    /// Model bundles offered by this provider.
    #[serde(default)]
    pub model_bundles: Vec<String>,
    /// Host-enforced request limits.
    pub resources: ResourceEnvelope,
}

/// A target-specific package file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginFileDescriptor {
    /// Portable relative archive and installation path.
    pub path: String,
    /// Exact byte length.
    pub bytes: u64,
    /// Lowercase SHA-256.
    pub sha256: String,
    /// Whether the installer should mark this file executable.
    #[serde(default)]
    pub executable: bool,
}

/// Platform payload within one package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginTargetDescriptor {
    /// Rust target triple.
    pub triple: String,
    /// Entrypoint below the installed package root.
    pub entrypoint: String,
    /// Complete target file inventory, including the entrypoint.
    pub files: Vec<PluginFileDescriptor>,
}

/// One model or runtime artifact declared by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelArtifactDescriptor {
    /// Package-specific role such as `model`, `detector`, or `vad`.
    pub role: String,
    /// Portable final filename.
    pub file_name: String,
    /// Exact byte length.
    pub bytes: u64,
    /// Lowercase SHA-256.
    pub sha256: String,
    /// Optional pinned HTTPS acquisition URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// SPDX license expression or reviewed identifier.
    pub license: String,
}

/// Installable model bundle owned by a provider plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelBundleDescriptor {
    /// Stable package-local bundle ID.
    pub id: String,
    /// Human-readable upstream identity.
    pub upstream_version: String,
    /// Platforms on which this exact bundle is supported.
    pub platforms: Vec<String>,
    /// Complete artifact inventory.
    pub artifacts: Vec<ModelArtifactDescriptor>,
}

/// Ambient capabilities requested by a package. All default to denied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginPermissions {
    /// Allow outbound network during conversion.
    pub network: bool,
    /// Allow a persistent model-caching worker.
    pub persistent_worker: bool,
    /// Allow the provider to launch authenticated helper executables from its own package.
    pub child_processes: bool,
}

/// Signed package manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginManifest {
    /// Manifest schema revision.
    pub schema_version: u32,
    /// Stable lowercase package ID.
    pub id: String,
    /// Package version.
    pub version: String,
    /// Stable publisher identity whose key signs the manifest bytes.
    pub publisher: String,
    /// Compatible host API range.
    pub host_api: HostApiRange,
    /// Must be [`CAPABILITY_PROTOCOL`].
    pub protocol: String,
    /// Target-specific executable payloads.
    pub targets: Vec<PluginTargetDescriptor>,
    /// Exported provider capabilities.
    pub capabilities: Vec<PluginCapabilityDescriptor>,
    /// Model bundles declared by this package.
    #[serde(default)]
    pub models: Vec<ModelBundleDescriptor>,
    /// Explicit ambient authority.
    #[serde(default)]
    pub permissions: PluginPermissions,
    /// Complete package license identifiers.
    pub licenses: Vec<String>,
}

impl PluginManifest {
    /// Validate all cross-field, path, identity, and resource invariants.
    ///
    /// # Errors
    ///
    /// Returns a stable, non-secret reason when the package authority is malformed.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION
            || !valid_id(&self.id, 128)
            || !valid_id(&self.publisher, 128)
            || !valid_version(&self.version)
            || self.host_api.minimum == 0
            || self.host_api.minimum > self.host_api.maximum
            || self.protocol != CAPABILITY_PROTOCOL
            || self.targets.is_empty()
            || self.targets.len() > 16
            || self.capabilities.is_empty()
            || self.capabilities.len() > MAX_CAPABILITIES
            || self.models.len() > MAX_MODELS
            || self.licenses.is_empty()
            || self.licenses.iter().any(|value| value.is_empty() || value.len() > 256)
        {
            return Err("manifest identity, compatibility, or inventory is invalid".into());
        }
        self.validate_targets()?;
        self.validate_models()?;
        self.validate_capabilities()
    }

    /// Return the exact current-target descriptor when present.
    #[must_use]
    pub fn target(&self, triple: &str) -> Option<&PluginTargetDescriptor> {
        self.targets.iter().find(|target| target.triple == triple)
    }

    fn validate_targets(&self) -> Result<(), String> {
        let mut triples = BTreeSet::new();
        for target in &self.targets {
            if !valid_target(&target.triple)
                || !triples.insert(&target.triple)
                || !portable_path(&target.entrypoint)
                || target.files.is_empty()
                || target.files.len() > MAX_PLUGIN_FILES
            {
                return Err("target identity or entrypoint is invalid".into());
            }
            let mut paths = BTreeSet::new();
            let mut folded = BTreeSet::new();
            let mut total = 0_u64;
            for file in &target.files {
                if !portable_path(&file.path)
                    || file.bytes == 0
                    || !valid_sha256(&file.sha256)
                    || !paths.insert(&file.path)
                    || !folded.insert(file.path.to_ascii_lowercase())
                {
                    return Err("target file authority is invalid or ambiguous".into());
                }
                total = total
                    .checked_add(file.bytes)
                    .filter(|bytes| *bytes <= MAX_DECLARED_BYTES)
                    .ok_or_else(|| "target byte inventory exceeds the package limit".to_owned())?;
            }
            let Some(entrypoint) = target.files.iter().find(|file| file.path == target.entrypoint)
            else {
                return Err("target entrypoint is absent from the file inventory".into());
            };
            if !entrypoint.executable {
                return Err("target entrypoint is not executable".into());
            }
        }
        Ok(())
    }

    fn validate_models(&self) -> Result<(), String> {
        let mut ids = BTreeSet::new();
        for model in &self.models {
            if !valid_id(&model.id, 128)
                || !ids.insert(&model.id)
                || model.upstream_version.is_empty()
                || model.upstream_version.len() > 1024
                || model.platforms.is_empty()
                || model.artifacts.is_empty()
                || model.artifacts.len() > MAX_MODEL_ARTIFACTS
            {
                return Err("model bundle identity or inventory is invalid".into());
            }
            let mut platforms = BTreeSet::new();
            if model
                .platforms
                .iter()
                .any(|platform| !valid_target(platform) || !platforms.insert(platform.as_str()))
            {
                return Err("model platform inventory is invalid".into());
            }
            let mut roles = BTreeSet::new();
            let mut names = BTreeSet::new();
            for artifact in &model.artifacts {
                if !valid_id(&artifact.role, 64)
                    || !roles.insert(&artifact.role)
                    || !portable_component(&artifact.file_name)
                    || !names.insert(artifact.file_name.to_ascii_lowercase())
                    || artifact.bytes == 0
                    || artifact.bytes > MAX_DECLARED_BYTES
                    || !valid_sha256(&artifact.sha256)
                    || artifact.license.is_empty()
                    || artifact.license.len() > 256
                {
                    return Err("model artifact authority is invalid or ambiguous".into());
                }
                if let Some(url) = &artifact.url {
                    let parsed = Url::parse(url).map_err(|_| "model URL is invalid")?;
                    if parsed.scheme() != "https"
                        || parsed.host_str().is_none()
                        || !parsed.username().is_empty()
                        || parsed.password().is_some()
                        || parsed.fragment().is_some()
                    {
                        return Err("model URL is not an authenticated HTTPS authority".into());
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_capabilities(&self) -> Result<(), String> {
        let model_ids = self.models.iter().map(|model| model.id.as_str()).collect::<BTreeSet<_>>();
        let mut ids = BTreeSet::new();
        let mut providers = BTreeSet::new();
        for capability in &self.capabilities {
            let resources = &capability.resources;
            if !valid_id(&capability.id, 128)
                || !ids.insert(&capability.id)
                || !valid_provider_id(&capability.provider_id)
                || !providers.insert(&capability.provider_id)
                || resources.max_input_bytes == 0
                || resources.max_input_bytes > MAX_DECLARED_BYTES
                || resources.max_output_bytes == 0
                || resources.max_output_bytes > 24 * 1024 * 1024
                || resources.max_memory_bytes < 32 * 1024 * 1024
                || resources.max_memory_bytes > MAX_DECLARED_BYTES
                || resources.max_temporary_bytes > MAX_DECLARED_BYTES
                || resources.timeout_ms == 0
                || resources.timeout_ms > 24 * 60 * 60 * 1000
                || capability.languages.len() > 256
                || capability.media_types.len() > 256
                || capability.model_bundles.len() > MAX_MODELS
                || capability.model_bundles.iter().any(|model| !model_ids.contains(model.as_str()))
            {
                return Err("capability identity, models, or resource envelope is invalid".into());
            }
            if capability.languages.iter().any(|value| !valid_language(value))
                || capability.media_types.iter().any(|value| !valid_media_type(value))
            {
                return Err("capability language or media-type inventory is invalid".into());
            }
        }
        Ok(())
    }
}

/// Load and validate the capability descriptor from a manager-verified installation.
///
/// The generic plugin manager authenticates the complete tree, including this file. This loader
/// additionally binds the inner provider identity and version to the signed outer package.
pub fn load_installed_manifest(
    installed: &into_markdown_plugin_manager::InstalledPlugin,
) -> Result<(PluginManifest, String), String> {
    if installed.protocol != "process-v1" {
        return Err("capability provider is not a process-v1 package".into());
    }
    let path = installed.root.join(PROVIDER_MANIFEST_NAME);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| "capability provider descriptor is unavailable".to_owned())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024 {
        return Err("capability provider descriptor is not a bounded regular file".into());
    }
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| "capability provider descriptor is too large".to_owned())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| "capability provider descriptor allocation failed".to_owned())?;
    File::open(&path)
        .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|_| "capability provider descriptor cannot be read".to_owned())?;
    if bytes.len() as u64 != metadata.len() {
        return Err("capability provider descriptor changed while reading".into());
    }
    let manifest: PluginManifest = serde_json::from_slice(&bytes)
        .map_err(|_| "capability provider descriptor is invalid".to_owned())?;
    manifest.validate()?;
    if manifest.id != installed.id || manifest.version != installed.version {
        return Err("capability provider identity differs from its signed package".into());
    }
    Ok((manifest, format!("{:x}", Sha256::digest(&bytes))))
}

fn valid_id(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn valid_provider_id(value: &str) -> bool {
    valid_id(value, 192) && value.contains('.')
}

fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

fn valid_target(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn portable_path(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 4096
        || value.contains('\\')
        || Path::new(value).is_absolute()
        || Path::new(value).components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return false;
    }
    value.split('/').all(portable_component)
}

fn portable_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 240
        && value.is_ascii()
        && value.trim_end_matches([' ', '.']) == value
        && !value.bytes().any(|byte| {
            byte < 0x20 || matches!(byte, b'<' | b'>' | b':' | b'"' | b'|' | b'?' | b'*')
        })
        && !matches!(
            value.split('.').next().unwrap_or(value).to_ascii_lowercase().as_str(),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        )
}

fn valid_language(value: &str) -> bool {
    value == "multilingual"
        || (!value.is_empty()
            && value.len() <= 64
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
}

fn valid_media_type(value: &str) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else { return false };
    !kind.is_empty()
        && !subtype.is_empty()
        && value.len() <= 127
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'/' | b'.' | b'+' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        PluginManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            id: "official.ocr.ppocrv6".into(),
            version: "3.0.0".into(),
            publisher: "into-markdown".into(),
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
                id: "ocr".into(),
                kind: CapabilityKind::Ocr,
                provider_id: "official.ocr.ppocrv6.image".into(),
                languages: vec!["zh-Hans".into(), "en".into()],
                media_types: vec!["image/png".into()],
                model_bundles: vec!["pp-ocrv6-tiny-zh-en".into()],
                resources: ResourceEnvelope {
                    max_input_bytes: 128 * 1024 * 1024,
                    max_output_bytes: 24 * 1024 * 1024,
                    max_memory_bytes: 1024 * 1024 * 1024,
                    max_temporary_bytes: 128 * 1024 * 1024,
                    timeout_ms: 60_000,
                },
            }],
            models: vec![ModelBundleDescriptor {
                id: "pp-ocrv6-tiny-zh-en".into(),
                upstream_version: "PP-OCRv6 tiny".into(),
                platforms: vec!["aarch64-apple-darwin".into()],
                artifacts: vec![ModelArtifactDescriptor {
                    role: "detector".into(),
                    file_name: "detector.onnx".into(),
                    bytes: 1,
                    sha256: "b".repeat(64),
                    url: Some("https://example.invalid/detector.onnx".into()),
                    license: "Apache-2.0".into(),
                }],
            }],
            permissions: PluginPermissions::default(),
            licenses: vec!["Apache-2.0".into()],
        }
    }

    #[test]
    fn validates_complete_capability_authority() {
        manifest().validate().unwrap();
    }

    #[test]
    fn rejects_path_aliases_unknown_models_and_ambient_http() {
        let mut value = manifest();
        value.targets[0].files.push(PluginFileDescriptor {
            path: "BIN/PROVIDER".into(),
            bytes: 1,
            sha256: "c".repeat(64),
            executable: false,
        });
        assert!(value.validate().is_err());
        let mut value = manifest();
        value.capabilities[0].model_bundles = vec!["missing".into()];
        assert!(value.validate().is_err());
        let mut value = manifest();
        value.models[0].artifacts[0].url = Some("http://example.invalid/model".into());
        assert!(value.validate().is_err());
    }
}
