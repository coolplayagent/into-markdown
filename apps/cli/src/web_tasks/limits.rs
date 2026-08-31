use super::{MAX_FILE_BYTES, MAX_WEB_MEMORY_BYTES, MAX_WEB_TEMPORARY_BYTES, WebTaskError};
use into_markdown::ResourceLimits;

pub(super) fn validate(limits: &ResourceLimits) -> Result<(), WebTaskError> {
    if limits.max_input_bytes == 0 || limits.max_input_bytes > MAX_FILE_BYTES {
        return Err(WebTaskError::Invalid("max_input_bytes must be within 1 and 512 MiB".into()));
    }
    if limits.max_memory_bytes == 0 || limits.max_memory_bytes > MAX_WEB_MEMORY_BYTES {
        return Err(WebTaskError::Invalid("max_memory_bytes exceeds the Web profile".into()));
    }
    if limits.max_temporary_bytes == 0 || limits.max_temporary_bytes > MAX_WEB_TEMPORARY_BYTES {
        return Err(WebTaskError::Invalid("max_temporary_bytes exceeds the Web profile".into()));
    }
    if limits.max_asset_bytes > 64 * 1024 * 1024
        || limits.max_total_asset_bytes > 128 * 1024 * 1024
        || limits.max_pages == 0
        || limits.max_pages > 10_000
        || limits.max_decompressed_bytes == 0
        || limits.max_decompressed_bytes > 1024 * 1024 * 1024
        || limits.max_archive_entries == 0
        || limits.max_archive_entries > 100_000
        || limits.max_archive_depth == 0
        || limits.max_archive_depth > 16
        || limits.max_archive_entry_bytes == 0
        || limits.max_archive_entry_bytes > 256 * 1024 * 1024
        || limits.max_archive_compression_ratio == 0
        || limits.max_archive_compression_ratio > 100
        || limits.max_presentation_xml_events == 0
        || limits.max_presentation_xml_events > 2_000_000
        || limits.max_nesting_depth == 0
        || limits.max_nesting_depth > 256
        || limits.max_table_rows == 0
        || limits.max_table_rows > 100_000
        || limits.max_table_columns == 0
        || limits.max_table_columns > 16_384
        || limits.max_table_cells == 0
        || limits.max_table_cells > 1_000_000
        || limits.max_field_bytes == 0
        || limits.max_field_bytes > 16 * 1024 * 1024
        || limits.max_feed_entries == 0
        || limits.max_feed_entries > 10_000
        || limits.max_feed_text_bytes == 0
        || limits.max_feed_text_bytes > 64 * 1024 * 1024
        || limits.max_feed_html_bytes == 0
        || limits.max_feed_html_bytes > 64 * 1024 * 1024
    {
        return Err(WebTaskError::Invalid("resource limits exceed the Web profile".into()));
    }
    Ok(())
}
