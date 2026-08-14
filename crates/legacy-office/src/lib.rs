//! Fail-closed isolated conversion boundary for legacy Microsoft Office files.
//!
//! This crate owns runtime authority validation, the length-prefixed worker
//! protocol, process lifecycle, and native worker entry point. It does not
//! parse the normalized OOXML package or construct document IR.
#![deny(unsafe_op_in_unsafe_fn)]

mod authority;
mod client;
mod native;
mod package;
mod process;
mod protocol;
mod sandbox;
mod snapshot;
#[cfg(windows)]
mod windows_support;

use into_markdown_core::{ConversionError, ExecutionContext, InputFormat};
use std::path::Path;

pub use authority::{RuntimeConfig, RuntimeIdentity};

/// Maximum wire-format output supported by this protocol revision.
pub const MAX_NORMALIZED_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;

/// Fixed transient metadata allowance for normalized-package validation.
pub const NORMALIZED_PACKAGE_AUDIT_MEMORY_BYTES: u64 = 32 * 1024 * 1024;

/// Normalized package kind returned by the compatibility worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizedFormat {
    /// `WordprocessingML` package.
    Docx,
    /// `PresentationML` package.
    Pptx,
    /// `SpreadsheetML` package.
    Xlsx,
}

impl NormalizedFormat {
    /// Corresponding engine format hint.
    #[must_use]
    pub const fn input_format(self) -> InputFormat {
        match self {
            Self::Docx => InputFormat::Docx,
            Self::Pptx => InputFormat::Pptx,
            Self::Xlsx => InputFormat::Xlsx,
        }
    }

    /// Canonical package extension.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Xlsx => "xlsx",
        }
    }
}

/// Validate a normalized OOXML package's exact ZIP envelope and family.
///
/// This is the common parent/worker audit boundary. It validates every local
/// and central record, member CRC, canonical path, content-type declaration,
/// and root office-document relationship before nested dispatch.
///
/// # Errors
///
/// Returns `Malformed`, `Encrypted`, `ResourceLimit`, cancellation, or timeout
/// without attempting to interpret document content.
pub fn audit_normalized_package(
    bytes: &[u8],
    expected: NormalizedFormat,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let _memory = context.reserve_memory(NORMALIZED_PACKAGE_AUDIT_MEMORY_BYTES)?;
    package::audit(bytes, expected, context)
}

/// Verified worker result and its compatibility-runtime identity.
#[derive(Debug)]
pub struct NormalizedPackage {
    /// Exact normalized OOXML bytes.
    pub bytes: Box<[u8]>,
    /// Output package family.
    pub format: NormalizedFormat,
    /// Runtime identity validated before the worker was launched.
    pub runtime: RuntimeIdentity,
    /// Live request-memory charge for `bytes`.
    pub memory: into_markdown_core::ResourceReservation,
}

/// Explicit worker/runtime configuration.
///
/// Paths are never taken from `PATH`, loader variables, proxy variables, or
/// the current working directory. `packaged` derives a fixed sibling layout
/// from the canonical current executable and still validates every authority
/// entry before launch.
#[derive(Debug, Clone)]
pub struct LegacyOfficeRuntime {
    config: RuntimeConfig,
}

impl LegacyOfficeRuntime {
    /// Use one explicit authority, bundle root, and worker executable.
    #[must_use]
    pub const fn new(config: RuntimeConfig) -> Self {
        Self { config }
    }

    /// Resolve the fixed packaged layout beside the running executable.
    ///
    /// # Errors
    ///
    /// Returns `ComponentUnavailable` if the executable has no canonical
    /// parent directory. Runtime contents are validated during conversion.
    pub fn packaged() -> Result<Self, ConversionError> {
        let executable = std::env::current_exe().map_err(|_| unavailable("runtimeNotPackaged"))?;
        let executable =
            executable.canonicalize().map_err(|_| unavailable("runtimeNotPackaged"))?;
        let parent = executable.parent().ok_or_else(|| unavailable("runtimeNotPackaged"))?;
        let root = parent.join("legacy-office-runtime");
        Ok(Self::new(RuntimeConfig::new(
            root.join("authority.json"),
            root.clone(),
            root.join(worker_file_name()),
        )))
    }

    /// Convert one legacy Office payload into an audited OOXML package.
    ///
    /// # Errors
    ///
    /// Returns stable conversion, component, resource, cancellation, or
    /// timeout errors. The worker is killed and reaped before any error returns.
    pub fn convert(
        &self,
        bytes: &[u8],
        source_format: InputFormat,
        maximum_output_bytes: u64,
        context: &ExecutionContext,
    ) -> Result<NormalizedPackage, ConversionError> {
        client::convert(&self.config, bytes, source_format, maximum_output_bytes, context)
    }

    /// Borrow the configured authority path for diagnostics and packaging.
    #[must_use]
    pub fn authority_path(&self) -> &Path {
        self.config.authority_path()
    }
}

fn unavailable(detail: &'static str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "legacy-office-runtime".into(),
        detail: detail.into(),
    }
}

#[cfg(windows)]
const fn worker_file_name() -> &'static str {
    "legacy-office-worker.exe"
}

#[cfg(not(windows))]
const fn worker_file_name() -> &'static str {
    "legacy-office-worker"
}

/// Run the native compatibility worker on stdin/stdout.
#[doc(hidden)]
#[must_use]
pub fn worker_main() -> std::process::ExitCode {
    process::worker_main()
}

/// Run the deterministic protocol fixture worker.
#[doc(hidden)]
#[must_use]
pub fn test_worker_main() -> std::process::ExitCode {
    process::test_worker_main()
}
