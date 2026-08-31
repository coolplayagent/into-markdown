use super::{ConversionArgs, ConversionOptions, MemorySizeArg, config};
pub(super) fn apply(arguments: &ConversionArgs, options: &mut ConversionOptions) {
    macro_rules! assign {
        ($argument:ident, $field:ident) => {
            if let Some(value) = arguments.$argument {
                options.limits.$field = value;
            }
        };
    }
    assign!(max_input_size, max_input_bytes);
    assign!(max_decompressed_size, max_decompressed_bytes);
    assign!(max_archive_entries, max_archive_entries);
    assign!(max_archive_depth, max_archive_depth);
    assign!(max_archive_entry_size, max_archive_entry_bytes);
    assign!(max_archive_compression_ratio, max_archive_compression_ratio);
    assign!(max_depth, max_nesting_depth);
    assign!(max_presentation_xml_events, max_presentation_xml_events);
    assign!(max_pages, max_pages);
    assign!(max_asset_size, max_asset_bytes);
    if let Some(value) = arguments.max_memory_size {
        options.limits.max_memory_bytes = match value {
            MemorySizeArg::Auto => config::adaptive_memory_budget(),
            MemorySizeArg::Bytes(bytes) => bytes,
        };
    }
    assign!(max_temporary_size, max_temporary_bytes);
    assign!(max_table_rows, max_table_rows);
    assign!(max_table_columns, max_table_columns);
    assign!(max_table_cells, max_table_cells);
    assign!(max_field_size, max_field_bytes);
    assign!(max_total_asset_size, max_total_asset_bytes);
}
