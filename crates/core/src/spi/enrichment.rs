//! Transaction contracts for output enrichment and spooled summaries.

use super::{BoxFuture, ConversionSummary, ConverterOutput, OutputEnricher, Services};
use crate::{ConversionError, ConversionOptions, ExecutionContext, InputFormat};

/// Transactional post-conversion enrichment preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichmentPlan {
    /// This enricher is an exact no-op for the request and is not invoked.
    Skip,
    /// Reserve this incremental peak before invoking the enricher.
    Reserve(u64),
}

/// Result that can prove a localized runtime refusal rolled back cleanly.
#[derive(Debug)]
pub enum TransactionalEnrichmentOutcome {
    /// Enrichment completed and produced the replacement output.
    Completed(ConverterOutput),
    /// Transient work was released and the original output was retained.
    RolledBack {
        /// Original converter output at the transaction boundary.
        output: ConverterOutput,
        /// Typed runtime failure that caused the rollback.
        error: ConversionError,
    },
}

pub(super) fn default_transaction<'a, T: OutputEnricher + ?Sized>(
    enricher: &'a T,
    output: ConverterOutput,
    converter_id: &'a str,
    format: InputFormat,
    options: &'a ConversionOptions,
    services: &'a Services,
    context: &'a ExecutionContext,
) -> BoxFuture<'a, Result<TransactionalEnrichmentOutcome, ConversionError>> {
    Box::pin(async move {
        enricher
            .enrich(output, converter_id, format, options, services, context)
            .await
            .map(TransactionalEnrichmentOutcome::Completed)
    })
}

pub(super) fn summary_with_asset_counts(
    output: ConverterOutput,
    format: InputFormat,
    markdown_bytes: u64,
    content: crate::ResultContent,
    counts: [u64; 3],
) -> ConversionSummary {
    let ConverterOutput { assets, diagnostics, memory_lease, .. } = output;
    ConversionSummary {
        format: Some(format),
        outcome: crate::conversion_outcome(&diagnostics),
        diagnostics,
        markdown_bytes,
        assets: u64::try_from(assets.len()).unwrap_or(u64::MAX),
        processing_duration_ms: None,
        content: Some(content),
        payload_only_assets: counts[0],
        external_only_assets: counts[1],
        dual_representation_assets: counts[2],
        _memory_lease: memory_lease,
    }
}

impl ConverterOutput {
    /// Consume emitted output while retaining bounded completion metadata.
    #[doc(hidden)]
    #[must_use]
    pub fn into_conversion_summary(
        self,
        format: InputFormat,
        markdown_bytes: u64,
        content: crate::ResultContent,
    ) -> ConversionSummary {
        let payload_only = self
            .assets
            .iter()
            .filter(|asset| !asset.bytes.is_empty() && asset.external_uri.is_none())
            .count();
        let external_only = self
            .assets
            .iter()
            .filter(|asset| asset.bytes.is_empty() && asset.external_uri.is_some())
            .count();
        let dual = self
            .assets
            .iter()
            .filter(|asset| !asset.bytes.is_empty() && asset.external_uri.is_some())
            .count();
        self.into_conversion_summary_with_asset_counts(
            format,
            markdown_bytes,
            content,
            u64::try_from(payload_only).unwrap_or(u64::MAX),
            u64::try_from(external_only).unwrap_or(u64::MAX),
            u64::try_from(dual).unwrap_or(u64::MAX),
        )
    }

    /// Consume output whose payloads were moved to authenticated temporary files.
    #[doc(hidden)]
    #[must_use]
    pub fn into_conversion_summary_with_asset_counts(
        self,
        format: InputFormat,
        markdown_bytes: u64,
        content: crate::ResultContent,
        payload_only: u64,
        external_only: u64,
        dual: u64,
    ) -> ConversionSummary {
        summary_with_asset_counts(
            self,
            format,
            markdown_bytes,
            content,
            [payload_only, external_only, dual],
        )
    }
}
