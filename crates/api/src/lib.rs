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
    ArtifactKind, ArtifactReference, BusyControl, ConfigurationSnapshot, DiagnosticCode,
    InputReference, NewTask, OutputFormat, ReconcileSummary, TaskCursor, TaskDiagnostic, TaskId,
    TaskRecord, TaskStatus, TaskStore, TaskStoreError, TaskTransition,
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
        .register_source_resolver(Arc::new(
            into_markdown_converters::MediaWikiSourceResolver::default(),
        ))
        .register_source_resolver(Arc::new(into_markdown_converters::HttpSourceResolver::default()))
        .register_format_detector(Arc::new(into_markdown_converters::MediaWikiFormatDetector))
        .register_format_detector(Arc::new(into_markdown_converters::HintFormatDetector))
        .register_format_detector(Arc::new(into_markdown_converters::ContentFormatDetector))
        .register_converter(Arc::new(into_markdown_converters::NotebookConverter))
        .register_converter(Arc::new(into_markdown_converters::DocxConverter))
        .register_converter(Arc::new(into_markdown_converters::PdfConverter::default()))
        .register_converter(Arc::new(into_markdown_converters::EpubConverter))
        .register_converter(Arc::new(into_markdown_converters::ZipConverter))
        .register_converter(Arc::new(into_markdown_converters::RtfConverter))
        .register_converter(Arc::new(into_markdown_converters::WorkbookConverter))
        .register_converter(Arc::new(into_markdown_converters::PresentationConverter))
        .register_converter(Arc::new(into_markdown_converters::StructuredDataConverter))
        .register_converter(Arc::new(into_markdown_converters::FeedConverter))
        .register_converter(Arc::new(into_markdown_converters::MsgConverter))
        .register_converter(Arc::new(into_markdown_converters::MediaWikiConverter))
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
mod epub_regression_tests;

#[cfg(test)]
mod epub_tests;

#[cfg(test)]
mod zip_recursive_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::future::Future;
    use std::io::{Cursor, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    fn mediawiki_resolver_precedence_is_unambiguous_in_engine_selection() {
        let calls = Arc::new(ResolverCalls::default());
        let mut builder = EngineBuilder::new();
        builder
            .registry_mut()
            .register_source_resolver(Arc::new(
                into_markdown_converters::MediaWikiSourceResolver::default(),
            ))
            .register_source_resolver(Arc::new(ObservedFixtureResolver { calls: calls.clone() }));
        let engine = builder.build().unwrap();

        for fallback in [
            "https://example.test/assets/wiki/manual.html",
            "https://example.test/wiki/help.json",
            "https://en.wikipedia.org/docs/wiki/Rust",
            "mediawiki+https://wiki.example.test/docs/wiki/Rust",
        ] {
            let error =
                block_on(engine.convert(ConversionRequest::new(InputRef::Uri(fallback.into()))))
                    .unwrap_err();
            assert!(error.to_string().contains("external fixture resolution is forbidden"));
        }
        assert_eq!(calls.remote.load(Ordering::SeqCst), 4);

        for mediawiki in
            ["https://en.wikipedia.org/wiki/Rust", "mediawiki+https://wiki.example.test/wiki/Rust"]
        {
            let error =
                block_on(engine.convert(ConversionRequest::new(InputRef::Uri(mediawiki.into()))))
                    .unwrap_err();
            assert!(error.to_string().contains("network resolution is disabled by default"));
        }
        assert_eq!(calls.remote.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn default_engine_mediawiki_pipeline_has_exact_memory_boundary_and_source_inventory() {
        use std::io::{ErrorKind, Read, Write};
        use std::net::TcpListener;

        let body = br#"{"requestid":"Rust","curtimestamp":"2026-08-13T00:00:00Z","parse":{"title":"Rust","pageid":1,"revid":123456,"text":"<main><ul><li><p>safe list</p></li></ul><table><tr><td>safe cell</td></tr></table><p><a href=\"/wiki/Safe\">safe link</a></p></main>","sections":[],"links":[{"title":"Safe"}],"images":[]}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        )
        .into_bytes();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let mut request = [0_u8; 4096];
                        let read = stream.read(&mut request).unwrap();
                        let request = std::str::from_utf8(&request[..read]).unwrap();
                        assert!(request.starts_with("GET /w/api.php?"));
                        assert!(request.contains("requestid=Rust"));
                        stream.write_all(&response).unwrap();
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("controlled MediaWiki server failed: {error}"),
                }
            }
        });
        let engine = default_engine().unwrap();
        let source = format!("mediawiki+http://{address}/wiki/Rust");
        let run = |memory| {
            let mut request = ConversionRequest::new(InputRef::Uri(source.clone()));
            request.options.network.enabled = true;
            request.options.network.deny_private_networks = false;
            request.options.network.allowed_hosts = vec!["127.0.0.1".into()];
            request.options.limits.max_memory_bytes = memory;
            request.execution.timeout = Some(std::time::Duration::from_secs(2));
            block_on(engine.convert(request))
        };

        let (mut low, mut high) = (0_u64, 32 * 1024 * 1024_u64);
        if let Err(error) = run(high) {
            panic!("controlled MediaWiki baseline failed: {error}");
        }
        while low < high {
            let middle = low + (high - low) / 2;
            if run(middle).is_ok() {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        let exact = low;
        let result = run(exact).unwrap();
        let error = run(exact - 1).unwrap_err();
        stop.store(true, Ordering::Release);
        server.join().unwrap();

        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert!(result.markdown.contains("safe list") && result.markdown.contains("safe cell"));
        assert_eq!(
            result.document.metadata.properties.get("mediawiki.provider").map(String::as_str),
            Some("builtin.converter.mediawiki")
        );
        assert_eq!(
            result.document.metadata.properties.get("mediawiki.sourceUrl").map(String::as_str),
            Some(format!("http://{address}/wiki/Rust").as_str())
        );
        assert!(!result.provenance.is_empty());
        assert!(result.provenance.iter().all(|item| {
            item.provider == "builtin.converter.mediawiki"
                && item.locator == SourceLocator::default()
        }));
    }

    #[test]
    fn ordinary_http_json_api_path_cannot_acquire_mediawiki_resolver_identity() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let body = br#"{"ordinary":true}"#;
        for content_type in
            ["application/json", "application/json; x-into-markdown-resolver=mediawiki"]
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                std::str::from_utf8(body).unwrap()
            )
            .into_bytes();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 2048];
                let read = stream.read(&mut request).unwrap();
                let request = std::str::from_utf8(&request[..read]).unwrap();
                assert!(request.starts_with("GET /w/api.php HTTP/1.1\r\n"));
                stream.write_all(&response).unwrap();
            });

            let mut request =
                ConversionRequest::new(InputRef::Uri(format!("http://{address}/w/api.php")));
            request.options.network.enabled = true;
            request.options.network.deny_private_networks = false;
            request.options.network.allowed_hosts = vec!["127.0.0.1".into()];
            request.execution.timeout = Some(std::time::Duration::from_secs(2));
            let result = block_on(default_engine().unwrap().convert(request)).unwrap();
            server.join().unwrap();

            assert!(result.markdown.contains("ordinary"));
            assert!(!result.document.metadata.properties.contains_key("mediawiki.provider"));
            assert!(!result.provenance.is_empty());
            assert!(
                result
                    .provenance
                    .iter()
                    .all(|item| item.provider == "builtin.converter.structured-data")
            );
        }
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
    fn default_engine_converts_repository_authored_xlsb() {
        let request = ConversionRequest::new(InputRef::bytes(
            repository_authored_xlsb(),
            Some("repository-authored.xlsb"),
        ));
        let result = block_on(default_engine().unwrap().convert(request)).unwrap();
        assert!(result.markdown.contains("`=1+2 [cached: 3]`"));
        assert!(result.markdown.contains("`=binary`"));
        assert!(!result.markdown.lines().any(|line| line.starts_with("=binary")));
        assert!(result.markdown.contains("https://example.invalid/xlsb"));
        assert!(result.markdown.contains("Comment A1 (Alice): reviewed"));
        assert!(result.markdown.contains("![xlsb pixel]"));
        assert!(result.markdown.contains("2024\\-01\\-01"));
        assert_eq!(result.document.metadata.properties["spreadsheet.sheet.0.hiddenRows"], "2");
        assert_eq!(result.document.metadata.properties["spreadsheet.sheet.0.hiddenColumns"], "C");
        let Block::Sheet { blocks, .. } = &result.document.blocks[0].block else { panic!() };
        let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
        let Block::Paragraph(formula_link) = &rows[1].cells[0].blocks[0].block else { panic!() };
        assert!(matches!(
            &formula_link[0],
            Inline::Link { target, content }
                if target == "https://example.invalid/xlsb"
                    && content == &[Inline::Code("=1+2 [cached: 3]".into())]
        ));
        assert_eq!(
            result.document.metadata.properties["spreadsheet.formulaStylePolicy"],
            "codeSemanticsOverrideCellMarks"
        );
        let image = blocks.iter().find(|node| matches!(node.block, Block::Image { .. })).unwrap();
        assert_eq!(image.provenance.locator.part.as_deref(), Some("xl/drawings/drawing1.xml"));
        assert_eq!(image.provenance.locator.cell, Some(CellRef { row: 1, column: 1 }));
        assert_eq!(
            result.document.metadata.properties["spreadsheet.sheet.0.image.0.target"],
            "xl/media/pixel.png"
        );
        assert_eq!(
            result.document.metadata.properties["spreadsheet.sheet.0.image.0.relationshipId"],
            "rIdImage"
        );
        assert!(result.has_memory_lease());
    }

    #[test]
    fn default_engine_workbook_memory_threshold_is_exact() {
        let engine = default_engine().unwrap();
        let bytes = Arc::<[u8]>::from(repository_authored_xlsb());
        let attempt = |memory| {
            let mut request = ConversionRequest::new(InputRef::bytes(
                Arc::clone(&bytes),
                Some("repository-authored.xlsb"),
            ));
            request.options.limits.max_memory_bytes = memory;
            block_on(engine.convert(request))
        };
        let mut low = 0_u64;
        let mut high = ConversionOptions::default().limits.max_memory_bytes;
        while low + 1 < high {
            let middle = low + (high - low) / 2;
            if attempt(middle).is_ok() {
                high = middle;
            } else {
                low = middle;
            }
        }
        let exact = attempt(high).unwrap();
        assert!(exact.has_memory_lease());
        drop(exact);
        assert!(matches!(
            attempt(high - 1),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
    }

    #[test]
    fn default_engine_stale_xlsb_dimension_peak_precedes_calamine() {
        let engine = default_engine().unwrap();
        for actual_cell in [false, true] {
            let bytes = Arc::<[u8]>::from(repository_authored_stale_xlsb(actual_cell));
            let broad = block_on(engine.convert(ConversionRequest::new(InputRef::bytes(
                Arc::clone(&bytes),
                Some("stale-dimension.xlsb"),
            ))))
            .unwrap();
            let peak = broad.document.metadata.properties["spreadsheet.preflight.memoryPeak"]
                .parse::<u64>()
                .unwrap();
            assert!(peak >= 32_000_000);
            drop(broad);

            let exact_limit = u64::try_from(bytes.len()).unwrap().checked_add(peak).unwrap();
            let attempt = |memory| {
                let mut request = ConversionRequest::new(InputRef::bytes(
                    Arc::clone(&bytes),
                    Some("stale-dimension.xlsb"),
                ));
                request.options.limits.max_memory_bytes = memory;
                block_on(engine.convert(request))
            };
            let exact = attempt(exact_limit).unwrap();
            assert_eq!(
                exact.document.metadata.properties["spreadsheet.sheet.0.bounds"],
                if actual_cell { "A1:A1" } else { "empty" }
            );
            drop(exact);
            let low = attempt(exact_limit - 1).unwrap_err();
            assert!(matches!(
                &low,
                ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }
            ));
            assert!(format!("{low:?}").contains(&format!("{peak} >")));
        }
    }

    // This fixture is assembled from the public MS-XLSB record layout during
    // the test. It contains no third-party workbook bytes and is deterministic.
    #[allow(clippy::too_many_lines)] // Keeping the record sequence linear makes the fixture auditable.
    fn repository_authored_xlsb() -> Vec<u8> {
        let mut workbook = Vec::new();
        xlsb_record(&mut workbook, 0x0099, &0_u64.to_le_bytes()); // BrtWbProp
        let mut bundle = Vec::new();
        bundle.extend_from_slice(&0_u32.to_le_bytes()); // visible
        bundle.extend_from_slice(&1_u32.to_le_bytes());
        xlsb_string(&mut bundle, "rId1");
        xlsb_string(&mut bundle, "Binary Sheet");
        xlsb_record(&mut workbook, 0x009c, &bundle); // BrtBundleSh
        xlsb_record(&mut workbook, 0x0090, &[]); // BrtEndBundleShs
        xlsb_record(&mut workbook, 0x009d, &[]); // BrtEndBook

        let mut styles = Vec::new();
        xlsb_record(&mut styles, 0x0267, &0_u32.to_le_bytes()); // BrtBeginFmts
        xlsb_record(&mut styles, 0x0268, &[]); // BrtEndFmts
        xlsb_record(&mut styles, 0x0269, &2_u32.to_le_bytes()); // BrtBeginCellXFs
        xlsb_record(&mut styles, 0x002f, &[0; 16]);
        let mut date_xf = [0_u8; 16];
        date_xf[2..4].copy_from_slice(&14_u16.to_le_bytes());
        xlsb_record(&mut styles, 0x002f, &date_xf);
        xlsb_record(&mut styles, 0x026a, &[]); // BrtEndCellXFs

        let mut sheet = Vec::new();
        xlsb_record(&mut sheet, 0x0081, &[]); // BrtBeginSheet
        let mut dimensions = Vec::new();
        dimensions.extend_from_slice(&0_u32.to_le_bytes());
        dimensions.extend_from_slice(&1_u32.to_le_bytes());
        dimensions.extend_from_slice(&0_u32.to_le_bytes());
        dimensions.extend_from_slice(&2_u32.to_le_bytes());
        xlsb_record(&mut sheet, 0x0094, &dimensions);
        let mut column = [0_u8; 18];
        column[0..4].copy_from_slice(&2_u32.to_le_bytes());
        column[4..8].copy_from_slice(&2_u32.to_le_bytes());
        column[16..18].copy_from_slice(&1_u16.to_le_bytes());
        xlsb_record(&mut sheet, 0x003c, &column); // hidden C
        xlsb_record(&mut sheet, 0x0091, &[]); // BrtBeginSheetData
        xlsb_row(&mut sheet, 0, false);
        let mut text_cell = xlsb_cell_header(0, 0);
        xlsb_string(&mut text_cell, "Binary value");
        xlsb_record(&mut sheet, 0x0006, &text_cell); // BrtCellSt
        let mut bool_cell = xlsb_cell_header(1, 0);
        bool_cell.push(1);
        xlsb_record(&mut sheet, 0x0004, &bool_cell); // BrtCellBool
        let mut date_cell = xlsb_cell_header(2, 1);
        date_cell.extend_from_slice(&45_292_f64.to_le_bytes());
        xlsb_record(&mut sheet, 0x0005, &date_cell); // BrtCellReal
        xlsb_row(&mut sheet, 1, true);
        let mut formula = xlsb_cell_header(0, 0);
        formula.extend_from_slice(&3_f64.to_le_bytes());
        formula.extend_from_slice(&0_u16.to_le_bytes());
        let tokens = [0x1e, 1, 0, 0x1e, 2, 0, 0x03]; // 1 + 2
        formula.extend_from_slice(&u32::try_from(tokens.len()).unwrap().to_le_bytes());
        formula.extend_from_slice(&tokens);
        xlsb_record(&mut sheet, 0x0009, &formula); // BrtFmlaNum
        let mut dangerous_text = xlsb_cell_header(1, 0);
        xlsb_string(&mut dangerous_text, "=binary");
        xlsb_record(&mut sheet, 0x0006, &dangerous_text); // inert BrtCellSt
        xlsb_record(&mut sheet, 0x0092, &[]); // BrtEndSheetData
        let mut hyperlink = Vec::new();
        hyperlink.extend_from_slice(&1_u32.to_le_bytes());
        hyperlink.extend_from_slice(&1_u32.to_le_bytes());
        hyperlink.extend_from_slice(&0_u32.to_le_bytes());
        hyperlink.extend_from_slice(&0_u32.to_le_bytes());
        xlsb_string(&mut hyperlink, "rIdHyper");
        xlsb_string(&mut hyperlink, "");
        xlsb_string(&mut hyperlink, "fixture hyperlink");
        xlsb_string(&mut hyperlink, "safe");
        xlsb_record(&mut sheet, 0x01ee, &hyperlink);
        let mut drawing_id = Vec::new();
        xlsb_string(&mut drawing_id, "rIdDrawing");
        xlsb_record(&mut sheet, 0x0226, &drawing_id); // BrtDrawing
        xlsb_record(&mut sheet, 0x0082, &[]); // BrtEndSheet

        let mut comments = Vec::new();
        xlsb_record(&mut comments, 0x0274, &[]); // BrtBeginComments
        xlsb_record(&mut comments, 0x0276, &[]);
        let mut author = Vec::new();
        xlsb_string(&mut author, "Alice");
        xlsb_record(&mut comments, 0x0278, &author);
        xlsb_record(&mut comments, 0x0277, &[]);
        xlsb_record(&mut comments, 0x0279, &[]);
        let mut comment = Vec::new();
        comment.extend_from_slice(&0_u32.to_le_bytes()); // author
        comment.extend_from_slice(&0_u32.to_le_bytes()); // row first
        comment.extend_from_slice(&0_u32.to_le_bytes()); // row last
        comment.extend_from_slice(&0_u32.to_le_bytes()); // col first
        comment.extend_from_slice(&0_u32.to_le_bytes()); // col last
        comment.extend_from_slice(&[0; 16]); // guid
        xlsb_record(&mut comments, 0x027b, &comment);
        let mut comment_text = vec![1]; // RichStr with one formatting run
        xlsb_string(&mut comment_text, "reviewed");
        comment_text.extend_from_slice(&1_u32.to_le_bytes());
        comment_text.extend_from_slice(&0_u16.to_le_bytes());
        comment_text.extend_from_slice(&0_u16.to_le_bytes());
        xlsb_record(&mut comments, 0x027d, &comment_text);
        xlsb_record(&mut comments, 0x027c, &[]);
        xlsb_record(&mut comments, 0x027a, &[]);
        xlsb_record(&mut comments, 0x0275, &[]);

        let content_types = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="bin" ContentType="application/vnd.ms-excel.sheet.binary.macroEnabled.main"/><Default Extension="png" ContentType="image/png"/><Override PartName="/xl/worksheets/sheet1.bin" ContentType="application/vnd.ms-excel.worksheet"/><Override PartName="/xl/styles.bin" ContentType="application/vnd.ms-excel.styles"/><Override PartName="/xl/comments1.bin" ContentType="application/vnd.ms-excel.comments"/><Override PartName="/xl/drawings/drawing1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/></Types>"#;
        let root_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.bin"/></Relationships>"#;
        let workbook_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.bin"/></Relationships>"#;
        let sheet_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdHyper" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.invalid/xlsb" TargetMode="External"/><Relationship Id="rIdComment" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.bin"/><Relationship Id="rIdDrawing" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#;
        let drawing = r#"<?xml version="1.0"?><xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:oneCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>1</xdr:row></xdr:from><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="1" name="XLSB pixel" descr="xlsb pixel"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="rIdImage"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#;
        let drawing_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/pixel.png"/></Relationships>"#;
        let png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
            0x00, 0xb5, 0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let mut output = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut output);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, bytes) in [
                ("[Content_Types].xml", content_types.as_bytes()),
                ("_rels/.rels", root_rels.as_bytes()),
                ("xl/workbook.bin", workbook.as_slice()),
                ("xl/_rels/workbook.bin.rels", workbook_rels.as_bytes()),
                ("xl/styles.bin", styles.as_slice()),
                ("xl/worksheets/sheet1.bin", sheet.as_slice()),
                ("xl/worksheets/_rels/sheet1.bin.rels", sheet_rels.as_bytes()),
                ("xl/comments1.bin", comments.as_slice()),
                ("xl/drawings/drawing1.xml", drawing.as_bytes()),
                ("xl/drawings/_rels/drawing1.xml.rels", drawing_rels.as_bytes()),
                ("xl/media/pixel.png", png),
            ] {
                zip.start_file(name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        output.into_inner()
    }

    fn repository_authored_stale_xlsb(actual_cell: bool) -> Vec<u8> {
        let mut workbook = Vec::new();
        xlsb_record(&mut workbook, 0x0099, &0_u64.to_le_bytes());
        let mut bundle = Vec::new();
        bundle.extend_from_slice(&0_u32.to_le_bytes());
        bundle.extend_from_slice(&1_u32.to_le_bytes());
        xlsb_string(&mut bundle, "rId1");
        xlsb_string(&mut bundle, "Stale");
        xlsb_record(&mut workbook, 0x009c, &bundle);
        xlsb_record(&mut workbook, 0x0090, &[]);
        xlsb_record(&mut workbook, 0x009d, &[]);

        let mut styles = Vec::new();
        xlsb_record(&mut styles, 0x0267, &0_u32.to_le_bytes());
        xlsb_record(&mut styles, 0x0268, &[]);
        xlsb_record(&mut styles, 0x0269, &1_u32.to_le_bytes());
        xlsb_record(&mut styles, 0x002f, &[0; 16]);
        xlsb_record(&mut styles, 0x026a, &[]);

        let mut sheet = Vec::new();
        xlsb_record(&mut sheet, 0x0081, &[]);
        let dimensions = [
            0_u32.to_le_bytes(),
            (MAX_XLSB_ROW - 1).to_le_bytes(),
            0_u32.to_le_bytes(),
            (MAX_XLSB_COLUMN - 1).to_le_bytes(),
        ]
        .concat();
        xlsb_record(&mut sheet, 0x0094, &dimensions);
        xlsb_record(&mut sheet, 0x0091, &[]);
        if actual_cell {
            xlsb_record(&mut sheet, 0x0000, &[0; 17]);
            xlsb_record(&mut sheet, 0x0001, &[0; 8]);
        }
        xlsb_record(&mut sheet, 0x0092, &[]);
        xlsb_record(&mut sheet, 0x0082, &[]);

        let content_types = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="bin" ContentType="application/vnd.ms-excel.sheet.binary.macroEnabled.main"/><Override PartName="/xl/worksheets/sheet1.bin" ContentType="application/vnd.ms-excel.worksheet"/><Override PartName="/xl/styles.bin" ContentType="application/vnd.ms-excel.styles"/></Types>"#;
        let root_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.bin"/></Relationships>"#;
        let workbook_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.bin"/></Relationships>"#;
        let mut output = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut output);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, bytes) in [
                ("[Content_Types].xml", content_types.as_bytes()),
                ("_rels/.rels", root_rels.as_bytes()),
                ("xl/workbook.bin", workbook.as_slice()),
                ("xl/_rels/workbook.bin.rels", workbook_rels.as_bytes()),
                ("xl/styles.bin", styles.as_slice()),
                ("xl/worksheets/sheet1.bin", sheet.as_slice()),
            ] {
                zip.start_file(name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        output.into_inner()
    }

    const MAX_XLSB_ROW: u32 = 1_048_576;
    const MAX_XLSB_COLUMN: u32 = 16_384;

    fn xlsb_record(output: &mut Vec<u8>, typ: u16, payload: &[u8]) {
        xlsb_varint(output, u32::from(typ));
        xlsb_varint(output, u32::try_from(payload.len()).unwrap());
        output.extend_from_slice(payload);
    }

    fn xlsb_varint(output: &mut Vec<u8>, mut value: u32) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            output.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn xlsb_string(output: &mut Vec<u8>, value: &str) {
        let utf16: Vec<_> = value.encode_utf16().collect();
        output.extend_from_slice(&u32::try_from(utf16.len()).unwrap().to_le_bytes());
        for unit in utf16 {
            output.extend_from_slice(&unit.to_le_bytes());
        }
    }

    fn xlsb_cell_header(column: u32, style: u32) -> Vec<u8> {
        let mut output = column.to_le_bytes().to_vec();
        output.extend_from_slice(&style.to_le_bytes()[..3]);
        output.push(0);
        output
    }

    fn xlsb_row(output: &mut Vec<u8>, row: u32, hidden: bool) {
        let mut payload = [0_u8; 17];
        payload[0..4].copy_from_slice(&row.to_le_bytes());
        payload[8..10].copy_from_slice(&300_u16.to_le_bytes());
        if hidden {
            payload[11] = 0x10;
        }
        xlsb_record(output, 0x0000, &payload);
    }

    #[test]
    fn default_engine_detects_and_converts_all_presentation_extensions_offline() {
        let bytes = std::fs::read(fixture_path("small/pptx/normal.pptx")).unwrap();
        let engine = default_engine().unwrap();
        for extension in ["pptx", "pptm", "ppsx", "ppsm", "potx"] {
            let request = ConversionRequest::new(InputRef::bytes(
                bytes.clone(),
                Some(format!("fixture.{extension}")),
            ));
            let result = block_on(engine.convert(request)).unwrap();
            assert_eq!(
                result.markdown,
                "## Slide 1: Corpus 你好 – Привет\n\n\
                 <em>English français</em>\n\n\
                 ### Speaker notes\n\n\
                 Nota 日本語\n\n\
                 ## Slide 2: Second layout\n\n\
                 <em>مرحبا</em>\n"
            );
            assert!(result.has_memory_lease());
        }
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
