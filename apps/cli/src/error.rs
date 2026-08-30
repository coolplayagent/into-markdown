//! Stable CLI error and exit-code mapping.

use crate::args::Language;
use into_markdown::{ConversionError, ErrorCode, InputFormat, ProviderError, ProviderErrorCode};

/// Stable shell-level failure classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClass {
    Success,
    Usage,
    Conversion,
    Io,
    Policy,
    Ocr,
    Ai,
    Network,
    Component,
    PartialFailure,
    Internal,
    Cancelled,
}

impl ExitClass {
    /// Stable process exit status.
    pub const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Usage => 2,
            Self::Conversion => 3,
            Self::Io => 4,
            Self::Policy => 5,
            Self::Ocr => 6,
            Self::Ai => 7,
            Self::Network => 8,
            Self::Component => 9,
            Self::PartialFailure => 10,
            Self::Internal => 70,
            Self::Cancelled => 130,
        }
    }
}

/// One user-facing CLI failure with a stable machine code.
#[derive(Debug)]
pub struct CliError {
    class: ExitClass,
    code: &'static str,
    message: String,
    metadata: Option<Box<CliErrorMetadata>>,
    broken_pipe: bool,
    language: Option<Language>,
    json_log: bool,
    duration_ms: Option<f64>,
    processing_duration_ms: Option<f64>,
    wall_duration_ms: Option<f64>,
}

#[derive(Debug)]
struct CliErrorMetadata {
    reason_code: String,
    component: Option<String>,
    part: Option<String>,
    limit: Option<(String, String)>,
    detected_format: Option<InputFormat>,
}

impl CliError {
    pub fn new(class: ExitClass, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            class,
            code,
            message: message.into(),
            metadata: None,
            broken_pipe: false,
            language: None,
            json_log: false,
            duration_ms: None,
            processing_duration_ms: None,
            wall_duration_ms: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ExitClass::Usage, "usage", message)
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ExitClass::Usage, "config", message)
    }

    pub fn component(message: impl Into<String>) -> Self {
        Self::new(ExitClass::Component, "componentUnavailable", message)
    }

    #[cfg(windows)]
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ExitClass::Io, "io", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ExitClass::Internal, "internal", message)
    }

    pub fn partial(message: impl Into<String>) -> Self {
        Self::new(ExitClass::PartialFailure, "partialFailure", message)
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn reason_code(&self) -> &str {
        self.metadata.as_ref().map_or(self.code, |metadata| &metadata.reason_code)
    }

    pub fn component_name(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|metadata| metadata.component.as_deref())
    }

    pub fn part(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|metadata| metadata.part.as_deref())
    }

    pub fn limit(&self) -> Option<(&str, &str)> {
        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.limit.as_ref())
            .map(|(name, detail)| (name.as_str(), detail.as_str()))
    }

    pub fn detected_format(&self) -> Option<InputFormat> {
        self.metadata.as_ref().and_then(|metadata| metadata.detected_format)
    }

    pub const fn processing_duration_ms(&self) -> Option<f64> {
        self.processing_duration_ms
    }

    pub const fn duration_ms(&self) -> Option<f64> {
        self.duration_ms
    }

    pub const fn wall_duration_ms(&self) -> Option<f64> {
        self.wall_duration_ms
    }

    pub fn with_duration(mut self, duration_ms: f64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    pub fn with_processing_duration(mut self, duration_ms: Option<f64>) -> Self {
        self.processing_duration_ms = duration_ms;
        self
    }

    pub fn with_wall_duration(mut self, duration_ms: f64) -> Self {
        self.wall_duration_ms = Some(duration_ms);
        self
    }

    pub fn with_detected_format(mut self, format: Option<InputFormat>) -> Self {
        if let Some(format) = format {
            self.metadata
                .get_or_insert_with(|| {
                    Box::new(CliErrorMetadata {
                        reason_code: self.code.into(),
                        component: None,
                        part: None,
                        limit: None,
                        detected_format: None,
                    })
                })
                .detected_format = Some(format);
        }
        self
    }

    pub fn exit_code(&self) -> i32 {
        self.class.code()
    }

    pub fn is_broken_pipe(&self) -> bool {
        self.broken_pipe
    }

    pub fn with_rendering(mut self, language: Language, json_log: bool) -> Self {
        self.language = Some(language);
        self.json_log = json_log;
        self
    }

    pub fn language(&self) -> Option<Language> {
        self.language
    }

    pub fn uses_json_log(&self) -> bool {
        self.json_log
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

#[cfg(unix)]
impl From<rustix::io::Errno> for CliError {
    fn from(error: rustix::io::Errno) -> Self {
        std::io::Error::from_raw_os_error(error.raw_os_error()).into()
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            Self {
                class: ExitClass::Success,
                code: "brokenPipe",
                message: error.to_string(),
                metadata: None,
                broken_pipe: true,
                language: None,
                json_log: false,
                duration_ms: None,
                processing_duration_ms: None,
                wall_duration_ms: None,
            }
        } else {
            Self::new(ExitClass::Io, "io", error.to_string())
        }
    }
}

impl From<ConversionError> for CliError {
    fn from(error: ConversionError) -> Self {
        let class = match error.code() {
            ErrorCode::Unsupported
            | ErrorCode::NoConverter
            | ErrorCode::Malformed
            | ErrorCode::Encrypted => ExitClass::Conversion,
            ErrorCode::ResourceLimit | ErrorCode::Timeout => ExitClass::Policy,
            ErrorCode::Ocr => ExitClass::Ocr,
            ErrorCode::Ai => ExitClass::Ai,
            ErrorCode::Network => ExitClass::Network,
            ErrorCode::Io => ExitClass::Io,
            ErrorCode::ComponentUnavailable => ExitClass::Component,
            ErrorCode::Cancelled => ExitClass::Cancelled,
            _ => ExitClass::Internal,
        };
        let reason_code = error.reason_code().to_owned();
        let component = error.component().map(str::to_owned);
        let part = error.part().map(str::to_owned);
        let limit = error.limit().map(|(name, detail)| (name.to_owned(), detail.to_owned()));
        let mut cli = Self::new(class, error.code().as_str(), error.to_string());
        cli.metadata = Some(Box::new(CliErrorMetadata {
            reason_code,
            component,
            part,
            limit,
            detected_format: None,
        }));
        cli
    }
}

impl From<ProviderError> for CliError {
    fn from(error: ProviderError) -> Self {
        let class = match error.code() {
            ProviderErrorCode::NetworkDenied
            | ProviderErrorCode::HostDenied
            | ProviderErrorCode::PrivateNetworkDenied
            | ProviderErrorCode::InvalidConfiguration
            | ProviderErrorCode::ResourceLimit => ExitClass::Policy,
            ProviderErrorCode::Dns
            | ProviderErrorCode::Connect
            | ProviderErrorCode::Tls
            | ProviderErrorCode::Timeout => ExitClass::Network,
            ProviderErrorCode::Cancelled => ExitClass::Cancelled,
            _ => ExitClass::Ai,
        };
        Self::new(class, error.code_str(), error.code_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_errors_map_to_stable_shell_classes() {
        let cases = [
            (ConversionError::Unsupported { detail: String::new() }, 3),
            (ConversionError::NoConverter { format: "pdf".into() }, 3),
            (ConversionError::Malformed { part: None, detail: String::new() }, 3),
            (ConversionError::Encrypted, 3),
            (ConversionError::ResourceLimit { limit: "bytes", detail: String::new() }, 5),
            (ConversionError::Timeout, 5),
            (ConversionError::Ocr { provider: "ocr".into(), detail: String::new() }, 6),
            (ConversionError::Ai { provider: "ai".into(), detail: String::new() }, 7),
            (ConversionError::Network { detail: String::new() }, 8),
            (ConversionError::Io { detail: String::new() }, 4),
            (
                ConversionError::ComponentUnavailable {
                    component: "x".into(),
                    detail: String::new(),
                },
                9,
            ),
            (ConversionError::Cancelled, 130),
            (ConversionError::Internal { detail: String::new() }, 70),
        ];
        for (source, exit) in cases {
            let code = source.code().as_str();
            let error = CliError::from(source);
            assert_eq!(error.code(), code);
            assert_eq!(error.exit_code(), exit);
        }
        assert_eq!(CliError::component("missing").exit_code(), 9);
    }
}
