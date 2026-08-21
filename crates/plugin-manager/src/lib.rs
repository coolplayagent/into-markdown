// SPDX-License-Identifier: Apache-2.0
//! Signed, transactional installation authority for process-v1 and WASI plugins.

#![forbid(unsafe_code)]
// The public manager API documents stable error categories centrally on
// `ManagerErrorCode`; repeating the same exhaustive list on every facade would
// obscure the capability and transaction contracts in their method docs.
#![allow(clippy::missing_errors_doc)]
// Transaction/recovery functions intentionally keep their ordered durability
// state machines together so review can verify every linearization point.
#![allow(clippy::too_many_lines)]

use base64::Engine as _;
use into_markdown_core::{ExecutionContext, ResourceReservation};
use into_markdown_plugin_wasi::{WasiCapabilities, WasiPluginManifest};
use ring::signature::{self, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use zip::ZipArchive;

const MANIFEST_NAME: &str = "plugin.json";
const INSTALLED_NAME: &str = ".installed.json";
const ARCHIVE_NAME: &str = ".package.zip";
const TRANSACTION_NAME: &str = ".transaction.json";
const LOCK_NAME: &str = ".manager.lock";
const TRUST_NAME: &str = ".trusted-signers.json";
const MAX_PACKAGE_BYTES: usize = 256 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FILES: usize = 4096;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_ZIP_CENTRAL_DIRECTORY_BYTES: u64 = 8 * 1024 * 1024;
static NONCE: AtomicU64 = AtomicU64::new(1);

/// Stable plugin-manager failure categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManagerErrorCode {
    /// Package structure or metadata is invalid.
    InvalidPackage,
    /// A declared digest does not match bytes.
    HashMismatch,
    /// Publisher signature or trust authority failed.
    Signature,
    /// Protocol version is unsupported.
    UnsupportedProtocol,
    /// No exact current-platform entrypoint exists.
    UnsupportedTarget,
    /// A path, link, alias, or file identity escaped policy.
    PathTraversal,
    /// Another manager operation owns the store lock.
    Conflict,
    /// Cooperative cancellation was observed.
    Cancelled,
    /// The execution deadline elapsed.
    Timeout,
    /// A configured resource budget was exceeded.
    ResourceLimit,
    /// A durable transaction intent exists but publication is not yet observable.
    Indeterminate,
    /// The requested plugin is absent.
    NotInstalled,
    /// A sanitized operating-system failure occurred.
    Io,
}

/// Sanitized package-management failure.
#[derive(Debug, Error)]
#[error("{code:?}: {detail}")]
pub struct ManagerError {
    /// Stable machine-readable category.
    pub code: ManagerErrorCode,
    /// Bounded, sanitized diagnostic detail.
    pub detail: String,
}

impl ManagerError {
    fn new(code: ManagerErrorCode, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        detail.truncate(512);
        Self { code, detail }
    }
}

impl From<std::io::Error> for ManagerError {
    fn from(error: std::io::Error) -> Self {
        Self::new(
            ManagerErrorCode::Io,
            format!("plugin store operation failed (os={:?})", error.raw_os_error()),
        )
    }
}

#[cfg(unix)]
impl From<rustix::io::Errno> for ManagerError {
    fn from(error: rustix::io::Errno) -> Self {
        std::io::Error::from_raw_os_error(error.raw_os_error()).into()
    }
}

/// One hash-bound regular file in the package.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageFile {
    /// Portable canonical relative path.
    pub path: String,
    /// Exact uncompressed byte length.
    pub bytes: u64,
    /// SHA-256 of exact file bytes.
    pub sha256: String,
    /// Whether Unix installations grant owner execute permission to this file.
    #[serde(default)]
    pub executable: bool,
}

/// Ed25519 authority over the canonical manifest without this field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageSignature {
    /// Canonical signed-payload schema version.
    pub signed_payload_version: u32,
    /// Signature algorithm; exactly `ed25519`.
    pub algorithm: String,
    /// Trust-store key identifier bound into the signature.
    pub key_id: String,
    /// Base64-encoded 32-byte public key.
    pub public_key_base64: String,
    /// SHA-256 fingerprint of the public key.
    pub public_key_sha256: String,
    /// SHA-256 of the canonical signed payload.
    pub signed_payload_sha256: String,
    /// Base64-encoded Ed25519 signature.
    pub signature_base64: String,
}

/// Signed plugin package manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageManifest {
    /// Package manifest schema version.
    pub schema_version: u32,
    /// Stable plugin identifier.
    pub id: String,
    /// Plugin release version.
    pub version: String,
    /// Runtime protocol identifier.
    pub protocol: String,
    /// Exact supported Rust target triples.
    pub supported_targets: BTreeSet<String>,
    /// Target triple to package entrypoint mapping.
    pub entrypoints: BTreeMap<String, String>,
    /// Hash-bound WASI runtime-manifest path when applicable.
    pub runtime_manifest: Option<String>,
    /// Sorted, complete package file inventory.
    pub files: Vec<PackageFile>,
    /// Publisher signature and trust identity.
    pub signature: PackageSignature,
}

#[derive(Serialize)]
#[allow(clippy::struct_field_names)]
#[serde(rename_all = "camelCase")]
struct SignedPayload<'a> {
    signature_domain: &'static str,
    signed_payload_version: u32,
    algorithm: &'a str,
    key_id: &'a str,
    public_key_sha256: &'a str,
    schema_version: u32,
    id: &'a str,
    version: &'a str,
    protocol: &'a str,
    supported_targets: &'a BTreeSet<String>,
    entrypoints: &'a BTreeMap<String, String>,
    runtime_manifest: &'a Option<String>,
    files: &'a [PackageFile],
}

/// Serialize the exact domain-separated bytes covered by a package signature.
pub fn canonical_signed_payload(manifest: &PackageManifest) -> Result<Vec<u8>, ManagerError> {
    serde_json::to_vec(&SignedPayload {
        signature_domain: "into-markdown/plugin-package/v1",
        signed_payload_version: manifest.signature.signed_payload_version,
        algorithm: &manifest.signature.algorithm,
        key_id: &manifest.signature.key_id,
        public_key_sha256: &manifest.signature.public_key_sha256,
        schema_version: manifest.schema_version,
        id: &manifest.id,
        version: &manifest.version,
        protocol: &manifest.protocol,
        supported_targets: &manifest.supported_targets,
        entrypoints: &manifest.entrypoints,
        runtime_manifest: &manifest.runtime_manifest,
        files: &manifest.files,
    })
    .map_err(|_| ManagerError::new(ManagerErrorCode::Signature, "signed payload unavailable"))
}

/// Validate one path with the same portable rules used during installation.
pub fn validate_package_file_path(path: &str) -> Result<(), ManagerError> {
    validate_package_path(path)
}

/// Explicit publisher trust and revocation authority for one scope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedSigners {
    /// Signing key id to SHA-256 fingerprint of its exact 32-byte Ed25519 public key.
    pub fingerprints: BTreeMap<String, String>,
    /// Key ids rejected even when their fingerprint remains configured.
    pub revoked: BTreeSet<String>,
    /// Public-key fingerprints revoked independently of aliases.
    pub revoked_fingerprints: BTreeSet<String>,
}

/// Verified installed package details suitable for CLI/HTTP DTOs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPlugin {
    /// Stable plugin identifier.
    pub id: String,
    /// Installed release version.
    pub version: String,
    /// Installed runtime protocol.
    pub protocol: String,
    /// SHA-256 of the source ZIP bytes verified during transfer.
    pub package_sha256: String,
    /// Signature-bound canonical content identity independent of ZIP encoding.
    pub content_root_sha256: String,
    /// Trusted signing key identifier.
    pub signing_key_id: String,
    /// Canonical installed directory.
    pub root: PathBuf,
}

/// Signed package identity inspected without changing the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedPackage {
    /// Manifest plugin identifier.
    pub id: String,
    /// Manifest protocol.
    pub protocol: String,
    /// Exact source archive SHA-256.
    pub package_sha256: String,
    /// Canonical signed-payload SHA-256.
    pub content_root_sha256: String,
    /// Signed publisher key identifier.
    pub signing_key_id: String,
}

/// A private retained-package snapshot held under the caller's temporary budget.
pub struct PackageSnapshot {
    path: PathBuf,
    installed: InstalledPlugin,
    _temporary: ResourceReservation,
    cleanup: TemporaryFile,
}

impl PackageSnapshot {
    /// Exact private snapshot path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Installed authority captured with the snapshot.
    #[must_use]
    pub fn installed(&self) -> &InstalledPlugin {
        &self.installed
    }

    /// Transfer the snapshot to an already-durable transaction journal.
    #[must_use]
    pub fn persist(mut self) -> InstalledPlugin {
        self.cleanup.keep();
        self.installed
    }
}

/// Private immutable snapshot kept alive across process-v1 verification and dispatch.
pub struct PreparedProcessPlugin {
    authority: into_markdown_process_plugin::PluginManifest,
    policy: into_markdown_process_plugin::RuntimePolicy,
    _snapshot_directory: tempfile::TempDir,
    _snapshot_reservation: ResourceReservation,
    _metadata_reservation: ResourceReservation,
}

/// Verified WASI component bytes and their request-scoped capability intersection.
pub struct PreparedWasiPlugin {
    runtime: into_markdown_plugin_wasi::WasiPluginRuntime,
    manifest: WasiPluginManifest,
    component: Vec<u8>,
    _component_reservation: ResourceReservation,
    _metadata_reservation: ResourceReservation,
    _store_lock: StoreLock,
}

impl PreparedWasiPlugin {
    /// Execute only through the sandboxed WASI runtime with the sealed manifest.
    pub fn execute(
        &self,
        request: &into_markdown_plugin_wasi::PluginRequest,
        execution: &ExecutionContext,
    ) -> Result<
        into_markdown_plugin_wasi::PluginRunOutput,
        into_markdown_plugin_wasi::WasiPluginError,
    > {
        self.runtime.run(&self.component, &self.manifest, request, execution)
    }
}

impl PreparedProcessPlugin {
    /// Consume the pinned snapshot only through the sandboxed process-v1 runtime.
    pub fn execute(
        &self,
        request: into_markdown_process_plugin::PluginRequest<'_>,
        execution: &ExecutionContext,
    ) -> Result<
        into_markdown_process_plugin::PluginExecution,
        into_markdown_process_plugin::PluginError,
    > {
        into_markdown_process_plugin::ProcessPlugin::new(
            self.authority.clone(),
            self.policy.clone(),
        )?
        .execute(request, execution)
    }

    /// Execute a structured process-v1 request while retaining the authenticated JSON result.
    ///
    /// Capability adapters use this form for typed OCR, transcription, and diarization payloads;
    /// the same pinned snapshot and sandbox policy apply as for [`Self::execute`].
    pub fn execute_raw(
        &self,
        request: into_markdown_process_plugin::PluginRequest<'_>,
        execution: &ExecutionContext,
    ) -> Result<
        into_markdown_process_plugin::RawPluginExecution,
        into_markdown_process_plugin::PluginError,
    > {
        into_markdown_process_plugin::ProcessPlugin::new(
            self.authority.clone(),
            self.policy.clone(),
        )?
        .execute_raw(request, execution)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstalledAuthority {
    source_archive_sha256: String,
    source_archive_bytes: u64,
    manifest_sha256: String,
    signing_key_id: String,
    signing_key_fingerprint: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum TransactionPhase {
    Staging,
    Ready,
    BackupMoved,
    Published,
    Removing,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Transaction {
    phase: TransactionPhase,
    id: String,
    staging: String,
    destination: String,
    backup: String,
}

/// A scope-specific plugin store. Construct one instance for project or global scope.
#[derive(Debug, Clone)]
pub struct PluginManager {
    root: PathBuf,
    root_identity: StoreIdentity,
    trusted_signers: TrustedSigners,
}

impl PluginManager {
    fn acquire_lock(&self) -> Result<StoreLock, ManagerError> {
        verify_store_identity(&self.root, self.root_identity)?;
        let lock = StoreLock::acquire(&self.root)?;
        verify_store_identity(&self.root, self.root_identity)?;
        Ok(lock)
    }

    /// Open a store beneath an explicit trusted scope anchor without following links.
    pub fn open_scoped(
        anchor: &Path,
        relative: &Path,
        trusted_signers: TrustedSigners,
    ) -> Result<Self, ManagerError> {
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative.components().any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "store relative path rejected",
            ));
        }
        reject_link(anchor)?;
        secure_scope_anchor(anchor)?;
        let requested_anchor_identity = store_identity(anchor)?;
        let anchor = fs::canonicalize(anchor)?;
        reject_link(&anchor)?;
        secure_scope_anchor(&anchor)?;
        verify_store_identity(&anchor, requested_anchor_identity)?;
        let anchor_identity = store_identity(&anchor)?;
        #[cfg(unix)]
        create_unix_store_path(&anchor, relative)?;
        #[cfg(windows)]
        create_windows_store_path_scoped(&anchor, relative)?;
        let root = anchor.join(relative);
        verify_store_identity(&anchor, anchor_identity)?;
        secure_store_root(&root)?;
        let root = fs::canonicalize(root)?;
        if !root.starts_with(&anchor) {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "store escaped scope anchor",
            ));
        }
        validate_trust_authority(&trusted_signers)?;
        let root_identity = store_identity(&root)?;
        let manager = Self { root, root_identity, trusted_signers };
        manager.recover()?;
        Ok(manager)
    }

    /// Open an already-existing scoped store without creating any path.
    pub fn open_existing_scoped(
        anchor: &Path,
        relative: &Path,
        trusted_signers: TrustedSigners,
    ) -> Result<Option<Self>, ManagerError> {
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative.components().any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "store relative path rejected",
            ));
        }
        reject_link(anchor)?;
        secure_scope_anchor(anchor)?;
        let requested_anchor_identity = store_identity(anchor)?;
        let anchor = fs::canonicalize(anchor)?;
        reject_link(&anchor)?;
        secure_scope_anchor(&anchor)?;
        verify_store_identity(&anchor, requested_anchor_identity)?;
        let anchor_identity = store_identity(&anchor)?;
        let candidate = anchor.join(relative);
        if !candidate.exists() {
            return Ok(None);
        }
        let root = fs::canonicalize(&candidate)?;
        verify_store_identity(&anchor, anchor_identity)?;
        if !root.starts_with(&anchor) || root != candidate {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "existing store escaped scope anchor",
            ));
        }
        secure_store_root(&root)?;
        validate_trust_authority(&trusted_signers)?;
        let root_identity = store_identity(&root)?;
        let manager = Self { root, root_identity, trusted_signers };
        manager.recover()?;
        Ok(Some(manager))
    }

    /// Open the protected global trust authority beneath an explicit scope anchor.
    pub fn open_persisted_scoped(anchor: &Path, relative: &Path) -> Result<Self, ManagerError> {
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative.components().any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "store relative path rejected",
            ));
        }
        reject_link(anchor)?;
        secure_scope_anchor(anchor)?;
        let requested_anchor_identity = store_identity(anchor)?;
        let anchor = fs::canonicalize(anchor)?;
        reject_link(&anchor)?;
        secure_scope_anchor(&anchor)?;
        verify_store_identity(&anchor, requested_anchor_identity)?;
        let anchor_identity = store_identity(&anchor)?;
        #[cfg(unix)]
        create_unix_store_path(&anchor, relative)?;
        #[cfg(windows)]
        create_windows_store_path_scoped(&anchor, relative)?;
        let root = fs::canonicalize(anchor.join(relative))?;
        verify_store_identity(&anchor, anchor_identity)?;
        if !root.starts_with(&anchor) {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "store escaped scope anchor",
            ));
        }
        secure_store_root(&root)?;
        let trust_path = root.join(TRUST_NAME);
        {
            let _lock = StoreLock::acquire(&root)?;
            recover_atomic_json(&trust_path)?;
        }
        let trusted_signers = load_trust_authority(&trust_path)?;
        validate_trust_authority(&trusted_signers)?;
        let root_identity = store_identity(&root)?;
        let manager = Self { root, root_identity, trusted_signers };
        manager.recover()?;
        Ok(manager)
    }

    /// Open a store and recover an interrupted atomic rename transaction.
    #[cfg(test)]
    pub fn open(
        root: impl Into<PathBuf>,
        trusted_signers: TrustedSigners,
    ) -> Result<Self, ManagerError> {
        let root = root.into();
        #[cfg(not(windows))]
        fs::create_dir_all(&root)?;
        #[cfg(windows)]
        create_windows_store_path(&root)?;
        secure_store_root(&root)?;
        let root = fs::canonicalize(root)?;
        validate_trust_authority(&trusted_signers)?;
        let root_identity = store_identity(&root)?;
        let manager = Self { root, root_identity, trusted_signers };
        manager.recover()?;
        Ok(manager)
    }

    /// Open the global store using only its protected, persisted trust authority.
    #[cfg(test)]
    pub fn open_persisted(root: impl Into<PathBuf>) -> Result<Self, ManagerError> {
        let root = root.into();
        #[cfg(not(windows))]
        fs::create_dir_all(&root)?;
        #[cfg(windows)]
        create_windows_store_path(&root)?;
        secure_store_root(&root)?;
        let root = fs::canonicalize(root)?;
        let trust_path = root.join(TRUST_NAME);
        {
            let _lock = StoreLock::acquire(&root)?;
            recover_atomic_json(&trust_path)?;
        }
        let trusted_signers = load_trust_authority(&trust_path)?;
        validate_trust_authority(&trusted_signers)?;
        let root_identity = store_identity(&root)?;
        let manager = Self { root, root_identity, trusted_signers };
        manager.recover()?;
        Ok(manager)
    }

    /// Persist one explicit global publisher trust anchor without permitting aliases.
    pub fn trust_signer(&mut self, id: &str, fingerprint: &str) -> Result<(), ManagerError> {
        if !valid_key_id(id) {
            return Err(ManagerError::new(ManagerErrorCode::Signature, "signing key id rejected"));
        }
        validate_hash(fingerprint)?;
        let _lock = self.acquire_lock()?;
        recover_atomic_json(&self.root.join(TRUST_NAME))?;
        let mut current = load_trust_authority(&self.root.join(TRUST_NAME))?;
        if current.revoked.contains(id)
            || current.revoked_fingerprints.contains(fingerprint)
            || current
                .fingerprints
                .iter()
                .any(|(known_id, known)| known_id != id && known == fingerprint)
            || current.fingerprints.get(id).is_some_and(|known| known != fingerprint)
        {
            return Err(ManagerError::new(ManagerErrorCode::Signature, "signer trust conflicts"));
        }
        current.fingerprints.insert(id.to_owned(), fingerprint.to_owned());
        validate_trust_authority(&current)?;
        let trust_path = self.root.join(TRUST_NAME);
        if let Err(error) = replace_atomic_json(&trust_path, &current) {
            let _ = recover_atomic_json(&trust_path);
            let final_authority = load_trust_authority(&trust_path)?;
            if final_authority.fingerprints.get(id).map(String::as_str) == Some(fingerprint) {
                self.trusted_signers = final_authority;
                return Ok(());
            }
            let pending = trust_path.with_extension("next");
            if pending.exists()
                && load_trust_authority(&pending).is_ok_and(|authority| {
                    authority.fingerprints.get(id).map(String::as_str) == Some(fingerprint)
                })
            {
                return Err(ManagerError::new(
                    ManagerErrorCode::Indeterminate,
                    "signer trust publication remains pending",
                ));
            }
            return Err(error);
        }
        self.trusted_signers = current;
        Ok(())
    }

    /// Snapshot the global trust authority for a project store to reference and narrow.
    #[must_use]
    pub fn trusted_signers(&self) -> TrustedSigners {
        self.trusted_signers.clone()
    }

    /// Return an in-memory manager authority extended by one candidate signer.
    ///
    /// This does not persist trust. It is intended for an installation transaction
    /// which persists the signer only after package and configuration commit.
    pub fn with_candidate_signer(&self, id: &str, fingerprint: &str) -> Result<Self, ManagerError> {
        if !valid_key_id(id) {
            return Err(ManagerError::new(ManagerErrorCode::Signature, "signing key id rejected"));
        }
        validate_hash(fingerprint)?;
        let mut candidate = self.clone();
        if candidate.trusted_signers.revoked.contains(id)
            || candidate.trusted_signers.revoked_fingerprints.contains(fingerprint)
            || candidate
                .trusted_signers
                .fingerprints
                .iter()
                .any(|(known_id, known)| known_id != id && known == fingerprint)
            || candidate
                .trusted_signers
                .fingerprints
                .get(id)
                .is_some_and(|known| known != fingerprint)
        {
            return Err(ManagerError::new(ManagerErrorCode::Signature, "signer trust conflicts"));
        }
        candidate.trusted_signers.fingerprints.insert(id.to_owned(), fingerprint.to_owned());
        validate_trust_authority(&candidate.trusted_signers)?;
        Ok(candidate)
    }

    /// Store root for this explicit scope.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Inspect and authenticate one package file without mutating the store.
    pub fn inspect_file(
        &self,
        package: &Path,
        expected_sha256: Option<&str>,
        execution: &ExecutionContext,
    ) -> Result<InspectedPackage, ManagerError> {
        execution.checkpoint().map_err(map_execution_error)?;
        let mut file = open_package_file(package)?;
        let metadata = file.metadata()?;
        let identity = file_identity(&file)?;
        if metadata.len() == 0 || metadata.len() > MAX_PACKAGE_BYTES as u64 {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "package size rejected",
            ));
        }
        let (package_sha256, bytes_read) = copy_and_digest_bounded(
            &mut file,
            &mut std::io::sink(),
            MAX_PACKAGE_BYTES as u64,
            execution,
        )?;
        if bytes_read != metadata.len()
            || file.metadata()?.len() != metadata.len()
            || file_identity(&file)? != identity
        {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "package changed while inspecting",
            ));
        }
        if expected_sha256.is_some_and(|expected| expected != package_sha256) {
            return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "package hash mismatch"));
        }
        file.seek(SeekFrom::Start(0))?;
        let _zip_memory = preflight_zip(&mut file, metadata.len(), Some(execution))?;
        let mut archive = ZipArchive::new(file).map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "package is not a valid ZIP")
        })?;
        let _manifest_memory =
            execution.reserve_memory(MAX_MANIFEST_BYTES * 8).map_err(map_execution_error)?;
        let manifest = read_manifest(&mut archive, execution)?;
        validate_manifest(&manifest, &self.trusted_signers)?;
        Ok(InspectedPackage {
            id: manifest.id,
            protocol: manifest.protocol,
            package_sha256,
            content_root_sha256: manifest.signature.signed_payload_sha256,
            signing_key_id: manifest.signature.key_id,
        })
    }

    /// Copy the exact retained source package into a new private transaction file.
    pub fn snapshot_package(
        &self,
        id: &str,
        destination: &Path,
        execution: &ExecutionContext,
    ) -> Result<PackageSnapshot, ManagerError> {
        let _lock = self.acquire_lock()?;
        self.recover_unlocked()?;
        if destination.parent() != Some(self.root.as_path()) || destination.file_name().is_none() {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "snapshot destination must be a direct store child",
            ));
        }
        if destination.exists() {
            return Err(ManagerError::new(
                ManagerErrorCode::Conflict,
                "snapshot destination already exists",
            ));
        }
        let installed = self.verify_unlocked(id, execution)?;
        let source_path = installed.root.join(ARCHIVE_NAME);
        let mut source = open_package_file(&source_path)?;
        let metadata = source.metadata()?;
        let identity = file_identity(&source)?;
        let temporary = execution.reserve_temporary(metadata.len()).map_err(map_execution_error)?;
        let next = self.root.join(format!(
            ".snapshot-next-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut next_cleanup = TemporaryFile::new(next.clone());
        let mut output = OpenOptions::new().create_new(true).write(true).open(&next)?;
        let (hash, copied) =
            copy_and_digest_bounded(&mut source, &mut output, metadata.len(), execution)?;
        output.sync_all()?;
        drop(output);
        if copied != metadata.len()
            || hash != installed.package_sha256
            || source.metadata()?.len() != metadata.len()
            || file_identity(&source)? != identity
        {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "retained package changed while snapshotting",
            ));
        }
        secure_regular_file(&next)?;
        fs::rename(&next, destination)?;
        next_cleanup.keep();
        let cleanup = TemporaryFile::new(destination.to_owned());
        sync_directory(&self.root)?;
        Ok(PackageSnapshot {
            path: destination.to_owned(),
            installed,
            _temporary: temporary,
            cleanup,
        })
    }

    /// Validate and atomically install exact package bytes.
    pub fn install_bytes(
        &self,
        package: &[u8],
        expected_sha256: Option<&str>,
        execution: &ExecutionContext,
    ) -> Result<InstalledPlugin, ManagerError> {
        execution.checkpoint().map_err(map_execution_error)?;
        if package.is_empty() || package.len() > MAX_PACKAGE_BYTES {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "package size rejected",
            ));
        }
        let package_sha256 = digest(package);
        if expected_sha256.is_some_and(|expected| expected != package_sha256) {
            return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "package hash mismatch"));
        }
        let package_bytes = u64::try_from(package.len()).map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "package size overflow")
        })?;
        let _package_memory =
            execution.reserve_memory(package_bytes).map_err(map_execution_error)?;
        self.install_archive(Cursor::new(package), package_bytes, package_sha256, execution)
    }

    /// Validate and atomically install a package file without materializing it in memory.
    pub fn install_file(
        &self,
        package: &Path,
        expected_sha256: Option<&str>,
        execution: &ExecutionContext,
    ) -> Result<InstalledPlugin, ManagerError> {
        execution.checkpoint().map_err(map_execution_error)?;
        let mut file = open_package_file(package)?;
        let metadata = file.metadata()?;
        let identity = file_identity(&file)?;
        if metadata.len() == 0 || metadata.len() > MAX_PACKAGE_BYTES as u64 {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "package size rejected",
            ));
        }
        let lock = self.acquire_lock()?;
        self.recover_unlocked()?;
        let incoming_name =
            format!(".incoming-{}-{}", std::process::id(), NONCE.fetch_add(1, Ordering::Relaxed));
        let incoming_path = self.root.join(incoming_name);
        let _incoming_cleanup = TemporaryFile::new(incoming_path.clone());
        let _incoming_temporary =
            execution.reserve_temporary(metadata.len()).map_err(map_execution_error)?;
        let mut incoming =
            OpenOptions::new().create_new(true).read(true).write(true).open(&incoming_path)?;
        let (package_sha256, bytes_read) =
            copy_and_digest_bounded(&mut file, &mut incoming, MAX_PACKAGE_BYTES as u64, execution)?;
        incoming.sync_all()?;
        let final_metadata = file.metadata()?;
        if bytes_read != metadata.len()
            || final_metadata.len() != metadata.len()
            || file_identity(&file)? != identity
        {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "package changed while hashing",
            ));
        }
        if expected_sha256.is_some_and(|expected| expected != package_sha256) {
            return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "package hash mismatch"));
        }
        incoming.seek(SeekFrom::Start(0))?;
        self.install_archive_locked(incoming, metadata.len(), package_sha256, execution, lock)
    }

    fn install_archive<R: Read + Seek>(
        &self,
        package: R,
        package_bytes: u64,
        package_sha256: String,
        execution: &ExecutionContext,
    ) -> Result<InstalledPlugin, ManagerError> {
        let lock = self.acquire_lock()?;
        self.recover_unlocked()?;
        self.install_archive_locked(package, package_bytes, package_sha256, execution, lock)
    }

    fn install_archive_locked<R: Read + Seek>(
        &self,
        mut package: R,
        package_bytes: u64,
        package_sha256: String,
        execution: &ExecutionContext,
        _lock: StoreLock,
    ) -> Result<InstalledPlugin, ManagerError> {
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let staging_name = format!(".staging-{}-{nonce}", std::process::id());
        let _zip_memory = preflight_zip(&mut package, package_bytes, Some(execution))?;
        let mut archive = ZipArchive::new(package).map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "package is not a valid ZIP")
        })?;
        let _manifest_memory =
            execution.reserve_memory(MAX_MANIFEST_BYTES * 8).map_err(map_execution_error)?;
        let manifest = read_manifest(&mut archive, execution)?;
        let signer_fingerprint = validate_manifest(&manifest, &self.trusted_signers)?;
        let aggregate = manifest.files.iter().try_fold(0_u64, |total, file| {
            total.checked_add(file.bytes).ok_or_else(|| {
                ManagerError::new(ManagerErrorCode::InvalidPackage, "expanded size overflow")
            })
        })?;
        if aggregate > MAX_PACKAGE_BYTES as u64 {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "expanded size rejected",
            ));
        }
        let temporary_bytes = aggregate
            .checked_add(package_bytes)
            .and_then(|value| value.checked_add(MAX_MANIFEST_BYTES * 2))
            .ok_or_else(|| {
                ManagerError::new(ManagerErrorCode::ResourceLimit, "installation size overflow")
            })?;
        let _expanded_temporary =
            execution.reserve_temporary(temporary_bytes).map_err(map_execution_error)?;
        let destination_name = manifest.id.clone();
        let expected_content_root = manifest.signature.signed_payload_sha256.clone();
        let backup_name = format!(".backup-{}-{nonce}", manifest.id);
        let mut transaction = Transaction {
            phase: TransactionPhase::Staging,
            id: manifest.id.clone(),
            staging: staging_name.clone(),
            destination: destination_name.clone(),
            backup: backup_name.clone(),
        };
        write_atomic_json(&self.root.join(TRANSACTION_NAME), &transaction)?;
        let staging = self.root.join(&staging_name);
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir(&staging)?;
        let result = (|| {
            extract_verified(&mut archive, &manifest, &staging, execution)?;
            let mut source = archive.into_inner();
            source.seek(SeekFrom::Start(0))?;
            copy_exact_archive(
                &mut source,
                &staging.join(ARCHIVE_NAME),
                package_bytes,
                &package_sha256,
                execution,
            )?;
            execution.checkpoint().map_err(map_execution_error)?;
            let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|_| {
                ManagerError::new(ManagerErrorCode::InvalidPackage, "manifest serialization failed")
            })?;
            fs::write(staging.join(MANIFEST_NAME), &manifest_bytes)?;
            write_atomic_json(
                &staging.join(INSTALLED_NAME),
                &InstalledAuthority {
                    source_archive_sha256: package_sha256.clone(),
                    source_archive_bytes: package_bytes,
                    manifest_sha256: digest(&manifest_bytes),
                    signing_key_id: manifest.signature.key_id.clone(),
                    signing_key_fingerprint: signer_fingerprint,
                },
            )?;
            sync_tree(&staging)?;
            verify_tree(&manifest, &staging, Some(execution))?;
            transaction.phase = TransactionPhase::Ready;
            replace_atomic_json(&self.root.join(TRANSACTION_NAME), &transaction)?;
            let destination = self.root.join(&destination_name);
            let backup = self.root.join(&backup_name);
            if destination.exists() {
                fs::rename(&destination, &backup)?;
                transaction.phase = TransactionPhase::BackupMoved;
                replace_atomic_json(&self.root.join(TRANSACTION_NAME), &transaction)?;
            }
            fs::rename(&staging, &destination)?;
            if let Err(error) = sync_directory(&self.root) {
                let removed_new = fs::remove_dir_all(&destination).is_ok();
                let restored_old =
                    !backup.exists() || (removed_new && fs::rename(&backup, &destination).is_ok());
                let rollback_synced =
                    removed_new && restored_old && sync_directory(&self.root).is_ok();
                if rollback_synced {
                    let _ = fs::remove_file(self.root.join(TRANSACTION_NAME));
                    let _ = sync_directory(&self.root);
                }
                return Err(error);
            }
            // The rename plus successful parent-directory sync is the install
            // linearization point. Everything below is restartable cleanup and
            // cannot turn a committed install into an error return.
            transaction.phase = TransactionPhase::Published;
            let _ = replace_atomic_json(&self.root.join(TRANSACTION_NAME), &transaction);
            if backup.exists() {
                let _ = fs::remove_dir_all(&backup);
            }
            if !backup.exists() && !staging.exists() {
                let _ = fs::remove_file(self.root.join(TRANSACTION_NAME));
                let _ = sync_directory(&self.root);
            }
            Ok(InstalledPlugin {
                id: manifest.id,
                version: manifest.version,
                protocol: manifest.protocol,
                package_sha256,
                content_root_sha256: manifest.signature.signed_payload_sha256,
                signing_key_id: manifest.signature.key_id,
                root: destination,
            })
        })();
        if let Err(error) = result {
            if self.recover_unlocked().is_ok()
                && let Ok(installed) = self.verify_unlocked(&destination_name, execution)
                && installed.content_root_sha256 == expected_content_root
            {
                return Ok(installed);
            }
            return Err(error);
        }
        result
    }

    /// Re-hash and re-validate one installed package before use.
    pub fn verify(
        &self,
        id: &str,
        execution: &ExecutionContext,
    ) -> Result<InstalledPlugin, ManagerError> {
        let _lock = self.acquire_lock()?;
        self.recover_unlocked()?;
        self.verify_unlocked(id, execution)
    }

    fn verify_unlocked(
        &self,
        id: &str,
        execution: &ExecutionContext,
    ) -> Result<InstalledPlugin, ManagerError> {
        execution.checkpoint().map_err(map_execution_error)?;
        let _metadata_memory =
            execution.reserve_memory(MAX_MANIFEST_BYTES * 16).map_err(map_execution_error)?;
        validate_id(id)?;
        let root = self.root.join(id);
        if !root.is_dir() {
            return Err(ManagerError::new(
                ManagerErrorCode::NotInstalled,
                "plugin is not installed",
            ));
        }
        reject_link(&root)?;
        let (manifest_bytes, _manifest_memory) =
            bounded_read_accounted(&root.join(MANIFEST_NAME), MAX_MANIFEST_BYTES, execution)?;
        let manifest: PackageManifest = serde_json::from_slice(&manifest_bytes).map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "installed manifest is invalid")
        })?;
        let fingerprint = validate_manifest(&manifest, &self.trusted_signers)?;
        if manifest.id != id {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "installed id mismatch",
            ));
        }
        let (authority_bytes, _authority_memory) =
            bounded_read_accounted(&root.join(INSTALLED_NAME), MAX_MANIFEST_BYTES, execution)?;
        let authority: InstalledAuthority =
            serde_json::from_slice(&authority_bytes).map_err(|_| {
                ManagerError::new(ManagerErrorCode::InvalidPackage, "authority is invalid")
            })?;
        if authority.manifest_sha256 != digest(&manifest_bytes) {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "manifest hash mismatch",
            ));
        }
        let archive = root.join(ARCHIVE_NAME);
        let archive_metadata = secure_regular_file(&archive)?;
        if archive_metadata.len() != authority.source_archive_bytes
            || digest_file(&archive, execution)? != authority.source_archive_sha256
        {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "installed package archive changed",
            ));
        }
        verify_archive(&archive, &manifest, Some(execution))?;
        if authority.signing_key_id != manifest.signature.key_id
            || authority.signing_key_fingerprint != fingerprint
        {
            return Err(ManagerError::new(ManagerErrorCode::Signature, "installed signer drifted"));
        }
        verify_tree(&manifest, &root, Some(execution))?;
        Ok(InstalledPlugin {
            id: manifest.id,
            version: manifest.version,
            protocol: manifest.protocol,
            package_sha256: authority.source_archive_sha256,
            content_root_sha256: manifest.signature.signed_payload_sha256,
            signing_key_id: manifest.signature.key_id,
            root,
        })
    }

    /// Verify every installed package. Hidden transaction directories are ignored.
    pub fn verify_all(
        &self,
        execution: &ExecutionContext,
    ) -> Result<Vec<InstalledPlugin>, ManagerError> {
        let _lock = self.acquire_lock()?;
        self.recover_unlocked()?;
        let mut result = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && !name.starts_with('.') {
                result.push(self.verify_unlocked(&name, execution)?);
            }
        }
        result.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(result)
    }

    /// Remove an installed package through an atomic visibility barrier.
    pub fn remove(&self, id: &str) -> Result<(), ManagerError> {
        validate_id(id)?;
        let _lock = self.acquire_lock()?;
        self.recover_unlocked()?;
        let source = self.root.join(id);
        if !source.is_dir() {
            return Err(ManagerError::new(
                ManagerErrorCode::NotInstalled,
                "plugin is not installed",
            ));
        }
        reject_link(&source)?;
        let removed_name = format!(".removed-{}-{}", id, NONCE.fetch_add(1, Ordering::Relaxed));
        let removed = self.root.join(&removed_name);
        let transaction = Transaction {
            phase: TransactionPhase::Removing,
            id: id.to_owned(),
            staging: format!(".unused-{}", std::process::id()),
            destination: id.to_owned(),
            backup: removed_name,
        };
        write_atomic_json(&self.root.join(TRANSACTION_NAME), &transaction)?;
        fs::rename(&source, &removed)?;
        if sync_directory(&self.root).is_err() {
            // The source name is already gone in this process.  Returning an
            // error here would be ambiguous: recovery deliberately completes
            // a Removing transaction by deleting the quarantined tree.  Keep
            // the durable intent and report the operation as committed; the
            // next open retries the directory sync and cleanup.
            if source.exists() || !removed.is_dir() {
                return Err(ManagerError::new(
                    ManagerErrorCode::Io,
                    "plugin removal visibility could not be established",
                ));
            }
            reject_link(&removed)?;
            return Ok(());
        }
        // The rename plus directory sync is the linearization point. Cleanup is
        // restartable and must not report failure after the plugin disappeared.
        if fs::remove_dir_all(removed).is_ok() && cleanup_process_identity(&self.root, id).is_ok() {
            let _ = fs::remove_file(self.root.join(TRANSACTION_NAME));
            let _ = sync_directory(&self.root);
        }
        Ok(())
    }

    /// Build the immutable process-v1 runtime authority from verified bytes.
    pub fn process_manifest(
        &self,
        id: &str,
        #[allow(unused_mut)] mut policy: into_markdown_process_plugin::RuntimePolicy,
        execution: &ExecutionContext,
    ) -> Result<PreparedProcessPlugin, ManagerError> {
        let _store_lock = self.acquire_lock()?;
        self.recover_unlocked()?;
        let installed = self.verify_unlocked(id, execution)?;
        let metadata_reservation =
            execution.reserve_memory(MAX_MANIFEST_BYTES * 8).map_err(map_execution_error)?;
        let manifest = read_installed_manifest(&installed.root)?;
        if manifest.protocol != "process-v1" {
            return Err(ManagerError::new(ManagerErrorCode::UnsupportedProtocol, "not process-v1"));
        }
        let entry = entrypoint(&manifest)?;
        let file = manifest.files.iter().find(|file| file.path == entry).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "entrypoint is not hash-bound")
        })?;
        if !policy.environment.is_empty() {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "manager policy forbids plugin environment capabilities",
            ));
        }
        #[cfg(windows)]
        {
            policy.windows = into_markdown_process_plugin::provision_windows_sandbox(&format!(
                "{}:{id}",
                self.root.display()
            ))
            .map_err(|error| ManagerError::new(ManagerErrorCode::Io, error.to_string()))?;
        }
        let snapshot_bytes = tree_size(&installed.root, execution)?;
        let snapshot_reservation =
            execution.reserve_temporary(snapshot_bytes).map_err(map_execution_error)?;
        // The verified copy is independent of the mutable package store. Keeping it in an
        // owner-private temporary directory lets one conversion retain multiple providers from
        // the same store without holding the store-wide mutation lock for their whole lifetime.
        let snapshot_directory = tempfile::Builder::new()
            .prefix(&format!("into-md-plugin-dispatch-{}-", NONCE.fetch_add(1, Ordering::Relaxed)))
            .tempdir()
            .map_err(ManagerError::from)?;
        let snapshot = snapshot_directory.path().join("runtime");
        copy_tree(&installed.root, &snapshot, execution)?;
        verify_tree(&manifest, &snapshot, Some(execution))?;
        #[cfg(windows)]
        into_markdown_process_plugin::authorize_windows_sandbox_path(&policy.windows, &snapshot)
            .map_err(|error| ManagerError::new(ManagerErrorCode::Io, error.to_string()))?;
        let authority = into_markdown_process_plugin::PluginManifest {
            plugin_id: manifest.id,
            executable: snapshot.join(&entry),
            runtime_root: snapshot.clone(),
            executable_sha256: file.sha256.clone(),
            protocol_versions: vec![1],
        };
        Ok(PreparedProcessPlugin {
            authority,
            policy,
            _snapshot_directory: snapshot_directory,
            _snapshot_reservation: snapshot_reservation,
            _metadata_reservation: metadata_reservation,
        })
    }

    /// Load a WASI runtime manifest while proving its network authority is a subset of this call.
    pub fn prepare_wasi(
        &self,
        id: &str,
        invocation_capabilities: &WasiCapabilities,
        execution: &ExecutionContext,
    ) -> Result<PreparedWasiPlugin, ManagerError> {
        let store_lock = self.acquire_lock()?;
        self.recover_unlocked()?;
        let installed = self.verify_unlocked(id, execution)?;
        let metadata_reservation =
            execution.reserve_memory(MAX_MANIFEST_BYTES * 16).map_err(map_execution_error)?;
        let package = read_installed_manifest(&installed.root)?;
        if package.protocol != "wasi-v1" {
            return Err(ManagerError::new(ManagerErrorCode::UnsupportedProtocol, "not wasi-v1"));
        }
        let runtime_path = package.runtime_manifest.as_deref().ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "WASI runtime manifest missing")
        })?;
        let runtime: WasiPluginManifest = serde_json::from_slice(&bounded_read(
            &installed.root.join(runtime_path),
            MAX_MANIFEST_BYTES,
        )?)
        .map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "WASI manifest invalid")
        })?;
        let entry = entrypoint(&package)?;
        let component = installed.root.join(&entry);
        let entry_file = package.files.iter().find(|file| file.path == entry).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "component is not hash-bound")
        })?;
        if runtime.id != package.id
            || runtime.protocol != "wasi-v1"
            || runtime.component_sha256 != entry_file.sha256
            || runtime.component_bytes != entry_file.bytes
            || !runtime.capabilities.preopens.is_empty()
            || runtime.capabilities.clocks && !invocation_capabilities.clocks
            || runtime.capabilities.random && !invocation_capabilities.random
            || runtime.capabilities.network.iter().any(|grant| {
                !invocation_capabilities.network.iter().any(|authorized| {
                    authorized.address == grant.address
                        && authorized.port == grant.port
                        && (!grant.allow_private || authorized.allow_private)
                })
            })
        {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "WASI authority exceeds installed or invocation authority",
            ));
        }
        let component_reservation =
            execution.reserve_memory(entry_file.bytes).map_err(map_execution_error)?;
        execution.checkpoint().map_err(map_execution_error)?;
        let bytes = bounded_read(&component, entry_file.bytes)?;
        if digest(&bytes) != entry_file.sha256 {
            return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "component changed"));
        }
        let runtime_engine = into_markdown_plugin_wasi::WasiPluginRuntime::new()
            .map_err(|error| ManagerError::new(ManagerErrorCode::Io, error.to_string()))?;
        Ok(PreparedWasiPlugin {
            runtime: runtime_engine,
            manifest: runtime,
            component: bytes,
            _component_reservation: component_reservation,
            _metadata_reservation: metadata_reservation,
            _store_lock: store_lock,
        })
    }

    fn recover(&self) -> Result<(), ManagerError> {
        let _lock = self.acquire_lock()?;
        self.recover_unlocked()
    }

    fn recover_unlocked(&self) -> Result<(), ManagerError> {
        let marker = self.root.join(TRANSACTION_NAME);
        let temporary = marker.with_extension("tmp");
        if temporary.exists() {
            fs::remove_file(temporary)?;
        }
        let next = marker.with_extension("next");
        let previous = marker.with_extension("previous");
        if !marker.exists() {
            if next.exists() {
                fs::rename(&next, &marker)?;
            } else if previous.exists() {
                fs::rename(&previous, &marker)?;
            }
        }
        for obsolete in [&next, &previous] {
            if obsolete.exists() {
                fs::remove_file(obsolete)?;
            }
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && name.starts_with(".dispatch-") {
                fs::remove_dir_all(entry.path())?;
            } else if orphan_temporary_name(&name) {
                if !entry.file_type()?.is_file() {
                    return Err(ManagerError::new(
                        ManagerErrorCode::PathTraversal,
                        "temporary package entry identity rejected",
                    ));
                }
                secure_regular_file(&entry.path())?;
                fs::remove_file(entry.path())?;
            }
        }
        if !marker.exists() {
            return Ok(());
        }
        let transaction: Transaction =
            serde_json::from_slice(&bounded_read(&marker, MAX_MANIFEST_BYTES)?).map_err(|_| {
                ManagerError::new(ManagerErrorCode::InvalidPackage, "transaction marker invalid")
            })?;
        validate_id(&transaction.id)?;
        validate_transaction(&transaction)?;
        let staging = self.root.join(&transaction.staging);
        let destination = self.root.join(&transaction.destination);
        let backup = self.root.join(&transaction.backup);
        match transaction.phase {
            TransactionPhase::Staging => {
                if staging.exists() {
                    fs::remove_dir_all(&staging)?;
                }
                if !destination.exists() && backup.exists() {
                    fs::rename(&backup, &destination)?;
                }
            }
            TransactionPhase::Ready => {
                if destination.exists() {
                    verify_staged(&destination, &self.trusted_signers)?;
                } else if staging.exists() {
                    verify_staged(&staging, &self.trusted_signers)?;
                    fs::rename(&staging, &destination)?;
                } else if backup.exists() {
                    fs::rename(&backup, &destination)?;
                }
                if backup.exists() && destination.exists() {
                    fs::remove_dir_all(&backup)?;
                }
                if staging.exists() {
                    fs::remove_dir_all(&staging)?;
                }
            }
            TransactionPhase::BackupMoved => {
                let destination_valid = destination.exists()
                    && verify_staged(&destination, &self.trusted_signers).is_ok();
                if !destination_valid && destination.exists() {
                    fs::remove_dir_all(&destination)?;
                }
                if !destination.exists() {
                    if staging.exists() && verify_staged(&staging, &self.trusted_signers).is_ok() {
                        fs::rename(&staging, &destination)?;
                    } else if backup.exists() {
                        verify_staged(&backup, &self.trusted_signers)?;
                        fs::rename(&backup, &destination)?;
                    }
                }
                if backup.exists() && destination.exists() {
                    fs::remove_dir_all(&backup)?;
                }
                if staging.exists() {
                    fs::remove_dir_all(&staging)?;
                }
            }
            TransactionPhase::Published => {
                verify_staged(&destination, &self.trusted_signers)?;
                if staging.exists() {
                    fs::remove_dir_all(&staging)?;
                }
                if backup.exists() {
                    fs::remove_dir_all(&backup)?;
                }
            }
            TransactionPhase::Removing => {
                if destination.exists() && backup.exists() {
                    return Err(ManagerError::new(
                        ManagerErrorCode::Conflict,
                        "ambiguous plugin removal transaction",
                    ));
                }
                // A directory rename whose parent sync failed may be rolled
                // back by a crash.  The durable Removing intent is a forward
                // commit record, so replay the rename rather than silently
                // restoring the plugin after remove returned success.
                if destination.exists() {
                    reject_link(&destination)?;
                    fs::rename(&destination, &backup)?;
                    sync_directory(&self.root)?;
                }
                if !destination.exists() && backup.exists() {
                    fs::remove_dir_all(&backup)?;
                }
                if !destination.exists() {
                    cleanup_process_identity(&self.root, &transaction.id)?;
                }
            }
        }
        fs::remove_file(marker)?;
        sync_directory(&self.root)?;
        Ok(())
    }
}

fn cleanup_process_identity(root: &Path, id: &str) -> Result<(), ManagerError> {
    #[cfg(windows)]
    into_markdown_process_plugin::remove_windows_sandbox(&format!("{}:{id}", root.display()))
        .map_err(|error| ManagerError::new(ManagerErrorCode::Io, error.to_string()))?;
    #[cfg(not(windows))]
    let _ = (root, id);
    Ok(())
}

fn validate_manifest(
    manifest: &PackageManifest,
    trusted: &TrustedSigners,
) -> Result<String, ManagerError> {
    validate_id(&manifest.id)?;
    validate_version(&manifest.version)?;
    if manifest.schema_version != 1
        || !matches!(manifest.protocol.as_str(), "process-v1" | "wasi-v1")
    {
        return Err(ManagerError::new(
            ManagerErrorCode::UnsupportedProtocol,
            "manifest protocol rejected",
        ));
    }
    let current = current_target();
    if !manifest.supported_targets.contains(current)
        || manifest.entrypoints.keys().any(|target| !known_target(target))
    {
        return Err(ManagerError::new(
            ManagerErrorCode::UnsupportedTarget,
            "manifest target rejected",
        ));
    }
    if manifest.supported_targets != manifest.entrypoints.keys().cloned().collect() {
        return Err(ManagerError::new(
            ManagerErrorCode::UnsupportedTarget,
            "entrypoint target set differs",
        ));
    }
    if manifest.files.is_empty() || manifest.files.len() > MAX_FILES {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "file count rejected"));
    }
    let mut previous: Option<&str> = None;
    let mut paths = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    for file in &manifest.files {
        validate_package_path(&file.path)?;
        validate_hash(&file.sha256)?;
        if file.bytes > MAX_FILE_BYTES
            || !paths.insert(file.path.clone())
            || !aliases.insert(file.path.to_ascii_lowercase())
        {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "file authority rejected",
            ));
        }
        if previous.is_some_and(|value| value >= file.path.as_str()) {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "file authority is not sorted",
            ));
        }
        previous = Some(&file.path);
    }
    for entry in manifest.entrypoints.values() {
        validate_package_path(entry)?;
        if !paths.contains(entry) {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "entrypoint is not listed",
            ));
        }
    }
    if manifest.protocol == "wasi-v1" {
        let runtime = manifest.runtime_manifest.as_deref().ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "WASI runtime manifest missing")
        })?;
        validate_package_path(runtime)?;
        if !paths.contains(runtime) {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "WASI manifest is not listed",
            ));
        }
    } else if manifest.runtime_manifest.is_some() {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "unexpected runtime manifest",
        ));
    }
    verify_signature(manifest, trusted)
}

fn validate_trust_authority(trusted: &TrustedSigners) -> Result<(), ManagerError> {
    let mut fingerprints = BTreeSet::new();
    for (id, fingerprint) in &trusted.fingerprints {
        if !valid_key_id(id) {
            return Err(ManagerError::new(ManagerErrorCode::Signature, "trusted key id rejected"));
        }
        validate_hash(fingerprint)?;
        if !fingerprints.insert(fingerprint) {
            return Err(ManagerError::new(
                ManagerErrorCode::Signature,
                "trusted key alias rejected",
            ));
        }
    }
    for id in &trusted.revoked {
        if !valid_key_id(id) {
            return Err(ManagerError::new(ManagerErrorCode::Signature, "revoked key id rejected"));
        }
    }
    for fingerprint in &trusted.revoked_fingerprints {
        validate_hash(fingerprint)?;
    }
    Ok(())
}

fn verify_signature(
    manifest: &PackageManifest,
    trusted: &TrustedSigners,
) -> Result<String, ManagerError> {
    let signed = &manifest.signature;
    if signed.signed_payload_version != 1
        || signed.algorithm != "ed25519"
        || !valid_key_id(&signed.key_id)
        || trusted.revoked.contains(&signed.key_id)
    {
        return Err(ManagerError::new(ManagerErrorCode::Signature, "signature metadata rejected"));
    }
    validate_hash(&signed.signed_payload_sha256)?;
    validate_hash(&signed.public_key_sha256)?;
    let payload = canonical_signed_payload(manifest)?;
    if digest(&payload) != signed.signed_payload_sha256 {
        return Err(ManagerError::new(ManagerErrorCode::Signature, "signed payload hash mismatch"));
    }
    let public_key =
        base64::engine::general_purpose::STANDARD.decode(&signed.public_key_base64).map_err(
            |_| ManagerError::new(ManagerErrorCode::Signature, "public key encoding rejected"),
        )?;
    let signature =
        base64::engine::general_purpose::STANDARD.decode(&signed.signature_base64).map_err(
            |_| ManagerError::new(ManagerErrorCode::Signature, "signature encoding rejected"),
        )?;
    if public_key.len() != 32 || signature.len() != 64 {
        return Err(ManagerError::new(ManagerErrorCode::Signature, "signature size rejected"));
    }
    let fingerprint = digest(&public_key);
    if signed.public_key_sha256 != fingerprint
        || trusted.revoked_fingerprints.contains(&fingerprint)
        || trusted.fingerprints.get(&signed.key_id) != Some(&fingerprint)
        || trusted
            .fingerprints
            .iter()
            .any(|(id, value)| id != &signed.key_id && value == &fingerprint)
    {
        return Err(ManagerError::new(ManagerErrorCode::Signature, "publisher is not trusted"));
    }
    UnparsedPublicKey::new(&signature::ED25519, public_key).verify(&payload, &signature).map_err(
        |_| ManagerError::new(ManagerErrorCode::Signature, "signature verification failed"),
    )?;
    Ok(fingerprint)
}

fn read_manifest<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    execution: &ExecutionContext,
) -> Result<PackageManifest, ManagerError> {
    let mut entry = archive
        .by_name(MANIFEST_NAME)
        .map_err(|_| ManagerError::new(ManagerErrorCode::InvalidPackage, "plugin.json missing"))?;
    if entry.size() > MAX_MANIFEST_BYTES || !entry.is_file() {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "plugin.json rejected"));
    }
    let declared = entry.size();
    let bytes = read_zip_entry_bounded(&mut entry, declared, MAX_MANIFEST_BYTES, Some(execution))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| ManagerError::new(ManagerErrorCode::InvalidPackage, "plugin.json invalid"))
}

fn read_zip_entry_bounded(
    entry: &mut impl Read,
    declared: u64,
    maximum: u64,
    execution: Option<&ExecutionContext>,
) -> Result<Vec<u8>, ManagerError> {
    if declared > maximum {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "ZIP entry size rejected"));
    }
    let _memory = execution
        .map(|context| context.reserve_memory(declared).map_err(map_execution_error))
        .transpose()?;
    let capacity = usize::try_from(declared)
        .map_err(|_| ManagerError::new(ManagerErrorCode::ResourceLimit, "ZIP size overflow"))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| {
        ManagerError::new(ManagerErrorCode::ResourceLimit, "ZIP entry allocation rejected")
    })?;
    let mut buffer = [0_u8; 16 * 1024];
    while (bytes.len() as u64) < declared {
        if let Some(context) = execution {
            context.checkpoint().map_err(map_execution_error)?;
        }
        let remaining = declared - bytes.len() as u64;
        let limit = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = entry.read(&mut buffer[..limit])?;
        if read == 0 {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "ZIP entry size differs from central directory",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if let Some(context) = execution {
        context.checkpoint().map_err(map_execution_error)?;
    }
    let mut probe = [0_u8; 1];
    if entry.read(&mut probe)? != 0 {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "ZIP entry exceeds declared size",
        ));
    }
    Ok(bytes)
}

fn preflight_zip<R: Read + Seek>(
    package: &mut R,
    expected_bytes: u64,
    execution: Option<&ExecutionContext>,
) -> Result<Vec<ResourceReservation>, ManagerError> {
    const EOCD_BYTES: u64 = 22;
    const MAX_COMMENT_BYTES: u64 = u16::MAX as u64;
    if expected_bytes < EOCD_BYTES || expected_bytes > MAX_PACKAGE_BYTES as u64 {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "ZIP size rejected"));
    }
    let actual_bytes = package.seek(SeekFrom::End(0))?;
    if actual_bytes != expected_bytes {
        return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "ZIP size changed"));
    }
    let tail_bytes = actual_bytes.min(EOCD_BYTES + MAX_COMMENT_BYTES);
    let mut reservations = Vec::new();
    if let Some(execution) = execution {
        reservations.push(execution.reserve_memory(tail_bytes).map_err(map_execution_error)?);
        execution.checkpoint().map_err(map_execution_error)?;
    }
    let tail_offset = i64::try_from(tail_bytes)
        .map_err(|_| ManagerError::new(ManagerErrorCode::ResourceLimit, "ZIP tail overflow"))?;
    package.seek(SeekFrom::End(-tail_offset))?;
    let mut tail = vec![
        0_u8;
        usize::try_from(tail_bytes).map_err(|_| {
            ManagerError::new(ManagerErrorCode::ResourceLimit, "ZIP tail size overflow")
        })?
    ];
    package.read_exact(&mut tail)?;
    let position = tail
        .windows(4)
        .rposition(|bytes| bytes == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(|| ManagerError::new(ManagerErrorCode::InvalidPackage, "ZIP EOCD missing"))?;
    if tail.len() - position < 22 {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "ZIP EOCD truncated"));
    }
    let eocd = &tail[position..];
    let u16_at = |offset: usize| u16::from_le_bytes([eocd[offset], eocd[offset + 1]]);
    let u32_at = |offset: usize| {
        u32::from_le_bytes([eocd[offset], eocd[offset + 1], eocd[offset + 2], eocd[offset + 3]])
    };
    let comment_bytes = usize::from(u16_at(20));
    if eocd.len() != 22 + comment_bytes
        || u16_at(4) != 0
        || u16_at(6) != 0
        || u16_at(8) != u16_at(10)
    {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "multi-disk or ambiguous ZIP rejected",
        ));
    }
    let entries = u16_at(10);
    let central_bytes = u64::from(u32_at(12));
    let central_offset = u64::from(u32_at(16));
    if entries == u16::MAX
        || central_bytes == u64::from(u32::MAX)
        || central_offset == u64::from(u32::MAX)
    {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "ZIP64 packages are unsupported",
        ));
    }
    if usize::from(entries) > MAX_FILES + 1
        || central_bytes > MAX_ZIP_CENTRAL_DIRECTORY_BYTES
        || central_offset
            .checked_add(central_bytes)
            .is_none_or(|end| end > actual_bytes.saturating_sub(eocd.len() as u64))
    {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "ZIP central directory rejected",
        ));
    }
    if let Some(execution) = execution {
        // `zip` materializes entry metadata and names while parsing the central directory.
        let parser_bytes = central_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(u64::from(entries) * 256))
            .ok_or_else(|| {
                ManagerError::new(ManagerErrorCode::ResourceLimit, "ZIP metadata size overflow")
            })?;
        reservations.push(execution.reserve_memory(parser_bytes).map_err(map_execution_error)?);
        execution.checkpoint().map_err(map_execution_error)?;
    }
    package.seek(SeekFrom::Start(0))?;
    Ok(reservations)
}

fn extract_verified<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    manifest: &PackageManifest,
    staging: &Path,
    execution: &ExecutionContext,
) -> Result<(), ManagerError> {
    if archive.len() != manifest.files.len() + 1 || archive.len() > MAX_FILES + 1 {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "ZIP inventory differs"));
    }
    let authorities =
        manifest.files.iter().map(|file| (file.path.as_str(), file)).collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for index in 0..archive.len() {
        execution.checkpoint().map_err(map_execution_error)?;
        let mut entry = archive.by_index(index).map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "ZIP entry unavailable")
        })?;
        let name = entry.name().to_owned();
        let special_mode = entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 != 0 && mode & 0o170_000 != 0o100_000);
        if !seen.insert(name.clone()) || entry.is_dir() || entry.is_symlink() || special_mode {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "ZIP entry type rejected",
            ));
        }
        if name == MANIFEST_NAME {
            continue;
        }
        validate_package_path(&name)?;
        let authority = authorities.get(name.as_str()).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "ZIP entry is not authorized")
        })?;
        if entry.compressed_size() > 0 && authority.bytes / entry.compressed_size().max(1) > 200 {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "compression ratio rejected",
            ));
        }
        if entry.size() != authority.bytes || entry.size() > MAX_FILE_BYTES {
            return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "file size mismatch"));
        }
        let target = staging.join(&name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new().create_new(true).write(true).open(&target)?;
        let mut hasher = Sha256::new();
        let mut remaining = authority.bytes;
        let mut buffer = [0_u8; 16 * 1024];
        while remaining > 0 {
            execution.checkpoint().map_err(map_execution_error)?;
            let take = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
            let read = entry.read(&mut buffer[..take])?;
            if read == 0 {
                return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "file truncated"));
            }
            output.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            remaining -= u64::try_from(read).unwrap_or(u64::MAX);
        }
        if format!("{:x}", hasher.finalize()) != authority.sha256 {
            return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "file hash mismatch"));
        }
        output.sync_all()?;
        #[cfg(unix)]
        if authority.executable || manifest.entrypoints.values().any(|entry| entry == &name) {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o500))?;
        }
    }
    if seen.len() != manifest.files.len() + 1 {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "ZIP inventory incomplete",
        ));
    }
    Ok(())
}

fn verify_archive(
    path: &Path,
    manifest: &PackageManifest,
    execution: Option<&ExecutionContext>,
) -> Result<(), ManagerError> {
    let mut file = open_package_file(path)?;
    let package_bytes = file.metadata()?.len();
    let _zip_memory = preflight_zip(&mut file, package_bytes, execution)?;
    let mut archive = ZipArchive::new(file).map_err(|_| {
        ManagerError::new(ManagerErrorCode::InvalidPackage, "retained archive is invalid")
    })?;
    if archive.len() != manifest.files.len() + 1 || archive.len() > MAX_FILES + 1 {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "retained archive inventory differs",
        ));
    }
    let authorities =
        manifest.files.iter().map(|file| (file.path.as_str(), file)).collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for index in 0..archive.len() {
        if let Some(execution) = execution {
            execution.checkpoint().map_err(map_execution_error)?;
        }
        let mut entry = archive.by_index(index).map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "archive entry unavailable")
        })?;
        let name = entry.name().to_owned();
        let special_mode = entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 != 0 && mode & 0o170_000 != 0o100_000);
        if !seen.insert(name.clone()) || entry.is_dir() || entry.is_symlink() || special_mode {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "retained archive entry rejected",
            ));
        }
        if name == MANIFEST_NAME {
            if entry.size() > MAX_MANIFEST_BYTES {
                return Err(ManagerError::new(
                    ManagerErrorCode::InvalidPackage,
                    "retained manifest size rejected",
                ));
            }
            let declared = entry.size();
            let bytes =
                read_zip_entry_bounded(&mut entry, declared, MAX_MANIFEST_BYTES, execution)?;
            let retained: PackageManifest = serde_json::from_slice(&bytes).map_err(|_| {
                ManagerError::new(ManagerErrorCode::InvalidPackage, "retained manifest invalid")
            })?;
            if retained != *manifest {
                return Err(ManagerError::new(
                    ManagerErrorCode::HashMismatch,
                    "retained manifest differs",
                ));
            }
            continue;
        }
        validate_package_path(&name)?;
        let authority = authorities.get(name.as_str()).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "retained entry unauthorized")
        })?;
        if entry.size() != authority.bytes || entry.size() > MAX_FILE_BYTES {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "retained entry size differs",
            ));
        }
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            if let Some(execution) = execution {
                execution.checkpoint().map_err(map_execution_error)?;
            }
            let read = entry.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > authority.bytes {
                return Err(ManagerError::new(
                    ManagerErrorCode::HashMismatch,
                    "retained entry grew",
                ));
            }
            hasher.update(&buffer[..read]);
        }
        if total != authority.bytes || format!("{:x}", hasher.finalize()) != authority.sha256 {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "retained entry differs",
            ));
        }
    }
    Ok(())
}

fn verify_tree(
    manifest: &PackageManifest,
    root: &Path,
    execution: Option<&ExecutionContext>,
) -> Result<(), ManagerError> {
    let expected = manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .chain([MANIFEST_NAME.to_owned(), INSTALLED_NAME.to_owned(), ARCHIVE_NAME.to_owned()])
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    collect_tree(root, root, &mut actual, execution)?;
    if actual != expected {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "installed tree inventory differs",
        ));
    }
    for authority in &manifest.files {
        let path = root.join(&authority.path);
        let metadata = secure_regular_file(&path)?;
        if metadata.len() != authority.bytes
            || streaming_digest(&path, authority.bytes, execution)? != authority.sha256
        {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "installed file differs",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let executable = authority.executable
                || manifest.entrypoints.values().any(|entry| entry == &authority.path);
            if metadata.permissions().mode() & 0o111 != if executable { 0o100 } else { 0 } {
                return Err(ManagerError::new(
                    ManagerErrorCode::InvalidPackage,
                    "installed executable authority differs",
                ));
            }
        }
    }
    Ok(())
}

fn verify_staged(root: &Path, trusted: &TrustedSigners) -> Result<(), ManagerError> {
    let manifest = read_installed_manifest(root)?;
    let fingerprint = validate_manifest(&manifest, trusted)?;
    let authority: InstalledAuthority =
        serde_json::from_slice(&bounded_read(&root.join(INSTALLED_NAME), MAX_MANIFEST_BYTES)?)
            .map_err(|_| {
                ManagerError::new(ManagerErrorCode::InvalidPackage, "authority invalid")
            })?;
    let manifest_bytes = bounded_read(&root.join(MANIFEST_NAME), MAX_MANIFEST_BYTES)?;
    if authority.manifest_sha256 != digest(&manifest_bytes)
        || authority.signing_key_id != manifest.signature.key_id
        || authority.signing_key_fingerprint != fingerprint
    {
        return Err(ManagerError::new(ManagerErrorCode::Signature, "staged authority differs"));
    }
    let archive = root.join(ARCHIVE_NAME);
    let metadata = secure_regular_file(&archive)?;
    if metadata.len() != authority.source_archive_bytes
        || streaming_digest(&archive, authority.source_archive_bytes, None)?
            != authority.source_archive_sha256
    {
        return Err(ManagerError::new(
            ManagerErrorCode::HashMismatch,
            "staged package archive differs",
        ));
    }
    verify_archive(&archive, &manifest, None)?;
    verify_tree(&manifest, root, None)
}

fn collect_tree(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
    execution: Option<&ExecutionContext>,
) -> Result<(), ManagerError> {
    reject_link(directory)?;
    secure_directory(directory)?;
    for entry in fs::read_dir(directory)? {
        if let Some(execution) = execution {
            execution.checkpoint().map_err(map_execution_error)?;
        }
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "installed link rejected",
            ));
        }
        if kind.is_dir() {
            collect_tree(root, &path, files, execution)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).map_err(|_| {
                ManagerError::new(ManagerErrorCode::PathTraversal, "installed path escaped")
            })?;
            let name = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            validate_relative_path(&name)?;
            if !files.insert(name) {
                return Err(ManagerError::new(
                    ManagerErrorCode::InvalidPackage,
                    "installed alias duplicated",
                ));
            }
            let _ = secure_regular_file(&path)?;
        } else {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "special file rejected",
            ));
        }
    }
    Ok(())
}

fn streaming_digest(
    path: &Path,
    expected: u64,
    execution: Option<&ExecutionContext>,
) -> Result<String, ManagerError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if let Some(execution) = execution {
            execution.checkpoint().map_err(map_execution_error)?;
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "file size overflow")
        })?;
        if total > expected {
            return Err(ManagerError::new(
                ManagerErrorCode::HashMismatch,
                "file grew while hashing",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if total != expected {
        return Err(ManagerError::new(
            ManagerErrorCode::HashMismatch,
            "file changed while hashing",
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_installed_manifest(root: &Path) -> Result<PackageManifest, ManagerError> {
    serde_json::from_slice(&bounded_read(&root.join(MANIFEST_NAME), MAX_MANIFEST_BYTES)?)
        .map_err(|_| ManagerError::new(ManagerErrorCode::InvalidPackage, "manifest invalid"))
}

fn entrypoint(manifest: &PackageManifest) -> Result<String, ManagerError> {
    manifest.entrypoints.get(current_target()).cloned().ok_or_else(|| {
        ManagerError::new(ManagerErrorCode::UnsupportedTarget, "current target has no entrypoint")
    })
}

fn validate_relative_path(value: &str) -> Result<(), ManagerError> {
    if value.is_empty() || value.len() > 1024 || value.contains(['\\', '\0']) || !value.is_ascii() {
        return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "package path rejected"));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|part| !matches!(part, Component::Normal(_)))
        || value.split('/').any(|part| {
            part.is_empty()
                || part.len() > 240
                || part.ends_with(['.', ' '])
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                || windows_device(part)
        })
    {
        return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "package path rejected"));
    }
    Ok(())
}

fn validate_package_path(value: &str) -> Result<(), ManagerError> {
    validate_relative_path(value)?;
    let lower = value.to_ascii_lowercase();
    if lower == MANIFEST_NAME
        || lower == INSTALLED_NAME
        || lower == ARCHIVE_NAME
        || lower == TRUST_NAME
        || lower.starts_with(".manager")
        || lower.starts_with(".transaction")
        || lower.starts_with(".staging-")
        || lower.starts_with(".backup-")
        || lower.starts_with(".dispatch-")
        || lower.starts_with(".removed-")
    {
        return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "reserved path rejected"));
    }
    Ok(())
}

fn windows_device(segment: &str) -> bool {
    let folded = segment.split('.').next().unwrap_or(segment).to_ascii_uppercase();
    matches!(folded.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || folded
            .strip_prefix("COM")
            .or_else(|| folded.strip_prefix("LPT"))
            .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}

fn validate_id(id: &str) -> Result<(), ManagerError> {
    if id.is_empty()
        || id.len() > 128
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        || !id.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase())
        || id.ends_with('.')
        || windows_device(id)
    {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "plugin id rejected"));
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<(), ManagerError> {
    if version.is_empty()
        || version.len() > 64
        || !version.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "plugin version rejected"));
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), ManagerError> {
    if hash.len() != 64
        || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "SHA-256 rejected"));
    }
    Ok(())
}

fn valid_key_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn validate_store_name(value: &str) -> Result<(), ManagerError> {
    if value.is_empty() || value.contains(['/', '\\', '\0']) || value == "." || value == ".." {
        return Err(ManagerError::new(
            ManagerErrorCode::PathTraversal,
            "transaction path rejected",
        ));
    }
    Ok(())
}

fn orphan_temporary_name(value: &str) -> bool {
    [".incoming-", ".snapshot-next-"].iter().any(|prefix| {
        value.strip_prefix(prefix).is_some_and(|suffix| {
            let mut pieces = suffix.split('-');
            matches!((pieces.next(), pieces.next(), pieces.next()), (Some(pid), Some(nonce), None)
                if !pid.is_empty()
                    && !nonce.is_empty()
                    && pid.bytes().all(|byte| byte.is_ascii_digit())
                    && nonce.bytes().all(|byte| byte.is_ascii_digit()))
        })
    })
}

fn validate_transaction(transaction: &Transaction) -> Result<(), ManagerError> {
    validate_id(&transaction.id)?;
    for name in [&transaction.staging, &transaction.destination, &transaction.backup] {
        validate_store_name(name)?;
    }
    let valid = transaction.destination == transaction.id
        && match transaction.phase {
            TransactionPhase::Removing => {
                transaction.staging.starts_with(".unused-")
                    && transaction.backup.starts_with(&format!(".removed-{}-", transaction.id))
            }
            _ => {
                transaction.staging.starts_with(".staging-")
                    && transaction.backup.starts_with(&format!(".backup-{}-", transaction.id))
            }
        };
    if !valid {
        return Err(ManagerError::new(
            ManagerErrorCode::PathTraversal,
            "transaction authority rejected",
        ));
    }
    Ok(())
}

fn known_target(value: &str) -> bool {
    matches!(
        value,
        "x86_64-pc-windows-msvc"
            | "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "aarch64-apple-darwin"
    )
}

fn current_target() -> &'static str {
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    return "x86_64-pc-windows-msvc";
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    return "x86_64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    return "aarch64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    return "aarch64-apple-darwin";
    #[allow(unreachable_code)]
    "unsupported"
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_reader(
    reader: &mut impl Read,
    execution: &ExecutionContext,
) -> Result<String, ManagerError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        execution.checkpoint().map_err(map_execution_error)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn copy_and_digest_bounded(
    reader: &mut impl Read,
    output: &mut impl Write,
    maximum: u64,
    execution: &ExecutionContext,
) -> Result<(String, u64), ManagerError> {
    let mut reader = reader.take(maximum.checked_add(1).ok_or_else(|| {
        ManagerError::new(ManagerErrorCode::ResourceLimit, "digest bound overflow")
    })?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    loop {
        execution.checkpoint().map_err(map_execution_error)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::ResourceLimit, "digest size overflow")
        })?;
        if total > maximum {
            return Err(ManagerError::new(
                ManagerErrorCode::InvalidPackage,
                "package grew beyond size limit",
            ));
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

struct TemporaryFile {
    path: Option<PathBuf>,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn keep(&mut self) {
        self.path.take();
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

fn digest_file(path: &Path, execution: &ExecutionContext) -> Result<String, ManagerError> {
    digest_reader(&mut File::open(path)?, execution)
}

fn copy_exact_archive(
    source: &mut impl Read,
    destination: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    execution: &ExecutionContext,
) -> Result<(), ManagerError> {
    let mut output = OpenOptions::new().create_new(true).write(true).open(destination)?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        execution.checkpoint().map_err(map_execution_error)?;
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::ResourceLimit, "archive size overflow")
        })?;
        if total > expected_bytes {
            return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "archive grew"));
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
    }
    if total != expected_bytes || format!("{:x}", hasher.finalize()) != expected_sha256 {
        return Err(ManagerError::new(ManagerErrorCode::HashMismatch, "archive changed"));
    }
    output.sync_all()?;
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn map_execution_error(error: into_markdown_core::ConversionError) -> ManagerError {
    use into_markdown_core::ErrorCode;
    let code = match error.code() {
        ErrorCode::Cancelled => ManagerErrorCode::Cancelled,
        ErrorCode::Timeout => ManagerErrorCode::Timeout,
        ErrorCode::ResourceLimit => ManagerErrorCode::ResourceLimit,
        _ => ManagerErrorCode::Io,
    };
    ManagerError::new(code, error.to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    bytes: u64,
    modified: i128,
}

fn file_identity(file: &File) -> Result<FileIdentity, ManagerError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.metadata()?;
        Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            modified: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
        })
    }
    #[cfg(windows)]
    {
        let information = winx::winapi_util::file::information(file)?;
        Ok(FileIdentity {
            device: information.volume_serial_number(),
            inode: information.file_index(),
            bytes: information.file_size(),
            modified: i128::from(information.last_write_time().unwrap_or_default()),
        })
    }
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>, ManagerError> {
    let mut file = open_package_file(path)?;
    let metadata = file.metadata()?;
    let identity = file_identity(&file)?;
    read_bounded_handle(&mut file, &metadata, identity, maximum)
}

fn read_bounded_handle(
    file: &mut File,
    metadata: &std::fs::Metadata,
    identity: FileIdentity,
    maximum: u64,
) -> Result<Vec<u8>, ManagerError> {
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "file size rejected"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(metadata.len()).map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "file size overflow")
        })?)
        .map_err(|_| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "file allocation rejected")
        })?;
    std::io::Read::by_ref(file)
        .take(maximum.checked_add(1).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::InvalidPackage, "file size bound overflow")
        })?)
        .read_to_end(&mut bytes)?;
    let final_metadata = file.metadata()?;
    if bytes.len() as u64 > maximum
        || bytes.len() as u64 != metadata.len()
        || final_metadata.len() != metadata.len()
        || file_identity(file)? != identity
    {
        return Err(ManagerError::new(
            ManagerErrorCode::InvalidPackage,
            "file changed while reading",
        ));
    }
    Ok(bytes)
}

fn bounded_read_accounted(
    path: &Path,
    maximum: u64,
    execution: &ExecutionContext,
) -> Result<(Vec<u8>, ResourceReservation), ManagerError> {
    let mut file = open_package_file(path)?;
    let metadata = file.metadata()?;
    let identity = file_identity(&file)?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(ManagerError::new(ManagerErrorCode::InvalidPackage, "file size rejected"));
    }
    let reservation = execution
        .reserve_memory(maximum.checked_add(1).ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::ResourceLimit, "file reservation overflow")
        })?)
        .map_err(map_execution_error)?;
    execution.checkpoint().map_err(map_execution_error)?;
    let bytes = read_bounded_handle(&mut file, &metadata, identity, maximum)?;
    Ok((bytes, reservation))
}

fn tree_size(root: &Path, execution: &ExecutionContext) -> Result<u64, ManagerError> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        execution.checkpoint().map_err(map_execution_error)?;
        reject_link(&directory)?;
        for entry in fs::read_dir(directory)? {
            execution.checkpoint().map_err(map_execution_error)?;
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                let metadata = secure_regular_file(&entry.path())?;
                total = total.checked_add(metadata.len()).ok_or_else(|| {
                    ManagerError::new(ManagerErrorCode::ResourceLimit, "tree size overflow")
                })?;
            } else {
                return Err(ManagerError::new(
                    ManagerErrorCode::PathTraversal,
                    "tree entry rejected",
                ));
            }
        }
    }
    Ok(total)
}

fn reject_link(path: &Path) -> Result<(), ManagerError> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    let is_reparse = {
        use std::os::windows::fs::MetadataExt as _;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let is_reparse = false;
    if metadata.file_type().is_symlink() || is_reparse {
        return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "symbolic link rejected"));
    }
    Ok(())
}

fn secure_store_root(path: &Path) -> Result<(), ManagerError> {
    reject_link(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        let metadata = fs::metadata(path)?;
        if metadata.uid() != rustix_uid() || metadata.mode() & 0o077 != 0 {
            return Err(ManagerError::new(ManagerErrorCode::Io, "plugin store is not private"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if fs::metadata(path)?.file_attributes() & 0x400 != 0 {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "plugin store reparse point rejected",
            ));
        }
        verify_windows_private_acl(path)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoreIdentity {
    volume: u64,
    file: u64,
}

fn store_identity(path: &Path) -> Result<StoreIdentity, ManagerError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(ManagerError::new(
            ManagerErrorCode::PathTraversal,
            "plugin store identity rejected",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(StoreIdentity { volume: metadata.dev(), file: metadata.ino() })
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let information = winx::winapi_util::file::information(&directory)?;
        if information.file_attributes() & 0x400 != 0 {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "plugin store reparse point rejected",
            ));
        }
        Ok(StoreIdentity {
            volume: information.volume_serial_number(),
            file: information.file_index(),
        })
    }
}

fn verify_store_identity(path: &Path, expected: StoreIdentity) -> Result<(), ManagerError> {
    reject_link(path)?;
    if fs::canonicalize(path)? != path || store_identity(path)? != expected {
        return Err(ManagerError::new(
            ManagerErrorCode::PathTraversal,
            "plugin store changed after open",
        ));
    }
    secure_store_root(path)
}

#[cfg(all(windows, test))]
fn create_windows_store_path(path: &Path) -> Result<(), ManagerError> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        missing.push(cursor.to_owned());
        cursor = cursor.parent().ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::PathTraversal, "store has no trusted anchor")
        })?;
    }
    reject_link(cursor)?;
    let mut parent = fs::canonicalize(cursor)?;
    for directory in missing.into_iter().rev() {
        let name = directory.file_name().ok_or_else(|| {
            ManagerError::new(ManagerErrorCode::PathTraversal, "store component rejected")
        })?;
        let target = parent.join(name);
        into_markdown_process_plugin::create_windows_plugin_store_directory(&target)
            .map_err(|error| ManagerError::new(ManagerErrorCode::Io, error.to_string()))?;
        reject_link(&target)?;
        let canonical = fs::canonicalize(&target)?;
        if canonical.parent() != Some(parent.as_path()) {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "store component escaped trusted parent",
            ));
        }
        parent = canonical;
    }
    Ok(())
}

#[cfg(windows)]
fn create_windows_store_path_scoped(anchor: &Path, relative: &Path) -> Result<(), ManagerError> {
    let mut parent = anchor.to_owned();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "store path rejected"));
        };
        reject_link(&parent)?;
        let child = parent.join(name);
        if child.exists() {
            reject_link(&child)?;
        } else {
            into_markdown_process_plugin::create_windows_plugin_store_directory(&child)
                .map_err(|error| ManagerError::new(ManagerErrorCode::Io, error.to_string()))?;
        }
        let canonical = fs::canonicalize(&child)?;
        if canonical.parent() != Some(parent.as_path()) {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "store component escaped scope anchor",
            ));
        }
        secure_directory(&canonical)?;
        parent = canonical;
    }
    Ok(())
}

#[cfg(unix)]
fn create_unix_store_path(anchor: &Path, relative: &Path) -> Result<(), ManagerError> {
    let mut descriptor = rustix::fs::open(
        anchor,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "store path rejected"));
        };
        descriptor = match rustix::fs::openat(
            &descriptor,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(opened) => opened,
            Err(rustix::io::Errno::NOENT) => {
                rustix::fs::mkdirat(
                    &descriptor,
                    name,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
                )?;
                rustix::fs::fsync(&descriptor)?;
                rustix::fs::openat(
                    &descriptor,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )?
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = rustix::fs::fstat(&descriptor)?;
        if metadata.st_uid != rustix_uid() || metadata.st_mode & 0o022 != 0 {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "store component permissions rejected",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn verify_windows_private_acl(path: &Path) -> Result<(), ManagerError> {
    into_markdown_process_plugin::verify_windows_plugin_store_path(path)
        .map_err(|error| ManagerError::new(ManagerErrorCode::Io, error.to_string()))
}

#[cfg(windows)]
fn verify_windows_private_child_acl(path: &Path) -> Result<(), ManagerError> {
    into_markdown_process_plugin::verify_windows_plugin_store_child(path)
        .map_err(|error| ManagerError::new(ManagerErrorCode::Io, error.to_string()))
}

#[cfg(unix)]
fn rustix_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn secure_directory(path: &Path) -> Result<(), ManagerError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "directory rejected"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != rustix_uid() || metadata.mode() & 0o022 != 0 {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "directory permissions rejected",
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() & 0o755))?;
    }
    #[cfg(windows)]
    verify_windows_private_child_acl(path)?;
    Ok(())
}

fn secure_scope_anchor(path: &Path) -> Result<(), ManagerError> {
    reject_link(path)?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "scope anchor rejected"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != rustix_uid() || metadata.mode() & 0o022 != 0 {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "scope anchor permissions rejected",
            ));
        }
    }
    #[cfg(windows)]
    verify_windows_private_acl(path)?;
    Ok(())
}

fn secure_regular_file(path: &Path) -> Result<std::fs::Metadata, ManagerError> {
    reject_link(path)?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || file_link_count(path, &metadata)? != 1 {
        return Err(ManagerError::new(ManagerErrorCode::PathTraversal, "file identity rejected"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != rustix_uid() || metadata.mode() & 0o022 != 0 {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "file permissions rejected",
            ));
        }
    }
    #[cfg(windows)]
    verify_windows_private_child_acl(path)?;
    Ok(metadata)
}

fn open_package_file(path: &Path) -> Result<File, ManagerError> {
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;
        let descriptor: OwnedFd = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?;
        let file = File::from(descriptor);
        let metadata = file.metadata()?;
        use std::os::unix::fs::MetadataExt as _;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != rustix_uid()
            || metadata.mode() & 0o022 != 0
        {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "package file identity rejected",
            ));
        }
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let file =
            OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
        let information = winx::winapi_util::file::information(&file)?;
        if information.file_attributes() & 0x400 != 0 || information.number_of_links() != 1 {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "package file identity rejected",
            ));
        }
        Ok(file)
    }
}

#[cfg(unix)]
fn file_link_count(_: &Path, metadata: &std::fs::Metadata) -> Result<u64, ManagerError> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(metadata.nlink())
}

#[cfg(windows)]
fn file_link_count(path: &Path, _: &std::fs::Metadata) -> Result<u64, ManagerError> {
    let file = File::open(path)?;
    Ok(winx::winapi_util::file::information(&file)?.number_of_links())
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    execution: &ExecutionContext,
) -> Result<(), ManagerError> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        execution.checkpoint().map_err(map_execution_error)?;
        let entry = entry?;
        let name = entry.file_name();
        let from = entry.path();
        let to = destination.join(name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&from, &to, execution)?;
        } else if kind.is_file() {
            let _ = secure_regular_file(&from)?;
            let mut input = File::open(&from)?;
            let mut output = OpenOptions::new().create_new(true).write(true).open(&to)?;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                execution.checkpoint().map_err(map_execution_error)?;
                let read = input.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                output.write_all(&buffer[..read])?;
            }
            output.sync_all()?;
            let permissions = fs::metadata(&from)?.permissions();
            fs::set_permissions(&to, permissions)?;
        } else {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "snapshot entry rejected",
            ));
        }
    }
    sync_tree(destination)
}

fn write_atomic_json(path: &Path, value: &impl Serialize) -> Result<(), ManagerError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| {
        ManagerError::new(ManagerErrorCode::InvalidPackage, "transaction serialization failed")
    })?;
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new().create_new(true).write(true).open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn load_trust_authority(path: &Path) -> Result<TrustedSigners, ManagerError> {
    if !path.exists() {
        return Ok(TrustedSigners::default());
    }
    secure_regular_file(path)?;
    let authority: TrustedSigners =
        serde_json::from_slice(&bounded_read(path, MAX_MANIFEST_BYTES)?).map_err(|_| {
            ManagerError::new(ManagerErrorCode::Signature, "global trust authority invalid")
        })?;
    validate_trust_authority(&authority)?;
    Ok(authority)
}

fn recover_atomic_json(path: &Path) -> Result<(), ManagerError> {
    let next = path.with_extension("next");
    let previous = path.with_extension("previous");
    if path.exists() && load_trust_authority(path).is_ok() {
        if next.exists() {
            fs::remove_file(&next)?;
        }
        if previous.exists() {
            fs::remove_file(&previous)?;
        }
    } else if previous.exists() && load_trust_authority(&previous).is_ok() {
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(&previous, path)?;
        if next.exists() {
            fs::remove_file(&next)?;
        }
    } else if next.exists() && load_trust_authority(&next).is_ok() {
        if path.exists() {
            fs::remove_file(path)?;
        }
        rename_atomic_file(&next, path)?;
        if previous.exists() {
            fs::remove_file(&previous)?;
        }
    } else if path.exists() || next.exists() || previous.exists() {
        return Err(ManagerError::new(
            ManagerErrorCode::Signature,
            "global trust recovery authority invalid",
        ));
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn replace_atomic_json(path: &Path, value: &impl Serialize) -> Result<(), ManagerError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| {
        ManagerError::new(ManagerErrorCode::InvalidPackage, "transaction serialization failed")
    })?;
    let temporary = path.with_extension("next");
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    secure_regular_file(&temporary)?;
    atomic_replace(&temporary, path)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn atomic_replace(source: &Path, destination: &Path) -> Result<(), ManagerError> {
    #[cfg(not(windows))]
    rename_atomic_file(source, destination)?;
    #[cfg(windows)]
    {
        if !destination.exists() {
            rename_atomic_file(source, destination)?;
            return Ok(());
        }
        let previous = destination.with_extension("previous");
        if previous.exists() {
            fs::remove_file(&previous)?;
        }
        fs::rename(destination, &previous)?;
        if let Err(error) = rename_atomic_file(source, destination) {
            let _ = fs::rename(&previous, destination);
            return Err(error);
        }
        fs::remove_file(previous)?;
    }
    Ok(())
}

fn rename_atomic_file(source: &Path, destination: &Path) -> Result<(), ManagerError> {
    #[cfg(any(test, feature = "fault-injection"))]
    if let Some(parent) = destination.parent() {
        let injected = parent.join(".test-fail-atomic-rename");
        if injected.is_file() {
            let remaining = fs::read_to_string(&injected)
                .ok()
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(1);
            if remaining <= 1 {
                fs::remove_file(injected)?;
            } else {
                fs::write(&injected, (remaining - 1).to_string())?;
            }
            return Err(ManagerError::new(ManagerErrorCode::Io, "injected atomic rename failure"));
        }
    }
    fs::rename(source, destination)?;
    Ok(())
}

fn sync_tree(path: &Path) -> Result<(), ManagerError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else if entry.file_type()?.is_file() {
            #[cfg(unix)]
            OpenOptions::new().read(true).open(entry.path())?.sync_all()?;
            #[cfg(not(unix))]
            OpenOptions::new().write(true).open(entry.path())?.sync_all()?;
        } else {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "tree sync entry rejected",
            ));
        }
    }
    sync_directory(path)
}

#[allow(clippy::unnecessary_wraps)]
fn sync_directory(path: &Path) -> Result<(), ManagerError> {
    #[cfg(test)]
    {
        let injected = path.join(".test-fail-next-directory-sync");
        if injected.is_file() {
            let remaining = fs::read_to_string(&injected)
                .ok()
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(1);
            if remaining <= 1 {
                fs::remove_file(injected)?;
                return Err(ManagerError::new(
                    ManagerErrorCode::Io,
                    "injected directory sync failure",
                ));
            }
            fs::write(injected, (remaining - 1).to_string())?;
        }
    }
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

struct StoreLock {
    file: File,
}

impl StoreLock {
    fn acquire(root: &Path) -> Result<Self, ManagerError> {
        let path = root.join(LOCK_NAME);
        let file = open_lock_file(&path)?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                ManagerError::new(ManagerErrorCode::Conflict, "plugin store is busy")
            }
            std::fs::TryLockError::Error(_) => {
                ManagerError::new(ManagerErrorCode::Io, "plugin store lock failed")
            }
        })?;
        Ok(Self { file })
    }
}

fn open_lock_file(path: &Path) -> Result<File, ManagerError> {
    let file = match OpenOptions::new().create_new(true).read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            reject_link(path)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt as _;
                const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(path)?;
                let information = winx::winapi_util::file::information(&file)?;
                if information.file_attributes() & 0x400 != 0 || information.number_of_links() != 1
                {
                    return Err(ManagerError::new(
                        ManagerErrorCode::PathTraversal,
                        "plugin lock identity rejected",
                    ));
                }
                file
            }
            #[cfg(unix)]
            {
                use std::os::fd::OwnedFd;
                let descriptor: OwnedFd = rustix::fs::open(
                    path,
                    rustix::fs::OFlags::RDWR
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )?;
                File::from(descriptor)
            }
        }
        Err(error) => return Err(error.into()),
    };
    validate_lock_file(path, &file)?;
    Ok(file)
}

fn validate_lock_file(path: &Path, file: &File) -> Result<(), ManagerError> {
    #[cfg(unix)]
    let _ = path;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != rustix_uid()
            || metadata.mode() & 0o022 != 0
        {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "plugin lock identity rejected",
            ));
        }
    }
    #[cfg(windows)]
    {
        let information = winx::winapi_util::file::information(file)?;
        if !metadata.is_file()
            || information.file_attributes() & 0x400 != 0
            || information.number_of_links() != 1
        {
            return Err(ManagerError::new(
                ManagerErrorCode::PathTraversal,
                "plugin lock identity rejected",
            ));
        }
        verify_windows_private_child_acl(path)?;
    }
    Ok(())
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
#[allow(
    clippy::field_reassign_with_default,
    clippy::manual_let_else,
    clippy::needless_pass_by_value,
    clippy::type_complexity,
    clippy::unreadable_literal
)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use zip::write::SimpleFileOptions;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn write_private_json(path: &Path, value: &impl Serialize) {
        fs::write(path, serde_json::to_vec(value).expect("json")).expect("write json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("permissions");
        }
    }

    fn signed_package(id: &str) -> (Vec<u8>, TrustedSigners) {
        signed_package_with_files(
            id,
            "bin/plugin.exe",
            vec![("bin/plugin.exe", b"fixture".as_slice(), 0o600)],
        )
    }

    fn signed_package_with_files(
        id: &str,
        entrypoint: &str,
        files: Vec<(&str, &[u8], u32)>,
    ) -> (Vec<u8>, TrustedSigners) {
        let key = Ed25519KeyPair::from_seed_unchecked(&[7_u8; 32]).expect("test key");
        let public = key.public_key().as_ref();
        let fingerprint = digest(public);
        let mut manifest = PackageManifest {
            schema_version: 1,
            id: id.to_owned(),
            version: "1.0.0".to_owned(),
            protocol: "process-v1".to_owned(),
            supported_targets: BTreeSet::from([current_target().to_owned()]),
            entrypoints: BTreeMap::from([(current_target().to_owned(), entrypoint.to_owned())]),
            runtime_manifest: None,
            files: files
                .iter()
                .map(|(path, contents, mode)| PackageFile {
                    path: (*path).to_owned(),
                    bytes: contents.len() as u64,
                    sha256: digest(contents),
                    executable: mode & 0o111 != 0,
                })
                .collect(),
            signature: PackageSignature {
                signed_payload_version: 1,
                algorithm: "ed25519".to_owned(),
                key_id: "publisher.test".to_owned(),
                public_key_base64: base64::engine::general_purpose::STANDARD.encode(public),
                public_key_sha256: fingerprint.clone(),
                signed_payload_sha256: String::new(),
                signature_base64: String::new(),
            },
        };
        let payload = serde_json::to_vec(&SignedPayload {
            signature_domain: "into-markdown/plugin-package/v1",
            signed_payload_version: manifest.signature.signed_payload_version,
            algorithm: &manifest.signature.algorithm,
            key_id: &manifest.signature.key_id,
            public_key_sha256: &manifest.signature.public_key_sha256,
            schema_version: manifest.schema_version,
            id: &manifest.id,
            version: &manifest.version,
            protocol: &manifest.protocol,
            supported_targets: &manifest.supported_targets,
            entrypoints: &manifest.entrypoints,
            runtime_manifest: &manifest.runtime_manifest,
            files: &manifest.files,
        })
        .expect("payload");
        manifest.signature.signed_payload_sha256 = digest(&payload);
        manifest.signature.signature_base64 =
            base64::engine::general_purpose::STANDARD.encode(key.sign(&payload).as_ref());
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file(MANIFEST_NAME, options).expect("manifest entry");
        writer
            .write_all(&serde_json::to_vec(&manifest).expect("manifest"))
            .expect("manifest bytes");
        let mut written = BTreeSet::new();
        let mut renamed = Vec::new();
        for &(path, contents, mode) in &files {
            let writer_path = if written.insert(path) {
                path.to_owned()
            } else {
                let mut alternate = path.as_bytes().to_vec();
                let last = alternate.last_mut().expect("non-empty package path");
                *last = if *last == b'x' { b'y' } else { b'x' };
                let alternate = String::from_utf8(alternate).expect("ASCII package path");
                renamed.push((alternate.clone(), path.to_owned()));
                alternate
            };
            if mode & 0o170000 == 0o120000 {
                writer
                    .add_symlink(
                        &writer_path,
                        std::str::from_utf8(contents).expect("symlink target"),
                        options.unix_permissions(mode),
                    )
                    .expect("symlink entry");
            } else {
                writer
                    .start_file(&writer_path, options.unix_permissions(mode))
                    .expect("file entry");
                writer.write_all(contents).expect("file bytes");
            }
        }
        let mut package = writer.finish().expect("zip").into_inner();
        for (from, to) in renamed {
            assert_eq!(from.len(), to.len());
            let mut replacements = 0;
            for offset in 0..=package.len() - from.len() {
                if package[offset..].starts_with(from.as_bytes()) {
                    package[offset..offset + to.len()].copy_from_slice(to.as_bytes());
                    replacements += 1;
                }
            }
            assert_eq!(replacements, 2, "local and central duplicate names");
        }
        for &(path, _, mode) in &files {
            if mode & 0o170000 != 0 && mode & 0o170000 != 0o100000 && mode & 0o170000 != 0o120000 {
                set_zip_external_unix_mode(&mut package, path, mode);
            }
        }
        let trusted = TrustedSigners {
            fingerprints: BTreeMap::from([("publisher.test".to_owned(), fingerprint)]),
            revoked: BTreeSet::new(),
            revoked_fingerprints: BTreeSet::new(),
        };
        (package, trusted)
    }

    fn set_zip_external_unix_mode(package: &mut [u8], path: &str, mode: u32) {
        let mut offset = 0;
        while offset + 46 <= package.len() {
            let Some(relative) =
                package[offset..].windows(4).position(|bytes| bytes == [0x50, 0x4b, 0x01, 0x02])
            else {
                break;
            };
            offset += relative;
            let name_bytes =
                u16::from_le_bytes([package[offset + 28], package[offset + 29]]) as usize;
            let extra_bytes =
                u16::from_le_bytes([package[offset + 30], package[offset + 31]]) as usize;
            let comment_bytes =
                u16::from_le_bytes([package[offset + 32], package[offset + 33]]) as usize;
            let end = offset + 46 + name_bytes + extra_bytes + comment_bytes;
            assert!(end <= package.len(), "central directory bounds");
            if &package[offset + 46..offset + 46 + name_bytes] == path.as_bytes() {
                package[offset + 5] = 3;
                package[offset + 38..offset + 42].copy_from_slice(&(mode << 16).to_le_bytes());
                return;
            }
            offset = end;
        }
        panic!("central directory entry not found: {path}");
    }

    fn signed_wasi_package() -> (Vec<u8>, TrustedSigners) {
        signed_wasi_package_with_capabilities(WasiCapabilities::default())
    }

    fn signed_wasi_package_with_capabilities(
        capabilities: WasiCapabilities,
    ) -> (Vec<u8>, TrustedSigners) {
        const COMPONENT: &[u8] =
            include_bytes!("../../plugin-wasi/tests/fixtures/guest.component.wasm");
        let runtime = WasiPluginManifest {
            id: "fixture".into(),
            protocol: "wasi-v1".into(),
            wasi_preview: "preview2".into(),
            runtime_version: into_markdown_plugin_wasi::WASMTIME_VERSION.into(),
            component_sha256: digest(COMPONENT),
            component_bytes: COMPONENT.len() as u64,
            supported_targets: BTreeSet::from([current_target().into()]),
            capabilities,
            limits: into_markdown_plugin_wasi::WasiLimits {
                fuel: 50_000_000,
                max_linear_memory_bytes: 32 * 1024 * 1024,
                max_output_bytes: 1024 * 1024,
                max_stderr_bytes: 64 * 1024,
                max_resources: 16,
                max_resource_bytes: 1024 * 1024,
            },
        };
        let runtime_bytes = serde_json::to_vec(&runtime).expect("runtime");
        let seed = [11_u8; 32];
        let key = Ed25519KeyPair::from_seed_unchecked(&seed).expect("key");
        let public = key.public_key().as_ref();
        let fingerprint = digest(public);
        let files = vec![
            PackageFile {
                path: "plugin.component.wasm".into(),
                bytes: COMPONENT.len() as u64,
                sha256: digest(COMPONENT),
                executable: false,
            },
            PackageFile {
                path: "runtime.json".into(),
                bytes: runtime_bytes.len() as u64,
                sha256: digest(&runtime_bytes),
                executable: false,
            },
        ];
        let mut manifest = PackageManifest {
            schema_version: 1,
            id: "fixture".into(),
            version: "1.0.0".into(),
            protocol: "wasi-v1".into(),
            supported_targets: BTreeSet::from([current_target().into()]),
            entrypoints: BTreeMap::from([(
                current_target().into(),
                "plugin.component.wasm".into(),
            )]),
            runtime_manifest: Some("runtime.json".into()),
            files,
            signature: PackageSignature {
                signed_payload_version: 1,
                algorithm: "ed25519".into(),
                key_id: "publisher.wasi".into(),
                public_key_base64: base64::engine::general_purpose::STANDARD.encode(public),
                public_key_sha256: fingerprint.clone(),
                signed_payload_sha256: String::new(),
                signature_base64: String::new(),
            },
        };
        let payload = serde_json::to_vec(&SignedPayload {
            signature_domain: "into-markdown/plugin-package/v1",
            signed_payload_version: manifest.signature.signed_payload_version,
            algorithm: &manifest.signature.algorithm,
            key_id: &manifest.signature.key_id,
            public_key_sha256: &manifest.signature.public_key_sha256,
            schema_version: manifest.schema_version,
            id: &manifest.id,
            version: &manifest.version,
            protocol: &manifest.protocol,
            supported_targets: &manifest.supported_targets,
            entrypoints: &manifest.entrypoints,
            runtime_manifest: &manifest.runtime_manifest,
            files: &manifest.files,
        })
        .expect("payload");
        manifest.signature.signed_payload_sha256 = digest(&payload);
        manifest.signature.signature_base64 =
            base64::engine::general_purpose::STANDARD.encode(key.sign(&payload).as_ref());
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o600);
        writer.start_file(MANIFEST_NAME, options).expect("manifest");
        writer
            .write_all(&serde_json::to_vec(&manifest).expect("manifest json"))
            .expect("manifest bytes");
        writer.start_file("plugin.component.wasm", options).expect("component");
        writer.write_all(COMPONENT).expect("component bytes");
        writer.start_file("runtime.json", options).expect("runtime");
        writer.write_all(&runtime_bytes).expect("runtime bytes");
        (
            writer.finish().expect("zip").into_inner(),
            TrustedSigners {
                fingerprints: BTreeMap::from([("publisher.wasi".into(), fingerprint)]),
                revoked: BTreeSet::new(),
                revoked_fingerprints: BTreeSet::new(),
            },
        )
    }

    #[test]
    fn signed_install_verify_and_remove_are_end_to_end() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_package("fixture.plugin");
        let manager = PluginManager::open(temporary.path().join("plugins"), trusted).expect("open");
        let sha256 = digest(&package);
        let installed =
            manager.install_bytes(&package, Some(&sha256), &context()).expect("install");
        assert_eq!(installed.package_sha256, sha256);
        assert_eq!(installed.content_root_sha256.len(), 64);
        assert_eq!(manager.verify("fixture.plugin", &context()).expect("verify"), installed);
        manager.remove("fixture.plugin").expect("remove");
        assert_eq!(
            manager.verify("fixture.plugin", &context()).expect_err("removed").code,
            ManagerErrorCode::NotInstalled
        );
    }

    #[test]
    fn remove_post_move_sync_failure_commits_and_restart_finishes_cleanup() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_package("fixture.plugin");
        let root = temporary.path().join("plugins");
        let manager = PluginManager::open(&root, trusted.clone()).expect("open");
        manager.install_bytes(&package, None, &context()).expect("install");

        // The transaction publication performs the first directory sync; fail
        // the second one immediately after the package directory rename.
        fs::write(root.join(".test-fail-next-directory-sync"), b"2").expect("arm sync fault");
        manager.remove("fixture.plugin").expect("committed remove");
        assert!(!root.join("fixture.plugin").exists(), "removed name is not visible");
        assert!(root.join(TRANSACTION_NAME).is_file(), "cleanup intent retained");
        drop(manager);

        let reopened = PluginManager::open(&root, trusted).expect("restart recovery");
        assert_eq!(
            reopened.verify("fixture.plugin", &context()).expect_err("removed after restart").code,
            ManagerErrorCode::NotInstalled
        );
        assert!(!root.join(TRANSACTION_NAME).exists(), "cleanup journal cleared");
        assert!(
            fs::read_dir(&root)
                .expect("inventory")
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".removed-")),
            "quarantine cleaned"
        );
    }

    #[test]
    fn remove_recovery_replays_a_rolled_back_rename_forward() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_package("fixture.plugin");
        let root = temporary.path().join("plugins");
        let manager = PluginManager::open(&root, trusted.clone()).expect("open");
        manager.install_bytes(&package, None, &context()).expect("install");

        let transaction = Transaction {
            phase: TransactionPhase::Removing,
            id: "fixture.plugin".to_owned(),
            staging: ".unused-1".to_owned(),
            destination: "fixture.plugin".to_owned(),
            backup: ".removed-fixture.plugin-1".to_owned(),
        };
        write_atomic_json(&root.join(TRANSACTION_NAME), &transaction).expect("removing intent");
        drop(manager);

        let reopened = PluginManager::open(&root, trusted).expect("forward recovery");
        assert_eq!(
            reopened.verify("fixture.plugin", &context()).expect_err("removed after replay").code,
            ManagerErrorCode::NotInstalled
        );
        assert!(!root.join(TRANSACTION_NAME).exists(), "journal cleared");
        assert!(!root.join(".removed-fixture.plugin-1").exists(), "quarantine cleared");
    }

    #[test]
    fn retained_archive_closes_package_hash_authority_chain() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_package("fixture.plugin");
        let manager = PluginManager::open(temporary.path().join("plugins"), trusted).expect("open");
        manager.install_bytes(&package, None, &context()).expect("install");
        fs::write(manager.root().join("fixture.plugin").join(ARCHIVE_NAME), b"changed")
            .expect("mutate");
        assert_eq!(
            manager.verify("fixture.plugin", &context()).expect_err("tamper").code,
            ManagerErrorCode::HashMismatch
        );
    }

    #[test]
    fn untrusted_and_revoked_signers_fail_closed() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, _) = signed_package("fixture.plugin");
        let manager =
            PluginManager::open(temporary.path().join("plugins"), TrustedSigners::default())
                .expect("open");
        assert_eq!(
            manager.install_bytes(&package, None, &context()).expect_err("untrusted").code,
            ManagerErrorCode::Signature
        );
    }

    #[cfg(unix)]
    #[test]
    fn scoped_store_rejects_world_writable_existing_intermediate() {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

        let temporary = tempfile::tempdir().expect("temporary directory");
        let anchor = temporary.path().join("anchor");
        fs::DirBuilder::new().mode(0o700).create(&anchor).expect("private anchor");
        let intermediate = anchor.join("existing");
        fs::DirBuilder::new().mode(0o700).create(&intermediate).expect("intermediate");
        fs::set_permissions(&intermediate, fs::Permissions::from_mode(0o777))
            .expect("make intermediate unsafe");

        let error = PluginManager::open_scoped(
            &anchor,
            Path::new("existing/plugins"),
            TrustedSigners::default(),
        )
        .expect_err("world-writable intermediate must fail closed");
        assert_eq!(error.code, ManagerErrorCode::PathTraversal);
        assert!(!intermediate.join("plugins").exists());
    }

    #[cfg(unix)]
    #[test]
    fn stale_manager_rejects_replaced_store_without_touching_trust_sentinel() {
        use std::os::unix::fs::DirBuilderExt as _;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let anchor = temporary.path().join("anchor");
        fs::DirBuilder::new().mode(0o700).create(&anchor).expect("private anchor");
        let mut manager =
            PluginManager::open_scoped(&anchor, Path::new("plugins"), TrustedSigners::default())
                .expect("open scoped store");
        let root = manager.root().to_owned();
        fs::rename(&root, anchor.join("old-plugins")).expect("replace store identity");
        fs::DirBuilder::new().mode(0o700).create(&root).expect("replacement store");
        let sentinel = root.join(TRUST_NAME);
        fs::write(&sentinel, b"replacement trust sentinel").expect("sentinel");

        let error = manager
            .trust_signer("publisher.replacement", &"a".repeat(64))
            .expect_err("stale manager must reject replacement");
        assert_eq!(error.code, ManagerErrorCode::PathTraversal);
        assert_eq!(fs::read(&sentinel).expect("sentinel remains"), b"replacement trust sentinel");
    }

    #[test]
    fn portable_paths_reject_aliases_devices_and_separators() {
        for invalid in [
            "../escape",
            "a//b",
            "a\\b",
            "CON",
            "clock$.txt",
            "name ",
            "name.",
            "a:b",
            "unicodé",
            ".package.zip",
        ] {
            assert!(validate_package_path(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(validate_package_path("assets/image-1.png").is_ok());
    }

    #[test]
    fn signed_manifests_reject_portable_device_plugin_ids_without_store_changes() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let sentinel = temporary.path().join("sentinel");
        fs::write(&sentinel, b"unchanged").expect("sentinel");
        for id in [
            "con",
            "prn.tool",
            "aux",
            "nul",
            "clock$",
            "com1",
            "com9.tool",
            "lpt1",
            "lpt9.tool",
            "trailing.",
        ] {
            let (package, trusted) = signed_package(id);
            let manager = PluginManager::open(
                temporary.path().join(format!("store-{}", id.replace('.', "-"))),
                trusted,
            )
            .expect("manager");
            assert_eq!(
                manager.install_bytes(&package, None, &context()).expect_err("portable id").code,
                ManagerErrorCode::InvalidPackage,
                "accepted {id:?}"
            );
            assert!(!manager.root().join(id).exists());
            assert_eq!(fs::read(&sentinel).expect("sentinel"), b"unchanged");
        }
    }

    #[test]
    fn signed_malicious_archives_fail_closed_through_both_install_paths() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let sentinel = temporary.path().join("outside-sentinel");
        fs::write(&sentinel, b"unchanged").expect("sentinel");
        let cases: Vec<(&str, ManagerErrorCode, Vec<(&str, &[u8], u32)>)> = vec![
            (
                "traversal",
                ManagerErrorCode::PathTraversal,
                vec![("../outside-sentinel", b"bad", 0o600)],
            ),
            ("absolute", ManagerErrorCode::PathTraversal, vec![("/absolute", b"bad", 0o600)]),
            ("drive", ManagerErrorCode::PathTraversal, vec![("C:/escape", b"bad", 0o600)]),
            ("unc", ManagerErrorCode::PathTraversal, vec![("//server/share", b"bad", 0o600)]),
            ("backslash", ManagerErrorCode::PathTraversal, vec![("dir\\escape", b"bad", 0o600)]),
            ("dot", ManagerErrorCode::PathTraversal, vec![("dir/./escape", b"bad", 0o600)]),
            (
                "case-alias",
                ManagerErrorCode::InvalidPackage,
                vec![("bin/plugin.exe", b"one", 0o600), ("BIN/PLUGIN.EXE", b"two", 0o600)],
            ),
            ("reserved", ManagerErrorCode::PathTraversal, vec![("assets/CON.txt", b"bad", 0o600)]),
            (
                "symlink",
                ManagerErrorCode::PathTraversal,
                vec![("bin/plugin.exe", b"../outside-sentinel", 0o120777)],
            ),
            (
                "special",
                ManagerErrorCode::PathTraversal,
                vec![("bin/plugin.exe", b"bad", 0o020600)],
            ),
            (
                "duplicate",
                ManagerErrorCode::InvalidPackage,
                vec![("bin/plugin.exe", b"one", 0o600), ("bin/plugin.exe", b"two", 0o600)],
            ),
        ];
        for (case, expected, files) in cases {
            let entrypoint = files[0].0;
            let (package, trusted) =
                signed_package_with_files("malicious.plugin", entrypoint, files);
            if matches!(case, "symlink" | "special") {
                let mut archive = ZipArchive::new(Cursor::new(&package)).expect("inspect fixture");
                let entry = archive.by_name(entrypoint).expect("malicious entry");
                if case == "symlink" {
                    assert!(entry.is_symlink(), "fixture must be a real ZIP symlink");
                } else {
                    assert_eq!(entry.unix_mode().expect("unix mode") & 0o170000, 0o020000);
                }
            }
            for method in ["bytes", "file"] {
                let root = temporary.path().join(format!("{case}-{method}"));
                let manager = PluginManager::open(&root, trusted.clone()).expect("open manager");
                let error = if method == "bytes" {
                    manager.install_bytes(&package, None, &context()).expect_err(case)
                } else {
                    let source = temporary.path().join(format!("{case}.zip"));
                    fs::write(&source, &package).expect("write malicious package");
                    manager.install_file(&source, None, &context()).expect_err(case)
                };
                assert_eq!(error.code, expected, "{case}/{method}");
                assert!(!root.join("malicious.plugin").exists(), "{case}/{method}");
                for entry in fs::read_dir(&root).expect("read store") {
                    let name = entry.expect("entry").file_name().to_string_lossy().into_owned();
                    assert!(
                        !name.starts_with(".staging-")
                            && !name.starts_with(".incoming-")
                            && !name.starts_with(".backup-")
                            && !name.starts_with(".remove-")
                            && !name.starts_with(".removed-")
                            && name != TRANSACTION_NAME,
                        "left transaction artifact {name:?} for {case}/{method}"
                    );
                }
                assert_eq!(fs::read(&sentinel).expect("sentinel"), b"unchanged");
            }
        }
    }

    #[test]
    fn wrong_package_hash_is_rejected_before_install() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_package("fixture.plugin");
        let manager = PluginManager::open(temporary.path().join("plugins"), trusted).expect("open");
        assert_eq!(
            manager
                .install_bytes(&package, Some(&"0".repeat(64)), &context())
                .expect_err("hash")
                .code,
            ManagerErrorCode::HashMismatch
        );
        assert!(!manager.root().join("fixture.plugin").exists());
    }

    #[test]
    fn file_install_does_not_materialize_package() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_package("fixture.plugin");
        let package_path = temporary.path().join("fixture.zip");
        fs::write(&package_path, &package).expect("package file");
        let manager = PluginManager::open(temporary.path().join("plugins"), trusted).expect("open");
        manager
            .install_file(&package_path, Some(&digest(&package)), &context())
            .expect("stream install");
        assert!(manager.verify("fixture.plugin", &context()).is_ok());
    }

    #[test]
    fn snapshot_is_published_only_after_complete_copy_and_cleans_cancelled_next_file() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_package("fixture.plugin");
        let root = temporary.path().join("plugins");
        let manager = PluginManager::open(&root, trusted.clone()).expect("open");
        manager.install_bytes(&package, None, &context()).expect("install");
        let destination = manager.root().join(".cli-backup-fixture.zip");

        let token = into_markdown_core::CancellationToken::new();
        token.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let error = match manager.snapshot_package("fixture.plugin", &destination, &cancelled) {
            Ok(_) => panic!("cancelled snapshot unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.code, ManagerErrorCode::Cancelled);
        assert!(!destination.exists(), "partial backup was published");
        assert!(
            fs::read_dir(&root)
                .expect("inventory")
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".snapshot-next-")),
            "partial next file was retained"
        );
        assert_eq!(cancelled.reserved_temporary_bytes(), 0);

        let snapshot = manager
            .snapshot_package("fixture.plugin", &destination, &context())
            .expect("complete snapshot");
        assert_eq!(digest(&fs::read(snapshot.path()).expect("snapshot bytes")), digest(&package));
        drop(snapshot);
        assert!(!destination.exists(), "unpersisted snapshot was not cleaned");

        let orphan = manager.root().join(".snapshot-next-123-456");
        let unrelated = manager.root().join(".snapshot-next-123-456.extra");
        fs::write(&orphan, b"partial").expect("orphan");
        fs::write(&unrelated, b"sentinel").expect("unrelated");
        drop(manager);
        PluginManager::open(&root, trusted).expect("recover orphan");
        assert!(!orphan.exists(), "strictly named orphan survived recovery");
        assert_eq!(fs::read(unrelated).expect("unrelated sentinel"), b"sentinel");
    }

    #[test]
    fn signer_alias_and_fingerprint_revocation_fail_closed() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, mut trusted) = signed_package("fixture.plugin");
        let fingerprint = trusted.fingerprints["publisher.test"].clone();
        trusted.fingerprints.insert("publisher.alias".to_owned(), fingerprint.clone());
        assert_eq!(
            PluginManager::open(temporary.path().join("alias"), trusted).expect_err("alias").code,
            ManagerErrorCode::Signature
        );

        let (_, mut revoked) = signed_package("fixture.plugin");
        revoked.revoked_fingerprints.insert(fingerprint);
        let manager = PluginManager::open(temporary.path().join("revoked"), revoked).expect("open");
        assert_eq!(
            manager.install_bytes(&package, None, &context()).expect_err("revoked").code,
            ManagerErrorCode::Signature
        );
    }

    #[test]
    fn cancellation_and_low_memory_are_stable_and_release_reservations() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_package("fixture.plugin");
        let manager =
            PluginManager::open(temporary.path().join("cancel"), trusted.clone()).expect("open");
        let token = into_markdown_core::CancellationToken::new();
        token.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        assert_eq!(
            manager.install_bytes(&package, None, &cancelled).expect_err("cancel").code,
            ManagerErrorCode::Cancelled
        );
        assert_eq!(cancelled.reserved_memory_bytes(), 0);
        assert_eq!(cancelled.reserved_temporary_bytes(), 0);

        let mut limits = ResourceLimits::default();
        limits.max_memory_bytes = MAX_MANIFEST_BYTES * 8 - 1;
        let limited = ExecutionContext::new(ExecutionOptions::default(), limits);
        assert_eq!(
            manager.install_bytes(&package, None, &limited).expect_err("memory").code,
            ManagerErrorCode::ResourceLimit
        );
        assert_eq!(limited.reserved_memory_bytes(), 0);
        assert_eq!(limited.reserved_temporary_bytes(), 0);
    }

    #[test]
    fn persisted_trust_merges_stale_managers_and_recovers_replace_phases() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("plugins");
        let mut first = PluginManager::open_persisted(&root).expect("first open");
        let mut stale = PluginManager::open_persisted(&root).expect("stale open");
        first.trust_signer("publisher.one", &"1".repeat(64)).expect("first signer");
        stale.trust_signer("publisher.two", &"2".repeat(64)).expect("merged signer");
        let reopened = PluginManager::open_persisted(&root).expect("reopen");
        assert_eq!(reopened.trusted_signers().fingerprints.len(), 2);

        let trust = root.join(TRUST_NAME);
        let next = trust.with_extension("next");
        let previous = trust.with_extension("previous");
        let mut interrupted = reopened.trusted_signers();
        interrupted.fingerprints.insert("publisher.three".to_owned(), "3".repeat(64));
        write_private_json(&next, &interrupted);
        let recovered = PluginManager::open_persisted(&root).expect("recover before replace");
        assert_eq!(recovered.trusted_signers().fingerprints.len(), 2);
        assert!(!next.exists());

        fs::rename(&trust, &previous).expect("old moved");
        let recovered = PluginManager::open_persisted(&root).expect("recover old move");
        assert_eq!(recovered.trusted_signers().fingerprints.len(), 2);
        assert!(trust.exists());
        assert!(!previous.exists());

        fs::remove_file(&trust).expect("remove authority");
        write_private_json(&next, &interrupted);
        let recovered = PluginManager::open_persisted(&root).expect("recover new publish");
        assert_eq!(recovered.trusted_signers().fingerprints.len(), 3);
        assert!(trust.exists());
        assert!(!next.exists());
    }

    #[test]
    fn first_trust_double_rename_failure_is_indeterminate_and_recovers_forward() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("plugins");
        let mut manager = PluginManager::open_persisted(&root).expect("open");
        fs::write(manager.root().join(".test-fail-atomic-rename"), b"2")
            .expect("arm publish and recovery rename failures");

        assert_eq!(
            manager
                .trust_signer("publisher.pending", &"4".repeat(64))
                .expect_err("pending publication")
                .code,
            ManagerErrorCode::Indeterminate
        );
        assert!(!manager.root().join(TRUST_NAME).exists(), "trust not yet visible");
        assert!(manager.root().join(TRUST_NAME).with_extension("next").is_file());
        drop(manager);

        let recovered = PluginManager::open_persisted(&root).expect("restart recovery");
        assert_eq!(
            recovered.trusted_signers().fingerprints.get("publisher.pending").map(String::as_str),
            Some("4444444444444444444444444444444444444444444444444444444444444444")
        );
        assert!(!recovered.root().join(TRUST_NAME).with_extension("next").exists());
    }

    #[test]
    fn zip_metadata_is_bounded_before_archive_parser_allocation() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let path = temporary.path().join("many.zip");
        let file = File::create(&path).expect("create zip");
        let mut writer = zip::ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for index in 0..(MAX_FILES + 2) {
            writer.start_file(format!("f{index}"), options).expect("entry");
        }
        writer.finish().expect("finish zip");
        let mut many = File::open(&path).expect("open zip");
        let many_bytes = many.metadata().expect("metadata").len();
        assert_eq!(
            preflight_zip(&mut many, many_bytes, Some(&context())).expect_err("entry count").code,
            ManagerErrorCode::InvalidPackage
        );
        let manager =
            PluginManager::open(temporary.path().join("plugins"), TrustedSigners::default())
                .expect("manager");
        assert_eq!(
            manager.install_file(&path, None, &context()).expect_err("entry count").code,
            ManagerErrorCode::InvalidPackage
        );

        let (package, trusted) = signed_package("fixture.plugin");
        let path = temporary.path().join("bounded.zip");
        fs::write(&path, package).expect("package");
        let mut limits = ResourceLimits::default();
        limits.max_memory_bytes = fs::metadata(&path).expect("metadata").len() - 1;
        let limited = ExecutionContext::new(ExecutionOptions::default(), limits);
        let manager = PluginManager::open(temporary.path().join("bounded"), trusted).expect("open");
        assert_eq!(
            manager.install_file(&path, None, &limited).expect_err("preflight memory").code,
            ManagerErrorCode::ResourceLimit
        );
        assert_eq!(limited.reserved_memory_bytes(), 0);

        let (valid, _) = signed_package("fixture.plugin");
        let eocd =
            valid.windows(4).rposition(|bytes| bytes == [0x50, 0x4b, 0x05, 0x06]).expect("eocd");
        for mutation in ["multidisk", "zip64", "central"] {
            let mut changed = valid.clone();
            match mutation {
                "multidisk" => changed[eocd + 4..eocd + 6].copy_from_slice(&1_u16.to_le_bytes()),
                "zip64" => changed[eocd + 10..eocd + 12].copy_from_slice(&u16::MAX.to_le_bytes()),
                "central" => changed[eocd + 12..eocd + 16]
                    .copy_from_slice(&(9_u32 * 1024 * 1024).to_le_bytes()),
                _ => unreachable!(),
            }
            let changed_len = changed.len() as u64;
            assert_eq!(
                preflight_zip(&mut Cursor::new(changed), changed_len, None)
                    .expect_err(mutation)
                    .code,
                ManagerErrorCode::InvalidPackage
            );
        }

        let mut fake_comment = valid.clone();
        fake_comment[eocd + 20..eocd + 22].copy_from_slice(&8_u16.to_le_bytes());
        fake_comment.extend_from_slice(b"xxPK\x05\x06xx");
        let fake_len = fake_comment.len() as u64;
        assert_eq!(
            preflight_zip(&mut Cursor::new(fake_comment), fake_len, None)
                .expect_err("EOCD signature in comment")
                .code,
            ManagerErrorCode::InvalidPackage
        );
    }

    #[test]
    fn manager_rejects_scope_root_replacement_after_open() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("plugins");
        let displaced = temporary.path().join("displaced");
        let manager = PluginManager::open(&root, TrustedSigners::default()).expect("manager");
        fs::rename(&root, &displaced).expect("replace root");
        let _replacement =
            PluginManager::open(&root, TrustedSigners::default()).expect("replacement");
        fs::write(displaced.join("sentinel"), b"unchanged").expect("sentinel");
        assert_eq!(
            manager.verify("missing", &context()).expect_err("identity").code,
            ManagerErrorCode::PathTraversal
        );
        assert_eq!(fs::read(displaced.join("sentinel")).expect("sentinel"), b"unchanged");
    }

    #[test]
    fn signed_install_prepare_and_execute_real_wasi_component() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let (package, trusted) = signed_wasi_package();
        let manager = PluginManager::open(temporary.path().join("plugins"), trusted).expect("open");
        manager.install_bytes(&package, Some(&digest(&package)), &context()).expect("install");
        let execution = context();
        let prepared = manager
            .prepare_wasi("fixture", &WasiCapabilities::default(), &execution)
            .expect("prepare");
        let output = prepared
            .execute(
                &into_markdown_plugin_wasi::PluginRequest {
                    protocol_version: into_markdown_plugin_wasi::PROTOCOL_VERSION,
                    source_name: "valid-resource".into(),
                    input: b"fixture".to_vec(),
                },
                &execution,
            )
            .expect("execute");
        assert_eq!(output.resources.len(), 1);
        assert_eq!(output.resources[0].bytes, b"abc");
        assert_eq!(output.document.blocks.len(), 1);
        drop(prepared);
        assert_eq!(execution.reserved_memory_bytes(), 0);
        assert_eq!(execution.reserved_temporary_bytes(), 0);
    }

    #[test]
    fn manager_intersects_declared_and_invocation_wasi_network_authority() {
        use std::net::{IpAddr, Ipv4Addr, TcpListener};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        let declared = into_markdown_plugin_wasi::NetworkGrant {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            allow_private: true,
        };
        let (package, trusted) = signed_wasi_package_with_capabilities(WasiCapabilities {
            network: vec![declared.clone()],
            ..WasiCapabilities::default()
        });
        let temporary = tempfile::tempdir().expect("tempdir");
        let manager = PluginManager::open(temporary.path().join("plugins"), trusted).expect("open");
        manager.install_bytes(&package, Some(&digest(&package)), &context()).expect("install");

        for invocation in [
            WasiCapabilities::default(),
            WasiCapabilities {
                network: vec![into_markdown_plugin_wasi::NetworkGrant {
                    allow_private: false,
                    ..declared.clone()
                }],
                ..WasiCapabilities::default()
            },
            WasiCapabilities {
                network: vec![into_markdown_plugin_wasi::NetworkGrant {
                    port: port.wrapping_add(1).max(1),
                    ..declared.clone()
                }],
                ..WasiCapabilities::default()
            },
            WasiCapabilities {
                network: vec![into_markdown_plugin_wasi::NetworkGrant {
                    address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
                    ..declared.clone()
                }],
                ..WasiCapabilities::default()
            },
        ] {
            let error = match manager.prepare_wasi("fixture", &invocation, &context()) {
                Ok(_) => panic!("authority must be rejected before execution"),
                Err(error) => error,
            };
            assert_eq!(error.code, ManagerErrorCode::InvalidPackage);
            assert!(listener.accept().is_err(), "rejected authority reached network");
        }

        let execution = context();
        let prepared = manager
            .prepare_wasi(
                "fixture",
                &WasiCapabilities { network: vec![declared], ..WasiCapabilities::default() },
                &execution,
            )
            .expect("exact authority subset");
        prepared
            .execute(
                &into_markdown_plugin_wasi::PluginRequest {
                    protocol_version: into_markdown_plugin_wasi::PROTOCOL_VERSION,
                    source_name: format!("network-call:{port}"),
                    input: b"fixture".to_vec(),
                },
                &execution,
            )
            .expect("execute exact endpoint");
        listener.accept().expect("exact endpoint reached");
    }

    #[test]
    fn bounded_zip_entry_probes_without_unaccounted_growth() {
        let execution =
            ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let mut exact = Cursor::new(b"abcd".to_vec());
        assert_eq!(read_zip_entry_bounded(&mut exact, 4, 4, Some(&execution)).unwrap(), b"abcd");
        assert_eq!(execution.reserved_memory_bytes(), 0);

        let mut oversized = Cursor::new(b"abcde".to_vec());
        let error = read_zip_entry_bounded(&mut oversized, 4, 4, Some(&execution)).unwrap_err();
        assert_eq!(error.code, ManagerErrorCode::InvalidPackage);
        assert_eq!(execution.reserved_memory_bytes(), 0);
    }
}
