use crate::{
    BoxFuture, ConversionError, ConversionOptions, ConverterOutput, ExecutionContext, FormatHint,
    ResolvedInput,
};

/// One already-resolved input submitted by a container converter.
///
/// The request borrows the caller's bytes and policy. Implementations must not
/// resolve the source again or broaden any authority from `options`.
#[derive(Debug, Clone, Copy)]
pub struct NestedConversionRequest<'a> {
    /// Bounded bytes and safe metadata for the container member.
    pub input: &'a ResolvedInput,
    /// Member-derived format hints.
    pub hint: &'a FormatHint,
    /// The root conversion policy, including offline and resource limits.
    pub options: &'a ConversionOptions,
    /// Converter IDs which must not be selected for this dispatch.
    pub excluded_converter_ids: &'a [&'a str],
}

/// Engine-owned dispatch seam used by recursive container converters.
///
/// The caller supplies the root [`ExecutionContext`], so cancellation,
/// timeout, memory, and temporary-storage authority remain request scoped.
pub trait NestedConversionService: Send + Sync {
    /// Detect and convert one already-resolved container member.
    fn convert<'a>(
        &'a self,
        request: NestedConversionRequest<'a>,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>>;
}
