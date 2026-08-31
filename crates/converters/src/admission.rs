//! Built-in support admission and signature authority shared by every engine entrypoint.

use super::media_type::format_from_media_type;
use into_markdown_core::{
    ConversionError, DetectionAuthority, ExecutionContext, FormatCandidate, FormatDetection,
    FormatHint, InputFormat, ResolvedInput,
};

pub(super) fn format_from_extension(extension: &str) -> Option<InputFormat> {
    let extension = extension.trim_start_matches('.');
    super::core_format_catalog().iter().find_map(|entry| {
        entry
            .descriptor
            .extensions
            .iter()
            .any(|known| known.eq_ignore_ascii_case(extension))
            .then_some(entry.descriptor.format)
    })
}

pub(super) fn binary_candidates(
    bytes: &[u8],
    compatible_hints: &mut Vec<InputFormat>,
) -> Option<Vec<FormatCandidate>> {
    if let Some(signature) = into_markdown_core::RarSignature::detect(bytes) {
        return Some(vec![FormatCandidate::new(
            InputFormat::Rar,
            1.0,
            format!("RAR signature: {signature:?}"),
        )]);
    }
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        return Some(super::detection::detect_zip_with_hints(bytes, compatible_hints));
    }
    if bytes.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
        return Some(super::detect_ole(bytes));
    }
    super::magic_candidate(bytes).map(|candidate| vec![candidate])
}

pub(super) fn detect(
    input: &ResolvedInput,
    hint: &FormatHint,
    context: &ExecutionContext,
) -> Result<FormatDetection, ConversionError> {
    context.checkpoint()?;
    let mut compatible_hints = Vec::new();
    if let Some(candidates) = binary_candidates(&input.bytes, &mut compatible_hints) {
        return Ok(binary_detection(input, hint, candidates, compatible_hints));
    }
    let extension = input_extension(input, hint);
    let media_type = hint.media_type.as_deref().or(input.metadata.media_type.as_deref());
    let concrete_mime =
        media_type.and_then(|value| value.split(';').next()).map(str::trim).filter(|value| {
            !value.is_empty() && !value.eq_ignore_ascii_case("application/octet-stream")
        });
    let unsupported = hint.format.is_none()
        && match extension {
            Some(extension) => {
                format_from_extension(extension).is_none_or(|format| !format_supported(format))
            }
            None => concrete_mime.is_some_and(|mime| {
                format_from_media_type(mime).is_none_or(|format| !format_supported(format))
            }),
        };
    if unsupported {
        return Ok(FormatDetection {
            candidates: Vec::new(),
            authority: DetectionAuthority::Content,
            compatible_hints: Vec::new(),
            unsupported_reason: Some(unsupported_detail(
                input,
                hint,
                "no supported file signature or container identity",
            )),
        });
    }
    let mut authority = DetectionAuthority::Heuristic;
    let candidates = super::detection::detect_text(&input.bytes, context, &mut authority)?;
    Ok(FormatDetection {
        candidates,
        authority,
        compatible_hints: Vec::new(),
        unsupported_reason: None,
    })
}

pub(super) fn complete_xml(
    bytes: &[u8],
    context: &ExecutionContext,
    authority: &mut DetectionAuthority,
) -> Result<bool, ConversionError> {
    let complete = super::structured::xml_complete_for_detection(bytes, context)?;
    if complete {
        *authority = DetectionAuthority::StructuredText;
    }
    Ok(complete)
}

pub(super) fn utf16_xml_candidate(
    bytes: &[u8],
    context: &ExecutionContext,
    authority: &mut DetectionAuthority,
) -> Result<Option<FormatCandidate>, ConversionError> {
    if let Some(decoded) = super::structured::decode_xml_for_detection(bytes, context)? {
        match decoded {
            super::structured::XmlDetectionText::Decoded(decoded) => {
                let text = decoded.trim_start();
                if let Some(root) = super::xml_root_name(text) {
                    return Ok(Some(if complete_xml(bytes, context, authority)? {
                        FormatCandidate::new(
                            InputFormat::Xml,
                            0.92,
                            format!("complete UTF-16 XML {root} root element"),
                        )
                    } else {
                        FormatCandidate::new(
                            InputFormat::Xml,
                            0.50,
                            format!("incomplete UTF-16 XML {root} root element"),
                        )
                        .with_diagnostic(
                            "incomplete XML evidence does not override a filename extension",
                        )
                    }));
                }
                if super::strong_xml_prefix(text) {
                    return Ok(Some(FormatCandidate::new(
                        InputFormat::Xml,
                        0.50,
                        "incomplete UTF-16 XML declaration or paired markup",
                    )));
                }
            }
            super::structured::XmlDetectionText::InvalidUtf16 => {
                return Ok(Some(FormatCandidate::new(
                    InputFormat::Xml,
                    0.50,
                    "UTF-16 XML signature with invalid encoded content",
                )));
            }
        }
    }

    Ok(None)
}

fn binary_detection(
    input: &ResolvedInput,
    hint: &FormatHint,
    candidates: Vec<FormatCandidate>,
    mut compatible_hints: Vec<InputFormat>,
) -> FormatDetection {
    if input.bytes.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
        compatible_hints = candidates.iter().map(|candidate| candidate.format).collect();
    }
    let candidates_are_package = candidates.iter().any(|candidate| {
        matches!(
            candidate.format,
            InputFormat::Doc
                | InputFormat::Ppt
                | InputFormat::Xls
                | InputFormat::OutlookMsg
                | InputFormat::Docx
                | InputFormat::Pptx
                | InputFormat::Xlsx
                | InputFormat::Odt
                | InputFormat::Odp
                | InputFormat::Ods
                | InputFormat::Epub
        )
    });
    FormatDetection {
        compatible_hints,
        unsupported_reason: (candidates.is_empty() && hint.format.is_none()).then(|| {
            unsupported_detail(input, hint, "compound input has no supported document identity")
        }),
        candidates,
        authority: if candidates_are_package {
            DetectionAuthority::Container
        } else {
            DetectionAuthority::Signature
        },
    }
}

pub(super) fn structured_candidate(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<FormatCandidate>, ConversionError> {
    super::structured_text_candidate(bytes, context, &mut DetectionAuthority::Heuristic)
}

pub(super) fn hint_mime_conflict(extension: Option<&str>, media_type: Option<&str>) -> bool {
    extension.and_then(format_from_extension).is_some()
        && media_type.and_then(|value| value.split(';').next()).map(str::trim).is_some_and(|mime| {
            !mime.is_empty()
                && !mime.eq_ignore_ascii_case("application/octet-stream")
                && format_from_media_type(mime).is_none()
        })
}

fn input_extension<'a>(input: &'a ResolvedInput, hint: &'a FormatHint) -> Option<&'a str> {
    hint.extension
        .as_deref()
        .or_else(|| hint.filename.as_deref().and_then(super::extension_of))
        .or_else(|| input.metadata.name.as_deref().and_then(super::extension_of))
        .filter(|value| !value.trim_start_matches('.').is_empty())
}

fn unsupported_detail(input: &ResolvedInput, hint: &FormatHint, reason: &str) -> String {
    format!(
        "unsupported input {:?} (extension {:?}, media type {:?}): {reason}; use an explicit format to interpret the content",
        hint.filename.as_deref().or(input.metadata.name.as_deref()).unwrap_or("unnamed input"),
        input_extension(input, hint),
        hint.media_type.as_deref().or(input.metadata.media_type.as_deref()),
    )
}

fn format_supported(format: InputFormat) -> bool {
    super::core_format_catalog().iter().any(|entry| {
        entry.descriptor.format == format
            && entry.descriptor.status == super::FormatStatus::Available
    })
}

/// Incomplete container inspection retains document hints for parser-specific safety errors.
pub(super) fn zip_package_hints() -> Vec<InputFormat> {
    vec![
        InputFormat::Docx,
        InputFormat::Pptx,
        InputFormat::Xlsx,
        InputFormat::Odt,
        InputFormat::Ods,
        InputFormat::Odp,
        InputFormat::Epub,
    ]
}

/// A completely inspected generic archive must not be mistaken for a document package.
pub(super) fn zip_package_hint_matches(format: InputFormat, names: &[String]) -> bool {
    names.iter().any(|name| match format {
        InputFormat::Docx => name == "[Content_Types].xml" || name.starts_with("word/"),
        InputFormat::Pptx => name == "[Content_Types].xml" || name.starts_with("ppt/"),
        InputFormat::Xlsx => name == "[Content_Types].xml" || name.starts_with("xl/"),
        InputFormat::Epub => name == "mimetype" || name == "META-INF/container.xml",
        InputFormat::Odt | InputFormat::Ods | InputFormat::Odp => {
            name == "mimetype" || name == "content.xml" || name == "META-INF/manifest.xml"
        }
        _ => false,
    })
}
