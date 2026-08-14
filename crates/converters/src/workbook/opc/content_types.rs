use crate::workbook::error::{limit, malformed};
use crate::workbook::opc::package::{canonical_part_name, has_extension, opc_extension};
use crate::workbook::opc::relationships::decode_attr;
use crate::workbook::schema::{CONTENT_TYPES_NS, PACKAGE_REL_CT};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub(in crate::workbook) struct ContentTypeMap {
    defaults: BTreeMap<String, String>,
    overrides: BTreeMap<String, String>,
}

impl ContentTypeMap {
    pub(in crate::workbook) fn for_part<'a>(&'a self, part: &str) -> Option<&'a str> {
        self.overrides
            .get(part)
            .or_else(|| {
                opc_extension(part)
                    .and_then(|extension| self.defaults.get(&extension.to_ascii_lowercase()))
            })
            .map(String::as_str)
    }
}

#[allow(clippy::too_many_lines)] // A single state machine makes OPC authority ordering auditable.
pub(in crate::workbook) fn parse_content_types(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ContentTypeMap, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut output = ContentTypeMap::default();
    let mut depth = 0_u16;
    let mut saw_root = false;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                match namespace {
                    ResolveResult::Bound(value) if value.as_ref() == CONTENT_TYPES_NS => {}
                    ResolveResult::Bound(_)
                    | ResolveResult::Unbound
                    | ResolveResult::Unknown(_) => {
                        return Err(malformed(
                            Some("[Content_Types].xml"),
                            "unexpected or unbound namespace",
                        ));
                    }
                }
                match event.local_name().as_ref() {
                    b"Types" => {
                        if saw_root || depth != 0 || is_empty {
                            return Err(malformed(
                                Some("[Content_Types].xml"),
                                "invalid content-types root",
                            ));
                        }
                        saw_root = true;
                    }
                    b"Override" => {
                        if !saw_root || depth != 1 || !is_empty {
                            return Err(malformed(
                                Some("[Content_Types].xml"),
                                "Override must be a direct Types child",
                            ));
                        }
                        let mut part = None;
                        let mut kind = None;
                        let mut attributes = BTreeSet::new();
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(
                                    Some("[Content_Types].xml"),
                                    format!("attribute: {error}"),
                                )
                            })?;
                            if !attributes.insert(attr.key.as_ref().to_vec()) {
                                return Err(malformed(
                                    Some("[Content_Types].xml"),
                                    "duplicate Override attribute",
                                ));
                            }
                            match attr.key.as_ref() {
                                b"PartName" => {
                                    part = Some(decode_attr(&attr, "[Content_Types].xml")?);
                                }
                                b"ContentType" => {
                                    kind = Some(decode_attr(&attr, "[Content_Types].xml")?);
                                }
                                _ => {
                                    return Err(malformed(
                                        Some("[Content_Types].xml"),
                                        "unexpected Override attribute",
                                    ));
                                }
                            }
                        }
                        let raw_part = part.unwrap_or_default();
                        let kind = kind.unwrap_or_default();
                        if u64::try_from(raw_part.len()).unwrap_or(u64::MAX)
                            > options.limits.max_field_bytes
                            || u64::try_from(kind.len()).unwrap_or(u64::MAX)
                                > options.limits.max_field_bytes
                            || kind.bytes().any(|byte| byte.is_ascii_control())
                        {
                            return Err(limit(
                                "max_field_bytes",
                                "content-type Override field is too large",
                            ));
                        }
                        let part = raw_part
                            .strip_prefix('/')
                            .ok_or_else(|| {
                                malformed(
                                    Some("[Content_Types].xml"),
                                    "Override PartName must be package-absolute",
                                )
                            })
                            .and_then(canonical_part_name)?;
                        if kind.is_empty() || output.overrides.insert(part, kind).is_some() {
                            return Err(malformed(
                                Some("[Content_Types].xml"),
                                "duplicate or empty content-type Override",
                            ));
                        }
                    }
                    b"Default" => {
                        if !saw_root || depth != 1 || !is_empty {
                            return Err(malformed(
                                Some("[Content_Types].xml"),
                                "Default must be a direct Types child",
                            ));
                        }
                        let mut extension = None;
                        let mut content_type = None;
                        let mut attributes = BTreeSet::new();
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(
                                    Some("[Content_Types].xml"),
                                    format!("attribute: {error}"),
                                )
                            })?;
                            if !attributes.insert(attr.key.as_ref().to_vec()) {
                                return Err(malformed(
                                    Some("[Content_Types].xml"),
                                    "duplicate Default attribute",
                                ));
                            }
                            match attr.key.as_ref() {
                                b"Extension" => {
                                    extension = Some(decode_attr(&attr, "[Content_Types].xml")?);
                                }
                                b"ContentType" => {
                                    content_type = Some(decode_attr(&attr, "[Content_Types].xml")?);
                                }
                                _ => {
                                    return Err(malformed(
                                        Some("[Content_Types].xml"),
                                        "unexpected Default attribute",
                                    ));
                                }
                            }
                        }
                        let extension = extension.unwrap_or_default().to_ascii_lowercase();
                        let content_type = content_type.unwrap_or_default();
                        if extension.is_empty()
                            || extension.starts_with('.')
                            || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
                            || content_type.is_empty()
                            || content_type.bytes().any(|byte| byte.is_ascii_control())
                            || u64::try_from(content_type.len()).unwrap_or(u64::MAX)
                                > options.limits.max_field_bytes
                            || output.defaults.insert(extension, content_type).is_some()
                        {
                            return Err(malformed(
                                Some("[Content_Types].xml"),
                                "duplicate or invalid content-type Default",
                            ));
                        }
                    }
                    _ => {
                        return Err(malformed(
                            Some("[Content_Types].xml"),
                            "unexpected content-types element",
                        ));
                    }
                }
                if !is_empty {
                    depth = depth.saturating_add(1);
                    if depth > options.limits.max_nesting_depth {
                        return Err(limit("max_nesting_depth", "content types XML too deep"));
                    }
                }
            }
            Ok((namespace, Event::End(event))) => {
                match namespace {
                    ResolveResult::Bound(value) if value.as_ref() == CONTENT_TYPES_NS => {}
                    _ => {
                        return Err(malformed(
                            Some("[Content_Types].xml"),
                            "unexpected closing namespace",
                        ));
                    }
                }
                if event.local_name().as_ref() != b"Types" || depth != 1 {
                    return Err(malformed(
                        Some("[Content_Types].xml"),
                        "invalid content-types closing element",
                    ));
                }
                depth = 0;
            }
            Ok((_, Event::DocType(_))) => {
                return Err(malformed(Some("[Content_Types].xml"), "DTD is forbidden"));
            }
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(
                    Some("[Content_Types].xml"),
                    format!("invalid XML: {error}"),
                ));
            }
            _ => {}
        }
    }
    if !saw_root || depth != 0 {
        return Err(malformed(Some("[Content_Types].xml"), "invalid Types document"));
    }
    Ok(output)
}

pub(in crate::workbook) fn require_content_type(
    content_types: &ContentTypeMap,
    part: &str,
    allowed: &[&str],
) -> Result<(), ConversionError> {
    let actual = content_types
        .for_part(part)
        .ok_or_else(|| malformed(Some(part), "reachable OPC part has no content type"))?;
    if !allowed.contains(&actual) {
        return Err(malformed(
            Some(part),
            format!("content type {actual} is inconsistent with the relationship"),
        ));
    }
    Ok(())
}

pub(in crate::workbook) fn validate_content_type_authority(
    content_types: &ContentTypeMap,
    package_parts: &BTreeSet<String>,
) -> Result<(), ConversionError> {
    for part in content_types.overrides.keys() {
        if !package_parts.contains(part) {
            return Err(malformed(Some(part), "orphan content-type Override"));
        }
    }
    for part in package_parts {
        if part != "[Content_Types].xml" && content_types.for_part(part).is_none() {
            return Err(malformed(Some(part), "OPC part has no content type authority"));
        }
        if has_extension(part, "rels") {
            require_content_type(content_types, part, &[PACKAGE_REL_CT])?;
        }
    }
    Ok(())
}
