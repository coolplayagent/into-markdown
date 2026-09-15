//! Enrichment inside the archive owner's already admitted request-memory credit.
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, EnrichmentPlan, ExecutionContext,
    InputFormat, OutputEnricher, Services, estimate_validation_working_set,
};
use std::sync::Arc;

pub(crate) async fn enrich(
    enrichers: &[Arc<dyn OutputEnricher>],
    mut output: ConverterOutput,
    converter_id: &str,
    format: InputFormat,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    for enricher in enrichers {
        context.checkpoint()?;
        let plan = enricher.planned_enrichment_bytes(
            &output,
            converter_id,
            format,
            options,
            services,
            context,
        )?;
        let EnrichmentPlan::Reserve(bytes) = plan else {
            if enricher.id() == crate::page_enrichment::EMBEDDED_OCR
                && !matches!(format, InputFormat::Image | InputFormat::Zip | InputFormat::Rar)
            {
                crate::page_enrichment::record_unattempted_images(&output, context);
            }
            continue;
        };
        if bytes > context.available_memory_bytes() {
            return Err(ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: format!(
                    "nested enrichment {} requires {bytes} bytes with {} available",
                    enricher.id(),
                    context.available_memory_bytes()
                ),
            });
        }
        // The enclosing converter owns the full credit. Reuse it so each concrete
        // parser, OCR worker and retained-output reservation shares that same ceiling.
        output = context
            .run(enricher.enrich(output, converter_id, format, options, services, context))
            .await??;
        let validation =
            estimate_validation_working_set(&output.document, &output.assets, &output.diagnostics)?;
        let guard = context.reserve_memory(validation)?;
        output.document.validate().map_err(|error| ConversionError::Internal {
            detail: format!("nested enricher {} returned invalid document: {error}", enricher.id()),
        })?;
        drop(guard);
        output = output.account_retained(context)?;
    }
    Ok(output)
}
