//! Public façade for the `into-markdown` conversion platform.

use std::sync::Arc;

pub use into_markdown_ai::{
    AiProviderDescriptor, GenerationEndpoint, GenerationInput, GenerationRequest, GenerationResult,
    OpenAiCompatibleClient, OpenAiCompatibleConfig, ProviderConfig, ProviderError,
    ProviderErrorCode, ProviderNetworkPolicy, ProviderTestResult,
};
pub use into_markdown_converters::{FormatDescriptor, FormatStatus};
pub use into_markdown_core::*;
pub use into_markdown_engine::{
    Engine, EngineBuilder, RecoveryStore, RecoveryToken, RegistryBuilder, TaskCheckpoint, TaskPhase,
};
pub use into_markdown_ocr::{
    AcquiredModelArtifact, ArchiveMember, CharacterSet, DataDirectoryEnvironment, ModelAcquisition,
    ModelArtifact, ModelBundle, ModelFetcher, ModelManager, ModelManagerError, ModelManifest,
    ModelStatus, ProductTarget, RuntimeArtifact, model_data_directory,
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
        nested: None,
    };
    let mut builder = EngineBuilder::new()
        .renderer(Arc::new(into_markdown_render_markdown::GfmRenderer))
        .services(services);
    builder
        .registry_mut()
        .register_source_resolver(Arc::new(into_markdown_converters::MemorySourceResolver))
        .register_source_resolver(Arc::new(into_markdown_converters::LocalFileSourceResolver))
        .register_source_resolver(Arc::new(into_markdown_converters::StdinSourceResolver))
        .register_source_resolver(Arc::new(into_markdown_converters::HttpSourceResolver::default()))
        .register_format_detector(Arc::new(into_markdown_converters::HintFormatDetector))
        .register_format_detector(Arc::new(into_markdown_converters::ContentFormatDetector))
        .register_converter(Arc::new(into_markdown_converters::NotebookConverter))
        .register_converter(Arc::new(into_markdown_converters::DocxConverter))
        .register_converter(Arc::new(into_markdown_converters::PdfConverter::default()))
        .register_converter(Arc::new(into_markdown_converters::ZipConverter))
        .register_converter(Arc::new(into_markdown_converters::RtfConverter))
        .register_converter(Arc::new(into_markdown_converters::StructuredDataConverter))
        .register_converter(Arc::new(into_markdown_converters::FeedConverter))
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
mod zip_recursive_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::future::Future;
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
    fn authorized_loopback_uri_runs_resolver_detector_converter_and_renderer() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..read]).unwrap();
            assert!(request.starts_with("GET /input.txt?signed=canary HTTP/1.1\r\n"));
            assert!(request.contains(&format!("\r\nHost: {address}\r\n")));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Disposition: attachment; filename=input.txt\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello\n")
                .unwrap();
        });

        let mut request = ConversionRequest::new(InputRef::Uri(format!(
            "http://{address}/input.txt?signed=canary"
        )));
        request.options.network.enabled = true;
        request.options.network.deny_private_networks = false;
        request.options.network.allowed_hosts = vec!["127.0.0.1".into()];
        request.hint.format = Some(InputFormat::Text);
        request.execution.timeout = Some(std::time::Duration::from_secs(2));
        let result = block_on(default_engine().unwrap().convert(request)).unwrap();
        server.join().unwrap();
        assert_eq!(result.markdown, "hello\n");
    }

    #[test]
    fn default_engine_detects_and_converts_rtf_offline() {
        let engine = default_engine().unwrap();
        let data: Arc<[u8]> = Arc::from(&b"{\\rtf1\\ansi API \\u20013?\\u25991?\\par}"[..]);
        let input = InputRef::bytes(data, Some("sample.rtf"));
        let result = block_on(engine.convert(ConversionRequest::new(input))).unwrap();
        assert_eq!(result.markdown, "API 中文\n");
        assert!(result.provenance.iter().all(|record| record.provider == "builtin.converter.rtf"));
    }

    #[test]
    fn external_docx_link_never_resolves_or_invokes_optional_services() {
        let resolver_calls = Arc::new(ResolverCalls::default());
        let service_calls = Arc::new(ServiceCalls::default());
        let services = Services {
            ocr: Some(service_calls.clone()),
            transcriber: Some(service_calls.clone()),
            ai: Some(service_calls.clone()),
            nested: None,
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

    #[test]
    #[ignore = "requires PDFIUM_LIBRARY pointing to the pinned current-target runtime"]
    fn native_pdf_runs_through_engine_and_emits_internal_page_anchor() {
        assert!(std::env::var_os("PDFIUM_LIBRARY").is_some());
        let request = ConversionRequest::new(InputRef::bytes(engine_pdf(), Some("fixture.pdf")));
        let result = block_on(default_engine().unwrap().convert(request)).unwrap();
        assert!(result.markdown.contains("#pdf-page-2"));
        assert!(result.markdown.contains("<a id=\"pdf-page-2\"></a>"));
        assert!(result.has_memory_lease());
    }

    fn engine_pdf() -> Vec<u8> {
        let content = stream_object("", b"BT /F1 12 Tf 10 60 Td (Engine PDF) Tj ET\n");
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 6 0 R >> >> /Contents 5 0 R /Annots [7 0 R] >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 6 0 R >> >> /Contents 5 0 R >>".to_vec(),
            content,
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            b"<< /Type /Annot /Subtype /Link /Rect [10 10 50 30] /Dest [4 0 R /Fit] >>".to_vec(),
        ];
        assemble_pdf(&objects)
    }

    fn stream_object(dictionary: &str, bytes: &[u8]) -> Vec<u8> {
        let mut object =
            format!("<< {dictionary} /Length {} >>\nstream\n", bytes.len()).into_bytes();
        object.extend_from_slice(bytes);
        object.extend_from_slice(b"\nendstream");
        object
    }

    fn assemble_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n%\x80\x80\x80\x80\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }
}
