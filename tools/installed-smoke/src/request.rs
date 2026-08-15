//! Strongly typed runner inputs.

use crate::path_policy::{canonical_directory, canonical_file, contained};
use clap::Parser;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

/// Inputs supplied by a platform packaging adapter.
#[derive(Clone, Debug, Parser)]
#[command(name = "installed-smoke")]
pub struct SmokeRequest {
    /// Root of the installed archive.
    #[arg(long)]
    pub install_root: PathBuf,
    /// Installed into-md executable.
    #[arg(long)]
    pub into_md: PathBuf,
    /// Installed standalone Rust package root, including Cargo.toml and vendor/.
    #[arg(long)]
    pub rust_library: PathBuf,
    /// #60 archive projection manifest.
    #[arg(long)]
    pub manifest: PathBuf,
    /// Installed smoke fixture root.
    #[arg(long)]
    pub fixtures: PathBuf,
    /// Empty parent directory for all mutable runner state.
    #[arg(long)]
    pub temp_root: PathBuf,
    /// Destination for the machine-readable report.
    #[arg(long)]
    pub report: PathBuf,
    /// SHA-256 of the distribution archive.
    #[arg(long)]
    pub archive_sha256: String,
    /// Absolute Cargo executable from the fixed release toolchain.
    #[arg(long)]
    pub cargo: PathBuf,
    /// Absolute rustc executable from the fixed release toolchain.
    #[arg(long)]
    pub rustc: PathBuf,
    /// Exact installed `PDFium` library, when the archive includes PDF support.
    #[arg(long)]
    pub pdfium_library: Option<PathBuf>,
    /// Per-process deadline in seconds.
    #[arg(long, default_value = "30")]
    pub timeout_seconds: NonZeroU64,
    /// Optional file whose appearance cancels the active run.
    #[arg(long)]
    pub cancel_file: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct ValidatedRequest {
    pub install_root: PathBuf,
    pub into_md: PathBuf,
    pub rust_library: PathBuf,
    pub manifest: PathBuf,
    pub fixtures: PathBuf,
    pub temp_root: PathBuf,
    pub report: PathBuf,
    pub archive_sha256: String,
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    pub timeout: std::time::Duration,
    pub cancel_file: Option<PathBuf>,
}

impl SmokeRequest {
    pub(crate) fn validate(self) -> Result<ValidatedRequest, String> {
        if self.archive_sha256.len() != 64
            || !self
                .archive_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("archive SHA-256 must be 64 lowercase hexadecimal characters".into());
        }
        let install_root = canonical_directory(&self.install_root, "install root")?;
        let into_md = canonical_file(&self.into_md, "into-md")?;
        let rust_library = canonical_directory(&self.rust_library, "Rust library")?;
        let manifest = canonical_file(&self.manifest, "archive manifest")?;
        let fixtures = canonical_directory(&self.fixtures, "fixture root")?;
        for (path, label) in [
            (&into_md, "into-md"),
            (&rust_library, "Rust library"),
            (&manifest, "archive manifest"),
            (&fixtures, "fixture root"),
        ] {
            contained(&install_root, path, label)?;
        }
        let cargo = canonical_file(&self.cargo, "cargo")?;
        let rustc = canonical_file(&self.rustc, "rustc")?;
        let pdfium_library =
            self.pdfium_library.map(|path| canonical_file(&path, "PDFium library")).transpose()?;
        if let Some(path) = &pdfium_library {
            contained(&install_root, path, "PDFium library")?;
        }
        let temp_root = canonical_directory(&self.temp_root, "temporary root")?;
        if contained(&install_root, &temp_root, "temporary root").is_ok() {
            return Err("temporary root must be outside the immutable installation".into());
        }
        if fs::read_dir(&temp_root)
            .map_err(|error| format!("cannot inspect temporary root: {error}"))?
            .next()
            .is_some()
        {
            return Err("temporary root must be empty".into());
        }
        let report = absolute_output(&self.report)?;
        if report.starts_with(&install_root) || report.starts_with(&temp_root) {
            return Err("report must be outside the installation and temporary root".into());
        }
        if !rust_library.join("Cargo.toml").is_file()
            || !rust_library.join("Cargo.lock").is_file()
            || !rust_library.join("vendor").is_dir()
        {
            return Err(
                "Rust library must contain Cargo.toml, Cargo.lock, and an offline vendor directory"
                    .into(),
            );
        }
        Ok(ValidatedRequest {
            install_root,
            into_md,
            rust_library,
            manifest,
            fixtures,
            temp_root,
            report,
            archive_sha256: self.archive_sha256,
            cargo,
            rustc,
            timeout: std::time::Duration::from_secs(self.timeout_seconds.get()),
            cancel_file: self.cancel_file,
        })
    }
}

impl ValidatedRequest {
    pub(crate) fn create_run_root(&self) -> Result<PathBuf, String> {
        tempfile::Builder::new()
            .prefix("into-md-installed-smoke-")
            .tempdir_in(&self.temp_root)
            .map_err(|error| format!("cannot create isolated run directory: {error}"))?
            .keep()
            .canonicalize()
            .map_err(|error| format!("cannot resolve isolated run directory: {error}"))
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancel_file.as_ref().is_some_and(|path| path.exists())
    }

    pub(crate) fn cli_environment(home: &Path) -> std::collections::BTreeMap<String, String> {
        crate::process::command_environment(home)
    }
}

fn absolute_output(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("report path must be absolute".into());
    }
    let parent = path.parent().ok_or_else(|| "report path has no parent".to_owned())?;
    let parent = canonical_directory(parent, "report parent")?;
    let name = path.file_name().ok_or_else(|| "report path has no file name".to_owned())?;
    Ok(parent.join(name))
}
