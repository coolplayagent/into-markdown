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
const ZIP_METADATA_READ_LIMIT: u64 = 256 * 1024;
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
        || bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
    {
        (InputFormat::Image, 0.98, "image magic bytes")
    } else if valid_bmp_header(bytes) {
        (InputFormat::Image, 0.98, "validated BMP file and DIB headers")
    } else if bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"ID3")
        || bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
    {
        (InputFormat::Audio, 0.96, "audio magic bytes")
    } else if valid_mpeg_audio_frame_header(bytes) {
        (InputFormat::Audio, 0.92, "MPEG audio frame header")
    } else if bytes.starts_with(b"OggS") {
        return Some(detect_ogg(bytes));
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some(detect_ebml(bytes));
    } else if bytes.get(4..8) == Some(b"ftyp") {
        return detect_iso_media(bytes);
    } else {
        return None;
    };
    Some(FormatCandidate::new(format, confidence, evidence))
}

fn valid_mpeg_audio_frame_header(bytes: &[u8]) -> bool {
    let Some(header) = bytes.get(..4) else {
        return false;
    };
    let version = (header[1] >> 3) & 0b11;
    let layer = (header[1] >> 1) & 0b11;
    let bitrate_index = header[2] >> 4;
    let sample_rate_index = (header[2] >> 2) & 0b11;
    header[0] == 0xff
        && header[1] & 0xe0 == 0xe0
        && version != 0b01
        && layer != 0
        && !matches!(bitrate_index, 0 | 0x0f)
        && sample_rate_index != 0b11
}

fn valid_bmp_header(bytes: &[u8]) -> bool {
    if bytes.get(..2) != Some(b"BM") {
        return false;
    }
    let Some(file_size) = little_u32(bytes, 2).and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(pixel_offset) = little_u32(bytes, 10).and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    let Some(dib_size) = little_u32(bytes, 14).and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(headers_end) = 14_usize.checked_add(dib_size) else {
        return false;
    };
    if !matches!(dib_size, 12 | 40 | 52 | 56 | 64 | 108 | 124)
        || headers_end > bytes.len()
        || pixel_offset < headers_end
        || file_size <= pixel_offset
        || file_size > bytes.len()
    {
        return false;
    }
    let (dimensions_valid, planes, bits_per_pixel) = if dib_size == 12 {
        let dimensions_valid = little_u16(bytes, 18).is_some_and(|value| value != 0)
            && little_u16(bytes, 20).is_some_and(|value| value != 0);
        (dimensions_valid, little_u16(bytes, 22), little_u16(bytes, 24))
    } else {
        let dimensions_valid = little_i32(bytes, 18).is_some_and(|value| value != 0)
            && little_i32(bytes, 22).is_some_and(|value| value != 0);
        (dimensions_valid, little_u16(bytes, 26), little_u16(bytes, 28))
    };
    dimensions_valid
        && planes == Some(1)
        && bits_per_pixel.is_some_and(|value| matches!(value, 1 | 4 | 8 | 16 | 24 | 32))
}

fn little_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn little_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn little_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    little_u32(bytes, offset).map(|value| i32::from_le_bytes(value.to_le_bytes()))
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

fn detect_iso_media(bytes: &[u8]) -> Option<FormatCandidate> {
    let box_size = usize::try_from(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    if box_size < 16 || box_size > bytes.len() || (box_size - 16) % 4 != 0 {
        return None;
    }
    let brand = bytes.get(8..12)?;
    if matches!(brand, b"M4A " | b"M4B " | b"F4A ") {
        Some(FormatCandidate::new(InputFormat::Audio, 0.96, "audio ISO base media brand"))
    } else if matches!(
        brand,
        b"avc1" | b"iso2" | b"isom" | b"mp41" | b"mp42" | b"qt  " | b"M4V " | b"F4V "
    ) {
        Some(
            FormatCandidate::new(InputFormat::Video, 0.70, "video-capable ISO base media brand")
                .with_diagnostic("container brand does not prove that a video track is present"),
        )
    } else {
        Some(
            FormatCandidate::new(InputFormat::Video, 0.40, "unknown ISO base media brand")
                .with_diagnostic("ISO base media brand is not recognized as audio or video"),
        )
    }
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

fn structured_text_candidate(bytes: &[u8]) -> Option<FormatCandidate> {
    let prefix = bytes.get(..bytes.len().min(TEXT_INSPECTION_BYTE_LIMIT))?;
    let text = std::str::from_utf8(prefix).ok()?.trim_start_matches('\u{feff}').trim_start();
    if html_prelude_identifies_html(text) {
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
    if root.eq_ignore_ascii_case("html") {
        Some(FormatCandidate::new(InputFormat::Html, 0.96, "HTML/XHTML root element"))
    } else if root.eq_ignore_ascii_case("rss") || root.eq_ignore_ascii_case("feed") {
        Some(FormatCandidate::new(InputFormat::Feed, 0.98, format!("XML {root} root element")))
    } else {
        Some(FormatCandidate::new(InputFormat::Xml, 0.92, format!("XML {root} root element")))
    }
}

fn html_prelude_identifies_html(mut text: &str) -> bool {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix("<?") {
            let Some((_, after)) = rest.split_once("?>") else {
                return false;
            };
            text = after;
        } else if let Some(rest) = text.strip_prefix("<!--") {
            let Some((_, after)) = rest.split_once("-->") else {
                return false;
            };
            text = after;
        } else if let Some(rest) = strip_ascii_case_prefix(text, "<!doctype") {
            let Some((declaration, after)) = rest.split_once('>') else {
                return false;
            };
            if declaration
                .trim_start()
                .split(|character: char| character.is_ascii_whitespace() || character == '[')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("html"))
            {
                return true;
            }
            text = after;
        } else {
            return starts_with_tag(text, "html");
        }
    }
}

fn strip_ascii_case_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        .then(|| &value[prefix.len()..])
}

fn starts_with_tag(text: &str, name: &str) -> bool {
    text.starts_with('<')
        && text.get(1..1 + name.len()).is_some_and(|value| value.eq_ignore_ascii_case(name))
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
        } else if let Some(rest) = strip_ascii_case_prefix(text, "<!doctype") {
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
    let entry_count = match zip_preflight(bytes) {
        Ok(entry_count) if entry_count <= ZIP_INSPECTION_ENTRY_LIMIT => entry_count,
        Ok(entry_count) => {
            candidates[0].diagnostics.push(format!(
                "ZIP inspection stopped before archive construction: {entry_count} entries exceed the {ZIP_INSPECTION_ENTRY_LIMIT} entry limit"
            ));
            return candidates;
        }
        Err(diagnostic) => {
            candidates[0].diagnostics.push(diagnostic);
            return candidates;
        }
    };
    let mut archive = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(error) => {
            candidates[0]
                .diagnostics
                .push(format!("ZIP directory could not be inspected: {error}"));
            return candidates;
        }
    };
    if archive.len() != entry_count {
        candidates[0].diagnostics.push(format!(
            "ZIP entry count changed after validated EOCD preflight: {entry_count} != {}",
            archive.len()
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
    let specialized = inspect_zip_package(&mut archive, &names, &mut candidates[0].diagnostics);
    if specialized.len() == 1 {
        let (format, evidence) = specialized[0];
        candidates.push(FormatCandidate::new(format, 0.99, evidence));
    } else if specialized.len() > 1 {
        candidates[0].diagnostics.push(format!(
            "conflicting package structures detected: {}",
            specialized.iter().map(|(format, _)| format.as_str()).collect::<Vec<_>>().join(",")
        ));
    }
    candidates
}

fn inspect_zip_package(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    names: &[String],
    diagnostics: &mut Vec<String>,
) -> Vec<(InputFormat, &'static str)> {
    let mimetype = read_zip_text(archive, "mimetype", ZIP_MIMETYPE_READ_LIMIT, diagnostics);
    let content_types =
        read_zip_text(archive, "[Content_Types].xml", ZIP_METADATA_READ_LIMIT, diagnostics);
    let mut matches = Vec::new();
    if let Some(content_types) = content_types.as_deref() {
        let ooxml = [
            (
                InputFormat::Docx,
                "word/document.xml",
                &[
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
                    "application/vnd.ms-word.document.macroEnabled.main+xml",
                ][..],
                "validated OOXML Word content type and package part",
            ),
            (
                InputFormat::Pptx,
                "ppt/presentation.xml",
                &[
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
                    "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml",
                    "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml",
                    "application/vnd.ms-powerpoint.slideshow.macroEnabled.main+xml",
                    "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml",
                    "application/vnd.ms-powerpoint.template.macroEnabled.main+xml",
                ][..],
                "validated OOXML presentation content type and package part",
            ),
            (
                InputFormat::Xlsx,
                "xl/workbook.xml",
                &[
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
                    "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
                ][..],
                "validated OOXML workbook content type and package part",
            ),
            (
                InputFormat::Xlsx,
                "xl/workbook.bin",
                &["application/vnd.ms-excel.sheet.binary.macroEnabled.main"][..],
                "validated OOXML binary workbook content type and package part",
            ),
        ];
        for (format, part, content_types_allowed, evidence) in ooxml {
            if names.iter().any(|name| name == part)
                && zip_entry_nonempty(archive, part, diagnostics)
                && content_types_allowed
                    .iter()
                    .any(|content_type| content_type_override(content_types, part, content_type))
                && !matches.iter().any(|(existing, _)| *existing == format)
            {
                matches.push((format, evidence));
            }
        }
    }

    if mimetype.as_deref() == Some("application/epub+zip")
        && let Some(container) =
            read_zip_text(archive, "META-INF/container.xml", ZIP_METADATA_READ_LIMIT, diagnostics)
        && let Some(rootfile) = xml_element_attribute(&container, "rootfile", "full-path")
        && is_safe_archive_name(rootfile)
        && names.iter().any(|name| name == rootfile)
        && zip_entry_nonempty(archive, rootfile, diagnostics)
    {
        matches.push((InputFormat::Epub, "validated EPUB mimetype, container, and rootfile"));
    }

    let odf = [
        (
            "application/vnd.oasis.opendocument.text",
            InputFormat::Odt,
            "validated OpenDocument text package",
        ),
        (
            "application/vnd.oasis.opendocument.spreadsheet",
            InputFormat::Ods,
            "validated OpenDocument spreadsheet package",
        ),
        (
            "application/vnd.oasis.opendocument.presentation",
            InputFormat::Odp,
            "validated OpenDocument presentation package",
        ),
    ];
    for (expected_mimetype, format, evidence) in odf {
        if mimetype.as_deref() == Some(expected_mimetype)
            && zip_entry_nonempty(archive, "content.xml", diagnostics)
            && read_zip_text(archive, "META-INF/manifest.xml", ZIP_METADATA_READ_LIMIT, diagnostics)
                .is_some_and(|manifest| manifest.contains(expected_mimetype))
        {
            matches.push((format, evidence));
        }
    }
    matches
}

fn read_zip_text(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    limit: u64,
    diagnostics: &mut Vec<String>,
) -> Option<String> {
    let mut entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return None,
        Err(error) => {
            diagnostics.push(format!("ZIP {name} could not be opened: {error}"));
            return None;
        }
    };
    let mut raw = Vec::new();
    if let Err(error) = entry.by_ref().take(limit + 1).read_to_end(&mut raw) {
        diagnostics.push(format!("ZIP {name} could not be inspected: {error}"));
        return None;
    }
    if raw.len() as u64 > limit {
        diagnostics.push(format!("ZIP {name} exceeds the {limit} byte read limit"));
        return None;
    }
    if let Ok(value) = String::from_utf8(raw) {
        Some(value.trim().to_owned())
    } else {
        diagnostics.push(format!("ZIP {name} is not UTF-8"));
        None
    }
}

fn zip_entry_nonempty(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    diagnostics: &mut Vec<String>,
) -> bool {
    match archive.by_name(name) {
        Ok(mut entry) => {
            let mut byte = [0_u8; 1];
            entry.read(&mut byte).is_ok_and(|read| read == 1)
        }
        Err(zip::result::ZipError::FileNotFound) => false,
        Err(error) => {
            diagnostics.push(format!("ZIP {name} could not be opened: {error}"));
            false
        }
    }
}

fn content_types_override(document: &str) -> impl Iterator<Item = &str> {
    document.split('<').filter_map(|fragment| {
        let tag = fragment.split_once('>')?.0;
        let element = tag.split_ascii_whitespace().next()?.rsplit(':').next()?;
        element.eq_ignore_ascii_case("Override").then_some(tag)
    })
}

fn content_type_override(document: &str, part: &str, content_type: &str) -> bool {
    let expected_part = format!("/{part}");
    content_types_override(document).any(|tag| {
        xml_attribute(tag, "PartName") == Some(expected_part.as_str())
            && xml_attribute(tag, "ContentType") == Some(content_type)
    })
}

fn xml_element_attribute<'a>(document: &'a str, element: &str, attribute: &str) -> Option<&'a str> {
    document.split('<').find_map(|fragment| {
        let tag = fragment.split_once('>')?.0;
        let name = tag.split_ascii_whitespace().next()?.rsplit(':').next()?;
        name.eq_ignore_ascii_case(element).then(|| xml_attribute(tag, attribute)).flatten()
    })
}

fn xml_attribute<'a>(tag: &'a str, attribute: &str) -> Option<&'a str> {
    let mut offset = 0;
    while let Some(found) = tag[offset..].find(attribute) {
        let start = offset + found;
        let before = tag[..start].chars().next_back();
        let after_name = start + attribute.len();
        if before.is_none_or(char::is_whitespace)
            && tag[after_name..]
                .chars()
                .next()
                .is_some_and(|value| value.is_whitespace() || value == '=')
        {
            let rest = tag[after_name..].trim_start();
            let rest = rest.strip_prefix('=')?.trim_start();
            let quote = rest.chars().next()?;
            if matches!(quote, '\'' | '"') {
                let value = &rest[quote.len_utf8()..];
                return value.split_once(quote).map(|(value, _)| value);
            }
        }
        offset = after_name;
    }
    None
}

fn is_safe_archive_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && name.split('/').all(|component| !matches!(component, "" | "." | ".."))
}

fn zip_preflight(bytes: &[u8]) -> Result<usize, String> {
    const EOCD_MIN_SIZE: usize = 22;
    const MAX_COMMENT_SIZE: usize = u16::MAX as usize;
    let search_start = bytes.len().saturating_sub(EOCD_MIN_SIZE + MAX_COMMENT_SIZE);
    let mut last_error = "ZIP EOCD record is missing or invalid".to_owned();
    for relative in (0..=bytes.len().saturating_sub(search_start + 4)).rev() {
        let eocd = search_start + relative;
        if bytes.get(eocd..eocd + 4) != Some(b"PK\x05\x06") {
            continue;
        }
        let Some(record) = bytes.get(eocd..eocd + EOCD_MIN_SIZE) else {
            continue;
        };
        let comment_size = usize::from(u16::from_le_bytes([record[20], record[21]]));
        if eocd.checked_add(EOCD_MIN_SIZE + comment_size) != Some(bytes.len()) {
            continue;
        }
        match validate_classic_eocd(bytes, eocd, record) {
            Ok(entry_count) => return Ok(entry_count),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn validate_classic_eocd(bytes: &[u8], eocd: usize, record: &[u8]) -> Result<usize, String> {
    let disk = u16::from_le_bytes([record[4], record[5]]);
    let central_disk = u16::from_le_bytes([record[6], record[7]]);
    let disk_entries = u16::from_le_bytes([record[8], record[9]]);
    let total_entries = u16::from_le_bytes([record[10], record[11]]);
    let central_size = u32::from_le_bytes(record[12..16].try_into().map_err(|_| "invalid EOCD")?);
    let central_offset = u32::from_le_bytes(record[16..20].try_into().map_err(|_| "invalid EOCD")?);
    if disk != 0 || central_disk != 0 || disk_entries != total_entries {
        return Err("multi-disk ZIP structure inspection is unsupported".into());
    }
    if total_entries == u16::MAX
        || central_size == u32::MAX
        || central_offset == u32::MAX
        || eocd >= 20 && bytes.get(eocd - 20..eocd - 16) == Some(b"PK\x06\x07")
    {
        return Err("ZIP64 structure inspection is unsupported; using ZIP-only evidence".into());
    }
    let entry_count = usize::from(total_entries);
    let central_size =
        usize::try_from(central_size).map_err(|_| "ZIP central size is too large")?;
    let central_offset =
        usize::try_from(central_offset).map_err(|_| "ZIP central offset is too large")?;
    if central_offset.checked_add(central_size) != Some(eocd) {
        return Err("ZIP central directory does not end at the validated EOCD".into());
    }
    if entry_count > ZIP_INSPECTION_ENTRY_LIMIT {
        return Ok(entry_count);
    }
    let mut cursor = central_offset;
    for _ in 0..entry_count {
        let header =
            bytes.get(cursor..cursor + 46).ok_or("ZIP central directory header is truncated")?;
        if &header[..4] != b"PK\x01\x02" {
            return Err("ZIP central directory entry signature is invalid".into());
        }
        let variable_size = usize::from(u16::from_le_bytes([header[28], header[29]]))
            + usize::from(u16::from_le_bytes([header[30], header[31]]))
            + usize::from(u16::from_le_bytes([header[32], header[33]]));
        cursor = cursor
            .checked_add(46 + variable_size)
            .filter(|end| *end <= eocd)
            .ok_or("ZIP central directory entry exceeds the EOCD boundary")?;
    }
    if cursor != eocd {
        return Err("ZIP central directory count or size is inconsistent".into());
    }
    Ok(entry_count)
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

    fn ooxml_content_type(part: &str, content_type: &str) -> Vec<u8> {
        format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/{part}" ContentType="{content_type}"/></Types>"#
        )
        .into_bytes()
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
    fn mpeg_audio_frame_header_detects_mp3_without_id3() {
        let candidate = magic_candidate(&[0xff, 0xfb, 0x90, 0x64]).unwrap();
        assert_eq!(candidate.format, InputFormat::Audio);
        assert_eq!(candidate.evidence, "MPEG audio frame header");
        assert!(magic_candidate(&[0xff, 0xff, 0xff, 0xff]).is_none());
        assert!(magic_candidate(&[0x12, 0xff, 0xfb, 0x90, 0x64]).is_none());
        assert!(magic_candidate(&[0xff, 0xfb, 0xf0, 0x64]).is_none());
    }

    #[test]
    fn bmp_requires_consistent_file_and_dib_headers() {
        let mut bmp = vec![0_u8; 58];
        bmp[..2].copy_from_slice(b"BM");
        bmp[2..6].copy_from_slice(&58_u32.to_le_bytes());
        bmp[10..14].copy_from_slice(&54_u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40_u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&1_i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&1_i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1_u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24_u16.to_le_bytes());
        assert_eq!(magic_candidate(&bmp).unwrap().format, InputFormat::Image);
        assert!(magic_candidate(b"BM").is_none());
        assert!(magic_candidate(&[b'B', b'M', 0, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0]).is_none());
        bmp[10..14].copy_from_slice(&200_u32.to_le_bytes());
        assert!(magic_candidate(&bmp).is_none());
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
    fn html_detection_handles_bounded_preludes_case_and_xhtml() {
        let fixtures = [
            "\u{feff}  <?xml version='1.0'?><!--lead--><HtMl xmlns='http://www.w3.org/1999/xhtml'>",
            " \n<!--lead--><!DoCtYpE HtMl><HTML>",
            "<?xml version='1.0'?><html xmlns='http://www.w3.org/1999/xhtml'/>",
            "<?xml version='1.0'?><xhtml:html xmlns:xhtml='http://www.w3.org/1999/xhtml'/>",
        ];
        for fixture in fixtures {
            let candidate = structured_text_candidate(fixture.as_bytes()).unwrap();
            assert_eq!(candidate.format, InputFormat::Html);
        }
        assert_eq!(
            structured_text_candidate(b"<?xml version='1.0'?><document/>").unwrap().format,
            InputFormat::Xml
        );
        assert!(!starts_with_tag("xhtml>", "html"));
        assert!(!starts_with_tag("zhtml>", "html"));
        assert_eq!(structured_text_candidate(b"<xhtml>").unwrap().format, InputFormat::Xml);
    }

    #[test]
    fn ambiguous_media_containers_do_not_receive_high_confidence() {
        let ogg = detect_content(b"OggS\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        let ebml = detect_content(b"\x1a\x45\xdf\xa3unknown");
        let iso = detect_content(b"\0\0\0\x10ftypzzzz\0\0\0\0");
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
    fn iso_media_requires_a_complete_bounded_ftyp_box() {
        let audio = detect_content(b"\0\0\0\x10ftypM4A \0\0\0\0");
        assert_eq!(audio[0].format, InputFormat::Audio);
        assert!((audio[0].confidence - 0.96).abs() < f32::EPSILON);
        assert!(detect_content(b"\0\0\0\x10ftypM4A ").is_empty());
        assert!(detect_content(b"\0\0\0\x0cftypM4A ").is_empty());
        assert!(detect_content(b"\0\0\0\x20ftypM4A \0\0\0\0").is_empty());
    }

    #[test]
    fn zip_parts_distinguish_ooxml_epub_and_odf() {
        let detector = ContentFormatDetector;
        let word_types = ooxml_content_type(
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        let fixtures = [
            (
                zip_with(&[
                    ("[Content_Types].xml", word_types.as_slice()),
                    ("word/document.xml", b"<w:document/>"),
                ]),
                InputFormat::Docx,
            ),
            (
                zip_with(&[
                    ("mimetype", b"application/epub+zip"),
                    (
                        "META-INF/container.xml",
                        b"<container><rootfiles><rootfile full-path='OPS/content.opf'/></rootfiles></container>",
                    ),
                    ("OPS/content.opf", b"<package/>"),
                ]),
                InputFormat::Epub,
            ),
            (
                zip_with(&[
                    ("mimetype", b"application/vnd.oasis.opendocument.text"),
                    ("content.xml", b"<office:document-content/>"),
                    (
                        "META-INF/manifest.xml",
                        b"<manifest:file-entry manifest:full-path='/' manifest:media-type='application/vnd.oasis.opendocument.text'/>",
                    ),
                ]),
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
        bytes.extend_from_slice(&22_u16.to_le_bytes());
        bytes.extend_from_slice(b"PK\x05\x06");
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&22_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        assert_eq!(zip_preflight(&bytes).unwrap(), 5000);
        let candidates = detect_zip(&bytes);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].diagnostics[0].contains("5000 entries"));
        assert!(candidates[0].diagnostics[0].contains("before archive construction"));
    }

    #[test]
    fn zip64_safely_skips_structure_inspection() {
        let mut bytes = b"PK\x05\x06".to_vec();
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        let candidates = detect_zip(&bytes);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].diagnostics[0].contains("ZIP64"));
    }

    #[test]
    fn package_detection_rejects_empty_plain_and_conflicting_archives() {
        let word_type = ooxml_content_type(
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        );
        let ppt_type = ooxml_content_type(
            "ppt/presentation.xml",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        );
        let combined_types = format!(
            "<Types>{}{}</Types>",
            String::from_utf8_lossy(&word_type)
                .trim_start_matches(
                    "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">"
                )
                .trim_end_matches("</Types>"),
            String::from_utf8_lossy(&ppt_type)
                .trim_start_matches(
                    "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">"
                )
                .trim_end_matches("</Types>")
        )
        .into_bytes();
        let ordinary = zip_with(&[("ordinary.txt", b"hello")]);
        let empty_part =
            zip_with(&[("[Content_Types].xml", word_type.as_slice()), ("word/document.xml", b"")]);
        for (index, bytes) in [ordinary, empty_part].into_iter().enumerate() {
            let candidates = detect_zip(&bytes);
            assert_eq!(candidates.len(), 1, "fixture {index}");
            assert_eq!(candidates[0].format, InputFormat::Zip);
        }
        let conflict_bytes = zip_with(&[
            ("[Content_Types].xml", combined_types.as_slice()),
            ("word/document.xml", b"<w:document/>"),
            ("ppt/presentation.xml", b"<p:presentation/>"),
        ]);
        let conflict = detect_zip(&conflict_bytes);
        assert_eq!(conflict.len(), 1);
        assert!(conflict[0].diagnostics.iter().any(|value| value.contains("conflicting")));
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
