//! Local soft-limit adaptation within the immutable request memory envelope.

use super::{ExecutionContext, lock_unpoisoned};
use crate::{ConversionError, ResourceLimits};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

impl std::fmt::Debug for super::ExecutionOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutionOptions")
            .field("cancellation", &self.cancellation)
            .field("timeout", &self.timeout)
            .field("progress_listener", &self.progress_listener.as_ref().map(|_| "registered"))
            .field("resource_adaptation", &self.resource_adaptation)
            .finish()
    }
}

/// Local-only authority to raise selected default soft limits once.
#[derive(Debug, Clone, Default)]
pub struct AdaptiveResourceLimits {
    implicit: BTreeSet<&'static str>,
    ceilings: ResourceLimits,
    enabled: bool,
}

impl AdaptiveResourceLimits {
    /// Create a local adaptation policy for fields not explicitly configured.
    #[must_use]
    pub fn local(
        implicit: impl IntoIterator<Item = &'static str>,
        ceilings: ResourceLimits,
    ) -> Self {
        Self { implicit: implicit.into_iter().collect(), ceilings, enabled: true }
    }

    pub(super) fn into_state(self) -> AdaptiveResourceState {
        AdaptiveResourceState {
            policy: self,
            attempted: Mutex::new(BTreeSet::new()),
            raised: Mutex::new(BTreeMap::new()),
        }
    }

    fn ceiling(&self, limit: &'static str) -> Option<u64> {
        self.enabled.then(|| match limit {
            "max_decompressed_bytes" => self.ceilings.max_decompressed_bytes,
            "max_archive_entries" => u64::from(self.ceilings.max_archive_entries),
            "max_pages" => u64::from(self.ceilings.max_pages),
            "max_asset_bytes" => self.ceilings.max_asset_bytes,
            "max_total_asset_bytes" => self.ceilings.max_total_asset_bytes,
            "max_temporary_bytes" => self.ceilings.max_temporary_bytes,
            "max_table_rows" => self.ceilings.max_table_rows,
            "max_table_columns" => self.ceilings.max_table_columns,
            "max_table_cells" => self.ceilings.max_table_cells,
            _ => 0,
        })
    }
}

#[derive(Debug)]
pub(super) struct AdaptiveResourceState {
    policy: AdaptiveResourceLimits,
    attempted: Mutex<BTreeSet<&'static str>>,
    raised: Mutex<BTreeMap<&'static str, u64>>,
}

impl ExecutionContext {
    /// Return a validated raised limit, or the configured value.
    #[doc(hidden)]
    #[must_use]
    pub fn effective_soft_limit(&self, limit: &'static str, configured: u64) -> u64 {
        lock_unpoisoned(&self.shared.adaptation.raised).get(limit).copied().unwrap_or(configured)
    }

    /// Raise one implicit soft limit once after an exact preflight.
    #[doc(hidden)]
    pub fn try_raise_soft_limit(
        &self,
        limit: &'static str,
        configured: u64,
        required: u64,
        additional_memory_required: u64,
    ) -> Result<Option<u64>, ConversionError> {
        self.checkpoint()?;
        let state = &self.shared.adaptation;
        if required <= configured
            || !state.policy.implicit.contains(limit)
            || additional_memory_required > self.available_memory_bytes()
        {
            return Ok(None);
        }
        let ceiling = state.policy.ceiling(limit).unwrap_or(0);
        let mut attempted = lock_unpoisoned(&state.attempted);
        if attempted.contains(limit) || required > ceiling || ceiling == 0 {
            return Ok(None);
        }
        attempted.insert(limit);
        drop(attempted);
        lock_unpoisoned(&state.raised).insert(limit, required);
        Ok(Some(required))
    }
}
