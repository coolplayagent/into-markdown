//! Private wire representation and invariants for invocation resource observations.
use super::*;
use crate::{MemoryBudgetSnapshotDto, OcrRuntimeUsageDto};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawBatchResourceUsageDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) memory: Option<RawMemoryBudgetSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) ocr_runtime: Option<RawOcrRuntimeUsage>,
    pub(super) shared_lease_budget_bytes: u64,
    pub(super) shared_lease_peak_bytes: u64,
    #[serde(default)]
    pub(super) temporary_lease_budget_bytes: u64,
    #[serde(default)]
    pub(super) temporary_lease_peak_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) ocr: Option<super::RawBatchOcrUsageDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawMemoryBudgetSnapshot {
    total_bytes: Option<u64>,
    available_bytes: Option<u64>,
    system_reserve_bytes: u64,
    auto_budget_bytes: u64,
    effective_budget_bytes: u64,
    automatic: bool,
}
impl From<RawMemoryBudgetSnapshot> for MemoryBudgetSnapshotDto {
    fn from(value: RawMemoryBudgetSnapshot) -> Self {
        Self {
            total_bytes: value.total_bytes,
            available_bytes: value.available_bytes,
            system_reserve_bytes: value.system_reserve_bytes,
            auto_budget_bytes: value.auto_budget_bytes,
            effective_budget_bytes: value.effective_budget_bytes,
            automatic: value.automatic,
        }
    }
}
impl From<MemoryBudgetSnapshotDto> for RawMemoryBudgetSnapshot {
    fn from(value: MemoryBudgetSnapshotDto) -> Self {
        Self {
            total_bytes: value.total_bytes,
            available_bytes: value.available_bytes,
            system_reserve_bytes: value.system_reserve_bytes,
            auto_budget_bytes: value.auto_budget_bytes,
            effective_budget_bytes: value.effective_budget_bytes,
            automatic: value.automatic,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawOcrRuntimeUsage {
    #[serde(default)]
    image_sources: u64,
    #[serde(default)]
    images_attempted: u64,
    #[serde(default)]
    images_completed: u64,
    #[serde(default)]
    images_with_text: u64,
    #[serde(default)]
    images_failed: u64,
    #[serde(default)]
    images_skipped: u64,

    requests: u64,
    recognition_memory_refusals: u64,
    worker_budget_min_bytes: u64,
    worker_budget_max_bytes: u64,
}
impl From<RawOcrRuntimeUsage> for OcrRuntimeUsageDto {
    fn from(value: RawOcrRuntimeUsage) -> Self {
        Self {
            image_sources: value.image_sources,
            images_attempted: value.images_attempted,
            images_completed: value.images_completed,
            images_with_text: value.images_with_text,
            images_failed: value.images_failed,
            images_skipped: value.images_skipped,
            requests: value.requests,
            recognition_memory_refusals: value.recognition_memory_refusals,
            worker_budget_min_bytes: value.worker_budget_min_bytes,
            worker_budget_max_bytes: value.worker_budget_max_bytes,
        }
    }
}
impl From<OcrRuntimeUsageDto> for RawOcrRuntimeUsage {
    fn from(value: OcrRuntimeUsageDto) -> Self {
        Self {
            image_sources: value.image_sources,
            images_attempted: value.images_attempted,
            images_completed: value.images_completed,
            images_with_text: value.images_with_text,
            images_failed: value.images_failed,
            images_skipped: value.images_skipped,
            requests: value.requests,
            recognition_memory_refusals: value.recognition_memory_refusals,
            worker_budget_min_bytes: value.worker_budget_min_bytes,
            worker_budget_max_bytes: value.worker_budget_max_bytes,
        }
    }
}

pub(super) fn validate(usage: &BatchResourceUsageDto) -> Result<(), DtoError> {
    if usage.shared_lease_budget_bytes == 0 {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            "$.resourceUsage.sharedLeaseBudgetBytes",
            "shared lease budget must be greater than zero",
        ));
    }
    if usage.shared_lease_peak_bytes > usage.shared_lease_budget_bytes {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            "$.resourceUsage.sharedLeasePeakBytes",
            "shared lease peak cannot exceed its budget",
        ));
    }
    if usage.temporary_lease_peak_bytes > usage.temporary_lease_budget_bytes {
        return Err(invalid(
            "temporaryLeasePeakBytes",
            "temporary lease peak cannot exceed its budget",
        ));
    }
    if usage.ocr.is_some_and(|ocr| (ocr.recognized_regions == 0) != (ocr.recognized_chars == 0)) {
        return Err(DtoError::new(
            DtoErrorCode::InvalidField,
            "$.resourceUsage.ocr",
            "OCR regions and characters must both be zero or both be positive",
        ));
    }

    if let Some(memory) = usage.memory {
        if memory.effective_budget_bytes != usage.shared_lease_budget_bytes
            || (memory.automatic && memory.auto_budget_bytes != memory.effective_budget_bytes)
            || memory.total_bytes == Some(0)
            || memory
                .total_bytes
                .zip(memory.available_bytes)
                .is_some_and(|(total, available)| available > total)
        {
            return Err(invalid(
                "memory",
                "memory selection must agree with its snapshot and shared budget",
            ));
        }
    }
    if let Some(ocr) = usage.ocr_runtime {
        if ocr.images_completed.checked_add(ocr.images_failed) != Some(ocr.images_attempted)
            || ocr.images_attempted.checked_add(ocr.images_skipped) != Some(ocr.image_sources)
            || ocr.images_with_text > ocr.images_completed
        {
            return Err(invalid("ocrRuntime", "OCR source image outcomes must balance"));
        }
        if ocr.recognition_memory_refusals > ocr.requests
            || (ocr.requests == 0)
                != (ocr.worker_budget_min_bytes == 0 && ocr.worker_budget_max_bytes == 0)
            || (ocr.requests > 0 && ocr.worker_budget_min_bytes == 0)
            || ocr.worker_budget_min_bytes > ocr.worker_budget_max_bytes
            || ocr.worker_budget_max_bytes > usage.shared_lease_budget_bytes
        {
            return Err(invalid(
                "ocrRuntime",
                "OCR request counters and worker allowances must agree",
            ));
        }
    }
    Ok(())
}

fn invalid(field: &str, detail: &str) -> DtoError {
    DtoError::new(DtoErrorCode::InvalidField, format!("$.resourceUsage.{field}"), detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> BatchResourceUsageDto {
        BatchResourceUsageDto {
            memory: Some(MemoryBudgetSnapshotDto {
                total_bytes: Some(32),
                available_bytes: Some(20),
                system_reserve_bytes: 4,
                auto_budget_bytes: 16,
                effective_budget_bytes: 16,
                automatic: true,
            }),
            ocr_runtime: Some(OcrRuntimeUsageDto {
                requests: 2,
                recognition_memory_refusals: 1,
                worker_budget_min_bytes: 4,
                worker_budget_max_bytes: 8,
                ..OcrRuntimeUsageDto::default()
            }),
            shared_lease_budget_bytes: 16,
            shared_lease_peak_bytes: 12,
            temporary_lease_budget_bytes: 32,
            temporary_lease_peak_bytes: 8,
            ocr: None,
        }
    }

    #[test]
    fn observations_round_trip_through_both_bounded_writers() {
        let report =
            BatchReportDto::try_new_with_resource_usage(vec![], None, Some(usage())).unwrap();
        let compact = report.to_json().unwrap();
        let mut streamed = Vec::new();
        report.write_json(DtoJsonStyle::Compact, &mut streamed).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&compact).unwrap(),
            serde_json::from_slice::<serde_json::Value>(&streamed).unwrap()
        );
        assert_eq!(BatchReportDto::from_json(&compact).unwrap(), report);
    }

    #[test]
    fn decoder_rejects_inconsistent_machine_and_worker_observations() {
        let report =
            BatchReportDto::try_new_with_resource_usage(vec![], None, Some(usage())).unwrap();
        let original: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
        for (field, key, value) in [
            ("memory", "effectiveBudgetBytes", 17),
            ("memory", "autoBudgetBytes", 17),
            ("memory", "availableBytes", 33),
            ("memory", "totalBytes", 0),
            ("ocrRuntime", "recognitionMemoryRefusals", 3),
            ("ocrRuntime", "workerBudgetMinBytes", 0),
            ("ocrRuntime", "workerBudgetMaxBytes", 17),
            ("ocrRuntime", "requests", 0),
            ("ocrRuntime", "imageSources", 1),
            ("ocrRuntime", "imagesAttempted", 1),
            ("ocrRuntime", "imagesCompleted", 1),
            ("ocrRuntime", "imagesFailed", 1),
            ("ocrRuntime", "imagesSkipped", 1),
            ("ocrRuntime", "imagesWithText", 1),
        ] {
            let mut modified = original.clone();
            modified["resourceUsage"][field][key] = value.into();
            assert!(BatchReportDto::from_json(&modified.to_string()).is_err(), "{field}.{key}");
        }
    }
}
