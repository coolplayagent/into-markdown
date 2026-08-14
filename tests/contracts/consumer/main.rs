//! An independently compiled downstream consumer of the public API.

use into_markdown::{
    AiCapability, AiInput, AiOutput, AiProvider, AiRequest, BoxFuture, ConversionError,
    ConversionOptions, ConversionRequest, DetectionRequest, ExecutionContext, ExecutionOptions,
    FormatDescriptor, FormatHint, FormatStatus, InputFormat, InputRef, OcrEngine, OcrRegion,
    OcrRequest, OcrResult, ResolvedInput, ResourceLimits, SourceMetadata, SourceResolver,
};
use std::collections::BTreeSet;
use std::sync::Arc;

struct LegacyResolver;

struct LegacyOcr;

struct LegacyAi;

impl AiProvider for LegacyAi {
    fn id(&self) -> &'static str {
        "contract.legacy-ai"
    }

    fn capabilities(&self) -> BTreeSet<AiCapability> {
        BTreeSet::from([AiCapability::MarkdownPostprocess])
    }

    fn execute<'a>(
        &'a self,
        _: AiRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<AiOutput, ConversionError>> {
        Box::pin(async { Ok(AiOutput::default()) })
    }
}

impl OcrEngine for LegacyOcr {
    fn id(&self) -> &'static str {
        "contract.legacy-ocr"
    }

    fn recognize<'a>(
        &'a self,
        _: OcrRequest<'a>,
        _: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
        Box::pin(async {
            Ok(OcrResult {
                regions: vec![OcrRegion {
                    text: "legacy".into(),
                    polygon: [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
                    confidence: 1.0,
                }],
                provider: "contract.legacy-ocr".into(),
            })
        })
    }
}

impl SourceResolver for LegacyResolver {
    fn id(&self) -> &'static str {
        "contract.legacy-resolver"
    }

    fn supports(&self, _: &InputRef) -> bool {
        true
    }

    fn resolve<'a>(
        &'a self,
        _: &'a InputRef,
        _: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Ok(ResolvedInput {
                bytes: Arc::from(b"consumer".as_slice()),
                // Exact legacy literal: adding any required public field to
                // SourceMetadata must fail this independently compiled consumer.
                metadata: SourceMetadata { name: None, media_type: None, uri: None, size: 8 },
            })
        })
    }
}

fn require_send_sync<T: ?Sized + Send + Sync>() {}

fn main() {
    // Exact legacy literal: catalog provenance must not add required fields to
    // the public format descriptor consumed by downstream crates.
    let format = FormatDescriptor {
        format: InputFormat::Text,
        family: "text",
        extensions: &["txt"],
        status: FormatStatus::Available,
    };
    std::hint::black_box(format);
    require_send_sync::<dyn SourceResolver>();
    let resolver: Arc<dyn SourceResolver> = Arc::new(LegacyResolver);
    let input = InputRef::bytes(b"consumer".as_slice(), Some("consumer.txt"));
    let _conversion = ConversionRequest::new(input.clone());
    let _detection = DetectionRequest::new(input);
    let _hint = FormatHint::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let options = ConversionOptions::default();
    let source = InputRef::bytes(b"x".as_slice(), None::<String>);
    // Calling the additive default method is part of the compatibility contract.
    let _future = resolver.resolve_accounted(&source, &options, &context);
    let ocr = LegacyOcr;
    let request = OcrRequest { image: b"image", media_type: "image/png", languages: &[] };
    // Exact legacy literals and the new default method must both remain valid
    // in an independently compiled downstream crate.
    let _future = ocr.recognize_bound(request, &context);
    let ai = LegacyAi;
    let request = AiRequest {
        capability: AiCapability::MarkdownPostprocess,
        input: AiInput::Markdown("legacy"),
        prompt: None,
    };
    // A legacy provider that implements only `execute` remains source-compatible,
    // while the new policy-bound default deterministically refuses unsafe opt-in.
    let _plan = ai.planned_output_bytes(request, &options, &context);
}
