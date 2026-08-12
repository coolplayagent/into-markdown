use crate::InputFormat;
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
#[derive(Debug, Clone)]
pub struct ResolvedInput {
    /// Complete input bytes.
    pub bytes: Arc<[u8]>,
    /// Trusted metadata attached by the resolver.
    pub metadata: SourceMetadata,
}
