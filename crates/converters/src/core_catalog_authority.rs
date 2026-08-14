//! Stable core-catalog schema and descriptor types.

#![allow(missing_docs)] // Fields are the stable public and JSON schema contracts.

use into_markdown_core::InputFormat;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySource {
    Core,
    OptionalRuntime,
    Plugin,
}
impl CapabilitySource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::OptionalRuntime => "optional_runtime",
            Self::Plugin => "plugin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityKind {
    SourceResolver,
    FormatDetector,
    Converter,
    Runtime,
}
impl CapabilityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceResolver => "source_resolver",
            Self::FormatDetector => "format_detector",
            Self::Converter => "converter",
            Self::Runtime => "runtime",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityAvailability {
    Available,
    OptionalRuntime,
}
impl CapabilityAvailability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::OptionalRuntime => "optional_runtime",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRequirement {
    pub component: &'static str,
    pub install_hint: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    pub id: &'static str,
    pub kind: CapabilityKind,
    pub source: CapabilitySource,
    pub availability: CapabilityAvailability,
    pub priority: i32,
    pub formats: &'static [InputFormat],
    pub runtime: Option<RuntimeRequirement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatStatus {
    Available,
    Planned,
}
impl FormatStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Planned => "planned",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatDescriptor {
    pub format: InputFormat,
    pub family: &'static str,
    pub extensions: &'static [&'static str],
    pub status: FormatStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogFormatDescriptor {
    pub descriptor: &'static FormatDescriptor,
    pub source: CapabilitySource,
    pub runtime: Option<RuntimeRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CoreCatalogAuthority {
    pub schema_version: u64,
    pub entries_sha256: String,
    pub entries: Vec<CoreCatalogAuthorityEntry>,
    pub optional_runtimes_sha256: String,
    pub optional_runtimes: Vec<CoreRuntimeAuthorityEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CoreRuntimeAuthorityEntry {
    pub id: String,
    pub component: String,
    pub install_hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CoreCatalogAuthorityEntry {
    pub format: String,
    pub family: String,
    pub extensions: Vec<String>,
    pub status: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
}
