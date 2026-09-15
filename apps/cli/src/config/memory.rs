//! One machine snapshot per loaded configuration, reused by command-line overrides.

use into_markdown::MemoryBudgetSnapshotDto;
#[cfg(not(test))]
use sysinfo::{MemoryRefreshKind, RefreshKind, System};

#[cfg(test)]
const GIB: u64 = 1024 * 1024 * 1024;
#[cfg(test)]
const MIB: u64 = 1024 * 1024;
// CLI unit tests use a deterministic host, with pressure/missing-probe cases
// injected through select(). Source-built and installed binaries probe the OS.
#[cfg(test)]
pub(crate) fn probe() -> MemoryBudgetSnapshotDto {
    select(Some(16 * GIB), Some(12 * GIB))
}

#[cfg(not(test))]
pub(crate) fn probe() -> MemoryBudgetSnapshotDto {
    let system = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
    );
    let total = (system.total_memory() > 0).then_some(system.total_memory());
    let available = total.map(|_| system.available_memory());
    select(total, available)
}

pub(crate) fn select(total: Option<u64>, available: Option<u64>) -> MemoryBudgetSnapshotDto {
    let total = total.filter(|value| *value > 0);
    let available = available.filter(|value| total.is_none_or(|total| *value <= total));
    // A momentary free-memory sample cannot describe reclaimable caches, swap,
    // or the memory needed by the next file. Use machine capacity as the stable
    // automatic ceiling; reservations still account actual concurrent work.
    let budget = total
        .or(available.filter(|value| *value > 0))
        .unwrap_or_else(|| into_markdown::ResourceLimits::default().max_memory_bytes);
    MemoryBudgetSnapshotDto {
        total_bytes: total,
        available_bytes: available,
        system_reserve_bytes: 0,
        auto_budget_bytes: budget,
        effective_budget_bytes: budget,
        automatic: true,
    }
}

pub(crate) fn apply_override(
    value: Option<crate::args::MemorySizeArg>,
    loaded: &mut super::LoadedConfig,
) {
    if let Some(value) = value {
        loaded.memory_snapshot.automatic = matches!(value, crate::args::MemorySizeArg::Auto);
        loaded.options.limits.max_memory_bytes = match value {
            crate::args::MemorySizeArg::Auto => loaded.memory_snapshot.auto_budget_bytes,
            crate::args::MemorySizeArg::Bytes(bytes) => bytes,
        };
    }
    loaded.memory_snapshot.effective_budget_bytes = loaded.options.limits.max_memory_bytes;
}

pub(crate) fn apply_overrides(
    arguments: &crate::args::ConversionArgs,
    loaded: &mut super::LoadedConfig,
) {
    apply_override(arguments.max_memory_size, loaded);
    apply_asset_defaults(
        loaded.effective.conversion.limits.max_asset_bytes.is_some()
            || arguments.max_asset_size.is_some(),
        loaded.effective.conversion.limits.max_total_asset_bytes.is_some()
            || arguments.max_total_asset_size.is_some(),
        &mut loaded.options,
    );
}

pub(crate) fn local_execution_options(
    arguments: &crate::args::ConversionArgs,
    loaded: &super::LoadedConfig,
) -> into_markdown::ExecutionOptions {
    into_markdown::ExecutionOptions {
        timeout: arguments.timeout_ms.or(loaded.timeout_ms).map(std::time::Duration::from_millis),
        ..into_markdown::ExecutionOptions::default()
    }
}

pub(crate) fn apply_asset_defaults(
    max_asset_explicit: bool,
    max_total_asset_explicit: bool,
    options: &mut into_markdown::ConversionOptions,
) {
    let budget = options.limits.max_memory_bytes;
    if !max_total_asset_explicit {
        options.limits.max_total_asset_bytes = budget;
    }
    if !max_asset_explicit {
        options.limits.max_asset_bytes = budget.min(options.limits.max_total_asset_bytes);
    }
}

pub(crate) fn resolve(
    config: &super::ConversionConfig,
) -> Result<(into_markdown::ConversionOptions, MemoryBudgetSnapshotDto), crate::error::CliError> {
    let mut snapshot = probe();
    let options = super::resolve_conversion_options(config, &mut snapshot)?;
    Ok((options, snapshot))
}

pub(super) fn apply_config(
    limits: &super::LimitsConfig,
    options: &mut into_markdown::ConversionOptions,
    snapshot: &mut MemoryBudgetSnapshotDto,
) -> Result<(), crate::error::CliError> {
    let value = &limits.max_memory_bytes;
    options.limits.max_memory_bytes = match value {
        Some(super::MemoryLimitConfig::Bytes(bytes)) => *bytes,
        Some(super::MemoryLimitConfig::Mode(mode)) if !mode.eq_ignore_ascii_case("auto") => {
            return Err(crate::error::CliError::config(format!(
                "conversion.limits.max_memory_bytes must be an integer or 'auto', got '{mode}'"
            )));
        }
        _ => snapshot.auto_budget_bytes,
    };
    snapshot.effective_budget_bytes = options.limits.max_memory_bytes;
    snapshot.automatic = !matches!(value, Some(super::MemoryLimitConfig::Bytes(_)));
    apply_asset_defaults(
        limits.max_asset_bytes.is_some(),
        limits.max_total_asset_bytes.is_some(),
        options,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_selection_overrides_configuration_and_reuses_the_invocation_snapshot() {
        use crate::args::MemorySizeArg;
        let root = tempfile::tempdir().unwrap();
        let mut loaded = crate::config::load(root.path(), &[], true, None, None).unwrap();
        let snapshot = select(Some(16 * GIB), Some(12 * GIB));
        loaded.memory_snapshot = snapshot;
        apply_config(
            &crate::config::LimitsConfig {
                max_memory_bytes: Some(crate::config::MemoryLimitConfig::Bytes(3 * GIB)),
                ..Default::default()
            },
            &mut loaded.options,
            &mut loaded.memory_snapshot,
        )
        .unwrap();
        apply_override(None, &mut loaded);
        assert_eq!(loaded.options.limits.max_memory_bytes, 3 * GIB);
        apply_override(Some(MemorySizeArg::Bytes(16 * GIB)), &mut loaded);
        assert_eq!(loaded.options.limits.max_memory_bytes, 16 * GIB);
        apply_asset_defaults(false, false, &mut loaded.options);
        assert_eq!(loaded.options.limits.max_asset_bytes, 16 * GIB);
        assert_eq!(loaded.options.limits.max_total_asset_bytes, 16 * GIB);
        assert!(!loaded.memory_snapshot.automatic);
        apply_override(Some(MemorySizeArg::Auto), &mut loaded);
        apply_asset_defaults(false, false, &mut loaded.options);
        assert_eq!(loaded.memory_snapshot, snapshot);
        assert_eq!(loaded.options.limits.max_memory_bytes, 16 * GIB);
        assert_eq!(loaded.options.limits.max_asset_bytes, 16 * GIB);
        assert_eq!(loaded.options.limits.max_total_asset_bytes, 16 * GIB);
    }

    #[test]
    fn transient_memory_pressure_does_not_reject_conversion() {
        for (total, available, expected) in [
            (4, 3, 4),
            (16, 12, 16),
            (64, 56, 64),
            (128, 100, 128),
            (16, 3, 16),
            (16, 2, 16),
            (16, 0, 16),
        ] {
            assert_eq!(
                select(Some(total * GIB), Some(available * GIB)).auto_budget_bytes,
                expected * GIB
            );
        }
    }

    #[test]
    fn missing_or_inconsistent_probes_use_known_capacity_conservatively() {
        assert_eq!(select(None, None).auto_budget_bytes, 2 * GIB);
        assert_eq!(select(Some(2 * GIB), None).auto_budget_bytes, 2 * GIB);
        assert_eq!(select(Some(64 * GIB), None).auto_budget_bytes, 64 * GIB);
        assert_eq!(select(None, Some(GIB / 2)).auto_budget_bytes, GIB / 2);
        let invalid = select(Some(4 * GIB), Some(8 * GIB));
        assert_eq!(invalid.available_bytes, None);
        assert_eq!(invalid.auto_budget_bytes, 4 * GIB);
        assert_eq!(select(Some(u64::MAX), Some(u64::MAX)).auto_budget_bytes, u64::MAX);
    }

    #[test]
    fn local_asset_defaults_follow_the_effective_memory_budget() {
        for (memory, expected_asset, expected_total) in [
            (2 * GIB, 2 * GIB, 2 * GIB),
            (10 * GIB, 10 * GIB, 10 * GIB),
            (64 * GIB, 64 * GIB, 64 * GIB),
        ] {
            let mut options = into_markdown::ConversionOptions::default();
            options.limits.max_memory_bytes = memory;
            apply_asset_defaults(false, false, &mut options);
            assert_eq!(options.limits.max_asset_bytes, expected_asset);
            assert_eq!(options.limits.max_total_asset_bytes, expected_total);
        }
    }

    #[test]
    fn explicit_asset_limits_are_preserved_independently() {
        let mut options = into_markdown::ConversionOptions::default();
        options.limits.max_memory_bytes = 16 * GIB;
        options.limits.max_asset_bytes = 96 * MIB;
        options.limits.max_total_asset_bytes = 768 * MIB;
        apply_asset_defaults(true, false, &mut options);
        assert_eq!(options.limits.max_asset_bytes, 96 * MIB);
        assert_eq!(options.limits.max_total_asset_bytes, 16 * GIB);

        options.limits.max_asset_bytes = 96 * MIB;
        options.limits.max_total_asset_bytes = 768 * MIB;
        apply_asset_defaults(false, true, &mut options);
        assert_eq!(options.limits.max_asset_bytes, 768 * MIB);
        assert_eq!(options.limits.max_total_asset_bytes, 768 * MIB);

        options.limits.max_asset_bytes = 96 * MIB;
        options.limits.max_total_asset_bytes = 768 * MIB;
        apply_asset_defaults(true, true, &mut options);
        assert_eq!(options.limits.max_asset_bytes, 96 * MIB);
        assert_eq!(options.limits.max_total_asset_bytes, 768 * MIB);
    }
}
