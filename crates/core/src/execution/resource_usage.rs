//! Invocation-wide lease and isolated OCR observations.
use super::*;

impl ExecutionContext {
    /// Return the invocation-wide historical resource accounting shared with every fork.
    #[doc(hidden)]
    #[must_use]
    pub fn resource_usage(&self) -> ExecutionResourceUsage {
        let ocr = lock_unpoisoned(&self.shared.resources.ocr);
        ExecutionResourceUsage {
            shared_lease_budget_bytes: self.shared.limits.max_memory_bytes,
            shared_lease_peak_bytes: self
                .shared
                .resources
                .memory_peak_bytes
                .load(Ordering::Acquire),
            temporary_lease_budget_bytes: self.shared.limits.max_temporary_bytes,
            temporary_lease_peak_bytes: self
                .shared
                .resources
                .temporary_peak_bytes
                .load(Ordering::Acquire),
            ocr_recognized_regions: ocr.recognized_regions,
            ocr_recognized_chars: ocr.recognized_chars,
        }
    }

    /// Isolated OCR request accounting, independent of accepted text contributions.
    #[must_use]
    pub fn ocr_runtime_usage(&self) -> crate::OcrRuntimeUsageDto {
        lock_unpoisoned(&self.shared.resources.ocr).runtime
    }

    /// Record an assigned worker allowance without retaining per-image history.
    pub fn record_ocr_request(&self, bytes: u64) {
        let mut state = lock_unpoisoned(&self.shared.resources.ocr);
        let usage = &mut state.runtime;
        usage.requests = usage.requests.saturating_add(1);
        usage.worker_budget_min_bytes =
            if usage.requests == 1 { bytes } else { usage.worker_budget_min_bytes.min(bytes) };
        usage.worker_budget_max_bytes = usage.worker_budget_max_bytes.max(bytes);
    }

    /// Record only a typed, controlled worker recognition memory refusal.
    pub fn record_ocr_memory_refusal(&self) {
        let mut state = lock_unpoisoned(&self.shared.resources.ocr);
        state.runtime.recognition_memory_refusals =
            state.runtime.recognition_memory_refusals.saturating_add(1);
    }
}
