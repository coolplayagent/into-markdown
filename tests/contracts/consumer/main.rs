//! An independently compiled downstream consumer of the public API.

use into_markdown::{
    BoxFuture, ConversionError, ConversionOptions, ConversionRequest, DetectionRequest,
    ExecutionContext, ExecutionOptions, FormatHint, InputRef, ResolvedInput, ResourceLimits,
    SourceMetadata, SourceResolver,
};
use std::sync::Arc;

struct LegacyResolver;

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
                metadata: SourceMetadata::default(),
            })
        })
    }
}

fn require_send_sync<T: ?Sized + Send + Sync>() {}

fn main() {
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
}
