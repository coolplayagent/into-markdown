//! Strict schemas shared by release metadata generation and archive verification.

#![allow(missing_docs)] // Field names are the documented JSON wire contract.

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u64 = 1;
pub const SUPPORTED_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inventory {
    pub schema_version: u64,
    pub components: Vec<Component>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Component {
    pub id: String,
    pub kind: String,
    pub status: String,
    #[serde(rename = "included_in_release")]
    pub _included_in_release: bool,
    #[serde(default)]
    pub release_eligible: bool,
    #[serde(default)]
    pub manual_only: bool,
    pub version: Option<String>,
    pub source: Option<String>,
    pub license: Option<String>,
    pub obligations: Option<String>,
    #[serde(skip)]
    pub integrity: Vec<String>,
    #[serde(skip)]
    pub authority: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    pub schema_version: u64,
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRequest {
    pub schema_version: u64,
    pub target: String,
    pub components: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveProjection {
    pub schema_version: u64,
    pub target: String,
    pub components: Vec<String>,
    pub files: Vec<ArchiveFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ffmpeg_evidence: Option<FfmpegEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FfmpegEvidence {
    pub authority_path: String,
    pub authority_bytes: u64,
    pub authority_sha256: String,
    pub schema_version: u64,
    pub ffmpeg_version: String,
    pub target: String,
    pub executable_path: String,
    pub executable_bytes: u64,
    pub executable_sha256: String,
    pub configure: Vec<String>,
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub kind: ArchiveFileKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedded_components: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveFileKind {
    Project,
    Component,
    Declaration,
    Generated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleaseInputs {
    pub schema_version: u64,
    pub target: String,
    pub component_ids: Vec<String>,
    pub notice: GeneratedFile,
    pub third_party_notices: GeneratedFile,
    pub sbom_input: GeneratedFile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GeneratedFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub contents: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SbomInput {
    pub schema_version: u64,
    pub target: String,
    pub components: Vec<SbomComponent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SbomComponent {
    pub id: String,
    pub kind: String,
    pub version: String,
    pub source: String,
    pub license: String,
    pub integrity: Vec<String>,
    pub authority: String,
}
