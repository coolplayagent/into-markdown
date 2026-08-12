//! Converter and source-resolution catalog.
//!
//! Format descriptors are present, but no production parser is registered by
//! this scaffold.

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, FormatCandidate, FormatDetector, FormatHint,
    InputFormat, InputRef, ResolvedInput, SourceMetadata, SourceResolver,
};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

/// Converter implementation status exposed by `into-md formats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatStatus {
    /// The architecture reserves this format but contains no parser yet.
    Planned,
    /// A converter is implemented and available in this build.
    Available,
}

impl FormatStatus {
    /// Stable lowercase display value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Available => "available",
        }
    }
}

/// User-facing format capability descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatDescriptor {
    /// Format family.
    pub format: InputFormat,
    /// User-facing group.
    pub family: &'static str,
    /// Recognized filename extensions.
    pub extensions: &'static [&'static str],
    /// Implementation status.
    pub status: FormatStatus,
}

const PLANNED: FormatStatus = FormatStatus::Planned;

const FORMATS: &[FormatDescriptor] = &[
    FormatDescriptor {
        format: InputFormat::Pdf,
        family: "document",
        extensions: &["pdf"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Doc,
        family: "document",
        extensions: &["doc"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Docx,
        family: "document",
        extensions: &["docx", "docm"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Ppt,
        family: "document",
        extensions: &["ppt", "pps", "pot"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Pptx,
        family: "document",
        extensions: &["pptx", "pptm", "ppsx", "ppsm"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Xls,
        family: "document",
        extensions: &["xls"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Xlsx,
        family: "document",
        extensions: &["xlsx", "xlsm", "xlsb"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Odt,
        family: "document",
        extensions: &["odt"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Ods,
        family: "document",
        extensions: &["ods"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Odp,
        family: "document",
        extensions: &["odp"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Rtf,
        family: "document",
        extensions: &["rtf"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Epub,
        family: "document",
        extensions: &["epub"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Text,
        family: "text",
        extensions: &["txt", "text", "log"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Markdown,
        family: "text",
        extensions: &["md", "markdown", "mdown"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Html,
        family: "text",
        extensions: &["html", "htm"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Csv,
        family: "text",
        extensions: &["csv"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Tsv,
        family: "text",
        extensions: &["tsv"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Json,
        family: "text",
        extensions: &["json"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Xml,
        family: "text",
        extensions: &["xml"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Feed,
        family: "remote",
        extensions: &["rss", "atom"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Ipynb,
        family: "text",
        extensions: &["ipynb"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Image,
        family: "media",
        extensions: &["png", "jpg", "jpeg", "tif", "tiff", "webp", "bmp"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Audio,
        family: "media",
        extensions: &["wav", "mp3", "m4a", "flac", "ogg"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Video,
        family: "media",
        extensions: &["mp4", "mov", "mkv", "webm"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Zip,
        family: "container",
        extensions: &["zip"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::OutlookMsg,
        family: "message",
        extensions: &["msg"],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::YouTube,
        family: "remote",
        extensions: &[],
        status: PLANNED,
    },
    FormatDescriptor {
        format: InputFormat::Wikipedia,
        family: "remote",
        extensions: &[],
        status: PLANNED,
    },
];

/// Complete planned format matrix.
#[must_use]
pub fn planned_formats() -> &'static [FormatDescriptor] {
    FORMATS
}

/// Resolver for in-memory inputs.
#[derive(Debug, Default)]
pub struct MemorySourceResolver;

impl SourceResolver for MemorySourceResolver {
    fn id(&self) -> &'static str {
        "builtin.source.memory"
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Bytes { .. })
    }

    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        Box::pin(async move {
            let InputRef::Bytes { data, name } = input else {
                return Err(ConversionError::Unsupported {
                    detail: "expected memory input".into(),
                });
            };
            enforce_input_limit(data.len() as u64, options)?;
            Ok(ResolvedInput {
                bytes: Arc::clone(data),
                metadata: SourceMetadata {
                    name: name.clone(),
                    size: data.len() as u64,
                    ..SourceMetadata::default()
                },
            })
        })
    }
}

/// Resolver for local paths.
#[derive(Debug, Default)]
pub struct LocalFileSourceResolver;

impl SourceResolver for LocalFileSourceResolver {
    fn id(&self) -> &'static str {
        "builtin.source.local-file"
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Path(_))
    }

    fn resolve<'a>(
        &'a self,
        input: &'a InputRef,
        options: &'a ConversionOptions,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        let result = (|| {
            let InputRef::Path(path) = input else {
                return Err(ConversionError::Unsupported { detail: "expected local path".into() });
            };
            let metadata = std::fs::metadata(path)?;
            enforce_input_limit(metadata.len(), options)?;
            let bytes = std::fs::read(path)?;
            Ok(ResolvedInput {
                bytes: Arc::from(bytes),
                metadata: SourceMetadata {
                    name: path.file_name().and_then(|v| v.to_str()).map(str::to_owned),
                    size: metadata.len(),
                    ..SourceMetadata::default()
                },
            })
        })();
        Box::pin(async move { result })
    }
}

/// Resolver for standard input.
#[derive(Debug, Default)]
pub struct StdinSourceResolver;

impl SourceResolver for StdinSourceResolver {
    fn id(&self) -> &'static str {
        "builtin.source.stdin"
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Stdin)
    }

    fn resolve<'a>(
        &'a self,
        _: &'a InputRef,
        options: &'a ConversionOptions,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        let result = (|| {
            let limit = options.limits.max_input_bytes;
            let mut bytes = Vec::new();
            std::io::stdin().take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
            enforce_input_limit(bytes.len() as u64, options)?;
            Ok(ResolvedInput {
                metadata: SourceMetadata {
                    name: Some("stdin".into()),
                    size: bytes.len() as u64,
                    ..SourceMetadata::default()
                },
                bytes: Arc::from(bytes),
            })
        })();
        Box::pin(async move { result })
    }
}

/// Deliberately non-networking URI resolver placeholder.
#[derive(Debug, Default)]
pub struct UriSourceResolver;

impl SourceResolver for UriSourceResolver {
    fn id(&self) -> &'static str {
        "builtin.source.uri-placeholder"
    }

    fn supports(&self, input: &InputRef) -> bool {
        matches!(input, InputRef::Uri(_))
    }

    fn resolve<'a>(
        &'a self,
        _: &'a InputRef,
        options: &'a ConversionOptions,
    ) -> BoxFuture<'a, Result<ResolvedInput, ConversionError>> {
        Box::pin(async move {
            if options.network.enabled {
                Err(ConversionError::ComponentUnavailable {
                    component: "builtin.source.uri".into(),
                    detail: "HTTP(S) resolution is not implemented".into(),
                })
            } else {
                Err(ConversionError::Network {
                    detail: "network resolution is disabled by default".into(),
                })
            }
        })
    }
}

fn enforce_input_limit(size: u64, options: &ConversionOptions) -> Result<(), ConversionError> {
    if size > options.limits.max_input_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: format!("{size} > {}", options.limits.max_input_bytes),
        });
    }
    Ok(())
}

/// Conservative detector using only explicit hints, names, extensions, and
/// MIME types. Content-signature detection belongs in future detectors.
#[derive(Debug, Default)]
pub struct HintFormatDetector;

impl FormatDetector for HintFormatDetector {
    fn id(&self) -> &'static str {
        "builtin.detector.hints"
    }

    fn priority(&self) -> i32 {
        100
    }

    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        hint: &'a FormatHint,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async move {
            if let Some(format) = hint.format {
                return Ok(vec![FormatCandidate::explicit(format)]);
            }
            let extension = hint
                .extension
                .as_deref()
                .or_else(|| hint.filename.as_deref().and_then(extension_of))
                .or_else(|| input.metadata.name.as_deref().and_then(extension_of));
            if let Some(format) = extension.and_then(InputFormat::from_extension) {
                return Ok(vec![FormatCandidate::new(format, 0.65, "filename extension")]);
            }
            let media_type = hint.media_type.as_deref().or(input.metadata.media_type.as_deref());
            Ok(media_type
                .and_then(format_from_media_type)
                .map(|format| vec![FormatCandidate::new(format, 0.60, "media type")])
                .unwrap_or_default())
        })
    }
}

fn extension_of(name: &str) -> Option<&str> {
    Path::new(name).extension().and_then(|value| value.to_str())
}

fn format_from_media_type(media_type: &str) -> Option<InputFormat> {
    Some(match media_type.split(';').next()?.trim().to_ascii_lowercase().as_str() {
        "application/pdf" => InputFormat::Pdf,
        "application/rtf" | "text/rtf" => InputFormat::Rtf,
        "application/epub+zip" => InputFormat::Epub,
        "application/json" => InputFormat::Json,
        "application/xml" | "text/xml" => InputFormat::Xml,
        "text/html" => InputFormat::Html,
        "text/csv" => InputFormat::Csv,
        "text/tab-separated-values" => InputFormat::Tsv,
        "text/markdown" => InputFormat::Markdown,
        "text/plain" => InputFormat::Text,
        "application/zip" => InputFormat::Zip,
        value if value.starts_with("image/") => InputFormat::Image,
        value if value.starts_with("audio/") => InputFormat::Audio,
        value if value.starts_with("video/") => InputFormat::Video,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn memory_resolver_enforces_input_budget() {
        let resolver = MemorySourceResolver;
        let input = InputRef::bytes(b"large".as_slice(), Some("x.txt"));
        let mut options = ConversionOptions::default();
        options.limits.max_input_bytes = 2;
        let error = block_on(resolver.resolve(&input, &options)).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::ResourceLimit);
    }

    #[test]
    fn remote_resolution_is_disabled_by_default() {
        let resolver = UriSourceResolver;
        let input = InputRef::Uri("https://example.com/a.pdf".into());
        let error = block_on(resolver.resolve(&input, &ConversionOptions::default())).unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Network);
    }
}
