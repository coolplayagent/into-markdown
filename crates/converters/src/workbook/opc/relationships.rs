use crate::workbook::budget::checked_field_bytes;
use crate::workbook::error::{limit, malformed};
use crate::workbook::opc::package::canonical_part_name;
use crate::workbook::schema::{PACKAGE_REL_NS, SPREADSHEET_NS, SPREADSHEET_STRICT_NS};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub(in crate::workbook) struct Relationship {
    pub(in crate::workbook) kind: String,
    pub(in crate::workbook) target: String,
    pub(in crate::workbook) external: bool,
}

pub(in crate::workbook) fn decode_attr(
    attr: &quick_xml::events::attributes::Attribute<'_>,
    part: &str,
) -> Result<String, ConversionError> {
    attr.unescape_value()
        .map(std::borrow::Cow::into_owned)
        .map_err(|error| malformed(Some(part), format!("invalid attribute: {error}")))
}

#[allow(clippy::too_many_lines)] // Relationship authority and hierarchy remain one fail-closed pass.
pub(in crate::workbook) fn parse_relationships(
    xml: &[u8],
    owner: &str,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<BTreeMap<String, Relationship>, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut output = BTreeMap::new();
    let mut depth = 0_u8;
    let mut saw_root = false;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                match namespace {
                    ResolveResult::Bound(value) if value.as_ref() == PACKAGE_REL_NS => {}
                    _ => return Err(malformed(Some(part), "invalid relationship namespace")),
                }
                if event.local_name().as_ref() == b"Relationships" {
                    if saw_root || depth != 0 || is_empty {
                        return Err(malformed(Some(part), "invalid Relationships root"));
                    }
                    saw_root = true;
                    depth = 1;
                    continue;
                }
                if event.local_name().as_ref() != b"Relationship"
                    || !saw_root
                    || depth != 1
                    || !is_empty
                {
                    return Err(malformed(Some(part), "invalid relationship element hierarchy"));
                }
                let mut id = None;
                let mut kind = None;
                let mut target = None;
                let mut target_mode = None;
                let mut attributes = BTreeSet::new();
                for attr in event.attributes().with_checks(false) {
                    let attr = attr.map_err(|error| {
                        malformed(Some(part), format!("invalid relationship attribute: {error}"))
                    })?;
                    if !attributes.insert(attr.key.as_ref().to_vec()) {
                        return Err(malformed(Some(part), "duplicate relationship attribute"));
                    }
                    match attr.key.as_ref() {
                        b"Id" => id = Some(decode_attr(&attr, part)?),
                        b"Type" => kind = Some(decode_attr(&attr, part)?),
                        b"Target" => target = Some(decode_attr(&attr, part)?),
                        b"TargetMode" => {
                            target_mode = Some(decode_attr(&attr, part)?);
                        }
                        _ => {
                            return Err(malformed(Some(part), "unexpected relationship attribute"));
                        }
                    }
                }
                let id = id.ok_or_else(|| malformed(Some(part), "relationship id is missing"))?;
                let kind =
                    kind.ok_or_else(|| malformed(Some(part), "relationship type is missing"))?;
                let target = target
                    .ok_or_else(|| malformed(Some(part), "relationship target is missing"))?;
                let external = match target_mode.as_deref() {
                    None => false,
                    Some("External") => true,
                    Some(_) => return Err(malformed(Some(part), "invalid TargetMode")),
                };
                if id.is_empty()
                    || kind.is_empty()
                    || target.is_empty()
                    || id.bytes().any(|byte| byte.is_ascii_control())
                    || kind.bytes().any(|byte| byte.is_ascii_control())
                    || target.bytes().any(|byte| byte.is_ascii_control())
                {
                    return Err(malformed(Some(part), "invalid relationship fields"));
                }
                for (label, value) in [
                    ("relationship id", id.as_str()),
                    ("relationship type", kind.as_str()),
                    ("relationship target", target.as_str()),
                ] {
                    checked_field_bytes(
                        options,
                        label,
                        &[u64::try_from(value.len()).unwrap_or(u64::MAX)],
                    )?;
                }
                let target = if external { target } else { resolve_part_target(owner, &target)? };
                checked_field_bytes(
                    options,
                    "resolved relationship target",
                    &[u64::try_from(target.len()).unwrap_or(u64::MAX)],
                )?;
                if output.insert(id, Relationship { kind, target, external }).is_some() {
                    return Err(malformed(Some(part), "duplicate relationship id"));
                }
            }
            Ok((namespace, Event::End(event))) => {
                match namespace {
                    ResolveResult::Bound(value) if value.as_ref() == PACKAGE_REL_NS => {}
                    _ => return Err(malformed(Some(part), "invalid relationship namespace")),
                }
                if event.local_name().as_ref() != b"Relationships" || depth != 1 {
                    return Err(malformed(Some(part), "invalid relationship closing element"));
                }
                depth = 0;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid relationships XML: {error}")));
            }
            _ => {}
        }
    }
    if !saw_root
        || depth != 0
        || output.len() as u64 > u64::from(options.limits.max_archive_entries)
    {
        return Err(if output.len() as u64 > u64::from(options.limits.max_archive_entries) {
            limit("max_archive_entries", "too many OPC relationships")
        } else {
            malformed(Some(part), "incomplete Relationships document")
        });
    }
    Ok(output)
}

fn resolve_part_target(owner: &str, target: &str) -> Result<String, ConversionError> {
    if target.starts_with('/') || target.contains('\\') || target.contains(['\0', '?', '#']) {
        return Err(malformed(Some(owner), "unsafe internal relationship target"));
    }
    let mut components = owner
        .rsplit_once('/')
        .map_or(Vec::new(), |(parent, _)| parent.split('/').map(str::to_owned).collect());
    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(malformed(Some(owner), "relationship target escapes package root"));
                }
            }
            value => components.push(value.to_owned()),
        }
    }
    if components.is_empty() {
        return Err(malformed(Some(owner), "empty relationship target"));
    }
    canonical_part_name(&components.join("/"))
}

pub(in crate::workbook) fn relationship_part(owner: &str) -> String {
    match owner.rsplit_once('/') {
        Some((parent, filename)) => format!("{parent}/_rels/{filename}.rels"),
        None => format!("_rels/{owner}.rels"),
    }
}

pub(in crate::workbook) fn is_relationship_kind(kind: &str, suffix: &str) -> bool {
    kind == format!("http://schemas.openxmlformats.org/officeDocument/2006/relationships/{suffix}")
        || kind == format!("http://purl.oclc.org/ooxml/officeDocument/relationships/{suffix}")
}

pub(in crate::workbook) fn require_spreadsheet_namespace(
    namespace: &ResolveResult<'_>,
    part: &str,
) -> Result<(), ConversionError> {
    match namespace {
        ResolveResult::Bound(value)
            if value.as_ref() == SPREADSHEET_NS || value.as_ref() == SPREADSHEET_STRICT_NS =>
        {
            Ok(())
        }
        _ => Err(malformed(Some(part), "invalid SpreadsheetML namespace")),
    }
}
