//! Complete, recovery-independent security audit for one XHTML spine member.

use super::budget::EpubBudget;
use super::path::BasePath;
use super::xml;
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use std::mem::size_of;

const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const CHECKPOINT_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy)]
struct Binding<'a> {
    prefix: &'a str,
    uri: &'a str,
}

struct Frame<'a> {
    qname: &'a str,
    binding_start: usize,
    base: Option<BasePath>,
}

#[derive(Clone, Copy)]
struct RawAttribute<'a> {
    name: &'a str,
    value: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ExpandedName<'a> {
    namespace: Option<&'a str>,
    local: &'a str,
}

struct Scratch<'a> {
    frames: Vec<Frame<'a>>,
    bindings: Vec<Binding<'a>>,
    attributes: Vec<RawAttribute<'a>>,
    expanded: Vec<ExpandedName<'a>>,
    memory: ResourceReservation,
}

/// Audit the complete raw chapter independently from the parser used for
/// recovery and rewriting. Local syntax damage never terminates this pass.
pub(super) fn audit(
    path: &str,
    bytes: &[u8],
    archive: &SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let source = std::str::from_utf8(bytes)
        .map_err(|_| xml::malformed("XHTML security scan requires UTF-8 input"))?;
    xml::validate_xml_chars(source, "chapter")?;
    let tags = byte_occurrences(bytes, b'<');
    let attributes = byte_occurrences(bytes, b'=');
    let planned = planned_scratch(tags, attributes, path.len())?;
    let memory = context.reserve_memory(planned)?;
    let mut scratch = Scratch {
        frames: reserve(tags, "XHTML security frame")?,
        bindings: reserve(attributes, "XHTML namespace binding")?,
        attributes: reserve(attributes, "XHTML security attribute")?,
        expanded: reserve(attributes, "XHTML expanded attribute")?,
        memory,
    };
    let actual = scratch_bytes(&scratch)?;
    if actual > planned {
        scratch.memory.grow(actual - planned)?;
    } else if planned > actual {
        scratch.memory.shrink(planned - actual)?;
    }
    let initial_base = BasePath::document(path)?;
    scan(source, archive, budget, context, &initial_base, &mut scratch)
}

fn byte_occurrences(bytes: &[u8], needle: u8) -> usize {
    bytes.split(|byte| *byte == needle).count().saturating_sub(1)
}

fn scan<'a>(
    source: &'a str,
    archive: &SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
    context: &ExecutionContext,
    initial_base: &BasePath,
    scratch: &mut Scratch<'a>,
) -> Result<(), ConversionError> {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if cursor.is_multiple_of(CHECKPOINT_BYTES) {
            context.checkpoint()?;
        }
        let Some(relative) = bytes[cursor..].iter().position(|byte| *byte == b'<') else {
            scan_entities(&source[cursor..])?;
            break;
        };
        let start = cursor + relative;
        scan_entities(&source[cursor..start])?;
        if starts(bytes, start, b"<!--") {
            cursor = find_after(bytes, start + 4, b"-->").unwrap_or(bytes.len());
            continue;
        }
        if starts(bytes, start, b"<![CDATA[") {
            cursor = find_after(bytes, start + 9, b"]]>").unwrap_or(bytes.len());
            continue;
        }
        if starts_ascii_case(bytes, start, b"<!DOCTYPE") {
            let (end, next) = declaration_end(bytes, start + 2);
            let declaration = source[start + 2..end].trim();
            if !declaration.eq_ignore_ascii_case("doctype html") {
                return Err(xml::malformed("XML doctype is forbidden or unsupported"));
            }
            cursor = next;
            continue;
        }
        if starts_ascii_case(bytes, start, b"<!ENTITY") {
            return Err(xml::malformed("custom XML entities are forbidden"));
        }
        if starts(bytes, start, b"<!") {
            return Err(xml::malformed("XHTML contains an unsupported declaration"));
        }
        if starts(bytes, start, b"<?") {
            cursor = find_after(bytes, start + 2, b"?>").unwrap_or(bytes.len());
            continue;
        }
        let (end, next, complete) = tag_end(bytes, start + 1);
        if starts(bytes, start, b"</") {
            close_frame(source[start + 2..end].trim(), scratch);
        } else {
            audit_start_tag(
                source[start + 1..end].trim(),
                complete,
                archive,
                budget,
                initial_base,
                scratch,
            )?;
        }
        cursor = next.max(start + 1);
    }
    context.checkpoint()
}

fn audit_start_tag<'a>(
    content: &'a str,
    complete: bool,
    archive: &SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
    initial_base: &BasePath,
    scratch: &mut Scratch<'a>,
) -> Result<(), ConversionError> {
    scratch.attributes.clear();
    scratch.expanded.clear();
    let (qname, empty, syntax_valid) = parse_start(content, &mut scratch.attributes);
    let scope_valid = complete && syntax_valid && xml::valid_qname(qname.as_bytes());
    budget.checkpoint()?;
    EpubBudget::attributes(scratch.attributes.len())?;
    let binding_start = scratch.bindings.len();
    for attribute in &scratch.attributes {
        if attribute.name == "xmlns" {
            validate_namespace_binding("", attribute.value)?;
            scratch.bindings.push(Binding { prefix: "", uri: attribute.value });
        } else if let Some(prefix) = attribute.name.strip_prefix("xmlns:") {
            if !xml::valid_ncname(prefix) {
                return Err(xml::malformed("invalid XML namespace declaration prefix"));
            }
            validate_namespace_binding(prefix, attribute.value)?;
            scratch.bindings.push(Binding { prefix, uri: attribute.value });
        }
    }
    if xml::valid_qname(qname.as_bytes()) {
        resolve_element(qname, &scratch.bindings)?;
    }
    let inherited_base =
        scratch.frames.iter().rev().find_map(|frame| frame.base.as_ref()).unwrap_or(initial_base);
    let mut base = None;
    for attribute in &scratch.attributes {
        scan_entities(attribute.value)?;
        if attribute.name == "xmlns" || attribute.name.starts_with("xmlns:") {
            continue;
        }
        let Some((prefix, local)) = split_valid_qname(attribute.name) else {
            continue;
        };
        let namespace = resolve_attribute(prefix, &scratch.bindings)?;
        let expanded = ExpandedName { namespace, local };
        if scratch.expanded.contains(&expanded) {
            return Err(xml::malformed("duplicate expanded XML attribute name"));
        }
        scratch.expanded.push(expanded);
        if namespace == Some(XML_NS) && local == "base" {
            let growth = inherited_base
                .retained_path_bytes()
                .checked_add(attribute.value.len())
                .and_then(|bytes| bytes.checked_add(256))
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or_else(memory_overflow)?;
            scratch.memory.grow(growth)?;
            base = Some(inherited_base.apply(attribute.value)?);
        }
    }
    let effective_base =
        if scope_valid { base.as_ref().unwrap_or(inherited_base) } else { inherited_base };
    for attribute in &scratch.attributes {
        let Some((prefix, local)) = split_valid_qname(attribute.name) else {
            continue;
        };
        if attribute.name == "xmlns" || prefix == Some("xmlns") {
            continue;
        }
        let namespace = resolve_attribute(prefix, &scratch.bindings)?;
        if namespace.is_none() && is_container_url_attribute(local) {
            effective_base.resolve(attribute.value)?.require_existing(archive)?;
        }
    }
    if empty || !scope_valid {
        scratch.bindings.truncate(binding_start);
    } else {
        scratch.frames.push(Frame { qname, binding_start, base });
    }
    Ok(())
}

fn close_frame<'a>(content: &'a str, scratch: &mut Scratch<'a>) {
    let qname = content.split_whitespace().next().unwrap_or_default();
    if !xml::valid_qname(qname.as_bytes()) {
        return;
    }
    if let Some(index) = scratch.frames.iter().rposition(|frame| frame.qname == qname) {
        let binding_start = scratch.frames[index].binding_start;
        scratch.frames.truncate(index);
        scratch.bindings.truncate(binding_start);
    }
}

fn parse_start<'a>(content: &'a str, output: &mut Vec<RawAttribute<'a>>) -> (&'a str, bool, bool) {
    let bytes = content.as_bytes();
    let mut cursor = skip_space(bytes, 0);
    let name_start = cursor;
    while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() && bytes[cursor] != b'/' {
        cursor += 1;
    }
    let qname = &content[name_start..cursor];
    let mut empty = false;
    let mut syntax_valid = !qname.is_empty();
    while cursor < bytes.len() {
        cursor = skip_space(bytes, cursor);
        if cursor >= bytes.len() {
            break;
        }
        if bytes[cursor] == b'/' {
            empty = true;
            syntax_valid &= skip_space(bytes, cursor + 1) == bytes.len();
            break;
        }
        let attribute_start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'=' | b'/')
        {
            cursor += 1;
        }
        let name = &content[attribute_start..cursor];
        cursor = skip_space(bytes, cursor);
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            syntax_valid = false;
            continue;
        }
        cursor += 1;
        cursor = skip_space(bytes, cursor);
        if cursor >= bytes.len() {
            break;
        }
        let quote = bytes[cursor];
        let (value_start, value_end) = if matches!(quote, b'\'' | b'"') {
            cursor += 1;
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor] != quote {
                cursor += 1;
            }
            let end = cursor;
            cursor = cursor.saturating_add(1).min(bytes.len());
            (start, end)
        } else {
            syntax_valid = false;
            let start = cursor;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && bytes[cursor] != b'/'
            {
                cursor += 1;
            }
            (start, cursor)
        };
        if !name.is_empty() {
            output.push(RawAttribute { name, value: &content[value_start..value_end] });
        }
    }
    (qname, empty, syntax_valid)
}

fn resolve_element<'a>(
    qname: &'a str,
    bindings: &[Binding<'a>],
) -> Result<Option<&'a str>, ConversionError> {
    let (prefix, _) = split_valid_qname(qname).ok_or_else(|| xml::malformed("invalid QName"))?;
    match prefix {
        Some("xml") => Ok(Some(XML_NS)),
        Some(prefix) => resolve_prefix(prefix, bindings).map(Some),
        None => Ok(bindings.iter().rev().find(|binding| binding.prefix.is_empty()).map(|b| b.uri)),
    }
}

fn resolve_attribute<'a>(
    prefix: Option<&'a str>,
    bindings: &[Binding<'a>],
) -> Result<Option<&'a str>, ConversionError> {
    match prefix {
        Some("xml") => Ok(Some(XML_NS)),
        Some(prefix) => resolve_prefix(prefix, bindings).map(Some),
        None => Ok(None),
    }
}

fn resolve_prefix<'a>(prefix: &str, bindings: &[Binding<'a>]) -> Result<&'a str, ConversionError> {
    bindings
        .iter()
        .rev()
        .find(|binding| binding.prefix == prefix)
        .map(|binding| binding.uri)
        .ok_or_else(|| xml::malformed(format!("unbound XML namespace prefix {prefix:?}")))
}

fn validate_namespace_binding(prefix: &str, uri: &str) -> Result<(), ConversionError> {
    scan_entities(uri)?;
    if prefix == "xmlns"
        || uri == XMLNS_NS
        || prefix == "xml" && uri != XML_NS
        || prefix != "xml" && uri == XML_NS
        || !prefix.is_empty() && uri.is_empty()
    {
        return Err(xml::malformed("invalid reserved XML namespace binding"));
    }
    Ok(())
}

fn scan_entities(value: &str) -> Result<(), ConversionError> {
    let bytes = value.as_bytes();
    let mut cursor = 0;
    while let Some(relative) = bytes[cursor..].iter().position(|byte| *byte == b'&') {
        let start = cursor + relative + 1;
        let Some(end_relative) = bytes[start..].iter().position(|byte| *byte == b';') else {
            break;
        };
        let end = start + end_relative;
        let entity = &value[start..end];
        if !matches!(entity, "amp" | "lt" | "gt" | "apos" | "quot")
            && !valid_character_reference(entity)
        {
            return Err(xml::malformed("custom XML entities are forbidden"));
        }
        cursor = end + 1;
    }
    Ok(())
}

fn valid_character_reference(entity: &str) -> bool {
    entity
        .strip_prefix("#x")
        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
        .or_else(|| entity.strip_prefix('#').and_then(|digits| digits.parse().ok()))
        .and_then(char::from_u32)
        .is_some_and(|character| {
            xml::validate_xml_chars(&character.to_string(), "character reference").is_ok()
        })
}

fn split_valid_qname(value: &str) -> Option<(Option<&str>, &str)> {
    if !xml::valid_qname(value.as_bytes()) {
        return None;
    }
    Some(value.split_once(':').map_or((None, value), |(prefix, local)| (Some(prefix), local)))
}

fn is_container_url_attribute(local: &str) -> bool {
    matches!(local, "href" | "src" | "action" | "poster" | "data" | "formaction")
}

fn tag_end(bytes: &[u8], mut cursor: usize) -> (usize, usize, bool) {
    let mut quote = None;
    while cursor < bytes.len() {
        match (quote, bytes[cursor]) {
            (Some(expected), byte) if byte == expected => quote = None,
            (_, b'<') => return (cursor, cursor, false),
            (None, byte @ (b'\'' | b'"')) => quote = Some(byte),
            (None, b'>') => return (cursor, cursor + 1, true),
            _ => {}
        }
        cursor += 1;
    }
    (bytes.len(), bytes.len(), false)
}

fn declaration_end(bytes: &[u8], mut cursor: usize) -> (usize, usize) {
    let mut quote = None;
    let mut subset = 0_u32;
    while cursor < bytes.len() {
        match (quote, bytes[cursor]) {
            (Some(expected), byte) if byte == expected => quote = None,
            (None, byte @ (b'\'' | b'"')) => quote = Some(byte),
            (None, b'[') => subset = subset.saturating_add(1),
            (None, b']') => subset = subset.saturating_sub(1),
            (None, b'>') if subset == 0 => return (cursor, cursor + 1),
            _ => {}
        }
        cursor += 1;
    }
    (bytes.len(), bytes.len())
}

fn find_after(bytes: &[u8], start: usize, needle: &[u8]) -> Option<usize> {
    bytes[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| start + position + needle.len())
}

fn starts(bytes: &[u8], start: usize, prefix: &[u8]) -> bool {
    bytes.get(start..start.saturating_add(prefix.len())) == Some(prefix)
}

fn starts_ascii_case(bytes: &[u8], start: usize, prefix: &[u8]) -> bool {
    bytes
        .get(start..start.saturating_add(prefix.len()))
        .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
}

fn skip_space(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}

fn planned_scratch(tags: usize, attributes: usize, path: usize) -> Result<u64, ConversionError> {
    tags.checked_mul(size_of::<Frame<'_>>())
        .and_then(|bytes| {
            attributes
                .checked_mul(
                    size_of::<Binding<'_>>()
                        + size_of::<RawAttribute<'_>>()
                        + size_of::<ExpandedName<'_>>(),
                )
                .and_then(|attributes| bytes.checked_add(attributes))
        })
        .and_then(|bytes| bytes.checked_add(path))
        .and_then(|bytes| bytes.checked_add(1_024))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(memory_overflow)
}

fn scratch_bytes(scratch: &Scratch<'_>) -> Result<u64, ConversionError> {
    scratch
        .frames
        .capacity()
        .checked_mul(size_of::<Frame<'_>>())
        .and_then(|bytes| {
            scratch
                .bindings
                .capacity()
                .checked_mul(size_of::<Binding<'_>>())
                .and_then(|value| bytes.checked_add(value))
        })
        .and_then(|bytes| {
            scratch
                .attributes
                .capacity()
                .checked_mul(size_of::<RawAttribute<'_>>())
                .and_then(|value| bytes.checked_add(value))
        })
        .and_then(|bytes| {
            scratch
                .expanded
                .capacity()
                .checked_mul(size_of::<ExpandedName<'_>>())
                .and_then(|value| bytes.checked_add(value))
        })
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(memory_overflow)
}

fn reserve<T>(capacity: usize, label: &str) -> Result<Vec<T>, ConversionError> {
    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|error| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("reserve {label}: {error}"),
    })?;
    Ok(output)
}

fn memory_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "EPUB XHTML security scan memory plan overflowed".into(),
    }
}
