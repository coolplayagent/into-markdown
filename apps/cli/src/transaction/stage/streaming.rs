use super::{
    CliError, CreatedDirectory, DIRECTORY_ENTRY_TEMPORARY_BYTES, Digest, EntryState,
    ExecutionContext, ExitClass, FILE_ENTRY_TEMPORARY_BYTES, File, FileIdentity, HookDecision,
    JournalEntry, JournalPhase, JournalRecord, MAX_JOURNAL_ENTRIES, MAX_RECOVERY_RETRIES,
    PARENT_LEASE_TEMPORARY_BYTES, Path, PathBuf, PreparedTransaction, Read, SafeDir, Sha256,
    StreamingTargetIndex, Target, TransactionSource, Write, absolute_lexical, checked_usize_bytes,
    crash_point, encode_path, file_identity, fs, io, journal_entry_retained_bytes,
    journal_identity_retained_bytes, journal_path_retained_bytes,
    prepare_sources_with_hook_internal, recovery_error, stage_name, transaction_index_limit,
    validate_relative_path,
};
use crate::transaction::lease::create_parent_lease;
use crate::transaction::model::JournalPath;
use crate::transaction::recovery::remove_created_output_directory;

pub struct StreamingFileTransaction {
    pub(in crate::transaction) transaction: Option<PreparedTransaction>,
    pub(in crate::transaction) stage: Option<File>,
    pub(in crate::transaction) digest: Sha256,
    pub(in crate::transaction) size: u64,
    pub(in crate::transaction) current_index: usize,
    pub(in crate::transaction) target_index: Option<StreamingTargetIndex>,
}

impl StreamingFileTransaction {
    /// Create and durably register an empty authenticated stage for `path`.
    pub fn begin(
        path: &Path,
        overwrite: bool,
        context: &ExecutionContext,
    ) -> Result<Self, CliError> {
        Self::begin_with_root_hint(path, None, overwrite, context)
    }

    /// Create a streaming transaction whose authenticated root also contains
    /// targets that will be registered later beneath `additional_directory`.
    /// The directory is only a root-selection hint: no target or physical
    /// parent lease exists for it until [`Self::begin_target`] durably adds a
    /// real target to the staging journal.
    pub fn begin_with_root_hint(
        path: &Path,
        additional_directory: Option<&Path>,
        overwrite: bool,
        context: &ExecutionContext,
    ) -> Result<Self, CliError> {
        let root_hint = additional_directory.map(|directory| directory.join(".into-md-authority"));
        for recovered in 0..=MAX_RECOVERY_RETRIES {
            context.checkpoint().map_err(CliError::from)?;
            match prepare_sources_with_hook_internal(
                &[Target { path: path.to_path_buf(), bytes: &[] }],
                overwrite,
                context,
                |_, _| Ok(HookDecision::Continue),
                true,
                root_hint.as_slice(),
            ) {
                Err(error) if error.code() == "transactionRecoveredRetry" => {
                    if recovered == MAX_RECOVERY_RETRIES {
                        return Err(CliError::new(
                            ExitClass::Io,
                            "recoveryLimit",
                            "streaming transaction recovery retry limit exceeded",
                        ));
                    }
                }
                Ok((mut transaction, Some(stage))) => {
                    let original = transaction
                        .journal
                        .entries
                        .first()
                        .and_then(|entry| entry.original.clone());
                    let primary_parent =
                        transaction.journal.parent_identities.first().cloned().ok_or_else(
                            || CliError::internal("streaming primary parent is missing"),
                        )?;
                    let primary = absolute_lexical(path)?;
                    let target_index =
                        match StreamingTargetIndex::new(primary, original, primary_parent, context)
                        {
                            Ok(index) => index,
                            Err(error) => return transaction.fail_and_recover(error),
                        };
                    return Ok(Self {
                        transaction: Some(transaction),
                        stage: Some(stage),
                        digest: Sha256::new(),
                        size: 0,
                        current_index: 0,
                        target_index: Some(target_index),
                    });
                }
                Ok((_, None)) => {
                    return Err(CliError::internal(
                        "streaming transaction did not return its authenticated stage",
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded recovery loop always returns")
    }

    /// Seal the current payload, durably register another target, and return
    /// a fresh authenticated stage for its ordered payload chunks.
    pub fn begin_target(&mut self, path: &Path, overwrite: bool) -> Result<(), CliError> {
        self.begin_target_with_hook(path, overwrite, |_, _| Ok(HookDecision::Continue))
    }

    pub(in crate::transaction) fn begin_target_with_hook(
        &mut self,
        path: &Path,
        overwrite: bool,
        hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<(), CliError> {
        match self.begin_target_with_hook_inner(path, overwrite, hook) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.close_after_operation_error(error)),
        }
    }

    fn begin_target_with_hook_inner(
        &mut self,
        path: &Path,
        overwrite: bool,
        mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<(), CliError> {
        self.seal_current()?;
        let transaction = self
            .transaction
            .as_mut()
            .ok_or_else(|| CliError::internal("streaming transaction is closed"))?;
        let target_index = self
            .target_index
            .as_mut()
            .ok_or_else(|| CliError::internal("streaming transaction index is closed"))?;
        let DynamicTargetPlan { absolute, encoded, parent, missing_directories, pending } =
            plan_dynamic_target(transaction, target_index, path)?;
        create_dynamic_directories(transaction, &pending, &missing_directories, &mut hook)?;
        let (authenticated_parent, original, parent_is_new, parent_index) =
            bind_dynamic_target(transaction, target_index, &absolute, &parent, overwrite)?;
        let entry = JournalEntry {
            target: encoded,
            parent_index: Some(parent_index),
            original,
            content_sha256: String::new(),
            size: 0,
            staged_identity: None,
            state: EntryState::Prepared,
        };
        let entry_growth = journal_entry_retained_bytes(&entry)?
            .checked_mul(2)
            .ok_or_else(|| transaction_index_limit("dynamic journal entry budget overflowed"))?;
        if let Err(error) = transaction.account_journal_growth(entry_growth) {
            return transaction.fail_and_recover(error);
        }
        let record = JournalRecord::TargetAdded {
            parent: parent_is_new.then(|| authenticated_parent.identity.clone()),
            entry: entry.clone(),
        };
        if let Err(error) = transaction.append_journal_record(&record) {
            return transaction.fail_and_recover(error);
        }
        if parent_is_new {
            transaction.journal.parent_identities.push(authenticated_parent.identity.clone());
            if let Err(error) = transaction.sync_journal_records() {
                return transaction.fail_and_recover(error);
            }
        }
        transaction.journal.entries.push(entry);
        if let Err(error) =
            transaction.journal_temporary.grow(FILE_ENTRY_TEMPORARY_BYTES).map_err(CliError::from)
        {
            return transaction.fail_and_recover(error);
        }
        transaction
            .temporary_reservations
            .push(transaction.context.reserve_temporary(0).map_err(CliError::from)?);
        // The journal names the target before a stage can exist. Recovery from
        // this point therefore only rolls back; it can never publish a partial
        // or absent payload.
        crash_point(&mut hook, "directoryIdentityBound", usize::MAX, transaction)?;
        if parent_is_new
            && let Err(error) = create_parent_lease(
                &authenticated_parent,
                &transaction.handles.directory,
                &transaction.journal,
            )
        {
            return transaction.fail_and_recover(error);
        }
        crash_point(&mut hook, "dynamicParentLeased", usize::MAX, transaction)?;
        let index = transaction.journal.entries.len() - 1;
        let file = match transaction.handles.directory.create_regular(&stage_name(index)) {
            Ok(file) => file,
            Err(error) => return transaction.fail_and_recover(error),
        };
        if let Err(error) = transaction.handles.directory.sync() {
            return transaction.fail_and_recover(error);
        }
        crash_point(&mut hook, "dynamicStageAllocated", index, transaction)?;
        self.stage = Some(file);
        self.digest = Sha256::new();
        self.size = 0;
        self.current_index = index;
        Ok(())
    }

    /// Append one payload chunk under the transaction's temporary-space budget.
    pub fn write_all_checked(&mut self, bytes: &[u8]) -> Result<(), CliError> {
        match self.write_all_checked_inner(bytes) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.close_after_operation_error(error)),
        }
    }

    fn write_all_checked_inner(&mut self, bytes: &[u8]) -> Result<(), CliError> {
        let transaction = self
            .transaction
            .as_mut()
            .ok_or_else(|| CliError::internal("streaming transaction is closed"))?;
        transaction.context.checkpoint().map_err(CliError::from)?;
        let amount = u64::try_from(bytes.len()).map_err(|_| {
            CliError::new(
                ExitClass::Policy,
                "resourceLimit",
                "streamed stage size cannot be represented",
            )
        })?;
        self.size.checked_add(amount).ok_or_else(|| {
            CliError::new(ExitClass::Policy, "resourceLimit", "streamed stage size overflowed")
        })?;
        transaction.temporary_reservations[self.current_index]
            .grow(amount)
            .map_err(CliError::from)?;
        let stage = self
            .stage
            .as_mut()
            .ok_or_else(|| CliError::internal("streaming transaction stage is closed"))?;
        let mut written = 0_usize;
        while written < bytes.len() {
            if let Err(error) = transaction.context.checkpoint() {
                let unwritten = amount - u64::try_from(written).unwrap_or(u64::MAX);
                transaction.temporary_reservations[self.current_index]
                    .shrink(unwritten)
                    .map_err(CliError::from)?;
                return Err(CliError::from(error));
            }
            match stage.write(&bytes[written..]) {
                Ok(0) => {
                    let unwritten = amount - u64::try_from(written).unwrap_or(u64::MAX);
                    transaction.temporary_reservations[self.current_index]
                        .shrink(unwritten)
                        .map_err(CliError::from)?;
                    return Err(CliError::from(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "streaming transaction stage write made no progress",
                    )));
                }
                Ok(count) => {
                    self.digest.update(&bytes[written..written + count]);
                    self.size = self
                        .size
                        .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
                        .ok_or_else(|| {
                            CliError::new(
                                ExitClass::Policy,
                                "resourceLimit",
                                "streamed stage size overflowed",
                            )
                        })?;
                    written += count;
                }
                Err(error) => {
                    let unwritten = amount - u64::try_from(written).unwrap_or(u64::MAX);
                    transaction.temporary_reservations[self.current_index]
                        .shrink(unwritten)
                        .map_err(CliError::from)?;
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }

    fn close_after_operation_error(&mut self, error: CliError) -> CliError {
        if error.code() == "simulatedCrash" {
            self.stage = None;
            self.target_index = None;
            // `crash_point` has already released in-process reservations and
            // deactivated the transaction. Dropping this inactive value leaves
            // its durable journal and stages for the next manager to recover,
            // while making this stream impossible to reuse in-process.
            self.transaction.take();
            return error;
        }
        self.stage = None;
        self.target_index = None;
        let Some(mut transaction) = self.transaction.take() else {
            return error;
        };
        if !transaction.active {
            return error;
        }
        transaction.fail_and_recover::<()>(error).unwrap_err()
    }

    /// Seal the authenticated stage and return the ordinary atomic commit transaction.
    pub fn seal(mut self) -> Result<PreparedTransaction, CliError> {
        self.seal_current()?;
        let mut transaction = self
            .transaction
            .take()
            .ok_or_else(|| CliError::internal("streaming transaction is closed"))?;
        if let Err(error) = transaction
            .context
            .checkpoint()
            .map_err(CliError::from)
            .and_then(|()| transaction.handles.directory.sync())
        {
            return transaction.fail_and_recover(error);
        }
        transaction.journal.phase = JournalPhase::Prepared;
        if let Err(error) = transaction.persist_journal() {
            return transaction.fail_and_recover(error);
        }
        Ok(transaction)
    }

    pub(in crate::transaction) fn seal_current(&mut self) -> Result<(), CliError> {
        let Some(stage) = self.stage.take() else {
            return Ok(());
        };
        let transaction = self
            .transaction
            .as_mut()
            .ok_or_else(|| CliError::internal("streaming transaction is closed"))?;
        if let Err(error) = transaction
            .context
            .checkpoint()
            .map_err(CliError::from)
            .and_then(|()| stage.sync_all().map_err(CliError::from))
        {
            return transaction.fail_and_recover(error);
        }
        let digest = format!("{:x}", self.digest.clone().finalize());
        let staged_identity = file_identity(&stage)?;
        let seal_growth = checked_usize_bytes(digest.capacity(), "journal digest overflowed")?
            .checked_add(journal_identity_retained_bytes(&staged_identity)?)
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or_else(|| transaction_index_limit("journal seal budget overflowed"))?;
        if let Err(error) = transaction.account_journal_growth(seal_growth) {
            return transaction.fail_and_recover(error);
        }
        let record = JournalRecord::StageSealed {
            index: self.current_index,
            size: self.size,
            content_sha256: digest.clone(),
            staged_identity: staged_identity.clone(),
        };
        if let Err(error) = transaction.append_journal_record(&record) {
            return transaction.fail_and_recover(error);
        }
        let entry = transaction.journal.entries.get_mut(self.current_index).ok_or_else(|| {
            CliError::internal("streaming transaction stage has no journal entry")
        })?;
        entry.size = self.size;
        entry.content_sha256 = digest;
        entry.staged_identity = Some(staged_identity);
        Ok(())
    }

    /// Roll back this unpublished stream.
    pub fn abort(mut self) -> Result<(), CliError> {
        self.stage = None;
        self.transaction.take().map_or(Ok(()), PreparedTransaction::abort)
    }

    #[cfg(test)]
    pub(in crate::transaction) fn abandon_for_test(mut self) {
        self.stage = None;
        if let Some(transaction) = self.transaction.take() {
            transaction.abandon_for_test();
        }
    }
}

struct DynamicTargetPlan {
    absolute: PathBuf,
    encoded: JournalPath,
    parent: PathBuf,
    missing_directories: Vec<PathBuf>,
    pending: Vec<JournalPath>,
}

fn bind_dynamic_target(
    transaction: &mut PreparedTransaction,
    target_index: &mut StreamingTargetIndex,
    absolute: &Path,
    parent: &Path,
    overwrite: bool,
) -> Result<(SafeDir, Option<FileIdentity>, bool, usize), CliError> {
    let parent_relative = parent.strip_prefix(&transaction.root).map_err(|_| {
        recovery_error("dynamic target parent is outside authenticated transaction root")
    })?;
    let authenticated_parent = transaction.handles.root.open_descendant(parent_relative)?;
    let name =
        absolute.file_name().ok_or_else(|| recovery_error("dynamic target has no file name"))?;
    let original = authenticated_parent.inspect_regular(name)?;
    if original.is_some() && !overwrite {
        return transaction.fail_and_recover(CliError::new(
            ExitClass::Io,
            "outputConflict",
            format!("output target already exists: {}", absolute.display()),
        ));
    }
    if let Some(identity) = &original
        && target_index.contains_original(identity)
    {
        return transaction.fail_and_recover(CliError::new(
            ExitClass::Io,
            "outputConflict",
            "multiple output paths resolve to the same existing file",
        ));
    }
    let parent_is_new = !target_index.contains_parent(&authenticated_parent.identity);
    if parent_is_new {
        let growth = journal_identity_retained_bytes(&authenticated_parent.identity)?;
        if let Err(error) = transaction.account_journal_growth(growth) {
            return transaction.fail_and_recover(error);
        }
        if let Err(error) =
            transaction.journal_temporary.grow(PARENT_LEASE_TEMPORARY_BYTES).map_err(CliError::from)
        {
            return transaction.fail_and_recover(error);
        }
    }
    if let Err(error) = target_index.insert_target(absolute.to_path_buf()) {
        return transaction.fail_and_recover(error);
    }
    if let Some(identity) = original.clone()
        && let Err(error) = target_index.insert_original(identity)
    {
        return transaction.fail_and_recover(error);
    }
    if parent_is_new
        && let Err(error) = target_index.insert_parent(authenticated_parent.identity.clone())
    {
        return transaction.fail_and_recover(error);
    }
    let parent_index = if parent_is_new {
        transaction.journal.parent_identities.len()
    } else {
        transaction
            .journal
            .parent_identities
            .iter()
            .position(|identity| identity == &authenticated_parent.identity)
            .ok_or_else(|| CliError::internal("streaming parent index lost its journal identity"))?
    };
    Ok((authenticated_parent, original, parent_is_new, parent_index))
}

fn create_dynamic_directories(
    transaction: &mut PreparedTransaction,
    pending: &[JournalPath],
    missing_directories: &[PathBuf],
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<(), CliError> {
    let pending_growth = pending.iter().try_fold(0_u64, |total, path| {
        total
            .checked_add(journal_path_retained_bytes(path)?)
            .ok_or_else(|| transaction_index_limit("pending directory budget overflowed"))
    })?;
    if let Err(error) = transaction.account_journal_growth(pending_growth) {
        return transaction.fail_and_recover(error);
    }
    let temporary_bytes = u64::try_from(missing_directories.len())
        .unwrap_or(u64::MAX)
        .checked_mul(DIRECTORY_ENTRY_TEMPORARY_BYTES)
        .ok_or_else(|| transaction_index_limit("directory metadata budget overflowed"))?;
    if let Err(error) = transaction.journal_temporary.grow(temporary_bytes).map_err(CliError::from)
    {
        return transaction.fail_and_recover(error);
    }
    if !pending.is_empty() {
        let record = JournalRecord::DirectoryIntent { paths: pending.to_vec() };
        if let Err(error) = transaction.append_journal_record(&record) {
            return transaction.fail_and_recover(error);
        }
        transaction.journal.pending_directories.extend_from_slice(pending);
        if let Err(error) = transaction.sync_journal_records() {
            return transaction.fail_and_recover(error);
        }
    }
    crash_point(hook, "directoryIntentPersisted", usize::MAX, transaction)?;
    let mut created_entries = Vec::with_capacity(missing_directories.len());
    for created in missing_directories.iter().rev() {
        let result = (|| -> Result<CreatedDirectory, CliError> {
            SafeDir::open_or_create_absolute(created)?;
            let relative = created.strip_prefix(&transaction.root).map_err(|_| {
                recovery_error("created output directory is outside transaction root")
            })?;
            let handle = transaction.handles.root.open_descendant(relative)?;
            Ok(CreatedDirectory { path: encode_path(relative)?, identity: handle.identity })
        })();
        match result {
            Ok(entry) => created_entries.push(entry),
            Err(error) => {
                return fail_dynamic_directory_creation(transaction, error, &created_entries);
            }
        }
    }
    if let Err(error) = crash_point(hook, "directoryCreated", usize::MAX, transaction) {
        if error.code() == "simulatedCrash" {
            return Err(error);
        }
        return fail_dynamic_directory_creation(transaction, error, &created_entries);
    }
    record_dynamic_directories(transaction, created_entries)
}

fn record_dynamic_directories(
    transaction: &mut PreparedTransaction,
    created_entries: Vec<CreatedDirectory>,
) -> Result<(), CliError> {
    let growth = created_entries.iter().try_fold(0_u64, |total, directory| {
        total
            .checked_add(journal_path_retained_bytes(&directory.path)?)
            .and_then(|bytes| {
                journal_identity_retained_bytes(&directory.identity)
                    .ok()
                    .and_then(|identity| bytes.checked_add(identity))
            })
            .ok_or_else(|| transaction_index_limit("created directory budget overflowed"))
    });
    let growth = match growth {
        Ok(growth) => growth,
        Err(error) => return fail_dynamic_directory_creation(transaction, error, &created_entries),
    };
    if let Err(error) = transaction.account_journal_growth(growth) {
        return fail_dynamic_directory_creation(transaction, error, &created_entries);
    }
    if !created_entries.is_empty() {
        let record = JournalRecord::DirectoriesCreated { directories: created_entries.clone() };
        if let Err(error) = transaction.append_journal_record(&record) {
            return fail_dynamic_directory_creation(transaction, error, &created_entries);
        }
        if let Err(error) = transaction.sync_journal_records() {
            return fail_dynamic_directory_creation(transaction, error, &created_entries);
        }
    }
    transaction.journal.created_directories.extend(created_entries);
    transaction.journal.pending_directories.clear();
    Ok(())
}

fn plan_dynamic_target(
    transaction: &mut PreparedTransaction,
    target_index: &StreamingTargetIndex,
    path: &Path,
) -> Result<DynamicTargetPlan, CliError> {
    transaction.context.checkpoint().map_err(CliError::from)?;
    if transaction.journal.phase != JournalPhase::Staging {
        return Err(CliError::internal("streaming target was added after transaction preparation"));
    }
    if transaction.journal.entries.len() >= MAX_JOURNAL_ENTRIES {
        return transaction.fail_and_recover(CliError::new(
            ExitClass::Policy,
            "transactionJournalLimit",
            "output transaction has too many targets",
        ));
    }
    let absolute = absolute_lexical(path)?;
    let relative = absolute.strip_prefix(&transaction.root).map_err(|_| {
        CliError::new(
            ExitClass::Io,
            "outputPathUnsupported",
            "dynamic target is outside the authenticated transaction root",
        )
    })?;
    validate_relative_path(relative)?;
    let encoded = encode_path(relative)?;
    if target_index.contains_target(&absolute) {
        return transaction.fail_and_recover(CliError::new(
            ExitClass::Io,
            "outputConflict",
            format!("duplicate transaction target: {}", absolute.display()),
        ));
    }
    let parent = absolute
        .parent()
        .ok_or_else(|| recovery_error("dynamic target has no parent"))?
        .to_path_buf();
    let mut missing_directories = Vec::new();
    let mut candidate = parent.as_path();
    while candidate.starts_with(&transaction.root) {
        match fs::symlink_metadata(candidate) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing_directories.push(candidate.to_path_buf());
                candidate = candidate.parent().ok_or_else(|| {
                    recovery_error("dynamic target has no existing directory ancestor")
                })?;
            }
            Err(error) => return transaction.fail_and_recover(error.into()),
        }
    }
    let pending = missing_directories
        .iter()
        .rev()
        .map(|created| {
            created
                .strip_prefix(&transaction.root)
                .map_err(|_| recovery_error("created output directory is outside transaction root"))
                .and_then(encode_path)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DynamicTargetPlan { absolute, encoded, parent, missing_directories, pending })
}

fn fail_dynamic_directory_creation<T>(
    transaction: &mut PreparedTransaction,
    original: CliError,
    created: &[CreatedDirectory],
) -> Result<T, CliError> {
    for directory in created.iter().rev() {
        if let Err(cleanup) = remove_created_output_directory(
            &transaction.handles.root,
            &directory.path,
            Some(&directory.identity),
        ) {
            return transaction.fail_and_recover(CliError::new(
                ExitClass::Io,
                "rollbackFailed",
                format!(
                    "streaming target registration failed ({}: {}); created-directory rollback failed ({}: {})",
                    original.code(),
                    original.message(),
                    cleanup.code(),
                    cleanup.message()
                ),
            ));
        }
    }
    transaction.fail_and_recover(original)
}

impl Drop for StreamingFileTransaction {
    fn drop(&mut self) {
        self.stage = None;
        if let Some(transaction) = self.transaction.take() {
            let _ = transaction.abort();
        }
    }
}
