use super::validate_external_uri;
use into_markdown_core::{Asset, AssetMode, ConversionError, ConversionOptions, Document};

pub(super) fn validate_document(document: &Document) -> Result<(), ConversionError> {
    document.validate().map_err(|error| ConversionError::Internal {
        detail: format!(
            "asset planner received invalid document IR ({} at {}): {}",
            error.code.as_str(),
            error.path,
            error.detail
        ),
    })
}

pub(super) fn validate_empty_asset(
    asset: &Asset,
    options: &ConversionOptions,
    id: &str,
) -> Result<(), ConversionError> {
    if options.output.asset_mode == AssetMode::Omit {
        Ok(())
    } else {
        validate_external_uri(asset.external_uri.as_deref(), id)
    }
}
