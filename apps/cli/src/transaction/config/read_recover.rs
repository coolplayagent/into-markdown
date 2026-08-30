use super::{
    CliError, Deserialize, Digest, ExitClass, File, OsStr, OsString, Path, Read, Serialize, Sha256,
    Write, cap_config_digest, cap_config_identity, config_open_file_identity,
    config_open_file_link_count, config_transaction_nonce, hex_bytes, io, recovery_error,
    sync_cap_directory, valid_windows_config_transaction_name, validate_single_name,
};

#[cfg(test)]
pub(super) fn config_test_mutate_before_publish(
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
pub(super) fn config_test_mutate_before_publish(_parent: &cap_std::fs::Dir, _name: &OsStr) {}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct WindowsConfigJournal {
    pub(super) schema_version: u32,
    pub(super) target: String,
    pub(super) temporary: String,
    pub(super) backup: String,
    pub(super) original: Option<(u64, u64)>,
    pub(super) original_sha256: Option<String>,
    pub(super) new_sha256: String,
    pub(super) phase: WindowsConfigPhase,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum WindowsConfigPhase {
    Prepared,
}

pub(super) fn write_windows_config_journal(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
    journal: &WindowsConfigJournal,
) -> Result<(), CliError> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| recovery_error(format!("serialize config journal: {error}")))?;
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

pub(super) fn recover_windows_config_transaction(
    directory: &cap_std::fs::Dir,
    target: &OsStr,
) -> Result<(), CliError> {
    let Some((journal_name, journal)) = load_windows_config_journal(directory, target)? else {
        return Ok(());
    };
    reconcile_windows_config_transaction(directory, target, &journal)?;
    directory.remove_file(journal_name)?;
    sync_cap_directory(directory)
}

fn load_windows_config_journal(
    directory: &cap_std::fs::Dir,
    target: &OsStr,
) -> Result<Option<(String, WindowsConfigJournal)>, CliError> {
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
        return Ok(None);
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
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| recovery_error("config transaction journal is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
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
    Ok(Some((journal_name.clone(), journal)))
}

fn reconcile_windows_config_transaction(
    directory: &cap_std::fs::Dir,
    target: &OsStr,
    journal: &WindowsConfigJournal,
) -> Result<(), CliError> {
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
    Ok(())
}

pub(super) fn config_rename_no_replace(
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
        .map_err(|error| recovery_error(error.to_string()))?;
    }
    Ok(())
}

pub(super) fn remove_config_with_authority(
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
    #[cfg(test)]
    config_test_replace_before_cleanup(directory, name)?;
    #[cfg(not(test))]
    config_test_replace_before_cleanup(directory, name);
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| CliError::new(ExitClass::Internal, "internal", "config nonce unavailable"))?;
    let nonce = hex_bytes(&nonce);
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

pub(super) fn config_open_file_digest(file: &mut File) -> Result<String, CliError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
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
pub(super) fn config_test_replace_before_cleanup(
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
pub(super) fn config_test_replace_before_cleanup(_directory: &cap_std::fs::Dir, _name: &OsStr) {}

#[cfg(test)]
pub(super) fn windows_config_test_crash(point: &str) {
    if std::env::var_os("INTO_MD_CONFIG_CRASH_POINT").as_deref() == Some(OsStr::new(point)) {
        std::process::exit(86);
    }
}

#[cfg(not(test))]
pub(super) fn windows_config_test_crash(_point: &str) {}
