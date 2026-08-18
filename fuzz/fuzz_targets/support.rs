use futures::executor::block_on;
use into_markdown::{
    ConversionRequest, FormatHint, InputFormat, InputRef, ResourceLimits, default_engine,
};

const MAX_FUZZ_INPUT_BYTES: usize = 1_048_576;

pub fn convert(data: &[u8], format: InputFormat, name: &str) {
    if data.len() > MAX_FUZZ_INPUT_BYTES {
        return;
    }
    let Ok(engine) = default_engine() else { return };
    let mut request = ConversionRequest::new(InputRef::bytes(data.to_vec(), Some(name)));
    request.hint = FormatHint { format: Some(format), ..FormatHint::default() };
    request.options.limits = fuzz_limits();
    let _ = block_on(engine.convert(request));
}

fn fuzz_limits() -> ResourceLimits {
    ResourceLimits {
        max_input_bytes: MAX_FUZZ_INPUT_BYTES as u64,
        max_decompressed_bytes: 8 * 1024 * 1024,
        max_archive_entries: 256,
        max_archive_depth: 4,
        max_archive_entry_bytes: 2 * 1024 * 1024,
        max_memory_bytes: 64 * 1024 * 1024,
        max_temporary_bytes: 16 * 1024 * 1024,
        max_pages: 64,
        max_asset_bytes: 4 * 1024 * 1024,
        max_total_asset_bytes: 8 * 1024 * 1024,
        max_nesting_depth: 64,
        max_table_rows: 4_096,
        max_table_columns: 256,
        max_table_cells: 65_536,
        max_field_bytes: 1024 * 1024,
        ..ResourceLimits::default()
    }
}
