use super::error::limit;
use into_markdown_core::ConversionError;

pub(super) fn try_clone_string(value: &str, purpose: &str) -> Result<String, ConversionError> {
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|error| limit("max_memory_bytes", format!("cannot reserve {purpose}: {error}")))?;
    output.push_str(value);
    Ok(output)
}

pub(super) fn try_clone_bytes(value: &[u8], purpose: &str) -> Result<Vec<u8>, ConversionError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|error| limit("max_memory_bytes", format!("cannot reserve {purpose}: {error}")))?;
    output.extend_from_slice(value);
    Ok(output)
}
