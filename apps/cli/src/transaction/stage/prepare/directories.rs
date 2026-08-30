use super::{
    BTreeSet, CliError, CreatedDirectory, ExecutionContext, ExitClass, File, FileIdentity,
    HookDecision, Journal, PARENT_HANDLE_BATCH, ParentLeaseRemovalIndex, Path, PathBuf, SafeDir,
    create_parent_lease, encode_path, fs, io, recover_parent_transactions, recovery_error,
    remove_created_output_directories, remove_initial_transaction_with_external_lock,
    remove_parent_lease,
};

pub(super) struct IntentLease {
    pub(super) parent: SafeDir,
}

pub(super) fn plan_intent_parent_paths(
    paths: &[PathBuf],
    missing: &[PathBuf],
) -> Result<Vec<PathBuf>, CliError> {
    let missing = missing.iter().collect::<BTreeSet<_>>();
    let mut ancestors = BTreeSet::new();
    for target in paths {
        let mut parent = target.parent().ok_or_else(|| recovery_error("target has no parent"))?;
        if !missing.contains(&parent.to_path_buf()) {
            continue;
        }
        while missing.contains(&parent.to_path_buf()) {
            parent = parent
                .parent()
                .ok_or_else(|| recovery_error("missing target parent has no existing ancestor"))?;
        }
        ancestors.insert(parent.to_path_buf());
    }
    Ok(ancestors.into_iter().collect())
}

pub(super) fn create_intent_leases(
    paths: &[PathBuf],
    transaction: &SafeDir,
    journal: &mut Journal,
) -> Result<Vec<IntentLease>, CliError> {
    let mut leases = Vec::with_capacity(paths.len());
    for path in paths {
        let parent = SafeDir::open_absolute(path)?;
        if let Err(error) = create_parent_lease(&parent, transaction, journal) {
            let _ = remove_intent_leases(&leases, transaction, journal, &BTreeSet::new());
            return Err(error);
        }
        leases.push(IntentLease { parent });
    }
    Ok(leases)
}

pub(super) fn remove_intent_leases(
    leases: &[IntentLease],
    transaction: &SafeDir,
    journal: &mut Journal,
    keep: &BTreeSet<FileIdentity>,
) -> Result<(), CliError> {
    let removal_identities = leases
        .iter()
        .filter(|lease| !keep.contains(&lease.parent.identity))
        .map(|lease| lease.parent.identity.clone())
        .collect::<Vec<_>>();
    let mut removal_index = ParentLeaseRemovalIndex::new(&removal_identities)?;
    for lease in leases {
        if keep.contains(&lease.parent.identity) {
            continue;
        }
        let inserted = !journal.parent_identities.contains(&lease.parent.identity);
        if inserted {
            journal.parent_identities.push(lease.parent.identity.clone());
        }
        let result = remove_parent_lease(&lease.parent, transaction, journal, &mut removal_index);
        if inserted {
            journal.parent_identities.pop();
        }
        result?;
    }
    removal_index.finish()
}

#[cfg(any(unix, windows))]
pub(in crate::transaction) fn create_missing_output_directory(
    root: &Path,
    root_handle: &SafeDir,
    path: &Path,
) -> Result<Option<CreatedDirectory>, CliError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| recovery_error("created output directory is outside transaction root"))?;
    let name = relative
        .file_name()
        .ok_or_else(|| recovery_error("created output directory has no name"))?;
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let parent = root_handle.open_descendant(parent_relative)?;
    #[cfg(unix)]
    let created = match rustix::fs::mkdirat(
        &parent.fd,
        name,
        rustix::fs::Mode::RUSR
            | rustix::fs::Mode::WUSR
            | rustix::fs::Mode::XUSR
            | rustix::fs::Mode::RGRP
            | rustix::fs::Mode::XGRP
            | rustix::fs::Mode::ROTH
            | rustix::fs::Mode::XOTH,
    ) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(error) => return Err(error.into()),
    };
    #[cfg(windows)]
    let created = match parent.directory.create_dir(name) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    if created {
        parent.sync()?;
    }
    let child = parent.open_child(name)?;
    if !created {
        return Ok(None);
    }
    Ok(Some(CreatedDirectory { path: encode_path(relative)?, identity: child.identity }))
}

#[cfg(any(unix, windows))]
pub(super) fn plan_missing_directories(
    root: &Path,
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>, CliError> {
    let mut missing = BTreeSet::new();
    for target in paths {
        let mut candidate =
            target.parent().ok_or_else(|| recovery_error("target has no parent"))?;
        loop {
            match fs::symlink_metadata(candidate) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !candidate.starts_with(root) || candidate == root {
                        return Err(recovery_error(
                            "missing output directory is outside authenticated transaction root",
                        ));
                    }
                    missing.insert(candidate.to_path_buf());
                    candidate = candidate.parent().ok_or_else(|| {
                        recovery_error("target has no existing directory ancestor")
                    })?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        SafeDir::open_absolute(candidate)?;
    }
    let mut planned = missing.into_iter().collect::<Vec<_>>();
    planned.sort_by(|left, right| {
        left.components().count().cmp(&right.components().count()).then_with(|| left.cmp(right))
    });
    Ok(planned)
}

#[cfg(any(unix, windows))]
pub(super) fn recover_existing_target_parents(
    paths: &[PathBuf],
    context: &ExecutionContext,
) -> Result<(), CliError> {
    let parent_paths = paths
        .iter()
        .map(|target| {
            target
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| recovery_error("target has no parent"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut seen = BTreeSet::new();
    let mut parents = Vec::with_capacity(PARENT_HANDLE_BATCH);
    for parent in parent_paths {
        match fs::symlink_metadata(&parent) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let handle = SafeDir::open_absolute(&parent)?;
        if !seen.insert(handle.identity.clone()) {
            continue;
        }
        parents.push(handle);
        if parents.len() == PARENT_HANDLE_BATCH {
            recover_parent_transactions(&parents, context)?;
            parents.clear();
        }
    }
    if !parents.is_empty() {
        recover_parent_transactions(&parents, context)?;
    }
    Ok(())
}

pub(super) fn unpublished_hook(
    hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    phase: &str,
) -> Result<(), CliError> {
    match hook(phase, usize::MAX)? {
        HookDecision::Continue => Ok(()),
        #[cfg(test)]
        HookDecision::SimulateCrash => {
            Err(CliError::new(ExitClass::Io, "simulatedCrash", format!("{phase}:{}", usize::MAX)))
        }
        #[cfg(test)]
        HookDecision::SimulateRollbackFailure => Err(CliError::new(
            ExitClass::Io,
            "injectedPermissionFailure",
            format!("deterministic rollback failure requested at {phase}:{}", usize::MAX),
        )),
    }
}

#[cfg(any(unix, windows))]
pub(super) fn create_parent_leases_windowed(
    parent_paths: &[PathBuf],
    already_leased: &BTreeSet<FileIdentity>,
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    let mut identities = already_leased.clone();
    for chunk in parent_paths.chunks(PARENT_HANDLE_BATCH) {
        let mut parents = Vec::with_capacity(chunk.len());
        for path in chunk {
            let parent = SafeDir::open_absolute(path)?;
            if identities.insert(parent.identity.clone()) {
                parents.push(parent);
            }
        }
        for parent in &parents {
            create_parent_lease(parent, transaction, journal)?;
        }
    }
    Ok(())
}

#[cfg(any(unix, windows))]
pub(super) fn remove_parent_leases_windowed(
    parent_paths: &[PathBuf],
    transaction: &SafeDir,
    journal: &Journal,
) -> Result<(), CliError> {
    let mut removal_index = ParentLeaseRemovalIndex::new(&journal.parent_identities)?;
    let mut identities = BTreeSet::new();
    for chunk in parent_paths.chunks(PARENT_HANDLE_BATCH) {
        let mut parents = Vec::with_capacity(chunk.len());
        for path in chunk {
            let parent = SafeDir::open_absolute(path)?;
            if identities.insert(parent.identity.clone()) {
                parents.push(parent);
            }
        }
        for parent in &parents {
            remove_parent_lease(parent, transaction, journal, &mut removal_index)?;
        }
    }
    removal_index.finish()
}

#[cfg(any(unix, windows))]
pub(super) struct UnpublishedPrepareCleanup<'a> {
    pub(super) root: &'a Path,
    pub(super) registry: &'a SafeDir,
    pub(super) initial_directory: &'a Path,
}

pub(super) fn cleanup_empty_initial(
    original: CliError,
    registry: &SafeDir,
    initial_directory: &Path,
    nonce: &str,
    lock: File,
) -> CliError {
    drop(lock);
    let Some(name) = initial_directory.file_name() else {
        return CliError::new(
            ExitClass::Io,
            "rollbackFailed",
            "initial transaction has no name during rollback",
        );
    };
    match remove_initial_transaction_with_external_lock(registry, name, nonce) {
        Ok(()) => original,
        Err(cleanup) => CliError::new(
            ExitClass::Io,
            "rollbackFailed",
            format!(
                "output transaction failed ({}: {}); initial rollback failed ({}: {})",
                original.code(),
                original.message(),
                cleanup.code(),
                cleanup.message()
            ),
        ),
    }
}

#[cfg(any(unix, windows))]
impl UnpublishedPrepareCleanup<'_> {
    pub(super) fn finish(
        &self,
        original: CliError,
        initial_handle: SafeDir,
        journal: &Journal,
        parent_paths: &[PathBuf],
        lock: File,
    ) -> CliError {
        if let Err(cleanup) = remove_parent_leases_windowed(parent_paths, &initial_handle, journal)
        {
            drop(lock);
            return CliError::new(
                ExitClass::Io,
                "rollbackFailed",
                format!(
                    "output transaction failed ({}: {}); lease rollback failed and the initial journal was preserved ({}: {})",
                    original.code(),
                    original.message(),
                    cleanup.code(),
                    cleanup.message()
                ),
            );
        }
        drop(initial_handle);
        drop(lock);
        let Some(initial_name) = self.initial_directory.file_name() else {
            return CliError::new(
                ExitClass::Io,
                "rollbackFailed",
                "initial transaction has no name during rollback",
            );
        };
        let initial_cleanup = remove_initial_transaction_with_external_lock(
            self.registry,
            initial_name,
            &journal.nonce,
        );
        let directory_cleanup = remove_created_output_directories(self.root, journal);
        match (initial_cleanup, directory_cleanup) {
            (Ok(()), Ok(())) => original,
            (initial, directories) => {
                let detail = initial
                    .err()
                    .map(|error| format!("initial cleanup {}: {}", error.code(), error.message()))
                    .or_else(|| {
                        directories.err().map(|error| {
                            format!("directory cleanup {}: {}", error.code(), error.message())
                        })
                    })
                    .unwrap_or_else(|| "unknown rollback failure".into());
                CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction failed ({}: {}); rollback failed ({detail})",
                        original.code(),
                        original.message()
                    ),
                )
            }
        }
    }
}
