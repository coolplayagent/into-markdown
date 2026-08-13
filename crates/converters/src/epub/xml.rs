//! Strict namespace-aware XML helpers for EPUB package documents.

use super::budget::EpubBudget;
use into_markdown_core::ConversionError;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use std::collections::BTreeSet;

pub(super) const XML_NS: &[u8] = b"http://www.w3.org/XML/1998/namespace";

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
        if value.chars().any(|character| {
            character == '\0' || character.is_control() && !character.is_whitespace()
        }) {
            return Err(malformed("XML attribute contains a forbidden control character"));
        }
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
        Event::Text(value) => value
            .xml_content()
            .map(|value| Some(value.into_owned()))
            .map_err(|error| malformed(format!("invalid XML text: {error}"))),
        Event::CData(value) => value
            .decode()
            .map(|value| Some(value.into_owned()))
            .map_err(|error| malformed(format!("invalid XML CDATA: {error}"))),
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
                    return Ok(Some(scalar.to_string()));
                }
                value if value.starts_with('#') => {
                    let scalar = value[1..]
                        .parse::<u32>()
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| malformed("invalid decimal XML character reference"))?;
                    return Ok(Some(scalar.to_string()));
                }
                _ => return Err(malformed("custom XML entities are forbidden")),
            };
            Ok(Some(replacement.into()))
        }
        _ => Ok(None),
    }
}

pub(super) fn reject_active(event: &Event<'_>) -> Result<(), ConversionError> {
    if matches!(event, Event::DocType(_) | Event::PI(_)) {
        return Err(malformed("DTD and processing instructions are forbidden in EPUB metadata"));
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
