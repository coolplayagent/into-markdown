//! Invocation-wide configuration and report assembly.
use super::*;

pub(super) fn prepare(
    arguments: &ConversionArgs,
    loaded: &mut LoadedConfig,
) -> Result<(), CliError> {
    apply_conversion_overrides(arguments, loaded)?;
    if loaded.options.limits.max_memory_bytes == 0 {
        let snapshot = loaded.memory_snapshot;
        return Err(into_markdown::ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!(
                "available memory is insufficient for the shared conversion budget after system reserve (totalBytes={:?}, availableBytes={:?}, systemReserveBytes={}, automatic={})",
                snapshot.total_bytes,
                snapshot.available_bytes,
                snapshot.system_reserve_bytes,
                snapshot.automatic,
            ),
        }.into());
    }
    Ok(())
}

pub(super) fn report(
    output_context: &into_markdown::ExecutionContext,
    memory_snapshot: into_markdown::MemoryBudgetSnapshotDto,
    ocr_enabled: bool,
) -> into_markdown::BatchResourceUsageDto {
    let usage = output_context.resource_usage();
    into_markdown::BatchResourceUsageDto {
        shared_lease_budget_bytes: usage.shared_lease_budget_bytes,
        shared_lease_peak_bytes: usage.shared_lease_peak_bytes,
        memory: Some(memory_snapshot),
        ocr_runtime: Some(output_context.ocr_runtime_usage()),
        ocr: ocr_enabled.then_some(into_markdown::BatchOcrUsageDto {
            recognized_regions: usage.ocr_recognized_regions,
            recognized_chars: usage.ocr_recognized_chars,
        }),
    }
}
