//! Converter and source-resolution catalog.
//!
//! Format descriptors are present, but no production parser is registered by
//! this scaffold.

use into_markdown_core::{
    BoxFuture, ConversionError, ConversionOptions, FormatCandidate, FormatDetector, FormatHint,
    InputFormat, InputRef, ResolvedInput, SourceMetadata, SourceResolver,
};
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
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

/// Detector for caller/source hints. It reports every usable hint so conflicts
/// remain visible instead of silently accepting the first value.
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
            let mut evidence: BTreeMap<InputFormat, Vec<&str>> = BTreeMap::new();
            let extension = hint
                .extension
                .as_deref()
                .or_else(|| hint.filename.as_deref().and_then(extension_of))
                .or_else(|| input.metadata.name.as_deref().and_then(extension_of));
            if let Some(format) = extension.and_then(InputFormat::from_extension) {
                evidence.entry(format).or_default().push("filename extension");
            }
            let media_type = hint.media_type.as_deref().or(input.metadata.media_type.as_deref());
            if let Some(format) = media_type.and_then(format_from_media_type) {
                evidence.entry(format).or_default().push("media type");
            }
            let conflict = evidence.len() > 1;
            Ok(evidence
                .into_iter()
                .map(|(format, reasons)| {
                    let confidence = if reasons.len() > 1 {
                        0.68
                    } else if reasons[0] == "media type" {
                        0.60
                    } else {
                        0.55
                    };
                    let candidate = FormatCandidate::new(format, confidence, reasons.join(" + "));
                    if conflict {
                        candidate.with_diagnostic("filename extension and media type disagree")
                    } else {
                        candidate
                    }
                })
                .collect())
        })
    }
}

/// Detector for file signatures and bounded inspection of ZIP/OLE containers.
#[derive(Debug, Default)]
pub struct ContentFormatDetector;

impl FormatDetector for ContentFormatDetector {
    fn id(&self) -> &'static str {
        "builtin.detector.content"
    }

    fn priority(&self) -> i32 {
        200
    }

    fn detect<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatHint,
    ) -> BoxFuture<'a, Result<Vec<FormatCandidate>, ConversionError>> {
        Box::pin(async move { Ok(detect_content(&input.bytes)) })
    }
}

const ZIP_INSPECTION_ENTRY_LIMIT: usize = 4096;
const ZIP_MIMETYPE_READ_LIMIT: u64 = 128;
const ZIP_NAME_READ_LIMIT: usize = 1024 * 1024;
const OLE_INSPECTION_BYTE_LIMIT: usize = 8 * 1024 * 1024;

fn detect_content(bytes: &[u8]) -> Vec<FormatCandidate> {
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        return detect_zip(bytes);
    }
    if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
        return detect_ole(bytes);
    }
    magic_candidate(bytes).into_iter().collect()
}

fn magic_candidate(bytes: &[u8]) -> Option<FormatCandidate> {
    let (format, confidence, evidence) = if bytes.starts_with(b"%PDF-") {
        (InputFormat::Pdf, 0.99, "PDF magic bytes")
    } else if bytes.starts_with(b"{\\rtf") {
        (InputFormat::Rtf, 0.99, "RTF signature")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"II*\0")
        || bytes.starts_with(b"MM\0*")
        || bytes.starts_with(b"BM")
        || bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
    {
        (InputFormat::Image, 0.98, "image magic bytes")
    } else if bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"OggS")
        || bytes.starts_with(b"ID3")
        || bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
    {
        (InputFormat::Audio, 0.96, "audio magic bytes")
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(&bytes[8..12], b"M4A " | b"M4B " | b"F4A ")
    {
        (InputFormat::Audio, 0.96, "audio ISO base media signature")
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        || bytes.len() >= 12 && &bytes[4..8] == b"ftyp"
    {
        (InputFormat::Video, 0.94, "video/container magic bytes")
    } else {
        return None;
    };
    Some(FormatCandidate::new(format, confidence, evidence))
}

fn detect_zip(bytes: &[u8]) -> Vec<FormatCandidate> {
    let mut candidates = vec![FormatCandidate::new(InputFormat::Zip, 0.90, "ZIP magic bytes")];
    if let Some(entry_count) = zip_claimed_entry_count(bytes)
        && entry_count > ZIP_INSPECTION_ENTRY_LIMIT
    {
        candidates[0].diagnostics.push(format!(
            "ZIP inspection stopped: {entry_count} entries exceed the {ZIP_INSPECTION_ENTRY_LIMIT} entry limit"
        ));
        return candidates;
    }
    let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(error) => {
            candidates[0]
                .diagnostics
                .push(format!("ZIP directory could not be inspected: {error}"));
            return candidates;
        }
    };
    if archive.len() > ZIP_INSPECTION_ENTRY_LIMIT {
        candidates[0].diagnostics.push(format!(
            "ZIP inspection stopped: {} entries exceed the {} entry limit",
            archive.len(),
            ZIP_INSPECTION_ENTRY_LIMIT
        ));
        return candidates;
    }

    let mut names = Vec::with_capacity(archive.len());
    let mut name_bytes = 0_usize;
    for index in 0..archive.len() {
        match archive.by_index(index) {
            Ok(entry) => {
                name_bytes = name_bytes.saturating_add(entry.name().len());
                if name_bytes > ZIP_NAME_READ_LIMIT {
                    candidates[0].diagnostics.push(format!(
                        "ZIP inspection stopped: entry names exceed the {ZIP_NAME_READ_LIMIT} byte limit"
                    ));
                    return candidates;
                }
                names.push(entry.name().replace('\\', "/"));
            }
            Err(error) => candidates[0]
                .diagnostics
                .push(format!("ZIP entry {index} could not be inspected: {error}")),
        }
    }
    let mimetype = match archive.by_name("mimetype") {
        Ok(entry) => {
            let mut raw = Vec::new();
            match entry.take(ZIP_MIMETYPE_READ_LIMIT + 1).read_to_end(&mut raw) {
                Ok(_) if raw.len() as u64 > ZIP_MIMETYPE_READ_LIMIT => {
                    candidates[0].diagnostics.push(format!(
                        "ZIP mimetype exceeds the {ZIP_MIMETYPE_READ_LIMIT} byte read limit"
                    ));
                    None
                }
                Ok(_) => {
                    if let Ok(value) = String::from_utf8(raw) {
                        Some(value.trim().to_owned())
                    } else {
                        candidates[0].diagnostics.push("ZIP mimetype is not UTF-8".into());
                        None
                    }
                }
                Err(error) => {
                    candidates[0]
                        .diagnostics
                        .push(format!("ZIP mimetype could not be inspected: {error}"));
                    None
                }
            }
        }
        Err(zip::result::ZipError::FileNotFound) => None,
        Err(error) => {
            candidates[0].diagnostics.push(format!("ZIP mimetype could not be opened: {error}"));
            None
        }
    };

    let specialized = if names.iter().any(|name| name == "word/document.xml") {
        Some((InputFormat::Docx, "OOXML word/document.xml package part"))
    } else if names.iter().any(|name| name == "ppt/presentation.xml") {
        Some((InputFormat::Pptx, "OOXML ppt/presentation.xml package part"))
    } else if names.iter().any(|name| name == "xl/workbook.xml" || name == "xl/workbook.bin") {
        Some((InputFormat::Xlsx, "OOXML xl/workbook.xml package part"))
    } else if mimetype.as_deref() == Some("application/epub+zip")
        && names.iter().any(|name| name == "META-INF/container.xml")
    {
        Some((InputFormat::Epub, "EPUB mimetype and container package parts"))
    } else if mimetype.as_deref() == Some("application/vnd.oasis.opendocument.text") {
        Some((InputFormat::Odt, "OpenDocument text mimetype"))
    } else if mimetype.as_deref() == Some("application/vnd.oasis.opendocument.spreadsheet") {
        Some((InputFormat::Ods, "OpenDocument spreadsheet mimetype"))
    } else if mimetype.as_deref() == Some("application/vnd.oasis.opendocument.presentation") {
        Some((InputFormat::Odp, "OpenDocument presentation mimetype"))
    } else {
        None
    };
    if let Some((format, evidence)) = specialized {
        candidates.push(FormatCandidate::new(format, 0.99, evidence));
    }
    candidates
}

fn zip_claimed_entry_count(bytes: &[u8]) -> Option<usize> {
    const EOCD_MIN_SIZE: usize = 22;
    const MAX_COMMENT_SIZE: usize = u16::MAX as usize;
    let search_start = bytes.len().saturating_sub(EOCD_MIN_SIZE + MAX_COMMENT_SIZE);
    let eocd = bytes[search_start..].windows(4).rposition(|window| window == b"PK\x05\x06")?
        + search_start;
    let record = bytes.get(eocd..eocd + EOCD_MIN_SIZE)?;
    Some(usize::from(u16::from_le_bytes([record[10], record[11]])))
}

fn detect_ole(bytes: &[u8]) -> Vec<FormatCandidate> {
    let inspected = &bytes[..bytes.len().min(OLE_INSPECTION_BYTE_LIMIT)];
    let streams = [
        (InputFormat::Doc, "WordDocument"),
        (InputFormat::Xls, "Workbook"),
        (InputFormat::Xls, "Book"),
        (InputFormat::Ppt, "PowerPoint Document"),
        (InputFormat::OutlookMsg, "__properties_version1.0"),
    ];
    let mut candidates = Vec::new();
    for (format, stream) in streams {
        if ole_has_directory_entry(inspected, stream) {
            candidates.push(FormatCandidate::new(
                format,
                0.98,
                format!("OLE compound file stream {stream}"),
            ));
        }
    }
    candidates.sort_by_key(|candidate| candidate.format);
    candidates.dedup_by_key(|candidate| candidate.format);
    if bytes.len() > inspected.len() {
        for candidate in &mut candidates {
            candidate
                .diagnostics
                .push(format!("OLE stream-name scan limited to {OLE_INSPECTION_BYTE_LIMIT} bytes"));
        }
    }
    candidates
}

fn ole_has_directory_entry(bytes: &[u8], expected: &str) -> bool {
    if bytes.len() < 512 {
        return false;
    }
    bytes[512..].chunks_exact(128).any(|entry| {
        let name_bytes = usize::from(u16::from_le_bytes([entry[64], entry[65]]));
        if !(2..=64).contains(&name_bytes) || name_bytes % 2 != 0 {
            return false;
        }
        let name = entry[..name_bytes - 2]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        char::decode_utf16(name).collect::<Result<String, _>>().is_ok_and(|name| name == expected)
    })
}

fn extension_of(name: &str) -> Option<&str> {
    Path::new(name).extension().and_then(|value| value.to_str())
}

fn format_from_media_type(media_type: &str) -> Option<InputFormat> {
    Some(match media_type.split(';').next()?.trim().to_ascii_lowercase().as_str() {
        "application/pdf" => InputFormat::Pdf,
        "application/rtf" | "text/rtf" => InputFormat::Rtf,
        "application/epub+zip" => InputFormat::Epub,
        "application/msword" => InputFormat::Doc,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.ms-word.document.macroenabled.12" => InputFormat::Docx,
        "application/vnd.ms-powerpoint" => InputFormat::Ppt,
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.ms-powerpoint.presentation.macroenabled.12" => InputFormat::Pptx,
        "application/vnd.ms-excel" => InputFormat::Xls,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.ms-excel.sheet.macroenabled.12" => InputFormat::Xlsx,
        "application/vnd.oasis.opendocument.text" => InputFormat::Odt,
        "application/vnd.oasis.opendocument.spreadsheet" => InputFormat::Ods,
        "application/vnd.oasis.opendocument.presentation" => InputFormat::Odp,
        "application/vnd.ms-outlook" => InputFormat::OutlookMsg,
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
    use std::io::Write as _;

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

    fn resolved(bytes: Vec<u8>, name: &str) -> ResolvedInput {
        ResolvedInput {
            metadata: SourceMetadata {
                name: Some(name.into()),
                size: bytes.len() as u64,
                ..SourceMetadata::default()
            },
            bytes: Arc::from(bytes),
        }
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in entries {
            archive.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            archive.write_all(contents).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }

    #[test]
    fn hints_preserve_conflicting_extension_and_media_type() {
        let detector = HintFormatDetector;
        let input = resolved(b"ignored".to_vec(), "report.docx");
        let hint =
            FormatHint { media_type: Some("application/pdf".into()), ..FormatHint::default() };
        let candidates = block_on(detector.detect(&input, &hint)).unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| !candidate.diagnostics.is_empty()));
    }

    #[test]
    fn magic_identification_does_not_trust_a_misleading_name() {
        let detector = ContentFormatDetector;
        let input = resolved(b"%PDF-1.7\n".to_vec(), "report.docx");
        let candidates = block_on(detector.detect(&input, &FormatHint::default())).unwrap();
        assert_eq!(candidates[0].format, InputFormat::Pdf);
        assert!((candidates[0].confidence - 0.99).abs() < f32::EPSILON);
    }

    #[test]
    fn zip_parts_distinguish_ooxml_epub_and_odf() {
        let detector = ContentFormatDetector;
        let fixtures = [
            (zip_with(&[("word/document.xml", b"<w:document/>" as &[u8])]), InputFormat::Docx),
            (
                zip_with(&[
                    ("mimetype", b"application/epub+zip"),
                    ("META-INF/container.xml", b"<container/>"),
                ]),
                InputFormat::Epub,
            ),
            (
                zip_with(&[("mimetype", b"application/vnd.oasis.opendocument.text")]),
                InputFormat::Odt,
            ),
        ];
        for (bytes, expected) in fixtures {
            let input = resolved(bytes, "misleading.zip");
            let candidates = block_on(detector.detect(&input, &FormatHint::default())).unwrap();
            assert_eq!(candidates[1].format, expected);
            assert_eq!(candidates[0].format, InputFormat::Zip);
        }
    }

    #[test]
    fn zip_mimetype_read_is_bounded_and_explained() {
        let bytes = zip_with(&[("mimetype", &[b'x'; 129])]);
        let input = resolved(bytes, "oversized.odt");
        let candidates =
            block_on(ContentFormatDetector.detect(&input, &FormatHint::default())).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].format, InputFormat::Zip);
        assert!(candidates[0].diagnostics[0].contains("128 byte read limit"));
    }

    #[test]
    fn zip_entry_limit_is_checked_before_archive_construction() {
        let mut bytes = b"PK\x05\x06".to_vec();
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&5000_u16.to_le_bytes());
        bytes.extend_from_slice(&5000_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        let candidates = detect_zip(&bytes);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].diagnostics[0].contains("5000 entries"));
    }

    #[test]
    fn ole_directory_entry_distinguishes_legacy_office() {
        let mut bytes = vec![0_u8; 640];
        bytes[..8].copy_from_slice(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]);
        let name =
            "PowerPoint Document\0".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        bytes[512..512 + name.len()].copy_from_slice(&name);
        bytes[576..578].copy_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        let input = resolved(bytes, "slides.doc");
        let candidates =
            block_on(ContentFormatDetector.detect(&input, &FormatHint::default())).unwrap();
        assert_eq!(candidates[0].format, InputFormat::Ppt);
    }
}
