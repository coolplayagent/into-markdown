use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct Authority {
    pub schema_version: u32,
    pub product: String,
    pub version: String,
    pub source_url: String,
    pub targets: std::collections::BTreeMap<String, Target>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct Target {
    pub artifact_url: String,
    pub artifact_bytes: u64,
    pub artifact_sha256: String,
    pub install_root: String,
    pub kit_library: String,
    pub worker: String,
    pub files: Vec<RuntimeFile>,
    pub licenses: Vec<RuntimeLicense>,
    pub abi: Abi,
    pub limits: WorkerLimits,
    pub sandbox: SandboxAuthority,
    pub container: Option<ContainerAuthority>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct ContainerAuthority {
    pub format: String,
    pub image_path: String,
    pub image_bytes: u64,
    pub image_sha256: String,
    pub mount_path: String,
    pub kit_sha256: String,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RuntimeFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub role: FileRole,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum FileRole {
    Worker,
    KitLibrary,
    Runtime,
    Configuration,
    License,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct RuntimeLicense {
    pub id: String,
    pub spdx: Option<String>,
    pub notice_path: String,
    pub notice_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct Abi {
    pub binary_format: String,
    pub architecture: String,
    pub library_identity: String,
    pub required_export: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct WorkerLimits {
    pub address_space_overhead_bytes: u64,
    pub file_size_limit_bytes: u64,
    pub open_file_limit: u32,
    pub process_limit: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct SandboxAuthority {
    pub system_libraries: Vec<SystemLibraryAuthority>,
    pub network: String,
    pub child_processes: String,
    pub compatibility_child: Option<CompatibilityChildAuthority>,
    pub app_container: Option<AppContainerAuthority>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct CompatibilityChildAuthority {
    pub executable: String,
    pub maximum_instances: u32,
    pub local_ip: String,
    pub local_ipc: String,
}

#[derive(Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct SystemLibraryAuthority {
    pub identity: String,
    pub path: String,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AppContainerAuthority {
    pub profile_name: String,
    pub sid: String,
    pub capabilities: Vec<String>,
    pub forbidden_capabilities: Vec<String>,
}
