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
        || limits.max_pdf_page_objects == 0
        || limits.max_pdf_page_objects > 100_000
        || limits.max_pdf_total_objects == 0
        || limits.max_pdf_total_objects > 10_000_000
        || limits.max_pdf_layout_comparisons == 0
        || limits.max_pdf_layout_comparisons > 12_000_000
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

#[cfg(test)]
mod tests {
    use super::super::{WebTaskRequest, decode_web_task_request};
    #[test]
    fn pdf_limits_web_defaults_and_profile_ceiling() {
        let request = WebTaskRequest::default();
        let mut value = serde_json::to_value(&request).unwrap();
        let fields = [
            ("max_pdf_page_objects", 100_000_u64),
            ("max_pdf_total_objects", 10_000_000),
            ("max_pdf_layout_comparisons", 12_000_000),
        ];
        for (field, maximum) in fields {
            value["options"]["limits"].as_object_mut().unwrap().remove(field);
            assert!(decode_web_task_request(&serde_json::to_vec(&value).unwrap()).is_ok());
            for rejected in [0, maximum + 1] {
                value["options"]["limits"][field] = rejected.into();
                assert!(decode_web_task_request(&serde_json::to_vec(&value).unwrap()).is_err());
            }
            value["options"]["limits"][field] = maximum.into();
        }
    }
}
