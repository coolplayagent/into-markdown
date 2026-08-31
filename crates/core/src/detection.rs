//! Format detection service contracts.

use crate::{
    BoxFuture, ConversionError, ExecutionContext, FormatCandidate, FormatHint, InputFormat,
    ResolvedInput,
};

/// Evidence supplied by a detector, independent of its numerical confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionAuthority {
    /// Filename, media type, or decoding metadata.
    Hint,
    /// Content inspection, including signature-less text formats.
    Content,
    /// Complete JSON/XML or validated delimited structure.
    StructuredText,
    /// Bounded prefixes, markup, or plain-text safety heuristics.
    Heuristic,
    /// Identity recovered from the internal structure of a binary container.
    Container,
    /// A binary signature, still subject to parser validation.
    Signature,
}

/// Internal routing evidence. Public detection/result DTOs remain unchanged.
#[derive(Debug)]
pub struct FormatDetection {
    /// Format hypotheses from this detector.
    pub candidates: Vec<FormatCandidate>,
    /// The kind of evidence shared by these hypotheses.
    pub authority: DetectionAuthority,
    /// Format hints compatible with an outer container whose subtype remains unresolved.
    pub compatible_hints: Vec<InputFormat>,
    /// A support-boundary rejection used when no other content detector accepts the input.
    pub unsupported_reason: Option<String>,
}

/// Produce format hypotheses from bytes, metadata, and explicit hints.
pub trait FormatDetector: Send + Sync {
    /// Stable implementation ID.
    fn id(&self) -> &'static str;
    /// Detector priority; larger values run first.
    fn priority(&self) -> i32 {
        0
    }
    /// Detect zero or more candidates.
    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        hint: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>>;

    /// Supply routing authority without changing existing detector implementations.
    /// Custom detectors default to content evidence and can accept additional inputs.
    fn detect_with_authority<'a>(
        &'a self,
        input: &'a ResolvedInput,
        hint: &'a FormatHint,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<FormatDetection, ConversionError>> {
        Box::pin(async move {
            Ok(FormatDetection {
                candidates: self.detect(input, hint, context).await?,
                authority: DetectionAuthority::Content,
                compatible_hints: Vec::new(),
                unsupported_reason: None,
            })
        })
    }
}
