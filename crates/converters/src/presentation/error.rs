use into_markdown_core::ConversionError;

pub(super) fn malformed(part: Option<&str>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: part.map(str::to_owned), detail: detail.into() }
}
pub(super) fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}
