//! Strict namespace-aware XML helpers for EPUB package documents.

use super::budget::EpubBudget;
use into_markdown_core::ConversionError;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use std::collections::BTreeSet;

pub(super) const XML_NS: &[u8] = b"http://www.w3.org/XML/1998/namespace";

#[derive(Clone, Copy)]
pub(super) enum DoctypePolicy {
    Forbidden,
    Html,
}

#[derive(Default)]
pub(super) struct DocumentEvents {
    event_seen: bool,
    declaration_seen: bool,
    doctype_seen: bool,
}

impl DocumentEvents {
    pub(super) fn validate(
        &mut self,
        event: &Event<'_>,
        root_seen: bool,
        inside_root: bool,
        doctype: DoctypePolicy,
    ) -> Result<(), ConversionError> {
        validate_event_chars(event)?;
        match event {
            Event::Decl(_) => {
                if self.event_seen
                    || self.declaration_seen
                    || self.doctype_seen
                    || root_seen
                    || inside_root
                {
                    return Err(malformed("misplaced or duplicate XML declaration"));
                }
                self.declaration_seen = true;
            }
            Event::DocType(value) => {
                if root_seen || inside_root || self.doctype_seen {
                    return Err(malformed("misplaced or duplicate XML doctype"));
                }
                if !matches!(doctype, DoctypePolicy::Html)
                    || !value.as_ref().eq_ignore_ascii_case(b"html")
                {
                    return Err(malformed("XML doctype is forbidden or unsupported"));
                }
                self.doctype_seen = true;
            }
            Event::PI(_) => return Err(malformed("XML processing instructions are forbidden")),
            Event::Comment(_) if self.declaration_seen && !self.event_seen => {
                self.event_seen = true;
            }
            _ => {}
        }
        if !matches!(event, Event::Decl(_) | Event::Eof) {
            self.event_seen = true;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Name {
    pub(super) namespace: Option<Vec<u8>>,
    pub(super) local: Vec<u8>,
}

impl Name {
    pub(super) fn matches(&self, namespace: Option<&[u8]>, local: &[u8]) -> bool {
        self.namespace.as_deref() == namespace && self.local == local
    }
}

#[derive(Clone, Debug)]
pub(super) struct Attribute {
    pub(super) namespace: Option<Vec<u8>>,
    pub(super) local: Vec<u8>,
    pub(super) value: String,
}

pub(super) fn reader(bytes: &[u8]) -> NsReader<&[u8]> {
    let mut reader = NsReader::from_reader(bytes);
    let config = reader.config_mut();
    config.allow_dangling_amp = false;
    config.allow_unmatched_ends = false;
    config.check_comments = true;
    config.check_end_names = true;
    config.expand_empty_elements = false;
    reader
}

pub(super) fn name(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<Name, ConversionError> {
    let (resolved, local) = reader.resolve_element(element.name());
    Ok(Name { namespace: namespace(resolved)?, local: local.as_ref().to_vec() })
}

pub(super) fn end_name(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
) -> Result<Name, ConversionError> {
    let (resolved, local) = reader.resolve_element(name);
    Ok(Name { namespace: namespace(resolved)?, local: local.as_ref().to_vec() })
}

pub(super) fn attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<Vec<Attribute>, ConversionError> {
    let mut output = Vec::new();
    let mut unique = BTreeSet::new();
    for attribute in element.attributes() {
        let attribute =
            attribute.map_err(|error| malformed(format!("invalid attribute: {error}")))?;
        let raw_name = attribute.key.as_ref();
        if raw_name == b"xmlns" || raw_name.starts_with(b"xmlns:") {
            continue;
        }
        let (resolved, local) = reader.resolve_attribute(attribute.key);
        let namespace = namespace(resolved)?;
        let key = (namespace.clone(), local.as_ref().to_vec());
        if !unique.insert(key.clone()) {
            return Err(malformed("duplicate expanded XML attribute name"));
        }
        let value = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(|error| malformed(format!("invalid XML attribute value: {error}")))?
            .into_owned();
        validate_xml_chars(&value, "attribute")?;
        output.push(Attribute { namespace: key.0, local: key.1, value });
    }
    EpubBudget::attributes(output.len())?;
    Ok(output)
}

pub(super) fn optional<'a>(
    attributes: &'a [Attribute],
    namespace: Option<&[u8]>,
    local: &[u8],
) -> Option<&'a str> {
    attributes
        .iter()
        .find(|attribute| attribute.namespace.as_deref() == namespace && attribute.local == local)
        .map(|attribute| attribute.value.as_str())
}

pub(super) fn required<'a>(
    attributes: &'a [Attribute],
    namespace: Option<&[u8]>,
    local: &[u8],
    label: &str,
) -> Result<&'a str, ConversionError> {
    optional(attributes, namespace, local)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed(format!("{label} is missing")))
}

pub(super) fn decoded_text(event: &Event<'_>) -> Result<Option<String>, ConversionError> {
    match event {
        Event::Text(value) => {
            let value = value
                .xml_content()
                .map_err(|error| malformed(format!("invalid XML text: {error}")))?
                .into_owned();
            validate_xml_chars(&value, "text")?;
            Ok(Some(value))
        }
        Event::CData(value) => {
            let value = value
                .decode()
                .map_err(|error| malformed(format!("invalid XML CDATA: {error}")))?
                .into_owned();
            validate_xml_chars(&value, "CDATA")?;
            Ok(Some(value))
        }
        Event::GeneralRef(value) => {
            let name = std::str::from_utf8(value.as_ref())
                .map_err(|_| malformed("XML entity name is not UTF-8"))?;
            let replacement = match name {
                "amp" => "&",
                "lt" => "<",
                "gt" => ">",
                "apos" => "'",
                "quot" => "\"",
                value if value.starts_with("#x") => {
                    let scalar = u32::from_str_radix(&value[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| malformed("invalid hexadecimal XML character reference"))?;
                    let scalar = scalar.to_string();
                    validate_xml_chars(&scalar, "character reference")?;
                    return Ok(Some(scalar));
                }
                value if value.starts_with('#') => {
                    let scalar = value[1..]
                        .parse::<u32>()
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| malformed("invalid decimal XML character reference"))?;
                    let scalar = scalar.to_string();
                    validate_xml_chars(&scalar, "character reference")?;
                    return Ok(Some(scalar));
                }
                _ => return Err(malformed("custom XML entities are forbidden")),
            };
            Ok(Some(replacement.into()))
        }
        _ => Ok(None),
    }
}

pub(super) fn valid_ncname(value: &str) -> bool {
    let mut characters = value.chars();
    characters.next().is_some_and(xml_name_start) && characters.all(xml_name_character)
}

fn xml_name_start(character: char) -> bool {
    matches!(character,
        'A'..='Z' | '_' | 'a'..='z'
        | '\u{00c0}'..='\u{00d6}' | '\u{00d8}'..='\u{00f6}'
        | '\u{00f8}'..='\u{02ff}' | '\u{0370}'..='\u{037d}'
        | '\u{037f}'..='\u{1fff}' | '\u{200c}'..='\u{200d}'
        | '\u{2070}'..='\u{218f}' | '\u{2c00}'..='\u{2fef}'
        | '\u{3001}'..='\u{d7ff}' | '\u{f900}'..='\u{fdcf}'
        | '\u{fdf0}'..='\u{fffd}' | '\u{10000}'..='\u{effff}'
    )
}

fn xml_name_character(character: char) -> bool {
    xml_name_start(character)
        || matches!(character,
            '-' | '.' | '0'..='9' | '\u{00b7}'
            | '\u{0300}'..='\u{036f}' | '\u{203f}'..='\u{2040}'
        )
}

fn validate_xml_chars(value: &str, label: &str) -> Result<(), ConversionError> {
    if value.chars().all(|character| {
        matches!(character,
            '\u{0009}' | '\u{000a}' | '\u{000d}'
            | '\u{0020}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}'
        )
    }) {
        Ok(())
    } else {
        Err(malformed(format!("XML {label} contains a character forbidden by XML 1.0")))
    }
}

fn validate_event_chars(event: &Event<'_>) -> Result<(), ConversionError> {
    let (bytes, label) = match event {
        Event::Start(value) | Event::Empty(value) => {
            attributes_for_char_validation(value)?;
            (value.as_ref(), "element")
        }
        Event::End(value) => (value.as_ref(), "end tag"),
        Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
            decoded_text(event)?;
            return Ok(());
        }
        Event::Comment(value) => (value.as_ref(), "comment"),
        Event::Decl(value) => (value.as_ref(), "declaration"),
        Event::DocType(value) => (value.as_ref(), "doctype"),
        Event::PI(value) => (value.as_ref(), "processing instruction"),
        Event::Eof => return Ok(()),
    };
    let value =
        std::str::from_utf8(bytes).map_err(|_| malformed(format!("XML {label} is not UTF-8")))?;
    validate_xml_chars(value, label)
}

fn attributes_for_char_validation(element: &BytesStart<'_>) -> Result<(), ConversionError> {
    for attribute in element.attributes() {
        let attribute =
            attribute.map_err(|error| malformed(format!("invalid attribute: {error}")))?;
        let value = attribute
            .unescape_value()
            .map_err(|error| malformed(format!("invalid XML attribute value: {error}")))?;
        validate_xml_chars(&value, "attribute")?;
    }
    Ok(())
}

fn namespace(value: ResolveResult<'_>) -> Result<Option<Vec<u8>>, ConversionError> {
    match value {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(value) => Ok(Some(value.as_ref().to_vec())),
        ResolveResult::Unknown(prefix) => Err(malformed(format!(
            "unbound XML namespace prefix {:?}",
            String::from_utf8_lossy(prefix.as_ref())
        ))),
    }
}

pub(super) fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: None, detail: format!("EPUB XML: {}", detail.into()) }
}
