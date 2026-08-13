//! Public façade for the `into-markdown` conversion platform.

use std::sync::Arc;

pub use into_markdown_ai::{AiProviderDescriptor, OpenAiCompatibleConfig};
pub use into_markdown_converters::{FormatDescriptor, FormatStatus};
pub use into_markdown_core::*;
pub use into_markdown_engine::{
    Engine, EngineBuilder, RecoveryStore, RecoveryToken, RegistryBuilder, TaskCheckpoint, TaskPhase,
};
pub use into_markdown_ocr::{
    CharacterSet, DataDirectoryEnvironment, ModelArtifact, ModelBundle, ModelFetcher, ModelManager,
    ModelManagerError, ModelManifest, ModelStatus, ProductTarget, RuntimeArtifact,
    model_data_directory,
};
pub use into_markdown_render_markdown::{
    AssetPlan, PlannedAsset, PlannedAssetReference, asset_filename, plan_assets,
    render as render_markdown,
};
pub use into_markdown_task_store::{
    ArtifactKind, ArtifactReference, BusyControl, ConfigurationSnapshot, DiagnosticCode, NewTask,
    OutputFormat, ReconcileSummary, TaskCursor, TaskDiagnostic, TaskId, TaskRecord, TaskStatus,
    TaskStore, TaskStoreError, TaskTransition,
};

/// Create the standard builder with safe local source resolvers, hint
/// detection, the deterministic GFM renderer, plain-text conversion, and
/// non-networking provider seams.
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
        .register_format_detector(Arc::new(into_markdown_converters::ContentFormatDetector))
        .register_converter(Arc::new(into_markdown_converters::NotebookConverter))
        .register_converter(Arc::new(into_markdown_converters::DocxConverter))
        .register_converter(Arc::new(into_markdown_converters::StructuredDataConverter))
        .register_converter(Arc::new(into_markdown_converters::HtmlConverter))
        .register_converter(Arc::new(into_markdown_converters::MarkdownConverter))
        .register_converter(Arc::new(into_markdown_converters::DelimitedTextConverter))
        .register_converter(Arc::new(into_markdown_converters::TextConverter));
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
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct ResolverCalls {
        local: AtomicUsize,
        remote: AtomicUsize,
    }

    struct ObservedFixtureResolver {
        calls: Arc<ResolverCalls>,
    }

    impl SourceResolver for ObservedFixtureResolver {
        fn id(&self) -> &'static str {
            "test.source.observed-fixture"
        }

        fn supports(&self, input: &InputRef) -> bool {
            matches!(input, InputRef::Path(_) | InputRef::Uri(_))
        }

        fn resolve<'a>(
            &'a self,
            input: &'a InputRef,
            _options: &'a ConversionOptions,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
            Box::pin(async move {
                context.checkpoint()?;
                match input {
                    InputRef::Path(path) => {
                        self.calls.local.fetch_add(1, Ordering::SeqCst);
                        let bytes =
                            std::fs::read(path).map_err(|error| ConversionError::Internal {
                                detail: format!("cannot read observed fixture: {error}"),
                            })?;
                        Ok(ResolvedInput {
                            metadata: SourceMetadata {
                                name: path
                                    .file_name()
                                    .and_then(|value| value.to_str())
                                    .map(str::to_owned),
                                size: u64::try_from(bytes.len()).map_err(|_| {
                                    ConversionError::ResourceLimit {
                                        limit: "max_input_bytes",
                                        detail: "fixture size cannot be represented as u64".into(),
                                    }
                                })?,
                                ..SourceMetadata::default()
                            },
                            bytes: Arc::from(bytes),
                        })
                    }
                    InputRef::Uri(_) => {
                        self.calls.remote.fetch_add(1, Ordering::SeqCst);
                        Err(ConversionError::Network {
                            detail: "external fixture resolution is forbidden".into(),
                        })
                    }
                    _ => Err(ConversionError::Unsupported {
                        detail: "observed fixture resolver accepts only paths and URIs".into(),
                    }),
                }
            })
        }
    }

    #[derive(Default)]
    struct ServiceCalls(AtomicUsize);

    impl ServiceCalls {
        fn unexpected<T>(&self) -> BoxFuture<'static, Result<T, ConversionError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(ConversionError::Internal {
                    detail: "external DOCX invoked an optional service".into(),
                })
            })
        }
    }

    impl OcrEngine for ServiceCalls {
        fn id(&self) -> &'static str {
            "test.service.observed"
        }

        fn recognize<'a>(
            &'a self,
            _request: OcrRequest<'a>,
            _context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
            self.unexpected()
        }
    }

    impl Transcriber for ServiceCalls {
        fn id(&self) -> &'static str {
            "test.service.observed"
        }

        fn transcribe<'a>(
            &'a self,
            _request: TranscriptionRequest<'a>,
            _context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
            self.unexpected()
        }
    }

    impl AiProvider for ServiceCalls {
        fn id(&self) -> &'static str {
            "test.service.observed"
        }

        fn capabilities(&self) -> BTreeSet<AiCapability> {
            BTreeSet::from([
                AiCapability::VisionOcr,
                AiCapability::AudioTranscription,
                AiCapability::MarkdownPostprocess,
            ])
        }

        fn execute<'a>(
            &'a self,
            _request: AiRequest<'a>,
            _context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
            self.unexpected()
        }
    }

    fn fixture_path(relative: &str) -> PathBuf {
        std::env::var_os("TEST_SRCDIR").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(relative),
            |runfiles| {
                PathBuf::from(runfiles)
                    .join(
                        std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "into_markdown".into()),
                    )
                    .join("fixtures")
                    .join(relative)
            },
        )
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn default_engine_builds_with_builtin_converters() {
        assert!(default_engine().is_ok());
    }

    #[test]
    fn external_docx_link_never_resolves_or_invokes_optional_services() {
        let resolver_calls = Arc::new(ResolverCalls::default());
        let service_calls = Arc::new(ServiceCalls::default());
        let services = Services {
            ocr: Some(service_calls.clone()),
            transcriber: Some(service_calls.clone()),
            ai: Some(service_calls.clone()),
        };
        let mut builder = EngineBuilder::new()
            .renderer(Arc::new(into_markdown_render_markdown::GfmRenderer))
            .services(services);
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(ObservedFixtureResolver {
                calls: resolver_calls.clone(),
            }))
            .register_format_detector(Arc::new(into_markdown_converters::HintFormatDetector))
            .register_format_detector(Arc::new(into_markdown_converters::ContentFormatDetector))
            .register_converter(Arc::new(into_markdown_converters::DocxConverter));
        let engine = builder.build().expect("observed fixture engine");
        let result = block_on(engine.convert(ConversionRequest::new(InputRef::Path(
            fixture_path("small/docx/malicious.docx"),
        ))))
        .expect("external-link fixture must convert without resolution");

        assert_eq!(
            result.markdown,
            "[safe external link](<https://example.invalid/fixture-link>)\n"
        );
        assert_eq!(resolver_calls.local.load(Ordering::SeqCst), 1);
        assert_eq!(resolver_calls.remote.load(Ordering::SeqCst), 0);
        assert_eq!(service_calls.0.load(Ordering::SeqCst), 0);
    }
}
