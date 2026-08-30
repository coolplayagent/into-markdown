use super::{CliError, Component, ExitClass, Path, PathBuf, fs, io, recovery_error};

pub(in crate::transaction) fn absolute_lexical(path: &Path) -> Result<PathBuf, CliError> {
    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "outputPathUnsupported",
                        format!("output path escapes its filesystem root: {}", path.display()),
                    ));
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    if !normalized.is_absolute() {
        return Err(CliError::new(
            ExitClass::Io,
            "outputPathUnsupported",
            format!("output path is not absolute after normalization: {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        resolve_existing_parent(&normalized)
    }
    #[cfg(not(unix))]
    {
        Ok(normalized)
    }
}

/// Resolve only the existing parent portion of an output path.
///
/// macOS exposes conventional writable locations such as `/tmp` and `/var`
/// through filesystem aliases. Opening every lexical component with
/// `O_NOFOLLOW` consequently rejects ordinary absolute output paths before a
/// transaction is created. Resolve the current parent once, append any missing
/// directory components and the untouched target name, then let `SafeDir`
/// authenticate every physical component and reject a symlink target. A later
/// alias retarget cannot redirect the already resolved physical destination.
#[cfg(unix)]
pub(in crate::transaction) fn resolve_existing_parent(path: &Path) -> Result<PathBuf, CliError> {
    let name = path.file_name().ok_or_else(|| recovery_error("target has no file name"))?;
    let mut existing = path.parent().ok_or_else(|| recovery_error("target has no parent"))?;
    let mut missing = Vec::new();
    loop {
        match fs::metadata(existing) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "outputPathUnsupported",
                        format!("output parent is not a directory: {}", existing.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = existing
                    .file_name()
                    .ok_or_else(|| recovery_error("no existing output ancestor"))?;
                missing.push(component.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| recovery_error("no existing output ancestor"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut resolved = fs::canonicalize(existing)?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    resolved.push(name);
    Ok(resolved)
}

pub(in crate::transaction) fn common_existing_ancestor(
    paths: &[PathBuf],
) -> Result<PathBuf, CliError> {
    let first = paths.first().ok_or_else(|| CliError::internal("empty output transaction"))?;
    let mut candidate = first.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    while !paths.iter().all(|path| path.starts_with(&candidate)) || !candidate.exists() {
        candidate = candidate
            .parent()
            .ok_or_else(|| {
                CliError::new(
                    ExitClass::Io,
                    "outputPathUnsupported",
                    "no common existing output ancestor",
                )
            })?
            .to_path_buf();
    }
    Ok(candidate)
}
