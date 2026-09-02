//! Enrich transient PDF pages without changing aggregate stream ownership.

mod policy;
use into_markdown_core::{
    Asset, ConversionError, ConversionOptions, ConverterEventSink, ConverterOutput, Document,
    EnrichmentPlan, ExecutionContext, InputFormat, LocalBoxFuture, OutputEnricher, Services,
    TransactionalEnrichmentOutcome, estimate_validation_working_set,
};

pub(crate) const EMBEDDED_OCR: &str = "builtin.enricher.embedded-visual-ocr";

pub(crate) struct PageEnrichmentSink<'a> {
    pub(crate) destination: &'a mut dyn ConverterEventSink,
    pub(crate) enricher: Option<&'a dyn OutputEnricher>,
    pub(crate) converter_id: &'a str,
    pub(crate) format: InputFormat,
    pub(crate) options: &'a ConversionOptions,
    pub(crate) services: &'a Services,
    pub(crate) context: &'a ExecutionContext,
    pub(crate) enrichment_attempted: bool,
}

impl ConverterEventSink for PageEnrichmentSink<'_> {
    fn checkpoint(&mut self) -> Result<(), ConversionError> {
        self.destination.checkpoint()
    }

    fn write_output(
        &mut self,
        document: Document,
        assets: Vec<Asset>,
    ) -> Result<(), ConversionError> {
        self.destination.write_output(document, assets)
    }

    fn supports_page_enrichment(&self) -> bool {
        self.enricher.is_some() && self.format == InputFormat::Pdf
    }

    fn enrich_page(
        &mut self,
        mut output: ConverterOutput,
    ) -> LocalBoxFuture<'_, Result<ConverterOutput, ConversionError>> {
        self.enrichment_attempted = true;
        Box::pin(async move {
            self.context.checkpoint()?;
            let enricher = self.enricher.ok_or_else(|| ConversionError::Internal {
                detail: "page enrichment was not negotiated".into(),
            })?;
            // As in nested conversion, the enclosing native invocation already
            // holds the global admission. Child credits cannot back new credits.
            // The transient page is a rollback boundary: a local OCR preflight
            // refusal can retain the untouched native page and continue.
            let plan = enricher
                .planned_enrichment_bytes(
                    &output,
                    self.converter_id,
                    self.format,
                    self.options,
                    self.services,
                    self.context,
                )
                .and_then(|plan| {
                    if let EnrichmentPlan::Reserve(bytes) = plan {
                        let available = self.context.available_memory_bytes();
                        if bytes > available {
                            return Err(ConversionError::ResourceLimit {
                                limit: "max_memory_bytes",
                                detail: format!(
                                    "page OCR planned {bytes} bytes but only {available} remain"
                                ),
                            });
                        }
                    }
                    Ok(plan)
                });
            let plan = match plan {
                Ok(plan) => plan,
                Err(error)
                    if policy::recovery(&output, self, enricher, &error)
                        == into_markdown_core::ResourceRecoveryAction::OmitUnit =>
                {
                    policy::push_omitted(&mut output, &error)?;
                    return Ok(output);
                }
                Err(error) => return Err(error),
            };
            let EnrichmentPlan::Reserve(_) = plan else { return Ok(output) };
            let outcome = self
                .context
                .run(enricher.enrich_transactionally(
                    output,
                    self.converter_id,
                    self.format,
                    self.options,
                    self.services,
                    self.context,
                ))
                .await??;
            let output = match outcome {
                TransactionalEnrichmentOutcome::Completed(output) => output,
                TransactionalEnrichmentOutcome::RolledBack { mut output, error }
                    if policy::recovery(&output, self, enricher, &error)
                        == into_markdown_core::ResourceRecoveryAction::OmitUnit =>
                {
                    policy::push_omitted(&mut output, &error)?;
                    return Ok(output);
                }
                TransactionalEnrichmentOutcome::RolledBack { error, .. } => return Err(error),
            };
            let validation = estimate_validation_working_set(
                &output.document,
                &output.assets,
                &output.diagnostics,
            )?;
            let _validation = self.context.reserve_memory(validation)?;
            output.document.validate().map_err(|error| ConversionError::Internal {
                detail: format!("page OCR returned invalid IR at {}: {}", error.path, error.detail),
            })?;
            output.account_retained(self.context)
        })
    }
}

#[cfg(test)]
#[path = "page_enrichment_tests.rs"]
mod tests;
