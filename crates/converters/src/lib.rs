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
const TEXT_INSPECTION_BYTE_LIMIT: usize = 1024 * 1024;

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
    magic_candidate(bytes).or_else(|| structured_text_candidate(bytes)).into_iter().collect()
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
        || bytes.starts_with(b"ID3")
        || bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
    {
        (InputFormat::Audio, 0.96, "audio magic bytes")
    } else if bytes.starts_with(b"OggS") {
        return Some(detect_ogg(bytes));
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(&bytes[8..12], b"M4A " | b"M4B " | b"F4A ")
    {
        (InputFormat::Audio, 0.96, "audio ISO base media signature")
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some(detect_ebml(bytes));
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some(detect_iso_media(bytes));
    } else {
        return None;
    };
    Some(FormatCandidate::new(format, confidence, evidence))
}

fn detect_ogg(bytes: &[u8]) -> FormatCandidate {
    let packet = bytes.get(26).and_then(|segments| {
        let table_end = 27_usize.checked_add(usize::from(*segments))?;
        let payload = bytes.get(table_end..)?;
        Some(payload)
    });
    if packet
        .is_some_and(|value| value.starts_with(b"OpusHead") || value.starts_with(b"\x01vorbis"))
    {
        FormatCandidate::new(InputFormat::Audio, 0.98, "Ogg audio codec signature")
    } else if packet.is_some_and(|value| value.starts_with(b"\x80theora")) {
        FormatCandidate::new(InputFormat::Video, 0.98, "Ogg Theora codec signature")
    } else {
        FormatCandidate::new(InputFormat::Audio, 0.40, "Ogg container signature")
            .with_diagnostic("Ogg codec could not be identified; container may hold audio or video")
    }
}

fn detect_ebml(bytes: &[u8]) -> FormatCandidate {
    let prefix = &bytes[..bytes.len().min(4096)];
    let document_type = if contains_ascii_case_insensitive(prefix, b"webm") {
        "WebM"
    } else if contains_ascii_case_insensitive(prefix, b"matroska") {
        "Matroska"
    } else {
        "unknown EBML"
    };
    FormatCandidate::new(InputFormat::Video, 0.60, format!("{document_type} container signature"))
        .with_diagnostic("container type does not prove that a video track is present")
}

fn detect_iso_media(bytes: &[u8]) -> FormatCandidate {
    let brand = &bytes[8..12];
    if matches!(brand, b"M4A " | b"M4B " | b"F4A ") {
        FormatCandidate::new(InputFormat::Audio, 0.96, "audio ISO base media brand")
    } else if matches!(
        brand,
        b"avc1" | b"iso2" | b"isom" | b"mp41" | b"mp42" | b"qt  " | b"M4V " | b"F4V "
    ) {
        FormatCandidate::new(InputFormat::Video, 0.70, "video-capable ISO base media brand")
            .with_diagnostic("container brand does not prove that a video track is present")
    } else {
        FormatCandidate::new(InputFormat::Video, 0.40, "unknown ISO base media brand")
            .with_diagnostic("ISO base media brand is not recognized as audio or video")
    }
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

fn structured_text_candidate(bytes: &[u8]) -> Option<FormatCandidate> {
    let prefix = bytes.get(..bytes.len().min(TEXT_INSPECTION_BYTE_LIMIT))?;
    let text = std::str::from_utf8(prefix).ok()?.trim_start_matches('\u{feff}').trim_start();
    if text.starts_with("<!DOCTYPE html")
        || text.starts_with("<!doctype html")
        || starts_with_tag(text, "html")
    {
        return Some(FormatCandidate::new(InputFormat::Html, 0.96, "HTML root markup"));
    }
    if (text.starts_with('{') || text.starts_with('['))
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
    {
        if value.as_object().is_some_and(|object| {
            object.get("nbformat").is_some_and(serde_json::Value::is_number)
                && object.get("cells").is_some_and(serde_json::Value::is_array)
                && object.get("metadata").is_some_and(serde_json::Value::is_object)
        }) {
            return Some(FormatCandidate::new(
                InputFormat::Ipynb,
                0.99,
                "Jupyter notebook JSON structure",
            ));
        }
        return Some(FormatCandidate::new(InputFormat::Json, 0.96, "valid JSON content"));
    }
    let root = xml_root_name(text)?;
    if root.eq_ignore_ascii_case("rss") || root.eq_ignore_ascii_case("feed") {
        Some(FormatCandidate::new(InputFormat::Feed, 0.98, format!("XML {root} root element")))
    } else {
        Some(FormatCandidate::new(InputFormat::Xml, 0.92, format!("XML {root} root element")))
    }
}

fn starts_with_tag(text: &str, name: &str) -> bool {
    text.get(1..1 + name.len()).is_some_and(|value| value.eq_ignore_ascii_case(name))
        && text
            .as_bytes()
            .get(1 + name.len())
            .is_some_and(|byte| matches!(byte, b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n'))
}

fn xml_root_name(mut text: &str) -> Option<&str> {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix("<?") {
            text = rest.split_once("?>")?.1;
        } else if let Some(rest) = text.strip_prefix("<!--") {
            text = rest.split_once("-->")?.1;
        } else if let Some(rest) = text.strip_prefix("<!DOCTYPE") {
            text = rest.split_once('>')?.1;
        } else {
            break;
        }
    }
    let rest = text.strip_prefix('<')?;
    if rest.starts_with(['!', '?', '/']) {
        return None;
    }
    let end = rest.find(|character: char| {
        character.is_ascii_whitespace() || matches!(character, '>' | '/')
    })?;
    let qualified = &rest[..end];
    let local = qualified.rsplit(':').next()?;
    (!local.is_empty()
        && local.chars().next().is_some_and(|value| value.is_ascii_alphabetic())
        && local
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '_' | '-' | '.')))
    .then_some(local)
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
    let streams = [
        (InputFormat::Doc, "WordDocument"),
        (InputFormat::Xls, "Workbook"),
        (InputFormat::Xls, "Book"),
        (InputFormat::Ppt, "PowerPoint Document"),
        (InputFormat::OutlookMsg, "__properties_version1.0"),
    ];
    let directory_names = match cfb_directory_stream_names(bytes) {
        Ok(names) => names,
        Err(diagnostic) => {
            return [InputFormat::Doc, InputFormat::Xls, InputFormat::Ppt, InputFormat::OutlookMsg]
                .into_iter()
                .map(|format| {
                    FormatCandidate::new(format, 0.20, "OLE compound file signature")
                        .with_diagnostic(diagnostic.clone())
                })
                .collect();
        }
    };
    let mut candidates = Vec::new();
    for (format, stream) in streams {
        if directory_names.iter().any(|name| name == stream) {
            candidates.push(FormatCandidate::new(
                format,
                0.98,
                format!("OLE compound file stream {stream}"),
            ));
        }
    }
    candidates.sort_by_key(|candidate| candidate.format);
    candidates.dedup_by_key(|candidate| candidate.format);
    candidates
}

const CFB_FREE_SECTOR: u32 = 0xffff_ffff;
const CFB_END_OF_CHAIN: u32 = 0xffff_fffe;

#[allow(clippy::too_many_lines)]
fn cfb_directory_stream_names(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.len() < 512 {
        return Err("CFB header is truncated".into());
    }
    if bytes[..8] != [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1] {
        return Err("CFB signature is invalid".into());
    }
    if read_u16(bytes, 28)? != 0xfffe {
        return Err("CFB byte order is invalid".into());
    }
    let major = read_u16(bytes, 26)?;
    let sector_shift = read_u16(bytes, 30)?;
    if !matches!((major, sector_shift), (3, 9) | (4, 12)) {
        return Err("CFB major version or sector size is invalid".into());
    }
    if read_u16(bytes, 32)? != 6 {
        return Err("CFB mini-sector size is invalid".into());
    }
    let sector_size = 1_usize << sector_shift;
    let inspected_len = bytes.len().min(OLE_INSPECTION_BYTE_LIMIT);
    let sector_count = inspected_len.saturating_sub(sector_size) / sector_size;
    if sector_count == 0 {
        return Err("CFB contains no complete sectors within the inspection limit".into());
    }
    let fat_sector_count =
        usize::try_from(read_u32(bytes, 44)?).map_err(|_| "CFB FAT count is too large")?;
    let max_fat_sectors = sector_count.min(OLE_INSPECTION_BYTE_LIMIT / sector_size);
    if fat_sector_count == 0 || fat_sector_count > max_fat_sectors {
        return Err("CFB FAT sector count exceeds the inspection limit".into());
    }

    let mut fat_sector_ids = Vec::with_capacity(fat_sector_count);
    for offset in (76..512).step_by(4) {
        let sector = read_u32(bytes, offset)?;
        if sector != CFB_FREE_SECTOR {
            fat_sector_ids.push(sector);
            if fat_sector_ids.len() == fat_sector_count {
                break;
            }
        }
    }
    let difat_sector_count =
        usize::try_from(read_u32(bytes, 72)?).map_err(|_| "CFB DIFAT count is too large")?;
    if difat_sector_count > sector_count {
        return Err("CFB DIFAT sector count exceeds the inspection limit".into());
    }
    let mut difat_sector = read_u32(bytes, 68)?;
    let mut seen_difat = std::collections::BTreeSet::new();
    for _ in 0..difat_sector_count {
        if difat_sector == CFB_END_OF_CHAIN || !seen_difat.insert(difat_sector) {
            return Err("CFB DIFAT chain is truncated or cyclic".into());
        }
        let sector = cfb_sector(bytes, difat_sector, sector_size, inspected_len)?;
        for offset in (0..sector_size - 4).step_by(4) {
            let fat_sector = read_u32(sector, offset)?;
            if fat_sector != CFB_FREE_SECTOR {
                fat_sector_ids.push(fat_sector);
                if fat_sector_ids.len() == fat_sector_count {
                    break;
                }
            }
        }
        difat_sector = read_u32(sector, sector_size - 4)?;
    }
    if fat_sector_ids.len() != fat_sector_count {
        return Err("CFB DIFAT does not reference the declared FAT sectors".into());
    }

    let mut fat = Vec::with_capacity(fat_sector_count * sector_size / 4);
    for sector_id in fat_sector_ids {
        let sector = cfb_sector(bytes, sector_id, sector_size, inspected_len)?;
        fat.extend(
            sector
                .chunks_exact(4)
                .map(|value| u32::from_le_bytes([value[0], value[1], value[2], value[3]])),
        );
    }
    let first_directory_sector = read_u32(bytes, 48)?;
    let mut directory_sector = first_directory_sector;
    let mut seen_directory = std::collections::BTreeSet::new();
    let mut names = Vec::new();
    while directory_sector != CFB_END_OF_CHAIN {
        if !seen_directory.insert(directory_sector) || seen_directory.len() > sector_count {
            return Err("CFB directory chain is cyclic or exceeds the inspection limit".into());
        }
        let sector = cfb_sector(bytes, directory_sector, sector_size, inspected_len)?;
        for entry in sector.chunks_exact(128) {
            if entry[66] != 2 {
                continue;
            }
            let name_bytes = usize::from(u16::from_le_bytes([entry[64], entry[65]]));
            if !(2..=64).contains(&name_bytes) || name_bytes % 2 != 0 {
                return Err("CFB directory stream contains an invalid stream name".into());
            }
            let name = entry[..name_bytes - 2]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
            let name = char::decode_utf16(name)
                .collect::<Result<String, _>>()
                .map_err(|_| "CFB directory stream contains invalid UTF-16")?;
            names.push(name);
        }
        directory_sector = *fat
            .get(
                usize::try_from(directory_sector)
                    .map_err(|_| "CFB directory sector is too large")?,
            )
            .ok_or("CFB directory sector has no FAT entry")?;
    }
    Ok(names)
}

fn cfb_sector(
    bytes: &[u8],
    sector_id: u32,
    sector_size: usize,
    inspected_len: usize,
) -> Result<&[u8], String> {
    let sector_id = usize::try_from(sector_id).map_err(|_| "CFB sector ID is too large")?;
    let start = sector_id
        .checked_add(1)
        .and_then(|value| value.checked_mul(sector_size))
        .ok_or("CFB sector offset overflows")?;
    let end = start.checked_add(sector_size).ok_or("CFB sector end overflows")?;
    if end > inspected_len {
        return Err(format!(
            "CFB sector falls outside the {OLE_INSPECTION_BYTE_LIMIT} byte inspection limit or input"
        ));
    }
    bytes.get(start..end).ok_or_else(|| "CFB sector is truncated".into())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let value = bytes.get(offset..offset + 2).ok_or("CFB structure is truncated")?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes.get(offset..offset + 4).ok_or("CFB structure is truncated")?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
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

    fn cfb_with_stream(name: &str) -> Vec<u8> {
        let mut bytes = vec![0_u8; 3 * 512];
        bytes[..8].copy_from_slice(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]);
        bytes[26..28].copy_from_slice(&3_u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&0xfffe_u16.to_le_bytes());
        bytes[30..32].copy_from_slice(&9_u16.to_le_bytes());
        bytes[32..34].copy_from_slice(&6_u16.to_le_bytes());
        bytes[44..48].copy_from_slice(&1_u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&1_u32.to_le_bytes());
        bytes[56..60].copy_from_slice(&4096_u32.to_le_bytes());
        bytes[60..64].copy_from_slice(&CFB_END_OF_CHAIN.to_le_bytes());
        bytes[68..72].copy_from_slice(&CFB_END_OF_CHAIN.to_le_bytes());
        for offset in (76..512).step_by(4) {
            bytes[offset..offset + 4].copy_from_slice(&CFB_FREE_SECTOR.to_le_bytes());
        }
        bytes[76..80].copy_from_slice(&0_u32.to_le_bytes());
        for offset in (512..1024).step_by(4) {
            bytes[offset..offset + 4].copy_from_slice(&CFB_FREE_SECTOR.to_le_bytes());
        }
        bytes[512..516].copy_from_slice(&0xffff_fffd_u32.to_le_bytes());
        bytes[516..520].copy_from_slice(&CFB_END_OF_CHAIN.to_le_bytes());
        let encoded =
            format!("{name}\0").encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        bytes[1024..1024 + encoded.len()].copy_from_slice(&encoded);
        bytes[1088..1090].copy_from_slice(&u16::try_from(encoded.len()).unwrap().to_le_bytes());
        bytes[1090] = 2;
        bytes
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
    fn structured_text_detection_orders_specific_formats_first() {
        let fixtures = [
            (b"<!doctype html><html></html>".as_slice(), InputFormat::Html),
            (b"<?xml version='1.0'?><rss></rss>".as_slice(), InputFormat::Feed),
            (b"<feed xmlns='http://www.w3.org/2005/Atom'></feed>".as_slice(), InputFormat::Feed),
            (b"<document/>".as_slice(), InputFormat::Xml),
            (br#"{"nbformat":4,"metadata":{},"cells":[]}"#.as_slice(), InputFormat::Ipynb),
            (br#"{"ordinary":true}"#.as_slice(), InputFormat::Json),
        ];
        for (bytes, expected) in fixtures {
            let input = resolved(bytes.to_vec(), "misleading.txt");
            let candidates =
                block_on(ContentFormatDetector.detect(&input, &FormatHint::default())).unwrap();
            assert_eq!(candidates[0].format, expected);
        }
    }

    #[test]
    fn ambiguous_media_containers_do_not_receive_high_confidence() {
        let ogg = detect_content(b"OggS\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        let ebml = detect_content(b"\x1a\x45\xdf\xa3unknown");
        let iso = detect_content(b"\0\0\0\x18ftypzzzz");
        for candidate in [&ogg[0], &ebml[0], &iso[0]] {
            assert!(candidate.confidence <= 0.60);
            assert!(!candidate.diagnostics.is_empty());
        }
        let mut theora = vec![0_u8; 28];
        theora[..4].copy_from_slice(b"OggS");
        theora[26] = 1;
        theora[27] = 7;
        theora.extend_from_slice(b"\x80theora");
        assert_eq!(detect_content(&theora)[0].format, InputFormat::Video);
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
    fn cfb_directory_chain_distinguishes_legacy_office() {
        let input = resolved(cfb_with_stream("PowerPoint Document"), "slides.doc");
        let candidates =
            block_on(ContentFormatDetector.detect(&input, &FormatHint::default())).unwrap();
        assert_eq!(candidates[0].format, InputFormat::Ppt);
        assert!((candidates[0].confidence - 0.98).abs() < f32::EPSILON);
    }

    #[test]
    fn aligned_bytes_outside_cfb_directory_are_not_stream_entries() {
        let mut bytes = cfb_with_stream("unrelated");
        let forged = "WordDocument\0".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
        bytes[640..640 + forged.len()].copy_from_slice(&forged);
        bytes[704..706].copy_from_slice(&u16::try_from(forged.len()).unwrap().to_le_bytes());
        bytes[706] = 2;
        let candidates = detect_ole(&bytes);
        assert!(candidates.iter().all(|candidate| candidate.format != InputFormat::Doc));
    }

    #[test]
    fn malformed_cfb_header_degrades_to_diagnostic_low_confidence() {
        let mut bytes = cfb_with_stream("WordDocument");
        bytes[28..30].copy_from_slice(&0_u16.to_le_bytes());
        let candidates = detect_ole(&bytes);
        assert_eq!(candidates.len(), 4);
        assert!(
            candidates.iter().all(|candidate| (candidate.confidence - 0.20).abs() < f32::EPSILON)
        );
        assert!(
            candidates.iter().all(|candidate| { candidate.diagnostics[0].contains("byte order") })
        );
    }
}
