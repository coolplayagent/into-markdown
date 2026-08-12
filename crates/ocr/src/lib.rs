//! Local OCR model metadata and management.
//!
//! The embedded manifests are the only authority used by this crate. Source
//! archives are never treated as installable runtime models.
#![allow(missing_docs, reason = "manifest fields preserve the documented JSON authority names")]
#![allow(
    clippy::large_stack_arrays,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    reason = "streaming supply-chain operations keep validation and fixed buffers auditable"
)]

use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, ExecutionStage, OcrEngine, OcrRequest, OcrResult,
    Tensor, TensorRuntime,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SUPPORTED_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];

/// One downloadable upstream source artifact. It is not installable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifact {
    pub id: String,
    pub role: String,
    pub url: String,
    pub sha256: String,
    pub format: String,
    pub license: String,
}

/// Character-table provenance for a runtime bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharacterSet {
    pub status: String,
    pub source_artifact_id: String,
}

/// A final runtime file that must agree with the download authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifact {
    pub id: String,
    pub role: String,
    pub file_name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub platforms: Vec<String>,
    pub license: String,
}

/// OCR model bundle and its supply-chain contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBundle {
    pub id: String,
    pub availability: String,
    pub upstream_version: String,
    pub languages: Vec<String>,
    pub platforms: Vec<String>,
    pub runtime_format: String,
    pub character_set: CharacterSet,
    pub runtime_artifacts: Vec<RuntimeArtifact>,
    pub source_artifacts: Vec<ModelArtifact>,
}

/// Versioned model manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelManifest {
    pub schema_version: u32,
    pub default_bundle: String,
    pub bundles: Vec<ModelBundle>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadManifest {
    schema_version: u32,
    model_files: Vec<SourceDownload>,
    model_runtime_files: Vec<RuntimeDownload>,
    native_archives: Vec<NativeDownload>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceDownload {
    artifact_id: String,
    repository: String,
    downloaded_file_path: String,
    url: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeDownload {
    artifact_id: String,
    repository: String,
    downloaded_file_path: String,
    url: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeDownload {
    target: String,
    repository: String,
    url: String,
    sha256: String,
    strip_prefix: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct InstallState {
    schema_version: u32,
    bundle_id: String,
    complete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct InstallJournal {
    schema_version: u32,
    bundle_id: String,
    nonce: String,
    staging_name: String,
    backup_name: String,
}

impl ModelManifest {
    /// Parses and cross-validates both embedded authoritative manifests.
    pub fn embedded() -> Result<Self, ConversionError> {
        Self::from_authorities(
            include_str!("../../../models/manifest.json"),
            include_str!("../../../third_party/licenses/downloads.json"),
        )
    }

    fn from_authorities(models: &str, downloads: &str) -> Result<Self, ConversionError> {
        let manifest: Self = serde_json::from_str(models)
            .map_err(|error| invalid_manifest(format!("invalid model JSON: {error}")))?;
        let downloads: DownloadManifest = serde_json::from_str(downloads)
            .map_err(|error| invalid_manifest(format!("invalid download JSON: {error}")))?;
        manifest.validate_against(&downloads)?;
        Ok(manifest)
    }

    fn validate_against(&self, downloads: &DownloadManifest) -> Result<(), ConversionError> {
        if self.schema_version != 1 || downloads.schema_version != 1 {
            return Err(invalid_manifest("unsupported schema version"));
        }
        let mut bundle_ids = BTreeSet::new();
        let mut source_ids = BTreeSet::new();
        let mut runtime_ids = BTreeSet::new();
        let sources = unique_sources(downloads)?;
        let runtimes = unique_runtimes(downloads)?;
        validate_native_downloads(downloads)?;
        for bundle in &self.bundles {
            validate_id(&bundle.id)?;
            if !bundle_ids.insert(bundle.id.as_str()) {
                return Err(invalid_manifest(format!("duplicate bundle ID {}", bundle.id)));
            }
            if !matches!(bundle.availability.as_str(), "planned" | "available") {
                return Err(invalid_manifest(format!("invalid availability for {}", bundle.id)));
            }
            let targets: BTreeSet<_> = bundle.platforms.iter().map(String::as_str).collect();
            if bundle.platforms.len() != SUPPORTED_TARGETS.len()
                || targets != BTreeSet::from(SUPPORTED_TARGETS)
            {
                return Err(invalid_manifest(format!(
                    "{} must declare exactly four targets",
                    bundle.id
                )));
            }
            if bundle.languages.is_empty()
                || bundle.runtime_format.is_empty()
                || bundle.upstream_version.is_empty()
            {
                return Err(invalid_manifest(format!("{} has incomplete metadata", bundle.id)));
            }
            if !matches!(bundle.character_set.status.as_str(), "planned" | "available") {
                return Err(invalid_manifest(format!(
                    "{} has invalid character set status",
                    bundle.id
                )));
            }
            for artifact in &bundle.source_artifacts {
                validate_id(&artifact.id)?;
                if !source_ids.insert(artifact.id.as_str()) {
                    return Err(invalid_manifest(format!(
                        "duplicate source artifact {}",
                        artifact.id
                    )));
                }
                validate_https(&artifact.url)?;
                validate_hash(&artifact.sha256)?;
                if artifact.role.is_empty()
                    || artifact.format.is_empty()
                    || artifact.license.is_empty()
                {
                    return Err(invalid_manifest(format!(
                        "source artifact {} is incomplete",
                        artifact.id
                    )));
                }
                match sources.get(artifact.id.as_str()) {
                    Some(item) if item.url == artifact.url && item.sha256 == artifact.sha256 => {}
                    _ => {
                        return Err(invalid_manifest(format!(
                            "source artifact {} disagrees with downloads.json",
                            artifact.id
                        )));
                    }
                }
            }
            if !bundle
                .source_artifacts
                .iter()
                .any(|item| item.id == bundle.character_set.source_artifact_id)
            {
                return Err(invalid_manifest(format!(
                    "{} character set source is absent",
                    bundle.id
                )));
            }
            for artifact in &bundle.runtime_artifacts {
                validate_runtime(artifact)?;
                if !runtime_ids.insert(artifact.id.as_str()) {
                    return Err(invalid_manifest(format!(
                        "duplicate runtime artifact {}",
                        artifact.id
                    )));
                }
                match runtimes.get(artifact.id.as_str()) {
                    Some(item)
                        if item.url == artifact.url
                            && item.sha256 == artifact.sha256
                            && item.downloaded_file_path == artifact.file_name
                            && item.size == artifact.size => {}
                    _ => {
                        return Err(invalid_manifest(format!(
                            "runtime artifact {} disagrees with downloads.json",
                            artifact.id
                        )));
                    }
                }
            }
            let installable = bundle.availability == "available"
                && bundle.character_set.status == "available"
                && !bundle.runtime_artifacts.is_empty();
            if (bundle.availability == "available") != installable {
                return Err(invalid_manifest(format!(
                    "{} claims availability without complete runtime artifacts",
                    bundle.id
                )));
            }
        }
        if !bundle_ids.contains(self.default_bundle.as_str()) {
            return Err(invalid_manifest("default bundle is absent"));
        }
        if sources.len() != source_ids.len() || runtimes.len() != runtime_ids.len() {
            return Err(invalid_manifest(
                "downloads.json contains orphan or duplicate model entries",
            ));
        }
        Ok(())
    }
}

fn unique_sources(
    downloads: &DownloadManifest,
) -> Result<BTreeMap<&str, &SourceDownload>, ConversionError> {
    let mut result = BTreeMap::new();
    let mut repositories = BTreeSet::new();
    for item in &downloads.model_files {
        validate_id(&item.artifact_id)?;
        validate_id(&item.repository)?;
        validate_file_name(&item.downloaded_file_path)?;
        validate_https(&item.url)?;
        validate_hash(&item.sha256)?;
        if result.insert(item.artifact_id.as_str(), item).is_some()
            || !repositories.insert(item.repository.as_str())
        {
            return Err(invalid_manifest("duplicate source download ID or repository"));
        }
    }
    Ok(result)
}

fn unique_runtimes(
    downloads: &DownloadManifest,
) -> Result<BTreeMap<&str, &RuntimeDownload>, ConversionError> {
    let mut result = BTreeMap::new();
    let mut repositories = BTreeSet::new();
    for item in &downloads.model_runtime_files {
        validate_id(&item.artifact_id)?;
        validate_id(&item.repository)?;
        validate_file_name(&item.downloaded_file_path)?;
        validate_https(&item.url)?;
        validate_hash(&item.sha256)?;
        if item.size == 0
            || result.insert(item.artifact_id.as_str(), item).is_some()
            || !repositories.insert(item.repository.as_str())
        {
            return Err(invalid_manifest("invalid or duplicate runtime download"));
        }
    }
    Ok(result)
}

fn validate_native_downloads(downloads: &DownloadManifest) -> Result<(), ConversionError> {
    let mut targets = BTreeSet::new();
    let mut repositories = BTreeSet::new();
    for item in &downloads.native_archives {
        validate_id(&item.repository)?;
        validate_file_name(&item.strip_prefix)?;
        validate_https(&item.url)?;
        validate_hash(&item.sha256)?;
        if !SUPPORTED_TARGETS.contains(&item.target.as_str())
            || !targets.insert(item.target.as_str())
            || !repositories.insert(item.repository.as_str())
        {
            return Err(invalid_manifest("invalid or duplicate native download"));
        }
    }
    if targets != BTreeSet::from(SUPPORTED_TARGETS) {
        return Err(invalid_manifest("native downloads must declare exactly four targets"));
    }
    Ok(())
}

fn validate_runtime(artifact: &RuntimeArtifact) -> Result<(), ConversionError> {
    validate_id(&artifact.id)?;
    validate_file_name(&artifact.file_name)?;
    validate_https(&artifact.url)?;
    validate_hash(&artifact.sha256)?;
    if artifact.size == 0 || artifact.role.is_empty() || artifact.license.is_empty() {
        return Err(invalid_manifest(format!("runtime artifact {} is incomplete", artifact.id)));
    }
    let platforms: BTreeSet<_> = artifact.platforms.iter().map(String::as_str).collect();
    if platforms.is_empty()
        || platforms.len() != artifact.platforms.len()
        || !platforms.iter().all(|target| SUPPORTED_TARGETS.contains(target))
    {
        return Err(invalid_manifest(format!(
            "runtime artifact {} has invalid platforms",
            artifact.id
        )));
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), ConversionError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        return Err(invalid_manifest(format!("unsafe identifier {value:?}")));
    }
    Ok(())
}

fn validate_file_name(value: &str) -> Result<(), ConversionError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(invalid_manifest(format!("unsafe file name {value:?}")));
    }
    Ok(())
}

fn validate_https(value: &str) -> Result<(), ConversionError> {
    let url = url::Url::parse(value)
        .map_err(|error| invalid_manifest(format!("invalid URL: {error}")))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_manifest(
            "model URL must be canonical HTTPS without credentials/query/fragment",
        ));
    }
    Ok(())
}

fn validate_hash(value: &str) -> Result<(), ConversionError> {
    if value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_manifest("SHA-256 must be 64 lowercase hexadecimal digits"));
    }
    Ok(())
}

fn invalid_manifest(detail: impl Into<String>) -> ConversionError {
    ConversionError::Internal { detail: format!("model manifest: {}", detail.into()) }
}

/// Product target used by the pure data-directory resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductTarget {
    MacOsArm64,
    LinuxX86_64,
    LinuxArm64,
    WindowsX86_64,
}

/// Environment values used for deterministic platform path resolution.
#[derive(Debug, Clone, Default)]
pub struct DataDirectoryEnvironment {
    pub xdg_data_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
}

/// Resolves the writable model root without accessing process environment.
pub fn model_data_directory(
    target: ProductTarget,
    env: &DataDirectoryEnvironment,
) -> Result<PathBuf, ModelManagerError> {
    let base = match target {
        ProductTarget::MacOsArm64 => {
            env.home.as_ref().map(|home| home.join("Library/Application Support"))
        }
        ProductTarget::LinuxX86_64 | ProductTarget::LinuxArm64 => env
            .xdg_data_home
            .clone()
            .or_else(|| env.home.as_ref().map(|home| home.join(".local/share"))),
        ProductTarget::WindowsX86_64 => env.local_app_data.clone(),
    }
    .ok_or(ModelManagerError::DataDirectoryUnavailable)?;
    if !base.is_absolute() {
        return Err(ModelManagerError::DataDirectoryUnsafe);
    }
    Ok(base.join("into-markdown/models"))
}

/// Stable model-management error.
#[derive(Debug, thiserror::Error)]
pub enum ModelManagerError {
    #[error("model bundle is unknown")]
    UnknownBundle,
    #[error("model bundle has no reviewed runtime artifacts")]
    ComponentUnavailable,
    #[error("model bundle is not installed")]
    NotInstalled,
    #[error("bundled model is read-only")]
    ReadOnly,
    #[error("model data directory is unavailable")]
    DataDirectoryUnavailable,
    #[error("model data directory is unsafe")]
    DataDirectoryUnsafe,
    #[error("model path contains a symlink or non-directory object")]
    UnsafePath,
    #[error("installed model is incomplete or corrupt: {0}")]
    Corrupt(String),
    #[error("model operation is busy")]
    Busy,
    #[error("model execution failed: {0}")]
    Execution(#[from] ConversionError),
    #[error("model I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Opens one authoritative runtime artifact as a bounded byte stream.
///
/// Network implementations must enforce HTTPS, redirect, DNS/address, host,
/// and response-size policy before returning a stream. The manager still
/// enforces the manifest size and hash while reading.
pub trait ModelFetcher {
    /// Opens the exact runtime artifact requested by the manager.
    fn open(
        &self,
        artifact: &RuntimeArtifact,
        context: &ExecutionContext,
    ) -> Result<Box<dyn Read>, ModelManagerError>;
}

/// Observed bundle state. Inspection never accesses the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub schema_version: u32,
    pub id: String,
    pub availability: String,
    pub state: String,
    pub ownership: String,
    pub path: Option<PathBuf>,
}

/// Offline manager for model query, verification, and removal.
pub struct ModelManager {
    manifest: ModelManifest,
    writable_root: PathBuf,
    bundled_root: Option<PathBuf>,
}

impl ModelManager {
    /// Creates a manager from the cross-validated embedded authorities.
    pub fn embedded(
        writable_root: PathBuf,
        bundled_root: Option<PathBuf>,
    ) -> Result<Self, ConversionError> {
        Ok(Self::new(ModelManifest::embedded()?, writable_root, bundled_root))
    }

    fn new(manifest: ModelManifest, writable_root: PathBuf, bundled_root: Option<PathBuf>) -> Self {
        Self { manifest, writable_root, bundled_root }
    }

    /// Embedded manifest accessor.
    #[must_use]
    pub fn manifest(&self) -> &ModelManifest {
        &self.manifest
    }

    /// Returns one offline state snapshot.
    pub fn status(&self, id: &str) -> Result<ModelStatus, ModelManagerError> {
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        self.status_with_context(id, &context)
    }

    fn status_with_context(
        &self,
        id: &str,
        context: &ExecutionContext,
    ) -> Result<ModelStatus, ModelManagerError> {
        let bundle = self.bundle(id)?;
        if !is_installable(bundle) {
            return Ok(unavailable_status(bundle));
        }
        self.recover_if_present(bundle, context)?;
        if let Some(root) = &self.bundled_root {
            let path = root.join(id);
            if safe_existing_directory(&path)? {
                let state =
                    verification_state(verify_directory_with_context(bundle, &path, context))?;
                return Ok(ModelStatus {
                    schema_version: 1,
                    id: id.to_owned(),
                    availability: bundle.availability.clone(),
                    state: state.into(),
                    ownership: "bundled-read-only".into(),
                    path: Some(path),
                });
            }
        }
        let path = self.writable_root.join(id);
        if safe_existing_directory(&path)? {
            let state = verification_state(verify_directory_with_context(bundle, &path, context))?;
            return Ok(ModelStatus {
                schema_version: 1,
                id: id.to_owned(),
                availability: bundle.availability.clone(),
                state: state.into(),
                ownership: "user".into(),
                path: Some(path),
            });
        }
        Ok(ModelStatus {
            schema_version: 1,
            id: id.to_owned(),
            availability: bundle.availability.clone(),
            state: "not-installed".into(),
            ownership: "none".into(),
            path: None,
        })
    }

    /// Returns all states in manifest order.
    pub fn list(&self) -> Result<Vec<ModelStatus>, ModelManagerError> {
        self.manifest.bundles.iter().map(|bundle| self.status(&bundle.id)).collect()
    }

    /// Verifies one installed bundle using local bytes only.
    pub fn verify(&self, id: &str) -> Result<ModelStatus, ModelManagerError> {
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        self.verify_with_context(id, &context)
    }

    /// Verifies one installed bundle with caller cancellation and timeout.
    pub fn verify_with_context(
        &self,
        id: &str,
        context: &ExecutionContext,
    ) -> Result<ModelStatus, ModelManagerError> {
        self.require_installable(id)?;
        let status = self.status_with_context(id, context)?;
        let path = status.path.as_ref().ok_or(ModelManagerError::NotInstalled)?;
        verify_directory_with_context(self.bundle(id)?, path, context)?;
        Ok(status)
    }

    /// Returns the path only for a complete installed bundle.
    pub fn path(&self, id: &str) -> Result<PathBuf, ModelManagerError> {
        self.verify(id)?.path.ok_or(ModelManagerError::NotInstalled)
    }

    /// Removes only a user-owned directory while holding an interprocess lock.
    pub fn remove(&self, id: &str) -> Result<(), ModelManagerError> {
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        self.remove_with_context(id, &context)
    }

    /// Removes a user-owned directory with caller cancellation and timeout.
    pub fn remove_with_context(
        &self,
        id: &str,
        context: &ExecutionContext,
    ) -> Result<(), ModelManagerError> {
        context.checkpoint()?;
        let bundle = self.require_installable(id)?;
        if let Some(root) = &self.bundled_root
            && safe_existing_directory(&root.join(id))?
        {
            return Err(ModelManagerError::ReadOnly);
        }
        fs::create_dir_all(&self.writable_root)?;
        reject_symlink(&self.writable_root)?;
        let lock = self.acquire_lock()?;
        self.recover_locked(bundle, context)?;
        let path = self.writable_root.join(id);
        if !safe_existing_directory(&path)? {
            return Err(ModelManagerError::NotInstalled);
        }
        // A directory named after a bundle is not sufficient ownership proof.
        verify_directory_with_context(bundle, &path, context)?;
        context.checkpoint()?;
        let tombstone = self.writable_root.join(format!(".{id}.removing-{}", std::process::id()));
        if fs::symlink_metadata(&tombstone).is_ok() {
            return Err(ModelManagerError::Busy);
        }
        fs::rename(&path, &tombstone)?;
        sync_directory(&self.writable_root)?;
        let result = fs::remove_dir_all(&tombstone).map_err(ModelManagerError::Io);
        drop(lock);
        result
    }

    /// Installation is fail-closed until the runtime list is complete.
    pub fn require_installable(&self, id: &str) -> Result<&ModelBundle, ModelManagerError> {
        let bundle = self.bundle(id)?;
        if !is_installable(bundle) {
            return Err(ModelManagerError::ComponentUnavailable);
        }
        Ok(bundle)
    }

    /// Fetches, verifies, fsyncs, and atomically publishes one runtime bundle.
    ///
    /// The caller supplies the transport so normal builds and tests remain
    /// offline. Publication runs under the same interprocess lock as removal.
    pub fn install(
        &self,
        id: &str,
        fetcher: &dyn ModelFetcher,
        context: &ExecutionContext,
    ) -> Result<ModelStatus, ModelManagerError> {
        self.install_inner(id, fetcher, context, InstallFault::None, false)
    }

    fn install_inner(
        &self,
        id: &str,
        fetcher: &dyn ModelFetcher,
        context: &ExecutionContext,
        fault: InstallFault,
        force_publish: bool,
    ) -> Result<ModelStatus, ModelManagerError> {
        let bundle = self.require_installable(id)?;
        context.checkpoint()?;
        let total_size = bundle.runtime_artifacts.iter().try_fold(0_u64, |total, artifact| {
            total
                .checked_add(artifact.size)
                .ok_or_else(|| ModelManagerError::Corrupt("runtime artifact sizes overflow".into()))
        })?;
        let _temporary = context.reserve_temporary(total_size)?;
        let _memory = context.reserve_memory(64 * 1024)?;
        fs::create_dir_all(&self.writable_root)?;
        reject_symlink(&self.writable_root)?;
        let lock = self.acquire_lock()?;
        self.recover_locked(bundle, context)?;
        let final_path = self.writable_root.join(id);
        let had_old = safe_existing_directory(&final_path)?;
        if had_old {
            verify_directory_with_context(bundle, &final_path, context)?;
        }
        if had_old && !force_publish {
            drop(lock);
            return self.status_with_context(id, context);
        }

        let nonce = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
        );
        let staging_name = format!(".{id}.staging-{nonce}");
        let backup_name = format!(".{id}.backup-{nonce}");
        let staging_path = self.writable_root.join(&staging_name);
        let backup_path = self.writable_root.join(&backup_name);
        fs::create_dir(&staging_path)?;
        let staging = StagingDirectory::new(staging_path);
        for artifact in &bundle.runtime_artifacts {
            context.checkpoint()?;
            let mut source = fetcher.open(artifact, context)?;
            let path = staging.path().join(&artifact.file_name);
            let mut destination = OpenOptions::new().write(true).create_new(true).open(&path)?;
            let mut digest = Sha256::new();
            let mut received = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                context.checkpoint()?;
                let count = source.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                let count_u64 = u64::try_from(count)
                    .map_err(|_| ModelManagerError::Corrupt(artifact.file_name.clone()))?;
                received = received
                    .checked_add(count_u64)
                    .ok_or_else(|| ModelManagerError::Corrupt(artifact.file_name.clone()))?;
                if received > artifact.size {
                    return Err(ModelManagerError::Corrupt(format!(
                        "{} exceeds declared size",
                        artifact.file_name
                    )));
                }
                destination.write_all(&buffer[..count])?;
                digest.update(&buffer[..count]);
            }
            if received != artifact.size || format!("{:x}", digest.finalize()) != artifact.sha256 {
                return Err(ModelManagerError::Corrupt(format!(
                    "{} has a size or SHA-256 mismatch",
                    artifact.file_name
                )));
            }
            destination.sync_all()?;
        }
        let state = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "bundleId": id,
            "complete": true,
        }))
        .map_err(|error| ModelManagerError::Corrupt(error.to_string()))?;
        let mut state_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staging.path().join("install-state.json"))?;
        state_file.write_all(&state)?;
        state_file.sync_all()?;
        sync_directory(staging.path())?;
        context.checkpoint()?;

        let journal = InstallJournal {
            schema_version: 1,
            bundle_id: id.to_owned(),
            nonce,
            staging_name,
            backup_name,
        };
        self.write_journal(&journal)?;
        // From this point the durable journal, rather than Drop, owns cleanup.
        staging.disarm();
        if had_old {
            fs::rename(&final_path, &backup_path)?;
            sync_directory(&self.writable_root)?;
            if fault == InstallFault::AfterBackup {
                return Err(ModelManagerError::Corrupt(
                    "simulated interruption after backup".into(),
                ));
            }
        }
        fs::rename(staging.path(), &final_path)?;
        sync_directory(&self.writable_root)?;
        if fault == InstallFault::AfterPublish {
            return Err(ModelManagerError::Corrupt("simulated interruption after publish".into()));
        }
        verify_directory_with_context(bundle, &final_path, context)?;
        if had_old {
            fs::remove_dir_all(&backup_path)?;
            sync_directory(&self.writable_root)?;
        }
        self.remove_journal(id)?;
        drop(lock);
        self.status_with_context(id, context)
    }

    fn bundle(&self, id: &str) -> Result<&ModelBundle, ModelManagerError> {
        self.manifest
            .bundles
            .iter()
            .find(|bundle| bundle.id == id)
            .ok_or(ModelManagerError::UnknownBundle)
    }

    fn acquire_lock(&self) -> Result<File, ModelManagerError> {
        let path = self.writable_root.join(".models.lock");
        reject_symlink_if_present(&path)?;
        let file =
            OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
        file.try_lock().map_err(|_| ModelManagerError::Busy)?;
        Ok(file)
    }

    fn recover_if_present(
        &self,
        bundle: &ModelBundle,
        context: &ExecutionContext,
    ) -> Result<(), ModelManagerError> {
        if !safe_existing_directory(&self.writable_root)? {
            return Ok(());
        }
        let lock = self.acquire_lock()?;
        let result = self.recover_locked(bundle, context);
        drop(lock);
        result
    }

    fn recover_locked(
        &self,
        bundle: &ModelBundle,
        context: &ExecutionContext,
    ) -> Result<(), ModelManagerError> {
        context.checkpoint()?;
        let journal_path = journal_path(&self.writable_root, &bundle.id);
        let journal = match required_file_if_present(&journal_path)? {
            Some(file) => {
                Some(serde_json::from_reader::<_, InstallJournal>(file).map_err(|error| {
                    ModelManagerError::Corrupt(format!("invalid install journal: {error}"))
                })?)
            }
            None => None,
        };
        let residues = transaction_residues(&self.writable_root, &bundle.id)?;
        let Some(journal) = journal else {
            return if residues.is_empty() {
                Ok(())
            } else {
                Err(ModelManagerError::Corrupt(
                    "unowned or ambiguous install transaction residue".into(),
                ))
            };
        };
        validate_journal(bundle, &journal)?;
        let expected: BTreeSet<_> =
            [journal.staging_name.as_str(), journal.backup_name.as_str()].into_iter().collect();
        if !residues.iter().all(|name| expected.contains(name.as_str())) {
            return Err(ModelManagerError::Corrupt(
                "multiple or ambiguous install transaction residues".into(),
            ));
        }

        let final_path = self.writable_root.join(&bundle.id);
        let staging_path = self.writable_root.join(&journal.staging_name);
        let backup_path = self.writable_root.join(&journal.backup_name);
        let final_exists = safe_existing_directory(&final_path)?;
        let staging_exists = safe_existing_directory(&staging_path)?;
        let backup_exists = safe_existing_directory(&backup_path)?;

        match (final_exists, staging_exists, backup_exists) {
            // Journal was durable but the old bundle was not moved: roll back the staged update.
            (true, true, false) => {
                verify_directory_with_context(bundle, &final_path, context)?;
                fs::remove_dir_all(&staging_path)?;
                sync_directory(&self.writable_root)?;
            }
            // The old bundle was backed up: finish publishing the complete staged bundle.
            (false, true, true) => {
                verify_directory_with_context(bundle, &backup_path, context)?;
                if let Err(error) = verify_directory_with_context(bundle, &staging_path, context) {
                    if !matches!(error, ModelManagerError::Corrupt(_)) {
                        return Err(error);
                    }
                    fs::rename(&backup_path, &final_path)?;
                    sync_directory(&self.writable_root)?;
                    fs::remove_dir_all(&staging_path)?;
                    sync_directory(&self.writable_root)?;
                    self.remove_journal(&bundle.id)?;
                    return Ok(());
                }
                fs::rename(&staging_path, &final_path)?;
                sync_directory(&self.writable_root)?;
                verify_directory_with_context(bundle, &final_path, context)?;
                fs::remove_dir_all(&backup_path)?;
                sync_directory(&self.writable_root)?;
            }
            // A new install was staged and its journal was durable: finish publication.
            (false, true, false) => {
                verify_directory_with_context(bundle, &staging_path, context)?;
                fs::rename(&staging_path, &final_path)?;
                sync_directory(&self.writable_root)?;
                verify_directory_with_context(bundle, &final_path, context)?;
            }
            // Publication completed; only durable cleanup remains.
            (true, false, true) => {
                verify_directory_with_context(bundle, &final_path, context)?;
                fs::remove_dir_all(&backup_path)?;
                sync_directory(&self.writable_root)?;
            }
            (true, false, false) => {
                verify_directory_with_context(bundle, &final_path, context)?;
            }
            // Publication failed after moving the old bundle and before a new final appeared.
            (false, false, true) => {
                verify_directory_with_context(bundle, &backup_path, context)?;
                fs::rename(&backup_path, &final_path)?;
                sync_directory(&self.writable_root)?;
            }
            _ => {
                return Err(ModelManagerError::Corrupt(
                    "ambiguous install journal topology".into(),
                ));
            }
        }
        self.remove_journal(&bundle.id)
    }

    fn write_journal(&self, journal: &InstallJournal) -> Result<(), ModelManagerError> {
        let path = journal_path(&self.writable_root, &journal.bundle_id);
        reject_symlink_if_present(&path)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        serde_json::to_writer(&mut file, journal)
            .map_err(|error| ModelManagerError::Corrupt(error.to_string()))?;
        file.sync_all()?;
        sync_directory(&self.writable_root)
    }

    fn remove_journal(&self, id: &str) -> Result<(), ModelManagerError> {
        let path = journal_path(&self.writable_root, id);
        reject_symlink(&path)?;
        fs::remove_file(path)?;
        sync_directory(&self.writable_root)
    }
}

fn verify_directory_with_context(
    bundle: &ModelBundle,
    path: &Path,
    context: &ExecutionContext,
) -> Result<(), ModelManagerError> {
    context.checkpoint()?;
    reject_symlink(path)?;
    let state_path = path.join("install-state.json");
    let state_file = required_regular_file(&state_path, "install state is missing")?;
    let state: InstallState = serde_json::from_reader(state_file)
        .map_err(|error| ModelManagerError::Corrupt(error.to_string()))?;
    if state.schema_version != 1 || state.bundle_id != bundle.id || !state.complete {
        return Err(ModelManagerError::Corrupt("invalid install state".into()));
    }
    let expected: BTreeSet<_> =
        bundle.runtime_artifacts.iter().map(|artifact| artifact.file_name.as_str()).collect();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(ModelManagerError::UnsafePath)?;
        if name == "install-state.json" {
            continue;
        }
        if !file_type.is_file() || !expected.contains(name) {
            return Err(ModelManagerError::UnsafePath);
        }
    }
    for artifact in &bundle.runtime_artifacts {
        context.checkpoint()?;
        let file_path = path.join(&artifact.file_name);
        let mut file =
            required_regular_file(&file_path, &format!("{} is missing", artifact.file_name))?;
        let metadata = file.metadata()?;
        if metadata.len() != artifact.size {
            return Err(ModelManagerError::Corrupt(artifact.file_name.clone()));
        }
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            context.checkpoint()?;
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        if format!("{:x}", digest.finalize()) != artifact.sha256 {
            return Err(ModelManagerError::Corrupt(artifact.file_name.clone()));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallFault {
    None,
    AfterBackup,
    AfterPublish,
}

fn is_installable(bundle: &ModelBundle) -> bool {
    bundle.availability == "available"
        && bundle.character_set.status == "available"
        && !bundle.runtime_artifacts.is_empty()
}

fn unavailable_status(bundle: &ModelBundle) -> ModelStatus {
    ModelStatus {
        schema_version: 1,
        id: bundle.id.clone(),
        availability: bundle.availability.clone(),
        state: "unavailable".into(),
        ownership: "none".into(),
        path: None,
    }
}

fn verification_state(
    result: Result<(), ModelManagerError>,
) -> Result<&'static str, ModelManagerError> {
    match result {
        Ok(()) => Ok("installed"),
        Err(ModelManagerError::Corrupt(_) | ModelManagerError::NotInstalled) => Ok("corrupt"),
        Err(error) => Err(error),
    }
}

fn journal_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!(".{id}.install-journal.json"))
}

fn validate_journal(
    bundle: &ModelBundle,
    journal: &InstallJournal,
) -> Result<(), ModelManagerError> {
    if journal.schema_version != 1
        || journal.bundle_id != bundle.id
        || journal.nonce.is_empty()
        || journal.nonce.len() > 80
        || !journal.nonce.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-')
        || journal.staging_name != format!(".{}.staging-{}", bundle.id, journal.nonce)
        || journal.backup_name != format!(".{}.backup-{}", bundle.id, journal.nonce)
    {
        return Err(ModelManagerError::Corrupt("install journal is not manager-owned".into()));
    }
    Ok(())
}

fn transaction_residues(root: &Path, id: &str) -> Result<Vec<String>, ModelManagerError> {
    let staging_prefix = format!(".{id}.staging-");
    let backup_prefix = format!(".{id}.backup-");
    let mut result = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().into_string().map_err(|_| ModelManagerError::UnsafePath)?;
        if name.starts_with(&staging_prefix) || name.starts_with(&backup_prefix) {
            result.push(name);
        }
    }
    result.sort_unstable();
    Ok(result)
}

fn required_file_if_present(path: &Path) -> Result<Option<File>, ModelManagerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ModelManagerError::UnsafePath)
        }
        Ok(_) => open_regular_no_follow(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ModelManagerError::Io(error)),
    }
}

fn required_regular_file(path: &Path, missing: &str) -> Result<File, ModelManagerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ModelManagerError::UnsafePath),
        Ok(metadata) if !metadata.is_file() => {
            Err(ModelManagerError::Corrupt(format!("{} is not a regular file", path.display())))
        }
        Ok(_) => open_regular_no_follow(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ModelManagerError::Corrupt(missing.into()))
        }
        Err(error) => Err(ModelManagerError::Io(error)),
    }
}

fn open_regular_no_follow(path: &Path) -> Result<File, ModelManagerError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return ModelManagerError::UnsafePath;
        }
        ModelManagerError::Io(error)
    })?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ModelManagerError::UnsafePath);
        }
    }
    if !metadata.is_file() {
        return Err(ModelManagerError::UnsafePath);
    }
    Ok(file)
}

struct StagingDirectory {
    path: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl StagingDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: std::cell::Cell::new(true) }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if self.armed.get() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn safe_existing_directory(path: &Path) -> Result<bool, ModelManagerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ModelManagerError::UnsafePath),
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(ModelManagerError::UnsafePath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ModelManagerError::Io(error)),
    }
}

fn reject_symlink(path: &Path) -> Result<(), ModelManagerError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() { Err(ModelManagerError::UnsafePath) } else { Ok(()) }
}

fn reject_symlink_if_present(path: &Path) -> Result<(), ModelManagerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ModelManagerError::UnsafePath),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ModelManagerError::Io(error)),
    }
}

fn sync_directory(path: &Path) -> Result<(), ModelManagerError> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    Ok(())
}

/// Non-inferencing OCR placeholder.
#[derive(Debug, Default)]
pub struct PlaceholderOcrEngine;

impl OcrEngine for PlaceholderOcrEngine {
    fn id(&self) -> &'static str {
        "builtin.ocr.placeholder"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            context.report(ExecutionStage::Ocr, None, None, Some("builtin.ocr.placeholder"))?;
            Err(ConversionError::Ocr {
                provider: "builtin.ocr.placeholder".into(),
                detail: "PP-OCRv6 inference is not implemented".into(),
            })
        })
    }
}

/// Non-inferencing tensor-runtime placeholder.
#[derive(Debug, Default)]
pub struct PlaceholderTensorRuntime;

impl TensorRuntime for PlaceholderTensorRuntime {
    fn id(&self) -> &'static str {
        "builtin.tensor-runtime.placeholder"
    }

    fn run<'a>(
        &'a self,
        _: &'a str,
        _: &'a [Tensor],
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<Tensor>, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            context.report(
                ExecutionStage::Ocr,
                None,
                None,
                Some("builtin.tensor-runtime.placeholder"),
            )?;
            Err(ConversionError::Ocr {
                provider: "builtin.tensor-runtime.placeholder".into(),
                detail: "ONNX Runtime integration is not implemented".into(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_is_fail_closed_and_source_archives_are_not_installable() {
        let manifest = ModelManifest::embedded().unwrap();
        let bundle = &manifest.bundles[0];
        assert_eq!(bundle.availability, "planned");
        assert!(bundle.runtime_artifacts.is_empty());
        assert_eq!(bundle.platforms.len(), 4);
    }

    #[test]
    fn unknown_schema_fields_and_traversal_fail_closed() {
        let models = include_str!("../../../models/manifest.json");
        let downloads = include_str!("../../../third_party/licenses/downloads.json");
        let unknown = models.replacen(
            "\"schema_version\": 1",
            "\"schema_version\": 1, \"surprise\": true",
            1,
        );
        assert!(ModelManifest::from_authorities(&unknown, downloads).is_err());
        let traversal = models.replace(
            "\"runtime_artifacts\": []",
            "\"runtime_artifacts\": [{\"id\":\"evil\",\"role\":\"detector\",\"file_name\":\"../evil\",\"url\":\"https://example.invalid/evil\",\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"size\":1,\"platforms\":[\"aarch64-apple-darwin\"],\"license\":\"MIT\"}]",
        );
        assert!(ModelManifest::from_authorities(&traversal, downloads).is_err());
    }

    #[test]
    fn four_product_targets_have_deterministic_data_directories() {
        let env = DataDirectoryEnvironment {
            xdg_data_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/alice")),
            local_app_data: Some(PathBuf::from("/windows/local")),
        };
        assert_eq!(
            model_data_directory(ProductTarget::MacOsArm64, &env).unwrap(),
            PathBuf::from("/home/alice/Library/Application Support/into-markdown/models")
        );
        assert_eq!(
            model_data_directory(ProductTarget::LinuxX86_64, &env).unwrap(),
            PathBuf::from("/xdg/into-markdown/models")
        );
        assert_eq!(
            model_data_directory(ProductTarget::LinuxArm64, &env).unwrap(),
            PathBuf::from("/xdg/into-markdown/models")
        );
        assert_eq!(
            model_data_directory(ProductTarget::WindowsX86_64, &env).unwrap(),
            PathBuf::from("/windows/local/into-markdown/models")
        );
    }

    #[test]
    fn manager_reports_unavailable_without_network_or_filesystem_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let manager =
            ModelManager::new(ModelManifest::embedded().unwrap(), temp.path().join("models"), None);
        let status = manager.status("pp-ocrv6-tiny-zh-en").unwrap();
        assert_eq!(status.state, "unavailable");
        assert!(!temp.path().join("models").exists());
        assert!(matches!(
            manager.require_installable(&status.id),
            Err(ModelManagerError::ComponentUnavailable)
        ));
    }

    #[test]
    fn planned_bundle_ignores_fake_bundled_and_user_install_state() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("bundled");
        let writable = temp.path().join("writable");
        let id = "pp-ocrv6-tiny-zh-en";
        for root in [&bundled, &writable] {
            fs::create_dir_all(root.join(id)).unwrap();
            fs::write(
                root.join(id).join("install-state.json"),
                br#"{"schemaVersion":1,"bundleId":"pp-ocrv6-tiny-zh-en","complete":true}"#,
            )
            .unwrap();
        }
        let manager = ModelManager::new(
            ModelManifest::embedded().unwrap(),
            writable.clone(),
            Some(bundled.clone()),
        );
        let status = manager.status(id).unwrap();
        assert_eq!(status.state, "unavailable");
        assert_eq!(status.ownership, "none");
        assert!(status.path.is_none());
        assert_eq!(manager.list().unwrap(), vec![status]);
        assert!(matches!(manager.verify(id), Err(ModelManagerError::ComponentUnavailable)));
        assert!(matches!(manager.path(id), Err(ModelManagerError::ComponentUnavailable)));
        assert!(matches!(manager.remove(id), Err(ModelManagerError::ComponentUnavailable)));
        let fetcher =
            BytesFetcher { bytes: Vec::new(), opens: std::sync::atomic::AtomicUsize::new(0) };
        assert!(matches!(
            manager.install(id, &fetcher, &execution(0)),
            Err(ModelManagerError::ComponentUnavailable)
        ));
        assert!(bundled.join(id).exists());
        assert!(writable.join(id).exists());
        assert_eq!(fetcher.opens.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    struct BytesFetcher {
        bytes: Vec<u8>,
        opens: std::sync::atomic::AtomicUsize,
    }

    impl ModelFetcher for BytesFetcher {
        fn open(
            &self,
            _: &RuntimeArtifact,
            context: &ExecutionContext,
        ) -> Result<Box<dyn Read>, ModelManagerError> {
            context.checkpoint()?;
            self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
        }
    }

    fn installable_manager(root: &Path) -> ModelManager {
        let mut manifest = ModelManifest::embedded().unwrap();
        let bundle = &mut manifest.bundles[0];
        bundle.availability = "available".into();
        bundle.character_set.status = "available".into();
        bundle.runtime_artifacts.push(RuntimeArtifact {
            id: "test-runtime".into(),
            role: "detector".into(),
            file_name: "model.onnx".into(),
            url: "https://example.invalid/model.onnx".into(),
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
            size: 5,
            platforms: vec!["aarch64-apple-darwin".into()],
            license: "MIT".into(),
        });
        ModelManager::new(manifest, root.to_path_buf(), None)
    }

    fn execution(max_temporary_bytes: u64) -> ExecutionContext {
        ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits { max_temporary_bytes, ..Default::default() },
        )
    }

    #[test]
    fn install_is_hash_verified_atomic_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let manager = installable_manager(temp.path());
        let fetcher = BytesFetcher {
            bytes: b"hello".to_vec(),
            opens: std::sync::atomic::AtomicUsize::new(0),
        };
        let id = "pp-ocrv6-tiny-zh-en";
        let status = manager.install(id, &fetcher, &execution(5)).unwrap();
        assert_eq!(status.state, "installed");
        assert_eq!(fs::read(manager.path(id).unwrap().join("model.onnx")).unwrap(), b"hello");
        manager.install(id, &fetcher, &execution(5)).unwrap();
        assert_eq!(fetcher.opens.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn install_journal_recovers_after_backup_and_after_publish() {
        for fault in [InstallFault::AfterBackup, InstallFault::AfterPublish] {
            let temp = tempfile::tempdir().unwrap();
            let manager = installable_manager(temp.path());
            let fetcher = BytesFetcher {
                bytes: b"hello".to_vec(),
                opens: std::sync::atomic::AtomicUsize::new(0),
            };
            let id = "pp-ocrv6-tiny-zh-en";
            manager.install(id, &fetcher, &execution(5)).unwrap();
            assert!(matches!(
                manager.install_inner(id, &fetcher, &execution(5), fault, true),
                Err(ModelManagerError::Corrupt(_))
            ));

            let restarted = installable_manager(temp.path());
            assert_eq!(restarted.status(id).unwrap().state, "installed");
            assert_eq!(fs::read(restarted.path(id).unwrap().join("model.onnx")).unwrap(), b"hello");
            assert!(!journal_path(temp.path(), id).exists());
            assert!(transaction_residues(temp.path(), id).unwrap().is_empty());
        }
    }

    #[test]
    fn corrupt_or_ambiguous_journal_fails_closed_without_cleanup() {
        let id = "pp-ocrv6-tiny-zh-en";
        let temp = tempfile::tempdir().unwrap();
        let manager = installable_manager(temp.path());
        let fetcher = BytesFetcher {
            bytes: b"hello".to_vec(),
            opens: std::sync::atomic::AtomicUsize::new(0),
        };
        manager.install(id, &fetcher, &execution(5)).unwrap();
        fs::write(journal_path(temp.path(), id), b"{").unwrap();
        assert!(matches!(manager.status(id), Err(ModelManagerError::Corrupt(_))));
        assert!(temp.path().join(id).exists());
        assert!(journal_path(temp.path(), id).exists());

        let ambiguous = tempfile::tempdir().unwrap();
        let manager = installable_manager(ambiguous.path());
        manager.install(id, &fetcher, &execution(5)).unwrap();
        assert!(
            manager
                .install_inner(id, &fetcher, &execution(5), InstallFault::AfterBackup, true)
                .is_err()
        );
        let extra = ambiguous.path().join(format!(".{id}.staging-999-999"));
        fs::create_dir(&extra).unwrap();
        let before = transaction_residues(ambiguous.path(), id).unwrap();
        assert!(matches!(manager.status(id), Err(ModelManagerError::Corrupt(_))));
        assert_eq!(transaction_residues(ambiguous.path(), id).unwrap(), before);
        assert!(extra.exists());
        assert!(journal_path(ambiguous.path(), id).exists());
    }

    #[test]
    fn journal_path_binding_rejects_traversal_without_touching_outside() {
        let id = "pp-ocrv6-tiny-zh-en";
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("models");
        fs::create_dir(&root).unwrap();
        let manager = installable_manager(&root);
        let outside = temp.path().join("must-not-touch");
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            journal_path(&root, id),
            format!(
                r#"{{"schemaVersion":1,"bundleId":"{id}","nonce":"1-1","stagingName":"../must-not-touch","backupName":".{id}.backup-1-1"}}"#
            ),
        )
        .unwrap();
        assert!(matches!(manager.status(id), Err(ModelManagerError::Corrupt(_))));
        assert!(outside.exists());
        assert!(journal_path(&root, id).exists());
    }

    #[test]
    fn missing_or_malformed_local_state_is_corrupt() {
        for case in 0..5 {
            let temp = tempfile::tempdir().unwrap();
            let manager = installable_manager(temp.path());
            let fetcher = BytesFetcher {
                bytes: b"hello".to_vec(),
                opens: std::sync::atomic::AtomicUsize::new(0),
            };
            let id = "pp-ocrv6-tiny-zh-en";
            manager.install(id, &fetcher, &execution(5)).unwrap();
            let bundle = temp.path().join(id);
            match case {
                0 => fs::remove_file(bundle.join("install-state.json")).unwrap(),
                1 => fs::write(bundle.join("install-state.json"), b"{").unwrap(),
                2 => fs::remove_file(bundle.join("model.onnx")).unwrap(),
                3 => fs::write(bundle.join("model.onnx"), b"hell").unwrap(),
                4 => fs::write(bundle.join("model.onnx"), b"jello").unwrap(),
                _ => unreachable!(),
            }
            assert!(matches!(manager.verify(id), Err(ModelManagerError::Corrupt(_))));
            if case == 0 {
                assert!(matches!(manager.remove(id), Err(ModelManagerError::Corrupt(_))));
                assert!(bundle.exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn permission_failure_remains_io_error() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let manager = installable_manager(temp.path());
        let fetcher = BytesFetcher {
            bytes: b"hello".to_vec(),
            opens: std::sync::atomic::AtomicUsize::new(0),
        };
        let id = "pp-ocrv6-tiny-zh-en";
        manager.install(id, &fetcher, &execution(5)).unwrap();
        let state = temp.path().join(id).join("install-state.json");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o0)).unwrap();
        assert!(matches!(manager.verify(id), Err(ModelManagerError::Io(_))));
        fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn hash_mismatch_truncation_and_temporary_budget_leave_no_partial_state() {
        for (bytes, budget) in [(b"wrong".as_slice(), 5), (b"hell".as_slice(), 5), (b"hello", 4)] {
            let temp = tempfile::tempdir().unwrap();
            let manager = installable_manager(temp.path());
            let fetcher = BytesFetcher {
                bytes: bytes.to_vec(),
                opens: std::sync::atomic::AtomicUsize::new(0),
            };
            assert!(manager.install("pp-ocrv6-tiny-zh-en", &fetcher, &execution(budget)).is_err());
            assert!(!temp.path().join("pp-ocrv6-tiny-zh-en").exists());
            assert!(
                fs::read_dir(temp.path())
                    .unwrap()
                    .flatten()
                    .all(|entry| !entry.file_name().to_string_lossy().contains(".staging-"))
            );
        }
    }

    #[test]
    fn cancellation_and_concurrent_lock_fail_without_fetching() {
        let temp = tempfile::tempdir().unwrap();
        let manager = installable_manager(temp.path());
        fs::create_dir_all(temp.path()).unwrap();
        let held = manager.acquire_lock().unwrap();
        let fetcher = BytesFetcher {
            bytes: b"hello".to_vec(),
            opens: std::sync::atomic::AtomicUsize::new(0),
        };
        assert!(matches!(
            manager.install("pp-ocrv6-tiny-zh-en", &fetcher, &execution(5)),
            Err(ModelManagerError::Busy)
        ));
        drop(held);
        let token = into_markdown_core::CancellationToken::new();
        token.cancel();
        let cancelled = ExecutionContext::new(
            into_markdown_core::ExecutionOptions { cancellation: token, ..Default::default() },
            into_markdown_core::ResourceLimits::default(),
        );
        assert!(matches!(
            manager.install("pp-ocrv6-tiny-zh-en", &fetcher, &cancelled),
            Err(ModelManagerError::Execution(ConversionError::Cancelled))
        ));
        assert_eq!(fetcher.opens.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_bundle_is_rejected_and_never_removed() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let root = temp.path().join("models");
        fs::create_dir(&root).unwrap();
        symlink(&outside, root.join("pp-ocrv6-tiny-zh-en")).unwrap();
        let manager = installable_manager(&root);
        assert!(matches!(
            manager.status("pp-ocrv6-tiny-zh-en"),
            Err(ModelManagerError::UnsafePath)
        ));
        assert!(matches!(
            manager.remove("pp-ocrv6-tiny-zh-en"),
            Err(ModelManagerError::UnsafePath)
        ));
        assert!(outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_install_journal_is_rejected_and_target_is_untouched() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("models");
        fs::create_dir(&root).unwrap();
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"outside").unwrap();
        let id = "pp-ocrv6-tiny-zh-en";
        symlink(&outside, journal_path(&root, id)).unwrap();
        let manager = installable_manager(&root);
        assert!(matches!(manager.status(id), Err(ModelManagerError::UnsafePath)));
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }
}
