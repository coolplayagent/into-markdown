#[cfg(unix)]
use super::super::{
    FileIdentity, NONCE_COUNTER, Ordering, SafeDir, SystemTime, UNIX_EPOCH, file_identity,
    verify_name_identity,
};
use super::{
    CliError, ConfigExpectedAuthority, Digest, ExitClass, OsStr, OsString, Path, Read, Sha256,
    WindowsConfigJournal, WindowsConfigPhase, Write, cap_config_digest, cap_config_identity,
    config_rename_no_replace, config_test_mutate_before_publish, fs, hex_bytes, io,
    recover_windows_config_transaction, recovery_error, remove_config_with_authority,
    sync_cap_directory, validate_single_name, windows_config_test_crash,
    write_windows_config_journal,
};
#[cfg(unix)]
use cap_std::fs::MetadataExt as _;

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
type WindowsConfigAuthority = (Option<(u64, u64)>, Option<String>);

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

pub(in crate::transaction) mod windows_config_tests {
    #[cfg(test)]
    use super::*;
    #[cfg(test)]
    use std::process::Command;

    #[cfg(test)]
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[cfg(test)]
    fn names() -> (String, String, String) {
        (
            format!(".into-md-config-{NONCE}.next"),
            format!(".into-md-config-{NONCE}.previous"),
            format!(".into-md-config-{NONCE}.journal"),
        )
    }

    #[cfg(test)]
    fn open(root: &Path) -> cap_std::fs::Dir {
        cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority()).unwrap()
    }

    #[cfg(test)]
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
    pub(in crate::transaction) fn config_replace_crash_child() {
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
                    "transaction::config::atomic_replace::windows_config_tests::config_replace_crash_child",
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
pub(in crate::transaction) fn atomic_replace_config_inner(
    path: &Path,
    bytes: &[u8],
    replace: bool,
    before_commit: impl FnOnce(&SafeDir, &OsStr, &OsStr) -> Result<(), CliError>,
) -> Result<(), CliError> {
    atomic_replace_config_inner_with_barriers(path, bytes, replace, before_commit, |_, _, _| Ok(()))
}

#[cfg(unix)]
pub(in crate::transaction) fn atomic_replace_config_inner_with_barriers(
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
pub(super) fn publish_config(
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
pub(super) fn unlink_name_if_identity(
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
fn inspect_windows_config_authority(
    parent: &cap_std::fs::Dir,
    name: &OsStr,
    replace: bool,
    bound_expected: Option<&ConfigExpectedAuthority>,
) -> Result<WindowsConfigAuthority, CliError> {
    use cap_std::fs::OpenOptionsExt as _;
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
    Ok((expected, expected_digest))
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
    validate_single_name(name)?;
    recover_windows_config_transaction(parent, name)?;
    let (expected, expected_digest) =
        inspect_windows_config_authority(parent, name, replace, bound_expected)?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| CliError::new(ExitClass::Internal, "internal", "config nonce unavailable"))?;
    let nonce = hex_bytes(&nonce);
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
        #[cfg(test)]
        config_test_mutate_before_publish(parent, name)?;
        #[cfg(not(test))]
        config_test_mutate_before_publish(parent, name);
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
    let nonce = hex_bytes(&nonce);
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
        #[cfg(test)]
        config_test_mutate_before_publish(parent, name)?;
        #[cfg(not(test))]
        config_test_mutate_before_publish(parent, name);
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
