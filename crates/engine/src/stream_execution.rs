//! Native semantic-stream admission and execution.
//!
//! Keeping this path separate makes the aggregate fallback, native producer,
//! and collector admission boundaries independently reviewable.

use crate::collecting::CollectingArtifactSink;
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, ConverterStream, ExecutionContext,
    FormatCandidate, ResolvedInput, Services, StreamConsumerKind, estimate_retained_output,
    estimate_validation_working_set,
};

pub(crate) async fn invoke_native_collecting(
    converter: &dyn ConverterStream,
    input: &ResolvedInput,
    candidate: &FormatCandidate,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    invoke_native(
        converter,
        input,
        candidate,
        options,
        services,
        context,
        StreamConsumerKind::Collecting,
    )
    .await
}

pub(crate) async fn invoke_native_immediate(
    converter: &dyn ConverterStream,
    input: &ResolvedInput,
    candidate: &FormatCandidate,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    invoke_native(
        converter,
        input,
        candidate,
        options,
        services,
        context,
        StreamConsumerKind::Immediate,
    )
    .await
}

async fn invoke_native(
    converter: &dyn ConverterStream,
    input: &ResolvedInput,
    candidate: &FormatCandidate,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
    consumer: StreamConsumerKind,
) -> Result<ConverterOutput, ConversionError> {
    let plan = converter.planned_stream_bytes(input, candidate, options, context, consumer)?;
    let mut admission = context.reserve_memory(plan)?;
    let credited = context.with_memory_credit(&mut admission)?;
    let mut sink = CollectingArtifactSink::new(&credited);
    let completion = context
        .run(converter.convert_stream(input, candidate, options, services, &credited, &mut sink))
        .await??;
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
    output.certify_preflight_reservation(context, admission)
}
