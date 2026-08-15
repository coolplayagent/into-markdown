use md5::Digest as _;
use std::path::{Path, PathBuf};

pub(crate) fn office_ipc_path(profile: &Path) -> PathBuf {
    let url = super::file_url(profile);
    let mut hash = md5::Md5::new();
    for unit in url.encode_utf16() {
        hash.update(unit.to_le_bytes());
    }
    // LibreOffice renders every digest byte without zero padding.
    let token = hash.finalize().iter().fold(String::new(), |mut token, byte| {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:x}");
        token
    });
    // SAFETY: geteuid takes no arguments and has no failure condition.
    let euid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/private/tmp/OSL_PIPE_{euid}_SingleOfficeIPC_{token}"))
}

pub(crate) fn office_ipc_network_path(path: &Path) -> Result<PathBuf, u8> {
    let name = path.strip_prefix("/private/tmp").map_err(|_| crate::protocol::ERROR_RUNTIME)?;
    Ok(Path::new("/tmp").join(name))
}

pub(crate) fn remove_office_ipc(path: &Path) {
    let _ = std::fs::remove_file(path);
    if let Ok(network_path) = office_ipc_network_path(path) {
        let _ = std::fs::remove_file(network_path);
    }
}

pub(super) struct OfficeIpcSocket(PathBuf);

impl OfficeIpcSocket {
    pub(super) fn new(profile: &Path) -> Result<Self, u8> {
        let path = office_ipc_path(profile);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self(path)),
            Ok(_) | Err(_) => Err(crate::protocol::ERROR_SANDBOX),
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for OfficeIpcSocket {
    fn drop(&mut self) {
        remove_office_ipc(&self.0);
    }
}
