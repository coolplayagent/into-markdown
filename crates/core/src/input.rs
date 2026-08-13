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
}

/// Resolver-specific provenance carried beside the source without changing
/// the source-compatible [`SourceMetadata`] struct layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceResolutionMetadata {
    /// Redacted, canonical HTTP redirects in request order.
    pub redirects: Vec<SourceRedirect>,
}

/// One HTTP redirect recorded without user information, query, or fragment.
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
    retained_metadata_memory: Option<ResourceReservation>,
    resolution_metadata: SourceResolutionMetadata,
}

impl ResolvedSource {
    /// Wrap resolver output that has not carried a source-memory reservation.
    #[must_use]
    pub fn new(input: ResolvedInput) -> Self {
        Self {
            input,
            memory_reservation: None,
            retained_metadata_memory: None,
            resolution_metadata: SourceResolutionMetadata::default(),
        }
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
        Self {
            input,
            memory_reservation: Some(memory_reservation),
            retained_metadata_memory: None,
            resolution_metadata: SourceResolutionMetadata::default(),
        }
    }

    /// Retain one additional resolver-owned lease for source metadata and
    /// resolver state that must outlive the resolution call.
    ///
    /// Unlike the source-buffer lease, this reservation is deliberately not
    /// resized to the byte length of `input.bytes` by
    /// [`Self::ensure_memory_reservation`]. This additive method does not
    /// change the source-compatible [`ResolvedInput`] layout.
    #[must_use]
    pub fn with_retained_metadata_memory(mut self, reservation: ResourceReservation) -> Self {
        self.retained_metadata_memory = Some(reservation);
        self
    }

    /// Attach resolver-specific provenance without changing `ResolvedInput`.
    #[must_use]
    pub fn with_resolution_metadata(mut self, metadata: SourceResolutionMetadata) -> Self {
        self.resolution_metadata = metadata;
        self
    }

    /// Borrow resolved bytes and metadata.
    #[must_use]
    pub fn input(&self) -> &ResolvedInput {
        &self.input
    }

    /// Borrow resolver-specific provenance retained at the accounted resolver boundary.
    #[must_use]
    pub fn resolution_metadata(&self) -> &SourceResolutionMetadata {
        &self.resolution_metadata
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
        if let Some(retained) = self.retained_metadata_memory.as_ref()
            && !retained.belongs_to_memory_context(context)
        {
            let replacement = context.reserve_memory(retained.bytes())?;
            self.retained_metadata_memory = Some(replacement);
        }
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

    #[test]
    fn resolution_provenance_is_a_compatible_resolved_source_sidecar() {
        let input = ResolvedInput {
            bytes: Arc::from(b"data".as_slice()),
            metadata: SourceMetadata {
                name: None,
                media_type: None,
                uri: Some("https://example.test/final".into()),
                size: 4,
            },
        };
        let source =
            ResolvedSource::new(input).with_resolution_metadata(SourceResolutionMetadata {
                redirects: vec![SourceRedirect {
                    from: "https://example.test/start".into(),
                    to: "https://example.test/final".into(),
                    status: 302,
                }],
            });
        assert_eq!(source.resolution_metadata().redirects.len(), 1);
        assert_eq!(source.input().metadata.size, 4);
    }

    #[test]
    fn retained_metadata_lease_is_not_shrunk_to_source_bytes() {
        let context = context(12);
        let source_memory = context.reserve_memory(4).unwrap();
        let metadata_memory = context.reserve_memory(8).unwrap();
        let input = ResolvedInput {
            bytes: Arc::from(b"data".as_slice()),
            metadata: SourceMetadata { size: 4, ..SourceMetadata::default() },
        };
        let mut source = ResolvedSource::with_memory_reservation(input, source_memory)
            .with_retained_metadata_memory(metadata_memory);

        source.ensure_memory_reservation(&context).unwrap();
        assert_eq!(context.reserved_memory_bytes(), 12);
        assert!(context.reserve_memory(1).is_err());
        drop(source);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn foreign_retained_metadata_lease_is_reauthenticated_before_use() {
        let foreign = context(12);
        let current = context(12);
        let input = ResolvedInput {
            bytes: Arc::from(b"data".as_slice()),
            metadata: SourceMetadata { size: 4, ..SourceMetadata::default() },
        };
        let mut source =
            ResolvedSource::with_memory_reservation(input, foreign.reserve_memory(4).unwrap())
                .with_retained_metadata_memory(foreign.reserve_memory(8).unwrap());

        source.ensure_memory_reservation(&current).unwrap();
        assert_eq!(foreign.reserved_memory_bytes(), 0);
        assert_eq!(current.reserved_memory_bytes(), 12);
        drop(source);
        assert_eq!(current.reserved_memory_bytes(), 0);
    }
}
