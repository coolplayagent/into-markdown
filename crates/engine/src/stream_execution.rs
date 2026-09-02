//! Native semantic-stream admission and execution.
//!
//! Keeping this path separate makes the aggregate fallback, native producer,
//! and collector admission boundaries independently reviewable.

use crate::collecting::CollectingArtifactSink;
use crate::page_enrichment::{EMBEDDED_OCR, PageEnrichmentSink};
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, ConverterStream, ExecutionContext,
    FormatCandidate, OutputEnricher, ResolvedInput, Services, StreamConsumerKind,
    estimate_retained_output, estimate_validation_working_set,
};
use std::sync::Arc;

pub(crate) async fn invoke_native_collecting(
    converter: &dyn ConverterStream,
    input: &ResolvedInput,
    candidate: &FormatCandidate,
    options: &ConversionOptions,
    services: &Services,
    enrichers: &[Arc<dyn OutputEnricher>],
    context: &ExecutionContext,
) -> Result<NativeExecution, ConversionError> {
    invoke_native(NativeRequest {
        converter,
        input,
        candidate,
        options,
        services,
        enrichers,
        context,
        consumer: StreamConsumerKind::Collecting,
    })
    .await
}

pub(crate) async fn invoke_native_immediate(
    converter: &dyn ConverterStream,
    input: &ResolvedInput,
    candidate: &FormatCandidate,
    options: &ConversionOptions,
    services: &Services,
    enrichers: &[Arc<dyn OutputEnricher>],
    context: &ExecutionContext,
) -> Result<NativeExecution, ConversionError> {
    invoke_native(NativeRequest {
        converter,
        input,
        candidate,
        options,
        services,
        enrichers,
        context,
        consumer: StreamConsumerKind::Immediate,
    })
    .await
}

struct NativeRequest<'a> {
    converter: &'a dyn ConverterStream,
    input: &'a ResolvedInput,
    candidate: &'a FormatCandidate,
    options: &'a ConversionOptions,
    services: &'a Services,
    enrichers: &'a [Arc<dyn OutputEnricher>],
    context: &'a ExecutionContext,
    consumer: StreamConsumerKind,
}

pub(crate) struct NativeExecution {
    pub(crate) output: ConverterOutput,
    pub(crate) completed_page_ocr: bool,
}

async fn invoke_native(request: NativeRequest<'_>) -> Result<NativeExecution, ConversionError> {
    let NativeRequest {
        converter,
        input,
        candidate,
        options,
        services,
        enrichers,
        context,
        consumer,
    } = request;
    let plan = converter.planned_stream_bytes(input, candidate, options, context, consumer)?;
    let mut admission = context.reserve_memory(plan)?;
    let credited = context.with_memory_credit(&mut admission)?;
    let mut sink = CollectingArtifactSink::new(&credited);
    let page_enricher =
        enrichers.iter().find(|enricher| enricher.id() == EMBEDDED_OCR).map(AsRef::as_ref);
    let (completion, completed_page_ocr) = {
        let mut page_services = services.clone();
        if candidate.format == into_markdown_core::InputFormat::Pdf {
            page_services.ocr = services.ocr.as_ref().map(|provider| {
                Arc::new(crate::page_ocr_cache::PageOcrCache::new(Arc::clone(provider)))
                    as Arc<dyn into_markdown_core::OcrEngine>
            });
        }
        let mut pages = PageEnrichmentSink {
            destination: &mut sink,
            enricher: page_enricher,
            converter_id: converter.id(),
            format: candidate.format,
            options,
            services: &page_services,
            context: &credited,
            enrichment_attempted: false,
        };
        let completion = context
            .run(
                converter
                    .convert_stream(input, candidate, options, services, &credited, &mut pages),
            )
            .await??;
        (completion, pages.enrichment_attempted)
    };
    let output = sink.finish(completion)?;
    let retained = estimate_retained_output(&output.document, &output.assets, &output.diagnostics)?;
    let validation =
        estimate_validation_working_set(&output.document, &output.assets, &output.diagnostics)?;
    let retained_guard =
        credited.reserve_memory(retained.saturating_sub(output.leased_memory_for(&credited)))?;
    let validation_guard = credited.reserve_memory(validation)?;
    output.document.validate().map_err(|error| ConversionError::Internal {
        detail: format!(
            "native converter {} returned invalid document IR ({} at {}): {}",
            converter.id(),
            error.code.as_str(),
            error.path,
            error.detail
        ),
    })?;
    drop(validation_guard);
    drop(retained_guard);
    drop(credited);
    Ok(NativeExecution {
        output: output.certify_preflight_reservation(context, admission)?,
        completed_page_ocr,
    })
}
