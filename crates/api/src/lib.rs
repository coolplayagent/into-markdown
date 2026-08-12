//! Public façade for the `into-markdown` conversion platform.

use std::sync::Arc;

pub use into_markdown_ai::{AiProviderDescriptor, OpenAiCompatibleConfig};
pub use into_markdown_converters::{FormatDescriptor, FormatStatus};
pub use into_markdown_core::*;
pub use into_markdown_engine::{Engine, EngineBuilder, RegistryBuilder};
pub use into_markdown_ocr::{
    CharacterSet, DataDirectoryEnvironment, ModelArtifact, ModelBundle, ModelManager,
    ModelManagerError, ModelManifest, ModelStatus, ProductTarget, RuntimeArtifact,
    model_data_directory,
};
pub use into_markdown_render_markdown::{asset_filename, render as render_markdown};

/// Create the standard builder with safe local source resolvers, hint
/// detection, the deterministic GFM renderer, and non-networking provider seams.
/// No production converter is registered by the scaffold.
#[must_use]
pub fn default_engine_builder() -> EngineBuilder {
    let services = into_markdown_core::Services {
        ocr: Some(Arc::new(into_markdown_ocr::PlaceholderOcrEngine)),
        transcriber: Some(Arc::new(into_markdown_ai::PlaceholderTranscriber)),
        ai: Some(Arc::new(into_markdown_ai::PlaceholderAiProvider)),
    };
    let mut builder = EngineBuilder::new()
        .renderer(Arc::new(into_markdown_render_markdown::GfmRenderer))
        .services(services);
    builder
        .registry_mut()
        .register_source_resolver(Arc::new(into_markdown_converters::MemorySourceResolver))
        .register_source_resolver(Arc::new(into_markdown_converters::LocalFileSourceResolver))
        .register_source_resolver(Arc::new(into_markdown_converters::StdinSourceResolver))
        .register_source_resolver(Arc::new(into_markdown_converters::UriSourceResolver))
        .register_format_detector(Arc::new(into_markdown_converters::HintFormatDetector))
        .register_format_detector(Arc::new(into_markdown_converters::ContentFormatDetector));
    builder
}

/// Build the standard scaffold engine.
///
/// # Errors
///
/// Returns [`ConversionError::Internal`] when built-in component registration
/// violates an engine invariant.
pub fn default_engine() -> Result<Engine, ConversionError> {
    default_engine_builder().build()
}

/// Planned converter capabilities.
#[must_use]
pub fn planned_formats() -> &'static [FormatDescriptor] {
    into_markdown_converters::planned_formats()
}

/// Planned AI/plugin adapters.
#[must_use]
pub fn planned_ai_providers() -> &'static [AiProviderDescriptor] {
    into_markdown_ai::planned_providers()
}

/// Parse and validate the embedded default OCR model manifest.
///
/// # Errors
///
/// Returns [`ConversionError::Internal`] when the embedded supply-chain
/// manifest cannot be parsed or validated.
pub fn model_manifest() -> Result<ModelManifest, ConversionError> {
    into_markdown_ocr::ModelManifest::embedded()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_engine_builds_without_converters() {
        assert!(default_engine().is_ok());
    }
}
