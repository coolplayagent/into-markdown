use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_ZIP_ENTRIES: usize = 25_000;
const MAX_ZIP_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ZIP_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ZIP_RATIO: u64 = 100;

// Variant payloads exist for their Drop implementations: mounted images must
// detach and extracted bytes must retain their request reservations.
#[allow(dead_code)]
pub(super) enum PreparedContainer {
    Mounted(MountedContainer),
    Extracted(ExtractedContainer),
}

impl PreparedContainer {
    pub(super) fn prepare(
        format: &str,
        image: &Path,
        root: &Path,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        match format {
            "udif" => MountedContainer::attach(image, root).map(Self::Mounted),
            "zip" => ExtractedContainer::extract(image, root, context).map(Self::Extracted),
            _ => Err(unavailable("containerFormat")),
        }
    }
}

pub(super) struct MountedContainer {
    mount: PathBuf,
}

impl MountedContainer {
    pub(super) fn attach(image: &Path, mount: &Path) -> Result<Self, ConversionError> {
        let status = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-noautoopen", "-readonly", "-mountpoint"])
            .arg(mount)
            .arg(image)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| unavailable("containerAttach"))?;
        if !status.success() {
            return Err(unavailable("containerAttach"));
        }
        Ok(Self { mount: mount.to_owned() })
    }

    fn detach(&self) {
        for attempt in 0..10 {
            let status = Command::new("/usr/bin/hdiutil")
                .args(["detach", "-quiet"])
                .arg(&self.mount)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if status.is_ok_and(|value| value.success()) {
                return;
            }
            if attempt != 9 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        let _ = Command::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.mount)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for MountedContainer {
    fn drop(&mut self) {
        self.detach();
    }
}

pub(super) struct ExtractedContainer {
    _reservations: Vec<ResourceReservation>,
}

impl ExtractedContainer {
    fn extract(
        image: &Path,
        root: &Path,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let file = std::fs::File::open(image).map_err(|_| unavailable("containerArchive"))?;
        let mut archive =
            zip::ZipArchive::new(file).map_err(|_| unavailable("containerArchive"))?;
        if archive.is_empty() || archive.len() > MAX_ZIP_ENTRIES {
            return Err(unavailable("containerEntries"));
        }
        let mut total = 0_u64;
        let mut names = std::collections::BTreeSet::new();
        let mut reservations = Vec::new();
        reservations
            .try_reserve_exact(archive.len())
            .map_err(|_| unavailable("containerMemory"))?;
        for index in 0..archive.len() {
            context.checkpoint()?;
            let mut entry = archive.by_index(index).map_err(|_| unavailable("containerEntry"))?;
            let relative =
                entry.enclosed_name().ok_or_else(|| unavailable("containerPath"))?.clone();
            if !relative.starts_with("LibreOffice.app") || relative.as_os_str().is_empty() {
                return Err(unavailable("containerPath"));
            }
            if !names.insert(relative.clone()) {
                return Err(unavailable("containerPath"));
            }
            let destination = root.join(&relative);
            let mode = entry.unix_mode().unwrap_or(0o100_644);
            if entry.is_dir() {
                create_directory(&destination)?;
                continue;
            }
            if entry.size() > MAX_ZIP_ENTRY_BYTES
                || entry.compressed_size() > 0
                    && entry.size() > entry.compressed_size().saturating_mul(MAX_ZIP_RATIO)
            {
                return Err(unavailable("containerSize"));
            }
            total = total
                .checked_add(entry.size())
                .filter(|bytes| *bytes <= MAX_ZIP_EXPANDED_BYTES)
                .ok_or_else(|| unavailable("containerSize"))?;
            if let Some(parent) = destination.parent() {
                create_directory(parent)?;
            }
            let file_type = mode & 0o170_000;
            if file_type == 0o120_000 {
                let mut target = String::new();
                entry
                    .take(4_097)
                    .read_to_string(&mut target)
                    .map_err(|_| unavailable("containerLink"))?;
                if target.is_empty()
                    || target.len() > 4_096
                    || !safe_link_target(&relative, &target)
                {
                    return Err(unavailable("containerLink"));
                }
                symlink(target, &destination).map_err(|_| unavailable("containerLink"))?;
                continue;
            }
            if !matches!(file_type, 0 | 0o100_000) {
                return Err(unavailable("containerType"));
            }
            let expected_bytes = entry.size();
            let reservation = context.reserve_temporary(expected_bytes)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(if mode & 0o111 == 0 { 0o400 } else { 0o500 })
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&destination)
                .map_err(|_| unavailable("containerWrite"))?;
            std::io::copy(&mut entry, &mut output).map_err(|_| unavailable("containerWrite"))?;
            output.sync_all().map_err(|_| unavailable("containerWrite"))?;
            if output.metadata().map_err(|_| unavailable("containerWrite"))?.len() != expected_bytes
            {
                return Err(unavailable("containerSize"));
            }
            reservations.push(reservation);
        }
        make_directories_read_only(root)?;
        Ok(Self { _reservations: reservations })
    }
}

fn create_directory(path: &Path) -> Result<(), ConversionError> {
    if path.is_dir() {
        return Ok(());
    }
    std::fs::create_dir(path).map_err(|_| unavailable("containerDirectory"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| unavailable("containerDirectory"))
}

fn make_directories_read_only(root: &Path) -> Result<(), ConversionError> {
    let mut directories = vec![root.to_owned()];
    let mut index = 0_usize;
    while index < directories.len() {
        let directory = directories[index].clone();
        index += 1;
        for entry in std::fs::read_dir(&directory).map_err(|_| unavailable("containerDirectory"))? {
            let entry = entry.map_err(|_| unavailable("containerDirectory"))?;
            if entry.file_type().map_err(|_| unavailable("containerDirectory"))?.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    for directory in directories.into_iter().rev() {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o500))
            .map_err(|_| unavailable("containerDirectory"))?;
    }
    Ok(())
}

fn safe_link_target(link: &Path, target: &str) -> bool {
    let target = Path::new(target);
    if target.is_absolute() || target.components().count() > 64 {
        return false;
    }
    let mut depth = link
        .parent()
        .map_or(0_i32, |parent| i32::try_from(parent.components().count()).unwrap_or(i32::MAX));
    for component in target.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => depth += 1,
            std::path::Component::ParentDir => depth -= 1,
            _ => return false,
        }
        if depth < 1 {
            return false;
        }
    }
    true
}

fn unavailable(detail: &str) -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "legacy-office-worker".into(),
        detail: detail.into(),
    }
}
