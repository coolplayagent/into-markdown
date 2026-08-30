use super::{
    CliError, Digest, ExecutionContext, FileIdentity, HashSet, Path, PathBuf, ResourceReservation,
    streaming_identity_index_bytes, streaming_index_capacity_plan, streaming_path_index_bytes,
    transaction_index_limit, verify_streaming_index_capacity,
};

pub(in crate::transaction) struct StreamingTargetIndex {
    pub(in crate::transaction) targets: HashSet<PathBuf>,
    pub(in crate::transaction) originals: HashSet<FileIdentity>,
    pub(in crate::transaction) parents: HashSet<FileIdentity>,
    pub(in crate::transaction) memory: ResourceReservation,
    pub(in crate::transaction) target_capacity_plan: u64,
    pub(in crate::transaction) original_capacity_plan: u64,
    pub(in crate::transaction) parent_capacity_plan: u64,
    #[cfg(test)]
    pub(in crate::transaction) target_lookups: std::cell::Cell<u64>,
    #[cfg(test)]
    pub(in crate::transaction) parent_lookups: std::cell::Cell<u64>,
}

impl StreamingTargetIndex {
    pub(in crate::transaction) fn new(
        primary: PathBuf,
        original: Option<FileIdentity>,
        primary_parent: FileIdentity,
        context: &ExecutionContext,
    ) -> Result<Self, CliError> {
        let target_capacity_plan = streaming_index_capacity_plan::<PathBuf>(1)?;
        let original_capacity_plan =
            if original.is_some() { streaming_index_capacity_plan::<FileIdentity>(1)? } else { 0 };
        let parent_capacity_plan = streaming_index_capacity_plan::<FileIdentity>(1)?;
        let retained = streaming_path_index_bytes(&primary)?
            .checked_add(
                original.as_ref().map(streaming_identity_index_bytes).transpose()?.unwrap_or(0),
            )
            .and_then(|bytes| bytes.checked_add(target_capacity_plan))
            .and_then(|bytes| bytes.checked_add(original_capacity_plan))
            .and_then(|bytes| bytes.checked_add(parent_capacity_plan))
            .and_then(|bytes| {
                streaming_identity_index_bytes(&primary_parent)
                    .ok()
                    .and_then(|parent| bytes.checked_add(parent))
            })
            .ok_or_else(|| {
                transaction_index_limit("streaming transaction index plan overflowed")
            })?;
        let memory = context.reserve_memory(retained).map_err(CliError::from)?;
        let mut targets = HashSet::new();
        targets.try_reserve(1).map_err(|error| {
            transaction_index_limit(format!("cannot reserve streaming target index: {error}"))
        })?;
        verify_streaming_index_capacity::<PathBuf>(targets.capacity(), target_capacity_plan)?;
        if !targets.insert(primary) {
            return Err(CliError::internal("primary streaming target index is duplicated"));
        }
        let mut originals = HashSet::new();
        if let Some(original) = original {
            originals.try_reserve(1).map_err(|error| {
                transaction_index_limit(format!(
                    "cannot reserve streaming original identity index: {error}"
                ))
            })?;
            verify_streaming_index_capacity::<FileIdentity>(
                originals.capacity(),
                original_capacity_plan,
            )?;
            originals.insert(original);
        }
        let mut parents = HashSet::new();
        parents.try_reserve(1).map_err(|error| {
            transaction_index_limit(format!("cannot reserve streaming parent index: {error}"))
        })?;
        verify_streaming_index_capacity::<FileIdentity>(parents.capacity(), parent_capacity_plan)?;
        parents.insert(primary_parent);
        Ok(Self {
            targets,
            originals,
            parents,
            memory,
            target_capacity_plan,
            original_capacity_plan,
            parent_capacity_plan,
            #[cfg(test)]
            target_lookups: std::cell::Cell::new(0),
            #[cfg(test)]
            parent_lookups: std::cell::Cell::new(0),
        })
    }

    pub(in crate::transaction) fn contains_target(&self, target: &Path) -> bool {
        #[cfg(test)]
        self.target_lookups.set(self.target_lookups.get().saturating_add(1));
        self.targets.contains(target)
    }

    pub(in crate::transaction) fn contains_original(&self, identity: &FileIdentity) -> bool {
        self.originals.contains(identity)
    }

    pub(in crate::transaction) fn contains_parent(&self, identity: &FileIdentity) -> bool {
        #[cfg(test)]
        self.parent_lookups.set(self.parent_lookups.get().saturating_add(1));
        self.parents.contains(identity)
    }

    pub(in crate::transaction) fn insert_target(
        &mut self,
        target: PathBuf,
    ) -> Result<(), CliError> {
        let path_bytes = streaming_path_index_bytes(&target)?;
        let grows = self.targets.len() == self.targets.capacity();
        let next_plan = if grows {
            streaming_index_capacity_plan::<PathBuf>(
                self.targets.len().checked_add(1).ok_or_else(|| {
                    transaction_index_limit("streaming target index length overflowed")
                })?,
            )?
        } else {
            self.target_capacity_plan
        };
        let structural = next_plan
            .checked_sub(self.target_capacity_plan)
            .ok_or_else(|| CliError::internal("streaming target index capacity plan regressed"))?;
        self.memory
            .grow(path_bytes.checked_add(structural).ok_or_else(|| {
                transaction_index_limit("streaming target index memory growth overflowed")
            })?)
            .map_err(CliError::from)?;
        if grows {
            self.targets.try_reserve(1).map_err(|error| {
                transaction_index_limit(format!("cannot grow streaming target index: {error}"))
            })?;
            verify_streaming_index_capacity::<PathBuf>(self.targets.capacity(), next_plan)?;
            self.target_capacity_plan = next_plan;
        }
        if !self.targets.insert(target) {
            return Err(CliError::internal("streaming target index accepted a duplicate"));
        }
        Ok(())
    }

    pub(in crate::transaction) fn insert_original(
        &mut self,
        identity: FileIdentity,
    ) -> Result<(), CliError> {
        let identity_bytes = streaming_identity_index_bytes(&identity)?;
        let grows = self.originals.len() == self.originals.capacity();
        let next_plan = if grows {
            streaming_index_capacity_plan::<FileIdentity>(
                self.originals.len().checked_add(1).ok_or_else(|| {
                    transaction_index_limit("streaming original index length overflowed")
                })?,
            )?
        } else {
            self.original_capacity_plan
        };
        let structural = next_plan.checked_sub(self.original_capacity_plan).ok_or_else(|| {
            CliError::internal("streaming original index capacity plan regressed")
        })?;
        self.memory
            .grow(identity_bytes.checked_add(structural).ok_or_else(|| {
                transaction_index_limit("streaming original index memory growth overflowed")
            })?)
            .map_err(CliError::from)?;
        if grows {
            self.originals.try_reserve(1).map_err(|error| {
                transaction_index_limit(format!("cannot grow streaming original index: {error}"))
            })?;
            verify_streaming_index_capacity::<FileIdentity>(self.originals.capacity(), next_plan)?;
            self.original_capacity_plan = next_plan;
        }
        if !self.originals.insert(identity) {
            return Err(CliError::internal("streaming original index accepted a duplicate"));
        }
        Ok(())
    }

    pub(in crate::transaction) fn insert_parent(
        &mut self,
        identity: FileIdentity,
    ) -> Result<(), CliError> {
        let identity_bytes = streaming_identity_index_bytes(&identity)?;
        let grows = self.parents.len() == self.parents.capacity();
        let next_plan = if grows {
            streaming_index_capacity_plan::<FileIdentity>(
                self.parents.len().checked_add(1).ok_or_else(|| {
                    transaction_index_limit("streaming parent index length overflowed")
                })?,
            )?
        } else {
            self.parent_capacity_plan
        };
        let structural = next_plan
            .checked_sub(self.parent_capacity_plan)
            .ok_or_else(|| CliError::internal("streaming parent index capacity plan regressed"))?;
        self.memory
            .grow(identity_bytes.checked_add(structural).ok_or_else(|| {
                transaction_index_limit("streaming parent index memory growth overflowed")
            })?)
            .map_err(CliError::from)?;
        if grows {
            self.parents.try_reserve(1).map_err(|error| {
                transaction_index_limit(format!("cannot grow streaming parent index: {error}"))
            })?;
            verify_streaming_index_capacity::<FileIdentity>(self.parents.capacity(), next_plan)?;
            self.parent_capacity_plan = next_plan;
        }
        if !self.parents.insert(identity) {
            return Err(CliError::internal("streaming parent index accepted a duplicate"));
        }
        Ok(())
    }
}
