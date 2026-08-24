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
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Component {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub included_in_release: bool,
    #[serde(default)]
    pub release_eligible: bool,
    #[serde(default)]
    pub manual_only: bool,
    #[serde(skip)]
    pub required_in_core: bool,
    pub version: Option<String>,
    pub source: Option<String>,
    pub license: Option<String>,
    pub obligations: Option<String>,
    #[serde(skip)]
    pub integrity: Vec<IntegrityEvidence>,
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
    #[serde(default)]
    pub artifact: ReleaseArtifact,
    #[serde(default = "default_release_version")]
    pub version: String,
    pub source_revision: String,
    pub components: Vec<String>,
}

fn default_release_version() -> String {
    "0.0.0".to_owned()
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseArtifact {
    #[default]
    Core,
    OcrPlugin,
    MediaPlugin,
    LegacyOfficePlugin,
}

impl ReleaseArtifact {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Core => "into-markdown-core",
            Self::OcrPlugin => "official.ocr.ppocrv6",
            Self::MediaPlugin => "official.media.whisper",
            Self::LegacyOfficePlugin => "official.legacy-office.libreoffice",
        }
    }

    pub(crate) fn cargo_root(self) -> &'static str {
        match self {
            Self::Core => "into-markdown-cli",
            Self::OcrPlugin | Self::MediaPlugin | Self::LegacyOfficePlugin => {
                "into-markdown-official-provider"
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveProjection {
    pub schema_version: u64,
    pub target: String,
    pub version: String,
    pub source_revision: String,
    pub components: Vec<String>,
    pub files: Vec<ArchiveFile>,
    #[serde(default)]
    pub license_materials: Vec<LicenseMaterial>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ffmpeg_evidence: Option<FfmpegEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_transformations: Vec<NativeTransformation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTransformation {
    pub component_id: String,
    pub path: String,
    pub kind: NativeTransformationKind,
    pub source_bytes: u64,
    pub source_sha256: String,
    pub output_bytes: u64,
    pub output_sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeTransformationKind {
    AppleCodeSign,
    Authenticode,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FfmpegEvidence {
    pub authority_path: String,
    pub authority_bytes: u64,
    pub authority_sha256: String,
    pub authority_contents: String,
    pub schema_version: u64,
    pub ffmpeg_version: String,
    pub target: String,
    pub executable_path: String,
    pub executable_bytes: u64,
    pub executable_sha256: String,
    pub configure: Vec<String>,
    pub dependencies: Vec<String>,
    pub binary_format: String,
    pub binary_architecture: String,
    pub toolchain: String,
    pub source_sha256: String,
    pub source_signature_sha256: String,
    pub signing_key_fingerprint: String,
    pub build_policy_sha256: String,
    pub config_log_sha256: String,
    pub relink_bytes: u64,
    pub relink_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveFile {
    pub path: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
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
    LicenseMaterial,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LicenseMaterial {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub kind: LicenseMaterialKind,
    pub component_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spdx_expressions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseMaterialKind {
    LicenseText,
    NoticeBundle,
    CorrespondingSource,
    RelinkMaterial,
    UpstreamSourceArchive,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleaseInputs {
    pub schema_version: u64,
    pub target: String,
    pub component_ids: Vec<String>,
    pub notice: GeneratedFile,
    pub third_party_notices: GeneratedFile,
    pub sbom: GeneratedFile,
    pub sources: GeneratedFile,
    pub core_catalog: GeneratedFile,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GeneratedFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub contents: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceManifest {
    pub schema_version: u64,
    pub target: String,
    pub artifact: String,
    pub version: String,
    pub source_revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_file: Option<SourceArtifact>,
    pub components: Vec<SourceComponent>,
    pub build_tools: Vec<BuildTool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceArtifact {
    pub file_name: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceComponent {
    pub id: String,
    pub kind: String,
    pub version: String,
    pub source: String,
    pub license: String,
    pub scope: String,
    pub distributed: bool,
    pub integrity: Vec<IntegrityEvidence>,
    pub authority: String,
    #[serde(default)]
    pub files: Vec<SourceFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub bytes: u64,
    pub sha1: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceDependencyInventory {
    pub schema_version: u64,
    pub components: Vec<SourceComponent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildTool {
    pub id: String,
    pub version: String,
    pub source: String,
    pub license: String,
    pub scope: String,
    pub distributed: bool,
    pub targets: Vec<String>,
    pub integrity: Vec<IntegrityEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executables: Vec<BuildToolExecutable>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildToolExecutable {
    pub authority_id: String,
    pub name: String,
    pub version: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildToolInventory {
    pub schema_version: u64,
    pub tools: Vec<BuildTool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactProjection {
    pub schema_version: u64,
    pub target: String,
    pub artifact: ReleaseArtifact,
    pub version: String,
    pub source_revision: String,
    pub file_name: String,
    pub bytes: u64,
    pub sha256: String,
    pub components: Vec<String>,
    pub files: Vec<ArchiveFile>,
    #[serde(default)]
    pub build_tools: Vec<BuildToolExecutable>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactMetadata {
    pub schema_version: u64,
    pub target: String,
    pub artifact: String,
    pub file_name: String,
    pub sha256: String,
    pub sbom: GeneratedFile,
    pub sources: GeneratedFile,
    pub third_party_notices: GeneratedFile,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSetRequest {
    pub schema_version: u64,
    pub target: String,
    pub version: String,
    pub source_revision: String,
    pub artifacts: Vec<ReleaseSetArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSetArtifact {
    pub artifact: ReleaseArtifact,
    pub file_name: String,
    pub bytes: u64,
    pub sha256: String,
    pub components: Vec<String>,
    pub sbom_sha256: String,
    pub sources_sha256: String,
    pub notices_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleaseSetMetadata {
    pub schema_version: u64,
    pub target: String,
    pub release_set: GeneratedFile,
    pub sbom: GeneratedFile,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrityEvidence {
    pub algorithm: String,
    pub digest: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}
