//! Crash-recoverable output-set transactions.

#![cfg_attr(not(unix), allow(dead_code, unused_imports, unused_mut, unused_variables))]
#![cfg_attr(not(unix), allow(unreachable_code))]

use crate::error::{CliError, ExitClass};
use into_markdown::{ExecutionContext, ResourceReservation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use cap_std::fs::MetadataExt as _;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigExpectedAuthority {
    pub identity: Option<(u64, u64)>,
    pub sha256: Option<String>,
}

#[cfg(unix)]
use std::os::fd::OwnedFd;

const JOURNAL_SIGNATURE: &str = "into-markdown-output-transaction";
const JOURNAL_VERSION: u32 = 1;
const TRANSACTION_PREFIX: &str = ".into-md-txn-01-";
const INITIAL_PREFIX: &str = ".into-md-init-01-";
const CLEANUP_PREFIX: &str = ".into-md-clean-01-";
#[cfg(windows)]
const EXTERNAL_LOCK_PREFIX: &str = ".into-md-lock-01-";
const PARENT_MARKER_PREFIX: &str = "parent-";
const PARENT_LEASE_NAME: &str = ".into-md-output-parent-lease-01";
const REGISTRY_NAME: &str = ".into-md-output-transactions-01";
const MAX_RECOVERY_TRANSACTIONS: usize = 128;
const MAX_RECOVERY_RETRIES: usize = 8;
const MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 16_384;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JOURNAL_ENTRIES: usize = 100_001;
const MAX_PATH_UNITS: usize = 32_768;

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);
static ACTIVE_TRANSACTIONS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

/// Atomically replace one configuration file through an authenticated parent
/// directory handle.
#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn atomic_replace_config(
    path: &Path,
    bytes: &[u8],
    replace: bool,
) -> Result<(), CliError> {
    atomic_replace_config_inner_with_barriers(
        path,
        bytes,
        replace,
        |_, _, _| Ok(()),
        |_, _, _| Ok(()),
    )
}

#[cfg(test)]
mod windows_config_tests {
    use super::*;
    use std::process::Command;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn names() -> (String, String, String) {
        (
            format!(".into-md-config-{NONCE}.next"),
            format!(".into-md-config-{NONCE}.previous"),
            format!(".into-md-config-{NONCE}.journal"),
        )
    }

    fn open(root: &Path) -> cap_std::fs::Dir {
        cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority()).unwrap()
    }

    fn seed_journal(
        directory: &cap_std::fs::Dir,
        original: Option<(u64, u64)>,
        new: &[u8],
    ) -> (String, String, String) {
        let (temporary, backup, journal_name) = names();
        let journal = WindowsConfigJournal {
            schema_version: 1,
            target: "config.toml".to_owned(),
            temporary: temporary.clone(),
            backup: backup.clone(),
            original,
            original_sha256: cap_config_digest(directory, OsStr::new("config.toml")).unwrap(),
            new_sha256: format!("{:x}", Sha256::digest(new)),
            phase: WindowsConfigPhase::Prepared,
        };
        write_windows_config_journal(directory, OsStr::new(&journal_name), &journal).unwrap();
        (temporary, backup, journal_name)
    }

    #[test]
    fn config_replace_crash_child() {
        let Some(root) = std::env::var_os("INTO_MD_CONFIG_CRASH_ROOT") else { return };
        let directory = open(Path::new(&root));
        let target = std::env::var_os("INTO_MD_CONFIG_CRASH_TARGET")
            .unwrap_or_else(|| OsString::from("config.toml"));
        let bytes =
            std::env::var("INTO_MD_CONFIG_CRASH_CONTENT").unwrap_or_else(|_| "new".to_owned());
        atomic_replace_config_in_dir(&directory, &target, bytes.as_bytes(), true, None).unwrap();
    }

    #[test]
    fn config_replace_recovers_every_rename_phase_in_a_new_process() {
        for phase in ["journal", "backup", "target"] {
            let temporary = tempfile::tempdir().unwrap();
            fs::write(temporary.path().join("config.toml"), b"old").unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "transaction::windows_config_tests::config_replace_crash_child",
                    "--nocapture",
                ])
                .env("INTO_MD_CONFIG_CRASH_ROOT", temporary.path())
                .env("INTO_MD_CONFIG_CRASH_POINT", phase)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(86), "phase {phase}");
            let directory = open(temporary.path());
            recover_windows_config_transaction(&directory, OsStr::new("config.toml")).unwrap();
            assert_eq!(fs::read(temporary.path().join("config.toml")).unwrap(), b"new");
            assert!(fs::read_dir(temporary.path()).unwrap().all(|entry| {
                !entry.unwrap().file_name().to_string_lossy().starts_with(".into-md-config-")
            }));
        }
    }

    #[test]
    fn config_recovery_rejects_ambiguous_and_forged_journals() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = open(temporary.path());
        seed_journal(&directory, None, b"new");
        let forged = format!(".into-md-config-{}.journal", "1".repeat(32));
        directory.write(&forged, b"{}").unwrap();
        let error =
            recover_windows_config_transaction(&directory, OsStr::new("config.toml")).unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");

        directory.remove_file(&forged).unwrap();
        let (_, _, journal) = names();
        directory.rename(&journal, &directory, format!("{journal}.extra.journal")).unwrap();
        let error =
            recover_windows_config_transaction(&directory, OsStr::new("config.toml")).unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
    }

    #[test]
    fn config_recovery_rejects_hardlinked_authority_without_touching_sentinel() {
        for position in ["target", "temporary", "backup"] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path();
            let sentinel = root.join("sentinel");
            fs::write(&sentinel, b"sentinel").unwrap();
            let directory = open(root);
            let (next, backup, _) = seed_journal(&directory, None, b"new");
            let attack = match position {
                "target" => "config.toml",
                "temporary" => next.as_str(),
                _ => backup.as_str(),
            };
            fs::hard_link(&sentinel, root.join(attack)).unwrap();
            let error = recover_windows_config_transaction(&directory, OsStr::new("config.toml"))
                .unwrap_err();
            assert_eq!(error.code(), "transactionRecoveryFailed", "{position}");
            assert_eq!(fs::read(&sentinel).unwrap(), b"sentinel");
        }
    }

    #[test]
    fn config_bound_authority_rejects_mutation_at_publish_barrier() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("config.toml");
        fs::write(&target, b"old").unwrap();
        let directory = open(temporary.path());
        let expected = ConfigExpectedAuthority {
            identity: cap_config_identity(&directory, OsStr::new("config.toml")).unwrap(),
            sha256: cap_config_digest(&directory, OsStr::new("config.toml")).unwrap(),
        };
        directory.write(".test-config-mutate-before-publish", b"racer").unwrap();

        let error = atomic_replace_config_in_dir(
            &directory,
            OsStr::new("config.toml"),
            b"new",
            true,
            Some(&expected),
        )
        .unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert_eq!(fs::read(&target).unwrap(), b"racer");
    }

    #[test]
    fn config_cleanup_rename_barrier_preserves_replacement_sentinel() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("config.toml");
        fs::write(&target, b"old").unwrap();
        let directory = open(temporary.path());
        let identity = cap_config_identity(&directory, OsStr::new("config.toml")).unwrap().unwrap();
        let digest = cap_config_digest(&directory, OsStr::new("config.toml")).unwrap().unwrap();
        directory.write(".test-config-replace-before-cleanup", b"sentinel").unwrap();

        let error =
            remove_config_with_authority(&directory, OsStr::new("config.toml"), identity, &digest)
                .unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert_eq!(fs::read(&target).unwrap(), b"sentinel");
        assert_eq!(fs::read(temporary.path().join(".test-config-clean-held")).unwrap(), b"old");
    }

    #[test]
    fn config_no_replace_rename_preserves_raced_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = open(temporary.path());
        directory.write("source.next", b"candidate").unwrap();
        assert!(directory.symlink_metadata("config.toml").is_err());
        directory.write("config.toml", b"racer sentinel").unwrap();

        let error = config_rename_no_replace(
            &directory,
            OsStr::new("source.next"),
            OsStr::new("config.toml"),
        )
        .unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert_eq!(directory.read("config.toml").unwrap(), b"racer sentinel");
        assert_eq!(directory.read("source.next").unwrap(), b"candidate");
    }

    #[cfg(windows)]
    #[test]
    fn pinned_config_directory_blocks_parent_namespace_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("config");
        fs::create_dir(&parent).unwrap();
        let directory = open(&parent);
        directory.write("sentinel", b"inside pinned parent").unwrap();

        let moved = temporary.path().join("moved");
        assert!(fs::rename(&parent, &moved).is_err());
        assert_eq!(directory.read("sentinel").unwrap(), b"inside pinned parent");
        assert!(!moved.exists());
    }
}

#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
fn atomic_replace_config_inner(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    before_commit: impl FnOnce(&SafeDir, &OsStr, &OsStr) -> Result<(), CliError>,
) -> Result<(), CliError> {
    atomic_replace_config_inner_with_barriers(path, bytes, replace, before_commit, |_, _, _| Ok(()))
}

#[cfg(unix)]
fn atomic_replace_config_inner_with_barriers(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    before_final_check: impl FnOnce(&SafeDir, &OsStr, &OsStr) -> Result<(), CliError>,
    after_final_check: impl FnOnce(&SafeDir, &OsStr, &OsStr) -> Result<(), CliError>,
) -> Result<(), CliError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let parent_path = absolute.parent().ok_or_else(|| recovery_error("config has no parent"))?;
    let name = absolute.file_name().ok_or_else(|| recovery_error("config has no file name"))?;
    validate_single_name(name)?;
    let parent = SafeDir::open_or_create_absolute(parent_path)?;
    parent.verify_namespace()?;
    let expected = parent.inspect_regular(name)?;
    if expected.is_some() && !replace {
        return Err(CliError::config(format!("path already exists: {}", absolute.display())));
    }
    let existing_mode = if expected.is_some() {
        let file = parent.open_regular(name)?;
        let metadata = file.metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(CliError::config(format!(
                "configuration file is not owned by the current user: {}",
                absolute.display()
            )));
        }
        Some(metadata.permissions().mode() & 0o777)
    } else {
        None
    };
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let sequence = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary_name =
        OsString::from(format!(".into-md-config-{}-{nonce}-{sequence}.tmp", std::process::id()));
    let mut temporary = parent.create_regular(&temporary_name)?;
    let result = (|| {
        temporary.write_all(bytes)?;
        if let Some(mode) = existing_mode {
            temporary.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        temporary.sync_all()?;
        let temporary_identity = file_identity(&temporary)?;
        before_final_check(&parent, name, &temporary_name)?;
        parent.verify_namespace()?;
        verify_name_identity(&parent, name, expected.as_ref())?;
        verify_name_identity(&parent, &temporary_name, Some(&temporary_identity))?;
        after_final_check(&parent, name, &temporary_name)?;
        // Re-authenticate sources after the deterministic race barrier. The
        // destination itself is protected by the atomic primitive below: an
        // absent target can never be overwritten, while replacement exchanges
        // the old inode so it can be verified and restored losslessly.
        parent.verify_namespace()?;
        verify_name_identity(&parent, &temporary_name, Some(&temporary_identity))?;
        publish_config(&parent, name, &temporary_name, expected.as_ref(), &temporary_identity)?;
        // Persist the exchange before discarding the displaced old target. If
        // this fsync fails, the old inode remains recoverable under the private
        // temporary name rather than being prematurely destroyed.
        parent.sync()?;
        if let Some(expected) = expected.as_ref() {
            unlink_name_if_identity(&parent, &temporary_name, expected)?;
            parent.sync()?;
        }
        parent.verify_namespace()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlink_name_if_identity(&parent, &temporary_name, &file_identity(&temporary)?);
        let _ = parent.sync();
    }
    result
}

#[cfg(unix)]
fn publish_config(
    parent: &SafeDir,
    name: &OsStr,
    temporary_name: &OsStr,
    expected: Option<&FileIdentity>,
    temporary_identity: &FileIdentity,
) -> Result<(), CliError> {
    #[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
    {
        if let Some(expected) = expected {
            rustix::fs::renameat_with(
                &parent.fd,
                temporary_name,
                &parent.fd,
                name,
                rustix::fs::RenameFlags::EXCHANGE,
            )?;
            let installed = verify_name_identity(parent, name, Some(temporary_identity));
            let displaced = verify_name_identity(parent, temporary_name, Some(expected));
            if let Err(error) = installed.and(displaced) {
                // Both namespace entries still exist after EXCHANGE. Put them
                // back before surfacing any identity mismatch; no old target is
                // discarded on the error path.
                rustix::fs::renameat_with(
                    &parent.fd,
                    temporary_name,
                    &parent.fd,
                    name,
                    rustix::fs::RenameFlags::EXCHANGE,
                )?;
                return Err(error);
            }
        } else {
            rustix::fs::renameat_with(
                &parent.fd,
                temporary_name,
                &parent.fd,
                name,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
            verify_name_identity(parent, name, Some(temporary_identity))?;
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
    Err(transaction_platform_unavailable())
}

#[cfg(unix)]
fn unlink_name_if_identity(
    directory: &SafeDir,
    name: &OsStr,
    expected: &FileIdentity,
) -> Result<(), CliError> {
    if directory.inspect_regular(name)?.as_ref() != Some(expected) {
        return Err(CliError::new(
            ExitClass::Io,
            "outputIdentityChanged",
            format!("refusing to unlink changed file: {}", directory.path.join(name).display()),
        ));
    }
    rustix::fs::unlinkat(&directory.fd, name, rustix::fs::AtFlags::empty())?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn atomic_replace_config_in_dir(
    parent: &cap_std::fs::Dir,
    name: &OsStr,
    bytes: &[u8],
    replace: bool,
    bound_expected: Option<&ConfigExpectedAuthority>,
) -> Result<(), CliError> {
    use cap_std::fs::OpenOptionsExt as _;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    validate_single_name(name)?;
    recover_windows_config_transaction(parent, name)?;
    let expected_digest = cap_config_digest(parent, name)?;
    let expected = match parent.symlink_metadata(name) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(recovery_error("config file identity rejected"));
            }
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            let file = parent.open_with(name, &options)?.into_std();
            let information = winapi_util::file::information(&file)?;
            if information.file_attributes() & 0x400 != 0 || information.number_of_links() != 1 {
                return Err(recovery_error("config file identity rejected"));
            }
            if !replace {
                return Err(CliError::config("configuration path already exists"));
            }
            Some((information.volume_serial_number(), information.file_index()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(bound) = bound_expected
        && (bound.identity != expected || bound.sha256 != expected_digest)
    {
        return Err(recovery_error("config changed after locked read"));
    }
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| CliError::new(ExitClass::Internal, "internal", "config nonce unavailable"))?;
    let nonce = nonce.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let temporary = OsString::from(format!(".into-md-config-{nonce}.next"));
    let backup = OsString::from(format!(".into-md-config-{nonce}.previous"));
    let journal = OsString::from(format!(".into-md-config-{nonce}.journal"));
    let mut options = cap_std::fs::OpenOptions::new();
    options.create_new(true).write(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let mut output = parent.open_with(&temporary, &options)?.into_std();
    let result = (|| {
        output.write_all(bytes)?;
        output.sync_all()?;
        let authority = WindowsConfigJournal {
            schema_version: 1,
            target: name
                .to_str()
                .ok_or_else(|| recovery_error("config name is not portable"))?
                .to_owned(),
            temporary: temporary
                .to_str()
                .ok_or_else(|| recovery_error("temporary name is not portable"))?
                .to_owned(),
            backup: backup
                .to_str()
                .ok_or_else(|| recovery_error("backup name is not portable"))?
                .to_owned(),
            original: expected,
            original_sha256: expected_digest.clone(),
            new_sha256: format!("{:x}", Sha256::digest(bytes)),
            phase: WindowsConfigPhase::Prepared,
        };
        write_windows_config_journal(parent, &journal, &authority)?;
        windows_config_test_crash("journal");
        config_test_mutate_before_publish(parent, name)?;
        if let Some(expected) = expected {
            let mut current_options = cap_std::fs::OpenOptions::new();
            current_options.read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            let current = parent.open_with(name, &current_options)?.into_std();
            let information = winapi_util::file::information(&current)?;
            if (information.volume_serial_number(), information.file_index()) != expected
                || information.file_attributes() & 0x400 != 0
                || information.number_of_links() != 1
                || cap_config_digest(parent, name)? != expected_digest
            {
                return Err(recovery_error("config changed before replacement"));
            }
            parent.rename(name, parent, &backup)?;
            sync_cap_directory(parent)?;
            windows_config_test_crash("backup");
            if let Err(error) = parent.rename(&temporary, parent, name) {
                let _ = parent.rename(&backup, parent, name);
                return Err(error.into());
            }
            sync_cap_directory(parent)?;
            windows_config_test_crash("target");
            let _ = parent.remove_file(&backup);
        } else {
            if parent.symlink_metadata(name).is_ok() {
                return Err(recovery_error("config appeared before replacement"));
            }
            parent.rename(&temporary, parent, name)?;
            sync_cap_directory(parent)?;
            windows_config_test_crash("target");
        }
        parent.remove_file(&journal)?;
        sync_cap_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = recover_windows_config_transaction(parent, name);
        if cap_config_digest(parent, name)? == Some(format!("{:x}", Sha256::digest(bytes))) {
            return Ok(());
        }
    }
    result
}

#[cfg(not(windows))]
pub(crate) fn atomic_replace_config_in_dir(
    parent: &cap_std::fs::Dir,
    name: &OsStr,
    bytes: &[u8],
    replace: bool,
    bound_expected: Option<&ConfigExpectedAuthority>,
) -> Result<(), CliError> {
    use cap_std::fs::OpenOptionsExt as _;
    validate_single_name(name)?;
    recover_windows_config_transaction(parent, name)?;
    let expected = match parent.symlink_metadata(name) {
        Ok(metadata) => {
            if !replace {
                return Err(recovery_error("configuration already exists"));
            }
            if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.nlink() != 1 {
                return Err(recovery_error("config file identity rejected"));
            }
            Some((metadata.dev(), metadata.ino(), cap_config_digest(parent, name)?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(bound) = bound_expected {
        let identity = expected.as_ref().map(|(device, inode, _)| (*device, *inode));
        let sha256 = expected.as_ref().and_then(|(_, _, digest)| digest.clone());
        if bound.identity != identity || bound.sha256 != sha256 {
            return Err(recovery_error("config changed after locked read"));
        }
    }
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| CliError::new(ExitClass::Internal, "internal", "config nonce unavailable"))?;
    let nonce = nonce.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let temporary = OsString::from(format!(".into-md-config-{nonce}.next"));
    let backup = OsString::from(format!(".into-md-config-{nonce}.previous"));
    let journal = OsString::from(format!(".into-md-config-{nonce}.journal"));
    let mut options = cap_std::fs::OpenOptions::new();
    options.create_new(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut output = parent.open_with(&temporary, &options)?.into_std();
    let result = (|| {
        output.write_all(bytes)?;
        output.sync_all()?;
        drop(output);
        let authority = WindowsConfigJournal {
            schema_version: 1,
            target: name
                .to_str()
                .ok_or_else(|| recovery_error("config name is not portable"))?
                .to_owned(),
            temporary: temporary.to_string_lossy().into_owned(),
            backup: backup.to_string_lossy().into_owned(),
            original: expected.as_ref().map(|(device, inode, _)| (*device, *inode)),
            original_sha256: expected.as_ref().and_then(|(_, _, digest)| digest.clone()),
            new_sha256: format!("{:x}", Sha256::digest(bytes)),
            phase: WindowsConfigPhase::Prepared,
        };
        write_windows_config_journal(parent, &journal, &authority)?;
        windows_config_test_crash("journal");
        config_test_mutate_before_publish(parent, name)?;
        match expected.as_ref() {
            Some((device, inode, digest)) => {
                let metadata = parent.symlink_metadata(name)?;
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.nlink() != 1
                    || (metadata.dev(), metadata.ino()) != (*device, *inode)
                    || cap_config_digest(parent, name)? != *digest
                {
                    return Err(recovery_error("config changed before replacement"));
                }
                rustix::fs::renameat_with(
                    parent,
                    name,
                    parent,
                    &backup,
                    rustix::fs::RenameFlags::NOREPLACE,
                )?;
                sync_cap_directory(parent)?;
                windows_config_test_crash("backup");
                let moved = parent.symlink_metadata(&backup)?;
                if (moved.dev(), moved.ino()) != (*device, *inode)
                    || cap_config_digest(parent, &backup)? != *digest
                {
                    let _ = rustix::fs::renameat_with(
                        parent,
                        &backup,
                        parent,
                        name,
                        rustix::fs::RenameFlags::NOREPLACE,
                    );
                    return Err(recovery_error("config changed at replacement barrier"));
                }
            }
            None if parent.symlink_metadata(name).is_ok() => {
                return Err(recovery_error("config appeared before replacement"));
            }
            None => {}
        }
        if let Err(error) = rustix::fs::renameat_with(
            parent,
            &temporary,
            parent,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            if expected.is_some() {
                let _ = rustix::fs::renameat_with(
                    parent,
                    &backup,
                    parent,
                    name,
                    rustix::fs::RenameFlags::NOREPLACE,
                );
            }
            return Err(error.into());
        }
        sync_cap_directory(parent)?;
        windows_config_test_crash("target");
        // The new target plus successful parent sync is the commit point.
        // Cleanup is restartable and cannot turn a committed mutation into an
        // ambiguous error return.
        let _ = recover_windows_config_transaction(parent, name);
        Ok(())
    })();
    if result.is_err() {
        let _ = recover_windows_config_transaction(parent, name);
        if cap_config_digest(parent, name)? == Some(format!("{:x}", Sha256::digest(bytes))) {
            return Ok(());
        }
    }
    result
}

#[cfg(test)]
fn config_test_mutate_before_publish(
    parent: &cap_std::fs::Dir,
    name: &OsStr,
) -> Result<(), CliError> {
    let marker = Path::new(".test-config-mutate-before-publish");
    let replacement = match parent.read(marker) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    parent.remove_file(marker)?;
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).truncate(true);
    let mut target = parent.open_with(name, &options)?.into_std();
    target.write_all(&replacement)?;
    target.sync_all()?;
    Ok(())
}

#[cfg(not(test))]
fn config_test_mutate_before_publish(
    _parent: &cap_std::fs::Dir,
    _name: &OsStr,
) -> Result<(), CliError> {
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WindowsConfigJournal {
    schema_version: u32,
    target: String,
    temporary: String,
    backup: String,
    original: Option<(u64, u64)>,
    original_sha256: Option<String>,
    new_sha256: String,
    phase: WindowsConfigPhase,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum WindowsConfigPhase {
    Prepared,
}

fn write_windows_config_journal(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
    journal: &WindowsConfigJournal,
) -> Result<(), CliError> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| recovery_error(&format!("serialize config journal: {error}")))?;
    let mut options = cap_std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = directory.open_with(name, &options)?.into_std();
    file.write_all(&bytes)?;
    file.sync_all()?;
    sync_cap_directory(directory)
}

fn recover_windows_config_transaction(
    directory: &cap_std::fs::Dir,
    target: &OsStr,
) -> Result<(), CliError> {
    let target_text =
        target.to_str().ok_or_else(|| recovery_error("config name is not portable"))?;
    let journals = directory
        .entries()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(".into-md-config-") && name.ends_with(".journal"))
        .collect::<Vec<_>>();
    if journals.len() > 1 {
        return Err(recovery_error("ambiguous config transaction journals"));
    }
    let Some(journal_name) = journals.first() else {
        return Ok(());
    };
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
    let file = directory.open_with(journal_name, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 || config_open_file_link_count(&file)? != 1
    {
        return Err(recovery_error("config transaction journal identity rejected"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(recovery_error("config transaction journal changed"));
    }
    let journal: WindowsConfigJournal = serde_json::from_slice(&bytes)
        .map_err(|_| recovery_error("config transaction journal is invalid"))?;
    if journal.schema_version != 1
        || journal.target != target_text
        || !valid_windows_config_transaction_name(journal_name, "journal")
        || !valid_windows_config_transaction_name(&journal.temporary, "next")
        || !valid_windows_config_transaction_name(&journal.backup, "previous")
        || journal.new_sha256.len() != 64
        || !journal
            .new_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || journal.original.is_some() != journal.original_sha256.is_some()
        || journal.original_sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64
                || !hash.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        || config_transaction_nonce(&journal.temporary) != config_transaction_nonce(&journal.backup)
        || config_transaction_nonce(journal_name) != config_transaction_nonce(&journal.temporary)
    {
        return Err(recovery_error("config transaction authority rejected"));
    }
    let target_digest = cap_config_digest(directory, target)?;
    let temporary_digest = cap_config_digest(directory, OsStr::new(&journal.temporary))?;
    let backup_identity = cap_config_identity(directory, OsStr::new(&journal.backup))?;
    let backup_digest = cap_config_digest(directory, OsStr::new(&journal.backup))?;
    let target_identity = cap_config_identity(directory, target)?;
    match (
        journal.original,
        journal.original_sha256.as_deref(),
        target_identity,
        backup_identity,
        target_digest,
        backup_digest,
        temporary_digest,
    ) {
        // Initial creation, before and after publishing the prepared file.
        (None, None, None, None, None, None, Some(temporary_hash))
            if temporary_hash == journal.new_sha256 =>
        {
            config_rename_no_replace(directory, OsStr::new(&journal.temporary), target)?;
            sync_cap_directory(directory)?;
        }
        (None, None, Some(_), None, Some(target_hash), None, None)
            if target_hash == journal.new_sha256 =>
        {
            // A recovered target is not considered committed until its parent
            // namespace has been made durable.
            sync_cap_directory(directory)?;
        }

        // Replacement, before moving the old file aside.
        (
            Some(original),
            Some(original_hash),
            Some(target_id),
            None,
            Some(target_hash),
            None,
            Some(temporary_hash),
        ) if target_id == original
            && target_hash == original_hash
            && temporary_hash == journal.new_sha256 =>
        {
            config_rename_no_replace(directory, target, OsStr::new(&journal.backup))?;
            sync_cap_directory(directory)?;
            config_rename_no_replace(directory, OsStr::new(&journal.temporary), target)?;
            sync_cap_directory(directory)?;
            remove_config_with_authority(
                directory,
                OsStr::new(&journal.backup),
                original,
                original_hash,
            )?;
            sync_cap_directory(directory)?;
        }
        // Replacement, after moving the old file aside but before publishing new.
        (
            Some(original),
            Some(original_hash),
            None,
            Some(backup_id),
            None,
            Some(backup_hash),
            Some(temporary_hash),
        ) if backup_id == original
            && backup_hash == original_hash
            && temporary_hash == journal.new_sha256 =>
        {
            config_rename_no_replace(directory, OsStr::new(&journal.temporary), target)?;
            sync_cap_directory(directory)?;
            remove_config_with_authority(
                directory,
                OsStr::new(&journal.backup),
                original,
                original_hash,
            )?;
            sync_cap_directory(directory)?;
        }
        // Replacement committed; only the exact old object may be purged.
        (
            Some(original),
            Some(original_hash),
            Some(_),
            Some(backup_id),
            Some(target_hash),
            Some(backup_hash),
            None,
        ) if backup_id == original
            && backup_hash == original_hash
            && target_hash == journal.new_sha256 =>
        {
            sync_cap_directory(directory)?;
            remove_config_with_authority(
                directory,
                OsStr::new(&journal.backup),
                original,
                original_hash,
            )?;
            sync_cap_directory(directory)?;
        }
        _ => return Err(recovery_error("config transaction cannot be recovered safely")),
    }
    directory.remove_file(journal_name)?;
    sync_cap_directory(directory)
}

fn config_rename_no_replace(
    directory: &cap_std::fs::Dir,
    source: &OsStr,
    destination: &OsStr,
) -> Result<(), CliError> {
    validate_single_name(source)?;
    validate_single_name(destination)?;
    #[cfg(unix)]
    rustix::fs::renameat_with(
        directory,
        source,
        directory,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|_| recovery_error("config transaction rename failed"))?;
    #[cfg(windows)]
    {
        let pinned = directory.try_clone()?.into_std_file();
        into_markdown_process_plugin::rename_windows_plugin_file_no_replace(
            &pinned,
            source,
            destination,
        )
        .map_err(|error| recovery_error(&error.to_string()))?;
    }
    Ok(())
}

fn remove_config_with_authority(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
    expected_identity: (u64, u64),
    expected_sha256: &str,
) -> Result<(), CliError> {
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
    let mut file = directory.open_with(name, &options)?.into_std();
    let identity = config_open_file_identity(&file)?;
    if config_open_file_link_count(&file)? != 1
        || identity != expected_identity
        || config_open_file_digest(&mut file)? != expected_sha256
        || config_open_file_identity(&file)? != identity
        || cap_config_identity(directory, name)? != Some(identity)
    {
        return Err(recovery_error("config cleanup authority changed"));
    }
    config_test_replace_before_cleanup(directory, name)?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| CliError::new(ExitClass::Internal, "internal", "config nonce unavailable"))?;
    let nonce = nonce.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let quarantine = OsString::from(format!(".into-md-config-clean-{nonce}"));
    config_rename_no_replace(directory, name, &quarantine)?;
    if cap_config_identity(directory, &quarantine)? != Some(identity)
        || cap_config_digest(directory, &quarantine)?.as_deref() != Some(expected_sha256)
    {
        let _ = config_rename_no_replace(directory, &quarantine, name);
        return Err(recovery_error("config cleanup authority changed at rename barrier"));
    }
    directory.remove_file(&quarantine)?;
    Ok(())
}

fn config_open_file_digest(file: &mut File) -> Result<String, CliError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
fn config_test_replace_before_cleanup(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
) -> Result<(), CliError> {
    let marker = Path::new(".test-config-replace-before-cleanup");
    let replacement = match directory.read(marker) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    directory.remove_file(marker)?;
    directory.rename(name, directory, ".test-config-clean-held")?;
    directory.write(name, replacement)?;
    Ok(())
}

#[cfg(not(test))]
fn config_test_replace_before_cleanup(
    _directory: &cap_std::fs::Dir,
    _name: &OsStr,
) -> Result<(), CliError> {
    Ok(())
}

#[cfg(test)]
fn windows_config_test_crash(point: &str) {
    if std::env::var_os("INTO_MD_CONFIG_CRASH_POINT").as_deref() == Some(OsStr::new(point)) {
        std::process::exit(86);
    }
}

#[cfg(not(test))]
fn windows_config_test_crash(_point: &str) {}

pub(crate) fn recover_config_in_dir(
    directory: &cap_std::fs::Dir,
    target: &OsStr,
) -> Result<(), CliError> {
    recover_windows_config_transaction(directory, target)
}

fn valid_windows_config_transaction_name(value: &str, suffix: &str) -> bool {
    value.starts_with(".into-md-config-")
        && value.ends_with(&format!(".{suffix}"))
        && value.len() == ".into-md-config-".len() + 32 + 1 + suffix.len()
        && value[".into-md-config-".len()..".into-md-config-".len() + 32]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn config_transaction_nonce(value: &str) -> Option<&str> {
    let prefix = ".into-md-config-";
    let remainder = value.strip_prefix(prefix)?;
    let (nonce, _) = remainder.split_once('.')?;
    (nonce.len() == 32
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some(nonce)
}

fn cap_config_identity(
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

fn cap_config_digest(
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
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Some(format!("{:x}", hasher.finalize())))
}

fn sync_cap_directory(directory: &cap_std::fs::Dir) -> Result<(), CliError> {
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

fn config_open_file_identity(file: &File) -> Result<(u64, u64), CliError> {
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

fn config_open_file_link_count(file: &File) -> Result<u64, CliError> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum JournalPhase {
    Staging,
    Prepared,
    Committing,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum EntryState {
    Prepared,
    BackedUp,
    Installed,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalPath {
    encoding: String,
    units: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileIdentity {
    platform: String,
    first: u64,
    second: u64,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalEntry {
    target: JournalPath,
    original: Option<FileIdentity>,
    content_sha256: String,
    size: u64,
    state: EntryState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Journal {
    signature: String,
    version: u32,
    nonce: String,
    root: JournalPath,
    root_identity: FileIdentity,
    parent_identities: Vec<FileIdentity>,
    generation: u64,
    phase: JournalPhase,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParentLease {
    signature: String,
    version: u32,
    nonce: String,
    root: JournalPath,
    root_identity: FileIdentity,
    parent_identity: FileIdentity,
}

/// One requested target and its complete staged contents.
pub struct Target<'a> {
    pub path: PathBuf,
    pub bytes: &'a [u8],
}

/// One requested target whose staged contents come from a seekable file.
pub struct FileTarget<'a> {
    pub path: PathBuf,
    pub file: &'a File,
}

trait TransactionSource {
    fn path(&self) -> &Path;
    fn size_and_sha256(&self, context: &ExecutionContext) -> Result<(u64, String), CliError>;
    fn write_to(&self, destination: &mut File, context: &ExecutionContext) -> Result<(), CliError>;
}

impl TransactionSource for Target<'_> {
    fn path(&self) -> &Path {
        &self.path
    }

    fn size_and_sha256(&self, _: &ExecutionContext) -> Result<(u64, String), CliError> {
        let size = u64::try_from(self.bytes.len()).map_err(|_| {
            CliError::new(ExitClass::Policy, "resourceLimit", "target size cannot be represented")
        })?;
        Ok((size, sha256_hex(self.bytes)))
    }

    fn write_to(&self, destination: &mut File, context: &ExecutionContext) -> Result<(), CliError> {
        context.checkpoint().map_err(CliError::from)?;
        destination.write_all(self.bytes).map_err(CliError::from)
    }
}

impl TransactionSource for FileTarget<'_> {
    fn path(&self) -> &Path {
        &self.path
    }

    fn size_and_sha256(&self, context: &ExecutionContext) -> Result<(u64, String), CliError> {
        let mut source = self.file.try_clone()?;
        source.rewind()?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            context.checkpoint().map_err(CliError::from)?;
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size = size.checked_add(u64::try_from(read).unwrap_or(u64::MAX)).ok_or_else(|| {
                CliError::new(ExitClass::Policy, "resourceLimit", "target size overflowed")
            })?;
            digest.update(&buffer[..read]);
        }
        Ok((size, format!("{:x}", digest.finalize())))
    }

    fn write_to(&self, destination: &mut File, context: &ExecutionContext) -> Result<(), CliError> {
        let mut source = self.file.try_clone()?;
        source.rewind()?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            context.checkpoint().map_err(CliError::from)?;
            let read = source.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            destination.write_all(&buffer[..read])?;
        }
    }
}

#[derive(Clone, Copy)]
enum MixedContent<'a> {
    Bytes(&'a [u8]),
    File(&'a File),
}

struct MixedTarget<'a> {
    path: &'a Path,
    content: MixedContent<'a>,
}

impl TransactionSource for MixedTarget<'_> {
    fn path(&self) -> &Path {
        self.path
    }

    fn size_and_sha256(&self, context: &ExecutionContext) -> Result<(u64, String), CliError> {
        match self.content {
            MixedContent::Bytes(bytes) => {
                Target { path: PathBuf::new(), bytes }.size_and_sha256(context)
            }
            MixedContent::File(file) => {
                FileTarget { path: PathBuf::new(), file }.size_and_sha256(context)
            }
        }
    }

    fn write_to(&self, destination: &mut File, context: &ExecutionContext) -> Result<(), CliError> {
        match self.content {
            MixedContent::Bytes(bytes) => {
                Target { path: PathBuf::new(), bytes }.write_to(destination, context)
            }
            MixedContent::File(file) => {
                FileTarget { path: PathBuf::new(), file }.write_to(destination, context)
            }
        }
    }
}

#[cfg(unix)]
pub(crate) struct SafeDir {
    fd: OwnedFd,
    path: PathBuf,
    identity: FileIdentity,
}

#[cfg(unix)]
impl SafeDir {
    pub(crate) fn open_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory handle path is not absolute"));
        }
        let mut current_path = PathBuf::from("/");
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    fd = rustix::fs::openat(
                        &fd,
                        name,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    )?;
                    current_path.push(name);
                }
                _ => return Err(recovery_error("directory handle path is not normalized")),
            }
        }
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: current_path, identity })
    }

    fn open_or_create_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory creation path is not absolute"));
        }
        let mut current_path = PathBuf::from("/");
        let mut fd = rustix::fs::open(
            "/",
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    let opened = rustix::fs::openat(
                        &fd,
                        name,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    );
                    fd = match opened {
                        Ok(opened) => opened,
                        Err(rustix::io::Errno::NOENT) => {
                            match rustix::fs::mkdirat(
                                &fd,
                                name,
                                rustix::fs::Mode::RUSR
                                    | rustix::fs::Mode::WUSR
                                    | rustix::fs::Mode::XUSR
                                    | rustix::fs::Mode::RGRP
                                    | rustix::fs::Mode::XGRP
                                    | rustix::fs::Mode::ROTH
                                    | rustix::fs::Mode::XOTH,
                            ) {
                                Ok(()) => rustix::fs::fsync(&fd)?,
                                // Another batch worker may have created this
                                // exact component after our failed open. The
                                // authenticated NOFOLLOW open below decides
                                // whether the winner created an acceptable
                                // directory.
                                Err(rustix::io::Errno::EXIST) => {}
                                Err(error) => return Err(error.into()),
                            }
                            rustix::fs::openat(
                                &fd,
                                name,
                                rustix::fs::OFlags::RDONLY
                                    | rustix::fs::OFlags::DIRECTORY
                                    | rustix::fs::OFlags::NOFOLLOW
                                    | rustix::fs::OFlags::CLOEXEC,
                                rustix::fs::Mode::empty(),
                            )?
                        }
                        Err(error) => return Err(error.into()),
                    };
                    current_path.push(name);
                }
                _ => return Err(recovery_error("directory creation path is not normalized")),
            }
        }
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: current_path, identity })
    }

    pub(crate) fn open_child(&self, name: &OsStr) -> Result<Self, CliError> {
        validate_single_name(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let identity = directory_identity(&fd)?;
        Ok(Self { fd, path: self.path.join(name), identity })
    }

    pub(crate) fn open_child_optional(&self, name: &OsStr) -> Result<Option<Self>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => {
                let identity = directory_identity(&fd)?;
                Ok(Some(Self { fd, path: self.path.join(name), identity }))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn open_descendant(&self, relative: &Path) -> Result<Self, CliError> {
        if relative.as_os_str().is_empty() {
            let fd = rustix::io::dup(&self.fd)?;
            return Ok(Self { fd, path: self.path.clone(), identity: self.identity.clone() });
        }
        validate_relative_path(relative)?;
        let mut current = Self {
            fd: rustix::io::dup(&self.fd)?,
            path: self.path.clone(),
            identity: self.identity.clone(),
        };
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(recovery_error("descendant path is not normalized"));
            };
            current = current.open_child(name)?;
        }
        Ok(current)
    }

    pub(crate) fn verify_namespace(&self) -> Result<(), CliError> {
        let changed = || {
            CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                format!("output directory changed after authentication: {}", self.path.display()),
            )
        };
        let current = Self::open_absolute(&self.path).map_err(|_| changed())?;
        if current.identity != self.identity {
            return Err(changed());
        }
        Ok(())
    }

    pub(crate) fn verify_private_namespace(&self) -> Result<(), CliError> {
        let stat = rustix::fs::fstat(&self.fd)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o777 != 0o700
        {
            return Err(recovery_error(
                "managed directory is not private, owner-bound, and descriptor-authenticated",
            ));
        }
        self.verify_namespace()
    }

    pub(crate) fn open_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child(name)?;
        self.verify_private_namespace()?;
        child.verify_private_namespace()?;
        Ok(child)
    }

    pub(crate) fn open_child_private_optional(
        &self,
        name: &OsStr,
    ) -> Result<Option<Self>, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child_optional(name)?;
        self.verify_private_namespace()?;
        if let Some(child) = &child {
            child.verify_private_namespace()?;
        }
        Ok(child)
    }

    pub(crate) fn open_regular(&self, name: &OsStr) -> Result<File, CliError> {
        validate_single_name(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?;
        let stat = rustix::fs::fstat(&fd)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
            return Err(CliError::new(
                ExitClass::Io,
                "outputTargetTypeDenied",
                format!("not a regular file: {}", self.path.join(name).display()),
            ));
        }
        Ok(File::from(fd))
    }

    pub(crate) fn open_regular_optional(&self, name: &OsStr) -> Result<Option<File>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => {
                let stat = rustix::fs::fstat(&fd)?;
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    != rustix::fs::FileType::RegularFile
                {
                    return Err(recovery_error("optional managed file is not regular"));
                }
                Ok(Some(File::from(fd)))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn create_regular(&self, name: &OsStr) -> Result<File, CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        self.verify_namespace()?;
        Ok(File::from(fd))
    }

    pub(crate) fn create_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.create_regular(name)?;
        verify_private_regular(&file)?;
        self.verify_private_namespace()?;
        Ok(file)
    }

    pub(crate) fn open_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular(name)?;
        verify_private_regular(&file)?;
        self.verify_private_namespace()?;
        Ok(file)
    }

    fn inspect_regular(&self, name: &OsStr) -> Result<Option<FileIdentity>, CliError> {
        validate_single_name(name)?;
        match rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::RegularFile =>
            {
                let file = self.open_regular(name)?;
                Ok(Some(file_identity(&file)?))
            }
            Ok(_) => Err(CliError::new(
                ExitClass::Io,
                "outputTargetTypeDenied",
                format!(
                    "output target is not a regular non-link file: {}",
                    self.path.join(name).display()
                ),
            )),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn sync(&self) -> Result<(), CliError> {
        rustix::fs::fsync(&self.fd)?;
        Ok(())
    }

    pub(crate) fn names(&self) -> Result<Vec<OsString>, CliError> {
        self.names_bounded(MAX_RECOVERY_DIRECTORY_ENTRIES)
    }

    pub(crate) fn names_private(&self) -> Result<Vec<OsString>, CliError> {
        self.verify_private_namespace()?;
        let names = self.names()?;
        self.verify_private_namespace()?;
        Ok(names)
    }

    pub(crate) fn names_bounded(&self, limit: usize) -> Result<Vec<OsString>, CliError> {
        use std::os::unix::ffi::OsStringExt as _;
        let mut directory = rustix::fs::Dir::read_from(&self.fd)?;
        let mut names = Vec::new();
        while let Some(entry) = directory.read() {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if names.len() >= limit {
                return Err(CliError::new(
                    ExitClass::Io,
                    "transactionRecoveryLimit",
                    format!("recovery scan exceeded {limit} entries under {}", self.path.display()),
                ));
            }
            names.try_reserve(1).map_err(|error| {
                CliError::new(
                    ExitClass::Io,
                    "transactionRecoveryLimit",
                    format!("cannot reserve recovery directory entry: {error}"),
                )
            })?;
            names.push(OsString::from_vec(bytes.to_vec()));
        }
        Ok(names)
    }

    pub(crate) fn create_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        rustix::fs::mkdirat(
            &self.fd,
            name,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
        )?;
        self.sync()?;
        let child = self.open_child(name)?;
        self.verify_private_namespace()?;
        child.verify_private_namespace()?;
        Ok(child)
    }

    pub(crate) fn rename_child_private_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        self.rename_child_no_replace(source, destination)?;
        self.verify_private_namespace()
    }

    pub(crate) fn rename_child_private_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        destination_directory.verify_private_namespace()?;
        self.rename_child_to_no_replace(source, destination_directory, destination)?;
        self.verify_private_namespace()?;
        destination_directory.verify_private_namespace()
    }

    pub(crate) fn remove_regular_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular_private(name)?;
        drop(file);
        self.remove_regular(name)?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_empty_child_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child(name)?;
        child.verify_private_namespace()?;
        self.remove_empty_child(name)?;
        self.verify_private_namespace()
    }

    pub(crate) fn rename_child_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        validate_single_name(source)?;
        validate_single_name(destination)?;
        self.verify_namespace()?;
        rustix::fs::renameat_with(
            &self.fd,
            source,
            &self.fd,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn rename_child_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        validate_single_name(source)?;
        validate_single_name(destination)?;
        self.verify_namespace()?;
        destination_directory.verify_namespace()?;
        rustix::fs::renameat_with(
            &self.fd,
            source,
            &destination_directory.fd,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        self.sync()?;
        destination_directory.sync()?;
        self.verify_namespace()?;
        destination_directory.verify_namespace()
    }

    pub(crate) fn remove_regular(&self, name: &OsStr) -> Result<(), CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let file = self.open_regular(name)?;
        if rustix::fs::fstat(&file)?.st_nlink != 1 {
            return Err(recovery_error("managed file has an external hard link"));
        }
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())?;
        self.sync()?;
        self.verify_namespace()
    }

    fn remove_committed_backup(
        &self,
        name: &OsStr,
        expected: &FileIdentity,
    ) -> Result<(), CliError> {
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        let file = self.open_regular(name)?;
        if file_identity(&file)? != *expected {
            return Err(recovery_error("committed backup identity changed before cleanup"));
        }
        // A pre-existing output may legitimately have another hard-link name.
        // The transaction directory is private and the journal binds this exact
        // inode, so unlinking only its private backup name cannot remove or
        // mutate the caller-owned alias.
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())?;
        self.sync()?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_empty_child(&self, name: &OsStr) -> Result<(), CliError> {
        validate_single_name(name)?;
        self.verify_namespace()?;
        let child = self.open_child(name)?;
        if !child.names()?.is_empty() {
            return Err(recovery_error("managed directory is not empty"));
        }
        rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::REMOVEDIR)?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn measured_tree_bytes(
        &self,
        max_depth: u8,
        max_entries: usize,
    ) -> Result<u64, CliError> {
        fn visit(
            directory: &SafeDir,
            depth: u8,
            max_depth: u8,
            entries: &mut usize,
            max_entries: usize,
        ) -> Result<u64, CliError> {
            use std::os::unix::ffi::OsStrExt as _;
            if depth > max_depth {
                return Err(recovery_error("managed storage depth exceeds its limit"));
            }
            let mut reader = rustix::fs::Dir::read_from(&directory.fd)?;
            let mut total = 0_u64;
            while let Some(entry) = reader.read() {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                *entries = entries
                    .checked_add(1)
                    .ok_or_else(|| recovery_error("managed storage entry count overflow"))?;
                if *entries > max_entries {
                    return Err(recovery_error("managed storage entry count exceeds its limit"));
                }
                let stat =
                    rustix::fs::statat(&directory.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
                match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
                    rustix::fs::FileType::Directory => {
                        let child = directory.open_child(OsStr::from_bytes(name.to_bytes()))?;
                        total = total
                            .checked_add(visit(&child, depth + 1, max_depth, entries, max_entries)?)
                            .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                    }
                    rustix::fs::FileType::RegularFile if stat.st_nlink == 1 => {
                        total = total
                            .checked_add(u64::try_from(stat.st_size).map_err(|_| {
                                recovery_error("managed file size is not representable")
                            })?)
                            .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                    }
                    _ => return Err(recovery_error("managed storage contains an unsafe object")),
                }
            }
            Ok(total)
        }
        let mut entries = 0;
        visit(self, 0, max_depth, &mut entries, max_entries)
    }
}

fn validate_single_name(name: &OsStr) -> Result<(), CliError> {
    let path = Path::new(name);
    if path.as_os_str().is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(recovery_error("transaction member is not a single safe name"));
    }
    Ok(())
}

#[cfg(unix)]
fn fd_identity(fd: &impl std::os::fd::AsFd) -> Result<FileIdentity, CliError> {
    let stat = rustix::fs::fstat(fd)?;
    Ok(FileIdentity {
        platform: "unix".into(),
        first: u64::try_from(stat.st_dev).unwrap_or(u64::MAX),
        #[allow(clippy::useless_conversion)]
        second: u64::try_from(stat.st_ino).unwrap_or(u64::MAX),
        size: u64::try_from(stat.st_size).unwrap_or(u64::MAX),
    })
}

#[cfg(unix)]
fn directory_identity(fd: &impl std::os::fd::AsFd) -> Result<FileIdentity, CliError> {
    let mut identity = fd_identity(fd)?;
    identity.size = 0;
    Ok(identity)
}

#[cfg(unix)]
fn verify_private_regular(file: &File) -> Result<(), CliError> {
    let stat = rustix::fs::fstat(file)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
        || stat.st_nlink != 1
    {
        return Err(recovery_error("managed file is not private, owner-bound, and singly linked"));
    }
    Ok(())
}

/// Test seam for deterministic failure and crash injection.
#[derive(Debug)]
pub enum HookDecision {
    Continue,
    #[cfg(test)]
    SimulateCrash,
    #[cfg(test)]
    SimulateRollbackFailure,
}

/// A fully staged transaction. Dropping it preserves the journal for recovery.
pub struct PreparedTransaction {
    root: PathBuf,
    directory: PathBuf,
    journal: Journal,
    context: ExecutionContext,
    active: bool,
    temporary_reservations: Vec<ResourceReservation>,
    #[cfg(test)]
    simulate_rollback_failure: bool,
    lock: Option<File>,
    handles: TransactionHandles,
}

#[cfg(unix)]
struct TransactionHandles {
    root: SafeDir,
    directory: SafeDir,
}

#[cfg(not(unix))]
struct TransactionHandles {
    root: SafeDir,
    directory: SafeDir,
}

#[cfg(unix)]
struct AuthenticatedTarget {
    parent: SafeDir,
    name: OsString,
}

#[cfg(not(unix))]
struct AuthenticatedTarget {
    parent: SafeDir,
    name: OsString,
}

#[cfg(windows)]
pub(crate) struct SafeDir {
    directory: cap_std::fs::Dir,
    path: PathBuf,
    identity: FileIdentity,
}

#[cfg(windows)]
impl SafeDir {
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const REPARSE_ATTRIBUTE: u64 = 0x0000_0400;

    fn from_file(path: PathBuf, file: File) -> Result<Self, CliError> {
        let metadata = file.metadata()?;
        let information = winapi_util::file::information(&file)?;
        if !metadata.is_dir() || information.file_attributes() & Self::REPARSE_ATTRIBUTE != 0 {
            return Err(recovery_error("directory handle is not a regular non-reparse directory"));
        }
        let identity = FileIdentity {
            platform: "windows".into(),
            first: information.volume_serial_number(),
            second: information.file_index(),
            size: 0,
        };
        Ok(Self { directory: cap_std::fs::Dir::from_std_file(file), path, identity })
    }

    fn open_direct(path: &Path) -> Result<Self, CliError> {
        use std::os::windows::fs::OpenOptionsExt as _;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .custom_flags(Self::BACKUP_SEMANTICS | Self::OPEN_REPARSE_POINT);
        Self::from_file(path.to_path_buf(), options.open(path)?)
    }

    pub(crate) fn open_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory handle path is not absolute"));
        }
        let mut root = PathBuf::new();
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
                Component::Normal(name) => names.push(name.to_os_string()),
                _ => return Err(recovery_error("directory handle path is not normalized")),
            }
        }
        let mut current = Self::open_direct(&root)?;
        for name in names {
            current = current.open_child(&name)?;
        }
        Ok(current)
    }

    fn open_or_create_absolute(path: &Path) -> Result<Self, CliError> {
        if !path.is_absolute() {
            return Err(recovery_error("directory creation path is not absolute"));
        }
        let mut root = PathBuf::new();
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => root.push(component.as_os_str()),
                Component::Normal(name) => names.push(name.to_os_string()),
                _ => return Err(recovery_error("directory creation path is not normalized")),
            }
        }
        let mut current = Self::open_direct(&root)?;
        for name in names {
            current = match current.open_child_optional(&name)? {
                Some(child) => child,
                None => {
                    match current.directory.create_dir(&name) {
                        Ok(()) => current.sync()?,
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.into()),
                    }
                    current.open_child(&name)?
                }
            };
        }
        Ok(current)
    }

    pub(crate) fn open_child(&self, name: &OsStr) -> Result<Self, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .read(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .custom_flags(Self::BACKUP_SEMANTICS | Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        Self::from_file(self.path.join(name), file)
    }

    pub(crate) fn open_child_optional(&self, name: &OsStr) -> Result<Option<Self>, CliError> {
        validate_single_name(name)?;
        match self.directory.symlink_metadata(name) {
            Ok(_) => self.open_child(name).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn open_descendant(&self, relative: &Path) -> Result<Self, CliError> {
        if relative.as_os_str().is_empty() {
            let current = Self::open_absolute(&self.path)?;
            if current.identity != self.identity {
                return Err(recovery_error("directory identity changed"));
            }
            return Ok(current);
        }
        validate_relative_path(relative)?;
        let mut current = Self::open_absolute(&self.path)?;
        if current.identity != self.identity {
            return Err(recovery_error("directory identity changed"));
        }
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(recovery_error("descendant path is not normalized"));
            };
            current = current.open_child(name)?;
        }
        Ok(current)
    }

    pub(crate) fn open_regular(&self, name: &OsStr) -> Result<File, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).custom_flags(Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        let information = winapi_util::file::information(&file)?;
        if !file.metadata()?.is_file()
            || information.file_attributes() & Self::REPARSE_ATTRIBUTE != 0
            || information.number_of_links() != 1
        {
            return Err(recovery_error("managed file identity rejected"));
        }
        Ok(file)
    }

    fn open_lease_file(&self, name: &OsStr) -> Result<File, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).custom_flags(Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        let information = winapi_util::file::information(&file)?;
        if !file.metadata()?.is_file()
            || information.file_attributes() & Self::REPARSE_ATTRIBUTE != 0
            || information.number_of_links() != 2
        {
            return Err(recovery_error("transaction lease identity rejected"));
        }
        Ok(file)
    }

    fn inspect_lease_file(&self, name: &OsStr) -> Result<Option<FileIdentity>, CliError> {
        validate_single_name(name)?;
        match self.directory.symlink_metadata(name) {
            Ok(_) => self.open_lease_file(name).and_then(|file| file_identity(&file)).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn remove_lease_file(&self, name: &OsStr) -> Result<(), CliError> {
        let file = self.open_lease_file(name)?;
        drop(file);
        self.verify_namespace()?;
        self.directory.remove_file(name)?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn open_regular_optional(&self, name: &OsStr) -> Result<Option<File>, CliError> {
        validate_single_name(name)?;
        match self.directory.symlink_metadata(name) {
            Ok(_) => self.open_regular(name).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn verify_private_namespace(&self) -> Result<(), CliError> {
        into_markdown_process_plugin::verify_windows_plugin_store_path(&self.path).map_err(
            |error| {
                recovery_error(format!(
                    "private transaction directory rejected ({}): {error}",
                    self.path.display()
                ))
            },
        )?;
        self.verify_namespace()
    }

    pub(crate) fn open_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child(name)?;
        child.verify_private_namespace()?;
        Ok(child)
    }

    pub(crate) fn open_child_private_optional(
        &self,
        name: &OsStr,
    ) -> Result<Option<Self>, CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child_optional(name)?;
        if let Some(child) = &child {
            child.verify_private_namespace()?;
        }
        Ok(child)
    }

    pub(crate) fn create_regular(&self, name: &OsStr) -> Result<File, CliError> {
        use cap_std::fs::OpenOptionsExt as _;
        validate_single_name(name)?;
        self.verify_namespace()?;
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .create_new(true)
            .write(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .custom_flags(Self::OPEN_REPARSE_POINT);
        let file = self.directory.open_with(name, &options)?.into_std();
        self.verify_namespace()?;
        Ok(file)
    }

    pub(crate) fn create_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.create_regular(name)?;
        into_markdown_process_plugin::verify_windows_plugin_store_child(&self.path.join(name))
            .map_err(|error| {
                recovery_error(format!(
                    "private transaction member rejected ({}): {error}",
                    self.path.join(name).display()
                ))
            })?;
        Ok(file)
    }

    pub(crate) fn open_regular_private(&self, name: &OsStr) -> Result<File, CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular(name)?;
        into_markdown_process_plugin::verify_windows_plugin_store_child(&self.path.join(name))
            .map_err(|error| {
                recovery_error(format!(
                    "private transaction member rejected ({}): {error}",
                    self.path.join(name).display()
                ))
            })?;
        Ok(file)
    }

    pub(crate) fn names(&self) -> Result<Vec<OsString>, CliError> {
        self.names_bounded(MAX_RECOVERY_DIRECTORY_ENTRIES)
    }

    pub(crate) fn names_private(&self) -> Result<Vec<OsString>, CliError> {
        self.verify_private_namespace()?;
        let names = self.names()?;
        self.verify_private_namespace()?;
        Ok(names)
    }

    pub(crate) fn names_bounded(&self, limit: usize) -> Result<Vec<OsString>, CliError> {
        let mut names = Vec::new();
        for entry in self.directory.entries()? {
            let entry = entry?;
            if names.len() >= limit {
                return Err(recovery_error("recovery directory entry limit exceeded"));
            }
            names.push(entry.file_name());
        }
        Ok(names)
    }

    pub(crate) fn create_child_private(&self, name: &OsStr) -> Result<Self, CliError> {
        validate_single_name(name)?;
        self.verify_private_namespace()?;
        into_markdown_process_plugin::create_windows_plugin_store_directory(&self.path.join(name))
            .map_err(|error| recovery_error(&error.to_string()))?;
        self.sync()?;
        self.open_child_private(name)
    }

    pub(crate) fn rename_child_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_namespace()?;
        let pinned = self.directory.try_clone()?.into_std_file();
        into_markdown_process_plugin::rename_windows_plugin_file_no_replace(
            &pinned,
            source,
            destination,
        )
        .map_err(|error| recovery_error(&error.to_string()))?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn rename_child_private_no_replace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        self.rename_child_no_replace(source, destination)?;
        self.verify_private_namespace()
    }

    pub(crate) fn rename_child_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_namespace()?;
        destination_directory.verify_namespace()?;
        let source_handle = self.directory.try_clone()?.into_std_file();
        let destination_handle = destination_directory.directory.try_clone()?.into_std_file();
        into_markdown_process_plugin::move_windows_plugin_file_no_replace(
            &source_handle,
            source,
            &destination_handle,
            destination,
        )
        .map_err(|error| recovery_error(&error.to_string()))?;
        self.sync()?;
        destination_directory.sync()?;
        Ok(())
    }

    pub(crate) fn rename_child_private_to_no_replace(
        &self,
        source: &OsStr,
        destination_directory: &Self,
        destination: &OsStr,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        destination_directory.verify_private_namespace()?;
        self.rename_child_to_no_replace(source, destination_directory, destination)?;
        self.verify_private_namespace()?;
        destination_directory.verify_private_namespace()
    }

    pub(crate) fn remove_regular(&self, name: &OsStr) -> Result<(), CliError> {
        let expected = self.inspect_regular(name)?.ok_or_else(|| recovery_error("file missing"))?;
        self.verify_namespace()?;
        verify_name_identity(self, name, Some(&expected))?;
        self.directory.remove_file(name)?;
        self.sync()?;
        self.verify_namespace()
    }

    fn remove_committed_backup(
        &self,
        name: &OsStr,
        expected: &FileIdentity,
    ) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        verify_name_identity(self, name, Some(expected))?;
        self.directory.remove_file(name)?;
        self.sync()?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_regular_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let file = self.open_regular_private(name)?;
        drop(file);
        self.remove_regular(name)?;
        self.verify_private_namespace()
    }

    pub(crate) fn remove_empty_child(&self, name: &OsStr) -> Result<(), CliError> {
        let child = self.open_child(name)?;
        if !child.names()?.is_empty() {
            return Err(recovery_error("managed directory is not empty"));
        }
        drop(child);
        self.verify_namespace()?;
        self.directory.remove_dir(name)?;
        self.sync()?;
        self.verify_namespace()
    }

    pub(crate) fn remove_empty_child_private(&self, name: &OsStr) -> Result<(), CliError> {
        self.verify_private_namespace()?;
        let child = self.open_child_private(name)?;
        drop(child);
        self.remove_empty_child(name)?;
        self.verify_private_namespace()
    }

    fn inspect_regular(&self, name: &OsStr) -> Result<Option<FileIdentity>, CliError> {
        self.open_regular_optional(name)?.map(|file| file_identity(&file)).transpose()
    }

    pub(crate) fn measured_tree_bytes(
        &self,
        max_depth: u8,
        max_entries: usize,
    ) -> Result<u64, CliError> {
        fn visit(
            directory: &SafeDir,
            depth: u8,
            max_depth: u8,
            entries: &mut usize,
            max_entries: usize,
        ) -> Result<u64, CliError> {
            if depth > max_depth {
                return Err(recovery_error("managed storage depth exceeds its limit"));
            }
            let mut total = 0_u64;
            for name in directory.names()? {
                *entries = entries.saturating_add(1);
                if *entries > max_entries {
                    return Err(recovery_error("managed storage entry count exceeds its limit"));
                }
                if let Some(file) = directory.open_regular_optional(&name)? {
                    total = total
                        .checked_add(file.metadata()?.len())
                        .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                } else {
                    let child = directory.open_child(&name)?;
                    total = total
                        .checked_add(visit(&child, depth + 1, max_depth, entries, max_entries)?)
                        .ok_or_else(|| recovery_error("managed storage byte count overflow"))?;
                }
            }
            Ok(total)
        }
        let mut entries = 0;
        visit(self, 0, max_depth, &mut entries, max_entries)
    }

    fn verify_namespace(&self) -> Result<(), CliError> {
        let current = Self::open_absolute(&self.path)?;
        if current.identity != self.identity {
            return Err(CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                format!("output directory changed after authentication: {}", self.path.display()),
            ));
        }
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), CliError> {
        match self.directory.try_clone()?.into_std_file().sync_all() {
            Ok(()) => Ok(()),
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || matches!(error.raw_os_error(), Some(1 | 6)) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) struct SafeDir;

impl PreparedTransaction {
    /// Commit every staged target, or recover the complete old set.
    pub fn commit(mut self) -> Result<Vec<PathBuf>, CliError> {
        self.commit_with_hook(|_, _| Ok(HookDecision::Continue))
    }

    /// Discard a transaction which has not begun committing.
    pub fn abort(mut self) -> Result<(), CliError> {
        self.temporary_reservations.clear();
        let result = recover_transaction(&self.root, &self.directory, self.lock.take());
        self.deactivate();
        if result.is_ok() {
            try_cleanup_empty_registry(&self.root);
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn commit_with_hook(
        &mut self,
        mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<Vec<PathBuf>, CliError> {
        self.journal.phase = JournalPhase::Committing;
        if let Err(error) = persist_journal_handle(&self.handles.directory, &mut self.journal) {
            return self.fail_and_recover(error);
        }
        if let Err(error) = crash_point(&mut hook, "committing", usize::MAX, self) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return self.fail_and_recover(error);
        }

        let authenticated = match authenticate_targets(&self.handles.root, &self.journal.entries) {
            Ok(targets) => targets,
            Err(error) => return self.fail_and_recover(error),
        };
        if let Err(error) =
            validate_parent_leases(&self.handles.directory, &authenticated, &self.journal)
        {
            return self.fail_and_recover_authenticated(error, &authenticated);
        }

        // Validate every destination immediately before the first output-set
        // mutation. This prevents a late directory/FIFO/link swap on a later
        // entry from producing an avoidable partially installed set.
        for (index, target) in authenticated.iter().enumerate() {
            if let Err(error) = self.context.checkpoint().map_err(CliError::from) {
                return self.fail_and_recover(error);
            }
            let expected = self.journal.entries[index].original.clone();
            if let Err(error) = verify_target_handle_identity(target, expected.as_ref()) {
                return self.fail_and_recover(error);
            }
        }
        if let Err(error) = crash_point(&mut hook, "afterTargetAuthentication", usize::MAX, self) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return self.fail_and_recover(error);
        }
        self.preserve_staged_files();

        for index in 0..self.journal.entries.len() {
            if let Err(error) = self.context.checkpoint().map_err(CliError::from) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = call_hook(&mut hook, "beforeTarget", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            let target = &authenticated[index];

            let expected = self.journal.entries[index].original.clone();
            if let Err(error) = target.parent.verify_namespace() {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = verify_target_handle_identity(target, expected.as_ref()) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if expected.is_some() {
                let backup = backup_name(index);
                if let Err(error) =
                    handle_rename(&target.parent, &target.name, &self.handles.directory, &backup)
                {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) =
                    verify_name_identity(&self.handles.directory, &backup, expected.as_ref())
                {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) = target.parent.sync() {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) = self.handles.directory.sync() {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) = crash_point(&mut hook, "backupRenamed", index, self) {
                    if error.code() == "simulatedCrash" {
                        return Err(error);
                    }
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                self.journal.entries[index].state = EntryState::BackedUp;
                if let Err(error) =
                    persist_journal_handle(&self.handles.directory, &mut self.journal)
                {
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
                if let Err(error) = crash_point(&mut hook, "backupJournaled", index, self) {
                    if error.code() == "simulatedCrash" {
                        return Err(error);
                    }
                    return self.fail_and_recover_authenticated(error, &authenticated);
                }
            }

            if let Err(error) = install_stage_no_replace_handle(
                &self.handles.directory,
                &stage_name(index),
                &target.parent,
                &target.name,
            ) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = target.parent.sync() {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = self.handles.directory.sync() {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = verify_handle_content(target, &self.journal.entries[index]) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = crash_point(&mut hook, "targetInstalled", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            self.journal.entries[index].state = EntryState::Installed;
            if let Err(error) = persist_journal_handle(&self.handles.directory, &mut self.journal) {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
            if let Err(error) = crash_point(&mut hook, "installJournaled", index, self) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
        }

        for target in &authenticated {
            if let Err(error) = target.parent.verify_namespace() {
                return self.fail_and_recover_authenticated(error, &authenticated);
            }
        }

        self.journal.phase = JournalPhase::Committed;
        if let Err(error) = persist_journal_handle(&self.handles.directory, &mut self.journal) {
            return self.fail_and_recover_authenticated(error, &authenticated);
        }
        crash_point(&mut hook, "committed", usize::MAX, self)?;
        let targets = self
            .journal
            .entries
            .iter()
            .map(|entry| target_path(&self.root, entry))
            .collect::<Result<Vec<_>, _>>()?;
        match finish_committed(&self.root, &self.directory, &self.journal, self.lock.take()) {
            Ok(()) => {
                self.deactivate();
                try_cleanup_empty_registry(&self.root);
                Ok(targets)
            }
            Err(error) => {
                self.deactivate();
                Err(recovery_failed("committed output cleanup", &error))
            }
        }
    }

    fn fail_and_recover<T>(&mut self, original: CliError) -> Result<T, CliError> {
        self.temporary_reservations.clear();
        match recover_transaction(&self.root, &self.directory, self.lock.take()) {
            Ok(()) => {
                self.deactivate();
                try_cleanup_empty_registry(&self.root);
                Err(original)
            }
            Err(recovery) => {
                self.deactivate();
                Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction failed ({}: {}); rollback failed and journal was preserved ({}: {})",
                        original.code(),
                        original.message(),
                        recovery.code(),
                        recovery.message()
                    ),
                ))
            }
        }
    }

    fn fail_and_recover_authenticated<T>(
        &mut self,
        original: CliError,
        targets: &[AuthenticatedTarget],
    ) -> Result<T, CliError> {
        #[cfg(not(any(unix, windows)))]
        {
            let _ = targets;
            return self.fail_and_recover(original);
        }
        #[cfg(any(unix, windows))]
        {
            self.temporary_reservations.clear();
            #[cfg(test)]
            let rollback = if self.simulate_rollback_failure {
                Err(CliError::new(
                    ExitClass::Io,
                    "injectedRollbackFailure",
                    "deterministic rollback failure injected by the test hook",
                ))
            } else {
                rollback_transaction_with_handles(&self.handles.directory, targets, &self.journal)
            };
            #[cfg(not(test))]
            let rollback =
                rollback_transaction_with_handles(&self.handles.directory, targets, &self.journal);
            if let Err(recovery) = rollback {
                self.lock.take();
                self.deactivate();
                return Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction failed ({}: {}); rollback through authenticated handles failed and journal was preserved ({}: {})",
                        original.code(),
                        original.message(),
                        recovery.code(),
                        recovery.message()
                    ),
                ));
            }
            match recover_transaction(&self.root, &self.directory, self.lock.take()) {
                Ok(()) => {
                    self.deactivate();
                    try_cleanup_empty_registry(&self.root);
                    Err(original)
                }
                Err(recovery) => {
                    self.lock.take();
                    self.deactivate();
                    Err(CliError::new(
                        ExitClass::Io,
                        "rollbackFailed",
                        format!(
                            "output transaction failed ({}: {}); the old set was restored through authenticated handles, but journal cleanup was preserved for later recovery ({}: {})",
                            original.code(),
                            original.message(),
                            recovery.code(),
                            recovery.message()
                        ),
                    ))
                }
            }
        }
    }

    fn preserve_staged_files(&mut self) {
        self.temporary_reservations.clear();
    }

    #[cfg(test)]
    fn abandon_for_test(mut self) {
        self.preserve_staged_files();
        self.lock.take();
        self.deactivate();
    }

    fn deactivate(&mut self) {
        if self.active {
            active_transactions()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.directory);
            self.active = false;
        }
    }
}

impl Drop for PreparedTransaction {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// Recover manager-owned transactions in the exact root, then fully stage a new transaction.
pub fn prepare(
    targets: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
) -> Result<PreparedTransaction, CliError> {
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        match prepare_with_hook(targets, overwrite, context, |_, _| Ok(HookDecision::Continue)) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

/// Recover manager-owned transactions, then stage seekable files in bounded chunks.
pub fn prepare_files(
    targets: &[FileTarget<'_>],
    overwrite: bool,
    context: &ExecutionContext,
) -> Result<PreparedTransaction, CliError> {
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        match prepare_sources_with_hook(targets, overwrite, context, |_, _| {
            Ok(HookDecision::Continue)
        }) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

/// Stage one seekable primary artifact and zero or more in-memory companion assets.
pub fn prepare_file_and_bytes(
    primary: &FileTarget<'_>,
    companions: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
) -> Result<PreparedTransaction, CliError> {
    let mut sources = Vec::with_capacity(companions.len() + 1);
    sources.push(MixedTarget { path: &primary.path, content: MixedContent::File(primary.file) });
    sources.extend(companions.iter().map(|target| MixedTarget {
        path: &target.path,
        content: MixedContent::Bytes(target.bytes),
    }));
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        match prepare_sources_with_hook(&sources, overwrite, context, |_, _| {
            Ok(HookDecision::Continue)
        }) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

/// Recover any interrupted transaction owning one of these physical parent
/// directories before higher-level conflict planning observes the filesystem.
pub fn recover_for_paths(paths: &[PathBuf], context: &ExecutionContext) -> Result<(), CliError> {
    ensure_transaction_platform()?;
    let paths = paths.iter().map(|path| absolute_lexical(path)).collect::<Result<Vec<_>, _>>()?;
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        let parents = open_target_parents(&paths)?;
        match recover_parent_transactions(&parents) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

#[allow(clippy::too_many_lines)]
pub(crate) fn prepare_with_hook(
    targets: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
    mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<PreparedTransaction, CliError> {
    prepare_sources_with_hook(targets, overwrite, context, hook)
}

#[allow(clippy::too_many_lines)]
fn prepare_sources_with_hook<T: TransactionSource>(
    targets: &[T],
    overwrite: bool,
    context: &ExecutionContext,
    mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<PreparedTransaction, CliError> {
    ensure_transaction_platform()?;
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    #[cfg(any(unix, windows))]
    {
        if targets.is_empty() {
            return Err(CliError::internal("empty output transaction"));
        }
        if targets.len() > MAX_JOURNAL_ENTRIES {
            return Err(CliError::new(
                ExitClass::Policy,
                "transactionJournalLimit",
                "output transaction has too many targets",
            ));
        }
        let paths = targets
            .iter()
            .map(|target| absolute_lexical(target.path()))
            .collect::<Result<Vec<_>, _>>()?;
        let parent_handles = open_target_parents(&paths)?;
        recover_parent_transactions(&parent_handles)?;
        let root = common_existing_ancestor(&paths)?;
        ensure_same_filesystem(&root, &paths)?;
        let root_handle = SafeDir::open_absolute(&root)?;

        let mut entries = Vec::with_capacity(targets.len());
        let mut seen = BTreeSet::new();
        let mut seen_originals = BTreeSet::new();
        for (target, absolute) in targets.iter().zip(&paths) {
            let (size, content_sha256) = target.size_and_sha256(context)?;
            let relative = absolute.strip_prefix(&root).map_err(|_| {
                CliError::new(
                    ExitClass::Io,
                    "outputPathUnsupported",
                    "target is outside transaction root",
                )
            })?;
            validate_relative_path(relative)?;
            let encoded = encode_path(relative)?;
            if !seen.insert(encoded.units.clone()) {
                return Err(CliError::new(
                    ExitClass::Io,
                    "outputConflict",
                    format!("duplicate transaction target: {}", absolute.display()),
                ));
            }
            let original = {
                let parent =
                    absolute.parent().ok_or_else(|| recovery_error("target has no parent"))?;
                let parent_relative = parent.strip_prefix(&root).map_err(|_| {
                    recovery_error("target parent is outside authenticated transaction root")
                })?;
                let parent_handle = root_handle.open_descendant(parent_relative)?;
                parent_handle.verify_namespace()?;
                parent_handle.inspect_regular(
                    absolute
                        .file_name()
                        .ok_or_else(|| recovery_error("target has no file name"))?,
                )?
            };
            if original.is_some() && !overwrite {
                return Err(CliError::new(
                    ExitClass::Io,
                    "outputConflict",
                    format!("output target already exists: {}", absolute.display()),
                ));
            }
            if let Some(identity) = &original
                && !seen_originals.insert(identity.clone())
            {
                return Err(CliError::new(
                    ExitClass::Io,
                    "outputConflict",
                    "multiple output paths resolve to the same existing file",
                ));
            }
            entries.push(JournalEntry {
                target: encoded,
                original,
                content_sha256,
                size,
                state: EntryState::Prepared,
            });
        }

        let encoded_root = encode_path(&root)?;
        let parent_identities =
            parent_handles.iter().map(|parent| parent.identity.clone()).collect::<Vec<_>>();
        let (nonce, initial_directory, directory, lock) = create_initial_transaction(&root)?;
        let registry_handle = transaction_registry(&root_handle, false)?
            .ok_or_else(|| recovery_error("transaction registry disappeared"))?;
        let initial_handle = registry_handle.open_child(
            initial_directory
                .file_name()
                .ok_or_else(|| recovery_error("initial transaction has no name"))?,
        )?;
        let mut journal = Journal {
            signature: JOURNAL_SIGNATURE.into(),
            version: JOURNAL_VERSION,
            nonce,
            root: encoded_root,
            root_identity: root_handle.identity.clone(),
            parent_identities,
            generation: 0,
            phase: JournalPhase::Staging,
            entries,
        };
        if let Err(error) = persist_journal_handle(&initial_handle, &mut journal) {
            drop(lock);
            let _ = remove_initial_transaction_with_external_lock(
                &registry_handle,
                initial_directory.file_name().expect("initial transaction has a name"),
                &journal.nonce,
            );
            return Err(error);
        }
        if let Err(error) = create_parent_leases(&parent_handles, &initial_handle, &journal) {
            let cleanup = remove_parent_leases(&parent_handles, &initial_handle, &journal);
            drop(lock);
            if let Err(cleanup) = cleanup {
                return Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction lease creation failed ({}: {}); lease rollback failed and the initial journal was preserved ({}: {})",
                        error.code(),
                        error.message(),
                        cleanup.code(),
                        cleanup.message()
                    ),
                ));
            }
            let _ = remove_initial_transaction_with_external_lock(
                &registry_handle,
                initial_directory.file_name().expect("initial transaction has a name"),
                &journal.nonce,
            );
            return Err(error);
        }
        if let Err(error) = handle_rename(
            &registry_handle,
            initial_directory.file_name().expect("initial transaction has a name"),
            &registry_handle,
            directory.file_name().expect("transaction has a name"),
        ) {
            let cleanup = remove_parent_leases(&parent_handles, &initial_handle, &journal);
            drop(lock);
            #[cfg(windows)]
            let external_cleanup = registry_handle.remove_lease_file(&OsString::from(format!(
                "{EXTERNAL_LOCK_PREFIX}{}",
                journal.nonce
            )));
            #[cfg(windows)]
            if let Err(external_cleanup) = external_cleanup {
                return Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction publication failed ({}: {}); external lock cleanup failed ({}: {})",
                        error.code(),
                        error.message(),
                        external_cleanup.code(),
                        external_cleanup.message()
                    ),
                ));
            }
            if let Err(cleanup) = cleanup {
                return Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction publication failed ({}: {}); lease rollback failed and the initial journal was preserved ({}: {})",
                        error.code(),
                        error.message(),
                        cleanup.code(),
                        cleanup.message()
                    ),
                ));
            }
            let _ = remove_initial_transaction_with_external_lock(
                &registry_handle,
                initial_directory.file_name().expect("initial transaction has a name"),
                &journal.nonce,
            );
            return Err(error);
        }
        #[cfg(windows)]
        registry_handle.remove_lease_file(&OsString::from(format!(
            "{EXTERNAL_LOCK_PREFIX}{}",
            journal.nonce
        )))?;
        if let Err(error) = registry_handle.sync() {
            // The authenticated transaction is now visible. Leave it intact for a
            // later bounded recovery scan instead of guessing whether the rename
            // reached stable storage.
            drop(lock);
            return Err(error);
        }
        active_transactions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(directory.clone());
        let directory_handle = registry_handle.open_child(
            directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
        )?;
        let mut transaction = PreparedTransaction {
            root: root.clone(),
            directory: directory.clone(),
            journal,
            context: context.clone(),
            active: true,
            temporary_reservations: Vec::with_capacity(targets.len()),
            #[cfg(test)]
            simulate_rollback_failure: false,
            lock: Some(lock),
            handles: TransactionHandles { root: root_handle, directory: directory_handle },
        };
        if let Err(error) = crash_point(&mut hook, "journalCreated", usize::MAX, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        for (index, target) in targets.iter().enumerate() {
            if let Err(error) = call_hook(&mut hook, "beforeStage", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
            let amount = transaction.journal.entries[index].size;
            let reservation = match context.reserve_temporary(amount).map_err(CliError::from) {
                Ok(reservation) => reservation,
                Err(error) => return transaction.fail_and_recover(error),
            };
            transaction.temporary_reservations.push(reservation);
            let mut file = match transaction.handles.directory.create_regular(&stage_name(index)) {
                Ok(file) => file,
                Err(error) => return transaction.fail_and_recover(error),
            };
            if let Err(error) = crash_point(&mut hook, "stageAllocated", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = target.write_to(&mut file, context) {
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = crash_point(&mut hook, "stageWritten", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = call_hook(&mut hook, "beforeStageSync", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = context
                .checkpoint()
                .map_err(CliError::from)
                .and_then(|()| file.sync_all().map_err(CliError::from))
            {
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = crash_point(&mut hook, "stageSynced", index, &mut transaction) {
                if error.code() == "simulatedCrash" {
                    return Err(error);
                }
                return transaction.fail_and_recover(error);
            }
        }
        if let Err(error) = transaction.handles.directory.sync() {
            return transaction.fail_and_recover(error);
        }
        transaction.journal.phase = JournalPhase::Prepared;
        if let Err(error) =
            persist_journal_handle(&transaction.handles.directory, &mut transaction.journal)
        {
            return transaction.fail_and_recover(error);
        }
        if let Err(error) = crash_point(&mut hook, "prepared", usize::MAX, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        Ok(transaction)
    }
}

fn call_hook(
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    phase: &str,
    index: usize,
    transaction: &mut PreparedTransaction,
) -> Result<(), CliError> {
    #[cfg(not(test))]
    let _ = transaction;
    match hook(phase, index)? {
        HookDecision::Continue => Ok(()),
        #[cfg(test)]
        HookDecision::SimulateCrash => {
            transaction.preserve_staged_files();
            transaction.deactivate();
            Err(CliError::new(ExitClass::Io, "simulatedCrash", format!("{phase}:{index}")))
        }
        #[cfg(test)]
        HookDecision::SimulateRollbackFailure => {
            transaction.simulate_rollback_failure = true;
            Err(CliError::new(
                ExitClass::Io,
                "injectedPermissionFailure",
                format!("deterministic rollback failure requested at {phase}:{index}"),
            ))
        }
    }
}

fn crash_point(
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    phase: &str,
    index: usize,
    transaction: &mut PreparedTransaction,
) -> Result<(), CliError> {
    call_hook(hook, phase, index, transaction)
}

fn active_transactions() -> &'static Mutex<BTreeSet<PathBuf>> {
    ACTIVE_TRANSACTIONS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

#[allow(clippy::unnecessary_wraps)]
fn ensure_transaction_platform() -> Result<(), CliError> {
    #[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
    {
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
    {
        Err(transaction_platform_unavailable())
    }
}

#[allow(dead_code)]
fn transaction_platform_unavailable() -> CliError {
    CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "output transactions require audited relative directory-handle filesystem operations",
    )
}

#[cfg(any(unix, windows))]
fn open_target_parents(targets: &[PathBuf]) -> Result<Vec<SafeDir>, CliError> {
    let mut parents = BTreeMap::new();
    for target in targets {
        let name = target.file_name().ok_or_else(|| recovery_error("target has no file name"))?;
        if name == OsStr::new(PARENT_LEASE_NAME) || name == OsStr::new(REGISTRY_NAME) {
            return Err(CliError::new(
                ExitClass::Io,
                "outputPathUnsupported",
                "output target conflicts with the transaction manager namespace",
            ));
        }
        let parent = target.parent().ok_or_else(|| recovery_error("target has no parent"))?;
        let handle = SafeDir::open_or_create_absolute(parent)?;
        parents.entry(handle.identity.clone()).or_insert(handle);
    }
    Ok(parents.into_values().collect())
}

#[cfg(not(any(unix, windows)))]
fn open_target_parents(_targets: &[PathBuf]) -> Result<Vec<SafeDir>, CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
struct RecoveryReference {
    root: PathBuf,
    directory: PathBuf,
    directory_handle: SafeDir,
    initial: bool,
}

/// Recover transactions named by the fixed leases in the authenticated
/// physical target-parent directories. Recovery completes before this call
/// asks the caller to repeat preflight.
#[allow(clippy::too_many_lines)]
fn recover_parent_transactions(parents: &[SafeDir]) -> Result<(), CliError> {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = parents;
        return Err(transaction_platform_unavailable());
    }
    #[cfg(any(unix, windows))]
    {
        let mut references = BTreeMap::new();
        for parent in parents {
            let Some(lease) = load_parent_lease(parent)? else { continue };
            let root_path = decode_path(&lease.root)?;
            let root = SafeDir::open_absolute(&root_path)
                .map_err(|error| recovery_failed("open leased transaction root", &error))?;
            if root.identity != lease.root_identity {
                return Err(recovery_error("leased transaction root identity changed"));
            }
            let registry = transaction_registry(&root, false)?
                .ok_or_else(|| recovery_error("leased transaction registry is missing"))?;
            let managed_name = OsString::from(format!("{TRANSACTION_PREFIX}{}", lease.nonce));
            let initial_name = OsString::from(format!("{INITIAL_PREFIX}{}", lease.nonce));
            let cleanup_name = OsString::from(format!("{CLEANUP_PREFIX}{}", lease.nonce));
            let (name, directory_handle, initial) =
                if let Ok(handle) = registry.open_child(&managed_name) {
                    (managed_name, handle, false)
                } else if let Ok(handle) = registry.open_child(&initial_name) {
                    (initial_name, handle, true)
                } else if let Ok(handle) = registry.open_child(&cleanup_name) {
                    (cleanup_name, handle, false)
                } else {
                    return Err(recovery_error("leased transaction directory is missing"));
                };
            let directory = root.path.join(REGISTRY_NAME).join(&name);
            let journal = load_journal_handle(&root, &directory_handle, &directory, &lease.nonce)?;
            validate_parent_lease(parent, &directory_handle, &journal, &lease)?;
            let key = (root.identity.clone(), lease.nonce.clone());
            references.entry(key).or_insert(RecoveryReference {
                root: root.path.clone(),
                directory,
                directory_handle,
                initial,
            });
        }
        if references.is_empty() {
            return Ok(());
        }
        if references.len() > MAX_RECOVERY_TRANSACTIONS {
            return Err(CliError::new(
                ExitClass::Io,
                "transactionRecoveryLimit",
                "too many physical parent transactions require recovery",
            ));
        }
        for reference in references.into_values() {
            let lock = try_recovery_lock_handle(&reference.directory_handle)
                .map_err(|error| recovery_failed("authenticate transaction lock", &error))?
                .ok_or_else(|| {
                    CliError::new(
                        ExitClass::Io,
                        "transactionBusy",
                        format!(
                            "an active output transaction covers {}",
                            reference.directory.display()
                        ),
                    )
                })?;
            if reference.initial {
                recover_initial_transaction(
                    &reference.root,
                    &reference.directory,
                    &reference.directory_handle,
                    Some(lock),
                )?;
            } else if reference
                .directory
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(CLEANUP_PREFIX))
            {
                recover_cleanup_transaction(
                    &reference.root,
                    &reference.directory,
                    &reference.directory_handle,
                    Some(lock),
                )?;
            } else {
                recover_transaction(&reference.root, &reference.directory, Some(lock)).map_err(
                    |error| recovery_failed("recover physical parent transaction", &error),
                )?;
            }
        }
        Err(CliError::new(
            ExitClass::Io,
            "transactionRecoveredRetry",
            "an interrupted transaction covering this target was recovered; retry the write",
        ))
    }
}

/// Recover every exact manager transaction directory directly under `root`.
#[cfg(all(test, any(unix, windows)))]
pub fn recover_pending(root: &Path) -> Result<(), CliError> {
    let root = root.canonicalize()?;
    let root_handle = SafeDir::open_absolute(&root)?;
    let Some(registry) = transaction_registry(&root_handle, false)? else { return Ok(()) };
    let active =
        active_transactions().lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let mut managed = Vec::new();
    let recovery_names = registry.names()?;
    for (scanned, name) in recovery_names.into_iter().enumerate() {
        if scanned >= MAX_RECOVERY_DIRECTORY_ENTRIES {
            return Err(CliError::new(
                ExitClass::Io,
                "transactionRecoveryLimit",
                format!(
                    "recovery scan exceeded {MAX_RECOVERY_DIRECTORY_ENTRIES} entries under {}",
                    root.display()
                ),
            ));
        }
        let Some(nonce) = managed_nonce(&name) else { continue };
        let path = root.join(REGISTRY_NAME).join(&name);
        if active.contains(&path) {
            continue;
        }
        let _ = registry
            .open_child(&name)
            .map_err(|error| recovery_failed("authenticate transaction directory", &error))?;
        managed.push((path, nonce));
        if managed.len() > MAX_RECOVERY_TRANSACTIONS {
            return Err(CliError::new(
                ExitClass::Io,
                "transactionRecoveryLimit",
                format!(
                    "more than {MAX_RECOVERY_TRANSACTIONS} pending transactions under {}",
                    root.display()
                ),
            ));
        }
    }
    managed.sort_by(|left, right| left.0.cmp(&right.0));
    for (directory, nonce) in managed {
        let directory_handle = registry.open_child(
            directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
        )?;
        let Some(lock) = try_recovery_lock_handle(&directory_handle)
            .map_err(|error| recovery_failed("authenticate transaction lock", &error))?
        else {
            continue;
        };
        let journal = load_journal(&root, &directory, &nonce)?;
        if journal.phase == JournalPhase::Committed {
            finish_committed(&root, &directory, &journal, Some(lock))
                .map_err(|error| recovery_failed("finish committed transaction", &error))?;
        } else {
            rollback_transaction(&root, &directory, &journal, Some(lock))
                .map_err(|error| recovery_failed("rollback interrupted transaction", &error))?;
        }
    }
    recover_cleanup_directories(&registry)?;
    Ok(())
}

#[cfg(all(test, any(unix, windows)))]
fn recover_cleanup_directories(registry: &SafeDir) -> Result<(), CliError> {
    for name in registry.names()? {
        let Some(nonce) =
            name.to_str().and_then(|name| name.strip_prefix(CLEANUP_PREFIX)).filter(|nonce| {
                nonce.len() == 32
                    && nonce
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
        else {
            continue;
        };
        let cleanup = registry.open_child(&name)?;
        for member in cleanup.names()? {
            if !matches!(
                member.to_str(),
                Some("journal-a.json" | "journal-b.json" | "transaction.lock")
            ) {
                return Err(recovery_error(format!(
                    "unexpected cleanup member for transaction {nonce}"
                )));
            }
            remove_regular_handle_if_present(&cleanup, &member)?;
        }
        cleanup.sync()?;
        drop(cleanup);
        registry.remove_empty_child(&name)?;
        registry.sync()?;
    }
    Ok(())
}

fn recover_transaction(root: &Path, directory: &Path, lock: Option<File>) -> Result<(), CliError> {
    let name = directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?;
    let nonce = managed_nonce(name).ok_or_else(|| recovery_error("invalid transaction name"))?;
    let journal = load_journal(root, directory, &nonce)?;
    if journal.phase == JournalPhase::Committed {
        finish_committed(root, directory, &journal, lock)
    } else {
        rollback_transaction(root, directory, &journal, lock)
    }
}

#[cfg(any(unix, windows))]
fn recover_initial_transaction(
    root: &Path,
    directory: &Path,
    directory_handle: &SafeDir,
    lock: Option<File>,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("initial recovery requires an owned lock"))?;
    let nonce = directory
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(INITIAL_PREFIX))
        .ok_or_else(|| recovery_error("invalid initial transaction name"))?;
    let root_handle = SafeDir::open_absolute(root)?;
    let journal = load_journal_handle(&root_handle, directory_handle, directory, nonce)?;
    if journal.phase != JournalPhase::Staging {
        return Err(recovery_error("initial transaction has advanced beyond staging"));
    }
    validate_recovery_layout(root, directory, &journal)?;
    let parents = journal_parent_handles(&root_handle, &journal)?;
    remove_parent_leases(&parents, directory_handle, &journal)?;
    drop(lock);
    let registry = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("initial transaction registry is missing"))?;
    remove_initial_transaction_with_external_lock(
        &registry,
        directory.file_name().ok_or_else(|| recovery_error("initial transaction has no name"))?,
        &journal.nonce,
    )
}

#[cfg(any(unix, windows))]
fn recover_cleanup_transaction(
    root: &Path,
    directory: &Path,
    directory_handle: &SafeDir,
    lock: Option<File>,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("cleanup recovery requires an owned lock"))?;
    let nonce = directory
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix(CLEANUP_PREFIX))
        .ok_or_else(|| recovery_error("invalid cleanup transaction name"))?;
    let root_handle = SafeDir::open_absolute(root)?;
    let journal = load_journal_handle(&root_handle, directory_handle, directory, nonce)?;
    validate_recovery_layout(root, directory, &journal)?;
    let parents = journal_parent_handles(&root_handle, &journal)?;
    remove_parent_leases(&parents, directory_handle, &journal)?;
    drop(lock);
    let registry = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("cleanup transaction registry is missing"))?;
    #[cfg(windows)]
    remove_external_lock_if_present(&registry, nonce)?;
    for name in ["journal-a.json", "journal-b.json", "transaction.lock"] {
        remove_regular_handle_if_present(directory_handle, OsStr::new(name))?;
    }
    directory_handle.sync()?;
    registry.remove_empty_child(
        directory.file_name().ok_or_else(|| recovery_error("cleanup has no name"))?,
    )?;
    registry.sync()
}

#[cfg(windows)]
fn remove_external_lock_if_present(registry: &SafeDir, nonce: &str) -> Result<(), CliError> {
    let name = OsString::from(format!("{EXTERNAL_LOCK_PREFIX}{nonce}"));
    match registry.inspect_regular(&name) {
        Ok(Some(_)) => registry.remove_regular_private(&name),
        Ok(None) => Ok(()),
        Err(_) => {
            if registry.inspect_lease_file(&name)?.is_some() {
                registry.remove_lease_file(&name)
            } else {
                Ok(())
            }
        }
    }
}

fn rollback_transaction(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
) -> Result<(), CliError> {
    validate_recovery_layout(root, directory, journal)?;
    #[cfg(any(unix, windows))]
    let root_handle = SafeDir::open_absolute(root)?;
    #[cfg(any(unix, windows))]
    let registry_handle = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("transaction registry is missing"))?;
    #[cfg(any(unix, windows))]
    let directory_handle = registry_handle.open_child(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )?;
    #[cfg(any(unix, windows))]
    let targets = authenticate_targets(&root_handle, &journal.entries)?;
    #[cfg(any(unix, windows))]
    rollback_transaction_with_handles(&directory_handle, &targets, journal)?;
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    remove_transaction_directory(root, directory, journal, lock)
}

#[cfg(any(unix, windows))]
fn rollback_transaction_with_handles(
    directory: &SafeDir,
    targets: &[AuthenticatedTarget],
    journal: &Journal,
) -> Result<(), CliError> {
    let mut failures = Vec::new();
    for (index, entry) in journal.entries.iter().enumerate().rev() {
        let result = rollback_entry_handle(directory, &targets[index], journal, index, entry);
        if let Err(error) = result
            && failures.len() < 16
        {
            failures.push(format!("entry {index}: {}: {}", error.code(), error.message()));
        }
    }
    if !failures.is_empty() {
        return Err(CliError::new(
            ExitClass::Io,
            "rollbackFailed",
            format!(
                "one or more rollback operations failed; journal and backups were preserved: {}",
                failures.join("; ")
            ),
        ));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn rollback_entry_handle(
    directory: &SafeDir,
    target: &AuthenticatedTarget,
    journal: &Journal,
    index: usize,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    let backup = backup_name(index);
    let staged = stage_name(index);
    let backup_identity = directory.inspect_regular(&backup)?;
    let target_identity = target.parent.inspect_regular(&target.name)?;

    if let Some(original) = &entry.original {
        if let Some(found) = &backup_identity {
            if found != original {
                return Err(recovery_error(format!(
                    "backup identity mismatch: {}",
                    directory.path.join(&backup).display()
                )));
            }
            if target_identity.is_some() {
                verify_handle_content(target, entry)?;
                target.parent.remove_regular(&target.name)?;
                target.parent.sync()?;
            }
            handle_rename(directory, &backup, &target.parent, &target.name)?;
            target.parent.sync()?;
            directory.sync()?;
        } else {
            let Some(found) = target_identity else {
                return Err(recovery_error(format!(
                    "original and backup are both missing: {}",
                    target.parent.path.join(&target.name).display()
                )));
            };
            if &found != original {
                return Err(recovery_error(format!(
                    "original identity mismatch: {}",
                    target.parent.path.join(&target.name).display()
                )));
            }
        }
    } else if target_identity.is_some() {
        verify_handle_content(target, entry)?;
        target.parent.remove_regular(&target.name)?;
        target.parent.sync()?;
    }

    if directory.inspect_regular(&staged)?.is_some() {
        if journal.phase != JournalPhase::Staging {
            verify_file_content(
                directory.open_regular(&staged)?,
                &directory.path.join(&staged),
                entry,
            )?;
        }
        directory.remove_regular(&staged)?;
        directory.sync()?;
    }
    Ok(())
}

fn finish_committed(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
) -> Result<(), CliError> {
    validate_recovery_layout(root, directory, journal)?;
    #[cfg(any(unix, windows))]
    let root_handle = SafeDir::open_absolute(root)?;
    #[cfg(any(unix, windows))]
    let registry_handle = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("transaction registry is missing"))?;
    #[cfg(any(unix, windows))]
    let directory_handle = registry_handle.open_child(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )?;
    #[cfg(any(unix, windows))]
    let targets = authenticate_targets(&root_handle, &journal.entries)?;
    #[cfg(any(unix, windows))]
    for (index, (entry, target)) in journal.entries.iter().zip(&targets).enumerate() {
        verify_handle_content(target, entry)?;
        let backup = backup_name(index);
        if let Some(identity) = directory_handle.inspect_regular(&backup)? {
            if entry.original.as_ref() != Some(&identity) {
                return Err(recovery_error(format!(
                    "committed backup identity mismatch: {}",
                    directory_handle.path.join(&backup).display()
                )));
            }
            directory_handle.remove_committed_backup(&backup, &identity)?;
        }
        let staged = stage_name(index);
        if directory_handle.inspect_regular(&staged)?.is_some() {
            verify_file_content(
                directory_handle.open_regular(&staged)?,
                &directory_handle.path.join(&staged),
                entry,
            )?;
            directory_handle.remove_regular(&staged)?;
        }
        directory_handle.sync()?;
    }
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    remove_transaction_directory(root, directory, journal, lock)
}

fn validate_recovery_layout(
    root: &Path,
    directory: &Path,
    journal: &Journal,
) -> Result<(), CliError> {
    validate_journal(root, directory, journal)?;
    let allowed = allowed_transaction_names(journal);
    #[cfg(any(unix, windows))]
    let directory_handle = SafeDir::open_absolute(directory)?;
    #[cfg(any(unix, windows))]
    for name in directory_handle.names()? {
        if !allowed.contains(&name) {
            return Err(recovery_error(format!(
                "unexpected transaction member: {}",
                directory.join(&name).display()
            )));
        }
        if name.to_string_lossy().starts_with(PARENT_MARKER_PREFIX) {
            inspect_transaction_lease_member(&directory_handle, &name)?
                .ok_or_else(|| recovery_error("transaction parent marker disappeared"))?;
        } else {
            let _ = directory_handle.open_regular(&name)?;
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

fn remove_transaction_directory(
    root: &Path,
    directory: &Path,
    journal: &Journal,
    lock: Option<File>,
) -> Result<(), CliError> {
    let lock = lock.ok_or_else(|| recovery_error("transaction cleanup requires an owned lock"))?;
    #[cfg(any(unix, windows))]
    let root_handle = SafeDir::open_absolute(root)?;
    #[cfg(any(unix, windows))]
    let registry_handle = transaction_registry(&root_handle, false)?
        .ok_or_else(|| recovery_error("transaction registry is missing"))?;
    #[cfg(any(unix, windows))]
    let directory_handle = registry_handle.open_child(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )?;
    #[cfg(any(unix, windows))]
    for index in 0..journal.entries.len() {
        remove_regular_handle_if_present(&directory_handle, &stage_name(index))?;
        remove_regular_handle_if_present(&directory_handle, &backup_name(index))?;
    }
    #[cfg(any(unix, windows))]
    directory_handle.sync()?;

    // Atomically remove the directory from the recovery namespace while its
    // signed journals and exclusive lock still exist. Cleanup failures after
    // this point cannot cause a later recovery to reinterpret a completed set.
    let nonce = managed_nonce(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )
    .ok_or_else(|| recovery_error("transaction directory name is invalid"))?;
    let cleanup_name = OsString::from(format!("{CLEANUP_PREFIX}{nonce}"));
    #[cfg(windows)]
    let external_lock_name = OsString::from(format!("{EXTERNAL_LOCK_PREFIX}{nonce}"));
    #[cfg(any(unix, windows))]
    if registry_handle.open_child_optional(&cleanup_name)?.is_some() {
        return Err(recovery_error("transaction cleanup path already exists"));
    }
    #[cfg(windows)]
    {
        match directory_handle.inspect_regular(OsStr::new("transaction.lock")) {
            Ok(Some(_)) => {
                fs::hard_link(
                    directory_handle.path.join("transaction.lock"),
                    registry_handle.path.join(&external_lock_name),
                )?;
            }
            Ok(None) => {
                if registry_handle.inspect_regular(&external_lock_name)?.is_none() {
                    return Err(recovery_error("cleanup lock handoff disappeared"));
                }
            }
            Err(_) => {
                if directory_handle.inspect_lease_file(OsStr::new("transaction.lock"))?.is_none()
                    || registry_handle.inspect_lease_file(&external_lock_name)?.is_none()
                {
                    return Err(recovery_error("cleanup lock handoff identity mismatch"));
                }
            }
        }
        if directory_handle.inspect_lease_file(OsStr::new("transaction.lock"))?.is_some() {
            directory_handle.remove_lease_file(OsStr::new("transaction.lock"))?;
        }
        registry_handle.sync()?;
    }
    #[cfg(any(unix, windows))]
    handle_rename(
        &registry_handle,
        directory.file_name().expect("transaction name checked"),
        &registry_handle,
        &cleanup_name,
    )?;
    #[cfg(any(unix, windows))]
    registry_handle.sync()?;

    #[cfg(any(unix, windows))]
    let cleanup_handle = registry_handle.open_child(&cleanup_name)?;
    #[cfg(any(unix, windows))]
    let parents = journal_parent_handles(&root_handle, journal)?;
    #[cfg(any(unix, windows))]
    remove_parent_leases(&parents, &cleanup_handle, journal)?;
    drop(lock);
    #[cfg(windows)]
    registry_handle.remove_regular_private(&external_lock_name)?;
    #[cfg(any(unix, windows))]
    for name in ["journal-a.json", "journal-b.json", "transaction.lock"] {
        remove_regular_handle_if_present(&cleanup_handle, OsStr::new(name))?;
    }
    #[cfg(any(unix, windows))]
    cleanup_handle.sync()?;
    #[cfg(any(unix, windows))]
    drop(cleanup_handle);
    #[cfg(any(unix, windows))]
    registry_handle.remove_empty_child(&cleanup_name)?;
    #[cfg(any(unix, windows))]
    registry_handle.sync()?;
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

#[cfg(any(unix, windows))]
fn remove_regular_handle_if_present(directory: &SafeDir, name: &OsStr) -> Result<(), CliError> {
    match directory.inspect_regular(name) {
        Ok(Some(_)) => {
            directory.remove_regular(name)?;
        }
        Ok(None) => {}
        Err(error) if error.code() == "outputTargetTypeDenied" => return Err(error),
        Err(error) => {
            if !error.message().contains("No such file or directory")
                && !error.message().contains("os error 2")
            {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn allowed_transaction_names(journal: &Journal) -> BTreeSet<OsString> {
    let mut names = BTreeSet::from([
        OsString::from("journal-a.json"),
        OsString::from("journal-b.json"),
        OsString::from("transaction.lock"),
    ]);
    for index in 0..journal.entries.len() {
        names.insert(OsString::from(format!("stage-{index}")));
        names.insert(OsString::from(format!("backup-{index}")));
    }
    for identity in &journal.parent_identities {
        names.insert(parent_marker_name(identity));
    }
    names
}

#[cfg(any(unix, windows))]
fn persist_journal_handle(directory: &SafeDir, journal: &mut Journal) -> Result<(), CliError> {
    journal.generation = journal.generation.checked_add(1).ok_or_else(|| {
        CliError::new(ExitClass::Io, "transactionJournalOverflow", "journal generation overflow")
    })?;
    let name =
        if journal.generation.is_multiple_of(2) { "journal-b.json" } else { "journal-a.json" };
    let name = OsStr::new(name);
    if directory.inspect_regular(name)?.is_some() {
        directory.remove_regular(name)?;
        directory.sync()?;
    }
    let bytes = serde_json::to_vec(journal).map_err(|error| {
        CliError::internal(format!("serialize output transaction journal: {error}"))
    })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_JOURNAL_BYTES {
        return Err(CliError::new(
            ExitClass::Policy,
            "transactionJournalLimit",
            "output transaction journal exceeds its byte limit",
        ));
    }
    let mut file = directory.create_regular(name)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    directory.sync()
}

#[cfg(not(any(unix, windows)))]
fn persist_journal_handle(_directory: &SafeDir, _journal: &mut Journal) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

fn load_journal(root: &Path, directory: &Path, nonce: &str) -> Result<Journal, CliError> {
    #[cfg(any(unix, windows))]
    {
        let root_handle = SafeDir::open_absolute(root)?;
        let registry = transaction_registry(&root_handle, false)?
            .ok_or_else(|| recovery_error("transaction registry is missing"))?;
        let directory_name = directory
            .file_name()
            .ok_or_else(|| recovery_error("transaction directory has no name"))?;
        let directory_handle = registry.open_child(directory_name)?;
        load_journal_handle(&root_handle, &directory_handle, directory, nonce)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, directory, nonce);
        Err(transaction_platform_unavailable())
    }
}

#[cfg(any(unix, windows))]
fn load_journal_handle(
    root: &SafeDir,
    directory: &SafeDir,
    directory_path: &Path,
    nonce: &str,
) -> Result<Journal, CliError> {
    let mut candidates = Vec::new();
    for name in ["journal-a.json", "journal-b.json"] {
        match read_limited_regular_handle(directory, OsStr::new(name), MAX_JOURNAL_BYTES) {
            Ok(bytes) => {
                if let Ok(journal) = serde_json::from_slice::<Journal>(&bytes)
                    && validate_journal_handle(root, directory_path, &journal).is_ok()
                    && journal.nonce == nonce
                {
                    candidates.push(journal);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    candidates.sort_by_key(|journal| journal.generation);
    let journal = candidates.pop().ok_or_else(|| {
        recovery_error(format!("no valid signed journal in {}", directory_path.display()))
    })?;
    if candidates.last().is_some_and(|other| other.generation == journal.generation) {
        return Err(recovery_error("ambiguous journal generations"));
    }
    Ok(journal)
}

fn validate_journal(root: &Path, directory: &Path, journal: &Journal) -> Result<(), CliError> {
    #[cfg(any(unix, windows))]
    {
        let root_handle = SafeDir::open_absolute(root)?;
        validate_journal_handle(&root_handle, directory, journal)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, directory, journal);
        Err(transaction_platform_unavailable())
    }
}

#[cfg(any(unix, windows))]
fn validate_journal_handle(
    root: &SafeDir,
    directory: &Path,
    journal: &Journal,
) -> Result<(), CliError> {
    if journal.signature != JOURNAL_SIGNATURE || journal.version != JOURNAL_VERSION {
        return Err(recovery_error("invalid transaction signature or version"));
    }
    let valid_nonce = journal.nonce.len() == 32
        && journal.nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    let valid_names = [TRANSACTION_PREFIX, INITIAL_PREFIX, CLEANUP_PREFIX]
        .map(|prefix| OsString::from(format!("{prefix}{}", journal.nonce)));
    if !valid_nonce || !valid_names.iter().any(|name| directory.file_name() == Some(name)) {
        return Err(recovery_error("transaction nonce does not match directory"));
    }
    if journal.entries.is_empty() || journal.entries.len() > MAX_JOURNAL_ENTRIES {
        return Err(recovery_error("transaction entry count is outside limits"));
    }
    let encoded_root = decode_path(&journal.root)?;
    if encoded_root != root.path || journal.root_identity != root.identity {
        return Err(recovery_error("transaction root does not match recovery root"));
    }
    let mut targets = BTreeSet::new();
    for entry in &journal.entries {
        if entry.content_sha256.len() != 64
            || !entry.content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(recovery_error("invalid transaction content digest"));
        }
        let relative = decode_path(&entry.target)?;
        validate_relative_path(&relative)?;
        if !targets.insert(entry.target.units.clone()) {
            return Err(recovery_error("duplicate transaction target"));
        }
    }
    if journal.parent_identities.is_empty()
        || journal.parent_identities.len() > journal.entries.len()
        || !journal.parent_identities.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(recovery_error("journal physical parent identities are invalid"));
    }
    let _ = journal_parent_handles(root, journal)?;
    Ok(())
}

fn managed_nonce(name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    let nonce = name.strip_prefix(TRANSACTION_PREFIX)?;
    (nonce.len() == 32
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then(|| nonce.to_owned())
}

#[cfg(unix)]
fn create_initial_transaction(root: &Path) -> Result<(String, PathBuf, PathBuf, File), CliError> {
    let root_handle = SafeDir::open_absolute(root)?;
    let registry = transaction_registry(&root_handle, true)?
        .ok_or_else(|| recovery_error("transaction registry could not be created"))?;
    for attempt in 0_u32..128 {
        let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let digest =
            Sha256::digest(format!("{}:{time}:{counter}:{attempt}", std::process::id()).as_bytes());
        let nonce = hex_bytes(&digest[..16]);
        let initial_name = OsString::from(format!("{INITIAL_PREFIX}{nonce}"));
        let directory_name = OsString::from(format!("{TRANSACTION_PREFIX}{nonce}"));
        let initial = root.join(REGISTRY_NAME).join(&initial_name);
        let directory = root.join(REGISTRY_NAME).join(&directory_name);
        match rustix::fs::mkdirat(
            &registry.fd,
            &initial_name,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
        ) {
            Ok(()) => {
                let initial_handle = match registry.open_child(&initial_name) {
                    Ok(handle) => handle,
                    Err(error) => {
                        let _ = remove_initial_transaction_handle(&registry, &initial_name);
                        return Err(error);
                    }
                };
                let lock = match initial_handle.create_regular(OsStr::new("transaction.lock")) {
                    Ok(lock) => lock,
                    Err(error) => {
                        let _ = remove_initial_transaction_handle(&registry, &initial_name);
                        return Err(error);
                    }
                };
                if let Err(error) = lock.try_lock() {
                    drop(lock);
                    let _ = remove_initial_transaction_handle(&registry, &initial_name);
                    return Err(lock_error("create transaction lock", &error));
                }
                if let Err(error) = lock
                    .sync_all()
                    .map_err(CliError::from)
                    .and_then(|()| initial_handle.sync())
                    .and_then(|()| registry.sync())
                {
                    drop(lock);
                    let _ = remove_initial_transaction_handle(&registry, &initial_name);
                    return Err(error);
                }
                return Ok((nonce, initial, directory, lock));
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(CliError::new(
        ExitClass::Io,
        "transactionAllocationFailed",
        "could not allocate an output transaction directory",
    ))
}

#[cfg(windows)]
fn create_initial_transaction(root: &Path) -> Result<(String, PathBuf, PathBuf, File), CliError> {
    let root_handle = SafeDir::open_absolute(root)?;
    let registry = transaction_registry(&root_handle, true)?
        .ok_or_else(|| recovery_error("transaction registry could not be created"))?;
    for attempt in 0_u32..128 {
        let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let digest =
            Sha256::digest(format!("{}:{time}:{counter}:{attempt}", std::process::id()).as_bytes());
        let nonce = hex_bytes(&digest[..16]);
        let initial_name = OsString::from(format!("{INITIAL_PREFIX}{nonce}"));
        let directory_name = OsString::from(format!("{TRANSACTION_PREFIX}{nonce}"));
        let initial = root.join(REGISTRY_NAME).join(&initial_name);
        let directory = root.join(REGISTRY_NAME).join(&directory_name);
        let initial_handle = match registry.create_child_private(&initial_name) {
            Ok(handle) => handle,
            Err(error) if error.message().contains("already exists") => continue,
            Err(error) => return Err(error),
        };
        // Keep the locked handle outside the directory during publication.
        // Windows otherwise denies renaming a directory which contains the
        // locked file, even when every directory handle shares deletion.
        let external_lock_name = OsString::from(format!("{EXTERNAL_LOCK_PREFIX}{nonce}"));
        let lock = match registry.create_regular_private(&external_lock_name) {
            Ok(lock) => lock,
            Err(error) => {
                let _ =
                    remove_initial_transaction_with_external_lock(&registry, &initial_name, &nonce);
                return Err(error);
            }
        };
        if let Err(error) = lock.try_lock() {
            drop(lock);
            let _ = remove_initial_transaction_with_external_lock(&registry, &initial_name, &nonce);
            return Err(lock_error("create transaction lock", &error));
        }
        if let Err(error) = fs::hard_link(
            registry.path.join(&external_lock_name),
            initial_handle.path.join("transaction.lock"),
        ) {
            drop(lock);
            let _ = remove_initial_transaction_with_external_lock(&registry, &initial_name, &nonce);
            return Err(error.into());
        }
        let publication = (|| {
            if registry.inspect_lease_file(&external_lock_name)?.is_none()
                || initial_handle.inspect_lease_file(OsStr::new("transaction.lock"))?.is_none()
            {
                return Err(recovery_error("transaction lock link identity mismatch"));
            }
            lock.sync_all()?;
            initial_handle.sync()?;
            registry.sync()
        })();
        if let Err(error) = publication {
            drop(lock);
            let cleanup =
                remove_initial_transaction_with_external_lock(&registry, &initial_name, &nonce);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "transaction lock publication failed ({}: {}); cleanup failed ({}: {})",
                        error.code(),
                        error.message(),
                        cleanup.code(),
                        cleanup.message()
                    ),
                )),
            };
        }
        return Ok((nonce, initial, directory, lock));
    }
    Err(CliError::new(
        ExitClass::Io,
        "transactionAllocationFailed",
        "could not allocate an output transaction directory",
    ))
}

#[cfg(not(any(unix, windows)))]
fn create_initial_transaction(_root: &Path) -> Result<(String, PathBuf, PathBuf, File), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
fn remove_initial_transaction_handle(registry: &SafeDir, name: &OsStr) -> Result<(), CliError> {
    let name_text =
        name.to_str().ok_or_else(|| recovery_error("initial transaction name is invalid"))?;
    let nonce = name_text
        .strip_prefix(INITIAL_PREFIX)
        .filter(|nonce| {
            nonce.len() == 32
                && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .ok_or_else(|| recovery_error("initial transaction name is not manager-owned"))?;
    let expected = format!("{INITIAL_PREFIX}{nonce}");
    if name != OsStr::new(&expected) {
        return Err(recovery_error("initial transaction nonce mismatch"));
    }
    let transaction = registry.open_child(name)?;
    for member in transaction.names()? {
        if !matches!(
            member.to_str(),
            Some("journal-a.json" | "journal-b.json" | "transaction.lock")
        ) && !member.to_string_lossy().starts_with(PARENT_MARKER_PREFIX)
        {
            return Err(recovery_error("unexpected initial transaction member"));
        }
        remove_regular_handle_if_present(&transaction, &member)?;
    }
    transaction.sync()?;
    registry.remove_empty_child(name)?;
    registry.sync()?;
    Ok(())
}

#[cfg(any(unix, windows))]
fn remove_initial_transaction_with_external_lock(
    registry: &SafeDir,
    name: &OsStr,
    nonce: &str,
) -> Result<(), CliError> {
    #[cfg(windows)]
    remove_external_lock_if_present(registry, nonce)?;
    #[cfg(not(windows))]
    let _ = nonce;
    remove_initial_transaction_handle(registry, name)
}

#[cfg(any(unix, windows))]
fn try_recovery_lock_handle(directory: &SafeDir) -> Result<Option<File>, CliError> {
    #[cfg(unix)]
    let lock = directory.open_regular(OsStr::new("transaction.lock"))?;
    #[cfg(windows)]
    let lock = match directory.open_regular(OsStr::new("transaction.lock")) {
        Ok(lock) => lock,
        Err(_) => match directory.open_lease_file(OsStr::new("transaction.lock")) {
            Ok(lock) => lock,
            Err(_) => {
                let nonce = transaction_directory_nonce(
                    directory
                        .path
                        .file_name()
                        .ok_or_else(|| recovery_error("transaction directory has no name"))?,
                )
                .ok_or_else(|| recovery_error("transaction directory name is invalid"))?;
                let registry_path = directory
                    .path
                    .parent()
                    .ok_or_else(|| recovery_error("transaction registry path is invalid"))?;
                let registry = SafeDir::open_absolute(registry_path)?;
                registry.open_regular_private(&OsString::from(format!(
                    "{EXTERNAL_LOCK_PREFIX}{nonce}"
                )))?
            }
        },
    };
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(lock_error(
            &format!("lock transaction for recovery: {}", directory.path.display()),
            &error,
        )),
    }
}

#[cfg(windows)]
fn transaction_directory_nonce(name: &OsStr) -> Option<&str> {
    let name = name.to_str()?;
    [TRANSACTION_PREFIX, INITIAL_PREFIX, CLEANUP_PREFIX]
        .into_iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .filter(|nonce| {
            nonce.len() == 32
                && nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

#[cfg(windows)]
fn verify_manager_directory(path: &Path) -> Result<(), CliError> {
    let handle = securely_open_regular_or_directory(path)?;
    if !handle.metadata()?.is_dir() {
        return Err(recovery_error(format!(
            "transaction path is not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_manager_directory(_path: &Path) -> Result<(), CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "secure transaction directories are unavailable",
    ))
}

fn lock_error(operation: &str, error: &std::fs::TryLockError) -> CliError {
    CliError::new(ExitClass::Io, "transactionLockFailed", format!("{operation}: {error}"))
}

fn target_path(root: &Path, entry: &JournalEntry) -> Result<PathBuf, CliError> {
    let relative = decode_path(&entry.target)?;
    validate_relative_path(&relative)?;
    Ok(root.join(relative))
}

fn stage_name(index: usize) -> OsString {
    OsString::from(format!("stage-{index}"))
}

fn backup_name(index: usize) -> OsString {
    OsString::from(format!("backup-{index}"))
}

fn parent_marker_name(identity: &FileIdentity) -> OsString {
    let mut digest = Sha256::new();
    digest.update(identity.platform.as_bytes());
    digest.update([0]);
    digest.update(identity.first.to_le_bytes());
    digest.update(identity.second.to_le_bytes());
    OsString::from(format!("{PARENT_MARKER_PREFIX}{}.json", hex_bytes(&digest.finalize())))
}

#[cfg(unix)]
fn inspect_linked_parent_lease(parent: &SafeDir) -> Result<Option<FileIdentity>, CliError> {
    parent.inspect_regular(OsStr::new(PARENT_LEASE_NAME))
}

#[cfg(windows)]
fn inspect_linked_parent_lease(parent: &SafeDir) -> Result<Option<FileIdentity>, CliError> {
    parent.inspect_lease_file(OsStr::new(PARENT_LEASE_NAME))
}

#[cfg(unix)]
fn inspect_transaction_lease_member(
    transaction: &SafeDir,
    name: &OsStr,
) -> Result<Option<FileIdentity>, CliError> {
    transaction.inspect_regular(name)
}

#[cfg(windows)]
fn inspect_transaction_lease_member(
    transaction: &SafeDir,
    name: &OsStr,
) -> Result<Option<FileIdentity>, CliError> {
    match transaction.inspect_regular(name) {
        Ok(identity) => Ok(identity),
        Err(_) => transaction.inspect_lease_file(name),
    }
}

#[cfg(unix)]
fn read_linked_parent_lease(parent: &SafeDir, limit: u64) -> io::Result<Vec<u8>> {
    read_limited_regular_handle(parent, OsStr::new(PARENT_LEASE_NAME), limit)
}

#[cfg(windows)]
fn read_linked_parent_lease(parent: &SafeDir, limit: u64) -> io::Result<Vec<u8>> {
    let mut file = parent
        .open_lease_file(OsStr::new(PARENT_LEASE_NAME))
        .map_err(|error| io::Error::other(error.to_string()))?;
    let metadata = file.metadata()?;
    if metadata.len() > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "managed file exceeds its limit"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
fn remove_linked_parent_lease(parent: &SafeDir) -> Result<(), CliError> {
    let name = OsStr::new(PARENT_LEASE_NAME);
    let Some(_) = parent.inspect_regular(name)? else {
        return Ok(());
    };
    parent.verify_namespace()?;
    let file = parent.open_regular(name)?;
    if rustix::fs::fstat(&file)?.st_nlink != 2 {
        return Err(recovery_error("physical parent lease link count is invalid"));
    }
    rustix::fs::unlinkat(&parent.fd, name, rustix::fs::AtFlags::empty())?;
    parent.sync()?;
    parent.verify_namespace()
}

#[cfg(windows)]
fn remove_linked_parent_lease(parent: &SafeDir) -> Result<(), CliError> {
    if parent.inspect_lease_file(OsStr::new(PARENT_LEASE_NAME))?.is_some() {
        parent.remove_lease_file(OsStr::new(PARENT_LEASE_NAME))?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_parent_leases(
    parents: &[SafeDir],
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    for parent in parents {
        let name = parent_marker_name(&parent.identity);
        let lease = ParentLease {
            signature: JOURNAL_SIGNATURE.into(),
            version: JOURNAL_VERSION,
            nonce: journal.nonce.clone(),
            root: journal.root.clone(),
            root_identity: journal.root_identity.clone(),
            parent_identity: parent.identity.clone(),
        };
        let bytes = serde_json::to_vec(&lease)
            .map_err(|error| CliError::internal(format!("serialize parent lease: {error}")))?;
        let mut transaction_file = transaction.create_regular(&name)?;
        transaction_file.write_all(&bytes)?;
        transaction_file.write_all(b"\n")?;
        transaction_file.sync_all()?;
        transaction.sync()?;
        rustix::fs::linkat(
            &transaction.fd,
            &name,
            &parent.fd,
            PARENT_LEASE_NAME,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                CliError::new(
                    ExitClass::Io,
                    "transactionBusy",
                    format!("another output transaction owns parent {}", parent.path.display()),
                )
            } else {
                error.into()
            }
        })?;
        parent.sync()?;
    }
    transaction.sync()
}

#[cfg(windows)]
fn create_parent_leases(
    parents: &[SafeDir],
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    for parent in parents {
        let name = parent_marker_name(&parent.identity);
        let lease = ParentLease {
            signature: JOURNAL_SIGNATURE.into(),
            version: JOURNAL_VERSION,
            nonce: journal.nonce.clone(),
            root: journal.root.clone(),
            root_identity: journal.root_identity.clone(),
            parent_identity: parent.identity.clone(),
        };
        let bytes = serde_json::to_vec(&lease)
            .map_err(|error| CliError::internal(format!("serialize parent lease: {error}")))?;
        let mut transaction_file = transaction.create_regular_private(&name)?;
        transaction_file.write_all(&bytes)?;
        transaction_file.write_all(b"\n")?;
        transaction_file.sync_all()?;
        let source_identity = file_identity(&transaction_file)?;
        transaction.sync()?;
        parent.verify_namespace()?;
        match fs::hard_link(transaction.path.join(&name), parent.path.join(PARENT_LEASE_NAME)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CliError::new(
                    ExitClass::Io,
                    "transactionBusy",
                    format!("another output transaction owns parent {}", parent.path.display()),
                ));
            }
            Err(error) => return Err(error.into()),
        }
        parent.sync()?;
        if inspect_linked_parent_lease(parent)?.as_ref() != Some(&source_identity)
            || inspect_transaction_lease_member(transaction, &name)?.as_ref()
                != Some(&source_identity)
        {
            return Err(recovery_error("physical parent lease identity mismatch"));
        }
    }
    transaction.sync()
}

#[cfg(any(unix, windows))]
fn load_parent_lease(parent: &SafeDir) -> Result<Option<ParentLease>, CliError> {
    if inspect_linked_parent_lease(parent)?.is_none() {
        return Ok(None);
    }
    let bytes = read_linked_parent_lease(parent, 8 * 1024).map_err(CliError::from)?;
    let lease: ParentLease =
        serde_json::from_slice(&bytes).map_err(|_| recovery_error("parent lease is malformed"))?;
    if lease.signature != JOURNAL_SIGNATURE
        || lease.version != JOURNAL_VERSION
        || lease.nonce.len() != 32
        || !lease.nonce.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || lease.parent_identity != parent.identity
    {
        return Err(recovery_error("parent lease authentication failed"));
    }
    Ok(Some(lease))
}

#[cfg(any(unix, windows))]
fn validate_parent_lease(
    parent: &SafeDir,
    transaction: &SafeDir,
    journal: &Journal,
    lease: &ParentLease,
) -> Result<(), CliError> {
    let name = parent_marker_name(&parent.identity);
    let transaction_identity = inspect_transaction_lease_member(transaction, &name)?;
    let parent_lease_identity = inspect_linked_parent_lease(parent)?;
    let (Some(transaction_identity), Some(parent_lease_identity)) =
        (transaction_identity, parent_lease_identity)
    else {
        return Err(recovery_error("physical parent lease is missing"));
    };
    if transaction_identity != parent_lease_identity {
        return Err(recovery_error("physical parent lease identity mismatch"));
    }
    if lease.signature != JOURNAL_SIGNATURE
        || lease.version != JOURNAL_VERSION
        || lease.nonce != journal.nonce
        || lease.root != journal.root
        || lease.root_identity != journal.root_identity
        || lease.parent_identity != parent.identity
        || journal.parent_identities.binary_search(&parent.identity).is_err()
    {
        return Err(recovery_error("physical parent lease does not match journal"));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn validate_parent_leases(
    transaction: &SafeDir,
    targets: &[AuthenticatedTarget],
    journal: &Journal,
) -> Result<(), CliError> {
    let mut parents = BTreeMap::new();
    for target in targets {
        parents.entry(target.parent.identity.clone()).or_insert(&target.parent);
    }
    for parent in parents.into_values() {
        let lease = load_parent_lease(parent)?
            .ok_or_else(|| recovery_error("physical parent lease is missing"))?;
        validate_parent_lease(parent, transaction, journal, &lease)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn validate_parent_leases(
    _transaction: &SafeDir,
    _targets: &[AuthenticatedTarget],
    _journal: &Journal,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
fn remove_parent_leases(
    parents: &[SafeDir],
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    for parent in parents {
        let marker = parent_marker_name(&parent.identity);
        let transaction_identity = inspect_transaction_lease_member(transaction, &marker)?;
        let parent_identity = inspect_linked_parent_lease(parent)?;
        let Some(transaction_identity) = transaction_identity else {
            continue;
        };
        if parent_identity.as_ref() == Some(&transaction_identity) {
            let lease = load_parent_lease(parent)?
                .ok_or_else(|| recovery_error("physical parent lease disappeared"))?;
            validate_parent_lease(parent, transaction, journal, &lease)?;
            remove_linked_parent_lease(parent)?;
            parent.sync()?;
        }
        remove_regular_handle_if_present(transaction, &marker)?;
        transaction.sync()?;
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn journal_parent_handles(root: &SafeDir, journal: &Journal) -> Result<Vec<SafeDir>, CliError> {
    let mut parents = BTreeMap::new();
    for entry in &journal.entries {
        let relative = decode_path(&entry.target)?;
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = root.open_descendant(parent_relative)?;
        parents.entry(parent.identity.clone()).or_insert(parent);
    }
    let identities = parents.keys().cloned().collect::<Vec<_>>();
    if identities != journal.parent_identities {
        return Err(recovery_error(
            "journal physical parent identities do not match its target paths",
        ));
    }
    Ok(parents.into_values().collect())
}

#[cfg(any(unix, windows))]
fn authenticate_targets(
    root: &SafeDir,
    entries: &[JournalEntry],
) -> Result<Vec<AuthenticatedTarget>, CliError> {
    entries
        .iter()
        .map(|entry| {
            let relative = decode_path(&entry.target)?;
            let parent_relative = relative.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            let parent = root.open_descendant(&parent_relative)?;
            parent.verify_namespace()?;
            let name = relative
                .file_name()
                .ok_or_else(|| recovery_error("transaction target has no file name"))?
                .to_os_string();
            Ok(AuthenticatedTarget { parent, name })
        })
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn authenticate_targets(
    _root: &SafeDir,
    _entries: &[JournalEntry],
) -> Result<Vec<AuthenticatedTarget>, CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
fn verify_target_handle_identity(
    target: &AuthenticatedTarget,
    expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    verify_name_identity(&target.parent, &target.name, expected)
}

#[cfg(not(any(unix, windows)))]
fn verify_target_handle_identity(
    _target: &AuthenticatedTarget,
    _expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
fn verify_name_identity(
    directory: &SafeDir,
    name: &OsStr,
    expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    let current = directory.inspect_regular(name)?;
    if current.as_ref() != expected {
        return Err(CliError::new(
            ExitClass::Io,
            "outputIdentityChanged",
            format!(
                "output target changed after preflight: {}",
                directory.path.join(name).display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_name_identity(
    _directory: &SafeDir,
    _name: &OsStr,
    _expected: Option<&FileIdentity>,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
fn handle_rename(
    from_directory: &SafeDir,
    from: &OsStr,
    to_directory: &SafeDir,
    to: &OsStr,
) -> Result<(), CliError> {
    validate_single_name(from)?;
    validate_single_name(to)?;
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    rustix::fs::renameat_with(
        &from_directory.fd,
        from,
        &to_directory.fd,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    #[cfg(windows)]
    from_directory.rename_child_to_no_replace(from, to_directory, to)?;
    #[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
    return Err(transaction_platform_unavailable());
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn handle_rename(
    _from_directory: &SafeDir,
    _from: &OsStr,
    _to_directory: &SafeDir,
    _to: &OsStr,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
fn install_stage_no_replace_handle(
    staged_directory: &SafeDir,
    staged: &OsStr,
    target_directory: &SafeDir,
    target: &OsStr,
) -> Result<(), CliError> {
    validate_single_name(staged)?;
    validate_single_name(target)?;
    #[cfg(windows)]
    {
        handle_rename(staged_directory, staged, target_directory, target)?;
        target_directory.sync()?;
        staged_directory.sync()?;
        return Ok(());
    }
    #[cfg(unix)]
    rustix::fs::linkat(
        &staged_directory.fd,
        staged,
        &target_directory.fd,
        target,
        rustix::fs::AtFlags::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                format!(
                    "output target appeared during commit: {}",
                    target_directory.path.join(target).display()
                ),
            )
        } else {
            CliError::new(
                ExitClass::Io,
                "outputInstallFailed",
                format!(
                    "cannot install staged output without replacing a concurrent target: {}: {error}",
                    target_directory.path.join(target).display()
                ),
            )
        }
    })?;
    #[cfg(unix)]
    {
        target_directory.sync()?;
        rustix::fs::unlinkat(&staged_directory.fd, staged, rustix::fs::AtFlags::empty())?;
        staged_directory.sync()
    }
}

#[cfg(not(any(unix, windows)))]
fn install_stage_no_replace_handle(
    _staged_directory: &SafeDir,
    _staged: &OsStr,
    _target_directory: &SafeDir,
    _target: &OsStr,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

#[cfg(any(unix, windows))]
fn verify_handle_content(
    target: &AuthenticatedTarget,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    verify_file_content(
        target.parent.open_regular(&target.name)?,
        &target.parent.path.join(&target.name),
        entry,
    )
}

#[cfg(not(any(unix, windows)))]
fn verify_handle_content(
    _target: &AuthenticatedTarget,
    _entry: &JournalEntry,
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

fn verify_file_content(
    mut file: File,
    display_path: &Path,
    entry: &JournalEntry,
) -> Result<(), CliError> {
    let metadata = file.metadata()?;
    if metadata.len() != entry.size {
        return Err(recovery_error(format!(
            "transaction content size mismatch: {}",
            display_path.display()
        )));
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if hex_bytes(&digest.finalize()) != entry.content_sha256 {
        return Err(recovery_error(format!(
            "transaction content digest mismatch: {}",
            display_path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<FileIdentity, CliError> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        platform: "unix".into(),
        first: metadata.dev(),
        second: metadata.ino(),
        size: metadata.len(),
    })
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<FileIdentity, CliError> {
    let information = winapi_util::file::information(file)?;
    Ok(FileIdentity {
        platform: "windows".into(),
        first: information.volume_serial_number(),
        second: information.file_index(),
        size: information.file_size(),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File) -> Result<FileIdentity, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "output file identity is unavailable on this platform",
    ))
}

#[cfg(any(unix, windows))]
fn read_limited_regular_handle(
    directory: &SafeDir,
    name: &OsStr,
    limit: u64,
) -> io::Result<Vec<u8>> {
    let file = directory.open_regular(name).map_err(|error| io::Error::other(error.to_string()))?;
    let size = file.metadata()?.len();
    if size > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "journal exceeds limit"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "journal exceeds limit"));
    }
    Ok(bytes)
}

fn validate_relative_path(path: &Path) -> Result<(), CliError> {
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
fn encode_path(path: &Path) -> Result<JournalPath, CliError> {
    use std::os::unix::ffi::OsStrExt as _;
    let units = path.as_os_str().as_bytes().iter().map(|byte| u32::from(*byte)).collect::<Vec<_>>();
    if units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("transaction path exceeds limit"));
    }
    Ok(JournalPath { encoding: "unixBytes".into(), units })
}

#[cfg(windows)]
fn encode_path(path: &Path) -> Result<JournalPath, CliError> {
    use std::os::windows::ffi::OsStrExt as _;
    let units = path.as_os_str().encode_wide().map(u32::from).collect::<Vec<_>>();
    if units.len() > MAX_PATH_UNITS {
        return Err(recovery_error("transaction path exceeds limit"));
    }
    Ok(JournalPath { encoding: "windowsUtf16".into(), units })
}

#[cfg(not(any(unix, windows)))]
fn encode_path(_path: &Path) -> Result<JournalPath, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "journal paths are unavailable on this platform",
    ))
}

#[cfg(unix)]
fn decode_path(path: &JournalPath) -> Result<PathBuf, CliError> {
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
fn decode_path(path: &JournalPath) -> Result<PathBuf, CliError> {
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
fn decode_path(_path: &JournalPath) -> Result<PathBuf, CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "journal paths are unavailable on this platform",
    ))
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, CliError> {
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
fn resolve_existing_parent(path: &Path) -> Result<PathBuf, CliError> {
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

fn common_existing_ancestor(paths: &[PathBuf]) -> Result<PathBuf, CliError> {
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
    // Keep the spelling through which the authenticated directory handle was
    // opened.  A case- or normalization-insensitive filesystem may expose the
    // same physical directory through a different lexical spelling; replacing
    // it with `canonicalize` would make otherwise valid target paths fail the
    // subsequent root-relative check.  Every mutation is still relative to the
    // no-follow handle opened below, and the durable lease records its physical
    // identity.
    Ok(candidate)
}

#[cfg(unix)]
fn transaction_registry(root: &SafeDir, create: bool) -> Result<Option<SafeDir>, CliError> {
    use std::os::unix::fs::PermissionsExt as _;

    match rustix::fs::openat(
        &root.fd,
        REGISTRY_NAME,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => {
            let identity = directory_identity(&fd)?;
            let directory = SafeDir { fd, path: root.path.join(REGISTRY_NAME), identity };
            let metadata = File::from(rustix::io::dup(&directory.fd)?).metadata()?;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(recovery_error("transaction registry is not private"));
            }
            Ok(Some(directory))
        }
        Err(rustix::io::Errno::NOENT) if !create => Ok(None),
        Err(rustix::io::Errno::NOENT) => {
            rustix::fs::mkdirat(
                &root.fd,
                REGISTRY_NAME,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
            )?;
            root.sync()?;
            Ok(Some(root.open_child(OsStr::new(REGISTRY_NAME))?))
        }
        Err(error) => Err(error.into()),
    }
}

/// Remove an empty manager-owned registry after the last in-process transaction
/// releases it. Races are harmless: a non-empty directory simply survives for
/// the next transaction or recovery pass.
#[cfg(any(unix, windows))]
fn try_cleanup_empty_registry(root: &Path) {
    let registry_path = root.join(REGISTRY_NAME);
    let active = active_transactions().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if active.iter().any(|path| path.starts_with(&registry_path)) {
        return;
    }
    drop(active);
    let Ok(root_handle) = SafeDir::open_absolute(root) else { return };
    let Ok(Some(registry)) = transaction_registry(&root_handle, false) else { return };
    let Ok(names) = registry.names() else { return };
    if !names.is_empty() {
        return;
    }
    drop(registry);
    if root_handle.remove_empty_child(OsStr::new(REGISTRY_NAME)).is_ok() {
        let _ = root_handle.sync();
    }
}

#[cfg(not(any(unix, windows)))]
fn try_cleanup_empty_registry(_root: &Path) {}

#[cfg(windows)]
fn transaction_registry(root: &SafeDir, create: bool) -> Result<Option<SafeDir>, CliError> {
    match root.open_child_optional(OsStr::new(REGISTRY_NAME))? {
        Some(directory) => {
            directory.verify_private_namespace()?;
            Ok(Some(directory))
        }
        None if !create => Ok(None),
        None => {
            root.verify_namespace()?;
            into_markdown_process_plugin::create_windows_plugin_store_directory(
                &root.path.join(REGISTRY_NAME),
            )
            .map_err(|error| {
                recovery_error(format!(
                    "create private transaction registry ({}): {error}",
                    root.path.join(REGISTRY_NAME).display()
                ))
            })?;
            root.sync()?;
            let directory = root.open_child(OsStr::new(REGISTRY_NAME))?;
            directory.verify_private_namespace()?;
            Ok(Some(directory))
        }
    }
}

#[cfg(unix)]
fn ensure_same_filesystem(root: &Path, targets: &[PathBuf]) -> Result<(), CliError> {
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
fn ensure_same_filesystem(root: &Path, targets: &[PathBuf]) -> Result<(), CliError> {
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
fn securely_open_regular_or_directory(path: &Path) -> Result<File, CliError> {
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
fn ensure_same_filesystem(_root: &Path, _targets: &[PathBuf]) -> Result<(), CliError> {
    Err(CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "safe output transactions are unavailable",
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn recovery_error(detail: impl Into<String>) -> CliError {
    CliError::new(ExitClass::Io, "transactionRecoveryFailed", detail)
}

fn recovery_failed(operation: &str, error: &CliError) -> CliError {
    CliError::new(
        ExitClass::Io,
        "transactionRecoveryFailed",
        format!("{operation}: {}: {}", error.code(), error.message()),
    )
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use into_markdown::{ExecutionOptions, ResourceLimits};
    use std::sync::{Arc, Barrier};

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn manager_directories(root: &Path) -> Vec<PathBuf> {
        let registry = root.join(REGISTRY_NAME);
        let Ok(entries) = fs::read_dir(registry) else { return Vec::new() };
        entries
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name();
                managed_nonce(&name).map(|_| entry.path())
            })
            .collect()
    }

    #[test]
    fn successful_transaction_removes_the_empty_registry() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output = root.join("document.md");
        prepare(&[Target { path: output.clone(), bytes: b"document" }], false, &context())
            .unwrap()
            .commit()
            .unwrap();

        assert_eq!(fs::read(output).unwrap(), b"document");
        assert!(!root.join(REGISTRY_NAME).exists());
    }

    #[test]
    fn file_backed_primary_and_byte_companion_commit_as_one_set() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let source_path = root.join("encoded.tmp");
        fs::write(&source_path, b"streamed primary").unwrap();
        let source = File::open(&source_path).unwrap();
        let primary = root.join("document.md");
        let asset = root.join("asset.bin");
        prepare_file_and_bytes(
            &FileTarget { path: primary.clone(), file: &source },
            &[Target { path: asset.clone(), bytes: b"asset" }],
            false,
            &context(),
        )
        .unwrap()
        .commit()
        .unwrap();

        assert_eq!(fs::read(primary).unwrap(), b"streamed primary");
        assert_eq!(fs::read(asset).unwrap(), b"asset");
        assert!(!root.join(REGISTRY_NAME).exists());
    }

    #[test]
    fn concurrent_parent_creation_authenticates_the_winning_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().canonicalize().unwrap().join("a/b/c");
        let barrier = Arc::new(Barrier::new(16));
        let identities = std::thread::scope(|scope| {
            let handles = (0..16)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let target = target.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        SafeDir::open_or_create_absolute(&target).unwrap().identity
                    })
                })
                .collect::<Vec<_>>();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect::<Vec<_>>()
        });
        assert!(identities.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[cfg(unix)]
    #[test]
    fn output_transaction_resolves_an_existing_symlinked_parent_once() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let physical = root.join("physical");
        fs::create_dir(&physical).unwrap();
        let alias = root.join("alias");
        symlink(&physical, &alias).unwrap();
        let requested = alias.join("new/nested/document.md");
        let target = [Target { path: requested, bytes: b"converted" }];

        prepare(&target, false, &context()).unwrap().commit().unwrap();

        assert_eq!(fs::read(physical.join("new/nested/document.md")).unwrap(), b"converted");
        assert!(manager_directories(&physical).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn config_replace_is_fd_relative_durable_and_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let target = root.join("config.toml");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        atomic_replace_config(&target, b"new", true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o640);

        let created = root.join("created.toml");
        atomic_replace_config(&created, b"created", false).unwrap();
        assert_eq!(fs::metadata(&created).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn config_replace_rejects_target_and_temporary_identity_races() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let target = root.join("config.toml");
        fs::write(&target, b"old").unwrap();
        let held = root.join("held.toml");
        let error = atomic_replace_config_inner(&target, b"new", true, |_, _, _| {
            fs::rename(&target, &held)?;
            fs::write(&target, b"racer")?;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(&target).unwrap(), b"racer");

        fs::remove_file(&target).unwrap();
        fs::rename(&held, &target).unwrap();
        let error =
            atomic_replace_config_inner(&target, b"new", true, |parent, _, temporary_name| {
                let path = parent.path.join(temporary_name);
                fs::remove_file(&path)?;
                fs::write(path, b"attacker temporary")?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn config_publish_atomic_primitive_closes_post_check_destination_races() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();

        let absent = root.join("absent.toml");
        let error = atomic_replace_config_inner_with_barriers(
            &absent,
            b"new",
            false,
            |_, _, _| Ok(()),
            |_, _, _| {
                fs::write(&absent, b"racer")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "io");
        assert_eq!(fs::read(&absent).unwrap(), b"racer");

        let target = root.join("existing.toml");
        let held = root.join("original-held.toml");
        fs::write(&target, b"old").unwrap();
        let error = atomic_replace_config_inner_with_barriers(
            &target,
            b"new",
            true,
            |_, _, _| Ok(()),
            |_, _, _| {
                fs::rename(&target, &held)?;
                fs::write(&target, b"racer")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(&target).unwrap(), b"racer");
        assert_eq!(fs::read(&held).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn config_publish_reauthenticates_parent_and_temporary_after_final_check() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let parent = root.join("config");
        let held = root.join("config-held");
        fs::create_dir(&parent).unwrap();
        let target = parent.join("settings.toml");
        fs::write(&target, b"old").unwrap();
        let error = atomic_replace_config_inner_with_barriers(
            &target,
            b"new",
            true,
            |_, _, _| Ok(()),
            |_, _, _| {
                fs::rename(&parent, &held)?;
                fs::create_dir(&parent)?;
                fs::write(parent.join("settings.toml"), b"attacker")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(parent.join("settings.toml")).unwrap(), b"attacker");
        assert_eq!(fs::read(held.join("settings.toml")).unwrap(), b"old");

        let target = held.join("settings.toml");
        let attacker_temporary = Arc::new(Mutex::new(None::<PathBuf>));
        let captured = Arc::clone(&attacker_temporary);
        let error = atomic_replace_config_inner_with_barriers(
            &target,
            b"new",
            true,
            |_, _, _| Ok(()),
            move |directory, _, temporary_name| {
                let path = directory.path.join(temporary_name);
                fs::remove_file(&path)?;
                fs::write(&path, b"attacker temporary")?;
                *captured.lock().unwrap() = Some(path);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(&target).unwrap(), b"old");
        let attacker_temporary = attacker_temporary.lock().unwrap().clone().unwrap();
        assert_eq!(fs::read(attacker_temporary).unwrap(), b"attacker temporary");
    }

    #[cfg(unix)]
    #[test]
    fn config_replace_rejects_parent_swap_and_symlink_paths() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let parent = root.join("config");
        let held = root.join("config-held");
        fs::create_dir(&parent).unwrap();
        let target = parent.join("settings.toml");
        fs::write(&target, b"old").unwrap();
        let error = atomic_replace_config_inner(&target, b"new", true, |_, _, _| {
            fs::rename(&parent, &held)?;
            fs::create_dir(&parent)?;
            fs::write(parent.join("settings.toml"), b"attacker")?;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.code(), "outputIdentityChanged");
        assert_eq!(fs::read(parent.join("settings.toml")).unwrap(), b"attacker");

        let destination = root.join("destination.toml");
        fs::write(&destination, b"keep").unwrap();
        let link = root.join("link.toml");
        symlink(&destination, &link).unwrap();
        assert!(atomic_replace_config(&link, b"new", true).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"keep");

        let real_parent = root.join("real-parent");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = root.join("linked-parent");
        symlink(&real_parent, &linked_parent).unwrap();
        assert!(atomic_replace_config(&linked_parent.join("new.toml"), b"new", false).is_err());
        assert!(!real_parent.join("new.toml").exists());
    }

    #[test]
    fn every_durable_phase_is_recoverable_by_a_new_manager() {
        let phases = [
            "journalCreated",
            "stageAllocated",
            "stageWritten",
            "stageSynced",
            "prepared",
            "committing",
            "backupRenamed",
            "backupJournaled",
            "targetInstalled",
            "installJournaled",
            "committed",
        ];
        for phase in phases {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let first = root.join("one.md");
            let second = root.join("two.bin");
            fs::write(&first, b"old-one").unwrap();
            fs::write(&second, b"old-two").unwrap();
            let targets = [
                Target { path: first.clone(), bytes: b"new-one" },
                Target { path: second.clone(), bytes: b"new-two" },
            ];
            let mut fired = false;
            let result = prepare_with_hook(&targets, true, &context(), |seen, _| {
                if !fired && seen == phase {
                    fired = true;
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            });
            let result = match result {
                Ok(mut transaction) => transaction.commit_with_hook(|seen, _| {
                    if !fired && seen == phase {
                        fired = true;
                        Ok(HookDecision::SimulateCrash)
                    } else {
                        Ok(HookDecision::Continue)
                    }
                }),
                Err(error) => Err(error),
            };
            assert_eq!(result.unwrap_err().code(), "simulatedCrash", "{phase}");
            recover_pending(&root).unwrap();
            let values = (fs::read(&first).unwrap(), fs::read(&second).unwrap());
            let expected = if phase == "committed" {
                (b"new-one".to_vec(), b"new-two".to_vec())
            } else {
                (b"old-one".to_vec(), b"old-two".to_vec())
            };
            assert_eq!(values, expected, "wrong recovered set after {phase}");
            assert!(manager_directories(&root).is_empty());
        }
    }

    #[test]
    fn stage_failure_fsync_failure_budget_and_cancellation_leave_old_set() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("one.md");
        let second = root.join("two.bin");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
        ];

        for (phase, index, code) in [
            ("beforeStage", 1, "injectedStageFailure"),
            ("beforeStageSync", 0, "injectedFsyncFailure"),
        ] {
            let error = prepare_with_hook(&targets, true, &context(), |seen, seen_index| {
                if seen == phase && seen_index == index {
                    Err(CliError::new(ExitClass::Io, code, "injected"))
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .err()
            .expect("injected prepare failure");
            assert_eq!(error.code(), code);
            assert_eq!(fs::read(&first).unwrap(), b"old-one");
            assert_eq!(fs::read(&second).unwrap(), b"old-two");
            assert!(manager_directories(&root).is_empty());
        }

        let limited = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_temporary_bytes: 4, ..ResourceLimits::default() },
        );
        let error = prepare(&targets, true, &limited).err().expect("temporary budget failure");
        assert_eq!(error.code(), "resourceLimit");
        assert!(manager_directories(&root).is_empty());

        let token = into_markdown::CancellationToken::new();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation: token.clone(), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let transaction = prepare(&targets, true, &cancelled).unwrap();
        token.cancel();
        let error = transaction.commit().unwrap_err();
        assert_eq!(error.code(), "cancelled");
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert_eq!(fs::read(&second).unwrap(), b"old-two");
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn overlapping_cross_directory_transaction_is_recovered_before_any_new_write() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(a.join("child")).unwrap();
        fs::create_dir_all(&b).unwrap();
        let first = a.join("child/one.md");
        let second = b.join("two.bin");
        let parent_level = a.join("parent.txt");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        fs::write(&parent_level, b"old-parent").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
            Target { path: parent_level.clone(), bytes: b"new-parent" },
        ];

        for requested in [&first, &second, &parent_level] {
            fs::write(&first, b"old-one").unwrap();
            fs::write(&second, b"old-two").unwrap();
            fs::write(&parent_level, b"old-parent").unwrap();
            let mut transaction = prepare(&targets, true, &context()).unwrap();
            let error = transaction
                .commit_with_hook(|phase, index| {
                    if phase == "targetInstalled" && index == 0 {
                        Ok(HookDecision::SimulateCrash)
                    } else {
                        Ok(HookDecision::Continue)
                    }
                })
                .unwrap_err();
            assert_eq!(error.code(), "simulatedCrash");
            drop(transaction);

            let third = [Target { path: requested.clone(), bytes: b"third" }];
            prepare(&third, true, &context()).unwrap().commit().unwrap();
            assert_eq!(fs::read(requested).unwrap(), b"third");
            for untouched in [&first, &second, &parent_level] {
                if untouched != requested {
                    let expected = if untouched == &first {
                        b"old-one".as_slice()
                    } else if untouched == &second {
                        b"old-two".as_slice()
                    } else {
                        b"old-parent".as_slice()
                    };
                    assert_eq!(fs::read(untouched).unwrap(), expected);
                }
            }
            assert!(manager_directories(&root).is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn physical_parent_lease_serializes_an_absent_different_basename() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first_path = root.join("first.md");
        let second_path = root.join("second.md");
        fs::write(&first_path, b"old").unwrap();
        let first = [Target { path: first_path.clone(), bytes: b"interrupted" }];
        let mut transaction = prepare(&first, true, &context()).unwrap();

        let active = [Target { path: second_path.clone(), bytes: b"blocked" }];
        let error = prepare(&active, false, &context()).err().expect("physical parent is leased");
        assert_eq!(error.code(), "transactionBusy");

        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash");
        drop(transaction);

        let second = [Target { path: second_path.clone(), bytes: b"final" }];
        prepare(&second, false, &context()).unwrap().commit().unwrap();
        assert_eq!(fs::read(first_path).unwrap(), b"old");
        assert_eq!(fs::read(second_path).unwrap(), b"final");
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_alias_observes_the_same_physical_parent_lease() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let original = root.join("original.md");
        let alias = root.join("alias.md");
        fs::write(&original, b"old").unwrap();
        fs::hard_link(&original, &alias).unwrap();
        let first = [Target { path: original.clone(), bytes: b"first" }];
        let mut transaction = prepare(&first, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(HookDecision::SimulateCrash)
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "simulatedCrash");
        drop(transaction);

        let second = [Target { path: alias.clone(), bytes: b"second" }];
        prepare(&second, true, &context()).unwrap().commit().unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"old");
        assert_eq!(fs::read(&alias).unwrap(), b"second");
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn case_and_unicode_parent_aliases_share_the_physical_lease() {
        for (created_name, alias_name) in [("CaseParent", "caseparent"), ("é", "e\u{301}")] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let created = root.join(created_name);
            fs::create_dir(&created).unwrap();
            let alias = root.join(alias_name);
            let (Ok(created_handle), Ok(alias_handle)) =
                (SafeDir::open_absolute(&created), SafeDir::open_absolute(&alias))
            else {
                continue;
            };
            if created_handle.identity != alias_handle.identity {
                continue;
            }
            let first_path = created.join("first.md");
            let alias_path = alias.join("second.md");
            fs::write(&first_path, b"old").unwrap();
            let first = [Target { path: first_path.clone(), bytes: b"interrupted" }];
            let mut transaction = prepare(&first, true, &context()).unwrap();
            let error = transaction
                .commit_with_hook(|phase, index| {
                    if phase == "targetInstalled" && index == 0 {
                        Ok(HookDecision::SimulateCrash)
                    } else {
                        Ok(HookDecision::Continue)
                    }
                })
                .unwrap_err();
            assert_eq!(error.code(), "simulatedCrash");
            drop(transaction);

            let second = [Target { path: alias_path.clone(), bytes: b"final" }];
            prepare(&second, false, &context()).unwrap().commit().unwrap();
            assert_eq!(fs::read(first_path).unwrap(), b"old");
            assert_eq!(fs::read(alias_path).unwrap(), b"final");
            assert!(manager_directories(&created).is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn deep_parent_without_a_lease_never_uses_an_ancestor_scan_limit() {
        for depth in [130_usize, 500] {
            let temporary = tempfile::tempdir().unwrap();
            let mut parent = temporary.path().canonicalize().unwrap();
            let mut supported = true;
            for _ in 0..depth {
                parent.push("d");
                if let Err(error) = fs::create_dir(&parent) {
                    assert!(
                        matches!(error.raw_os_error(), Some(libc::ENAMETOOLONG | libc::EINVAL)),
                        "unexpected deep directory failure: {error}"
                    );
                    supported = false;
                    break;
                }
            }
            if !supported {
                continue;
            }
            let target = parent.join("document.md");
            let output = [Target { path: target.clone(), bytes: b"deep" }];
            prepare(&output, false, &context()).unwrap().commit().unwrap();
            assert_eq!(fs::read(target).unwrap(), b"deep");
        }
    }

    #[cfg(unix)]
    #[test]
    fn parent_swap_after_handle_authentication_never_writes_external_directory() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let parent = root.join("safe");
        let held = root.join("safe-held");
        let external = root.join("external");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&external).unwrap();
        let target = parent.join("document.md");
        fs::write(&target, b"old").unwrap();
        let targets = [Target { path: target.clone(), bytes: b"new" }];
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, _| {
                if phase == "afterTargetAuthentication" {
                    fs::rename(&parent, &held)?;
                    symlink(&external, &parent)?;
                }
                Ok(HookDecision::Continue)
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert!(!external.join("document.md").exists());
        assert_eq!(fs::read(held.join("document.md")).unwrap(), b"old");
        assert_eq!(manager_directories(&held).len(), 1);

        fs::remove_file(&parent).unwrap();
        fs::rename(&held, &parent).unwrap();
        recover_pending(&parent).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!external.join("document.md").exists());
        assert!(manager_directories(&root).is_empty());
    }

    #[test]
    fn rollback_failure_preserves_backup_and_a_later_recovery_completes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("one.md");
        let second = root.join("two.bin");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
        ];
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let directory = transaction.directory.clone();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    fs::remove_file(&first)?;
                    fs::create_dir(&first)?;
                    fs::write(first.join("blocker"), b"do-not-delete")?;
                    Err(CliError::new(ExitClass::Io, "injectedCommitFailure", "injected"))
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert!(error.message().contains("injectedCommitFailure"));
        #[cfg(unix)]
        assert!(error.message().contains("outputTargetTypeDenied"));
        #[cfg(windows)]
        assert!(
            error.message().contains("one or more rollback operations failed"),
            "{}",
            error.message()
        );
        assert_eq!(fs::read(directory.join("backup-0")).unwrap(), b"old-one");
        assert!(directory.join("journal-a.json").exists());
        assert_eq!(fs::read(&second).unwrap(), b"old-two");

        fs::remove_dir_all(&first).unwrap();
        recover_pending(&root).unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert_eq!(fs::read(&second).unwrap(), b"old-two");
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rollback_failure_keeps_the_backup_recoverable() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output_parent = root.join("locked");
        fs::create_dir(&output_parent).unwrap();
        let output = output_parent.join("document.md");
        fs::write(&output, b"old").unwrap();
        let targets = [Target { path: output.clone(), bytes: b"new" }];
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let directory = transaction.directory.clone();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "targetInstalled" && index == 0 {
                    Ok(HookDecision::SimulateRollbackFailure)
                } else {
                    Ok(HookDecision::Continue)
                }
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert!(error.message().contains("injectedPermissionFailure"));
        assert_eq!(fs::read(directory.join("backup-0")).unwrap(), b"old");

        recover_pending(&output_parent).unwrap();
        assert_eq!(fs::read(output).unwrap(), b"old");
        assert!(!directory.exists());
    }

    #[test]
    fn late_non_regular_target_is_rejected_before_the_first_output_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("one.md");
        let second = root.join("two.bin");
        fs::write(&first, b"old-one").unwrap();
        fs::write(&second, b"old-two").unwrap();
        let targets = [
            Target { path: first.clone(), bytes: b"new-one" },
            Target { path: second.clone(), bytes: b"new-two" },
        ];
        let held_second = root.join("held-two.bin");
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, _| {
                if phase == "committing" {
                    fs::rename(&second, &held_second)?;
                    fs::create_dir(&second)?;
                }
                Ok(HookDecision::Continue)
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert!(second.is_dir());
        assert_eq!(manager_directories(&root).len(), 1);

        fs::remove_dir(&second).unwrap();
        fs::rename(&held_second, &second).unwrap();
        recover_pending(&root).unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"old-one");
        assert_eq!(fs::read(&second).unwrap(), b"old-two");
    }

    #[test]
    fn absent_target_race_is_never_replaced_by_commit_or_rollback() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let target = root.join("document.md");
        let targets = [Target { path: target.clone(), bytes: b"new" }];
        let mut transaction = prepare(&targets, false, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "beforeTarget" && index == 0 {
                    fs::write(&target, b"racer")?;
                }
                Ok(HookDecision::Continue)
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert_eq!(fs::read(&target).unwrap(), b"racer");
        assert_eq!(manager_directories(&root).len(), 1);

        fs::remove_file(&target).unwrap();
        recover_pending(&root).unwrap();
        assert!(!target.exists());
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn directories_symlinks_fifos_and_devices_are_never_overwrite_targets() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let directory = root.join("directory");
        fs::create_dir(&directory).unwrap();
        let link = root.join("link");
        symlink(&directory, &link).unwrap();
        let fifo = root.join("fifo");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());

        for path in [directory, link, fifo, PathBuf::from("/dev/null")] {
            let target = [Target { path: path.clone(), bytes: b"new" }];
            let error = prepare(&target, true, &context()).err().expect("non-regular target");
            assert_eq!(error.code(), "outputTargetTypeDenied", "{}", path.display());
        }
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_swap_preserves_the_safe_old_primary_and_defers_unsafe_cleanup() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let primary = root.join("document.md");
        let asset_parent = root.join("assets");
        let held_parent = root.join("assets-held");
        let attacker = root.join("attacker");
        fs::write(&primary, b"old-document").unwrap();
        fs::create_dir(&asset_parent).unwrap();
        fs::create_dir(&attacker).unwrap();
        let asset = asset_parent.join("image.png");
        let targets = [
            Target { path: primary.clone(), bytes: b"new-document" },
            Target { path: asset.clone(), bytes: b"new-image" },
        ];
        let mut transaction = prepare(&targets, true, &context()).unwrap();
        let error = transaction
            .commit_with_hook(|phase, index| {
                if phase == "beforeTarget" && index == 1 {
                    fs::rename(&asset_parent, &held_parent)?;
                    symlink(&attacker, &asset_parent)?;
                }
                Ok(HookDecision::Continue)
            })
            .unwrap_err();
        assert_eq!(error.code(), "rollbackFailed");
        assert_eq!(fs::read(&primary).unwrap(), b"old-document");
        assert!(!attacker.join("image.png").exists());
        assert_eq!(manager_directories(&root).len(), 1);

        fs::remove_file(&asset_parent).unwrap();
        fs::rename(&held_parent, &asset_parent).unwrap();
        if let Err(error) = recover_pending(&root) {
            panic!("recovery failed: {error}");
        }
        assert_eq!(fs::read(&primary).unwrap(), b"old-document");
        assert!(!asset.exists());
        assert!(manager_directories(&root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn malformed_manager_directory_is_preserved_and_rejected() {
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let nonce = "0123456789abcdef0123456789abcdef";
        let registry = root.join(REGISTRY_NAME);
        fs::create_dir(&registry).unwrap();
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
        let managed = registry.join(format!("{TRANSACTION_PREFIX}{nonce}"));
        fs::create_dir(&managed).unwrap();
        fs::write(managed.join("journal-a.json"), b"not-json").unwrap();
        let error = recover_pending(&root).unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert!(managed.exists());
        let unrelated = root.join(".into-md-txn-01-not-managed");
        fs::create_dir(&unrelated).unwrap();
        assert!(unrelated.exists());
    }

    #[test]
    fn active_transaction_is_locked_and_unexpected_members_block_recovery() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let output = root.join("document.md");
        fs::write(&output, b"old").unwrap();
        let targets = [Target { path: output.clone(), bytes: b"new" }];
        let transaction = prepare(&targets, true, &context()).unwrap();
        let directory = transaction.directory.clone();

        recover_pending(&root).unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"old");
        assert!(directory.exists());
        transaction.abort().unwrap();

        let transaction = prepare(&targets, true, &context()).unwrap();
        let directory = transaction.directory.clone();
        transaction.abandon_for_test();
        fs::write(directory.join("not-in-journal"), b"untrusted").unwrap();
        let error = recover_pending(&root).unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert!(directory.join("not-in-journal").exists());
        assert_eq!(fs::read(&output).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn manager_symlink_is_rejected_without_touching_its_destination() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let destination = root.join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), b"keep").unwrap();
        let registry = root.join(REGISTRY_NAME);
        fs::create_dir(&registry).unwrap();
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o700)).unwrap();
        let manager =
            registry.join(format!("{TRANSACTION_PREFIX}{}", "0123456789abcdef0123456789abcdef"));
        symlink(&destination, &manager).unwrap();
        let error = recover_pending(&root).unwrap_err();
        assert_eq!(error.code(), "transactionRecoveryFailed");
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"keep");
        assert!(fs::symlink_metadata(manager).unwrap().file_type().is_symlink());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cross_filesystem_set_is_rejected_before_transaction_allocation() {
        use std::os::unix::fs::MetadataExt as _;

        if !Path::new("/dev/shm").is_dir()
            || fs::metadata("/dev/shm").unwrap().dev() == fs::metadata("/tmp").unwrap().dev()
        {
            return;
        }
        let first_root = tempfile::tempdir_in("/tmp").unwrap();
        let second_root = tempfile::tempdir_in("/dev/shm").unwrap();
        let first = first_root.path().join("document.md");
        let second = second_root.path().join("asset.bin");
        let targets = [
            Target { path: first.clone(), bytes: b"document" },
            Target { path: second.clone(), bytes: b"asset" },
        ];
        let error = prepare(&targets, true, &context()).err().expect("cross-filesystem rejection");
        assert_eq!(error.code(), "crossFilesystemTransaction");
        assert!(!first.exists());
        assert!(!second.exists());
    }
}
