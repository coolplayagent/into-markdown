use crate::args::{ConversionArgs, MemorySizeArg};
use crate::config;
use into_markdown::ConversionOptions;

pub(super) fn apply_limit_overrides(arguments: &ConversionArgs, options: &mut ConversionOptions) {
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
    assign!(max_pdf_page_objects, max_pdf_page_objects);
    assign!(max_pdf_total_objects, max_pdf_total_objects);
    assign!(max_pdf_layout_comparisons, max_pdf_layout_comparisons);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Cli;
    use clap::Parser;
    #[test]
    fn pdf_limits_cli_overrides_and_ranges() {
        let cli = Cli::try_parse_from([
            "into-md",
            "--max-pdf-page-objects",
            "50",
            "--max-pdf-total-objects",
            "400",
            "--max-pdf-layout-comparisons",
            "5000",
        ])
        .unwrap();
        let mut options = ConversionOptions::default();
        apply_limit_overrides(&cli.conversion, &mut options);
        assert_eq!(
            (
                options.limits.max_pdf_page_objects,
                options.limits.max_pdf_total_objects,
                options.limits.max_pdf_layout_comparisons
            ),
            (50, 400, 5000)
        );
        for flag in
            ["--max-pdf-page-objects", "--max-pdf-total-objects", "--max-pdf-layout-comparisons"]
        {
            for invalid in ["0", "-1", "18446744073709551616"] {
                assert!(Cli::try_parse_from(["into-md", flag, invalid]).is_err());
            }
        }
        assert!(Cli::try_parse_from(["into-md", "--max-pdf-page-objects", "10000001"]).is_err());
    }
}
