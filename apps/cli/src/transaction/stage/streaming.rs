use super::{
    CliError, CreatedDirectory, DIRECTORY_ENTRY_TEMPORARY_BYTES, Digest, EntryState,
    ExecutionContext, ExitClass, FILE_ENTRY_TEMPORARY_BYTES, File, FileIdentity, HookDecision,
    JournalEntry, JournalPhase, JournalRecord, MAX_JOURNAL_ENTRIES, MAX_RECOVERY_RETRIES,
    PARENT_LEASE_TEMPORARY_BYTES, Path, PathBuf, PreparedTransaction, Read, SafeDir, Seek, Sha256,
    StreamingTargetIndex, Target, TransactionSource, Write, absolute_lexical, checked_usize_bytes,
    crash_point, create_missing_output_directory, encode_path, file_identity, fs, io,
    journal_entry_retained_bytes, journal_identity_retained_bytes, journal_path_retained_bytes,
    prepare_sources_with_hook_internal, recovery_error, stage_name, transaction_index_limit,
    validate_relative_path,
};
use crate::transaction::lease::{
    ParentLeaseRemovalIndex, create_parent_lease, remove_parent_lease,
};
use crate::transaction::model::JournalPath;
use crate::transaction::model::StageResume;
use crate::transaction::recovery::{
    remove_created_output_directory, try_resume_streaming_transaction,
};

const RESUME_CHUNK_BYTES: u64 = 1024 * 1024;

pub struct StreamingFileTransaction {
    pub(in crate::transaction) transaction: Option<PreparedTransaction>,
    pub(in crate::transaction) stage: Option<File>,
    pub(in crate::transaction) digest: Sha256,
    pub(in crate::transaction) size: u64,
    pub(in crate::transaction) current_index: usize,
    pub(in crate::transaction) target_index: Option<StreamingTargetIndex>,
    pub(in crate::transaction) config_fingerprint: String,
    pub(in crate::transaction) source_fingerprint: Option<String>,
    pub(in crate::transaction) chunk_sequence: u64,
    pub(in crate::transaction) durable_len: u64,
    pub(in crate::transaction) current_target: PathBuf,
}

impl StreamingFileTransaction {
    /// Create and durably register an empty authenticated stage for `path`.
    pub fn begin(
        path: &Path,
        overwrite: bool,
        context: &ExecutionContext,
    ) -> Result<Self, CliError> {
        Self::begin_with_root_hint_internal(path, None, overwrite, None, context)
    }

    /// Create a streaming transaction which may resume only when the exact
    /// authenticated local source file is unchanged.
    pub fn begin_resumable_local(
        path: &Path,
        source_path: &Path,
        source: &File,
        overwrite: bool,
        context: &ExecutionContext,
    ) -> Result<Self, CliError> {
        Self::begin_resumable_local_with_root_hint(
            path,
            None,
            source_path,
            source,
            overwrite,
            context,
        )
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
        Self::begin_with_root_hint_internal(path, additional_directory, overwrite, None, context)
    }

    /// Create a multi-artifact stream resumable only for the exact unchanged
    /// authenticated local source file. Streams without such a file identity,
    /// including standard input and remote inputs, use [`Self::begin_with_root_hint`]
    /// and are deliberately restarted after recovery.
    pub fn begin_resumable_local_with_root_hint(
        path: &Path,
        additional_directory: Option<&Path>,
        source_path: &Path,
        source: &File,
        overwrite: bool,
        context: &ExecutionContext,
    ) -> Result<Self, CliError> {
        let source_fingerprint = local_source_fingerprint(source_path, source, context)?;
        Self::begin_with_root_hint_internal(
            path,
            additional_directory,
            overwrite,
            Some(source_fingerprint),
            context,
        )
    }

    fn begin_with_root_hint_internal(
        path: &Path,
        additional_directory: Option<&Path>,
        overwrite: bool,
        source_fingerprint: Option<String>,
        context: &ExecutionContext,
    ) -> Result<Self, CliError> {
        let root_hint = additional_directory.map(|directory| directory.join(".into-md-authority"));
        let resume_target = absolute_lexical(path)?;
        let config_fingerprint =
            streaming_config_fingerprint(path, additional_directory, overwrite)?;
        if let Some(source_fingerprint) = source_fingerprint.as_deref()
            && let Some(resumed) = try_resume_streaming_transaction(
                &resume_target,
                &config_fingerprint,
                source_fingerprint,
                context,
            )?
        {
            return Ok(resumed);
        }
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
                        config_fingerprint,
                        source_fingerprint,
                        chunk_sequence: 0,
                        durable_len: 0,
                        current_target: absolute_lexical(path)?,
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
        let intent_parent =
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
        if let Err(error) =
            transaction.journal_temporary.grow(FILE_ENTRY_TEMPORARY_BYTES).map_err(CliError::from)
        {
            return transaction.fail_and_recover(error);
        }
        transaction
            .temporary_reservations
            .push(transaction.context.reserve_temporary(0).map_err(CliError::from)?);
        let record = JournalRecord::TargetAdded {
            parent: parent_is_new.then(|| authenticated_parent.identity.clone()),
            entry: entry.clone(),
        };
        if parent_is_new {
            transaction.journal.parent_identities.push(authenticated_parent.identity.clone());
        }
        transaction.journal.entries.push(entry);
        if parent_is_new
            && let Err(error) = create_parent_lease(
                &authenticated_parent,
                &transaction.handles.directory,
                &transaction.journal,
            )
        {
            return transaction.fail_and_recover(error);
        }
        if let Err(error) = transaction.append_journal_record(&record) {
            return transaction.fail_and_recover(error);
        }
        if parent_is_new && let Err(error) = transaction.sync_journal_records() {
            return transaction.fail_and_recover(error);
        }
        if let Some(intent_parent) = intent_parent {
            remove_dynamic_intent_lease(transaction, &intent_parent)?;
        }
        crash_point(&mut hook, "directoryCreated", usize::MAX, transaction)?;
        // The journal names the target before a stage can exist. Recovery from
        // this point therefore only rolls back; it can never publish a partial
        // or absent payload.
        crash_point(&mut hook, "directoryIdentityBound", usize::MAX, transaction)?;
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
        self.chunk_sequence = 0;
        self.durable_len = 0;
        self.current_target = absolute;
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
        self.transaction
            .as_ref()
            .ok_or_else(|| CliError::internal("streaming transaction is closed"))?
            .context
            .checkpoint()
            .map_err(CliError::from)?;
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
        self.transaction.as_mut().expect("streaming transaction checked").temporary_reservations
            [self.current_index]
            .grow(amount)
            .map_err(CliError::from)?;
        let mut written = 0_usize;
        while written < bytes.len() {
            if let Err(error) = self
                .transaction
                .as_ref()
                .expect("streaming transaction checked")
                .context
                .checkpoint()
            {
                let unwritten = amount - u64::try_from(written).unwrap_or(u64::MAX);
                self.transaction
                    .as_mut()
                    .expect("streaming transaction checked")
                    .temporary_reservations[self.current_index]
                    .shrink(unwritten)
                    .map_err(CliError::from)?;
                return Err(CliError::from(error));
            }
            let until_checkpoint = RESUME_CHUNK_BYTES - (self.size % RESUME_CHUNK_BYTES);
            let write_limit =
                usize::try_from(until_checkpoint).unwrap_or(usize::MAX).min(bytes.len() - written);
            let stage = self
                .stage
                .as_mut()
                .ok_or_else(|| CliError::internal("streaming transaction stage is closed"))?;
            match stage.write(&bytes[written..written + write_limit]) {
                Ok(0) => {
                    let unwritten = amount - u64::try_from(written).unwrap_or(u64::MAX);
                    self.transaction
                        .as_mut()
                        .expect("streaming transaction checked")
                        .temporary_reservations[self.current_index]
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
                    if self.size.is_multiple_of(RESUME_CHUNK_BYTES) {
                        self.checkpoint_current_chunk()?;
                    }
                }
                Err(error) => {
                    let unwritten = amount - u64::try_from(written).unwrap_or(u64::MAX);
                    self.transaction
                        .as_mut()
                        .expect("streaming transaction checked")
                        .temporary_reservations[self.current_index]
                        .shrink(unwritten)
                        .map_err(CliError::from)?;
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }

    fn checkpoint_current_chunk(&mut self) -> Result<(), CliError> {
        let Some(source_fingerprint) = self.source_fingerprint.as_ref() else {
            return Ok(());
        };
        let transaction = self
            .transaction
            .as_mut()
            .ok_or_else(|| CliError::internal("streaming transaction is closed"))?;
        let stage = self
            .stage
            .as_ref()
            .ok_or_else(|| CliError::internal("streaming transaction stage is closed"))?;
        stage.sync_all()?;
        let next_sequence = self
            .chunk_sequence
            .checked_add(1)
            .ok_or_else(|| transaction_index_limit("stage chunk sequence overflowed"))?;
        let resume = StageResume {
            config_fingerprint: self.config_fingerprint.clone(),
            source_fingerprint: source_fingerprint.clone(),
            chunk_sequence: next_sequence,
            durable_len: self.size,
            content_sha256: format!("{:x}", self.digest.clone().finalize()),
        };
        transaction.append_journal_record(&JournalRecord::StageChunk {
            index: self.current_index,
            resume,
        })?;
        transaction.sync_journal_records()?;
        self.chunk_sequence = next_sequence;
        self.durable_len = self.size;
        Ok(())
    }

    /// Bytes in the current stage covered by a durable resume checkpoint.
    #[must_use]
    pub fn resumable_bytes(&self) -> u64 {
        self.durable_len
    }

    /// Authenticated output path for the current resumable stage.
    #[must_use]
    pub fn resumable_target(&self) -> &Path {
        &self.current_target
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

fn streaming_config_fingerprint(
    path: &Path,
    additional_directory: Option<&Path>,
    overwrite: bool,
) -> Result<String, CliError> {
    let target = absolute_lexical(path)?;
    let root_hint = additional_directory.map(absolute_lexical).transpose()?;
    let mut digest = Sha256::new();
    digest.update(b"into-md-stream-resume-v2\0");
    digest.update([u8::from(overwrite)]);
    for value in [Some(target.as_path()), root_hint.as_deref()] {
        match value {
            Some(value) => {
                let bytes = value.as_os_str().as_encoded_bytes();
                digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
                digest.update(bytes);
            }
            None => digest.update(u64::MAX.to_le_bytes()),
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn local_source_fingerprint(
    source_path: &Path,
    source: &File,
    context: &ExecutionContext,
) -> Result<String, CliError> {
    let absolute = absolute_lexical(source_path)?;
    let path_metadata = fs::symlink_metadata(&absolute)?;
    if !path_metadata.file_type().is_file() || path_metadata.file_type().is_symlink() {
        return Err(CliError::new(
            ExitClass::Io,
            "unsafeInput",
            "resumable streaming requires a regular local source file",
        ));
    }
    let canonical = fs::canonicalize(&absolute)?;
    let mut file = source.try_clone()?;
    file.rewind()?;
    let before = file_identity(&file)?;
    if file_identity(&File::open(&canonical)?)? != before {
        return Err(CliError::new(
            ExitClass::Io,
            "inputIdentityChanged",
            "local source path does not name the authenticated source handle",
        ));
    }
    let first_digest = hash_local_source(&mut file, context)?;
    file.rewind()?;
    let second_digest = hash_local_source(&mut file, context)?;
    let after = file_identity(&file)?;
    if before != after
        || file_identity(&File::open(&canonical)?)? != before
        || first_digest != second_digest
    {
        return Err(CliError::new(
            ExitClass::Io,
            "inputIdentityChanged",
            "local source changed while its resume identity was authenticated",
        ));
    }
    let mut digest = Sha256::new();
    digest.update(b"into-md-local-stream-source-v1\0");
    let path_bytes = canonical.as_os_str().as_encoded_bytes();
    digest.update(u64::try_from(path_bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(path_bytes);
    digest.update(u64::try_from(before.platform.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(before.platform.as_bytes());
    digest.update(before.first.to_le_bytes());
    digest.update(before.second.to_le_bytes());
    digest.update(before.size.to_le_bytes());
    digest.update(first_digest);
    Ok(format!("{:x}", digest.finalize()))
}

fn hash_local_source(file: &mut File, context: &ExecutionContext) -> Result<[u8; 32], CliError> {
    const BUFFER_BYTES: usize = 64 * 1024;
    let _memory = context.reserve_memory(BUFFER_BYTES as u64).map_err(CliError::from)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; BUFFER_BYTES].into_boxed_slice();
    loop {
        context.checkpoint().map_err(CliError::from)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(digest.finalize().into());
        }
        digest.update(&buffer[..count]);
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
) -> Result<Option<SafeDir>, CliError> {
    let pending_growth = pending.iter().try_fold(0_u64, |total, path| {
        total
            .checked_add(journal_path_retained_bytes(path)?)
            .ok_or_else(|| transaction_index_limit("pending directory budget overflowed"))
    })?;
    if let Err(error) = transaction.account_journal_growth(pending_growth) {
        return transaction.fail_and_recover(error);
    }
    let created_growth = pending.iter().try_fold(0_u64, |total, path| {
        total
            .checked_add(journal_path_retained_bytes(path)?)
            .and_then(|bytes| {
                journal_identity_retained_bytes(&transaction.journal.root_identity)
                    .ok()
                    .and_then(|identity| bytes.checked_add(identity))
            })
            .ok_or_else(|| transaction_index_limit("created directory budget overflowed"))
    })?;
    if let Err(error) = transaction.account_journal_growth(created_growth) {
        return transaction.fail_and_recover(error);
    }
    let intent_parent = if let Some(highest_missing) = missing_directories.last() {
        let existing = highest_missing
            .parent()
            .ok_or_else(|| recovery_error("missing dynamic parent has no existing ancestor"))?;
        let relative = existing.strip_prefix(&transaction.root).map_err(|_| {
            recovery_error("dynamic intent ancestor is outside authenticated transaction root")
        })?;
        let parent = transaction.handles.root.open_descendant(relative)?;
        if transaction.journal.parent_identities.contains(&parent.identity) {
            None
        } else {
            let growth = journal_identity_retained_bytes(&parent.identity)?;
            if let Err(error) = transaction.account_journal_growth(growth) {
                return transaction.fail_and_recover(error);
            }
            if let Err(error) = transaction
                .journal_temporary
                .grow(PARENT_LEASE_TEMPORARY_BYTES)
                .map_err(CliError::from)
            {
                return transaction.fail_and_recover(error);
            }
            Some(parent)
        }
    } else {
        None
    };
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
    if let Some(parent) = &intent_parent
        && let Err(error) =
            create_parent_lease(parent, &transaction.handles.directory, &transaction.journal)
    {
        return transaction.fail_and_recover(error);
    }
    crash_point(hook, "directoryIntentPersisted", usize::MAX, transaction)?;
    let mut created_entries = Vec::with_capacity(missing_directories.len());
    for created in missing_directories.iter().rev() {
        let result = (|| -> Result<CreatedDirectory, CliError> {
            create_missing_output_directory(&transaction.root, &transaction.handles.root, created)?
                .ok_or_else(|| {
                    CliError::new(
                        ExitClass::Io,
                        "outputConflict",
                        format!(
                            "output directory appeared during preparation: {}",
                            created.display()
                        ),
                    )
                })
        })();
        match result {
            Ok(entry) => created_entries.push(entry),
            Err(error) => {
                return fail_dynamic_directory_creation(transaction, error, &created_entries);
            }
        }
    }
    record_dynamic_directories(transaction, created_entries)?;
    Ok(intent_parent)
}

fn remove_dynamic_intent_lease(
    transaction: &mut PreparedTransaction,
    parent: &SafeDir,
) -> Result<(), CliError> {
    let inserted = !transaction.journal.parent_identities.contains(&parent.identity);
    if inserted {
        transaction.journal.parent_identities.push(parent.identity.clone());
    }
    let result = ParentLeaseRemovalIndex::new(std::slice::from_ref(&parent.identity)).and_then(
        |mut index| {
            remove_parent_lease(
                parent,
                &transaction.handles.directory,
                &transaction.journal,
                &mut index,
            )
            .and_then(|()| index.finish())
        },
    );
    if inserted {
        transaction.journal.parent_identities.pop();
    }
    match result {
        Ok(()) => Ok(()),
        Err(error) => transaction.fail_and_recover(error),
    }
}

fn record_dynamic_directories(
    transaction: &mut PreparedTransaction,
    created_entries: Vec<CreatedDirectory>,
) -> Result<(), CliError> {
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
