use super::WebTaskError;
use into_markdown::{CancellationToken, ExecutionOptions, ProgressListener, ResourceLimits};
use std::sync::Arc;

pub(super) fn execution_options(
    cancellation: CancellationToken,
    progress_listener: Arc<dyn ProgressListener>,
) -> ExecutionOptions {
    ExecutionOptions {
        cancellation,
        timeout: None,
        progress_listener: Some(progress_listener),
        ..ExecutionOptions::default()
    }
}

pub(super) fn validate(limits: &ResourceLimits) -> Result<(), WebTaskError> {
    macro_rules! positive {
        ($($field:ident),+ $(,)?) => {$(
            if limits.$field == 0 {
                return Err(WebTaskError::Invalid(concat!(stringify!($field), " must be positive").into()));
            }
        )+};
    }
    positive!(
        max_input_bytes,
        max_memory_bytes,
        max_temporary_bytes,
        max_decompressed_bytes,
        max_archive_entries,
        max_archive_depth,
        max_archive_entry_bytes,
        max_archive_compression_ratio,
        max_nesting_depth,
        max_presentation_xml_events,
        max_table_rows,
        max_table_columns,
        max_table_cells,
        max_field_bytes,
        max_feed_entries,
        max_feed_text_bytes,
        max_feed_html_bytes
    );
    limits.validate_pdf().map_err(|error| WebTaskError::Invalid(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{WebTaskRequest, decode_web_task_request};
    #[test]
    fn explicit_limits_preserve_cli_ranges() {
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
            for rejected in [0] {
                value["options"]["limits"][field] = rejected.into();
                assert!(decode_web_task_request(&serde_json::to_vec(&value).unwrap()).is_err());
            }
            value["options"]["limits"][field] = (maximum + 1).into();
            assert!(decode_web_task_request(&serde_json::to_vec(&value).unwrap()).is_ok());
        }
    }
}
