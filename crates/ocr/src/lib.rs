//! Local OCR model metadata and management.
//!
//! The embedded manifests are the only authority used by this crate. Source
//! archives are never treated as installable runtime models.

use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, ExecutionStage, OcrEngine, OcrRequest, OcrResult,
    Tensor, TensorRuntime,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

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
            if targets != BTreeSet::from(SUPPORTED_TARGETS) {
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
    if platforms.is_empty() || !platforms.iter().all(|target| SUPPORTED_TARGETS.contains(target)) {
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
    #[error("model I/O failed: {0}")]
    Io(#[from] std::io::Error),
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
    /// Creates a manager without touching the filesystem.
    pub fn new(
        manifest: ModelManifest,
        writable_root: PathBuf,
        bundled_root: Option<PathBuf>,
    ) -> Self {
        Self { manifest, writable_root, bundled_root }
    }

    /// Embedded manifest accessor.
    pub fn manifest(&self) -> &ModelManifest {
        &self.manifest
    }

    /// Returns one offline state snapshot.
    pub fn status(&self, id: &str) -> Result<ModelStatus, ModelManagerError> {
        let bundle = self.bundle(id)?;
        if let Some(root) = &self.bundled_root {
            let path = root.join(id);
            if safe_existing_directory(&path)? {
                return Ok(ModelStatus {
                    schema_version: 1,
                    id: id.to_owned(),
                    availability: bundle.availability.clone(),
                    state: "installed".into(),
                    ownership: "bundled-read-only".into(),
                    path: Some(path),
                });
            }
        }
        let path = self.writable_root.join(id);
        if safe_existing_directory(&path)? {
            let verified = verify_directory(bundle, &path).is_ok();
            return Ok(ModelStatus {
                schema_version: 1,
                id: id.to_owned(),
                availability: bundle.availability.clone(),
                state: if verified { "installed" } else { "corrupt" }.into(),
                ownership: "user".into(),
                path: Some(path),
            });
        }
        Ok(ModelStatus {
            schema_version: 1,
            id: id.to_owned(),
            availability: bundle.availability.clone(),
            state: if bundle.availability == "available" { "not-installed" } else { "unavailable" }
                .into(),
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
        let status = self.status(id)?;
        let path = status.path.as_ref().ok_or(ModelManagerError::NotInstalled)?;
        verify_directory(self.bundle(id)?, path)?;
        Ok(status)
    }

    /// Returns the path only for a complete installed bundle.
    pub fn path(&self, id: &str) -> Result<PathBuf, ModelManagerError> {
        self.verify(id)?.path.ok_or(ModelManagerError::NotInstalled)
    }

    /// Removes only a user-owned directory while holding an interprocess lock.
    pub fn remove(&self, id: &str) -> Result<(), ModelManagerError> {
        let _ = self.bundle(id)?;
        if let Some(root) = &self.bundled_root {
            if safe_existing_directory(&root.join(id))? {
                return Err(ModelManagerError::ReadOnly);
            }
        }
        fs::create_dir_all(&self.writable_root)?;
        reject_symlink(&self.writable_root)?;
        let lock = self.acquire_lock()?;
        let path = self.writable_root.join(id);
        if !safe_existing_directory(&path)? {
            return Err(ModelManagerError::NotInstalled);
        }
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
        if bundle.availability != "available" || bundle.runtime_artifacts.is_empty() {
            return Err(ModelManagerError::ComponentUnavailable);
        }
        Ok(bundle)
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
        let file = OpenOptions::new().create(true).read(true).write(true).open(path)?;
        file.try_lock().map_err(|_| ModelManagerError::Busy)?;
        Ok(file)
    }
}

fn verify_directory(bundle: &ModelBundle, path: &Path) -> Result<(), ModelManagerError> {
    reject_symlink(path)?;
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
        let file_path = path.join(&artifact.file_name);
        reject_symlink(&file_path)?;
        let metadata = fs::metadata(&file_path)?;
        if !metadata.is_file() || metadata.len() != artifact.size {
            return Err(ModelManagerError::Corrupt(artifact.file_name.clone()));
        }
        let mut file = File::open(&file_path)?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
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
        let manager = ModelManager::new(ModelManifest::embedded().unwrap(), root, None);
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
}
