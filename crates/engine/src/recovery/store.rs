//! Capability-bound checkpoint storage and bounded envelope decoding.

use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
#[cfg(unix)]
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::File;
#[cfg(unix)]
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
#[cfg(unix)]
use std::path::{Component, Path};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(all(test, unix))]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const TOKEN_BYTES: usize = 16;
#[cfg(unix)]
const MAX_CHECKPOINT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
#[cfg(unix)]
const HEADER_BLOCK_BYTES: usize = 4 * 1024;
#[cfg(unix)]
// A maximally nested Table node adds node/block/rows/row/cells/cell/blocks
// containers around the next public IR level. The fixed allowance covers the
// checkpoint envelope, document metadata, and enum representation.
pub(super) const MAX_CHECKPOINT_JSON_DEPTH: usize = into_markdown_core::MAX_DOCUMENT_DEPTH * 8 + 32;
#[cfg(unix)]
const MAX_JSON_CONTAINER_ENTRIES: u64 = 1_100_000;
#[cfg(unix)]
const MAX_JSON_VALUES: u64 = 4_000_000;
#[cfg(unix)]
const JSON_VALUE_ALLOCATION_BYTES: u64 = 32;
#[cfg(unix)]
const MAGIC: &[u8] = b"into-markdown-checkpoint-v1\n";

/// Opaque, filesystem-safe identifier for one recoverable task.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecoveryToken(String);

impl RecoveryToken {
    /// Parse a token previously returned by [`RecoveryStore::create_token`].
    ///
    /// # Errors
    ///
    /// Rejects non-canonical tokens before any filesystem access.
    pub fn parse(value: impl Into<String>) -> Result<Self, ConversionError> {
        let value = value.into();
        if value.len() != TOKEN_BYTES * 2
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(recovery_error("invalidToken", "recovery token is not canonical"));
        }
        Ok(Self(value))
    }

    /// Borrow the stable token text used by Web APIs and persistent storage.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Durable states visible to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskPhase {
    /// Conversion produced validated IR and may resume at rendering.
    Converted,
    /// The complete result was atomically committed.
    Succeeded,
}

#[cfg(unix)]
impl TaskPhase {
    fn file_label(self) -> &'static str {
        match self {
            Self::Converted => "converted",
            Self::Succeeded => "succeeded",
        }
    }

    fn stages(self) -> &'static [&'static str] {
        match self {
            Self::Converted => &["converted"],
            Self::Succeeded => &["converted", "rendered", "succeeded"],
        }
    }
}

/// Versioned, payload-free task metadata suitable for a Web status endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskCheckpoint {
    /// Recovery protocol schema version.
    pub schema_version: u32,
    /// Opaque task token.
    pub token: String,
    /// SHA-256 fingerprint of resolved input bytes and trusted metadata.
    pub input_fingerprint: String,
    /// SHA-256 fingerprint of the format hint and conversion options.
    pub options_fingerprint: String,
    /// Latest atomically committed task phase.
    pub phase: TaskPhase,
    /// Ordered phase names committed through `phase`.
    pub completed_stages: Vec<String>,
    /// Exact serialized payload bytes.
    pub payload_bytes: u64,
    /// SHA-256 of the serialized payload.
    pub payload_sha256: String,
}

#[cfg(unix)]
impl TaskCheckpoint {
    fn validate(
        &self,
        token: &RecoveryToken,
        phase: TaskPhase,
        measured_bytes: u64,
        measured_sha256: Option<&str>,
    ) -> Result<(), ConversionError> {
        if self.schema_version != CHECKPOINT_SCHEMA_VERSION {
            return Err(recovery_error(
                "unsupportedVersion",
                format!("checkpoint schema {} is not supported", self.schema_version),
            ));
        }
        if self.token != token.as_str() {
            return Err(recovery_error("corrupt", "checkpoint token does not match its name"));
        }
        if self.phase != phase
            || self.completed_stages.len() != phase.stages().len()
            || !self
                .completed_stages
                .iter()
                .map(String::as_str)
                .zip(phase.stages())
                .all(|(actual, expected)| actual == *expected)
        {
            return Err(recovery_error("corrupt", "checkpoint stage history is inconsistent"));
        }
        if !canonical_sha256(&self.input_fingerprint)
            || !canonical_sha256(&self.options_fingerprint)
        {
            return Err(recovery_error("corrupt", "checkpoint fingerprint is not canonical"));
        }
        if self.payload_bytes != measured_bytes
            || !canonical_sha256(&self.payload_sha256)
            || measured_sha256.is_some_and(|digest| self.payload_sha256 != digest)
        {
            return Err(recovery_error("corrupt", "checkpoint payload digest does not match"));
        }
        Ok(())
    }
}

/// Directory-backed checkpoint store. It performs no network access.
#[derive(Debug, Clone)]
pub struct RecoveryStore {
    #[cfg(unix)]
    directory: Arc<SafeDirectory>,
    #[cfg(all(test, unix))]
    quarantine_failure: Arc<AtomicUsize>,
}

/// Checkpoint files atomically quarantined for a task-history deletion.
#[derive(Debug)]
pub struct RecoveryPurge {
    token: RecoveryToken,
}

/// Keeps a task lock and decoded-payload memory charge alive.
#[derive(Debug)]
pub(crate) struct LoadedCheckpoint<T> {
    pub metadata: TaskCheckpoint,
    pub payload: T,
    pub memory: ResourceReservation,
}

impl RecoveryStore {
    /// Open or create a private local checkpoint directory through no-follow
    /// handles.
    ///
    /// # Errors
    ///
    /// On Unix, the final store root must be owned by the current effective
    /// user and grant no group or other permissions. Existing directories are
    /// rejected rather than modified. Ancestor directories may be shared.
    /// Returns a stable recovery error if the root or any path component is
    /// unsafe. Builds without audited relative-directory operations fail
    /// closed.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ConversionError> {
        #[cfg(unix)]
        {
            Ok(Self {
                directory: Arc::new(SafeDirectory::open_or_create(root.into())?),
                #[cfg(test)]
                quarantine_failure: Arc::new(AtomicUsize::new(0)),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            Err(ConversionError::ComponentUnavailable {
                component: "recovery-store".into(),
                detail: "capability-bound checkpoint operations are unavailable".into(),
            })
        }
    }

    /// Generate a cryptographically random task token.
    ///
    /// # Errors
    ///
    /// Returns a recovery error when the operating system RNG is unavailable.
    pub fn create_token(&self) -> Result<RecoveryToken, ConversionError> {
        #[cfg(unix)]
        self.directory.verify_namespace()?;
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|error| {
            recovery_error("entropy", format!("generate recovery token: {error}"))
        })?;
        let token = RecoveryToken(hex(&bytes));
        #[cfg(unix)]
        self.directory.verify_namespace()?;
        Ok(token)
    }

    /// Inspect committed metadata with one fixed-size footer read.
    ///
    /// The payload is neither read, allocated, nor deserialized by this
    /// status-oriented API. Full recovery authenticates the payload digest.
    ///
    /// # Errors
    ///
    /// Returns a stable recovery error for malformed, unsupported, or swapped
    /// metadata state.
    pub fn inspect(
        &self,
        token: &RecoveryToken,
    ) -> Result<Option<TaskCheckpoint>, ConversionError> {
        #[cfg(unix)]
        {
            self.directory.verify_namespace()?;
            for phase in [TaskPhase::Succeeded, TaskPhase::Converted] {
                if let Some(file) = self.directory.open_regular(&phase_name(token, phase))? {
                    let metadata = inspect_file(file, token, phase)?;
                    self.directory.verify_namespace()?;
                    return Ok(Some(metadata));
                }
            }
            self.directory.verify_namespace()?;
            Ok(None)
        }
        #[cfg(not(unix))]
        {
            let _ = token;
            Err(platform_unavailable())
        }
    }

    /// Verify that every recovery file for a token is safe to remove.
    ///
    /// Retention callers use this before committing their task-store deletion,
    /// so an unsafe checkpoint cannot turn a reversible object quarantine into
    /// a partially committed deletion.
    pub fn verify_purge(&self, token: &RecoveryToken) -> Result<(), ConversionError> {
        #[cfg(unix)]
        {
            self.directory.verify_namespace()?;
            for name in [
                phase_name(token, TaskPhase::Succeeded),
                phase_name(token, TaskPhase::Converted),
                lock_name(token),
            ] {
                if let Some(file) = self.directory.open_regular(&name)? {
                    let stat = rustix::fs::fstat(&file)
                        .map_err(|error| recovery_io("inspect checkpoint for deletion", error))?;
                    if stat.st_nlink != 1 {
                        return Err(recovery_error(
                            "unsafePath",
                            "checkpoint selected for deletion has an external hard link",
                        ));
                    }
                }
            }
            self.directory.verify_namespace()
        }
        #[cfg(not(unix))]
        {
            let _ = token;
            Err(platform_unavailable())
        }
    }

    /// Atomically move every checkpoint name out of the live token namespace.
    /// The returned intent must be restored if the coordinating database
    /// transaction fails, or finished after it commits.
    pub fn quarantine_purge(
        &self,
        token: &RecoveryToken,
    ) -> Result<RecoveryPurge, ConversionError> {
        #[cfg(unix)]
        {
            self.verify_purge(token)?;
            let mut moved: Vec<(String, String)> = Vec::new();
            for source in purge_names(token) {
                if self.directory.open_regular(&source)?.is_none() {
                    continue;
                }
                let target = purge_quarantine_name(&source);
                if let Err(error) = self.directory.rename_no_replace(&source, &target) {
                    for (source, target) in moved.into_iter().rev() {
                        let _ = self.directory.rename_no_replace(&target, &source);
                    }
                    return Err(error);
                }
                moved.push((source, target));
            }
            let post_move = {
                #[cfg(test)]
                match self.quarantine_failure.swap(0, Ordering::SeqCst) {
                    1 => Err(recovery_error("io", "injected quarantine sync failure")),
                    2 => Err(recovery_error("unsafePath", "injected quarantine verify failure")),
                    _ => self.directory.sync().and_then(|()| self.verify_quarantined_purge(token)),
                }
                #[cfg(not(test))]
                self.directory.sync().and_then(|()| self.verify_quarantined_purge(token))
            };
            if let Err(error) = post_move {
                let mut rollback_error = None;
                for (source, target) in moved.iter().rev() {
                    if let Err(restore) = self.directory.rename_no_replace(target, source) {
                        rollback_error.get_or_insert(restore);
                    }
                }
                if let Err(sync) = self.directory.sync() {
                    rollback_error.get_or_insert(sync);
                }
                if let Err(verify) = self.verify_purge(token) {
                    rollback_error.get_or_insert(verify);
                }
                return Err(rollback_error.unwrap_or(error));
            }
            Ok(RecoveryPurge { token: token.clone() })
        }
        #[cfg(not(unix))]
        {
            let _ = token;
            Err(platform_unavailable())
        }
    }

    /// Restore a pre-commit checkpoint quarantine.
    pub fn restore_purge(&self, purge: RecoveryPurge) -> Result<(), ConversionError> {
        self.restore_quarantined_purge(&purge.token)
    }

    /// Permanently remove a post-commit checkpoint quarantine.
    pub fn finish_purge(&self, purge: RecoveryPurge) -> Result<(), ConversionError> {
        self.remove_quarantined_purge(&purge.token)
    }

    /// Restore checkpoint quarantine left by a crash before the DB commit.
    pub fn restore_quarantined_purge(&self, token: &RecoveryToken) -> Result<(), ConversionError> {
        #[cfg(unix)]
        {
            self.verify_quarantined_purge(token)?;
            for source in purge_names(token) {
                let target = purge_quarantine_name(&source);
                if self.directory.open_regular(&target)?.is_some() {
                    self.directory.rename_no_replace(&target, &source)?;
                }
            }
            self.directory.sync()?;
            self.directory.verify_namespace()
        }
        #[cfg(not(unix))]
        {
            let _ = token;
            Err(platform_unavailable())
        }
    }

    /// Remove checkpoint quarantine left by a crash after the DB commit.
    pub fn remove_quarantined_purge(&self, token: &RecoveryToken) -> Result<(), ConversionError> {
        #[cfg(unix)]
        {
            self.verify_quarantined_purge(token)?;
            for source in purge_names(token) {
                self.directory.unlink(&purge_quarantine_name(&source))?;
            }
            self.directory.sync()?;
            self.directory.verify_namespace()
        }
        #[cfg(not(unix))]
        {
            let _ = token;
            Err(platform_unavailable())
        }
    }

    #[cfg(unix)]
    fn verify_quarantined_purge(&self, token: &RecoveryToken) -> Result<(), ConversionError> {
        self.directory.verify_namespace()?;
        for source in purge_names(token) {
            if let Some(file) = self.directory.open_regular(&purge_quarantine_name(&source))? {
                let stat = rustix::fs::fstat(&file)
                    .map_err(|error| recovery_io("inspect quarantined checkpoint", error))?;
                if stat.st_nlink != 1 {
                    return Err(recovery_error(
                        "unsafePath",
                        "quarantined checkpoint has an external hard link",
                    ));
                }
            }
        }
        self.directory.verify_namespace()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn test_fail_quarantine_after_move(&self, phase: usize) {
        self.quarantine_failure.store(phase, Ordering::SeqCst);
    }

    /// Permanently remove every checkpoint and lock owned by one canonical
    /// token. This is intended for retention after a task reached a terminal
    /// state; it performs no path construction from caller-controlled text.
    pub fn purge(&self, token: &RecoveryToken) -> Result<(), ConversionError> {
        #[cfg(unix)]
        {
            self.verify_purge(token)?;
            for name in [
                phase_name(token, TaskPhase::Succeeded),
                phase_name(token, TaskPhase::Converted),
                lock_name(token),
            ] {
                if let Some(file) = self.directory.open_regular(&name)? {
                    let stat = rustix::fs::fstat(&file)
                        .map_err(|error| recovery_io("inspect checkpoint for deletion", error))?;
                    if stat.st_nlink != 1 {
                        return Err(recovery_error(
                            "unsafePath",
                            "checkpoint selected for deletion has an external hard link",
                        ));
                    }
                    drop(file);
                    self.directory.unlink(&name)?;
                }
            }
            self.directory.sync()?;
            self.directory.verify_namespace()
        }
        #[cfg(not(unix))]
        {
            let _ = token;
            Err(platform_unavailable())
        }
    }

    pub(crate) fn lock(
        &self,
        token: &RecoveryToken,
        context: &ExecutionContext,
    ) -> Result<TaskLock, ConversionError> {
        #[cfg(unix)]
        {
            self.directory.verify_namespace()?;
            let file = self.directory.open_lock(&lock_name(token))?;
            loop {
                match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                {
                    Ok(()) => break,
                    Err(error)
                        if error == rustix::io::Errno::AGAIN
                            || error == rustix::io::Errno::WOULDBLOCK =>
                    {
                        context.checkpoint()?;
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => return Err(recovery_io("lock recovery task", error)),
                }
            }
            self.directory.verify_namespace()?;
            Ok(TaskLock { _file: file })
        }
        #[cfg(not(unix))]
        {
            let _ = (token, context);
            Err(platform_unavailable())
        }
    }

    pub(crate) fn load<T: DeserializeOwned>(
        &self,
        token: &RecoveryToken,
        context: &ExecutionContext,
    ) -> Result<Option<LoadedCheckpoint<T>>, ConversionError> {
        #[cfg(unix)]
        {
            self.directory.verify_namespace()?;
            for phase in [TaskPhase::Succeeded, TaskPhase::Converted] {
                let Some(mut file) = self.directory.open_regular(&phase_name(token, phase))? else {
                    continue;
                };
                let size = file
                    .metadata()
                    .map_err(|error| recovery_io("inspect checkpoint", error))?
                    .len();
                if size > MAX_CHECKPOINT_BYTES {
                    return Err(recovery_error("limit", "checkpoint exceeds the 2 GiB limit"));
                }
                let mut memory = context.reserve_memory(size)?;
                let capacity = usize::try_from(size).map_err(|_| {
                    recovery_error("limit", "checkpoint size cannot be represented in memory")
                })?;
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(capacity).map_err(|_| ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "checkpoint buffer allocation failed".into(),
                })?;
                bytes.resize(capacity, 0);
                file.read_exact(&mut bytes)
                    .map_err(|error| recovery_io("read checkpoint", error))?;
                let mut trailing = [0_u8; 1];
                if file
                    .read(&mut trailing)
                    .map_err(|error| recovery_io("finish checkpoint read", error))?
                    != 0
                {
                    return Err(recovery_error("corrupt", "checkpoint changed while reading"));
                }
                let (payload, metadata) = split_envelope(&bytes, token, phase, Some(context))?;
                let stats = preflight_json(payload, context)?;
                let structural_bytes =
                    stats.values.checked_mul(JSON_VALUE_ALLOCATION_BYTES).ok_or_else(|| {
                        recovery_error("limit", "checkpoint allocation estimate overflow")
                    })?;
                memory.grow(structural_bytes)?;
                memory.grow(stats.string_bytes)?;
                let mut decoder = serde_json::Deserializer::from_slice(payload);
                decoder.disable_recursion_limit();
                let payload = T::deserialize(&mut decoder).map_err(|error| {
                    recovery_error("corrupt", format!("decode checkpoint payload: {error}"))
                })?;
                decoder.end().map_err(|error| {
                    recovery_error("corrupt", format!("finish checkpoint payload: {error}"))
                })?;
                self.directory.verify_namespace()?;
                return Ok(Some(LoadedCheckpoint { metadata, payload, memory }));
            }
            self.directory.verify_namespace()?;
            Ok(None)
        }
        #[cfg(not(unix))]
        {
            let _ = (token, context);
            Err(platform_unavailable())
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit<T: Serialize>(
        &self,
        token: &RecoveryToken,
        context: &ExecutionContext,
        input_fingerprint: &str,
        options_fingerprint: &str,
        phase: TaskPhase,
        payload: &T,
    ) -> Result<(), ConversionError> {
        #[cfg(unix)]
        {
            self.directory.verify_namespace()?;
            if self.directory.open_regular(&phase_name(token, phase))?.is_some() {
                return Err(recovery_error(
                    "conflict",
                    "a checkpoint already occupies this task phase",
                ));
            }
            let mut nonce = [0_u8; 8];
            getrandom::fill(&mut nonce).map_err(|error| {
                recovery_error("entropy", format!("name checkpoint temporary file: {error}"))
            })?;
            let temporary = format!(".{}.{}.tmp", token.as_str(), hex(&nonce));
            let target = phase_name(token, phase);
            let result = (|| {
                let file = self.directory.create_regular(&temporary)?;
                let reservation = context.reserve_temporary(0)?;
                let mut writer = BudgetWriter::new(file, reservation);
                writer.write_all(MAGIC).map_err(|_| writer.error())?;
                let (payload_bytes, payload_sha256) = {
                    let mut hashing = HashingWriter::new(&mut writer);
                    if let Err(error) = serde_json::to_writer(&mut hashing, payload) {
                        return Err(hashing.error(&error));
                    }
                    hashing.finish()
                };
                let metadata = TaskCheckpoint {
                    schema_version: CHECKPOINT_SCHEMA_VERSION,
                    token: token.as_str().into(),
                    input_fingerprint: input_fingerprint.into(),
                    options_fingerprint: options_fingerprint.into(),
                    phase,
                    completed_stages: phase.stages().iter().map(ToString::to_string).collect(),
                    payload_bytes,
                    payload_sha256,
                };
                let mut header = serde_json::to_vec(&metadata).map_err(|error| {
                    recovery_error("internal", format!("encode checkpoint metadata: {error}"))
                })?;
                if header.len() >= HEADER_BLOCK_BYTES {
                    return Err(recovery_error("limit", "checkpoint metadata is oversized"));
                }
                header.push(b'\n');
                header.resize(HEADER_BLOCK_BYTES, b' ');
                writer.write_all(&header).map_err(|_| writer.error())?;
                writer.flush().map_err(|_| writer.error())?;
                writer.sync_all()?;
                drop(writer);
                self.directory.verify_namespace()?;
                self.directory.link_no_replace(&temporary, &target)?;
                self.directory.sync()?;
                self.directory.unlink(&temporary)?;
                self.directory.sync()?;
                self.directory.verify_namespace()
            })();
            if result.is_err() {
                let _ = self.directory.unlink(&temporary);
            }
            result
        }
        #[cfg(not(unix))]
        {
            let _ = (token, context, input_fingerprint, options_fingerprint, phase, payload);
            Err(platform_unavailable())
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn test_path(&self, token: &RecoveryToken, phase: TaskPhase) -> PathBuf {
        self.directory.path.join(phase_name(token, phase))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn test_payload(&self, token: &RecoveryToken, phase: TaskPhase) -> Vec<u8> {
        let bytes = std::fs::read(self.test_path(token, phase)).unwrap();
        split_envelope(&bytes, token, phase, None).unwrap().0.to_vec()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn test_replace_envelope(
        &self,
        token: &RecoveryToken,
        phase: TaskPhase,
        schema_version: u32,
        input_fingerprint: &str,
        options_fingerprint: &str,
        payload: &[u8],
    ) {
        let metadata = TaskCheckpoint {
            schema_version,
            token: token.as_str().into(),
            input_fingerprint: input_fingerprint.into(),
            options_fingerprint: options_fingerprint.into(),
            phase,
            completed_stages: phase.stages().iter().map(ToString::to_string).collect(),
            payload_bytes: u64::try_from(payload.len()).unwrap(),
            payload_sha256: hex(&Sha256::digest(payload)),
        };
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(payload);
        let mut header = serde_json::to_vec(&metadata).unwrap();
        assert!(header.len() < HEADER_BLOCK_BYTES);
        header.push(b'\n');
        header.resize(HEADER_BLOCK_BYTES, b' ');
        bytes.extend_from_slice(&header);
        std::fs::write(self.test_path(token, phase), bytes).unwrap();
    }
}

/// Exclusive advisory token lock. Unlocks when dropped.
pub(crate) struct TaskLock {
    _file: File,
}

#[cfg(unix)]
fn inspect_file(
    mut file: File,
    token: &RecoveryToken,
    phase: TaskPhase,
) -> Result<TaskCheckpoint, ConversionError> {
    let size = file.metadata().map_err(|error| recovery_io("inspect checkpoint", error))?.len();
    let minimum = u64::try_from(MAGIC.len() + HEADER_BLOCK_BYTES).unwrap_or(u64::MAX);
    if size > MAX_CHECKPOINT_BYTES {
        return Err(recovery_error("limit", "checkpoint exceeds the 2 GiB limit"));
    }
    if size < minimum {
        return Err(recovery_error("corrupt", "checkpoint envelope is truncated"));
    }
    let mut magic = [0_u8; MAGIC.len()];
    file.read_exact(&mut magic).map_err(|error| recovery_io("read checkpoint signature", error))?;
    if magic != MAGIC {
        return Err(recovery_error("corrupt", "checkpoint signature is invalid"));
    }
    file.seek(SeekFrom::End(-i64::try_from(HEADER_BLOCK_BYTES).unwrap_or(i64::MAX)))
        .map_err(|error| recovery_io("seek checkpoint metadata", error))?;
    let mut header = [0_u8; HEADER_BLOCK_BYTES];
    file.read_exact(&mut header).map_err(|error| recovery_io("read checkpoint metadata", error))?;
    let header = fixed_header_json(&header)?;
    let metadata: TaskCheckpoint = serde_json::from_slice(header).map_err(|error| {
        recovery_error("corrupt", format!("decode checkpoint metadata: {error}"))
    })?;
    metadata.validate(token, phase, size - minimum, None)?;
    Ok(metadata)
}

#[cfg(unix)]
fn split_envelope<'a>(
    bytes: &'a [u8],
    token: &RecoveryToken,
    phase: TaskPhase,
    context: Option<&ExecutionContext>,
) -> Result<(&'a [u8], TaskCheckpoint), ConversionError> {
    if !bytes.starts_with(MAGIC) || bytes.len() < MAGIC.len() + HEADER_BLOCK_BYTES {
        return Err(recovery_error("corrupt", "checkpoint envelope is invalid"));
    }
    let footer = bytes.len() - HEADER_BLOCK_BYTES;
    let payload = &bytes[MAGIC.len()..footer];
    let header = fixed_header_json(&bytes[footer..])?;
    let metadata: TaskCheckpoint = serde_json::from_slice(header).map_err(|error| {
        recovery_error("corrupt", format!("decode checkpoint metadata: {error}"))
    })?;
    let mut hash = Sha256::new();
    for chunk in payload.chunks(64 * 1024) {
        if let Some(context) = context {
            context.checkpoint()?;
        }
        hash.update(chunk);
    }
    let digest = hex(&hash.finalize());
    metadata.validate(
        token,
        phase,
        u64::try_from(payload.len()).unwrap_or(u64::MAX),
        Some(&digest),
    )?;
    Ok((payload, metadata))
}

#[cfg(unix)]
fn fixed_header_json(header: &[u8]) -> Result<&[u8], ConversionError> {
    let Some(end) = header.iter().position(|byte| *byte == b'\n') else {
        return Err(recovery_error("corrupt", "checkpoint metadata is unterminated"));
    };
    if end == 0 || header[end + 1..].iter().any(|byte| *byte != b' ') {
        return Err(recovery_error("corrupt", "checkpoint metadata padding is invalid"));
    }
    Ok(&header[..end])
}

#[cfg(unix)]
struct JsonStats {
    values: u64,
    string_bytes: u64,
}

#[cfg(unix)]
fn preflight_json(bytes: &[u8], context: &ExecutionContext) -> Result<JsonStats, ConversionError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Container {
        Array,
        Object,
    }

    let mut stack: Vec<(Container, u64)> = Vec::new();
    let mut values = 0_u64;
    let mut in_string = false;
    let mut escaped = false;
    let mut primitive = false;
    let mut string_bytes = 0_u64;
    let mut next_checkpoint = 0_usize;
    for (offset, byte) in bytes.iter().enumerate() {
        if offset >= next_checkpoint {
            context.checkpoint()?;
            next_checkpoint = offset.saturating_add(64 * 1024);
        }
        if in_string {
            string_bytes = string_bytes
                .checked_add(1)
                .ok_or_else(|| recovery_error("limit", "checkpoint string size overflowed"))?;
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => {
                primitive = false;
                in_string = true;
                values = checked_value(values)?;
            }
            b'[' | b'{' => {
                primitive = false;
                values = checked_value(values)?;
                if stack.len() >= MAX_CHECKPOINT_JSON_DEPTH {
                    return Err(recovery_error("limit", "checkpoint JSON nesting is too deep"));
                }
                stack.push((if *byte == b'[' { Container::Array } else { Container::Object }, 1));
            }
            b']' | b'}' => {
                primitive = false;
                let expected = if *byte == b']' { Container::Array } else { Container::Object };
                let Some((actual, _)) = stack.pop() else {
                    return Err(recovery_error(
                        "corrupt",
                        "checkpoint JSON delimiters are unbalanced",
                    ));
                };
                if actual != expected {
                    return Err(recovery_error("corrupt", "checkpoint JSON delimiters mismatch"));
                }
            }
            b',' => {
                primitive = false;
                let Some((_, entries)) = stack.last_mut() else {
                    return Err(recovery_error(
                        "corrupt",
                        "checkpoint JSON comma is outside a container",
                    ));
                };
                *entries = entries.saturating_add(1);
                if *entries > MAX_JSON_CONTAINER_ENTRIES {
                    return Err(recovery_error("limit", "checkpoint JSON container is too wide"));
                }
            }
            b':' | b' ' | b'\t' | b'\r' | b'\n' => primitive = false,
            _ if !primitive => {
                primitive = true;
                values = checked_value(values)?;
            }
            _ => {}
        }
    }
    if in_string || !stack.is_empty() {
        return Err(recovery_error("corrupt", "checkpoint JSON is unterminated"));
    }
    Ok(JsonStats { values, string_bytes })
}

#[cfg(unix)]
fn checked_value(values: u64) -> Result<u64, ConversionError> {
    let values = values.saturating_add(1);
    if values > MAX_JSON_VALUES {
        return Err(recovery_error("limit", "checkpoint JSON contains too many values"));
    }
    Ok(values)
}

#[cfg(unix)]
struct BudgetWriter {
    file: File,
    reservation: ResourceReservation,
    written: u64,
    failure: Option<ConversionError>,
}

#[cfg(unix)]
impl BudgetWriter {
    fn new(file: File, reservation: ResourceReservation) -> Self {
        Self { file, reservation, written: 0, failure: None }
    }

    fn error(&self) -> ConversionError {
        self.failure.clone().unwrap_or_else(|| recovery_error("io", "write checkpoint failed"))
    }

    fn serialization_error(&self, error: &serde_json::Error) -> ConversionError {
        self.failure
            .clone()
            .unwrap_or_else(|| recovery_error("internal", format!("encode checkpoint: {error}")))
    }

    fn sync_all(&self) -> Result<(), ConversionError> {
        self.file.sync_all().map_err(|error| recovery_io("sync checkpoint", error))
    }

    fn fail(&mut self, error: ConversionError) -> io::Error {
        self.failure = Some(error.clone());
        io::Error::other(error)
    }
}

#[cfg(unix)]
impl Write for BudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let amount = u64::try_from(bytes.len()).map_err(io::Error::other)?;
        if self.written.checked_add(amount).is_none_or(|size| size > MAX_CHECKPOINT_BYTES) {
            let error = recovery_error("limit", "checkpoint write exceeds the 2 GiB limit");
            return Err(self.fail(error));
        }
        if let Err(error) = self.reservation.grow(amount) {
            return Err(self.fail(error));
        }
        match self.file.write(bytes) {
            Ok(written) => {
                let written = u64::try_from(written).map_err(io::Error::other)?;
                if written < amount {
                    self.reservation.shrink(amount - written).map_err(io::Error::other)?;
                }
                self.written += written;
                Ok(usize::try_from(written).unwrap_or(bytes.len()))
            }
            Err(error) => {
                let _ = self.reservation.shrink(amount);
                let stable = recovery_error("io", format!("write checkpoint: {error}"));
                Err(self.fail(stable))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .flush()
            .map_err(|error| self.fail(recovery_error("io", format!("flush checkpoint: {error}"))))
    }
}

#[cfg(unix)]
struct HashingWriter<'a> {
    inner: &'a mut BudgetWriter,
    hash: Sha256,
    bytes: u64,
}

#[cfg(unix)]
impl<'a> HashingWriter<'a> {
    fn new(inner: &'a mut BudgetWriter) -> Self {
        Self { inner, hash: Sha256::new(), bytes: 0 }
    }

    fn finish(self) -> (u64, String) {
        (self.bytes, hex(&self.hash.finalize()))
    }

    fn error(&self, error: &serde_json::Error) -> ConversionError {
        self.inner.serialization_error(error)
    }
}

#[cfg(unix)]
impl Write for HashingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.hash.update(&bytes[..written]);
        self.bytes = self.bytes.saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct SafeDirectory {
    fd: rustix::fd::OwnedFd,
    path: PathBuf,
    identity: (u64, u64),
}

#[cfg(unix)]
impl SafeDirectory {
    fn open_or_create(path: PathBuf) -> Result<Self, ConversionError> {
        let path = resolved_absolute(path)?;
        let mut fd = open_root()?;
        for component in path.components() {
            let Component::Normal(name) = component else { continue };
            fd = match open_directory_at(&fd, name) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => {
                    rustix::fs::mkdirat(
                        &fd,
                        name,
                        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
                    )
                    .map_err(|error| recovery_io("create checkpoint directory", error))?;
                    rustix::fs::fsync(&fd)
                        .map_err(|error| recovery_io("sync checkpoint ancestor", error))?;
                    open_directory_at(&fd, name)
                        .map_err(|error| recovery_io("open created checkpoint directory", error))?
                }
                Err(error) => return Err(recovery_io("open checkpoint directory", error)),
            };
        }
        let identity = private_directory_identity(&fd)?;
        Ok(Self { fd, path, identity })
    }

    fn open_existing(path: &Path) -> Result<Self, ConversionError> {
        let mut fd = open_root()?;
        for component in path.components() {
            let Component::Normal(name) = component else { continue };
            fd = open_directory_at(&fd, name)
                .map_err(|error| recovery_io("verify checkpoint directory", error))?;
        }
        let identity = private_directory_identity(&fd)?;
        Ok(Self { fd, path: path.to_path_buf(), identity })
    }

    fn verify_namespace(&self) -> Result<(), ConversionError> {
        let retained = private_directory_identity(&self.fd)?;
        if retained != self.identity {
            return Err(recovery_error(
                "unsafePath",
                "checkpoint directory handle identity changed after opening",
            ));
        }
        let current = Self::open_existing(&self.path)?;
        if current.identity != self.identity {
            return Err(recovery_error(
                "unsafePath",
                "checkpoint directory identity changed after opening",
            ));
        }
        Ok(())
    }

    fn open_regular(&self, name: &str) -> Result<Option<File>, ConversionError> {
        let fd = match rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(rustix::io::Errno::LOOP) => {
                return Err(recovery_error("unsafePath", "checkpoint symlink is denied"));
            }
            Err(error) => return Err(recovery_io("open checkpoint", error)),
        };
        require_regular(&fd)?;
        Ok(Some(File::from(fd)))
    }

    fn create_regular(&self, name: &str) -> Result<File, ConversionError> {
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|error| recovery_io("create checkpoint temporary file", error))?;
        Ok(File::from(fd))
    }

    fn open_lock(&self, name: &str) -> Result<File, ConversionError> {
        let mut attempts = 0_u8;
        let fd = loop {
            match rustix::fs::openat(
                &self.fd,
                name,
                rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            ) {
                Ok(fd) => break fd,
                // macOS can transiently report ENOENT when two open(O_CREAT)
                // calls race for the same previously absent leaf.
                Err(rustix::io::Errno::NOENT) if attempts < 32 => {
                    attempts += 1;
                    std::thread::yield_now();
                }
                Err(error) => return Err(recovery_io("open recovery task lock", error)),
            }
        };
        require_regular(&fd)?;
        Ok(File::from(fd))
    }

    fn link_no_replace(&self, source: &str, target: &str) -> Result<(), ConversionError> {
        rustix::fs::linkat(&self.fd, source, &self.fd, target, rustix::fs::AtFlags::empty())
            .map_err(|error| {
                if error == rustix::io::Errno::EXIST {
                    recovery_error("conflict", "another task invocation published this phase")
                } else {
                    recovery_io("publish checkpoint", error)
                }
            })
    }

    fn unlink(&self, name: &str) -> Result<(), ConversionError> {
        match rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(error) => Err(recovery_io("remove checkpoint temporary file", error)),
        }
    }

    fn rename_no_replace(&self, source: &str, target: &str) -> Result<(), ConversionError> {
        rustix::fs::renameat_with(
            &self.fd,
            source,
            &self.fd,
            target,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(|error| recovery_io("quarantine checkpoint", error))
    }

    fn sync(&self) -> Result<(), ConversionError> {
        rustix::fs::fsync(&self.fd).map_err(|error| recovery_io("sync checkpoint directory", error))
    }
}

#[cfg(unix)]
fn open_root() -> Result<rustix::fd::OwnedFd, ConversionError> {
    rustix::fs::open(
        "/",
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| recovery_io("open filesystem root", error))
}

#[cfg(unix)]
fn open_directory_at(
    parent: &rustix::fd::OwnedFd,
    name: &std::ffi::OsStr,
) -> Result<rustix::fd::OwnedFd, rustix::io::Errno> {
    rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
}

#[cfg(unix)]
fn private_directory_identity(fd: &rustix::fd::OwnedFd) -> Result<(u64, u64), ConversionError> {
    let stat = rustix::fs::fstat(fd)
        .map_err(|error| recovery_io("inspect checkpoint directory handle", error))?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
        return Err(recovery_error("unsafePath", "checkpoint root is not a directory"));
    }
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(recovery_error(
            "unsafePath",
            "checkpoint root is not owned by the current effective user",
        ));
    }
    if stat.st_mode & 0o077 != 0 {
        return Err(recovery_error(
            "unsafePath",
            "checkpoint root grants group or other permissions",
        ));
    }
    Ok((
        u64::try_from(stat.st_dev)
            .map_err(|_| recovery_error("unsafePath", "directory device ID is invalid"))?,
        stat.st_ino,
    ))
}

#[cfg(unix)]
fn require_regular(fd: &rustix::fd::OwnedFd) -> Result<(), ConversionError> {
    let stat = rustix::fs::fstat(fd)
        .map_err(|error| recovery_io("inspect checkpoint file handle", error))?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(recovery_error("unsafePath", "checkpoint is not a regular file"));
    }
    Ok(())
}

#[cfg(unix)]
fn resolved_absolute(path: PathBuf) -> Result<PathBuf, ConversionError> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|error| recovery_io("resolve checkpoint root", error))?
            .join(path)
    };
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(recovery_error(
                    "unsafePath",
                    "checkpoint root must be an absolute normalized path",
                ));
            }
        }
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&normalized)
        && metadata.file_type().is_symlink()
    {
        return Err(recovery_error("unsafePath", "checkpoint root cannot be a symlink"));
    }
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            recovery_error("unsafePath", "checkpoint root has no existing ancestor")
        })?;
        missing.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            recovery_error("unsafePath", "checkpoint root has no existing ancestor")
        })?;
    }
    let mut resolved = std::fs::canonicalize(existing)
        .map_err(|error| recovery_io("resolve checkpoint root ancestor", error))?;
    for name in missing.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

#[cfg(unix)]
fn phase_name(token: &RecoveryToken, phase: TaskPhase) -> String {
    format!("{}.{}.checkpoint", token.as_str(), phase.file_label())
}

#[cfg(unix)]
fn lock_name(token: &RecoveryToken) -> String {
    format!("{}.lock", token.as_str())
}

#[cfg(unix)]
fn purge_names(token: &RecoveryToken) -> [String; 3] {
    [
        phase_name(token, TaskPhase::Succeeded),
        phase_name(token, TaskPhase::Converted),
        lock_name(token),
    ]
}

#[cfg(unix)]
fn purge_quarantine_name(source: &str) -> String {
    format!(".{source}.retention-trash")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut output, byte| {
        let _ = write!(&mut output, "{byte:02x}");
        output
    })
}

#[cfg(unix)]
fn canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn recovery_error(reason: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Recovery { reason, detail: detail.into() }
}

#[cfg(unix)]
fn recovery_io(operation: &str, error: impl std::fmt::Display) -> ConversionError {
    recovery_error("io", format!("{operation}: {error}"))
}

#[cfg(not(unix))]
fn platform_unavailable() -> ConversionError {
    ConversionError::ComponentUnavailable {
        component: "recovery-store".into(),
        detail: "capability-bound checkpoint operations are unavailable".into(),
    }
}

#[cfg(all(test, not(unix)))]
mod platform_tests {
    use super::*;

    #[test]
    fn unsupported_platform_fails_closed_before_filesystem_access() {
        let error = RecoveryStore::open("recovery-must-not-be-created").unwrap_err();
        assert!(matches!(
            error,
            ConversionError::ComponentUnavailable { ref component, .. }
                if component == "recovery-store"
        ));
    }
}
