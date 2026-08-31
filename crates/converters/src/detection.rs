//! Bounded content detection, including recognized unsupported archives.
use super::{detect_ole, detect_zip, magic_candidate, structured_text_candidate, text};
use into_markdown_core::{
    BoxFuture, ConversionError, ExecutionContext, FormatCandidate, FormatDetector, FormatHint,
    InputFormat, ResolvedInput,
};

/// Detector for file signatures and bounded inspection of ZIP/OLE containers.
#[derive(Debug, Default)]
pub struct ContentFormatDetector;

impl FormatDetector for ContentFormatDetector {
    fn id(&self) -> &'static str {
        "builtin.detector.content"
    }

    fn priority(&self) -> i32 {
        200
    }

    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            detect_content(&input.bytes, context)
        })
    }
}

pub(super) fn detect_content(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Vec<FormatCandidate>, ConversionError> {
    if let Some(signature) = into_markdown_core::RarSignature::detect(bytes) {
        return Ok(vec![FormatCandidate::new(
            InputFormat::Rar,
            1.0,
            format!("RAR signature: {signature:?}"),
        )]);
    }
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        return Ok(detect_zip(bytes));
    }
    if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
        return Ok(detect_ole(bytes));
    }
    if let Some(candidate) = magic_candidate(bytes) {
        return Ok(vec![candidate]);
    }
    if super::drawio::evidence(bytes, context)? {
        return Ok(vec![FormatCandidate::new(InputFormat::Drawio, 0.99, "Drawio graph root")]);
    }
    if let Some(candidate) = structured_text_candidate(bytes, context)? {
        return Ok(vec![candidate]);
    }
    Ok(text::sniff_unstructured_text(bytes, context)?
        .map(|confidence| {
            FormatCandidate::new(
                InputFormat::Text,
                confidence,
                "plain-text safety and encoding thresholds",
            )
        })
        .into_iter()
        .collect())
}
