use super::{
    BTreeSet, CLEANUP_PREFIX, CliError, Deserialize, Digest, ExecutionContext, ExitClass, File,
    FileIdentity, HashSet, INITIAL_PREFIX, JOURNAL_SIGNATURE, JOURNAL_VERSION, Journal,
    JournalPhase, JournalRecord, MAX_JOURNAL_BYTES, MAX_JOURNAL_ENTRIES, OsStr, OsString, Path,
    PreparedTransaction, Read, ResourceReservation, SafeDir, Seek, Serialize, Sha256,
    TRANSACTION_PREFIX, TransactionSource, Write, decode_path, fs, io, journal_retained_bytes,
    recovery_error, remove_regular_handle_if_present, transaction_registry, validate_relative_path,
};

const JOURNAL_READ_BUFFER_BYTES: usize = 64 * 1024;
const JOURNAL_DECODE_FIXED_BYTES: u64 = 64 * 1024;
const JOURNAL_DECODE_EXPANSION: u64 = 4;

mod replay;
pub(super) use replay::replay_journal_records;

/// Keeps the exact decoded-journal memory charge and the authenticated files'
/// temporary-storage charges alive for every recovery operation that borrows
/// the journal.
pub(super) struct LoadedJournal {
    journal: Journal,
    memory: ResourceReservation,
    temporary: [Option<ResourceReservation>; 2],
    retained_bytes: u64,
}

impl std::ops::Deref for LoadedJournal {
    type Target = Journal;

    fn deref(&self) -> &Self::Target {
        &self.journal
    }
}

impl std::ops::DerefMut for LoadedJournal {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.journal
    }
}

impl PreparedTransaction {
    pub(super) fn account_journal_growth(&mut self, bytes: u64) -> Result<(), CliError> {
        self.journal_memory.grow(bytes).map_err(CliError::from)
    }

    pub(super) fn append_journal_record(&mut self, record: &JournalRecord) -> Result<(), CliError> {
        let bytes = append_journal_record_handle(
            &self.handles.directory,
            &mut self.journal,
            record,
            &mut self.journal_temporary,
            &mut self.journal_log_bytes,
        )?;
        #[cfg(test)]
        {
            self.journal_record_calls = self.journal_record_calls.saturating_add(1);
            self.journal_record_bytes = self.journal_record_bytes.saturating_add(bytes);
        }
        Ok(())
    }

    pub(super) fn sync_journal_records(&mut self) -> Result<(), CliError> {
        if self.journal_log_bytes != 0 {
            self.handles.directory.open_regular_append(OsStr::new(JOURNAL_LOG_NAME))?.sync_all()?;
            self.handles.directory.sync()?;
            #[cfg(test)]
            {
                self.journal_record_sync_calls = self.journal_record_sync_calls.saturating_add(1);
            }
        }
        Ok(())
    }

    pub(super) fn persist_journal(&mut self) -> Result<(), CliError> {
        self.sync_journal_records()?;
        let result = persist_journal_handle(
            &self.handles.directory,
            &mut self.journal,
            &mut self.journal_temporary,
            &mut self.journal_slot_bytes,
        );
        #[cfg(test)]
        if result.is_ok() {
            self.journal_persist_calls = self.journal_persist_calls.saturating_add(1);
        }
        result
    }
}

pub(super) const JOURNAL_LOG_NAME: &str = "journal.log";
pub(super) const JOURNAL_RECORD_MAGIC: [u8; 8] = *b"IMDJRNL1";
pub(super) const JOURNAL_RECORD_HEADER_BYTES: u64 = 8 + 8 + 32;
const JOURNAL_RECORD_HEADER_SIZE: usize = 8 + 8 + 32;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct JournalRecordEnvelope<'a> {
    sequence: u64,
    record: &'a JournalRecord,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OwnedJournalRecordEnvelope {
    sequence: u64,
    record: JournalRecord,
}

pub(super) struct HashingJournalWriter<'a> {
    inner: Option<&'a mut File>,
    digest: Sha256,
    written: u64,
    limit: u64,
}

impl Write for HashingJournalWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("transaction journal record length overflowed"))?;
        if next > self.limit {
            return Err(io::Error::other("transaction journal record exceeds its byte limit"));
        }
        let count =
            if let Some(inner) = self.inner.as_mut() { inner.write(bytes)? } else { bytes.len() };
        self.digest.update(&bytes[..count]);
        self.written = self
            .written
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("transaction journal record length overflowed"))?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.as_deref_mut().map_or(Ok(()), Write::flush)
    }
}

pub(super) fn append_journal_record_handle(
    directory: &SafeDir,
    journal: &mut Journal,
    record: &JournalRecord,
    temporary: &mut ResourceReservation,
    log_bytes: &mut u64,
) -> Result<u64, CliError> {
    let sequence = journal.log_sequence.checked_add(1).ok_or_else(|| {
        CliError::new(ExitClass::Io, "transactionJournalOverflow", "journal sequence overflow")
    })?;
    let envelope = JournalRecordEnvelope { sequence, record };
    let mut measured = HashingJournalWriter {
        inner: None,
        digest: Sha256::new(),
        written: 0,
        limit: MAX_JOURNAL_BYTES,
    };
    serde_json::to_writer(&mut measured, &envelope)
        .map_err(|error| CliError::internal(format!("measure journal record: {error}")))?;
    let payload_bytes = measured.written;
    let expected_digest: [u8; 32] = measured.digest.finalize().into();
    let frame_bytes = JOURNAL_RECORD_HEADER_BYTES.checked_add(payload_bytes).ok_or_else(|| {
        CliError::new(
            ExitClass::Policy,
            "transactionJournalLimit",
            "journal record byte count overflowed",
        )
    })?;
    let next_log_bytes = log_bytes.checked_add(frame_bytes).ok_or_else(|| {
        CliError::new(
            ExitClass::Policy,
            "transactionJournalLimit",
            "journal log byte count overflowed",
        )
    })?;
    if next_log_bytes > MAX_JOURNAL_BYTES {
        return Err(CliError::new(
            ExitClass::Policy,
            "transactionJournalLimit",
            "output transaction journal log exceeds its byte limit",
        ));
    }
    temporary.grow(frame_bytes).map_err(CliError::from)?;
    let name = OsStr::new(JOURNAL_LOG_NAME);
    let mut file = if directory.inspect_regular(name)?.is_some() {
        directory.open_regular_append(name)?
    } else {
        let file = directory.create_regular_private(name)?;
        directory.sync()?;
        file
    };
    let result = (|| {
        file.write_all(&JOURNAL_RECORD_MAGIC)?;
        file.write_all(&payload_bytes.to_le_bytes())?;
        file.write_all(&expected_digest)?;
        let mut written = HashingJournalWriter {
            inner: Some(&mut file),
            digest: Sha256::new(),
            written: 0,
            limit: payload_bytes,
        };
        serde_json::to_writer(&mut written, &envelope)
            .map_err(|error| CliError::internal(format!("serialize journal record: {error}")))?;
        written.flush().map_err(CliError::from)?;
        let actual_digest: [u8; 32] = written.digest.finalize().into();
        if written.written != payload_bytes || actual_digest != expected_digest {
            return Err(CliError::internal("journal record changed while it was serialized"));
        }
        Ok(())
    })();
    result?;
    journal.log_sequence = sequence;
    *log_bytes = next_log_bytes;
    Ok(frame_bytes)
}

#[cfg(any(unix, windows))]
pub(super) fn persist_journal_handle(
    directory: &SafeDir,
    journal: &mut Journal,
    temporary: &mut ResourceReservation,
    slot_bytes: &mut [u64; 2],
) -> Result<(), CliError> {
    journal.generation = journal.generation.checked_add(1).ok_or_else(|| {
        CliError::new(ExitClass::Io, "transactionJournalOverflow", "journal generation overflow")
    })?;
    let slot = usize::from(journal.generation.is_multiple_of(2));
    let name = if slot == 1 { "journal-b.json" } else { "journal-a.json" };
    let name = OsStr::new(name);
    let serialized_bytes = serialized_journal_bytes(journal)?;
    if serialized_bytes > MAX_JOURNAL_BYTES {
        return Err(CliError::new(
            ExitClass::Policy,
            "transactionJournalLimit",
            "output transaction journal exceeds its byte limit",
        ));
    }
    let persisted_bytes = serialized_bytes.checked_add(1).ok_or_else(|| {
        CliError::new(
            ExitClass::Policy,
            "transactionJournalLimit",
            "output transaction journal byte count overflowed",
        )
    })?;
    let old_bytes = slot_bytes[slot];
    let growth = persisted_bytes.saturating_sub(old_bytes);
    temporary.grow(growth).map_err(CliError::from)?;
    let result = (|| {
        if directory.inspect_regular(name)?.is_some() {
            directory.remove_regular(name)?;
            directory.sync()?;
        }
        let mut file = directory.create_regular(name)?;
        let mut writer = LimitedJournalWriter { inner: &mut file, written: 0 };
        serde_json::to_writer(&mut writer, journal).map_err(|error| {
            CliError::internal(format!("serialize output transaction journal: {error}"))
        })?;
        if writer.written != serialized_bytes {
            return Err(CliError::internal(
                "streamed transaction journal length changed between validation and write",
            ));
        }
        writer.write_all(b"\n")?;
        file.sync_all()?;
        directory.sync()
    })();
    if let Err(error) = result {
        let _ = remove_regular_handle_if_present(directory, name);
        temporary.shrink(growth).map_err(CliError::from)?;
        return Err(error);
    }
    if persisted_bytes < old_bytes {
        temporary.shrink(old_bytes - persisted_bytes).map_err(CliError::from)?;
    }
    slot_bytes[slot] = persisted_bytes;
    Ok(())
}

pub(super) struct CountingJournalWriter {
    written: u64,
}

impl Write for CountingJournalWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.written = self
            .written
            .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("transaction journal length overflowed"))?;
        if self.written > MAX_JOURNAL_BYTES {
            return Err(io::Error::other("transaction journal exceeds its byte limit"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn serialized_journal_bytes(journal: &Journal) -> Result<u64, CliError> {
    let mut writer = CountingJournalWriter { written: 0 };
    if let Err(error) = serde_json::to_writer(&mut writer, journal) {
        if writer.written > MAX_JOURNAL_BYTES {
            return Err(CliError::new(
                ExitClass::Policy,
                "transactionJournalLimit",
                "output transaction journal exceeds its byte limit",
            ));
        }
        return Err(CliError::internal(format!("measure output transaction journal: {error}")));
    }
    Ok(writer.written)
}

pub(super) struct LimitedJournalWriter<'a> {
    inner: &'a mut File,
    written: u64,
}

impl Write for LimitedJournalWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("transaction journal length overflowed"))?;
        if next > MAX_JOURNAL_BYTES + 1 {
            return Err(io::Error::other("transaction journal exceeds its byte limit"));
        }
        let count = self.inner.write(bytes)?;
        self.written = self
            .written
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("transaction journal length overflowed"))?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(not(any(unix, windows)))]
pub(super) fn persist_journal_handle(
    _directory: &SafeDir,
    _journal: &mut Journal,
    _temporary: &mut ResourceReservation,
    _slot_bytes: &mut [u64; 2],
) -> Result<(), CliError> {
    Err(transaction_platform_unavailable())
}

pub(super) fn load_journal(
    root: &Path,
    directory: &Path,
    nonce: &str,
    context: &ExecutionContext,
) -> Result<LoadedJournal, CliError> {
    #[cfg(any(unix, windows))]
    {
        let root_handle = SafeDir::open_absolute(root)?;
        let registry = transaction_registry(&root_handle, false)?
            .ok_or_else(|| recovery_error("transaction registry is missing"))?;
        let directory_name = directory
            .file_name()
            .ok_or_else(|| recovery_error("transaction directory has no name"))?;
        let directory_handle = registry.open_child(directory_name)?;
        load_journal_handle(&root_handle, &directory_handle, directory, nonce, context)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, directory, nonce, context);
        Err(transaction_platform_unavailable())
    }
}

#[cfg(any(unix, windows))]
pub(super) fn load_journal_handle(
    root: &SafeDir,
    directory: &SafeDir,
    directory_path: &Path,
    nonce: &str,
    context: &ExecutionContext,
) -> Result<LoadedJournal, CliError> {
    context.checkpoint().map_err(CliError::from)?;
    let mut probes = Vec::with_capacity(2);
    for name in ["journal-a.json", "journal-b.json"] {
        if let Some(probe) = load_slot_value::<JournalGenerationProbe>(
            directory,
            OsStr::new(name),
            context,
            |_| Ok(0),
            generation_probe_memory_bytes,
        )? {
            probes.push((name, probe.value.generation));
        }
    }
    probes.sort_by_key(|probe| std::cmp::Reverse(probe.1));

    let mut selected = None;
    let mut index = 0;
    while index < probes.len() {
        let (name, generation) = probes[index];
        let Some(candidate) =
            load_valid_journal_slot(root, directory, directory_path, nonce, name, context)?
        else {
            index += 1;
            continue;
        };
        if candidate.generation != generation {
            index += 1;
            continue;
        }
        if probes.get(index + 1).is_some_and(|other| other.1 == generation) {
            // Never retain two complete journals. Release the selected slot
            // while authenticating its equal-generation peer, then reload it
            // only when the peer proves invalid.
            drop(candidate);
            let other_name = probes[index + 1].0;
            if load_valid_journal_slot(root, directory, directory_path, nonce, other_name, context)?
                .is_some()
            {
                return Err(recovery_error("ambiguous journal generations"));
            }
            selected =
                load_valid_journal_slot(root, directory, directory_path, nonce, name, context)?;
        } else {
            selected = Some(candidate);
        }
        break;
    }
    let mut journal = selected.ok_or_else(|| {
        recovery_error(format!("no valid signed journal in {}", directory_path.display()))
    })?;
    replay_journal_records(directory, &mut journal, context)?;
    validate_journal_handle(root, directory_path, &journal)?;
    Ok(journal)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalGenerationProbe {
    generation: u64,
}

struct LoadedSlot<T> {
    value: T,
    memory: ResourceReservation,
    temporary: ResourceReservation,
    retained_bytes: u64,
}

impl LoadedJournal {
    fn update_memory_charge(&mut self) -> Result<(), CliError> {
        let retained_bytes = journal_retained_bytes(&self.journal)?;
        if retained_bytes > self.retained_bytes {
            self.memory.grow(retained_bytes - self.retained_bytes).map_err(CliError::from)?;
        } else if retained_bytes < self.retained_bytes {
            self.memory.shrink(self.retained_bytes - retained_bytes).map_err(CliError::from)?;
        }
        self.retained_bytes = retained_bytes;
        Ok(())
    }
}

fn load_valid_journal_slot(
    root: &SafeDir,
    directory: &SafeDir,
    directory_path: &Path,
    nonce: &str,
    name: &str,
    context: &ExecutionContext,
) -> Result<Option<LoadedJournal>, CliError> {
    let Some(slot) = load_slot_value::<Journal>(
        directory,
        OsStr::new(name),
        context,
        journal_retained_bytes,
        journal_decode_memory_bytes,
    )?
    else {
        return Ok(None);
    };
    if slot.value.nonce != nonce
        || validate_journal_handle(root, directory_path, &slot.value).is_err()
    {
        return Ok(None);
    }
    Ok(Some(LoadedJournal {
        journal: slot.value,
        memory: slot.memory,
        temporary: [Some(slot.temporary), None],
        retained_bytes: slot.retained_bytes,
    }))
}

fn load_slot_value<T>(
    directory: &SafeDir,
    name: &OsStr,
    context: &ExecutionContext,
    retained_bytes: impl FnOnce(&T) -> Result<u64, CliError>,
    decode_memory_bytes: impl FnOnce(u64) -> Result<u64, CliError>,
) -> Result<Option<LoadedSlot<T>>, CliError>
where
    T: for<'de> Deserialize<'de>,
{
    let Some(mut file) = (match directory.open_regular_optional(name) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    }) else {
        return Ok(None);
    };
    let file_bytes = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return Ok(None),
    };
    let persisted_limit = MAX_JOURNAL_BYTES
        .checked_add(1)
        .ok_or_else(|| recovery_error("journal byte limit overflowed"))?;
    if file_bytes > persisted_limit || !slot_boundary_is_valid(&mut file, file_bytes)? {
        return Ok(None);
    }
    file.rewind()?;
    let temporary = context.reserve_temporary(file_bytes).map_err(CliError::from)?;
    let buffer_bytes = journal_buffer_bytes(file_bytes);
    // Reserve before serde can allocate. The conservative expansion covers
    // decoded Vec/String capacity as well as the fixed streaming input buffer.
    // Only one slot receives this complete-candidate permit at a time.
    let decode_bytes = decode_memory_bytes(file_bytes)?;
    let mut memory = context.reserve_memory(decode_bytes).map_err(CliError::from)?;
    let mut reader = std::io::BufReader::with_capacity(buffer_bytes, file.take(file_bytes));
    let mut decoder = serde_json::Deserializer::from_reader(&mut reader);
    let Ok(value) = T::deserialize(&mut decoder) else { return Ok(None) };
    if decoder.end().is_err() {
        return Ok(None);
    }
    context.checkpoint().map_err(CliError::from)?;
    drop(decoder);
    let mut file = reader.into_inner().into_inner();
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Ok(None);
    }
    let retained_bytes = retained_bytes(&value)?;
    if retained_bytes > decode_bytes {
        memory.grow(retained_bytes - decode_bytes).map_err(CliError::from)?;
    } else if retained_bytes < decode_bytes {
        memory.shrink(decode_bytes - retained_bytes).map_err(CliError::from)?;
    }
    Ok(Some(LoadedSlot { value, memory, temporary, retained_bytes }))
}

fn journal_buffer_bytes(file_bytes: u64) -> usize {
    usize::try_from(file_bytes.min(JOURNAL_READ_BUFFER_BYTES as u64)).unwrap_or(1).max(1)
}

fn generation_probe_memory_bytes(file_bytes: u64) -> Result<u64, CliError> {
    u64::try_from(journal_buffer_bytes(file_bytes))
        .unwrap_or(u64::MAX)
        .checked_add(JOURNAL_DECODE_FIXED_BYTES)
        .ok_or_else(|| recovery_error("journal probe memory bound overflowed"))
}

pub(super) fn journal_decode_memory_bytes(file_bytes: u64) -> Result<u64, CliError> {
    file_bytes
        .checked_mul(JOURNAL_DECODE_EXPANSION)
        .and_then(|bytes| bytes.checked_add(JOURNAL_DECODE_FIXED_BYTES))
        .and_then(|bytes| {
            bytes.checked_add(u64::try_from(journal_buffer_bytes(file_bytes)).unwrap_or(u64::MAX))
        })
        .ok_or_else(|| recovery_error("journal decode memory bound overflowed"))
}

pub(super) fn slot_boundary_is_valid(file: &mut File, file_bytes: u64) -> Result<bool, CliError> {
    if file_bytes <= MAX_JOURNAL_BYTES {
        return Ok(true);
    }
    file.seek(io::SeekFrom::End(-1))?;
    let mut newline = [0_u8; 1];
    file.read_exact(&mut newline)?;
    Ok(newline == *b"\n")
}

pub(super) fn validate_journal(
    root: &Path,
    directory: &Path,
    journal: &Journal,
) -> Result<(), CliError> {
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
pub(super) fn validate_journal_handle(
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
        let digest_is_sealed = entry.content_sha256.len() == 64
            && entry.content_sha256.bytes().all(|byte| byte.is_ascii_hexdigit());
        let digest_is_pending = journal.phase == JournalPhase::Staging
            && entry.content_sha256.is_empty()
            && entry.size == 0
            && entry.staged_identity.is_none();
        if !digest_is_sealed && !digest_is_pending {
            return Err(recovery_error("invalid transaction content digest"));
        }
        if journal.phase != JournalPhase::Staging && entry.staged_identity.is_none() {
            return Err(recovery_error(
                "transaction stage identity does not match its sealed content",
            ));
        }
        let requires_parent =
            journal.phase != JournalPhase::Staging || entry.staged_identity.is_some();
        if requires_parent && entry.parent_index.is_none() {
            return Err(recovery_error("sealed transaction target has no bound parent identity"));
        }
        if entry.parent_index.is_some_and(|index| index >= journal.parent_identities.len()) {
            return Err(recovery_error("transaction target parent index is outside limits"));
        }
        let relative = decode_path(&entry.target)?;
        validate_relative_path(&relative)?;
        if !targets.insert(entry.target.units.clone()) {
            return Err(recovery_error("duplicate transaction target"));
        }
    }
    let unique_parents = journal.parent_identities.iter().collect::<HashSet<_>>();
    if journal.parent_identities.len() > journal.entries.len()
        || unique_parents.len() != journal.parent_identities.len()
    {
        return Err(recovery_error("journal physical parent identities are invalid"));
    }
    if journal
        .created_directories
        .len()
        .checked_add(journal.pending_directories.len())
        .is_none_or(|count| count > MAX_JOURNAL_ENTRIES)
    {
        return Err(recovery_error("created output directory inventory exceeds its limit"));
    }
    let mut created_paths = BTreeSet::new();
    for pending in &journal.pending_directories {
        let relative = decode_path(pending)?;
        validate_relative_path(&relative)?;
        if relative.as_os_str().is_empty() || !created_paths.insert(pending.units.clone()) {
            return Err(recovery_error("pending output directory path is invalid or duplicated"));
        }
    }
    for created in &journal.created_directories {
        let relative = decode_path(&created.path)?;
        validate_relative_path(&relative)?;
        if relative.as_os_str().is_empty() || !created_paths.insert(created.path.units.clone()) {
            return Err(recovery_error("created output directory path is invalid or duplicated"));
        }
        match fs::symlink_metadata(root.path.join(&relative)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let directory = root.open_descendant(&relative)?;
        if directory.identity != created.identity {
            return Err(recovery_error("created output directory identity changed"));
        }
    }
    Ok(())
}
