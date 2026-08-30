use super::{
    CliError, Digest, File, OsStr, Read, Sha256, io, recover_windows_config_transaction,
    recovery_error,
};

pub(crate) fn recover_config_in_dir(
    directory: &cap_std::fs::Dir,
    target: &OsStr,
) -> Result<(), CliError> {
    recover_windows_config_transaction(directory, target)
}

pub(super) fn valid_windows_config_transaction_name(value: &str, suffix: &str) -> bool {
    value.starts_with(".into-md-config-")
        && value.ends_with(&format!(".{suffix}"))
        && value.len() == ".into-md-config-".len() + 32 + 1 + suffix.len()
        && value[".into-md-config-".len()..".into-md-config-".len() + 32]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn config_transaction_nonce(value: &str) -> Option<&str> {
    let prefix = ".into-md-config-";
    let remainder = value.strip_prefix(prefix)?;
    let (nonce, _) = remainder.split_once('.')?;
    (nonce.len() == 32
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some(nonce)
}

pub(super) fn cap_config_identity(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
) -> Result<Option<(u64, u64)>, CliError> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    match directory.open_with(name, &options) {
        Ok(file) => {
            let file = file.into_std();
            if config_open_file_link_count(&file)? != 1 {
                return Err(recovery_error("config transaction file identity rejected"));
            }
            Ok(Some(config_open_file_identity(&file)?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn cap_config_digest(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
) -> Result<Option<String>, CliError> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match directory.open_with(name, &options) {
        Ok(file) => file.into_std(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if config_open_file_link_count(&file)? != 1 {
        return Err(recovery_error("config transaction file identity rejected"));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Some(format!("{:x}", hasher.finalize())))
}

pub(super) fn sync_cap_directory(directory: &cap_std::fs::Dir) -> Result<(), CliError> {
    #[cfg(unix)]
    let result = {
        use cap_std::fs::OpenOptionsExt as _;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC);
        directory.open_with(".", &options)?.into_std().sync_all()
    };
    #[cfg(windows)]
    let result = directory.try_clone()?.into_std_file().sync_all();
    match result {
        Ok(()) => Ok(()),
        // Windows commonly denies FlushFileBuffers for directory handles. The
        // journal and replacement file themselves are flushed; the journaled
        // state machine therefore provides process-crash recovery. This does
        // not claim a power-loss directory-flush guarantee on such volumes.
        #[cfg(windows)]
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                || matches!(error.raw_os_error(), Some(1 | 6)) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn config_open_file_identity(file: &File) -> Result<(u64, u64), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.metadata()?;
        return Ok((metadata.dev(), metadata.ino()));
    }
    #[cfg(windows)]
    {
        let information = winapi_util::file::information(file)?;
        return Ok((information.volume_serial_number(), information.file_index()));
    }
    #[allow(unreachable_code)]
    Err(recovery_error("config identity unavailable"))
}

pub(super) fn config_open_file_link_count(file: &File) -> Result<u64, CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        return Ok(file.metadata()?.nlink());
    }
    #[cfg(windows)]
    return Ok(winapi_util::file::information(file)?.number_of_links());
    #[allow(unreachable_code)]
    Err(recovery_error("config link count unavailable"))
}

#[cfg(all(not(unix), not(windows)))]
pub(crate) fn atomic_replace_config(
    _path: &Path,
    _bytes: &[u8],
    _replace: bool,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}
