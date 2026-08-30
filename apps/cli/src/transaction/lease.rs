use super::{
    BTreeSet, CliError, Digest, ExitClass, FileIdentity, HashSet, JOURNAL_SIGNATURE,
    JOURNAL_VERSION, Journal, MAX_RECOVERY_DIRECTORY_ENTRIES, MAX_RECOVERY_TRANSACTIONS, OsStr,
    OsString, PARENT_LEASE_NAME, PARENT_MARKER_PREFIX, ParentLease, Path, PathBuf, REGISTRY_NAME,
    Read, SafeDir, Sha256, TransactionSource, Write, decode_path, file_identity, fs, hex_bytes, io,
    read_limited_regular_handle, recovery_error, remove_regular_handle_if_present,
};

pub(super) fn ensure_transaction_platform() -> Result<(), CliError> {
    if cfg!(any(target_os = "linux", target_vendor = "apple", windows)) {
        Ok(())
    } else {
        Err(transaction_platform_unavailable())
    }
}

pub(super) fn transaction_platform_unavailable() -> CliError {
    CliError::new(
        ExitClass::Component,
        "componentUnavailable",
        "output transactions require audited relative directory-handle filesystem operations",
    )
}

#[cfg(any(unix, windows))]
pub(super) fn for_each_target_parent(
    targets: &[PathBuf],
    mut visit: impl FnMut(&SafeDir) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut identities = BTreeSet::new();
    for target in targets {
        let name = target.file_name().ok_or_else(|| recovery_error("target has no file name"))?;
        if name == OsStr::new(REGISTRY_NAME) || name == OsStr::new(PARENT_LEASE_NAME) {
            return Err(CliError::new(
                ExitClass::Io,
                "outputPathUnsupported",
                "output target conflicts with the transaction manager namespace",
            ));
        }
        let parent = target.parent().ok_or_else(|| recovery_error("target has no parent"))?;
        let handle = SafeDir::open_or_create_absolute(parent)?;
        if identities.insert(handle.identity.clone()) {
            visit(&handle)?;
        }
    }
    Ok(())
}

#[cfg(any(unix, windows))]
pub(super) fn parent_marker_name(identity: &FileIdentity) -> OsString {
    OsString::from(format!("{PARENT_MARKER_PREFIX}{}.json", parent_identity_digest(identity)))
}

pub(super) fn parent_identity_digest(identity: &FileIdentity) -> String {
    let mut digest = Sha256::new();
    digest.update(identity.platform.as_bytes());
    digest.update([0]);
    digest.update(identity.first.to_le_bytes());
    digest.update(identity.second.to_le_bytes());
    hex_bytes(&digest.finalize())
}

#[cfg(unix)]
pub(super) fn inspect_linked_parent_lease(
    parent: &SafeDir,
) -> Result<Option<FileIdentity>, CliError> {
    parent.inspect_regular(OsStr::new(PARENT_LEASE_NAME))
}

#[cfg(windows)]
pub(super) fn inspect_linked_parent_lease(
    parent: &SafeDir,
) -> Result<Option<FileIdentity>, CliError> {
    parent.inspect_lease_file(OsStr::new(PARENT_LEASE_NAME))
}

#[cfg(unix)]
pub(super) fn inspect_transaction_lease_member(
    transaction: &SafeDir,
    name: &OsStr,
) -> Result<Option<FileIdentity>, CliError> {
    transaction.inspect_regular(name)
}

#[cfg(windows)]
pub(super) fn inspect_transaction_lease_member(
    transaction: &SafeDir,
    name: &OsStr,
) -> Result<Option<FileIdentity>, CliError> {
    match transaction.inspect_regular(name) {
        Ok(identity) => Ok(identity),
        Err(_) => transaction.inspect_lease_file(name),
    }
}

#[cfg(unix)]
pub(super) fn read_linked_parent_lease(parent: &SafeDir, limit: u64) -> io::Result<Vec<u8>> {
    read_limited_regular_handle(parent, OsStr::new(PARENT_LEASE_NAME), limit)
}

#[cfg(windows)]
pub(super) fn read_linked_parent_lease(parent: &SafeDir, limit: u64) -> io::Result<Vec<u8>> {
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
pub(super) fn remove_linked_parent_lease(parent: &SafeDir) -> Result<(), CliError> {
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
pub(super) fn remove_linked_parent_lease(parent: &SafeDir) -> Result<(), CliError> {
    if parent.inspect_lease_file(OsStr::new(PARENT_LEASE_NAME))?.is_some() {
        parent.remove_lease_file(OsStr::new(PARENT_LEASE_NAME))?;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn create_parent_lease(
    parent: &SafeDir,
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
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
    transaction.sync()
}

#[cfg(windows)]
pub(super) fn create_parent_lease(
    parent: &SafeDir,
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
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
        || inspect_transaction_lease_member(transaction, &name)?.as_ref() != Some(&source_identity)
    {
        return Err(recovery_error("physical parent lease identity mismatch"));
    }
    transaction.sync()
}

#[cfg(any(unix, windows))]
pub(super) fn load_parent_lease(parent: &SafeDir) -> Result<Option<ParentLease>, CliError> {
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
pub(super) fn validate_parent_lease(
    parent: &SafeDir,
    transaction: &SafeDir,
    journal: &Journal,
    lease: &ParentLease,
) -> Result<(), CliError> {
    validate_parent_lease_binding(parent, transaction, journal, lease)?;
    if !journal.parent_identities.contains(&parent.identity) {
        return Err(recovery_error("physical parent lease does not match journal"));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn validate_parent_lease_binding(
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
    {
        return Err(recovery_error("physical parent lease does not match journal"));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
pub(super) fn validate_journal_parent_leases(
    transaction: &SafeDir,
    root: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    for_each_journal_parent_indexed(root, journal, |_, parent| {
        let lease = load_parent_lease(parent)?
            .ok_or_else(|| recovery_error("physical parent lease is missing"))?;
        validate_parent_lease_binding(parent, transaction, journal, &lease)
    })
}

#[cfg(any(unix, windows))]
pub(super) struct ParentLeaseRemovalIndex {
    remaining: HashSet<FileIdentity>,
    #[cfg(test)]
    build_insertions: u64,
    #[cfg(test)]
    membership_probes: u64,
}

#[cfg(any(unix, windows))]
impl ParentLeaseRemovalIndex {
    pub(super) fn new(parent_identities: &[FileIdentity]) -> Result<Self, CliError> {
        let mut remaining = HashSet::new();
        #[cfg(test)]
        let mut build_insertions = 0_u64;
        remaining.try_reserve(parent_identities.len()).map_err(|error| {
            recovery_error(format!("cannot reserve physical parent lease index: {error}"))
        })?;
        for identity in parent_identities {
            if !remaining.insert(identity.clone()) {
                return Err(recovery_error("journal contains duplicate physical parents"));
            }
            #[cfg(test)]
            {
                build_insertions = build_insertions.saturating_add(1);
            }
        }
        Ok(Self {
            remaining,
            #[cfg(test)]
            build_insertions,
            #[cfg(test)]
            membership_probes: 0,
        })
    }

    pub(super) fn consume(&mut self, identity: &FileIdentity) -> Result<(), CliError> {
        #[cfg(test)]
        {
            self.membership_probes = self.membership_probes.saturating_add(1);
        }
        if !self.remaining.remove(identity) {
            return Err(recovery_error("physical parent is absent from the journal index"));
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<(), CliError> {
        if !self.remaining.is_empty() {
            return Err(recovery_error("journal physical parent cleanup is incomplete"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn build_insertions(&self) -> u64 {
        self.build_insertions
    }

    #[cfg(test)]
    pub(super) fn membership_probes(&self) -> u64 {
        self.membership_probes
    }
}

#[cfg(any(unix, windows))]
pub(super) fn remove_parent_lease(
    parent: &SafeDir,
    transaction: &SafeDir,
    journal: &Journal,
    index: &mut ParentLeaseRemovalIndex,
) -> Result<(), CliError> {
    index.consume(&parent.identity)?;
    remove_parent_lease_binding(parent, transaction, journal)
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn remove_parent_lease_binding(
    parent: &SafeDir,
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    let marker = parent_marker_name(&parent.identity);
    let transaction_identity = inspect_transaction_lease_member(transaction, &marker)?;
    let parent_identity = inspect_linked_parent_lease(parent)?;
    let Some(transaction_identity) = transaction_identity else {
        return Ok(());
    };
    if parent_identity.as_ref() == Some(&transaction_identity) {
        let lease = load_parent_lease(parent)?
            .ok_or_else(|| recovery_error("physical parent lease disappeared"))?;
        validate_parent_lease_binding(parent, transaction, journal, &lease)?;
        remove_linked_parent_lease(parent)?;
        parent.sync()?;
    }
    remove_regular_handle_if_present(transaction, &marker)?;
    transaction.sync()
}

#[cfg(any(unix, windows))]
pub(super) fn for_each_journal_parent(
    root: &SafeDir,
    journal: &Journal,
    mut visit: impl FnMut(&SafeDir) -> Result<(), CliError>,
) -> Result<(), CliError> {
    for_each_journal_parent_indexed(root, journal, |_, parent| visit(parent))
}

#[cfg(any(unix, windows))]
fn for_each_journal_parent_indexed(
    root: &SafeDir,
    journal: &Journal,
    mut visit: impl FnMut(usize, &SafeDir) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let journal_identities = journal.parent_identities.iter().cloned().collect::<HashSet<_>>();
    if journal_identities.len() != journal.parent_identities.len() {
        return Err(recovery_error("journal contains duplicate physical parents"));
    }
    let mut representatives = vec![None; journal.parent_identities.len()];
    for entry in &journal.entries {
        let Some(parent_index) = entry.parent_index else {
            continue;
        };
        let representative = representatives
            .get_mut(parent_index)
            .ok_or_else(|| recovery_error("transaction target parent index is outside limits"))?;
        if representative.is_none() {
            *representative = Some(&entry.target);
        }
    }
    for (parent_index, target) in representatives.into_iter().enumerate() {
        let target = target.ok_or_else(|| {
            recovery_error("journal physical parent has no bound transaction target")
        })?;
        let relative = decode_path(target)?;
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = root.open_descendant(parent_relative)?;
        if parent.identity != journal.parent_identities[parent_index] {
            return Err(recovery_error(
                "journal physical parent identities do not match its target paths",
            ));
        }
        visit(parent_index, &parent)?;
    }
    Ok(())
}

#[cfg(any(unix, windows))]
pub(super) fn remove_journal_parent_leases(
    root: &SafeDir,
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    for_each_journal_parent_indexed(root, journal, |_, parent| {
        remove_parent_lease_binding(parent, transaction, journal)
    })?;
    transaction.sync()
}
