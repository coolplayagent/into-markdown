//! Converter-to-engine semantic streaming contracts.
//!
//! This module owns the one-shot stream protocol so the general service-provider
//! interface in `spi` does not keep growing with engine execution details.

use crate::{
    Asset, ConversionError, ConversionOptions, Converter, ConverterOutput, Diagnostic, Document,
    ExecutionContext, FormatCandidate, ResolvedInput, Services,
};
use std::future::Future;
use std::pin::Pin;

/// Thread-local boxed future used by synchronous stream callbacks.
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Retention behavior of the engine-side stream consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamConsumerKind {
    /// Consume artifacts without retaining a backwards-compatible result.
    Immediate,
    /// Retain the semantic stream as a backwards-compatible aggregate result.
    Collecting,
}

/// How a converter participates in semantic streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConverterStreamMode {
    /// Run the source-compatible aggregate converter and move its output into the stream.
    AggregateAdapter,
    /// Run the converter's native one-shot stream entry point.
    Native,
}

/// Optional native semantic-stream capability implemented independently from
/// the general converter SPI.
pub trait ConverterStream: Converter {
    /// Declare whether this converter has a native one-shot semantic stream.
    fn stream_mode(&self) -> ConverterStreamMode {
        ConverterStreamMode::AggregateAdapter
    }

    /// Select the stream mode for this exact input and consumer.
    fn stream_mode_for(
        &self,
        input: &ResolvedInput,
        candidate: &FormatCandidate,
        options: &ConversionOptions,
        consumer: StreamConsumerKind,
    ) -> ConverterStreamMode {
        let _ = (input, candidate, options, consumer);
        self.stream_mode()
    }

    /// Conservative peak for the selected semantic stream producer.
    ///
    /// # Errors
    ///
    /// Returns a stable component or resource error when no safe stream plan
    /// can be declared for this request.
    fn planned_stream_bytes(
        &self,
        input: &ResolvedInput,
        candidate: &FormatCandidate,
        options: &ConversionOptions,
        context: &ExecutionContext,
        consumer: StreamConsumerKind,
    ) -> Result<u64, ConversionError> {
        let _ = consumer;
        self.planned_output_bytes(input, candidate, options, context)
    }

    /// Convert through the owned semantic stream.
    fn convert_stream<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
        sink: &'a mut dyn ConverterEventSink,
    ) -> LocalBoxFuture<'a, Result<ConverterStreamCompletion, ConversionError>> {
        let _ = (input, candidate, options, services, context, sink);
        Box::pin(async move {
            Err(ConversionError::Unsupported {
                detail: format!("converter {} requires aggregate adaptation", self.id()),
            })
        })
    }
}

/// Synchronous destination for owned converter events.
pub trait ConverterEventSink {
    /// Whether this sink can enrich an owned transient page before its pixels
    /// are released. This does not change the single `write_output` contract.
    fn supports_page_enrichment(&self) -> bool {
        false
    }

    /// Enrich one unpublished page using the enclosing converter's memory credit.
    /// The returned output owns its diagnostics and authenticated leases; the
    /// producer must consume it before extracting the next transient page.
    fn enrich_page<'a>(
        &'a mut self,
        output: ConverterOutput,
    ) -> LocalBoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            drop(output);
            Err(ConversionError::Unsupported {
                detail: "semantic sink does not support page enrichment".into(),
            })
        })
    }

    /// Observe cancellation or timeout before the next allocation or callback.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, or sink-defined failure.
    fn checkpoint(&mut self) -> Result<(), ConversionError> {
        Ok(())
    }

    /// Take ownership of the complete semantic document and asset inventory.
    /// Existing nested Vec allocations and capacities must be preserved rather
    /// than copied into a second collector inventory.
    ///
    /// # Errors
    ///
    /// Returns a protocol, resource, cancellation, or destination failure.
    fn write_output(
        &mut self,
        document: Document,
        assets: Vec<Asset>,
    ) -> Result<(), ConversionError>;
}

/// Terminal converter state after every semantic event was accepted.
///
/// The residual output retains diagnostics and authenticated memory ownership;
/// document and asset ownership has already moved to the sink.
#[derive(Debug)]
pub struct ConverterStreamCompletion {
    residual: ConverterOutput,
}

impl ConverterStreamCompletion {
    /// Reassemble the converter output without losing its authenticated leases.
    #[doc(hidden)]
    #[must_use]
    pub fn into_output(mut self, document: Document, assets: Vec<Asset>) -> ConverterOutput {
        self.residual.document = document;
        self.residual.assets = assets;
        self.residual
    }

    /// Borrow terminal diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.residual.diagnostics
    }
}

/// Move one aggregate converter output through the canonical semantic stream.
///
/// No document node, asset payload, or diagnostic is cloned. This is the sole
/// aggregate adapter used by the engine and by native converters that share an
/// existing parser while adopting the single-execution stream contract.
///
/// # Errors
///
/// Returns the first sink or cancellation error.
#[doc(hidden)]
pub fn stream_converter_output(
    mut output: ConverterOutput,
    sink: &mut dyn ConverterEventSink,
) -> Result<ConverterStreamCompletion, ConversionError> {
    sink.checkpoint()?;
    let document = std::mem::take(&mut output.document);
    let assets = std::mem::take(&mut output.assets);
    sink.write_output(document, assets)?;
    Ok(ConverterStreamCompletion { residual: output })
}
