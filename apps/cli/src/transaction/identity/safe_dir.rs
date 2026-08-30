use super::{CliError, Component, Digest, OsStr, Path, recovery_error};

pub(in crate::transaction) fn validate_single_name(name: &OsStr) -> Result<(), CliError> {
    let path = Path::new(name);
    if path.as_os_str().is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(recovery_error("transaction member is not a single safe name"));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) struct SafeDir;
