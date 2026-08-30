use into_markdown_core::{ConversionOptions, ErrorPolicy, ResourceLimits};

const EXCEL_MAX_ROWS: u64 = 1_048_576;
const NATIVE_CELL_HIGH_WATER_BYTES: u64 = 96;
const NATIVE_CELL_CEILING: u64 = 20_000_000;
const LARGE_XML_ENTRY_CEILING: u64 = 300 * 1024 * 1024;
const XML_ENTRY_MEMORY_DIVISOR: u64 = 7;

/// Derive structure limits for the bounded native `SpreadsheetML` data plane.
///
/// Shared memory and temporary budgets are never increased. Explicitly
/// customized structure limits are preserved; only untouched best-effort
/// defaults receive a capacity derived from the already authenticated memory
/// credit.
pub(super) fn xlsx_auto_profile(
    options: &ConversionOptions,
    available_memory: u64,
) -> ConversionOptions {
    let defaults = ResourceLimits::default();
    let mut profiled = options.clone();
    if options.error_policy != ErrorPolicy::BestEffort {
        return profiled;
    }
    if options.limits.max_table_rows == defaults.max_table_rows {
        profiled.limits.max_table_rows = EXCEL_MAX_ROWS;
    }
    if options.limits.max_table_cells == defaults.max_table_cells {
        profiled.limits.max_table_cells = (available_memory / NATIVE_CELL_HIGH_WATER_BYTES)
            .min(NATIVE_CELL_CEILING)
            .max(defaults.max_table_cells);
    }
    if options.limits.max_archive_entry_bytes == defaults.max_archive_entry_bytes {
        profiled.limits.max_archive_entry_bytes = (available_memory / XML_ENTRY_MEMORY_DIVISOR)
            .clamp(defaults.max_archive_entry_bytes, LARGE_XML_ENTRY_CEILING);
    }
    profiled
}

#[cfg(test)]
mod tests {
    use super::xlsx_auto_profile;
    use into_markdown_core::{ConversionOptions, ErrorPolicy};

    #[test]
    fn profile_expands_structure_only_and_preserves_shared_budgets() {
        let options = ConversionOptions::default();
        let profiled = xlsx_auto_profile(&options, 2 * 1024 * 1024 * 1024);
        assert!(profiled.limits.max_table_cells >= 14_000_000);
        assert_eq!(profiled.limits.max_memory_bytes, options.limits.max_memory_bytes);
        assert_eq!(profiled.limits.max_temporary_bytes, options.limits.max_temporary_bytes);

        let mut strict = options.clone();
        strict.error_policy = ErrorPolicy::Strict;
        assert_eq!(xlsx_auto_profile(&strict, u64::MAX), strict);
    }
}
