//! Stable process failures and structured terminal-code mapping.
use thiserror::Error;

/// Stable process-plugin failure categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginErrorCode {
    /// The local plugin authority is malformed or no longer matches disk.
    Authority,
    /// The operating-system sandbox could not be installed fail-closed.
    SandboxUnavailable,
    /// The authenticated executable could not be launched.
    Launch,
    /// The peer violated framing, ordering, identity, or version rules.
    Protocol,
    /// The peer emitted a frame larger than policy permits.
    FrameTooLarge,
    /// The plugin exited or its protocol stream ended before a terminal response.
    Crashed,
    /// The request exceeded its host deadline.
    Timeout,
    /// The caller cancelled the request.
    Cancelled,
    /// The returned IR, resources, diagnostics, or provenance were invalid.
    InvalidResult,
    /// The plugin returned a controlled terminal error.
    Plugin,
    /// A host or provider resource budget was exceeded.
    ResourceLimit,
    /// Controlled rejection of the worker-private OCR recognition memory budget.
    OcrRecognitionMemory,
    /// Controlled provider report that its optional component is unavailable.
    ComponentUnavailable,
    /// Fixed recognizer width bound.
    OcrWidthLimit,
    /// Fixed recognition crop pixel bound.
    OcrPixelLimit,
    /// Fixed recognition tensor bound.
    OcrTensorLimit,
    /// Fixed region or decoded output bound.
    OcrStructureLimit,
}

impl PluginErrorCode {
    /// Stable lower-camel-case representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authority => "pluginAuthority",
            Self::SandboxUnavailable => "pluginSandboxUnavailable",
            Self::Launch => "pluginLaunch",
            Self::Protocol => "pluginProtocol",
            Self::FrameTooLarge => "pluginFrameTooLarge",
            Self::Crashed => "pluginCrashed",
            Self::Timeout => "pluginTimeout",
            Self::Cancelled => "pluginCancelled",
            Self::InvalidResult => "pluginInvalidResult",
            Self::Plugin => "pluginError",
            Self::ResourceLimit => "pluginResourceLimit",
            Self::OcrRecognitionMemory => "ocrRecognitionMemory",
            Self::ComponentUnavailable => "componentUnavailable",
            Self::OcrWidthLimit => "ocrWidthLimit",
            Self::OcrPixelLimit => "ocrPixelLimit",
            Self::OcrTensorLimit => "ocrTensorLimit",
            Self::OcrStructureLimit => "ocrStructureLimit",
        }
    }
}

/// Sanitized process-plugin failure. Child stderr is never included verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{code}: {detail}", code = code.as_str())]
pub struct PluginError {
    /// Stable machine category.
    pub code: PluginErrorCode,
    /// Bounded host-generated detail.
    pub detail: String,
}

impl PluginError {
    pub(crate) fn new(code: PluginErrorCode, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        let mut length = detail.len().min(512);
        while !detail.is_char_boundary(length) {
            length -= 1;
        }
        detail.truncate(length);
        Self { code, detail }
    }
}

pub(crate) fn terminal_error_code(code: &str) -> PluginErrorCode {
    match code {
        "ocrRecognitionMemory" => PluginErrorCode::OcrRecognitionMemory,
        "componentUnavailable" => PluginErrorCode::ComponentUnavailable,
        "ocrWidthLimit" => PluginErrorCode::OcrWidthLimit,
        "ocrPixelLimit" => PluginErrorCode::OcrPixelLimit,
        "ocrTensorLimit" => PluginErrorCode::OcrTensorLimit,
        "ocrStructureLimit" => PluginErrorCode::OcrStructureLimit,
        "resourceLimit" => PluginErrorCode::ResourceLimit,
        "cancelled" => PluginErrorCode::Cancelled,
        "timeout" => PluginErrorCode::Timeout,
        _ => PluginErrorCode::Plugin,
    }
}
