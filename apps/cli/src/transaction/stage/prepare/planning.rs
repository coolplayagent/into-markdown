use super::{
    BTreeMap, BTreeSet, CliError, CreatedDirectory, DIRECTORY_ENTRY_TEMPORARY_BYTES, EntryState,
    ExecutionContext, ExitClass, FILE_ENTRY_TEMPORARY_BYTES, HookDecision, JOURNAL_SIGNATURE,
    JOURNAL_VERSION, Journal, JournalEntry, JournalPhase, MAX_JOURNAL_ENTRIES, OsStr,
    PARENT_LEASE_NAME, PARENT_LEASE_TEMPORARY_BYTES, Path, PathBuf, PreparingTransactionRoot,
    REGISTRY_LOCK_DIRECTORY_NAME, REGISTRY_NAME, ResourceReservation, SafeDir,
    TRANSACTION_METADATA_TEMPORARY_BYTES, TransactionSource, absolute_lexical,
    common_existing_ancestor, encode_path, ensure_same_filesystem, fs, io, journal_retained_bytes,
    plan_missing_directories, recover_existing_target_parents, recover_root_transactions,
    recovery_error, transaction_index_limit, unpublished_hook, validate_relative_path,
};

pub(super) struct StaticRootPlan {
    pub(super) paths: Vec<PathBuf>,
    pub(super) root: PathBuf,
    pub(super) root_handle: SafeDir,
    pub(super) missing_directories: Vec<PathBuf>,
    pub(super) root_guard: PreparingTransactionRoot,
    pub(super) planning_memory: ResourceReservation,
}

pub(super) fn plan_static_root<T: TransactionSource>(
    targets: &[T],
    root_hints: &[PathBuf],
    context: &ExecutionContext,
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<StaticRootPlan, CliError> {
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
    let planning_bytes = targets
        .iter()
        .map(TransactionSource::path)
        .chain(root_hints.iter().map(PathBuf::as_path))
        .try_fold(64_u64 * 1024, |total, path| {
            let path_bytes = u64::try_from(path.as_os_str().as_encoded_bytes().len())
                .unwrap_or(u64::MAX)
                .checked_mul(16)
                .and_then(|bytes| bytes.checked_add(2 * 1024))
                .ok_or_else(|| transaction_index_limit("static path plan memory overflowed"))?;
            total
                .checked_add(path_bytes)
                .ok_or_else(|| transaction_index_limit("static path plan memory overflowed"))
        })?;
    let planning_memory = context.reserve_memory(planning_bytes).map_err(CliError::from)?;
    let paths = targets
        .iter()
        .map(|target| absolute_lexical(target.path()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut root_paths = Vec::with_capacity(paths.len().saturating_add(root_hints.len()));
    root_paths.extend(paths.iter().cloned());
    root_paths.extend(
        root_hints.iter().map(|path| absolute_lexical(path)).collect::<Result<Vec<_>, _>>()?,
    );
    let root = common_existing_ancestor(&root_paths)?;
    ensure_same_filesystem(&root, &paths)?;
    let guard = PreparingTransactionRoot::enter(&root)?;
    let _ = hook("preparingRootRegistered", usize::MAX)?;
    recover_root_transactions(&root, context)?;
    let root_handle = SafeDir::open_absolute(&root)?;
    let missing_directories = plan_missing_directories(&root, &paths)?;
    recover_existing_target_parents(&paths, context)?;
    Ok(StaticRootPlan {
        paths,
        root,
        root_handle,
        missing_directories,
        root_guard: guard,
        planning_memory,
    })
}

pub(super) struct ReservedStaticResources {
    pub(super) journal: Journal,
    pub(super) journal_memory: ResourceReservation,
    pub(super) journal_temporary: ResourceReservation,
    pub(super) temporary_reservations: Vec<ResourceReservation>,
    pub(super) parent_paths: Vec<PathBuf>,
}

pub(super) fn create_and_bind_static_directories(
    journal: &mut Journal,
    missing: &[PathBuf],
    paths: &[PathBuf],
    root: &Path,
    root_handle: &SafeDir,
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<(), CliError> {
    for planned in missing {
        SafeDir::open_or_create_absolute(planned)?;
        let relative = planned
            .strip_prefix(root)
            .map_err(|_| recovery_error("created output directory is outside transaction root"))?;
        let handle = root_handle.open_descendant(relative)?;
        journal
            .created_directories
            .push(CreatedDirectory { path: encode_path(relative)?, identity: handle.identity });
    }
    unpublished_hook(hook, "directoryCreated")?;
    let mut parent_indices = BTreeMap::new();
    let (entries, parent_identities) = (&mut journal.entries, &mut journal.parent_identities);
    for (entry, absolute) in entries.iter_mut().zip(paths) {
        let parent = absolute.parent().ok_or_else(|| recovery_error("target has no parent"))?;
        let parent_relative = parent.strip_prefix(root).map_err(|_| {
            recovery_error("target parent is outside authenticated transaction root")
        })?;
        let parent_handle = root_handle.open_descendant(parent_relative)?;
        let name = absolute.file_name().ok_or_else(|| recovery_error("target has no file name"))?;
        if parent_handle.inspect_regular(name)? != entry.original {
            return Err(CliError::new(
                ExitClass::Io,
                "outputConflict",
                format!("output target changed during preparation: {}", absolute.display()),
            ));
        }
        let next_index = parent_identities.len();
        entry.parent_index =
            Some(*parent_indices.entry(parent_handle.identity.clone()).or_insert_with(|| {
                parent_identities.push(parent_handle.identity.clone());
                next_index
            }));
    }
    journal.pending_directories.clear();
    Ok(())
}

pub(super) fn reserve_static_resources(
    entries: Vec<JournalEntry>,
    missing: &[PathBuf],
    paths: &[PathBuf],
    root: &Path,
    root_handle: &SafeDir,
    context: &ExecutionContext,
) -> Result<ReservedStaticResources, CliError> {
    let pending = missing
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .map_err(|_| recovery_error("planned directory is outside transaction root"))
                .and_then(encode_path)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let journal = Journal {
        signature: JOURNAL_SIGNATURE.into(),
        version: JOURNAL_VERSION,
        nonce: "0".repeat(32),
        root: encode_path(root)?,
        root_identity: root_handle.identity.clone(),
        parent_identities: Vec::new(),
        generation: 0,
        log_sequence: 0,
        phase: JournalPhase::Staging,
        entries,
        created_directories: Vec::new(),
        pending_directories: pending,
    };
    let parent_paths = paths
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut projected = journal.clone();
    projected.parent_identities = vec![root_handle.identity.clone(); parent_paths.len()];
    projected.created_directories = projected
        .pending_directories
        .iter()
        .cloned()
        .map(|path| CreatedDirectory { path, identity: root_handle.identity.clone() })
        .collect();
    let journal_memory =
        context.reserve_memory(journal_retained_bytes(&projected)?).map_err(CliError::from)?;
    let parent_bytes = u64::try_from(parent_paths.len())
        .unwrap_or(u64::MAX)
        .checked_mul(PARENT_LEASE_TEMPORARY_BYTES);
    let entry_bytes = u64::try_from(journal.entries.len())
        .unwrap_or(u64::MAX)
        .checked_mul(FILE_ENTRY_TEMPORARY_BYTES);
    let directory_bytes = u64::try_from(missing.len())
        .unwrap_or(u64::MAX)
        .checked_mul(DIRECTORY_ENTRY_TEMPORARY_BYTES);
    let metadata_bytes = parent_bytes
        .and_then(|bytes| bytes.checked_add(TRANSACTION_METADATA_TEMPORARY_BYTES))
        .and_then(|bytes| entry_bytes.and_then(|entries| bytes.checked_add(entries)))
        .and_then(|bytes| directory_bytes.and_then(|dirs| bytes.checked_add(dirs)))
        .ok_or_else(|| transaction_index_limit("transaction metadata budget overflowed"))?;
    let journal_temporary = context.reserve_temporary(metadata_bytes).map_err(CliError::from)?;
    let temporary_reservations = journal
        .entries
        .iter()
        .map(|entry| context.reserve_temporary(entry.size).map_err(CliError::from))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ReservedStaticResources {
        journal,
        journal_memory,
        journal_temporary,
        temporary_reservations,
        parent_paths,
    })
}

pub(super) fn plan_static_entries<T: TransactionSource>(
    targets: &[T],
    paths: &[PathBuf],
    root: &Path,
    root_handle: &SafeDir,
    overwrite: bool,
    context: &ExecutionContext,
    defer_single_stage: bool,
) -> Result<Vec<JournalEntry>, CliError> {
    let mut entries = Vec::with_capacity(targets.len());
    let mut seen = BTreeSet::new();
    let mut originals = BTreeSet::new();
    for (target, absolute) in targets.iter().zip(paths) {
        let (size, content_sha256) = target.size_and_sha256(context)?;
        let relative = absolute.strip_prefix(root).map_err(|_| {
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
        let name = absolute.file_name().ok_or_else(|| recovery_error("target has no file name"))?;
        if [REGISTRY_NAME, REGISTRY_LOCK_DIRECTORY_NAME, PARENT_LEASE_NAME]
            .iter()
            .any(|reserved| name == OsStr::new(reserved))
        {
            return Err(CliError::new(
                ExitClass::Io,
                "outputPathUnsupported",
                "output target conflicts with the transaction manager namespace",
            ));
        }
        let parent = absolute.parent().ok_or_else(|| recovery_error("target has no parent"))?;
        let original = match fs::symlink_metadata(parent) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
            Ok(_) => {
                let relative = parent.strip_prefix(root).map_err(|_| {
                    recovery_error("target parent is outside authenticated transaction root")
                })?;
                let handle = root_handle.open_descendant(relative)?;
                handle.verify_namespace()?;
                handle.inspect_regular(name)?
            }
        };
        if original.is_some() && !overwrite {
            return Err(CliError::new(
                ExitClass::Io,
                "outputConflict",
                format!("output target already exists: {}", absolute.display()),
            ));
        }
        if let Some(identity) = &original
            && !originals.insert(identity.clone())
        {
            return Err(CliError::new(
                ExitClass::Io,
                "outputConflict",
                "multiple output paths resolve to the same existing file",
            ));
        }
        entries.push(JournalEntry {
            target: encoded,
            parent_index: None,
            original,
            content_sha256,
            size,
            staged_identity: None,
            state: EntryState::Prepared,
        });
    }
    if defer_single_stage {
        let [entry] = entries.as_mut_slice() else {
            return Err(CliError::internal("deferred streaming stage requires exactly one target"));
        };
        entry.content_sha256.clear();
    }
    Ok(entries)
}
