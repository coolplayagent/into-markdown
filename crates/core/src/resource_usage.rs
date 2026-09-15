//! Portable observations. Process memory measurements remain separate from application leases.

/// Machine snapshot and the final invocation-wide memory selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudgetSnapshotDto {
    /// Physical memory, when the operating system supplied a valid value.
    pub total_bytes: Option<u64>,
    /// Available memory at invocation configuration resolution.
    pub available_bytes: Option<u64>,
    /// Capacity retained for other processes when automatic selection is used.
    pub system_reserve_bytes: u64,
    /// Automatic budget derived from this exact snapshot; zero refuses admission.
    pub auto_budget_bytes: u64,
    /// Final budget after explicit command-line/configuration overrides.
    pub effective_budget_bytes: u64,
    /// Whether the effective budget was selected automatically.
    pub automatic: bool,
}

/// Bounded aggregate of isolated OCR request attempts, including setup failures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrRuntimeUsageDto {
    /// Image assets considered within each OCR transaction, deduplicated by asset identity.
    pub image_sources: u64,
    /// Source images submitted to recognition, including identities sharing cached bytes.
    pub images_attempted: u64,
    /// Source images whose recognizer returned a valid result, including empty results.
    pub images_completed: u64,
    /// Completed source images with accepted recognizer text before container merging.
    pub images_with_text: u64,
    /// Source images whose recognition attempt failed.
    pub images_failed: u64,
    /// Source images excluded or left unattempted, including preflight refusal.
    pub images_skipped: u64,
    /// Calls assigned a worker allowance and submitted to the process adapter.
    pub requests: u64,
    /// Controlled worker-private recognition memory refusals.
    pub recognition_memory_refusals: u64,
    /// Smallest assigned worker allowance; zero when no requests were submitted.
    pub worker_budget_min_bytes: u64,
    /// Largest assigned worker allowance; zero when no requests were submitted.
    pub worker_budget_max_bytes: u64,
}

/// Auditable OCR text that survived component filtering, deduplication, and merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchOcrUsageDto {
    /// Accepted and merged OCR source regions.
    pub recognized_regions: u64,
    /// Unicode scalar values contributed by those regions.
    pub recognized_chars: u64,
}

/// Invocation-wide resource usage for a CLI batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchResourceUsageDto {
    /// Machine snapshot used to select this invocation's budget.
    pub memory: Option<crate::MemoryBudgetSnapshotDto>,
    /// Isolated OCR requests, including controlled failures.
    pub ocr_runtime: Option<crate::OcrRuntimeUsageDto>,
    /// Actual batch-wide shared memory lease budget.
    pub shared_lease_budget_bytes: u64,
    /// Historical shared memory lease high-water mark.
    pub shared_lease_peak_bytes: u64,
    /// Actual batch-wide shared temporary-storage lease budget.
    pub temporary_lease_budget_bytes: u64,
    /// Historical shared temporary-storage lease high-water mark.
    pub temporary_lease_peak_bytes: u64,
    /// OCR contribution evidence when OCR was enabled for the invocation.
    pub ocr: Option<BatchOcrUsageDto>,
}
