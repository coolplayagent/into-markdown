//! Compatibility-result sink used by `Engine::convert`.

use into_markdown_core::{ArtifactSink, AssetStreamInfo, ConversionError, ConversionResult};

#[derive(Default)]
pub(crate) struct CollectingResultSink {
    result: Option<ConversionResult>,
}

impl CollectingResultSink {
    pub(crate) fn write_result(&mut self, result: ConversionResult) -> Result<(), ConversionError> {
        if self.result.is_some() {
            return Err(protocol("collecting sink received multiple terminal results"));
        }
        self.result = Some(result);
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<ConversionResult, ConversionError> {
        self.result.ok_or_else(|| protocol("collecting sink finalized without a result"))
    }
}

impl ArtifactSink for CollectingResultSink {
    fn write_markdown(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        Err(protocol("collecting sink received replayed Markdown"))
    }

    fn begin_asset(&mut self, _: &AssetStreamInfo) -> Result<(), ConversionError> {
        Err(protocol("collecting sink received a replayed asset"))
    }

    fn write_asset(&mut self, _: &[u8]) -> Result<(), ConversionError> {
        Err(protocol("collecting sink received replayed asset bytes"))
    }

    fn end_asset(&mut self) -> Result<(), ConversionError> {
        Err(protocol("collecting sink received a replayed asset terminator"))
    }
}

fn protocol(detail: &str) -> ConversionError {
    ConversionError::Internal { detail: detail.into() }
}
