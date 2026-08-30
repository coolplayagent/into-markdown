use super::super::{
    CliError, Deserialize, Digest, ExecutionContext, FileIdentity, HashSet, Journal, JournalPhase,
    JournalRecord, MAX_JOURNAL_BYTES, MAX_JOURNAL_ENTRIES, OsStr, Read, SafeDir, Sha256, io,
    recovery_error,
};
use super::{
    JOURNAL_LOG_NAME, JOURNAL_RECORD_HEADER_BYTES, JOURNAL_RECORD_HEADER_SIZE,
    JOURNAL_RECORD_MAGIC, LoadedJournal, OwnedJournalRecordEnvelope, journal_buffer_bytes,
};

pub(in crate::transaction) fn replay_journal_records(
    directory: &SafeDir,
    journal: &mut LoadedJournal,
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let Some(file) = directory.open_regular_optional(OsStr::new(JOURNAL_LOG_NAME))? else {
        return Ok(());
    };
    let file_bytes = file.metadata()?.len();
    if file_bytes > MAX_JOURNAL_BYTES {
        return Err(recovery_error("journal log exceeds its byte limit"));
    }
    let temporary = context.reserve_temporary(file_bytes).map_err(CliError::from)?;
    let buffer_bytes = journal_buffer_bytes(file_bytes);
    let _buffer_memory = context
        .reserve_memory(u64::try_from(buffer_bytes).unwrap_or(u64::MAX))
        .map_err(CliError::from)?;
    let mut reader = std::io::BufReader::with_capacity(buffer_bytes, file.take(file_bytes));
    let mut consumed = 0_u64;
    let mut expected_sequence = 1_u64;
    let mut parent_index = journal.parent_identities.iter().cloned().collect::<HashSet<_>>();
    if parent_index.len() != journal.parent_identities.len() {
        return Err(recovery_error("journal contains duplicate physical parent identities"));
    }
    let header_bytes = usize::try_from(JOURNAL_RECORD_HEADER_BYTES)
        .map_err(|_| recovery_error("journal record header size is invalid"))?;
    while consumed < file_bytes {
        context.checkpoint().map_err(CliError::from)?;
        if file_bytes - consumed < JOURNAL_RECORD_HEADER_BYTES {
            break;
        }
        let mut header = [0_u8; JOURNAL_RECORD_HEADER_SIZE];
        if !read_complete_or_tail(&mut reader, &mut header)? {
            break;
        }
        consumed = consumed
            .checked_add(JOURNAL_RECORD_HEADER_BYTES)
            .ok_or_else(|| recovery_error("journal record offset overflowed"))?;
        if header[..8] != JOURNAL_RECORD_MAGIC {
            return Err(recovery_error("journal record magic is invalid"));
        }
        let length = u64::from_le_bytes(
            header[8..16]
                .try_into()
                .map_err(|_| recovery_error("journal record length is invalid"))?,
        );
        if length > file_bytes - consumed {
            break;
        }
        let _payload_memory = context.reserve_memory(length).map_err(CliError::from)?;
        let mut payload = HashingJournalReader {
            inner: (&mut reader).take(length),
            digest: Sha256::new(),
            read: 0,
        };
        let mut decoder = serde_json::Deserializer::from_reader(&mut payload);
        let envelope_result = OwnedJournalRecordEnvelope::deserialize(&mut decoder);
        let finished = decoder.end();
        drop(decoder);
        io::copy(&mut payload, &mut io::sink())?;
        context.checkpoint().map_err(CliError::from)?;
        if payload.read != length {
            break;
        }
        consumed = consumed
            .checked_add(length)
            .ok_or_else(|| recovery_error("journal record offset overflowed"))?;
        let actual_digest: [u8; 32] = payload.digest.finalize().into();
        if actual_digest.as_slice() != &header[16..header_bytes] {
            return Err(recovery_error("journal record digest is invalid"));
        }
        let envelope =
            envelope_result.map_err(|_| recovery_error("journal record payload is malformed"))?;
        finished.map_err(|_| recovery_error("journal record payload is malformed"))?;
        if envelope.sequence != expected_sequence {
            return Err(recovery_error("journal record sequence is not contiguous"));
        }
        if envelope.sequence > journal.log_sequence {
            apply_journal_record(journal, envelope.record, &mut parent_index)?;
            journal.update_memory_charge()?;
            journal.log_sequence = envelope.sequence;
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| recovery_error("journal record sequence overflowed"))?;
    }
    journal.temporary[1] = Some(temporary);
    Ok(())
}

pub(super) struct HashingJournalReader<R> {
    inner: R,
    digest: Sha256,
    read: u64,
}

impl<R: Read> Read for HashingJournalReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(bytes)?;
        self.digest.update(&bytes[..count]);
        self.read = self
            .read
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("journal record length overflowed"))?;
        Ok(count)
    }
}

fn read_complete_or_tail(reader: &mut impl Read, bytes: &mut [u8]) -> io::Result<bool> {
    let mut read = 0;
    while read < bytes.len() {
        let count = reader.read(&mut bytes[read..])?;
        if count == 0 {
            return Ok(false);
        }
        read += count;
    }
    Ok(true)
}

pub(super) fn apply_journal_record(
    journal: &mut Journal,
    record: JournalRecord,
    parent_index: &mut HashSet<FileIdentity>,
) -> Result<(), CliError> {
    if journal.phase != JournalPhase::Staging {
        return Err(recovery_error("journal record follows a non-staging checkpoint"));
    }
    match record {
        JournalRecord::DirectoryIntent { paths } => {
            if !journal.pending_directories.is_empty() {
                return Err(recovery_error("journal contains overlapping directory intents"));
            }
            journal.pending_directories = paths;
        }
        JournalRecord::DirectoriesCreated { directories } => {
            if journal.pending_directories.len() != directories.len()
                || !journal
                    .pending_directories
                    .iter()
                    .zip(&directories)
                    .all(|(pending, created)| pending == &created.path)
            {
                return Err(recovery_error("created directories do not match their intent"));
            }
            journal.created_directories.extend(directories);
            journal.pending_directories.clear();
        }
        JournalRecord::TargetAdded { parent, entry } => {
            if journal.entries.len() >= MAX_JOURNAL_ENTRIES {
                return Err(recovery_error("journal record target count exceeds its limit"));
            }
            if let Some(parent) = parent {
                if !parent_index.insert(parent.clone()) {
                    return Err(recovery_error("journal repeats a physical parent identity"));
                }
                journal.parent_identities.push(parent);
            }
            journal.entries.push(entry);
        }
        JournalRecord::StageSealed { index, size, content_sha256, staged_identity } => {
            let entry = journal
                .entries
                .get_mut(index)
                .ok_or_else(|| recovery_error("sealed stage has no journal target"))?;
            if entry.staged_identity.is_some() || !entry.content_sha256.is_empty() {
                return Err(recovery_error("journal stage was sealed more than once"));
            }
            entry.size = size;
            entry.content_sha256 = content_sha256;
            entry.staged_identity = Some(staged_identity);
        }
    }
    Ok(())
}
