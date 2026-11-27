//! Invocation-wide lease and isolated OCR observations.
use super::*;

impl ExecutionContext {
    /// Whether this context already spends from an enclosing preflight credit.
    #[doc(hidden)]
    #[must_use]
    pub fn has_memory_credit(&self) -> bool {
        self.memory_credit.is_some()
    }

    /// Borrow a same-context memory reservation as an SPI credit while
    /// cancellation, deadline, progress, and temporary storage remain
    /// request-scoped. The derived context cannot outlive or detach from the
    /// authentic reservation.
    ///
    /// # Errors
    ///
    /// Returns a stable internal error when the reservation is not a memory
    /// charge from this exact context.
    ///
    /// The mutable borrow prevents one parent permit from backing concurrent
    /// credits and statically prevents dropping the permit before its child:
    ///
    /// ```compile_fail
    /// # use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};
    /// let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    /// let mut permit = context.reserve_memory(1).unwrap();
    /// let first = context.with_memory_credit(&mut permit).unwrap();
    /// let second = context.with_memory_credit(&mut permit).unwrap();
    /// let _ = (first, second);
    /// ```
    ///
    /// ```compile_fail
    /// # use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};
    /// let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    /// let mut permit = context.reserve_memory(1).unwrap();
    /// let credit = context.with_memory_credit(&mut permit).unwrap();
    /// drop(permit);
    /// let _ = credit.reserve_memory(1);
    /// ```
    ///
    /// ```compile_fail
    /// # use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};
    /// let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    /// let forged = context.with_memory_credit(u64::MAX).unwrap();
    /// # let _ = forged;
    /// ```
    #[doc(hidden)]
    pub fn with_memory_credit<'a>(
        &self,
        reservation: &'a mut ResourceReservation,
    ) -> Result<PreflightMemoryCredit<'a>, ConversionError> {
        if !reservation.belongs_to_memory_context(self) {
            return Err(ConversionError::Internal {
                detail: "preflight credit requires an authentic same-context memory reservation"
                    .into(),
            });
        }
        // A reservation drawn from an outer credit is already charged to that parent.
        // Promote it to a backing only if it is actually divided. Detached children retain
        // the entire chain of charges until the last child releases its allocation.
        let backing = reservation.memory_backing.get_or_insert_with(|| {
            Arc::new(MemoryBacking {
                shared: Arc::clone(&self.shared),
                parent_credit: self.memory_credit.clone(),
                bytes: AtomicU64::new(reservation.bytes),
            })
        });
        if reservation.active_memory_credit.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(ConversionError::Internal {
                detail: "preflight reservation already backs a live memory credit".into(),
            });
        }
        let memory_credit =
            Arc::new(MemoryCredit { backing: Arc::clone(backing), bytes: AtomicU64::new(0) });
        reservation.active_memory_credit = Some(Arc::downgrade(&memory_credit));
        Ok(PreflightMemoryCredit {
            context: Self {
                shared: Arc::clone(&self.shared),
                memory_credit: Some(memory_credit),
                ignore_request_controls: self.ignore_request_controls,
            },
            _reservation: reservation,
        })
    }

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

    /// Record terminal source-image outcomes independently of merged text and worker calls.
    pub fn record_ocr_images(&self, completed: u64, with_text: u64, failed: u64, skipped: u64) {
        let mut state = lock_unpoisoned(&self.shared.resources.ocr);
        let usage = &mut state.runtime;
        usage.image_sources = usage
            .image_sources
            .saturating_add(completed.saturating_add(failed).saturating_add(skipped));
        usage.images_attempted =
            usage.images_attempted.saturating_add(completed.saturating_add(failed));
        usage.images_completed = usage.images_completed.saturating_add(completed);
        usage.images_with_text = usage.images_with_text.saturating_add(with_text.min(completed));
        usage.images_failed = usage.images_failed.saturating_add(failed);
        usage.images_skipped = usage.images_skipped.saturating_add(skipped);
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
