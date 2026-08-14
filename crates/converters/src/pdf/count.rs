use super::{ConversionError, resource};

pub(super) fn checked_count(
    current: usize,
    added: usize,
    maximum: usize,
    limit: &'static str,
) -> Result<usize, ConversionError> {
    let total = current.checked_add(added).ok_or_else(|| resource(limit, "count overflow"))?;
    if total > maximum {
        return Err(resource(limit, format!("{total} > {maximum}")));
    }
    Ok(total)
}
