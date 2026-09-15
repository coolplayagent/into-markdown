use super::*;

pub(super) fn validate_diagnostics(diagnostics: &[Diagnostic]) -> Result<(), ConversionError> {
    if diagnostics.len() > into_markdown_core::MAX_DTO_DIAGNOSTICS {
        return Err(recovery_error("limit", "checkpoint contains too many diagnostics"));
    }
    for diagnostic in diagnostics {
        if diagnostic.code.is_empty() || diagnostic.code.chars().any(char::is_control) {
            return Err(recovery_error(
                "corrupt",
                "checkpoint diagnostic code must be non-empty and control-free",
            ));
        }
        if let Some(locator) = &diagnostic.locator {
            validate_locator(locator)?;
        }
    }
    Ok(())
}
