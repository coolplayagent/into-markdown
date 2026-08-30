use super::{
    BTreeSet, CliError, Component, CreatedDirectory, DIRECTORY_ENTRY_TEMPORARY_BYTES, Digest,
    EXTERNAL_LOCK_PREFIX, EntryState, ExecutionContext, ExitClass, FILE_ENTRY_TEMPORARY_BYTES,
    File, FileIdentity, FileTarget, HashSet, HookDecision, JOURNAL_SIGNATURE, JOURNAL_VERSION,
    Journal, JournalEntry, JournalPhase, MAX_JOURNAL_ENTRIES, MAX_RECOVERY_RETRIES, MixedContent,
    MixedTarget, OsString, PARENT_LEASE_TEMPORARY_BYTES, Path, PathBuf, PreparedTransaction,
    PreparingTransactionRoot, Read, SafeDir, TRANSACTION_METADATA_TEMPORARY_BYTES, Target,
    TransactionHandles, TransactionSource, absolute_lexical, active_transactions, call_hook,
    common_existing_ancestor, crash_point, encode_path, ensure_same_filesystem,
    ensure_transaction_platform, file_identity, fs, handle_rename, io, journal_retained_bytes,
    persist_journal_handle, recover_parent_transactions, recovery_error,
    remove_initial_transaction_with_external_lock, stage_name, transaction_index_limit,
    validate_relative_path,
};
use crate::transaction::lease::{
    ParentLeaseRemovalIndex, create_parent_lease, remove_parent_lease,
};
use crate::transaction::recovery::remove_created_output_directories;

mod directories;
mod planning;
use crate::transaction::{
    BTreeMap, OsStr, PARENT_LEASE_NAME, REGISTRY_NAME, ResourceReservation,
    create_initial_transaction_in_registry, lock_registry_epoch, recover_root_transactions,
};
pub(in crate::transaction) use directories::create_missing_output_directory;
use directories::{
    IntentLease, UnpublishedPrepareCleanup, cleanup_empty_initial, create_intent_leases,
    create_parent_leases_windowed, plan_intent_parent_paths, plan_missing_directories,
    recover_existing_target_parents, remove_intent_leases, unpublished_hook,
};
use planning::{
    ReservedStaticResources, StaticRootPlan, create_and_bind_static_directories,
    plan_static_entries, plan_static_root, reserve_static_resources,
};

const PARENT_HANDLE_BATCH: usize = 256;

pub fn prepare(
    targets: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
) -> Result<PreparedTransaction, CliError> {
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        match prepare_with_hook(targets, overwrite, context, |_, _| Ok(HookDecision::Continue)) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

/// Recover manager-owned transactions, then stage seekable files in bounded chunks.
pub fn prepare_files(
    targets: &[FileTarget<'_>],
    overwrite: bool,
    context: &ExecutionContext,
) -> Result<PreparedTransaction, CliError> {
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        match prepare_sources_with_hook(targets, overwrite, context, |_, _| {
            Ok(HookDecision::Continue)
        }) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

/// Stage one seekable primary artifact and zero or more in-memory companion assets.
pub fn prepare_file_and_bytes(
    primary: &FileTarget<'_>,
    companions: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
) -> Result<PreparedTransaction, CliError> {
    let mut sources = Vec::with_capacity(companions.len() + 1);
    sources.push(MixedTarget { path: &primary.path, content: MixedContent::File(primary.file) });
    sources.extend(companions.iter().map(|target| MixedTarget {
        path: &target.path,
        content: MixedContent::Bytes(target.bytes),
    }));
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        match prepare_sources_with_hook(&sources, overwrite, context, |_, _| {
            Ok(HookDecision::Continue)
        }) {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

/// Recover any interrupted transaction owning one of these physical parent
/// directories before higher-level conflict planning observes the filesystem.
pub fn recover_for_paths(paths: &[PathBuf], context: &ExecutionContext) -> Result<(), CliError> {
    ensure_transaction_platform()?;
    let paths = paths.iter().map(|path| absolute_lexical(path)).collect::<Result<Vec<_>, _>>()?;
    if paths.is_empty() {
        return Ok(());
    }
    let root = common_existing_ancestor(&paths)?;
    let _preparing_root = PreparingTransactionRoot::enter(&root)?;
    for recovered in 0..=MAX_RECOVERY_RETRIES {
        context.checkpoint().map_err(CliError::from)?;
        match recover_root_transactions(&root, context)
            .and_then(|()| recover_existing_target_parents(&paths, context))
        {
            Err(error) if error.code() == "transactionRecoveredRetry" => {
                if recovered == MAX_RECOVERY_RETRIES {
                    return Err(CliError::new(
                        ExitClass::Io,
                        "recoveryLimit",
                        "output transaction recovery retry limit exceeded",
                    ));
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded recovery loop always returns")
}

pub(crate) fn prepare_with_hook(
    targets: &[Target<'_>],
    overwrite: bool,
    context: &ExecutionContext,
    mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<PreparedTransaction, CliError> {
    prepare_sources_with_hook(targets, overwrite, context, hook)
}

pub(in crate::transaction) fn prepare_sources_with_hook<T: TransactionSource>(
    targets: &[T],
    overwrite: bool,
    context: &ExecutionContext,
    mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<PreparedTransaction, CliError> {
    prepare_sources_with_hook_internal(targets, overwrite, context, hook, false, &[])
        .map(|(transaction, _)| transaction)
}

pub(in crate::transaction) fn prepare_sources_with_hook_internal<T: TransactionSource>(
    targets: &[T],
    overwrite: bool,
    context: &ExecutionContext,
    mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    defer_single_stage: bool,
    root_hints: &[PathBuf],
) -> Result<(PreparedTransaction, Option<File>), CliError> {
    ensure_transaction_platform()?;
    #[cfg(not(any(unix, windows)))]
    return Err(transaction_platform_unavailable());
    #[cfg(any(unix, windows))]
    {
        let plan = plan_static_root(targets, root_hints, context, &mut hook)?;
        let entries = plan_static_entries(
            targets,
            &plan.paths,
            &plan.root,
            &plan.root_handle,
            overwrite,
            context,
            defer_single_stage,
        )?;
        let resources = reserve_static_resources(
            entries,
            &plan.missing_directories,
            &plan.paths,
            &plan.root,
            &plan.root_handle,
            plan.intent_parent_paths.len(),
            context,
        )?;
        finish_static_transaction(targets, context, defer_single_stage, &mut hook, plan, resources)
    }
}

#[cfg(any(unix, windows))]
struct StaticIntentState {
    plan: StaticRootPlan,
    resources: ReservedStaticResources,
    initial: InitialStaticTransaction,
    intent_leases: Vec<IntentLease>,
}

#[cfg(any(unix, windows))]
struct StaticBindInputs<'a> {
    journal: &'a mut Journal,
    journal_temporary: &'a mut ResourceReservation,
    journal_slot_bytes: &'a mut [u64; 2],
    paths: &'a [PathBuf],
    missing_directories: &'a [PathBuf],
    parent_paths: &'a [PathBuf],
    root: &'a Path,
    root_handle: &'a SafeDir,
    initial_handle: &'a SafeDir,
    intent_leases: &'a [IntentLease],
}

#[cfg(any(unix, windows))]
struct StaticBindError {
    error: CliError,
    intent_leases_active: bool,
}

#[cfg(any(unix, windows))]
fn bind_static_resources(inputs: &mut StaticBindInputs<'_>) -> Result<(), StaticBindError> {
    let active = |error| StaticBindError { error, intent_leases_active: true };
    create_and_bind_static_directories(
        inputs.journal,
        inputs.missing_directories,
        inputs.paths,
        inputs.root,
        inputs.root_handle,
    )
    .map_err(active)?;
    let intent_parents =
        inputs.intent_leases.iter().map(|lease| lease.identity.clone()).collect::<BTreeSet<_>>();
    create_parent_leases_windowed(
        inputs.parent_paths,
        &intent_parents,
        inputs.initial_handle,
        inputs.journal,
    )
    .map_err(active)?;
    persist_journal_handle(
        inputs.initial_handle,
        inputs.journal,
        inputs.journal_temporary,
        inputs.journal_slot_bytes,
    )
    .map_err(active)?;
    let final_parents = inputs.journal.parent_identities.iter().cloned().collect::<HashSet<_>>();
    remove_intent_leases(
        inputs.intent_leases,
        inputs.initial_handle,
        inputs.journal,
        &final_parents,
    )
    .map(|_| ())
    .map_err(|error| StaticBindError { error, intent_leases_active: false })
}

#[cfg(any(unix, windows))]
fn acquire_static_intent(
    mut plan: StaticRootPlan,
    mut resources: ReservedStaticResources,
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<StaticIntentState, CliError> {
    let initial = initialize_static_transaction(
        &plan.root,
        &plan.root_handle,
        &mut resources.journal,
        &mut resources.journal_temporary,
    )?;
    let cleanup = UnpublishedPrepareCleanup {
        root: &plan.root,
        registry: &initial.registry_handle,
        initial_directory: &initial.initial_directory,
    };
    let intent_leases = match create_intent_leases(
        &plan.intent_parent_paths,
        &initial.initial_handle,
        &mut resources.journal,
    ) {
        Ok(leases) => leases,
        Err(error) => {
            return Err(cleanup.finish(
                error,
                initial.initial_handle,
                &resources.journal,
                &[],
                initial.lock,
            ));
        }
    };
    if let Err(error) = unpublished_hook(hook, "directoryIntentPersisted") {
        if error.code() == "simulatedCrash" {
            drop(initial.lock);
            return Err(error);
        }
        return Err(finish_with_intent_leases(
            &cleanup,
            error,
            initial.initial_handle,
            &mut resources.journal,
            &[],
            &intent_leases,
            initial.lock,
        ));
    }
    Ok(StaticIntentState { plan, resources, initial, intent_leases })
}

#[cfg(any(unix, windows))]
fn finish_static_transaction<T: TransactionSource>(
    targets: &[T],
    context: &ExecutionContext,
    defer_single_stage: bool,
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    plan: StaticRootPlan,
    resources: ReservedStaticResources,
) -> Result<(PreparedTransaction, Option<File>), CliError> {
    let StaticIntentState { plan, resources, initial, intent_leases } =
        acquire_static_intent(plan, resources, hook)?;
    let StaticRootPlan {
        paths,
        root,
        root_handle,
        missing_directories: missing,
        intent_parent_paths: intents,
        root_guard: guard,
        planning_memory: memory,
    } = plan;
    let ReservedStaticResources {
        mut journal,
        journal_memory,
        mut journal_temporary,
        temporary_reservations,
        parent_paths,
    } = resources;
    let InitialStaticTransaction {
        initial_directory,
        directory,
        initial_handle,
        registry_handle,
        lock,
        registry_identity,
        mut journal_slot_bytes,
    } = initial;
    let cleanup = UnpublishedPrepareCleanup {
        root: &root,
        registry: &registry_handle,
        initial_directory: &initial_directory,
    };
    if let Err(failure) = bind_static_resources(&mut StaticBindInputs {
        journal: &mut journal,
        journal_temporary: &mut journal_temporary,
        journal_slot_bytes: &mut journal_slot_bytes,
        paths: &paths,
        missing_directories: &missing,
        parent_paths: &parent_paths,
        root: &root,
        root_handle: &root_handle,
        initial_handle: &initial_handle,
        intent_leases: &intent_leases,
    }) {
        return Err(if failure.intent_leases_active {
            finish_with_intent_leases(
                &cleanup,
                failure.error,
                initial_handle,
                &mut journal,
                &parent_paths,
                &intent_leases,
                lock,
            )
        } else {
            cleanup.finish(failure.error, initial_handle, &journal, &parent_paths, lock)
        });
    }
    if let Err(error) = unpublished_hook(hook, "directoryCreated") {
        if error.code() == "simulatedCrash" {
            drop(lock);
            return Err(error);
        }
        return Err(cleanup.finish(error, initial_handle, &journal, &parent_paths, lock));
    }
    let (directory_handle, lock) = publish_static_transaction(StaticPublication {
        root_handle: &root_handle,
        registry_identity: &registry_identity,
        initial_directory: &initial_directory,
        directory: &directory,
        initial_handle,
        lock,
        journal: &journal,
        parent_paths: &parent_paths,
        cleanup: &cleanup,
    })?;
    drop((paths, missing, intents, parent_paths, memory, guard));
    stage_published_static(
        PublishedStaticState {
            root,
            directory,
            journal,
            temporary_reservations,
            journal_memory,
            journal_temporary,
            journal_slot_bytes,
            lock,
            root_handle,
            directory_handle,
        },
        targets,
        context,
        defer_single_stage,
        hook,
    )
}

#[cfg(any(unix, windows))]
struct PublishedStaticState {
    root: PathBuf,
    directory: PathBuf,
    journal: Journal,
    temporary_reservations: Vec<ResourceReservation>,
    journal_memory: ResourceReservation,
    journal_temporary: ResourceReservation,
    journal_slot_bytes: [u64; 2],
    lock: File,
    root_handle: SafeDir,
    directory_handle: SafeDir,
}

#[cfg(any(unix, windows))]
fn stage_published_static<T: TransactionSource>(
    state: PublishedStaticState,
    targets: &[T],
    context: &ExecutionContext,
    defer_single_stage: bool,
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<(PreparedTransaction, Option<File>), CliError> {
    let transaction = PreparedTransaction {
        root: state.root,
        directory: state.directory,
        journal: state.journal,
        context: context.clone(),
        active: true,
        temporary_reservations: state.temporary_reservations,
        backup_reservations: Vec::with_capacity(targets.len()),
        journal_memory: state.journal_memory,
        journal_temporary: state.journal_temporary,
        journal_slot_bytes: state.journal_slot_bytes,
        journal_log_bytes: 0,
        #[cfg(test)]
        journal_persist_calls: 2,
        #[cfg(test)]
        journal_record_calls: 0,
        #[cfg(test)]
        journal_record_bytes: 0,
        #[cfg(test)]
        journal_record_sync_calls: 0,
        #[cfg(test)]
        simulate_rollback_failure: false,
        lock: Some(state.lock),
        handles: TransactionHandles { root: state.root_handle, directory: state.directory_handle },
    };
    stage_static_targets(transaction, targets, context, defer_single_stage, hook)
}

#[cfg(any(unix, windows))]
fn finish_with_intent_leases(
    cleanup: &UnpublishedPrepareCleanup<'_>,
    original: CliError,
    initial_handle: SafeDir,
    journal: &mut Journal,
    parent_paths: &[PathBuf],
    intent_leases: &[IntentLease],
    lock: File,
) -> CliError {
    if let Err(error) =
        remove_intent_leases(intent_leases, &initial_handle, journal, &HashSet::new())
    {
        drop(lock);
        return CliError::new(
            ExitClass::Io,
            "rollbackFailed",
            format!(
                "output transaction failed ({}: {}); intent lease rollback failed ({}: {})",
                original.code(),
                original.message(),
                error.code(),
                error.message()
            ),
        );
    }
    cleanup.finish(original, initial_handle, journal, parent_paths, lock)
}

#[cfg(any(unix, windows))]
struct InitialStaticTransaction {
    initial_directory: PathBuf,
    directory: PathBuf,
    initial_handle: SafeDir,
    registry_handle: SafeDir,
    lock: File,
    registry_identity: super::FileIdentity,
    journal_slot_bytes: [u64; 2],
}

#[cfg(any(unix, windows))]
fn initialize_static_transaction(
    root: &Path,
    root_handle: &SafeDir,
    journal: &mut Journal,
    journal_temporary: &mut ResourceReservation,
) -> Result<InitialStaticTransaction, CliError> {
    let epoch = lock_registry_epoch(root_handle, true)?
        .ok_or_else(|| recovery_error("transaction registry epoch disappeared"))?;
    let registry_epoch_handle = epoch.registry();
    let (nonce, initial_directory, directory, lock) =
        create_initial_transaction_in_registry(root, registry_epoch_handle)?;
    journal.nonce = nonce;
    let registry_handle = match root_handle.open_child(OsStr::new(REGISTRY_NAME)) {
        Ok(handle) => handle,
        Err(error) => {
            return Err(cleanup_empty_initial(
                error,
                registry_epoch_handle,
                &initial_directory,
                &journal.nonce,
                lock,
            ));
        }
    };
    if registry_handle.identity != registry_epoch_handle.identity {
        return Err(cleanup_empty_initial(
            recovery_error("transaction registry changed during initial publication"),
            registry_epoch_handle,
            &initial_directory,
            &journal.nonce,
            lock,
        ));
    }
    let Some(initial_name) = initial_directory.file_name() else {
        return Err(cleanup_empty_initial(
            recovery_error("initial transaction has no name"),
            &registry_handle,
            &initial_directory,
            &journal.nonce,
            lock,
        ));
    };
    let initial_handle = match registry_handle.open_child(initial_name) {
        Ok(handle) => handle,
        Err(error) => {
            return Err(cleanup_empty_initial(
                error,
                &registry_handle,
                &initial_directory,
                &journal.nonce,
                lock,
            ));
        }
    };
    let cleanup = UnpublishedPrepareCleanup {
        root,
        registry: &registry_handle,
        initial_directory: &initial_directory,
    };
    let mut journal_slot_bytes = [0_u64; 2];
    if let Err(error) =
        persist_journal_handle(&initial_handle, journal, journal_temporary, &mut journal_slot_bytes)
    {
        return Err(cleanup.finish(error, initial_handle, journal, &[], lock));
    }
    let registry_identity = match epoch.release() {
        Ok(identity) => identity,
        Err(error) => return Err(cleanup.finish(error, initial_handle, journal, &[], lock)),
    };
    Ok(InitialStaticTransaction {
        initial_directory,
        directory,
        initial_handle,
        registry_handle,
        lock,
        registry_identity,
        journal_slot_bytes,
    })
}

#[cfg(any(unix, windows))]
struct StaticPublication<'a> {
    root_handle: &'a SafeDir,
    registry_identity: &'a super::FileIdentity,
    initial_directory: &'a Path,
    directory: &'a Path,
    initial_handle: SafeDir,
    lock: File,
    journal: &'a Journal,
    parent_paths: &'a [PathBuf],
    cleanup: &'a UnpublishedPrepareCleanup<'a>,
}

#[cfg(any(unix, windows))]
fn publish_static_transaction(request: StaticPublication<'_>) -> Result<(SafeDir, File), CliError> {
    let StaticPublication {
        root_handle,
        registry_identity,
        initial_directory,
        directory,
        initial_handle,
        lock,
        journal,
        parent_paths,
        cleanup,
    } = request;
    let publish_epoch = match lock_registry_epoch(root_handle, false) {
        Ok(Some(epoch)) => epoch,
        Ok(None) => {
            return Err(cleanup.finish(
                recovery_error("transaction registry disappeared before publication"),
                initial_handle,
                journal,
                parent_paths,
                lock,
            ));
        }
        Err(error) => {
            return Err(cleanup.finish(error, initial_handle, journal, parent_paths, lock));
        }
    };
    if !publish_epoch.matches_registry_identity(registry_identity) {
        return Err(cleanup.finish(
            recovery_error("transaction registry changed before publication"),
            initial_handle,
            journal,
            parent_paths,
            lock,
        ));
    }
    if let Err(error) = handle_rename(
        publish_epoch.registry(),
        initial_directory.file_name().expect("initial transaction has a name"),
        publish_epoch.registry(),
        directory.file_name().expect("transaction has a name"),
    ) {
        return Err(cleanup.finish(error, initial_handle, journal, parent_paths, lock));
    }
    #[cfg(windows)]
    publish_epoch
        .registry()
        .remove_lease_file(&OsString::from(format!("{EXTERNAL_LOCK_PREFIX}{}", journal.nonce)))?;
    if let Err(error) = publish_epoch.registry().sync() {
        drop(lock);
        return Err(error);
    }
    let directory_handle = publish_epoch.registry().open_child(
        directory.file_name().ok_or_else(|| recovery_error("transaction has no name"))?,
    )?;
    let _ = publish_epoch.release()?;
    active_transactions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(directory.to_path_buf());
    Ok((directory_handle, lock))
}

fn stage_static_targets<T: TransactionSource>(
    mut transaction: PreparedTransaction,
    targets: &[T],
    context: &ExecutionContext,
    defer_single_stage: bool,
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
) -> Result<(PreparedTransaction, Option<File>), CliError> {
    if let Err(error) = crash_point(hook, "journalCreated", usize::MAX, &mut transaction) {
        if error.code() == "simulatedCrash" {
            return Err(error);
        }
        return transaction.fail_and_recover(error);
    }
    let mut deferred_stage = None;
    for (index, target) in targets.iter().enumerate() {
        if let Err(error) = call_hook(hook, "beforeStage", index, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        let mut file = match transaction.handles.directory.create_regular(&stage_name(index)) {
            Ok(file) => file,
            Err(error) => return transaction.fail_and_recover(error),
        };
        if let Err(error) = crash_point(hook, "stageAllocated", index, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        if defer_single_stage {
            if targets.len() != 1 || index != 0 {
                return transaction.fail_and_recover(CliError::internal(
                    "deferred streaming stage requires exactly one target",
                ));
            }
            deferred_stage = Some(file);
            continue;
        }
        if let Err(error) = target.write_to(&mut file, context) {
            return transaction.fail_and_recover(error);
        }
        if let Err(error) = crash_point(hook, "stageWritten", index, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        if let Err(error) = call_hook(hook, "beforeStageSync", index, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        if let Err(error) = context
            .checkpoint()
            .map_err(CliError::from)
            .and_then(|()| file.sync_all().map_err(CliError::from))
        {
            return transaction.fail_and_recover(error);
        }
        if let Err(error) = crash_point(hook, "stageSynced", index, &mut transaction) {
            if error.code() == "simulatedCrash" {
                return Err(error);
            }
            return transaction.fail_and_recover(error);
        }
        transaction.journal.entries[index].staged_identity = Some(file_identity(&file)?);
    }
    if let Err(error) = transaction.handles.directory.sync() {
        return transaction.fail_and_recover(error);
    }
    if defer_single_stage {
        return Ok((transaction, deferred_stage));
    }
    transaction.journal.phase = JournalPhase::Prepared;
    if let Err(error) = transaction.persist_journal() {
        return transaction.fail_and_recover(error);
    }
    if let Err(error) = crash_point(hook, "prepared", usize::MAX, &mut transaction) {
        if error.code() == "simulatedCrash" {
            return Err(error);
        }
        return transaction.fail_and_recover(error);
    }
    Ok((transaction, None))
}
