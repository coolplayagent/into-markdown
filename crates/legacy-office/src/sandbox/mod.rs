use std::ffi::OsString;
use std::path::{Path, PathBuf};

const MAX_SYSTEM_PATHS: usize = 128;

#[derive(Debug)]
pub(crate) struct Policy {
    pub runtime_root: PathBuf,
    pub install_root: PathBuf,
    pub kit_library: PathBuf,
    pub kit_sha256: String,
    pub authority_sha256: String,
    pub temporary_root: PathBuf,
    pub address_limit: u64,
    pub file_limit: u64,
    pub open_file_limit: u32,
    pub system_read_paths: Vec<PathBuf>,
    #[cfg(windows)]
    pub app_container_sid: String,
}

impl Policy {
    pub(crate) fn parse(arguments: impl Iterator<Item = OsString>) -> Result<Self, ()> {
        let mut runtime_root = None;
        let mut install_root = None;
        let mut kit_library = None;
        let mut kit_sha256 = None;
        let mut authority_sha256 = None;
        let mut temporary_root = None;
        let mut address_limit = None;
        let mut file_limit = None;
        let mut open_file_limit = None;
        #[cfg(windows)]
        let mut app_container_sid = None;
        let mut system_read_paths = Vec::new();
        let mut arguments = arguments;
        while let Some(flag) = arguments.next() {
            let value = arguments.next().ok_or(())?;
            match flag.to_str().ok_or(())? {
                "--runtime-root" => set_path(&mut runtime_root, value)?,
                "--install-root" => set_path(&mut install_root, value)?,
                "--kit-library" => set_path(&mut kit_library, value)?,
                "--kit-sha256" => set_text(&mut kit_sha256, value)?,
                "--authority-sha256" => set_text(&mut authority_sha256, value)?,
                "--temporary-root" => set_path(&mut temporary_root, value)?,
                "--address-limit" => set_number(&mut address_limit, &value)?,
                "--file-limit" => set_number(&mut file_limit, &value)?,
                "--open-file-limit" => set_number(&mut open_file_limit, &value)?,
                "--system-read" if system_read_paths.len() < MAX_SYSTEM_PATHS => {
                    system_read_paths.push(PathBuf::from(value));
                }
                #[cfg(windows)]
                "--app-container-sid" => set_text(&mut app_container_sid, value)?,
                _ => return Err(()),
            }
        }
        let policy = Self {
            runtime_root: runtime_root.ok_or(())?,
            install_root: install_root.ok_or(())?,
            kit_library: kit_library.ok_or(())?,
            kit_sha256: kit_sha256.ok_or(())?,
            authority_sha256: authority_sha256.ok_or(())?,
            temporary_root: temporary_root.ok_or(())?,
            address_limit: address_limit.ok_or(())?,
            file_limit: file_limit.ok_or(())?,
            open_file_limit: open_file_limit.ok_or(())?,
            system_read_paths,
            #[cfg(windows)]
            app_container_sid: app_container_sid.ok_or(())?,
        };
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> Result<(), ()> {
        let roots = [
            self.runtime_root.as_path(),
            self.install_root.as_path(),
            self.temporary_root.as_path(),
        ];
        if roots.iter().any(|path| canonical_directory(path).is_err())
            || canonical_file(&self.kit_library).is_err()
            || !self.install_root.starts_with(&self.runtime_root)
            || !self.kit_library.starts_with(&self.runtime_root)
            || self.address_limit < 256 * 1024 * 1024
            || self.file_limit == 0
            || !(16..=4_096).contains(&self.open_file_limit)
            || self.system_read_paths.iter().any(|path| canonical_directory(path).is_err())
            || self.kit_sha256.len() != 64
            || self.authority_sha256.len() != 64
            || !self
                .kit_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !self
                .authority_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !valid_platform_policy(self)
        {
            return Err(());
        }
        Ok(())
    }
}

#[cfg(not(windows))]
const fn valid_platform_policy(_: &Policy) -> bool {
    true
}

#[cfg(windows)]
fn valid_platform_policy(policy: &Policy) -> bool {
    policy.app_container_sid.starts_with("S-1-15-2-")
        && policy.app_container_sid.len() <= 192
        && policy
            .app_container_sid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'S'))
}

fn set_text(slot: &mut Option<String>, value: OsString) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(value.into_string().map_err(|_| ())?);
    Ok(())
}

fn set_path(slot: &mut Option<PathBuf>, value: OsString) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(PathBuf::from(value));
    Ok(())
}

fn set_number<T: std::str::FromStr>(slot: &mut Option<T>, value: &OsString) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(value.to_str().ok_or(())?.parse().map_err(|_| ())?);
    Ok(())
}

fn canonical_directory(path: &Path) -> Result<(), ()> {
    if !path.is_absolute() || !path.is_dir() || path.canonicalize().map_err(|_| ())? != path {
        return Err(());
    }
    Ok(())
}

fn canonical_file(path: &Path) -> Result<(), ()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ())?;
    if !path.is_absolute()
        || metadata.file_type().is_symlink()
        || !metadata.is_file()
        || path.canonicalize().map_err(|_| ())? != path
    {
        return Err(());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub(crate) fn install(policy: &Policy) -> Result<(), ()> {
    #[cfg(target_os = "linux")]
    return linux::install(policy);
    #[cfg(target_os = "macos")]
    return macos::install(policy);
    #[cfg(windows)]
    return windows::install(policy);
    #[allow(unreachable_code)]
    Err(())
}

#[cfg(unix)]
fn install_unix_limits(policy: &Policy) -> Result<(), ()> {
    set_limit(libc::RLIMIT_AS, policy.address_limit)?;
    set_limit(libc::RLIMIT_FSIZE, policy.file_limit)?;
    set_limit(libc::RLIMIT_NOFILE, u64::from(policy.open_file_limit))?;
    set_limit(libc::RLIMIT_CORE, 0)?;
    Ok(())
}

#[cfg(unix)]
fn set_limit(resource: RlimitResource, value: u64) -> Result<(), ()> {
    let value = libc::rlim_t::try_from(value).map_err(|_| ())?;
    let limit = libc::rlimit { rlim_cur: value, rlim_max: value };
    // SAFETY: `limit` has the exact libc layout and the resource is one of the
    // supported fixed constants above.
    if unsafe { libc::setrlimit(resource, &raw const limit) } != 0 {
        return Err(());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
type RlimitResource = libc::c_int;

#[cfg(target_os = "linux")]
type RlimitResource = libc::__rlimit_resource_t;
