//! Engine-owned semantic output collection.
//!
//! The collector adopts converter-owned allocations. It never creates a
//! second block, table-row, asset, or payload inventory.

use into_markdown_core::{
    Asset, ConversionError, ConverterEventSink, ConverterOutput, ConverterStreamCompletion,
    Document, ExecutionContext,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitOutput,
    Finished,
}

pub(crate) struct CollectingArtifactSink<'a> {
    context: &'a ExecutionContext,
    phase: Phase,
    document: Option<Document>,
    assets: Option<Vec<Asset>>,
}

impl<'a> CollectingArtifactSink<'a> {
    pub(crate) fn new(context: &'a ExecutionContext) -> Self {
        Self { context, phase: Phase::AwaitOutput, document: None, assets: None }
    }

    pub(crate) fn finish(
        mut self,
        completion: ConverterStreamCompletion,
    ) -> Result<ConverterOutput, ConversionError> {
        self.context.checkpoint()?;
        if self.phase != Phase::Finished {
            return Err(protocol("semantic stream ended before its owned output"));
        }
        let document =
            self.document.take().ok_or_else(|| protocol("semantic document is missing"))?;
        let assets =
            self.assets.take().ok_or_else(|| protocol("semantic asset inventory is missing"))?;
        Ok(completion.into_output(document, assets))
    }
}

impl ConverterEventSink for CollectingArtifactSink<'_> {
    fn checkpoint(&mut self) -> Result<(), ConversionError> {
        self.context.checkpoint()
    }

    fn write_output(
        &mut self,
        document: Document,
        assets: Vec<Asset>,
    ) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        if self.phase == Phase::Finished {
            return Err(protocol("semantic stream repeated its owned output"));
        }
        self.document = Some(document);
        self.assets = Some(assets);
        self.phase = Phase::Finished;
        Ok(())
    }
}

fn protocol(detail: &str) -> ConversionError {
    ConversionError::Internal { detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        AssetId, Block, BlockNode, ConverterOutput, Diagnostic, DiagnosticSeverity,
        DocumentMetadata, ExecutionOptions, NodeId, Provenance, ProvenanceKind, ResourceLimits,
        SourceLocator, stream_converter_output,
    };

    fn node(index: usize) -> BlockNode {
        BlockNode {
            id: NodeId(format!("node-{index}")),
            block: Block::Rule,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: "test.collecting".into(),
                locator: SourceLocator::default(),
                confidence: None,
            },
        }
    }

    fn source(schema_version: u32, blocks: usize) -> ConverterOutput {
        ConverterOutput::new(
            Document {
                schema_version,
                metadata: DocumentMetadata { title: Some("exact".into()), ..Default::default() },
                blocks: (0..blocks).map(node).collect(),
            },
            vec![Asset {
                id: AssetId("asset".into()),
                filename: Some("asset.bin".into()),
                media_type: "application/octet-stream".into(),
                bytes: vec![0, 1, 2, 255],
                external_uri: None,
            }],
            vec![Diagnostic {
                code: "exact".into(),
                severity: DiagnosticSeverity::Warning,
                message: "preserved".into(),
                locator: None,
            }],
        )
    }

    #[test]
    fn adapter_preserves_document_assets_diagnostics_and_capacities_without_growth() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let expected = source(999, 32_768);
        let block_capacity = expected.document.blocks.capacity();
        let block_pointer = expected.document.blocks.as_ptr();
        let asset_capacity = expected.assets.capacity();
        let asset_pointer = expected.assets.as_ptr();
        let asset_bytes_pointer = expected.assets[0].bytes.as_ptr();
        let diagnostic_pointer = expected.diagnostics.as_ptr();
        let mut sink = CollectingArtifactSink::new(&context);
        let completion = stream_converter_output(expected, &mut sink).unwrap();
        let output = sink.finish(completion).unwrap();
        assert_eq!(output.document.schema_version, 999);
        assert_eq!(output.document.blocks.len(), 32_768);
        assert_eq!(output.document.blocks.capacity(), block_capacity);
        assert_eq!(output.document.blocks.as_ptr(), block_pointer);
        assert_eq!(output.assets.capacity(), asset_capacity);
        assert_eq!(output.assets.as_ptr(), asset_pointer);
        assert_eq!(output.assets[0].bytes.as_ptr(), asset_bytes_pointer);
        assert_eq!(output.diagnostics.as_ptr(), diagnostic_pointer);
        assert_eq!(output.diagnostics[0].code, "exact");
    }

    #[test]
    fn cancellation_and_protocol_failure_never_finalize() {
        let cancellation = into_markdown_core::CancellationToken::new();
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation: cancellation.clone(), ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let mut sink = CollectingArtifactSink::new(&context);
        cancellation.cancel();
        let mut output = source(1, 1);
        let document = std::mem::take(&mut output.document);
        let assets = std::mem::take(&mut output.assets);
        assert!(matches!(sink.write_output(document, assets), Err(ConversionError::Cancelled)));

        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let mut sink = CollectingArtifactSink::new(&context);
        let mut first = source(1, 0);
        sink.write_output(std::mem::take(&mut first.document), std::mem::take(&mut first.assets))
            .unwrap();
        let mut second = source(1, 0);
        assert!(matches!(
            sink.write_output(
                std::mem::take(&mut second.document),
                std::mem::take(&mut second.assets),
            ),
            Err(ConversionError::Internal { .. })
        ));
    }
}

#[cfg(test)]
#[path = "collecting/integration_tests.rs"]
mod integration_tests;
