//! Enrich transient PDF pages without changing aggregate stream ownership.

use into_markdown_core::{
    Asset, ConversionError, ConversionOptions, ConverterEventSink, ConverterOutput, Document,
    EnrichmentPlan, ExecutionContext, InputFormat, LocalBoxFuture, OutputEnricher, Services,
    estimate_validation_working_set,
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

    fn enrich_page<'a>(
        &'a mut self,
        output: ConverterOutput,
    ) -> LocalBoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            self.context.checkpoint()?;
            let enricher = self.enricher.ok_or_else(|| ConversionError::Internal {
                detail: "page enrichment was not negotiated".into(),
            })?;
            let plan = enricher.planned_enrichment_bytes(
                &output,
                self.converter_id,
                self.format,
                self.options,
                self.services,
                self.context,
            )?;
            let EnrichmentPlan::Reserve(bytes) = plan else { return Ok(output) };
            // As in nested conversion, the enclosing native invocation already
            // holds the global admission. Child credits cannot back new credits.
            // Check the incremental peak before entry and use the same context
            // for all actual OCR working-set and retained-output reservations.
            if bytes > self.context.available_memory_bytes() {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: format!(
                        "page OCR planned {bytes} bytes but only {} remain",
                        self.context.available_memory_bytes()
                    ),
                });
            }
            let output = self
                .context
                .run(enricher.enrich(
                    output,
                    self.converter_id,
                    self.format,
                    self.options,
                    self.services,
                    self.context,
                ))
                .await??;
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
