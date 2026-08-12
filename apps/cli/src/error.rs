//! Stable CLI error and exit-code mapping.

use crate::args::Language;
use into_markdown::{ConversionError, ErrorCode};

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
    broken_pipe: bool,
    language: Option<Language>,
    json_log: bool,
}

impl CliError {
    pub fn new(class: ExitClass, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            class,
            code,
            message: message.into(),
            broken_pipe: false,
            language: None,
            json_log: false,
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

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            Self {
                class: ExitClass::Success,
                code: "brokenPipe",
                message: error.to_string(),
                broken_pipe: true,
                language: None,
                json_log: false,
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
            ErrorCode::ResourceLimit => ExitClass::Policy,
            ErrorCode::Ocr => ExitClass::Ocr,
            ErrorCode::Ai => ExitClass::Ai,
            ErrorCode::Network => ExitClass::Network,
            ErrorCode::Io => ExitClass::Io,
            ErrorCode::ComponentUnavailable => ExitClass::Component,
            ErrorCode::Cancelled => ExitClass::Cancelled,
            _ => ExitClass::Internal,
        };
        Self::new(class, error.code().as_str(), error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_errors_map_to_stable_shell_classes() {
        let no_converter = CliError::from(ConversionError::NoConverter { format: "pdf".into() });
        assert_eq!(no_converter.exit_code(), 3);
        assert_eq!(no_converter.code(), "noConverter");
        assert_eq!(CliError::component("missing").exit_code(), 9);
    }
}
