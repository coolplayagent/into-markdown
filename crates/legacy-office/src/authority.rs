use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use object::{Architecture, BinaryFormat, Object as _};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

mod container;
mod dependencies;
mod paths;
mod schema;
use paths::{
    checked_join, explicit_directory, explicit_regular_file, is_reparse, safe_relative,
    system_library_path,
};
#[cfg(windows)]
pub(crate) use schema::AppContainerAuthority;
#[cfg(all(test, not(windows)))]
use schema::AppContainerAuthority;
use schema::{Abi, Authority, FileRole, RuntimeFile, SystemLibraryAuthority, Target};
#[cfg(test)]
use schema::{RuntimeLicense, SandboxAuthority, WorkerLimits};

const MAX_AUTHORITY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RUNTIME_FILES: usize = 100_000;
const MAX_LICENSES: usize = 4_096;
const MAX_PATH_BYTES: usize = 1_024;
const MAX_RUNTIME_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 16 * 1024;

/// Paths to an explicit, package-owned runtime authority and worker.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    authority_path: PathBuf,
    bundle_root: PathBuf,
    worker_executable: PathBuf,
}

impl RuntimeConfig {
    /// Construct an explicit configuration without performing I/O.
    #[must_use]
    pub fn new(
        authority_path: impl Into<PathBuf>,
        bundle_root: impl Into<PathBuf>,
        worker_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            authority_path: authority_path.into(),
            bundle_root: bundle_root.into(),
            worker_executable: worker_executable.into(),
        }
    }

    /// Authority JSON path.
    #[must_use]
    pub fn authority_path(&self) -> &Path {
        &self.authority_path
    }
}

/// Exact compatibility runtime recorded on converted output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIdentity {
    version: String,
    artifact_sha256: String,
    target: String,
}

impl RuntimeIdentity {
    /// Fixed `LibreOffice` version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// SHA-256 of the exact platform artifact.
    #[must_use]
    pub fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }

    /// Audited Rust target triple.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }
}

pub(crate) struct VerifiedBundle {
    pub root: PathBuf,
    pub worker: PathBuf,
    pub worker_sha256: String,
    pub runtime_files: Vec<VerifiedRuntimeFile>,
    pub runtime_snapshot_bytes: u64,
    pub install_root: PathBuf,
    pub kit_library: PathBuf,
    pub kit_sha256: String,
    pub authority_sha256: String,
    pub identity: RuntimeIdentity,
    pub address_space_overhead: u64,
    pub file_size_limit: u64,
    pub open_file_limit: u32,
    pub process_limit: u32,
    pub system_read_paths: Vec<PathBuf>,
    pub container: Option<VerifiedContainer>,
    #[cfg(windows)]
    pub app_container: AppContainerAuthority,
    pub _memory: ResourceReservation,
}

#[derive(Clone)]
pub(crate) struct VerifiedContainer {
    pub image_relative: String,
    pub mount_relative: String,
    pub kit_sha256: String,
}

#[derive(Clone)]
pub(crate) struct VerifiedRuntimeFile {
    pub relative: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub executable: bool,
}

pub(crate) fn verify(
    config: &RuntimeConfig,
    context: &ExecutionContext,
) -> Result<VerifiedBundle, ConversionError> {
    context.checkpoint()?;
    let authority_path = explicit_regular_file(&config.authority_path, None)?;
    let metadata = authority_path.metadata().map_err(|_| unavailable("authorityIo"))?;
    if metadata.len() == 0 || metadata.len() > MAX_AUTHORITY_BYTES {
        return Err(unavailable("authoritySize"));
    }
    let planned = metadata.len().checked_mul(16).ok_or_else(|| unavailable("authoritySize"))?;
    let memory = context.reserve_memory(planned)?;
    let bytes = fs::read(&authority_path).map_err(|_| unavailable("authorityIo"))?;
    let authority: Authority =
        serde_json::from_slice(&bytes).map_err(|_| unavailable("authoritySchema"))?;
    let authority_sha256 = format!("{:x}", Sha256::digest(&bytes));
    let target_name = current_target().ok_or_else(|| unavailable("unsupportedTarget"))?;
    validate_authority(&authority, target_name)?;
    let target = authority.targets.get(target_name).ok_or_else(|| unavailable("targetMissing"))?;
    #[cfg(windows)]
    verify_platform_sandbox(target)?;
    let root = explicit_directory(&config.bundle_root)?;
    if authority_path != root.join("authority.json") {
        return Err(unavailable("authorityPath"));
    }
    let worker = explicit_regular_file(&config.worker_executable, Some(&root))?;
    let expected_worker = checked_join(&root, &target.worker)?;
    if worker != expected_worker {
        return Err(unavailable("workerAuthority"));
    }
    validate_target(target, target_name)?;
    let mut inventory = validate_files(target, &root, &authority_path, context)?;
    let kit_library = checked_join(&root, &target.kit_library)?;
    let install_root = checked_join(&root, &target.install_root)?;
    let container = container::verified(target)?;
    if container.is_none() {
        validate_abi(&kit_library, &target.abi, context)?;
        dependencies::validate(target, target_name, &root, context)?;
        explicit_directory(&install_root).map_err(|_| unavailable("installRoot"))?;
    }
    let system_read_paths = target
        .sandbox
        .system_libraries
        .iter()
        .map(|library| system_library_path(library, target_name))
        .collect::<Result<Vec<_>, _>>()?;
    let authority_bytes = u64::try_from(bytes.len()).map_err(|_| unavailable("authoritySize"))?;
    inventory.files.try_reserve(1).map_err(|_| unavailable("fileInventory"))?;
    inventory.files.push(VerifiedRuntimeFile {
        relative: "authority.json".into(),
        path: authority_path,
        bytes: authority_bytes,
        sha256: authority_sha256.clone(),
        executable: false,
    });
    let runtime_snapshot_bytes = inventory
        .total_bytes
        .checked_add(authority_bytes)
        .ok_or_else(|| unavailable("fileInventory"))?;
    Ok(VerifiedBundle {
        root,
        worker,
        worker_sha256: inventory.worker_sha256,
        runtime_files: inventory.files,
        runtime_snapshot_bytes,
        install_root,
        kit_library,
        kit_sha256: inventory.kit_sha256,
        authority_sha256,
        identity: RuntimeIdentity {
            version: authority.version,
            artifact_sha256: target.artifact_sha256.clone(),
            target: target_name.into(),
        },
        address_space_overhead: target.limits.address_space_overhead_bytes,
        file_size_limit: target.limits.file_size_limit_bytes,
        open_file_limit: target.limits.open_file_limit,
        process_limit: target.limits.process_limit,
        system_read_paths,
        container,
        #[cfg(windows)]
        app_container: target
            .sandbox
            .app_container
            .clone()
            .ok_or_else(|| unavailable("appContainerAuthority"))?,
        _memory: memory,
    })
}

struct ValidatedInventory {
    kit_sha256: String,
    worker_sha256: String,
    files: Vec<VerifiedRuntimeFile>,
    total_bytes: u64,
}

fn validate_files(
    target: &Target,
    root: &Path,
    authority_path: &Path,
    context: &ExecutionContext,
) -> Result<ValidatedInventory, ConversionError> {
    let mut paths = BTreeSet::new();
    let mut total = 0_u64;
    let mut worker_roles = 0_usize;
    let mut kit_roles = 0_usize;
    let mut license_files = BTreeSet::new();
    let mut files = Vec::new();
    files.try_reserve_exact(target.files.len()).map_err(|_| unavailable("fileInventory"))?;
    for entry in &target.files {
        context.checkpoint()?;
        if !paths.insert(entry.path.as_str()) || !is_sha256(&entry.sha256) {
            return Err(unavailable("fileInventory"));
        }
        total = total.checked_add(entry.bytes).ok_or_else(|| unavailable("fileInventory"))?;
        if total > MAX_RUNTIME_BYTES {
            return Err(unavailable("fileInventory"));
        }
        let path = checked_join(root, &entry.path)?;
        let actual = explicit_regular_file(&path, Some(root))?;
        if actual != path || !file_matches(&actual, entry, context)? {
            return Err(unavailable("runtimeHash"));
        }
        files.push(VerifiedRuntimeFile {
            relative: entry.path.clone(),
            path: actual,
            bytes: entry.bytes,
            sha256: entry.sha256.clone(),
            executable: entry.path == target.worker,
        });
        match entry.role {
            FileRole::Worker => {
                worker_roles += 1;
                if entry.path != target.worker {
                    return Err(unavailable("fileInventory"));
                }
            }
            FileRole::KitLibrary => {
                kit_roles += 1;
                if entry.path != target.kit_library {
                    return Err(unavailable("fileInventory"));
                }
            }
            FileRole::License => {
                license_files.insert((entry.path.as_str(), entry.sha256.as_str()));
            }
            FileRole::Runtime | FileRole::Configuration => {}
        }
    }
    let container = target.container.is_some();
    if worker_roles != 1
        || (!container && kit_roles != 1)
        || (container && kit_roles != 0)
        || total == 0
    {
        return Err(unavailable("fileInventory"));
    }
    if target.container.is_none() {
        validate_inventory_complete(root, authority_path, &paths, context)?;
    }
    let mut license_ids = BTreeSet::new();
    let mut license_paths = BTreeSet::new();
    for license in &target.licenses {
        if license.id.is_empty()
            || license.id.len() > 128
            || license.spdx.as_ref().is_some_and(|value| value.is_empty() || value.len() > 128)
            || !license_files
                .contains(&(license.notice_path.as_str(), license.notice_sha256.as_str()))
            || !license_ids.insert(license.id.as_str())
            || !license_paths.insert(license.notice_path.as_str())
        {
            return Err(unavailable("licenseInventory"));
        }
    }
    if license_paths.len() != license_files.len() {
        return Err(unavailable("licenseInventory"));
    }
    let kit_hash = if let Some(container) = &target.container {
        container.kit_sha256.clone()
    } else {
        target
            .files
            .iter()
            .find(|entry| entry.path == target.kit_library)
            .ok_or_else(|| unavailable("fileInventory"))?
            .sha256
            .clone()
    };
    let worker = target
        .files
        .iter()
        .find(|entry| entry.path == target.worker)
        .ok_or_else(|| unavailable("fileInventory"))?;
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(ValidatedInventory {
        kit_sha256: kit_hash,
        worker_sha256: worker.sha256.clone(),
        files,
        total_bytes: total,
    })
}

fn validate_authority(authority: &Authority, target: &str) -> Result<(), ConversionError> {
    if authority.schema_version != 1
        || authority.product != "LibreOffice"
        || authority.version.is_empty()
        || authority.version.len() > 64
        || !https_url(&authority.source_url)
        || authority.targets.is_empty()
        || authority.targets.len() > 4
        || !authority.targets.contains_key(target)
        || !authority.targets.keys().all(|name| SUPPORTED_TARGETS.contains(&name.as_str()))
    {
        return Err(unavailable("authoritySchema"));
    }
    for (name, candidate) in &authority.targets {
        validate_target(candidate, name)?;
    }
    Ok(())
}

fn validate_target(target: &Target, target_name: &str) -> Result<(), ConversionError> {
    let unique_system_identities = target
        .sandbox
        .system_libraries
        .iter()
        .map(|library| library.identity.as_str())
        .collect::<BTreeSet<_>>();
    let unique_system_paths = target
        .sandbox
        .system_libraries
        .iter()
        .map(|library| library.path.as_str())
        .collect::<BTreeSet<_>>();
    if !https_url(&target.artifact_url)
        || target.artifact_bytes == 0
        || !is_sha256(&target.artifact_sha256)
        || target.files.is_empty()
        || target.files.len() > MAX_RUNTIME_FILES
        || target.licenses.is_empty()
        || target.licenses.len() > MAX_LICENSES
        || !safe_relative(&target.install_root)
        || !safe_relative(&target.kit_library)
        || !Path::new(&target.kit_library).starts_with(Path::new(&target.install_root))
        || !safe_relative(&target.worker)
        || target.limits.address_space_overhead_bytes < 256 * 1024 * 1024
        || target.limits.file_size_limit_bytes == 0
        || !(16..=4_096).contains(&target.limits.open_file_limit)
        || !valid_process_authority(target, target_name)
        || !valid_app_container_authority(target, target_name)
        || target.sandbox.system_libraries.len() > 128
        || target.sandbox.system_libraries.iter().collect::<BTreeSet<_>>().len()
            != target.sandbox.system_libraries.len()
        || unique_system_identities.len() != target.sandbox.system_libraries.len()
        || unique_system_paths.len() != target.sandbox.system_libraries.len()
        || target.abi.library_identity.is_empty()
        || target.abi.library_identity.len() > MAX_PATH_BYTES
        || target.abi.required_export != "libreofficekit_hook_2"
        || !abi_matches_target(&target.abi, target_name)
        || !container_authority_is_valid(target, target_name)
    {
        return Err(unavailable("targetAuthority"));
    }
    Ok(())
}

fn valid_process_authority(target: &Target, target_name: &str) -> bool {
    if target_name == "aarch64-apple-darwin" && target.container.is_some() {
        let Some(child) = &target.sandbox.compatibility_child else {
            return false;
        };
        return target.limits.process_limit == 2
            && target.sandbox.network == "denyIp"
            && target.sandbox.child_processes == "exactCompatibilityChild"
            && child.maximum_instances == 1
            && child.local_ip == "deny"
            && child.local_ipc == "uidBoundTemporaryUnixSocketOnly"
            && child.executable == "container/LibreOffice.app/Contents/MacOS/soffice";
    }
    target.limits.process_limit == 1
        && target.sandbox.network == "deny"
        && target.sandbox.child_processes == "deny"
        && target.sandbox.compatibility_child.is_none()
}

fn container_authority_is_valid(target: &Target, target_name: &str) -> bool {
    let Some(container) = &target.container else {
        return true;
    };
    target_name == "aarch64-apple-darwin"
        && container.format == "udif"
        && container.image_bytes == target.artifact_bytes
        && container.image_sha256 == target.artifact_sha256
        && is_sha256(&container.image_sha256)
        && is_sha256(&container.kit_sha256)
        && safe_relative(&container.image_path)
        && safe_relative(&container.mount_path)
        && Path::new(&target.install_root).starts_with(&container.mount_path)
        && Path::new(&target.kit_library).starts_with(&container.mount_path)
        && target.files.iter().any(|file| {
            file.path == container.image_path
                && file.bytes == container.image_bytes
                && file.sha256 == container.image_sha256
                && file.role == FileRole::Runtime
        })
}

#[cfg(target_os = "macos")]
pub(crate) use container::validate_mounted;

const FORBIDDEN_WINDOWS_CAPABILITIES: [&str; 10] = [
    "documentsLibrary",
    "enterpriseAuthentication",
    "internetClient",
    "internetClientServer",
    "musicLibrary",
    "picturesLibrary",
    "privateNetworkClientServer",
    "removableStorage",
    "sharedUserCertificates",
    "videosLibrary",
];

fn valid_app_container_authority(target: &Target, target_name: &str) -> bool {
    let Some(authority) = &target.sandbox.app_container else {
        return target_name != "x86_64-pc-windows-msvc";
    };
    target_name == "x86_64-pc-windows-msvc"
        && authority.profile_name.len() >= 8
        && authority.profile_name.len() <= 128
        && authority
            .profile_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && authority.sid.starts_with("S-1-15-2-")
        && authority.sid.len() <= 192
        && authority
            .sid
            .strip_prefix('S')
            .is_some_and(|tail| tail.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-'))
        && authority.capabilities.is_empty()
        && authority.forbidden_capabilities.len() == FORBIDDEN_WINDOWS_CAPABILITIES.len()
        && authority
            .forbidden_capabilities
            .iter()
            .map(String::as_str)
            .eq(FORBIDDEN_WINDOWS_CAPABILITIES)
}

#[cfg(windows)]
fn verify_platform_sandbox(target: &Target) -> Result<(), ConversionError> {
    let authority = target
        .sandbox
        .app_container
        .as_ref()
        .ok_or_else(|| unavailable("appContainerAuthority"))?;
    crate::windows_support::AppContainerSid::derive(authority)
        .map(drop)
        .map_err(|()| unavailable("appContainerIdentity"))
}

fn file_matches(
    path: &Path,
    entry: &RuntimeFile,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let before = path.metadata().map_err(|_| unavailable("runtimeIo"))?;
    if before.len() != entry.bytes {
        return Ok(false);
    }
    let mut file = File::open(path).map_err(|_| unavailable("runtimeIo"))?;
    if !same_file(&before, &file.metadata().map_err(|_| unavailable("runtimeIo"))?) {
        return Ok(false);
    }
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        context.checkpoint()?;
        let count = file.read(&mut buffer).map_err(|_| unavailable("runtimeIo"))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| unavailable("runtimeIo"))?;
        hash.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|_| unavailable("runtimeIo"))?;
    Ok(total == entry.bytes
        && same_file(&before, &after)
        && format!("{:x}", hash.finalize()) == entry.sha256)
}

fn validate_abi(path: &Path, abi: &Abi, context: &ExecutionContext) -> Result<(), ConversionError> {
    let length = path.metadata().map_err(|_| unavailable("runtimeIo"))?.len();
    if length == 0 || length > MAX_RUNTIME_BYTES {
        return Err(unavailable("runtimeAbi"));
    }
    let _memory = context.reserve_memory(length)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(length).map_err(|_| unavailable("runtimeAbi"))?)
        .map_err(|_| unavailable("runtimeAbi"))?;
    File::open(path)
        .map_err(|_| unavailable("runtimeIo"))?
        .take(length.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable("runtimeIo"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != length {
        return Err(unavailable("runtimeIo"));
    }
    let object = object::File::parse(bytes.as_slice()).map_err(|_| unavailable("runtimeAbi"))?;
    let format = match object.format() {
        BinaryFormat::Elf => "elf",
        BinaryFormat::MachO => "mach-o",
        BinaryFormat::Coff | BinaryFormat::Pe => "pe",
        _ => return Err(unavailable("runtimeAbi")),
    };
    let architecture = match object.architecture() {
        Architecture::Aarch64 => "aarch64",
        Architecture::X86_64 => "x86_64",
        _ => return Err(unavailable("runtimeAbi")),
    };
    let underscored = format!("_{}", abi.required_export);
    let exported =
        object.exports().map_err(|_| unavailable("runtimeAbi"))?.filter_map(Result::ok).any(
            |export| match export.name() {
                object::read::NameOrOrdinal::Name(name) => {
                    name == abi.required_export.as_bytes() || name == underscored.as_bytes()
                }
                object::read::NameOrOrdinal::Ordinal(_) => false,
            },
        );
    if format != abi.binary_format
        || architecture != abi.architecture
        || path.file_name().and_then(std::ffi::OsStr::to_str) != Some(&abi.library_identity)
        || !exported
    {
        return Err(unavailable("runtimeAbi"));
    }
    Ok(())
}

fn validate_inventory_complete(
    root: &Path,
    authority_path: &Path,
    expected: &BTreeSet<&str>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut expected_directories = BTreeSet::new();
    for path in expected {
        let mut current = *path;
        while let Some((parent, _)) = current.rsplit_once('/') {
            if !expected_directories.insert(parent) {
                break;
            }
            current = parent;
        }
    }
    let mut pending = vec![root.to_owned()];
    let mut files = 0_usize;
    let mut directories = 0_usize;
    while let Some(directory) = pending.pop() {
        context.checkpoint()?;
        let entries = fs::read_dir(&directory).map_err(|_| unavailable("runtimeIo"))?;
        for entry in entries {
            context.checkpoint()?;
            let entry = entry.map_err(|_| unavailable("runtimeIo"))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|_| unavailable("runtimeIo"))?;
            if metadata.file_type().is_symlink() || is_reparse(&metadata) {
                return Err(unavailable("fileInventory"));
            }
            if metadata.is_dir() {
                let relative = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(Path::to_str)
                    .ok_or_else(|| unavailable("fileInventory"))?;
                if !expected_directories.contains(relative) {
                    return Err(unavailable("fileInventory"));
                }
                directories =
                    directories.checked_add(1).ok_or_else(|| unavailable("fileInventory"))?;
                pending.try_reserve(1).map_err(|_| unavailable("fileInventory"))?;
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                return Err(unavailable("fileInventory"));
            }
            if path == authority_path {
                continue;
            }
            files = files.checked_add(1).ok_or_else(|| unavailable("fileInventory"))?;
            if files > MAX_RUNTIME_FILES {
                return Err(unavailable("fileInventory"));
            }
            let relative = path
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| unavailable("fileInventory"))?;
            if !expected.contains(relative) {
                return Err(unavailable("fileInventory"));
            }
        }
    }
    if files != expected.len() || directories != expected_directories.len() {
        return Err(unavailable("fileInventory"));
    }
    Ok(())
}

fn https_url(value: &str) -> bool {
    value.starts_with("https://")
        && value.len() <= 2_048
        && value.is_ascii()
        && !value.bytes().any(|byte| byte.is_ascii_control())
        && !value[8..].contains('@')
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn abi_matches_target(abi: &Abi, target: &str) -> bool {
    let expected = match target {
        "aarch64-apple-darwin" => ("mach-o", "aarch64"),
        "aarch64-unknown-linux-gnu" => ("elf", "aarch64"),
        "x86_64-unknown-linux-gnu" => ("elf", "x86_64"),
        "x86_64-pc-windows-msvc" => ("pe", "x86_64"),
        _ => return false,
    };
    (abi.binary_format.as_str(), abi.architecture.as_str()) == expected
}

const SUPPORTED_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
];

fn current_target() -> Option<&'static str> {
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    return Some("aarch64-apple-darwin");
    #[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"))]
    return Some("aarch64-unknown-linux-gnu");
    #[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
    return Some("x86_64-unknown-linux-gnu");
    #[cfg(all(target_arch = "x86_64", target_os = "windows", target_env = "msvc"))]
    return Some("x86_64-pc-windows-msvc");
    #[allow(unreachable_code)]
    None
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino() && left.len() == right.len()
}

#[cfg(not(unix))]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn unavailable(detail: &'static str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "legacy-office-runtime".into(),
        detail: detail.into(),
    }
}

#[cfg(test)]
#[path = "authority_tests.rs"]
mod tests;
