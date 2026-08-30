use super::{
    CliError, Component, ExitClass, JournalPath, MAX_PATH_UNITS, OsString, Path, PathBuf,
    managed_nonce, recovery_error,
};
#[cfg(unix)]
use std::fs;
#[cfg(windows)]
use std::fs::{File, OpenOptions};

pub(in crate::transaction) fn validate_relative_path(path: &Path) -> Result<(), CliError> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(recovery_error("transaction target is not a strict relative path"));
    }
    if path.components().any(
        |component| matches!(component, Component::Normal(name) if managed_nonce(name).is_some()),
    ) {
        return Err(recovery_error("transaction target aliases a manager directory"));
    }
    Ok(())
}

#[cfg(unix)]
pub(in crate::transaction) fn encode_path(path: &Path) -> Result<JournalPath, CliError> {
    use std::os::unix::ffi::OsStrExt as _;
    let units = path.as_os_str().as_bytes().iter().map(|byte| u32::from(*byte)).collect::<Vec<_>>();
    if units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("transaction path exceeds limit"));
    }
    Ok(JournalPath { encoding: "unixBytes".into(), units })
}

#[cfg(windows)]
pub(in crate::transaction) fn encode_path(path: &Path) -> Result<JournalPath, CliError> {
    use std::os::windows::ffi::OsStrExt as _;
    let units = path.as_os_str().encode_wide().map(u32::from).collect::<Vec<_>>();
    if units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("transaction path exceeds limit"));
    }
    Ok(JournalPath { encoding: "windowsUtf16".into(), units })
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn encode_path(_path: &Path) -> Result<JournalPath, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "journal paths are unavailable on this platform",
    ))
}

#[cfg(unix)]
pub(in crate::transaction) fn decode_path(path: &JournalPath) -> Result<PathBuf, CliError> {
    use std::os::unix::ffi::OsStringExt as _;
    if path.encoding != "unixBytes" || path.units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("invalid Unix journal path encoding"));
    }
    let bytes = path
        .units
        .iter()
        .map(|unit| u8::try_from(*unit).map_err(|_| recovery_error("invalid Unix path byte")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
pub(in crate::transaction) fn decode_path(path: &JournalPath) -> Result<PathBuf, CliError> {
    use std::os::windows::ffi::OsStringExt as _;
    if path.encoding != "windowsUtf16" || path.units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("invalid Windows journal path encoding"));
    }
    let units = path
        .units
        .iter()
        .map(|unit| u16::try_from(*unit).map_err(|_| recovery_error("invalid Windows path unit")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn decode_path(_path: &JournalPath) -> Result<PathBuf, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "journal paths are unavailable on this platform",
    ))
}

#[cfg(unix)]
pub(in crate::transaction) fn ensure_same_filesystem(
    root: &Path,
    targets: &[PathBuf],
) -> Result<(), CliError> {
    use std::os::unix::fs::MetadataExt as _;
    let device = fs::metadata(root)?.dev();
    for target in targets {
        let mut ancestor = target.parent().unwrap_or_else(|| Path::new("."));
        while !ancestor.exists() {
            ancestor =
                ancestor.parent().ok_or_else(|| recovery_error("no existing output ancestor"))?;
        }
        if fs::metadata(ancestor)?.dev() != device {
            return Err(CliError::new(
                ExitClass::Io,
                "crossFilesystemTransaction",
                "output set crosses filesystems",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(in crate::transaction) fn ensure_same_filesystem(
    root: &Path,
    targets: &[PathBuf],
) -> Result<(), CliError> {
    let root_file = securely_open_regular_or_directory(root)?;
    let root_volume = winapi_util::file::information(&root_file)?.volume_serial_number();
    for target in targets {
        let mut ancestor = target.parent().unwrap_or_else(|| Path::new("."));
        while !ancestor.exists() {
            ancestor =
                ancestor.parent().ok_or_else(|| recovery_error("no existing output ancestor"))?;
        }
        let handle = securely_open_regular_or_directory(ancestor)?;
        if winapi_util::file::information(&handle)?.volume_serial_number() != root_volume {
            return Err(CliError::new(
                ExitClass::Io,
                "crossFilesystemTransaction",
                "output set crosses volumes",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(in crate::transaction) fn securely_open_regular_or_directory(
    path: &Path,
) -> Result<File, CliError> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(CliError::new(
            ExitClass::Io,
            "symlinkDenied",
            format!("reparse point denied: {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
pub(in crate::transaction) fn ensure_same_filesystem(
    _root: &Path,
    _targets: &[PathBuf],
) -> Result<(), CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "safe output transactions are unavailable",
    ))
}
