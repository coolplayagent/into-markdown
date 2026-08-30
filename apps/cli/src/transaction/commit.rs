use super::{
    AuthenticatedTarget, CliError, EntryState, ExitClass, FILE_ENTRY_TEMPORARY_BYTES, FileIdentity,
    HookDecision, JournalPhase, PathBuf, PreparedTransaction, SafeDir, TargetAuthenticator,
    active_transactions, backup_name, call_hook, crash_point, finish_committed, handle_rename,
    install_stage_no_replace_handle, recover_transaction, recovery_failed, stage_name, target_path,
    transaction_index_limit, try_cleanup_empty_registry, validate_journal_parent_leases,
    verify_handle_content, verify_name_identity, verify_target_handle_identity,
};

impl PreparedTransaction {
    /// Commit every staged target, or recover the complete old set.
    pub fn commit(mut self) -> Result<Vec<PathBuf>, CliError> {
        self.commit_with_hook(|_, _| Ok(HookDecision::Continue))
    }

    /// Discard a transaction which has not begun committing.
    pub fn abort(mut self) -> Result<(), CliError> {
        let cleanup = self.context.cleanup_scope();
        let result = recover_transaction(&self.root, &self.directory, self.lock.take(), &cleanup);

        if result.is_ok() {
            self.temporary_reservations.clear();

            self.backup_reservations.clear();
        }

        self.deactivate();

        if result.is_ok() {
            try_cleanup_empty_registry(&self.root);
        }

        result
    }

    pub(crate) fn commit_with_hook(
        &mut self,
        mut hook: impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<Vec<PathBuf>, CliError> {
        let targets = match self.commit_until_durable(&mut hook) {
            Ok(targets) => targets,
            Err(error) if error.code() == "simulatedCrash" => return Err(error),
            Err(error) => return self.fail_and_recover(error),
        };
        match self.finish_durable_commit() {
            Ok(()) => Ok(targets),
            Err(error) => Err(recovery_failed("committed output cleanup", &error)),
        }
    }

    fn commit_until_durable(
        &mut self,
        hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<Vec<PathBuf>, CliError> {
        self.authenticate_before_commit()?;
        crash_point(hook, "afterTargetAuthentication", usize::MAX, self)?;

        // Recovery may interpret a partially installed set only after every
        // destination and stage has passed authenticated preflight.
        self.journal.phase = JournalPhase::Committing;
        self.persist_journal()?;
        crash_point(hook, "committing", usize::MAX, self)?;

        let root = SafeDir::open_absolute(&self.root)?;
        if root.identity != self.handles.root.identity {
            return Err(CliError::new(
                ExitClass::Io,
                "outputIdentityChanged",
                "transaction root identity changed before publication",
            ));
        }
        let mut authenticator = TargetAuthenticator::new(&root, &self.context)?;
        for index in 0..self.journal.entries.len() {
            self.publish_target(index, &mut authenticator, hook)?;
        }
        validate_journal_parent_leases(&self.handles.directory, &self.handles.root, &self.journal)?;

        self.journal.phase = JournalPhase::Committed;
        self.persist_journal()?;
        crash_point(hook, "committed", usize::MAX, self)?;
        self.journal.entries.iter().map(|entry| target_path(&self.root, entry)).collect()
    }

    fn authenticate_before_commit(&self) -> Result<(), CliError> {
        validate_journal_parent_leases(&self.handles.directory, &self.handles.root, &self.journal)?;
        let mut authenticator = TargetAuthenticator::new(&self.handles.root, &self.context)?;
        for (index, entry) in self.journal.entries.iter().enumerate() {
            self.context.checkpoint().map_err(CliError::from)?;
            let target = authenticator.authenticate(entry, &self.journal.parent_identities)?;
            verify_target_handle_identity(&target, entry.original.as_ref())?;
            let staged_identity =
                self.handles.directory.inspect_regular(&stage_name(index))?.ok_or_else(|| {
                    CliError::new(
                        ExitClass::Io,
                        "outputIdentityChanged",
                        "authenticated transaction stage disappeared before commit",
                    )
                })?;
            if entry.staged_identity.as_ref() != Some(&staged_identity) {
                return Err(CliError::new(
                    ExitClass::Io,
                    "outputIdentityChanged",
                    "authenticated transaction stage identity changed before commit",
                ));
            }
        }
        Ok(())
    }

    fn publish_target(
        &mut self,
        index: usize,
        authenticator: &mut TargetAuthenticator<'_>,
        hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<(), CliError> {
        self.context.checkpoint().map_err(CliError::from)?;
        call_hook(hook, "beforeTarget", index, self)?;
        let entry = self
            .journal
            .entries
            .get(index)
            .ok_or_else(|| CliError::internal("transaction target index is outside journal"))?;
        let target = authenticator.authenticate(entry, &self.journal.parent_identities)?;
        target.parent.verify_namespace()?;
        let expected = entry.original.clone();
        verify_target_handle_identity(&target, expected.as_ref())?;

        if let Some(identity) = expected.as_ref() {
            self.back_up_target(index, &target, identity, hook)?;
        }
        install_stage_no_replace_handle(
            &self.handles.directory,
            &stage_name(index),
            &target.parent,
            &target.name,
        )?;
        target.parent.sync()?;
        self.handles.directory.sync()?;
        verify_handle_content(&target, &self.journal.entries[index])?;
        let staged_bytes = self.journal.entries[index].size;
        self.temporary_reservations[index].shrink(staged_bytes).map_err(CliError::from)?;
        crash_point(hook, "targetInstalled", index, self)?;
        self.journal.entries[index].state = EntryState::Installed;
        crash_point(hook, "installJournaled", index, self)?;
        Ok(())
    }

    fn back_up_target(
        &mut self,
        index: usize,
        target: &AuthenticatedTarget,
        identity: &FileIdentity,
        hook: &mut impl FnMut(&str, usize) -> Result<HookDecision, CliError>,
    ) -> Result<(), CliError> {
        let backup_bytes = identity
            .size
            .checked_add(FILE_ENTRY_TEMPORARY_BYTES)
            .ok_or_else(|| transaction_index_limit("backup metadata budget overflowed"))?;
        self.backup_reservations
            .push(self.context.reserve_temporary(backup_bytes).map_err(CliError::from)?);
        let backup = backup_name(index);
        handle_rename(&target.parent, &target.name, &self.handles.directory, &backup)?;
        verify_name_identity(&self.handles.directory, &backup, Some(identity))?;
        target.parent.sync()?;
        self.handles.directory.sync()?;
        crash_point(hook, "backupRenamed", index, self)?;
        self.journal.entries[index].state = EntryState::BackedUp;
        crash_point(hook, "backupJournaled", index, self)?;
        Ok(())
    }

    fn finish_durable_commit(&mut self) -> Result<(), CliError> {
        let result = finish_committed(
            &self.root,
            &self.directory,
            &self.journal,
            self.lock.take(),
            &self.context,
        );
        if result.is_ok() {
            self.temporary_reservations.clear();
            self.backup_reservations.clear();
        }
        self.deactivate();
        if result.is_ok() {
            try_cleanup_empty_registry(&self.root);
        }
        result
    }

    pub(super) fn fail_and_recover<T>(&mut self, original: CliError) -> Result<T, CliError> {
        #[cfg(test)]
        if self.simulate_rollback_failure {
            self.lock.take();
            self.deactivate();
            return Err(CliError::new(
                ExitClass::Io,
                "rollbackFailed",
                format!(
                    "output transaction failed ({}: {}); rollback failed and journal was preserved (injectedPermissionFailure: deterministic rollback failure)",
                    original.code(),
                    original.message()
                ),
            ));
        }
        let cleanup = self.context.cleanup_scope();
        match recover_transaction(&self.root, &self.directory, self.lock.take(), &cleanup) {
            Ok(()) => {
                self.temporary_reservations.clear();

                self.backup_reservations.clear();

                self.deactivate();

                try_cleanup_empty_registry(&self.root);

                Err(original)
            }

            Err(recovery) => {
                self.deactivate();

                Err(CliError::new(
                    ExitClass::Io,
                    "rollbackFailed",
                    format!(
                        "output transaction failed ({}: {}); rollback failed and journal was preserved ({}: {})",
                        original.code(),
                        original.message(),
                        recovery.code(),
                        recovery.message()
                    ),
                ))
            }
        }
    }

    #[cfg(test)]
    pub(super) fn preserve_staged_files(&mut self) {
        // A simulated process crash cannot keep in-process resource leases alive.

        self.temporary_reservations.clear();

        self.backup_reservations.clear();
    }

    #[cfg(test)]
    pub(super) fn abandon_for_test(mut self) {
        self.preserve_staged_files();

        self.lock.take();

        self.deactivate();
    }

    pub(super) fn deactivate(&mut self) {
        if self.active {
            active_transactions()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.directory);

            self.active = false;
        }
    }
}

impl Drop for PreparedTransaction {
    fn drop(&mut self) {
        self.deactivate();
    }
}
