use super::model::StageResume;
use super::{
    CliError, Digest, ExitClass, FileIdentity, Journal, JournalEntry, JournalPath, Path,
    TransactionSource,
};

pub(super) const STREAMING_INDEX_FIXED_BYTES: u64 = 8 * 1024;

pub(super) fn transaction_index_limit(detail: impl Into<String>) -> CliError {
    CliError::new(ExitClass::Policy, "resourceLimit", detail.into())
}

pub(super) fn streaming_index_capacity_plan<T>(entries: usize) -> Result<u64, CliError> {
    u64::try_from(entries)
        .unwrap_or(u64::MAX)
        .checked_mul(u64::try_from(std::mem::size_of::<T>()).unwrap_or(u64::MAX).saturating_add(24))
        .and_then(|bytes| bytes.checked_mul(4))
        .and_then(|bytes| bytes.checked_add(STREAMING_INDEX_FIXED_BYTES))
        .ok_or_else(|| transaction_index_limit("streaming transaction index capacity overflowed"))
}

pub(super) fn verify_streaming_index_capacity<T>(
    capacity: usize,
    planned: u64,
) -> Result<(), CliError> {
    let actual = u64::try_from(capacity)
        .unwrap_or(u64::MAX)
        .checked_mul(u64::try_from(std::mem::size_of::<T>()).unwrap_or(u64::MAX).saturating_add(24))
        .ok_or_else(|| {
            transaction_index_limit("streaming transaction index capacity overflowed")
        })?;
    if actual > planned {
        return Err(CliError::internal(
            "streaming transaction index exceeded its authenticated memory plan",
        ));
    }
    Ok(())
}

pub(super) fn streaming_path_index_bytes(path: &Path) -> Result<u64, CliError> {
    u64::try_from(path.as_os_str().as_encoded_bytes().len())
        .unwrap_or(u64::MAX)
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(64))
        .ok_or_else(|| transaction_index_limit("streaming target path memory overflowed"))
}

pub(super) fn streaming_identity_index_bytes(identity: &FileIdentity) -> Result<u64, CliError> {
    u64::try_from(identity.platform.len())
        .unwrap_or(u64::MAX)
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(64))
        .ok_or_else(|| transaction_index_limit("streaming original identity memory overflowed"))
}

pub(super) const TRANSACTION_METADATA_TEMPORARY_BYTES: u64 = 16 * 1024;
pub(super) const PARENT_LEASE_TEMPORARY_BYTES: u64 = 8 * 1024;
// Filesystem allocation units and directory entries consume temporary space in
// addition to payload bytes. Charge a conservative platform-independent unit
// so a target-heavy transaction cannot bypass `max_temporary_bytes` with empty
// stages, backups, or directories.
pub(super) const FILE_ENTRY_TEMPORARY_BYTES: u64 = 4 * 1024;
pub(super) const DIRECTORY_ENTRY_TEMPORARY_BYTES: u64 = 4 * 1024;
pub(super) const JOURNAL_ELEMENT_FIXED_BYTES: u64 = 512;

pub(super) fn checked_usize_bytes(value: usize, detail: &'static str) -> Result<u64, CliError> {
    u64::try_from(value).map_err(|_| transaction_index_limit(detail))
}

pub(super) fn add_journal_bytes(
    total: &mut u64,
    bytes: u64,
    detail: &'static str,
) -> Result<(), CliError> {
    *total = total.checked_add(bytes).ok_or_else(|| transaction_index_limit(detail))?;
    Ok(())
}

pub(super) fn journal_path_retained_bytes(path: &JournalPath) -> Result<u64, CliError> {
    let mut bytes =
        checked_usize_bytes(std::mem::size_of::<JournalPath>(), "journal path size overflowed")?;
    add_journal_bytes(
        &mut bytes,
        checked_usize_bytes(path.encoding.capacity(), "journal path encoding overflowed")?,
        "journal path memory estimate overflowed",
    )?;
    let units = checked_usize_bytes(path.units.capacity(), "journal path units overflowed")?
        .checked_mul(4)
        .ok_or_else(|| transaction_index_limit("journal path units overflowed"))?;
    add_journal_bytes(&mut bytes, units, "journal path memory estimate overflowed")?;
    add_journal_bytes(
        &mut bytes,
        JOURNAL_ELEMENT_FIXED_BYTES,
        "journal path memory estimate overflowed",
    )?;
    Ok(bytes)
}

pub(super) fn journal_identity_retained_bytes(identity: &FileIdentity) -> Result<u64, CliError> {
    let mut bytes = checked_usize_bytes(
        std::mem::size_of::<FileIdentity>(),
        "journal identity size overflowed",
    )?;
    add_journal_bytes(
        &mut bytes,
        checked_usize_bytes(identity.platform.capacity(), "journal identity platform overflowed")?,
        "journal identity memory estimate overflowed",
    )?;
    add_journal_bytes(
        &mut bytes,
        JOURNAL_ELEMENT_FIXED_BYTES,
        "journal identity memory estimate overflowed",
    )?;
    Ok(bytes)
}

pub(super) fn journal_entry_retained_bytes(entry: &JournalEntry) -> Result<u64, CliError> {
    let mut bytes =
        checked_usize_bytes(std::mem::size_of::<JournalEntry>(), "journal entry size overflowed")?;
    add_journal_bytes(
        &mut bytes,
        journal_path_retained_bytes(&entry.target)?,
        "journal entry memory estimate overflowed",
    )?;
    if let Some(identity) = &entry.original {
        add_journal_bytes(
            &mut bytes,
            journal_identity_retained_bytes(identity)?,
            "journal entry memory estimate overflowed",
        )?;
    }
    add_journal_bytes(
        &mut bytes,
        checked_usize_bytes(entry.content_sha256.capacity(), "journal digest overflowed")?,
        "journal entry memory estimate overflowed",
    )?;
    if let Some(identity) = &entry.staged_identity {
        add_journal_bytes(
            &mut bytes,
            journal_identity_retained_bytes(identity)?,
            "journal entry memory estimate overflowed",
        )?;
    }
    add_journal_bytes(
        &mut bytes,
        JOURNAL_ELEMENT_FIXED_BYTES,
        "journal entry memory estimate overflowed",
    )?;
    Ok(bytes)
}

pub(super) fn stage_resume_retained_bytes(resume: &StageResume) -> Result<u64, CliError> {
    checked_usize_bytes(std::mem::size_of::<StageResume>(), "stage resume size overflowed")?
        .checked_add(checked_usize_bytes(
            resume.config_fingerprint.capacity(),
            "stage resume fingerprint overflowed",
        )?)
        .and_then(|bytes| {
            checked_usize_bytes(
                resume.source_fingerprint.capacity(),
                "stage resume source fingerprint overflowed",
            )
            .ok()
            .and_then(|source| bytes.checked_add(source))
        })
        .and_then(|bytes| {
            checked_usize_bytes(resume.content_sha256.capacity(), "stage resume digest overflowed")
                .ok()
                .and_then(|digest| bytes.checked_add(digest))
        })
        .and_then(|bytes| bytes.checked_add(JOURNAL_ELEMENT_FIXED_BYTES))
        .ok_or_else(|| transaction_index_limit("stage resume memory estimate overflowed"))
}

pub(super) fn recovered_tree_temporary_bytes(
    transaction_tree_bytes: u64,
    journal: &Journal,
) -> Result<u64, CliError> {
    let parent_leases = u64::try_from(journal.parent_identities.len())
        .unwrap_or(u64::MAX)
        .checked_mul(PARENT_LEASE_TEMPORARY_BYTES)
        .ok_or_else(|| transaction_index_limit("recovered parent lease budget overflowed"))?;
    let output_directories = journal
        .created_directories
        .len()
        .checked_add(journal.pending_directories.len())
        .and_then(|count| u64::try_from(count).ok())
        .and_then(|count| count.checked_mul(DIRECTORY_ENTRY_TEMPORARY_BYTES))
        .ok_or_else(|| transaction_index_limit("recovered directory budget overflowed"))?;
    transaction_tree_bytes
        .checked_add(parent_leases)
        .and_then(|bytes| bytes.checked_add(output_directories))
        .ok_or_else(|| transaction_index_limit("recovered temporary tree budget overflowed"))
}

pub(super) fn journal_retained_bytes(journal: &Journal) -> Result<u64, CliError> {
    let mut bytes = checked_usize_bytes(std::mem::size_of::<Journal>(), "journal size overflowed")?;
    for value in [
        checked_usize_bytes(journal.signature.capacity(), "journal signature overflowed")?,
        checked_usize_bytes(journal.nonce.capacity(), "journal nonce overflowed")?,
        journal_path_retained_bytes(&journal.root)?,
    ] {
        add_journal_bytes(&mut bytes, value, "journal memory estimate overflowed")?;
    }
    for identity in &journal.parent_identities {
        add_journal_bytes(
            &mut bytes,
            journal_identity_retained_bytes(identity)?,
            "journal parent memory estimate overflowed",
        )?;
    }
    for entry in &journal.entries {
        add_journal_bytes(
            &mut bytes,
            journal_entry_retained_bytes(entry)?,
            "journal entry memory estimate overflowed",
        )?;
    }
    for directory in &journal.created_directories {
        add_journal_bytes(
            &mut bytes,
            journal_path_retained_bytes(&directory.path)?,
            "journal directory memory estimate overflowed",
        )?;
        add_journal_bytes(
            &mut bytes,
            journal_identity_retained_bytes(&directory.identity)?,
            "journal directory memory estimate overflowed",
        )?;
    }
    for path in &journal.pending_directories {
        add_journal_bytes(
            &mut bytes,
            journal_path_retained_bytes(path)?,
            "journal pending memory estimate overflowed",
        )?;
    }
    // Account conservatively for Vec/hash-table capacity and allocator bookkeeping.
    bytes
        .checked_mul(2)
        .and_then(|value| value.checked_add(16 * 1024))
        .ok_or_else(|| transaction_index_limit("journal memory estimate overflowed"))
}
