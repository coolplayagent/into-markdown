//! Prepared single-execution conversion for caller-owned artifact sinks.

use crate::{
    Engine, PreparedArtifactConversion, artifact_output, invoke_converter_preflighted,
    invoke_enrichers, rendering, stream_execution,
};
use into_markdown_core::{
    ArtifactSink, ConversionError, ConversionSummary, ConverterStreamMode, ExecutionStage,
    StreamConsumerKind,
};

pub(crate) async fn execute(
    engine: &Engine,
    prepared: PreparedArtifactConversion,
    sink: &mut dyn ArtifactSink,
) -> Result<ConversionSummary, ConversionError> {
    if sink.capabilities() != prepared.capabilities {
        return Err(ConversionError::Internal {
            detail: "artifact sink capabilities changed after preparation".into(),
        });
    }
    let PreparedArtifactConversion { inner, capabilities } = prepared;
    let crate::preparation::PreparedConversion {
        request,
        context,
        source,
        attempt,
        preparation_duration,
    } = inner;
    let execution_timer = crate::timing::ProcessingTimer::start();
    context.report(ExecutionStage::Converting, None, None, Some(attempt.converter.id()))?;
    let native = attempt.converter.stream_support().filter(|stream| {
        stream.stream_mode_for(
            source.input(),
            &attempt.candidate,
            &request.options,
            StreamConsumerKind::Immediate,
        ) == ConverterStreamMode::Native
    });
    let output = if let Some(stream) = native {
        stream_execution::invoke_native_immediate(
            stream,
            source.input(),
            &attempt.candidate,
            &request.options,
            &engine.services,
            &context,
        )
        .await?
    } else {
        invoke_converter_preflighted(
            attempt.converter.as_ref(),
            source.input(),
            &attempt.candidate,
            &request.options,
            &engine.services,
            &context,
            |_| Ok(()),
        )
        .await?
    };
    let output = invoke_enrichers(
        &engine.enrichers,
        output,
        attempt.converter.id(),
        attempt.candidate.format,
        &request.options,
        &engine.services,
        &context,
    )
    .await?;
    let output = crate::result_policy::attach_evidence(output, &context)?;
    let renderer = engine.renderer.as_ref().ok_or_else(|| ConversionError::Internal {
        detail: "no Markdown renderer is registered".into(),
    })?;
    let artifacts = rendering::render_artifacts(rendering::RenderRequest {
        renderer: renderer.as_ref(),
        output,
        source: source.input(),
        format: attempt.candidate.format,
        options: &request.options,
        context: &context,
    })
    .await?;
    let processing_duration = preparation_duration.saturating_add(execution_timer.elapsed());
    let mut summary =
        artifact_output::emit(artifacts, attempt.candidate.format, capabilities, sink, &context)?;
    summary.processing_duration_ms = Some(processing_duration.as_secs_f64() * 1_000.0);
    drop(source);
    context.report(ExecutionStage::Completed, Some(1), Some(1), None::<String>)?;
    Ok(summary)
}
