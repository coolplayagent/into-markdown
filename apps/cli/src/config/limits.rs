use super::{CliError, ConversionOptions, MemoryLimitConfig, adaptive_memory_budget};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
pub struct LimitsConfig {
    pub max_input_bytes: Option<u64>,
    pub max_decompressed_bytes: Option<u64>,
    pub max_archive_entries: Option<u32>,
    pub max_archive_depth: Option<u16>,
    pub max_archive_entry_bytes: Option<u64>,
    pub max_archive_compression_ratio: Option<u32>,
    pub max_nesting_depth: Option<u16>,
    pub max_presentation_xml_events: Option<u64>,
    pub max_pages: Option<u32>,
    pub max_asset_bytes: Option<u64>,
    pub max_total_asset_bytes: Option<u64>,
    pub max_memory_bytes: Option<MemoryLimitConfig>,
    pub max_temporary_bytes: Option<u64>,
    pub max_table_rows: Option<u64>,
    pub max_table_columns: Option<u64>,
    pub max_table_cells: Option<u64>,
    pub max_field_bytes: Option<u64>,
}

pub(super) fn apply(
    limits: &LimitsConfig,
    options: &mut ConversionOptions,
) -> Result<(), CliError> {
    if limits.max_presentation_xml_events == Some(0) {
        return Err(CliError::usage("max_presentation_xml_events must be greater than zero"));
    }
    macro_rules! assign {
        ($field:ident) => {
            if let Some(value) = limits.$field {
                options.limits.$field = value;
            }
        };
    }
    assign!(max_input_bytes);
    assign!(max_decompressed_bytes);
    assign!(max_archive_entries);
    assign!(max_archive_depth);
    assign!(max_archive_entry_bytes);
    assign!(max_archive_compression_ratio);
    assign!(max_nesting_depth);
    assign!(max_presentation_xml_events);
    assign!(max_pages);
    assign!(max_asset_bytes);
    assign!(max_total_asset_bytes);
    options.limits.max_memory_bytes = match limits.max_memory_bytes.as_ref() {
        Some(MemoryLimitConfig::Bytes(value)) => *value,
        Some(MemoryLimitConfig::Mode(value)) if value.eq_ignore_ascii_case("auto") => {
            adaptive_memory_budget()
        }
        Some(MemoryLimitConfig::Mode(value)) => {
            return Err(CliError::config(format!(
                "conversion.limits.max_memory_bytes must be an integer or 'auto', got '{value}'"
            )));
        }
        None => adaptive_memory_budget(),
    };
    assign!(max_temporary_bytes);
    assign!(max_table_rows);
    assign!(max_table_columns);
    assign!(max_table_cells);
    assign!(max_field_bytes);
    Ok(())
}
