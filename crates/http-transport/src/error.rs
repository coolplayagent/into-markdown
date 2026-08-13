use into_markdown_core::ConversionError;

/// Stable transport failure category. It intentionally carries no URL or server payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportErrorKind {
    /// Network access was not explicitly authorized.
    NetworkDenied,
    /// The URL or HTTP response grammar is invalid.
    InvalidMessage,
    /// The target host is outside the effective allowlist.
    HostDenied,
    /// A private or otherwise non-global target lacks separate authorization.
    PrivateNetworkDenied,
    /// DNS failed, returned an invalid set, or its bounded queue was exhausted.
    Dns,
    /// No checked address accepted a connection before the deadline.
    Connect,
    /// TLS configuration, authentication, or handshake failed.
    Tls,
    /// A strict HTTP status or redirect rule was violated.
    Http,
    /// A source or transport resource limit was exceeded.
    ResourceLimit,
    /// The request was cancelled.
    Cancelled,
    /// The total request deadline elapsed.
    Timeout,
    /// The platform cannot provide the audited transport primitive.
    Unavailable,
}

/// Sanitized transport error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportError {
    kind: TransportErrorKind,
}

impl TransportError {
    pub(crate) fn new(kind: TransportErrorKind) -> Self {
        Self { kind }
    }

    /// Return the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> TransportErrorKind {
        self.kind
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.kind {
            TransportErrorKind::NetworkDenied => "network access denied",
            TransportErrorKind::InvalidMessage => "invalid HTTP message",
            TransportErrorKind::HostDenied => "HTTP host denied",
            TransportErrorKind::PrivateNetworkDenied => "private network access denied",
            TransportErrorKind::Dns => "DNS resolution failed",
            TransportErrorKind::Connect => "HTTP connection failed",
            TransportErrorKind::Tls => "TLS authentication failed",
            TransportErrorKind::Http => "HTTP response rejected",
            TransportErrorKind::ResourceLimit => "HTTP resource limit exceeded",
            TransportErrorKind::Cancelled => "HTTP request cancelled",
            TransportErrorKind::Timeout => "HTTP request timed out",
            TransportErrorKind::Unavailable => "HTTP transport unavailable",
        })
    }
}

impl std::error::Error for TransportError {}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn map_context_error(error: ConversionError) -> TransportError {
    TransportError::new(match error {
        ConversionError::Cancelled => TransportErrorKind::Cancelled,
        ConversionError::Timeout => TransportErrorKind::Timeout,
        ConversionError::ResourceLimit { .. } => TransportErrorKind::ResourceLimit,
        _ => TransportErrorKind::Unavailable,
    })
}
