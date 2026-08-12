use crate::{ConversionError, ExecutionContext, InputFormat, ResourceReservation};
use std::fmt;
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
}

/// Seek-independent bytes passed from source resolution into detection and
/// conversion.
#[derive(Clone)]
pub struct ResolvedInput {
    /// Complete input bytes.
    pub bytes: Arc<[u8]>,
    /// Trusted metadata attached by the resolver.
    pub metadata: SourceMetadata,
    memory_reservation: Option<ResourceReservation>,
}

impl ResolvedInput {
    /// Construct resolver output whose memory has not yet been charged.
    ///
    /// The engine validates and charges these bytes at the resolver boundary.
    #[must_use]
    pub fn new(bytes: Arc<[u8]>, metadata: SourceMetadata) -> Self {
        Self { bytes, metadata, memory_reservation: None }
    }

    /// Construct resolver output with a request-scoped memory reservation.
    ///
    /// The engine verifies the reservation's context identity and exact byte
    /// count before treating it as the source-buffer charge.
    #[must_use]
    pub fn with_memory_reservation(
        bytes: Arc<[u8]>,
        metadata: SourceMetadata,
        memory_reservation: ResourceReservation,
    ) -> Self {
        Self { bytes, metadata, memory_reservation: Some(memory_reservation) }
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
            u64::try_from(self.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "resolved input size cannot be represented as u64".into(),
            })?;
        if let Some(reservation) = &self.memory_reservation {
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

impl fmt::Debug for ResolvedInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedInput")
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .field("metadata", &self.metadata)
            .field("memory_reserved", &self.memory_reservation.is_some())
            .finish()
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
        let mut input = ResolvedInput::with_memory_reservation(
            Arc::from(b"data".as_slice()),
            SourceMetadata { size: 4, ..SourceMetadata::default() },
            foreign_reservation,
        );

        input.ensure_memory_reservation(&current).unwrap();
        assert!(foreign.reserve_memory(4).is_ok());
        assert_eq!(current.reserve_memory(1).unwrap_err().code(), crate::ErrorCode::ResourceLimit);
        let cloned = input.clone();
        drop(input);
        assert_eq!(current.reserve_memory(1).unwrap_err().code(), crate::ErrorCode::ResourceLimit);
        drop(cloned);
        assert!(current.reserve_memory(4).is_ok());
    }

    #[test]
    fn undersized_same_context_reservation_is_not_a_budget_credential() {
        let context = context(4);
        let reservation = context.reserve_memory(0).unwrap();
        let mut input = ResolvedInput::with_memory_reservation(
            Arc::from(b"data".as_slice()),
            SourceMetadata { size: 4, ..SourceMetadata::default() },
            reservation,
        );
        input.ensure_memory_reservation(&context).unwrap();
        assert_eq!(context.reserve_memory(1).unwrap_err().code(), crate::ErrorCode::ResourceLimit);
    }
}
