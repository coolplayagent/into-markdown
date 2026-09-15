use super::*;

pub(super) fn required_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    part: &str,
) -> Result<&'a Map<String, Value>, ConversionError> {
    object
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(part, format!("{key} must be an object")))
}

pub(super) fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    part: &str,
) -> Result<&'a str, ConversionError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(part, format!("{key} must be a string")))
}

pub(super) fn required_u64(
    object: &Map<String, Value>,
    key: &str,
    part: &str,
) -> Result<u64, ConversionError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed(part, format!("{key} must be a non-negative integer")))
}
