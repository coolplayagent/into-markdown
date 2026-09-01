//! One machine snapshot per loaded configuration, reused by command-line overrides.

use into_markdown::MemoryBudgetSnapshotDto;
#[cfg(not(test))]
use sysinfo::{MemoryRefreshKind, RefreshKind, System};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
const MIN_ASSET_BYTES: u64 = 256 * MIB;
const MAX_ASSET_BYTES: u64 = 2 * GIB;
const MIN_TOTAL_ASSET_BYTES: u64 = GIB;
const MAX_TOTAL_ASSET_BYTES: u64 = 4 * GIB;

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
    let reserve = total.map_or(GIB, |total| (total / 8).max(GIB));
    let budget = match (total, available) {
        (Some(total), Some(available)) => {
            three_quarters(total).min(available.saturating_sub(reserve))
        }
        (Some(total), None) => (total / 4).min(2 * GIB),
        (None, Some(available)) => (2 * GIB).min(available.saturating_sub(reserve)),
        (None, None) => 2 * GIB,
    };
    MemoryBudgetSnapshotDto {
        total_bytes: total,
        available_bytes: available,
        system_reserve_bytes: reserve,
        auto_budget_bytes: budget,
        effective_budget_bytes: budget,
        automatic: true,
    }
}

fn three_quarters(value: u64) -> u64 {
    (value / 4) * 3 + (value % 4) * 3 / 4
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
    apply_adaptive_asset_defaults(
        loaded.effective.conversion.limits.max_asset_bytes.is_some()
            || arguments.max_asset_size.is_some(),
        loaded.effective.conversion.limits.max_total_asset_bytes.is_some()
            || arguments.max_total_asset_size.is_some(),
        &mut loaded.options,
    );
}

pub(crate) fn apply_adaptive_asset_defaults(
    max_asset_explicit: bool,
    max_total_asset_explicit: bool,
    options: &mut into_markdown::ConversionOptions,
) {
    let budget = options.limits.max_memory_bytes;
    if !max_total_asset_explicit {
        options.limits.max_total_asset_bytes =
            (budget / 2).clamp(MIN_TOTAL_ASSET_BYTES, MAX_TOTAL_ASSET_BYTES);
    }
    if !max_asset_explicit {
        options.limits.max_asset_bytes = (budget / 4)
            .clamp(MIN_ASSET_BYTES, MAX_ASSET_BYTES)
            .min(options.limits.max_total_asset_bytes);
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
    apply_adaptive_asset_defaults(
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
        apply_adaptive_asset_defaults(false, false, &mut loaded.options);
        assert_eq!(loaded.options.limits.max_asset_bytes, 2 * GIB);
        assert_eq!(loaded.options.limits.max_total_asset_bytes, 4 * GIB);
        assert!(!loaded.memory_snapshot.automatic);
        apply_override(Some(MemorySizeArg::Auto), &mut loaded);
        apply_adaptive_asset_defaults(false, false, &mut loaded.options);
        assert_eq!(loaded.memory_snapshot, snapshot);
        assert_eq!(loaded.options.limits.max_memory_bytes, 10 * GIB);
        assert_eq!(loaded.options.limits.max_asset_bytes, 2 * GIB);
        assert_eq!(loaded.options.limits.max_total_asset_bytes, 4 * GIB);
    }

    #[test]
    fn machine_capacity_and_pressure_select_deterministic_budgets() {
        for (total, available, expected) in [
            (4, 3, 2),
            (16, 12, 10),
            (64, 56, 48),
            (128, 100, 84),
            (16, 3, 1),
            (16, 2, 0),
            (16, 0, 0),
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
        assert_eq!(select(Some(2 * GIB), None).auto_budget_bytes, GIB / 2);
        assert_eq!(select(Some(64 * GIB), None).auto_budget_bytes, 2 * GIB);
        assert_eq!(select(None, Some(GIB / 2)).auto_budget_bytes, 0);
        let invalid = select(Some(4 * GIB), Some(8 * GIB));
        assert_eq!(invalid.available_bytes, None);
        assert_eq!(invalid.auto_budget_bytes, GIB);
        assert_eq!(
            select(Some(u64::MAX), Some(u64::MAX)).auto_budget_bytes,
            three_quarters(u64::MAX)
        );
    }

    #[test]
    fn local_asset_defaults_follow_the_effective_memory_budget() {
        for (memory, expected_asset, expected_total) in
            [(2 * GIB, 512 * MIB, GIB), (10 * GIB, 2 * GIB, 4 * GIB), (64 * GIB, 2 * GIB, 4 * GIB)]
        {
            let mut options = into_markdown::ConversionOptions::default();
            options.limits.max_memory_bytes = memory;
            apply_adaptive_asset_defaults(false, false, &mut options);
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
        apply_adaptive_asset_defaults(true, false, &mut options);
        assert_eq!(options.limits.max_asset_bytes, 96 * MIB);
        assert_eq!(options.limits.max_total_asset_bytes, 4 * GIB);

        options.limits.max_asset_bytes = 96 * MIB;
        options.limits.max_total_asset_bytes = 768 * MIB;
        apply_adaptive_asset_defaults(false, true, &mut options);
        assert_eq!(options.limits.max_asset_bytes, 768 * MIB);
        assert_eq!(options.limits.max_total_asset_bytes, 768 * MIB);

        options.limits.max_asset_bytes = 96 * MIB;
        options.limits.max_total_asset_bytes = 768 * MIB;
        apply_adaptive_asset_defaults(true, true, &mut options);
        assert_eq!(options.limits.max_asset_bytes, 96 * MIB);
        assert_eq!(options.limits.max_total_asset_bytes, 768 * MIB);
    }
}
