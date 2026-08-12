use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable machine-readable conversion error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ErrorCode {
    /// No subsystem recognizes the input.
    Unsupported,
    /// The format is known but no converter is registered.
    NoConverter,
    /// A recognized input is structurally unusable.
    Malformed,
    /// The input is encrypted or password protected.
    Encrypted,
    /// A resource or safety budget was exceeded.
    ResourceLimit,
    /// Local OCR failed.
    Ocr,
    /// An AI provider failed or returned an invalid result.
    Ai,
    /// A permitted network operation failed.
    Network,
    /// Local input/output failed.
    Io,
    /// A recognized optional component is unavailable.
    ComponentUnavailable,
    /// The caller cancelled conversion.
    Cancelled,
    /// An invariant or unavailable internal component prevented conversion.
    Internal,
}

impl ErrorCode {
    /// Stable lower-camel-case representation intended for APIs and bindings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::NoConverter => "noConverter",
            Self::Malformed => "malformed",
            Self::Encrypted => "encrypted",
            Self::ResourceLimit => "resourceLimit",
            Self::Ocr => "ocr",
            Self::Ai => "ai",
            Self::Network => "network",
            Self::Io => "io",
            Self::ComponentUnavailable => "componentUnavailable",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
        }
    }
}

/// Failure returned by any stage of the conversion pipeline.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum ConversionError {
    /// The input type is not recognized or supported.
    #[error("unsupported input: {detail}")]
    Unsupported {
        /// Human-readable reason the input is unsupported.
        detail: String,
    },
    /// The input format is known but no implementation is registered.
    #[error("no converter is registered for {format}")]
    NoConverter {
        /// Comma-separated detected format identifiers.
        format: String,
    },
    /// The input is recognized but malformed.
    #[error("malformed input{part}: {detail}", part = part.as_ref().map(|p| format!(" ({p})")).unwrap_or_default())]
    Malformed {
        /// Package part or stream in which corruption was detected.
        part: Option<String>,
        /// Human-readable structural failure.
        detail: String,
    },
    /// The input cannot be opened without a password.
    #[error("input is encrypted or password protected")]
    Encrypted,
    /// A named safety budget was exceeded.
    #[error("resource limit exceeded ({limit}): {detail}")]
    ResourceLimit {
        /// Stable resource-limit identifier.
        limit: &'static str,
        /// Observed and permitted values where known.
        detail: String,
    },
    /// Local OCR failed.
    #[error("OCR failed in {provider}: {detail}")]
    Ocr {
        /// OCR provider ID.
        provider: String,
        /// Provider or validation failure.
        detail: String,
    },
    /// A configured AI provider failed.
    #[error("AI provider {provider} failed: {detail}")]
    Ai {
        /// AI provider ID.
        provider: String,
        /// Provider or validation failure.
        detail: String,
    },
    /// An explicitly enabled network operation failed.
    #[error("network operation failed: {detail}")]
    Network {
        /// Policy or transport failure.
        detail: String,
    },
    /// Local input/output failed.
    #[error("I/O failed: {detail}")]
    Io {
        /// Underlying I/O failure without source bytes or secrets.
        detail: String,
    },
    /// A recognized optional component is unavailable in this build or installation.
    #[error("component {component} is unavailable: {detail}")]
    ComponentUnavailable {
        /// Stable component or subsystem ID.
        component: String,
        /// Build, installation, or runtime availability detail.
        detail: String,
    },
    /// The caller cancelled conversion.
    #[error("conversion cancelled")]
    Cancelled,
    /// An internal invariant or required component was unavailable.
    #[error("internal conversion error: {detail}")]
    Internal {
        /// Broken invariant or unavailable implementation component.
        detail: String,
    },
}

impl ConversionError {
    /// Stable code callers can branch on without parsing display text.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Unsupported { .. } => ErrorCode::Unsupported,
            Self::NoConverter { .. } => ErrorCode::NoConverter,
            Self::Malformed { .. } => ErrorCode::Malformed,
            Self::Encrypted => ErrorCode::Encrypted,
            Self::ResourceLimit { .. } => ErrorCode::ResourceLimit,
            Self::Ocr { .. } => ErrorCode::Ocr,
            Self::Ai { .. } => ErrorCode::Ai,
            Self::Network { .. } => ErrorCode::Network,
            Self::Io { .. } => ErrorCode::Io,
            Self::ComponentUnavailable { .. } => ErrorCode::ComponentUnavailable,
            Self::Cancelled => ErrorCode::Cancelled,
            Self::Internal { .. } => ErrorCode::Internal,
        }
    }
}

impl From<std::io::Error> for ConversionError {
    fn from(value: std::io::Error) -> Self {
        Self::Io { detail: value.to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_stable_code() {
        let cases = [
            (ConversionError::Unsupported { detail: String::new() }, "unsupported"),
            (ConversionError::NoConverter { format: "pdf".into() }, "noConverter"),
            (ConversionError::Malformed { part: None, detail: String::new() }, "malformed"),
            (ConversionError::Encrypted, "encrypted"),
            (
                ConversionError::ResourceLimit { limit: "bytes", detail: String::new() },
                "resourceLimit",
            ),
            (ConversionError::Ocr { provider: "local".into(), detail: String::new() }, "ocr"),
            (ConversionError::Ai { provider: "test".into(), detail: String::new() }, "ai"),
            (ConversionError::Network { detail: String::new() }, "network"),
            (ConversionError::Io { detail: String::new() }, "io"),
            (
                ConversionError::ComponentUnavailable {
                    component: "test".into(),
                    detail: String::new(),
                },
                "componentUnavailable",
            ),
            (ConversionError::Cancelled, "cancelled"),
            (ConversionError::Internal { detail: String::new() }, "internal"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.code().as_str(), expected);
        }
    }
}
