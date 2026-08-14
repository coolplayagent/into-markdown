use super::{MAX_PATH_BYTES, unavailable};
use into_markdown_core::ConversionError;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(super) fn explicit_directory(path: &Path) -> Result<PathBuf, ConversionError> {
    if !path.is_absolute() {
        return Err(unavailable("unsafePath"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable("runtimeNotPackaged"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || is_reparse(&metadata) {
        return Err(unavailable("unsafePath"));
    }
    let canonical = path.canonicalize().map_err(|_| unavailable("unsafePath"))?;
    if canonical != path {
        return Err(unavailable("unsafePath"));
    }
    Ok(canonical)
}

pub(super) fn explicit_regular_file(
    path: &Path,
    root: Option<&Path>,
) -> Result<PathBuf, ConversionError> {
    if !path.is_absolute() || root.is_some_and(|root| !path.starts_with(root)) {
        return Err(unavailable("unsafePath"));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable("runtimeNotPackaged"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || is_reparse(&metadata) {
        return Err(unavailable("unsafePath"));
    }
    let canonical = path.canonicalize().map_err(|_| unavailable("unsafePath"))?;
    if canonical != path || root.is_some_and(|root| !canonical.starts_with(root)) {
        return Err(unavailable("unsafePath"));
    }
    Ok(canonical)
}

pub(super) fn checked_join(root: &Path, relative: &str) -> Result<PathBuf, ConversionError> {
    if !safe_relative(relative) {
        return Err(unavailable("unsafePath"));
    }
    let joined = root.join(relative);
    if !joined.starts_with(root) {
        return Err(unavailable("unsafePath"));
    }
    Ok(joined)
}

pub(super) fn explicit_system_directory(
    value: &str,
    target: &str,
) -> Result<PathBuf, ConversionError> {
    let path = Path::new(value);
    if value.len() > MAX_PATH_BYTES || !path.is_absolute() || !allowed_system_path(value, target) {
        return Err(unavailable("sandboxAuthority"));
    }
    system_directory(path).map_err(|_| unavailable("sandboxAuthority"))
}

#[cfg(not(windows))]
fn system_directory(path: &Path) -> Result<PathBuf, ConversionError> {
    explicit_directory(path)
}

#[cfg(windows)]
fn system_directory(path: &Path) -> Result<PathBuf, ConversionError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| unavailable("sandboxAuthority"))?;
    if !metadata.is_dir() || is_reparse(&metadata) {
        return Err(unavailable("sandboxAuthority"));
    }
    path.canonicalize().map_err(|_| unavailable("sandboxAuthority"))
}

pub(super) fn allowed_system_path(value: &str, target: &str) -> bool {
    let allowed: &[&str] = match target {
        "aarch64-apple-darwin" => {
            &["/System/Library", "/usr/lib", "/Library/Fonts", "/System/Library/Fonts"]
        }
        "aarch64-unknown-linux-gnu" | "x86_64-unknown-linux-gnu" => &[
            "/lib",
            "/lib64",
            "/usr/lib",
            "/usr/lib64",
            "/usr/share/fonts",
            "/usr/share/zoneinfo",
            "/etc/fonts",
        ],
        "x86_64-pc-windows-msvc" => &[r"C:\Windows\System32", r"C:\Windows\Fonts"],
        _ => &[],
    };
    allowed.contains(&value)
}

pub(super) fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PATH_BYTES
        && value.is_ascii()
        && !value.bytes().any(|byte| byte.is_ascii_control() || byte == 0x7f)
        && !value.contains(['\\', '\0'])
        && value.split('/').all(|component| !matches!(component, "" | "." | ".."))
        && Path::new(value).components().all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(windows)]
pub(super) fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
pub(super) const fn is_reparse(_: &fs::Metadata) -> bool {
    false
}
