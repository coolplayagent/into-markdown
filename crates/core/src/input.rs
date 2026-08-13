use crate::{ConversionError, ExecutionContext, InputFormat, ResourceReservation};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Source supplied to the conversion engine.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InputRef {
    /// Local filesystem path.
    Path(PathBuf),
    /// In-memory bytes with an optional display name.
    Bytes {
        /// Immutable source bytes.
        data: Arc<[u8]>,
        /// Optional display filename used only as a format hint.
        name: Option<String>,
    },
    /// Standard input.
    Stdin,
    /// Remote or special-purpose URI.
    Uri(String),
}

impl InputRef {
    /// Construct an in-memory source without forcing a second copy.
    #[must_use]
    pub fn bytes(data: impl Into<Arc<[u8]>>, name: Option<impl Into<String>>) -> Self {
        Self::Bytes { data: data.into(), name: name.map(Into::into) }
    }
}

/// Caller-provided and source-derived hints used by format detectors.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatHint {
    /// Explicit format selection. This takes precedence over inference.
    pub format: Option<InputFormat>,
    /// Filename, when known.
    pub filename: Option<String>,
    /// Extension with or without a leading dot.
    pub extension: Option<String>,
    /// MIME media type.
    pub media_type: Option<String>,
    /// Character encoding name, when supplied by the caller.
    pub charset: Option<String>,
}

/// Metadata recorded by a source resolver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMetadata {
    /// Stable display name, never interpreted as a filesystem path.
    pub name: Option<String>,
    /// MIME media type if supplied by a trusted source.
    pub media_type: Option<String>,
    /// Original URI when resolution was explicitly enabled.
    pub uri: Option<String>,
    /// Byte length after resolution.
    pub size: u64,
    /// Redacted, canonical HTTP redirect provenance in request order.
    pub redirects: Vec<SourceRedirect>,
}

/// One HTTP redirect recorded without credentials, query, or fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRedirect {
    /// Redacted canonical source URL.
    pub from: String,
    /// Redacted canonical destination URL.
    pub to: String,
    /// Redirect response status.
    pub status: u16,
}

/// Seek-independent bytes passed from source resolution into detection and
/// conversion.
#[derive(Debug, Clone)]
pub struct ResolvedInput {
    /// Complete input bytes.
    pub bytes: Arc<[u8]>,
    /// Trusted metadata attached by the resolver.
    pub metadata: SourceMetadata,
}

/// Resolver output plus an optional request-scoped source-memory lease.
///
/// Existing resolver implementations can keep returning [`ResolvedInput`]
/// from [`crate::SourceResolver::resolve`]. Engines use the additive
/// `resolve_accounted` hook to retain built-in resolver accounting across the
/// resolver boundary without changing the layout of [`ResolvedInput`].
pub struct ResolvedSource {
    input: ResolvedInput,
    memory_reservation: Option<ResourceReservation>,
}

impl ResolvedSource {
    /// Wrap resolver output that has not carried a source-memory reservation.
    #[must_use]
    pub fn new(input: ResolvedInput) -> Self {
        Self { input, memory_reservation: None }
    }

    /// Wrap resolver output with its request-scoped memory reservation.
    ///
    /// The engine verifies the reservation's context identity and exact byte
    /// count before treating it as the source-buffer charge.
    #[must_use]
    pub fn with_memory_reservation(
        input: ResolvedInput,
        memory_reservation: ResourceReservation,
    ) -> Self {
        Self { input, memory_reservation: Some(memory_reservation) }
    }

    /// Borrow resolved bytes and metadata.
    #[must_use]
    pub fn input(&self) -> &ResolvedInput {
        &self.input
    }

    /// Consume the accounting wrapper and return its ordinary resolver output.
    ///
    /// This is intended for compatibility implementations of `resolve`; the
    /// carried reservation is released as the wrapper is consumed.
    #[must_use]
    pub fn into_input(self) -> ResolvedInput {
        self.input
    }

    /// Ensure the complete source bytes are charged to this exact request.
    ///
    /// Existing reservations from another request are never accepted as a
    /// budget credential. A same-request reservation is resized to the exact
    /// byte length before the engine continues.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, arithmetic, or memory-budget error.
    pub fn ensure_memory_reservation(
        &mut self,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        let bytes =
            u64::try_from(self.input.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "resolved input size cannot be represented as u64".into(),
            })?;
        if let Some(reservation) = self.memory_reservation.as_mut() {
            if reservation.accounts_memory_for(context, bytes) {
                return Ok(());
            }
            if reservation.belongs_to_memory_context(context) {
                let held = reservation.bytes();
                if held < bytes {
                    reservation.grow(bytes - held)?;
                } else {
                    reservation.shrink(held - bytes)?;
                }
                debug_assert!(reservation.accounts_memory_for(context, bytes));
                return Ok(());
            }
        }
        let replacement = context.reserve_memory(bytes)?;
        self.memory_reservation = Some(replacement);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecutionOptions, ResourceLimits};

    fn context(max_memory_bytes: u64) -> ExecutionContext {
        ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes, ..ResourceLimits::default() },
        )
    }

    #[test]
    fn source_reservation_is_exact_and_bound_to_one_context() {
        let foreign = context(4);
        let current = context(4);
        let foreign_reservation = foreign.reserve_memory(4).unwrap();
        let input = ResolvedInput {
            bytes: Arc::from(b"data".as_slice()),
            metadata: SourceMetadata { size: 4, ..SourceMetadata::default() },
        };
        let mut source = ResolvedSource::with_memory_reservation(input, foreign_reservation);

        source.ensure_memory_reservation(&current).unwrap();
        assert!(foreign.reserve_memory(4).is_ok());
        assert_eq!(current.reserve_memory(1).unwrap_err().code(), crate::ErrorCode::ResourceLimit);
        drop(source);
        assert!(current.reserve_memory(4).is_ok());
    }

    #[test]
    fn undersized_same_context_reservation_is_not_a_budget_credential() {
        let context = context(4);
        let reservation = context.reserve_memory(0).unwrap();
        let input = ResolvedInput {
            bytes: Arc::from(b"data".as_slice()),
            metadata: SourceMetadata { size: 4, ..SourceMetadata::default() },
        };
        let mut source = ResolvedSource::with_memory_reservation(input, reservation);
        source.ensure_memory_reservation(&context).unwrap();
        assert_eq!(context.reserve_memory(1).unwrap_err().code(), crate::ErrorCode::ResourceLimit);
    }

    #[test]
    fn legacy_resolved_input_literal_remains_source_compatible() {
        let input = ResolvedInput {
            bytes: Arc::from(b"legacy".as_slice()),
            metadata: SourceMetadata::default(),
        };
        assert_eq!(&*input.bytes, b"legacy");
    }
}
