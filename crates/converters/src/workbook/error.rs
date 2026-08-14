use into_markdown_core::{ConversionError, Diagnostic, DiagnosticSeverity, SourceLocator};

pub(super) fn warning(code: &str, message: String, locator: Option<SourceLocator>) -> Diagnostic {
    Diagnostic { code: code.into(), severity: DiagnosticSeverity::Warning, message, locator }
}

pub(super) fn malformed(part: Option<&str>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: part.map(str::to_owned), detail: detail.into() }
}

pub(super) fn limit(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}

pub(super) fn map_calamine(label: &str, error: impl std::fmt::Debug) -> ConversionError {
    let detail = format!("{error:?}");
    if detail.to_ascii_lowercase().contains("password")
        || detail.to_ascii_lowercase().contains("encrypted")
    {
        ConversionError::Encrypted
    } else {
        malformed(None, format!("{label} parser: {detail}"))
    }
}
