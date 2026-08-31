use super::{CliError, ConversionOptions, MemoryLimitConfig};
use serde::{Deserialize, Serialize};

/// Partial resource budgets.
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
    pub max_pdf_page_objects: Option<u32>,
    pub max_pdf_total_objects: Option<u64>,
    pub max_pdf_layout_comparisons: Option<u64>,
    pub max_asset_bytes: Option<u64>,
    pub max_total_asset_bytes: Option<u64>,
    pub max_memory_bytes: Option<MemoryLimitConfig>,
    pub max_temporary_bytes: Option<u64>,
    pub max_table_rows: Option<u64>,
    pub max_table_columns: Option<u64>,
    pub max_table_cells: Option<u64>,
    pub max_field_bytes: Option<u64>,
}

impl LimitsConfig {
    pub(super) fn apply(&self, options: &mut ConversionOptions) -> Result<(), CliError> {
        if self.max_presentation_xml_events == Some(0) {
            return Err(CliError::usage("max_presentation_xml_events must be greater than zero"));
        }
        let limits = self;
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
        assign!(max_pdf_page_objects);
        assign!(max_pdf_total_objects);
        assign!(max_pdf_layout_comparisons);
        options.limits.validate_pdf().map_err(|error| CliError::config(error.to_string()))?;
        assign!(max_asset_bytes);
        assign!(max_total_asset_bytes);
        assign!(max_temporary_bytes);
        assign!(max_table_rows);
        assign!(max_table_columns);
        assign!(max_table_cells);
        assign!(max_field_bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pdf_limits_config_defaults_and_roundtrip() {
        let configured: LimitsConfig = toml::from_str("max_pdf_page_objects = 25\nmax_pdf_total_objects = 300\nmax_pdf_layout_comparisons = 20000").unwrap();
        let mut options = ConversionOptions::default();
        configured.apply(&mut options).unwrap();
        assert_eq!(
            (
                options.limits.max_pdf_page_objects,
                options.limits.max_pdf_total_objects,
                options.limits.max_pdf_layout_comparisons
            ),
            (25, 300, 20000)
        );
        let encoded = toml::to_string(&configured).unwrap();
        let decoded: LimitsConfig = toml::from_str(&encoded).unwrap();
        decoded.apply(&mut options).unwrap();
        let default = ConversionOptions::default();
        let mut old = default.clone();
        toml::from_str::<LimitsConfig>("max_pages = 600").unwrap().apply(&mut old).unwrap();
        assert_eq!(old.limits.max_pdf_total_objects, default.limits.max_pdf_total_objects);
        for field in ["max_pdf_page_objects", "max_pdf_total_objects", "max_pdf_layout_comparisons"]
        {
            let zero: LimitsConfig = toml::from_str(&format!("{field} = 0")).unwrap();
            assert!(zero.apply(&mut ConversionOptions::default()).is_err());
        }
    }
}
