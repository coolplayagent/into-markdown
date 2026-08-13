use super::*;
use into_markdown_core::{
    AiCapability, AiOutput, AiProvider, AiRequest, BoxFuture, ConversionError, Converter,
    ErrorCode, ExecutionOptions, FormatCandidate, OcrEngine, OcrRequest, OcrResult, Services,
    Transcriber, TranscriptionRequest, TranscriptionResult,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct RequestCounter(AtomicUsize);

impl RequestCounter {
    fn unexpected<T>(&self) -> BoxFuture<'static, Result<T, ConversionError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ConversionError::Internal {
                detail: "fixture converter invoked an optional service".into(),
            })
        })
    }
}

impl OcrEngine for RequestCounter {
    fn id(&self) -> &'static str {
        "fixture-request-counter"
    }

    fn recognize<'a>(
        &'a self,
        _request: OcrRequest<'a>,
        _context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        self.unexpected()
    }
}

impl Transcriber for RequestCounter {
    fn id(&self) -> &'static str {
        "fixture-request-counter"
    }

    fn transcribe<'a>(
        &'a self,
        _request: TranscriptionRequest<'a>,
        _context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<TranscriptionResult, ConversionError>> {
        self.unexpected()
    }
}

impl AiProvider for RequestCounter {
    fn id(&self) -> &'static str {
        "fixture-request-counter"
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    generator: serde_json::Value,
    available_formats: Vec<String>,
    fixtures: Vec<Fixture>,
    large_artifacts: serde_json::Value,
    ocr_quality: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    id: String,
    format: String,
    scenario: String,
    path: String,
    bytes: u64,
    sha256: String,
    media_type: String,
    license: serde_json::Value,
    provenance: serde_json::Value,
    expected: Expected,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    outcome: String,
    error_code: String,
    semantic_sha256: String,
    description: String,
    #[serde(default)]
    limit: Option<Limit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limit {
    option: String,
    failing_value: u64,
    passing_value: u64,
    #[serde(rename = "error_limit")]
    reported_name: String,
    passing_semantic_sha256: String,
}

fn manifest() -> Manifest {
    serde_json::from_str(include_str!("../../../fixtures/manifest.json"))
        .expect("fixture manifest must match the converter-side strict schema")
}

fn format(value: &str) -> InputFormat {
    match value {
        "csv" => InputFormat::Csv,
        "docx" => InputFormat::Docx,
        "feed" => InputFormat::Feed,
        "html" => InputFormat::Html,
        "ipynb" => InputFormat::Ipynb,
        "json" => InputFormat::Json,
        "markdown" => InputFormat::Markdown,
        "rtf" => InputFormat::Rtf,
        "text" => InputFormat::Text,
        "tsv" => InputFormat::Tsv,
        "xml" => InputFormat::Xml,
        unknown => panic!("fixture uses unknown converter format {unknown}"),
    }
}

fn converter(format: InputFormat) -> Box<dyn Converter> {
    match format {
        InputFormat::Text => Box::new(TextConverter),
        InputFormat::Markdown => Box::new(MarkdownConverter),
        InputFormat::Html => Box::new(HtmlConverter),
        InputFormat::Csv | InputFormat::Tsv => Box::new(DelimitedTextConverter),
        InputFormat::Json | InputFormat::Xml => Box::new(StructuredDataConverter),
        InputFormat::Ipynb => Box::new(NotebookConverter),
        InputFormat::Docx => Box::new(DocxConverter),
        InputFormat::Feed => Box::new(FeedConverter),
        InputFormat::Rtf => Box::new(RtfConverter),
        unsupported => panic!("no corpus converter for {unsupported}"),
    }
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

fn options(limit: Option<(&str, u64)>) -> ConversionOptions {
    let mut options = ConversionOptions::default();
    options.text.charset = Some("utf-8".into());
    if let Some((name, value)) = limit {
        match name {
            "max_input_bytes" => options.limits.max_input_bytes = value,
            "max_nesting_depth" => options.limits.max_nesting_depth = u16::try_from(value).unwrap(),
            "max_table_columns" => options.limits.max_table_columns = value,
            unknown => panic!("unsupported fixture limit option {unknown}"),
        }
    }
    options
}

fn execute(
    fixture: &Fixture,
    limit: Option<(&str, u64)>,
) -> Result<String, into_markdown_core::ConversionError> {
    let fixture_root = std::env::var_os("TEST_SRCDIR").map_or_else(
        || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"),
        |runfiles| {
            std::path::PathBuf::from(runfiles)
                .join(std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "into_markdown".into()))
                .join("fixtures")
        },
    );
    let bytes = std::fs::read(fixture_root.join(&fixture.path)).unwrap();
    assert_eq!(u64::try_from(bytes.len()).unwrap(), fixture.bytes, "{}", fixture.id);
    assert_eq!(hex(&bytes), fixture.sha256, "{}", fixture.id);
    let format = format(&fixture.format);
    let options = options(limit);
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let input = ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata {
            name: Some(fixture.id.clone()),
            media_type: Some(fixture.media_type.clone()),
            uri: None,
            size: fixture.bytes,
        },
    };
    let requests = Arc::new(RequestCounter::default());
    let services = Services {
        ocr: Some(requests.clone()),
        transcriber: Some(requests.clone()),
        ai: Some(requests.clone()),
        nested: None,
    };
    let converted = block_on(converter(format).convert(
        &input,
        &FormatCandidate::explicit(format),
        &options,
        &services,
        &context,
    ));
    assert_eq!(
        requests.0.load(Ordering::SeqCst),
        0,
        "fixture {} attempted an optional service request",
        fixture.id
    );
    let output = converted?;
    into_markdown_render_markdown::render(&output.document, &output.assets, &options)
}

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn corpus_available_formats_match_the_product_registry() {
    let manifest = manifest();
    assert_eq!(manifest.schema_version, 1);
    let _ = (&manifest.generator, &manifest.large_artifacts, &manifest.ocr_quality);
    let actual: BTreeSet<_> = planned_formats()
        .iter()
        .filter(|descriptor| descriptor.status == FormatStatus::Available)
        .map(|descriptor| descriptor.format.as_str())
        .collect();
    let declared: BTreeSet<_> = manifest.available_formats.iter().map(String::as_str).collect();
    assert_eq!(declared.len(), manifest.available_formats.len(), "duplicate available format");
    assert_eq!(declared, actual, "fixture corpus drifted from the product format registry");
}

#[test]
fn corpus_contracts_execute_through_real_converters() {
    let mut failures = Vec::new();
    for fixture in manifest().fixtures {
        if fixture.format == "ocr-image" {
            continue;
        }
        let _ = (
            &fixture.scenario,
            &fixture.license,
            &fixture.provenance,
            &fixture.expected.description,
        );
        if let Some(limit) = &fixture.expected.limit {
            let Ok(error) = execute(&fixture, Some((&limit.option, limit.failing_value)))
                .map(|_| None)
                .or_else(|error| Ok::<_, ()>(Some(error)))
            else {
                unreachable!()
            };
            let Some(error) = error else {
                failures.push(format!("{} failing boundary unexpectedly succeeded", fixture.id));
                continue;
            };
            if error.code() != ErrorCode::ResourceLimit {
                failures.push(format!("{} failing boundary returned {error}", fixture.id));
                continue;
            }
            match &error {
                into_markdown_core::ConversionError::ResourceLimit { limit: actual, .. } => {
                    if *actual != limit.reported_name {
                        failures.push(format!(
                            "{} limit name {actual:?} != {:?}",
                            fixture.id, limit.reported_name
                        ));
                    }
                }
                _ => unreachable!(),
            }
            match execute(&fixture, Some((&limit.option, limit.passing_value))) {
                Ok(markdown) => {
                    let actual = hex(markdown.as_bytes());
                    if actual != limit.passing_semantic_sha256 {
                        failures.push(format!(
                            "{} passing semantic {actual}, markdown={markdown:?}",
                            fixture.id
                        ));
                    }
                }
                Err(error) => failures.push(format!(
                    "{} passing boundary {} failed: {error}",
                    fixture.id, limit.passing_value
                )),
            }
            continue;
        }
        match fixture.expected.outcome.as_str() {
            "success" => match execute(&fixture, None) {
                Ok(markdown) => {
                    let actual = hex(markdown.as_bytes());
                    if actual != fixture.expected.semantic_sha256 {
                        failures.push(format!(
                            "{} semantic {actual}, markdown={markdown:?}",
                            fixture.id
                        ));
                    }
                }
                Err(error) => failures.push(format!("{} failed: {error}", fixture.id)),
            },
            "error" => match execute(&fixture, None) {
                Ok(markdown) => {
                    failures.push(format!("{} unexpectedly succeeded: {markdown:?}", fixture.id));
                }
                Err(error) if error.code().as_str() == fixture.expected.error_code => {}
                Err(error) => failures.push(format!(
                    "{} error {} != {}: {error}",
                    fixture.id,
                    error.code().as_str(),
                    fixture.expected.error_code
                )),
            },
            unknown => panic!("{} has unknown outcome {unknown}", fixture.id),
        }
    }
    assert!(failures.is_empty(), "corpus contract failures:\n{}", failures.join("\n"));
}
