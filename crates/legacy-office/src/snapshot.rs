use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use sha2::{Digest as _, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

const COPY_BUFFER_BYTES: usize = 16 * 1024;

pub(crate) struct VerifiedSnapshot {
    pub path: PathBuf,
    pub _file: File,
}

pub(crate) struct VerifiedTree {
    root: PathBuf,
    entries: Vec<(String, VerifiedSnapshot)>,
    _memory: ResourceReservation,
}

impl VerifiedTree {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn path(&self, relative: &str) -> Option<&Path> {
        self.entries
            .iter()
            .find_map(|(name, snapshot)| (name == relative).then_some(snapshot.path.as_path()))
    }
}

impl Drop for VerifiedTree {
    fn drop(&mut self) {
        // The tree is immutable for the entire worker lifetime. Restore only
        // directory owner-write after the worker has been reaped so the
        // enclosing private TempDir can remove every file without residue.
        make_tree_writable(&self.root);
    }
}

pub(crate) fn copy_tree(
    files: &[crate::authority::VerifiedRuntimeFile],
    destination: &Path,
    context: &ExecutionContext,
) -> Result<VerifiedTree, ConversionError> {
    context.checkpoint()?;
    let bookkeeping = files.iter().try_fold(0_u64, |total, file| {
        let path = u64::try_from(file.relative.len()).map_err(|_| invalid())?;
        let entry = u64::try_from(std::mem::size_of::<(String, VerifiedSnapshot)>())
            .map_err(|_| invalid())?;
        total
            .checked_add(path.checked_mul(3).ok_or_else(invalid)?)
            .and_then(|value| value.checked_add(entry))
            .ok_or_else(invalid)
    })?;
    let memory = context.reserve_memory(bookkeeping)?;
    std::fs::create_dir(destination).map_err(|_| invalid())?;
    set_directory_permissions(destination, true).map_err(|()| invalid())?;
    let root = destination.canonicalize().map_err(|_| invalid())?;
    let mut cleanup = BuildCleanup { root: root.clone(), armed: true };
    let mut entries = Vec::new();
    entries.try_reserve_exact(files.len()).map_err(|_| invalid())?;
    let mut directories = std::collections::BTreeSet::new();
    for entry in files {
        context.checkpoint()?;
        let relative = Path::new(&entry.relative);
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let mut current = PathBuf::new();
        for component in parent.components() {
            current.push(component);
            if directories.insert(current.clone()) {
                let directory = root.join(&current);
                std::fs::create_dir(&directory).map_err(|_| invalid())?;
                set_directory_permissions(&directory, true).map_err(|()| invalid())?;
            }
        }
        let snapshot = copy_verified(
            &entry.path,
            entry.bytes,
            &entry.sha256,
            &root.join(relative),
            entry.executable,
            context,
        )?;
        entries.push((entry.relative.clone(), snapshot));
    }
    for directory in directories.iter().rev() {
        set_directory_permissions(&root.join(directory), false).map_err(|()| invalid())?;
        sync_directory(&root.join(directory)).map_err(|()| invalid())?;
    }
    set_directory_permissions(&root, false).map_err(|()| invalid())?;
    sync_directory(&root).map_err(|()| invalid())?;
    cleanup.armed = false;
    Ok(VerifiedTree { root, entries, _memory: memory })
}

struct BuildCleanup {
    root: PathBuf,
    armed: bool,
}

impl Drop for BuildCleanup {
    fn drop(&mut self) {
        if self.armed {
            make_tree_writable(&self.root);
        }
    }
}

fn make_tree_writable(root: &Path) {
    let _ = set_directory_permissions(root, true);
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            make_tree_writable(&path);
        }
    }
}
pub(crate) fn copy_verified(
    source: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
    destination: &Path,
    executable: bool,
    context: &ExecutionContext,
) -> Result<VerifiedSnapshot, ConversionError> {
    context.checkpoint()?;
    #[cfg(windows)]
    let _ = executable;
    let source_metadata = std::fs::symlink_metadata(source).map_err(|_| invalid())?;
    if source_metadata.file_type().is_symlink()
        || !source_metadata.is_file()
        || source_metadata.len() != expected_bytes
    {
        return Err(invalid());
    }
    let mut source_options = OpenOptions::new();
    source_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        source_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut source_file = source_options.open(source).map_err(|_| invalid())?;
    let opened_source = source_file.metadata().map_err(|_| invalid())?;
    if !same_file(&source_metadata, &opened_source) {
        return Err(invalid());
    }

    let mut destination_options = OpenOptions::new();
    destination_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        destination_options.mode(if executable { 0o500 } else { 0o400 });
    }
    let mut destination_file = destination_options.open(destination).map_err(|_| invalid())?;
    let mut hash = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        context.checkpoint()?;
        let count = source_file.read(&mut buffer).map_err(|_| invalid())?;
        if count == 0 {
            break;
        }
        copied =
            copied.checked_add(u64::try_from(count).map_err(|_| invalid())?).ok_or_else(invalid)?;
        if copied > expected_bytes {
            return Err(invalid());
        }
        hash.update(&buffer[..count]);
        destination_file.write_all(&buffer[..count]).map_err(|_| invalid())?;
    }
    if copied != expected_bytes || format!("{:x}", hash.finalize()) != expected_sha256 {
        return Err(invalid());
    }
    destination_file.sync_all().map_err(|_| invalid())?;
    let closed_source = source_file.metadata().map_err(|_| invalid())?;
    if !same_file(&opened_source, &closed_source) {
        return Err(invalid());
    }
    drop(destination_file);

    let permissions = std::fs::metadata(destination).map_err(|_| invalid())?.permissions();
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
    std::fs::set_permissions(destination, permissions).map_err(|_| invalid())?;

    let mut opened_options = OpenOptions::new();
    opened_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opened_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = opened_options.open(destination).map_err(|_| invalid())?;
    let metadata = file.metadata().map_err(|_| invalid())?;
    if metadata.len() != expected_bytes {
        return Err(invalid());
    }
    let path = destination.canonicalize().map_err(|_| invalid())?;
    Ok(VerifiedSnapshot { path, _file: file })
}

fn invalid() -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "legacy-office-worker".into(),
        detail: "workerIdentity".into(),
    }
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino() && left.len() == right.len()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::authority::VerifiedRuntimeFile;
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
        let context = into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        for index in 0..128 {
            let destination = root.path().join(format!("snapshot-{index}"));
            match copy_verified(
                &source,
                u64::try_from(valid.len()).unwrap(),
                &expected,
                &destination,
                false,
                &context,
            ) {
                Ok(snapshot) => {
                    successes += 1;
                    assert_eq!(std::fs::read(snapshot.path).unwrap(), valid);
                }
                Err(_) => {
                    let _ = std::fs::remove_file(destination);
                }
            }
        }
        stopped.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        if successes == 0 {
            std::fs::write(&source, &valid).unwrap();
            let snapshot = copy_verified(
                &source,
                u64::try_from(valid.len()).unwrap(),
                &expected,
                &root.path().join("snapshot-final"),
                false,
                &context,
            )
            .unwrap();
            assert_eq!(std::fs::read(snapshot.path).unwrap(), valid);
            successes += 1;
        }
        assert!(successes > 0);
    }

    #[test]
    fn whole_runtime_tree_copies_only_authority_hashed_dynamic_plugin_inode() {
        let root = tempfile::tempdir().unwrap();
        let swap = tempfile::tempdir().unwrap();
        let source = root.path().join("dynamic-plugin.bin");
        let stable = root.path().join("worker");
        let valid = b"authority-verified-dynamic-plugin".to_vec();
        let canary = b"constructor-canary-plugin-content".to_vec();
        std::fs::write(&source, &valid).unwrap();
        std::fs::write(&stable, b"worker").unwrap();
        let files = vec![
            VerifiedRuntimeFile {
                relative: "runtime/plugins/dynamic-plugin.bin".into(),
                path: source.clone(),
                bytes: u64::try_from(valid.len()).unwrap(),
                sha256: format!("{:x}", Sha256::digest(&valid)),
                executable: false,
            },
            VerifiedRuntimeFile {
                relative: "worker".into(),
                path: stable,
                bytes: 6,
                sha256: format!("{:x}", Sha256::digest(b"worker")),
                executable: true,
            },
        ];
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker_source = source.clone();
        let swap_root = swap.path().to_owned();
        let worker_valid = valid.clone();
        let worker = std::thread::spawn(move || {
            let mut count = 0_u64;
            while !worker_stopped.load(Ordering::Relaxed) {
                let candidate = swap_root.join(format!("plugin-{count}"));
                let bytes = if count.is_multiple_of(2) { &worker_valid } else { &canary };
                std::fs::write(&candidate, bytes).unwrap();
                std::fs::rename(candidate, &worker_source).unwrap();
                count = count.wrapping_add(1);
            }
        });
        let context = into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let mut successes = 0;
        for index in 0..64 {
            let destination = root.path().join(format!("tree-{index}"));
            if let Ok(tree) = copy_tree(&files, &destination, &context) {
                successes += 1;
                assert_eq!(
                    std::fs::read(tree.path("runtime/plugins/dynamic-plugin.bin").unwrap())
                        .unwrap(),
                    valid
                );
            }
        }
        stopped.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        if successes == 0 {
            std::fs::write(&source, &valid).unwrap();
            let tree = copy_tree(&files, &root.path().join("tree-final"), &context).unwrap();
            assert_eq!(
                std::fs::read(tree.path("runtime/plugins/dynamic-plugin.bin").unwrap()).unwrap(),
                valid
            );
            successes += 1;
        }
        assert!(successes > 0);
    }

    #[test]
    fn runtime_tree_bookkeeping_fails_before_destination_creation() {
        let root = tempfile::tempdir().unwrap();
        let context = into_markdown_core::ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: 1,
                ..into_markdown_core::ResourceLimits::default()
            },
        );
        let destination = root.path().join("must-not-exist");
        let file = VerifiedRuntimeFile {
            relative: "runtime/plugin.bin".into(),
            path: root.path().join("missing-is-never-opened"),
            bytes: 1,
            sha256: "0".repeat(64),
            executable: false,
        };
        assert!(copy_tree(&[file], &destination, &context).is_err());
        assert!(!destination.exists());
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ()> {
    File::open(path).and_then(|directory| directory.sync_all()).map_err(|_| ())
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> Result<(), ()> {
    path.is_dir().then_some(()).ok_or(())
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
