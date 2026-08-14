use sha2::{Digest as _, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

const COPY_BUFFER_BYTES: usize = 16 * 1024;

pub(crate) struct VerifiedSnapshot {
    pub path: PathBuf,
    pub file: File,
}

pub(crate) struct VerifiedTree {
    entries: Vec<(String, VerifiedSnapshot)>,
}

impl VerifiedTree {
    pub(crate) fn path(&self, relative: &str) -> Option<&Path> {
        self.entries
            .iter()
            .find_map(|(name, snapshot)| (name == relative).then_some(snapshot.path.as_path()))
    }

    pub(crate) fn into_files(self) -> Vec<File> {
        self.entries.into_iter().map(|(_, snapshot)| snapshot.file).collect()
    }
}

pub(crate) fn copy_tree(
    files: &[crate::authority::VerifiedRuntimeFile],
    destination: &Path,
) -> Result<VerifiedTree, ()> {
    std::fs::create_dir(destination).map_err(|_| ())?;
    set_directory_permissions(destination, true)?;
    let root = destination.canonicalize().map_err(|_| ())?;
    let mut entries = Vec::new();
    entries.try_reserve_exact(files.len()).map_err(|_| ())?;
    let mut directories = std::collections::BTreeSet::new();
    for entry in files {
        let relative = Path::new(&entry.relative);
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let mut current = PathBuf::new();
        for component in parent.components() {
            current.push(component);
            if directories.insert(current.clone()) {
                let directory = root.join(&current);
                std::fs::create_dir(&directory).map_err(|_| ())?;
                set_directory_permissions(&directory, true)?;
            }
        }
        let snapshot = copy_verified(
            &entry.path,
            entry.bytes,
            &entry.sha256,
            &root.join(relative),
            entry.executable,
        )?;
        entries.push((entry.relative.clone(), snapshot));
    }
    for directory in directories.iter().rev() {
        set_directory_permissions(&root.join(directory), false)?;
    }
    set_directory_permissions(&root, false)?;
    Ok(VerifiedTree { entries })
}

pub(crate) fn copy_verified(
    source: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    destination: &Path,
    executable: bool,
) -> Result<VerifiedSnapshot, ()> {
    #[cfg(windows)]
    let _ = executable;
    let source_metadata = std::fs::symlink_metadata(source).map_err(|_| ())?;
    if source_metadata.file_type().is_symlink()
        || !source_metadata.is_file()
        || source_metadata.len() != expected_bytes
    {
        return Err(());
    }
    let mut source_options = OpenOptions::new();
    source_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        source_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut source_file = source_options.open(source).map_err(|_| ())?;
    let opened_source = source_file.metadata().map_err(|_| ())?;
    if !same_file(&source_metadata, &opened_source) {
        return Err(());
    }

    let mut destination_options = OpenOptions::new();
    destination_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        destination_options.mode(if executable { 0o500 } else { 0o400 });
    }
    let mut destination_file = destination_options.open(destination).map_err(|_| ())?;
    let mut hash = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = source_file.read(&mut buffer).map_err(|_| ())?;
        if count == 0 {
            break;
        }
        copied = copied.checked_add(u64::try_from(count).map_err(|_| ())?).ok_or(())?;
        if copied > expected_bytes {
            return Err(());
        }
        hash.update(&buffer[..count]);
        destination_file.write_all(&buffer[..count]).map_err(|_| ())?;
    }
    if copied != expected_bytes || format!("{:x}", hash.finalize()) != expected_sha256 {
        return Err(());
    }
    destination_file.sync_all().map_err(|_| ())?;
    let closed_source = source_file.metadata().map_err(|_| ())?;
    if !same_file(&opened_source, &closed_source) {
        return Err(());
    }
    drop(destination_file);

    let permissions = std::fs::metadata(destination).map_err(|_| ())?.permissions();
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = permissions;
        permissions.set_mode(if executable { 0o500 } else { 0o400 });
        permissions
    };
    #[cfg(windows)]
    let permissions = {
        let mut permissions = permissions;
        permissions.set_readonly(true);
        permissions
    };
    std::fs::set_permissions(destination, permissions).map_err(|_| ())?;

    let mut opened_options = OpenOptions::new();
    opened_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opened_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = opened_options.open(destination).map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if metadata.len() != expected_bytes {
        return Err(());
    }
    let path = destination.canonicalize().map_err(|_| ())?;
    Ok(VerifiedSnapshot { path, file })
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino() && left.len() == right.len()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn high_frequency_source_swaps_never_enter_verified_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let swap = tempfile::tempdir().unwrap();
        let source = root.path().join("kit.dylib");
        let valid = b"authority-verified-kit-bytes".to_vec();
        let canary = b"constructor-canary-must-never-load".to_vec();
        std::fs::write(&source, &valid).unwrap();
        let expected = format!("{:x}", Sha256::digest(&valid));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker_source = source.clone();
        let swap_root = swap.path().to_owned();
        let worker_valid = valid.clone();
        let worker = std::thread::spawn(move || {
            let mut count = 0_u64;
            while !worker_stopped.load(Ordering::Relaxed) {
                let candidate = swap_root.join(format!("candidate-{count}"));
                let bytes = if count.is_multiple_of(2) { &worker_valid } else { &canary };
                std::fs::write(&candidate, bytes).unwrap();
                std::fs::rename(&candidate, &worker_source).unwrap();
                count = count.wrapping_add(1);
            }
        });
        let mut successes = 0_usize;
        for index in 0..128 {
            let destination = root.path().join(format!("snapshot-{index}"));
            match copy_verified(
                &source,
                u64::try_from(valid.len()).unwrap(),
                &expected,
                &destination,
                false,
            ) {
                Ok(snapshot) => {
                    successes += 1;
                    assert_eq!(std::fs::read(snapshot.path).unwrap(), valid);
                }
                Err(()) => {
                    let _ = std::fs::remove_file(destination);
                }
            }
        }
        stopped.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(successes > 0);
    }
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path, writable: bool) -> Result<(), ()> {
    let permissions = std::fs::metadata(path).map_err(|_| ())?.permissions();
    let permissions = {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = permissions;
        permissions.set_mode(if writable { 0o700 } else { 0o500 });
        permissions
    };
    std::fs::set_permissions(path, permissions).map_err(|_| ())
}

#[cfg(windows)]
fn set_directory_permissions(path: &Path, writable: bool) -> Result<(), ()> {
    let _ = writable;
    path.is_dir().then_some(()).ok_or(())
}

#[cfg(not(unix))]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}
