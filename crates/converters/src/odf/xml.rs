use crate::odf::model::{
    CONFIG_NS, DC_NS, DRAW_NS, FO_NS, MANIFEST_NS, MAX_XML_EVENTS, META_NS, NUMBER_NS, OFFICE_NS,
    PRESENTATION_NS, STYLE_NS, SVG_NS, TABLE_NS, TEXT_NS, XLINK_NS, XML_NS, limit, malformed,
};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use std::borrow::Cow;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Name {
    pub(super) ns: String,
    pub(super) local: String,
}

#[derive(Clone, Debug)]
pub(super) struct Attr {
    pub(super) name: Name,
    pub(super) value: String,
}

#[derive(Clone, Debug)]
pub(super) enum XmlContent {
    Text(String),
    Node(XmlNode),
}

#[derive(Clone, Debug)]
pub(super) struct XmlNode {
    pub(super) name: Name,
    pub(super) attrs: Vec<Attr>,
    pub(super) content: Vec<XmlContent>,
}

impl XmlNode {
    pub(super) fn is(&self, ns: &str, local: &str) -> bool {
        self.name.ns == ns && self.name.local == local
    }
    pub(super) fn attr(&self, ns: &str, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|value| value.name.ns == ns && value.name.local == local)
            .map(|value| value.value.as_str())
    }
    pub(super) fn children(&self) -> impl Iterator<Item = &XmlNode> {
        self.content.iter().filter_map(|value| match value {
            XmlContent::Node(node) => Some(node),
            XmlContent::Text(_) => None,
        })
    }
    pub(super) fn text(&self) -> String {
        let mut output = String::new();
        self.append_text(&mut output);
        output
    }
    pub(super) fn append_text(&self, output: &mut String) {
        for value in &self.content {
            match value {
                XmlContent::Text(text) => output.push_str(text),
                XmlContent::Node(node) => node.append_text(output),
            }
        }
    }
}

pub(super) fn bounded_text(
    node: &XmlNode,
    options: &ConversionOptions,
    part: &str,
) -> Result<String, ConversionError> {
    let value = node.text();
    let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
    if length > options.limits.max_field_bytes {
        Err(limit(
            "max_field_bytes",
            format!("{part}: {length} > {}", options.limits.max_field_bytes),
        ))
    } else {
        Ok(value)
    }
}

pub(super) fn only_child<'a>(
    node: &'a XmlNode,
    ns: &str,
    local: &str,
    part: &str,
) -> Result<&'a XmlNode, ConversionError> {
    let mut values = node.children().filter(|child| child.is(ns, local));
    let value = values
        .next()
        .ok_or_else(|| malformed(Some(part), format!("required {local} element is missing")))?;
    if values.next().is_some() {
        return Err(malformed(Some(part), format!("duplicate {local} element")));
    }
    Ok(value)
}

pub(super) fn contains_element(node: &XmlNode, ns: &str, local: &str) -> bool {
    node.children().any(|child| child.is(ns, local) || contains_element(child, ns, local))
}

#[allow(clippy::too_many_lines)]
pub(super) fn parse_xml(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<XmlNode, ConversionError> {
    reject_dangerous_xml(bytes, part)?;
    let mut reader = NsReader::from_reader(bytes);
    let config = reader.config_mut();
    config.allow_dangling_amp = false;
    config.allow_unmatched_ends = false;
    config.check_end_names = true;
    config.check_comments = true;
    let mut stack: Vec<XmlNode> = Vec::new();
    let mut root = None;
    let mut events = 0_usize;
    loop {
        events = events
            .checked_add(1)
            .ok_or_else(|| limit("xml_events", "ODF XML event count overflow"))?;
        if events > MAX_XML_EVENTS {
            return Err(limit("xml_events", format!("{part} exceeds {MAX_XML_EVENTS} events")));
        }
        if events.is_multiple_of(1024) {
            context.checkpoint()?;
        }
        let event = reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?;
        match event {
            Event::Start(element) => {
                if stack.len() >= usize::from(options.limits.max_nesting_depth) {
                    return Err(limit(
                        "max_nesting_depth",
                        format!(
                            "{} > {} in {part}",
                            stack.len() + 1,
                            options.limits.max_nesting_depth
                        ),
                    ));
                }
                let node = xml_node(&reader, &element, part)?;
                validate_known_namespace(&node.name, part)?;
                stack.push(node);
            }
            Event::Empty(element) => {
                let node = xml_node(&reader, &element, part)?;
                validate_known_namespace(&node.name, part)?;
                attach_node(node, &mut stack, &mut root, part)?;
            }
            Event::End(element) => {
                let actual = resolved_name(&reader, element.name(), part)?;
                let node =
                    stack.pop().ok_or_else(|| malformed(Some(part), "end tag has no start tag"))?;
                if node.name != actual {
                    return Err(malformed(Some(part), "end tag namespace differs from start tag"));
                }
                attach_node(node, &mut stack, &mut root, part)?;
            }
            Event::Text(text) => {
                let value = text
                    .xml_content()
                    .map_err(|error| malformed(Some(part), format!("invalid XML text: {error}")))?;
                validate_xml_chars(&value, part)?;
                attach_text(value, &mut stack, root.is_some(), part)?;
            }
            Event::CData(text) => {
                let value = text
                    .decode()
                    .map_err(|error| malformed(Some(part), format!("invalid CDATA: {error}")))?;
                validate_xml_chars(&value, part)?;
                attach_text(value, &mut stack, root.is_some(), part)?;
            }
            Event::GeneralRef(reference) => {
                let value = decode_reference(reference.as_ref(), part)?;
                attach_text(Cow::Owned(value), &mut stack, root.is_some(), part)?;
            }
            Event::Decl(decl) => {
                if root.is_some() || !stack.is_empty() {
                    return Err(malformed(Some(part), "XML declaration is not first"));
                }
                if decl
                    .version()
                    .map_err(|error| {
                        malformed(Some(part), format!("invalid XML declaration: {error}"))
                    })?
                    .as_ref()
                    != b"1.0"
                {
                    return Err(malformed(Some(part), "only XML 1.0 is supported"));
                }
                if let Some(encoding) = decl.encoding() {
                    let encoding = encoding.map_err(|error| {
                        malformed(Some(part), format!("invalid XML encoding: {error}"))
                    })?;
                    if !encoding.as_ref().eq_ignore_ascii_case(b"UTF-8") {
                        return Err(malformed(Some(part), "ODF XML must be UTF-8"));
                    }
                }
            }
            Event::DocType(_) => return Err(malformed(Some(part), "DOCTYPE is forbidden")),
            Event::PI(_) => {
                return Err(malformed(
                    Some(part),
                    "processing instructions are outside the safe ODF profile",
                ));
            }
            Event::Eof => break,
            Event::Comment(_) => {}
        }
    }
    if !stack.is_empty() {
        return Err(malformed(Some(part), "XML root is incomplete"));
    }
    root.ok_or_else(|| malformed(Some(part), "XML root is missing"))
}

fn attach_node(
    node: XmlNode,
    stack: &mut [XmlNode],
    root: &mut Option<XmlNode>,
    part: &str,
) -> Result<(), ConversionError> {
    if let Some(parent) = stack.last_mut() {
        parent.content.push(XmlContent::Node(node));
    } else if root.replace(node).is_some() {
        return Err(malformed(Some(part), "XML contains multiple roots"));
    }
    Ok(())
}

fn attach_text(
    value: Cow<'_, str>,
    stack: &mut [XmlNode],
    root_seen: bool,
    part: &str,
) -> Result<(), ConversionError> {
    if let Some(parent) = stack.last_mut() {
        if !value.is_empty() {
            parent.content.push(XmlContent::Text(value.into_owned()));
        }
    } else if !value.chars().all(char::is_whitespace)
        || !root_seen && !value.chars().all(char::is_whitespace)
    {
        return Err(malformed(Some(part), "character data outside XML root"));
    }
    Ok(())
}

fn xml_node(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    part: &str,
) -> Result<XmlNode, ConversionError> {
    let name = resolved_name(reader, element.name(), part)?;
    let mut attrs = Vec::new();
    let mut seen = BTreeSet::new();
    for attribute in element.attributes().with_checks(true) {
        let attribute = attribute
            .map_err(|error| malformed(Some(part), format!("invalid XML attribute: {error}")))?;
        let raw = attribute.key.as_ref();
        if raw == b"xmlns" || raw.starts_with(b"xmlns:") {
            continue;
        }
        let (namespace, local) = reader.resolve_attribute(attribute.key);
        let ns = resolved_namespace(namespace, part)?;
        let name = Name { ns, local: utf8_name(local.as_ref(), part)? };
        validate_known_attribute_namespace(&name, part)?;
        if !seen.insert((name.ns.clone(), name.local.clone())) {
            return Err(malformed(Some(part), "duplicate expanded XML attribute"));
        }
        let value = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(|error| {
                malformed(Some(part), format!("invalid XML attribute value: {error}"))
            })?
            .into_owned();
        validate_xml_chars(&value, part)?;
        attrs.push(Attr { name, value });
    }
    Ok(XmlNode { name, attrs, content: Vec::new() })
}

fn resolved_name(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
    part: &str,
) -> Result<Name, ConversionError> {
    let (namespace, local) = reader.resolve_element(name);
    Ok(Name { ns: resolved_namespace(namespace, part)?, local: utf8_name(local.as_ref(), part)? })
}

fn resolved_namespace(value: ResolveResult<'_>, part: &str) -> Result<String, ConversionError> {
    match value {
        ResolveResult::Bound(value) => utf8_name(value.as_ref(), part),
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(prefix) => Err(malformed(
            Some(part),
            format!("undeclared namespace prefix {}", String::from_utf8_lossy(&prefix)),
        )),
    }
}

fn utf8_name(value: &[u8], part: &str) -> Result<String, ConversionError> {
    std::str::from_utf8(value)
        .map(str::to_owned)
        .map_err(|_| malformed(Some(part), "XML name is not UTF-8"))
}

fn validate_known_namespace(name: &Name, part: &str) -> Result<(), ConversionError> {
    if [
        OFFICE_NS,
        TEXT_NS,
        TABLE_NS,
        DRAW_NS,
        PRESENTATION_NS,
        STYLE_NS,
        MANIFEST_NS,
        META_NS,
        DC_NS,
        XLINK_NS,
        SVG_NS,
        FO_NS,
        CONFIG_NS,
        NUMBER_NS,
    ]
    .contains(&name.ns.as_str())
    {
        Ok(())
    } else {
        Err(malformed(
            Some(part),
            format!("element {} uses namespace outside the ODF 1.3 safe profile", name.local),
        ))
    }
}

fn validate_known_attribute_namespace(name: &Name, part: &str) -> Result<(), ConversionError> {
    if name.ns.is_empty()
        || name.ns == XML_NS
        || [
            OFFICE_NS,
            TEXT_NS,
            TABLE_NS,
            DRAW_NS,
            PRESENTATION_NS,
            STYLE_NS,
            MANIFEST_NS,
            META_NS,
            DC_NS,
            XLINK_NS,
            SVG_NS,
            FO_NS,
            CONFIG_NS,
            NUMBER_NS,
        ]
        .contains(&name.ns.as_str())
    {
        Ok(())
    } else {
        Err(malformed(
            Some(part),
            format!("attribute {} uses namespace outside the ODF 1.3 safe profile", name.local),
        ))
    }
}

fn reject_dangerous_xml(bytes: &[u8], part: &str) -> Result<(), ConversionError> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) || bytes.contains(&0) {
        return Err(malformed(Some(part), "ODF XML must be non-NUL UTF-8"));
    }
    let lower = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    if lower.contains("<!doctype")
        || lower.contains("<!entity")
        || lower.contains("<?xml-stylesheet")
    {
        return Err(malformed(
            Some(part),
            "DTD, entities, and stylesheet processing instructions are forbidden",
        ));
    }
    Ok(())
}

fn validate_xml_chars(value: &str, part: &str) -> Result<(), ConversionError> {
    if value.chars().all(|value| {
        matches!(value, '\u{9}' | '\u{a}' | '\u{d}')
            || value >= '\u{20}' && value != '\u{fffe}' && value != '\u{ffff}'
    }) {
        Ok(())
    } else {
        Err(malformed(Some(part), "XML contains a forbidden XML 1.0 character"))
    }
}

fn decode_reference(reference: &[u8], part: &str) -> Result<String, ConversionError> {
    let value = match reference {
        b"amp" => '&',
        b"lt" => '<',
        b"gt" => '>',
        b"apos" => '\'',
        b"quot" => '"',
        raw if raw.starts_with(b"#x") => {
            u32::from_str_radix(std::str::from_utf8(&raw[2..]).unwrap_or(""), 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| malformed(Some(part), "invalid hexadecimal character reference"))?
        }
        raw if raw.starts_with(b"#") => std::str::from_utf8(&raw[1..])
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .and_then(char::from_u32)
            .ok_or_else(|| malformed(Some(part), "invalid decimal character reference"))?,
        _ => return Err(malformed(Some(part), "custom entity references are forbidden")),
    };
    let output = value.to_string();
    validate_xml_chars(&output, part)?;
    Ok(output)
}
