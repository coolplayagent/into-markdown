use super::allocation::try_clone_bytes;
use super::budget::MAX_EXACT_EMU;
use super::error::malformed;
use crate::docx::decode_xml_attribute;
use into_markdown_core::ConversionError;
use quick_xml::events::BytesStart;
use quick_xml::name::{QName, ResolveResult};
use quick_xml::reader::NsReader;

pub(super) fn level_paragraph(local: &[u8]) -> Option<u8> {
    if local.len() == 7
        && local.starts_with(b"lvl")
        && local.ends_with(b"pPr")
        && matches!(local[3], b'1'..=b'9')
    {
        Some(local[3] - b'1')
    } else {
        None
    }
}

pub(super) fn resolved(
    reader: &NsReader<&[u8]>,
    name: QName<'_>,
    part: &str,
) -> Result<(Vec<u8>, Vec<u8>), ConversionError> {
    let (namespace, local) = reader.resolve_element(name);
    let namespace = match namespace {
        ResolveResult::Bound(value) => {
            std::str::from_utf8(value.as_ref())
                .map_err(|_| malformed(Some(part), "XML namespace is not UTF-8"))?;
            try_clone_bytes(value.as_ref(), "XML namespace")?
        }
        ResolveResult::Unbound => Vec::new(),
        ResolveResult::Unknown(prefix) => {
            return Err(malformed(
                Some(part),
                format!("undeclared namespace prefix {}", String::from_utf8_lossy(&prefix)),
            ));
        }
    };
    std::str::from_utf8(local.as_ref())
        .map_err(|_| malformed(Some(part), "XML local name is not UTF-8"))?;
    Ok((namespace, try_clone_bytes(local.as_ref(), "XML local name")?))
}

pub(super) fn attr(
    element: &BytesStart<'_>,
    key: &str,
    part: &str,
) -> Result<Option<String>, ConversionError> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| malformed(Some(part), error.to_string()))?;
        if local(attribute.key.as_ref()) == key {
            if attribute.key.as_ref() != key.as_bytes() {
                return Err(malformed(
                    Some(part),
                    format!("interpreted attribute {key} must be unqualified"),
                ));
            }
            return decode_xml_attribute(attribute.value.as_ref(), part).map(Some);
        }
    }
    Ok(None)
}

pub(super) fn optional_xml_bool(
    element: &BytesStart<'_>,
    key: &str,
    part: &str,
) -> Result<Option<bool>, ConversionError> {
    attr(element, key, part)?
        .map(|value| match value.as_str() {
            "1" | "true" => Ok(true),
            "0" | "false" => Ok(false),
            _ => Err(malformed(Some(part), format!("attribute {key} is not an XML boolean"))),
        })
        .transpose()
}

pub(super) fn required_attr(
    element: &BytesStart<'_>,
    key: &str,
    part: &str,
) -> Result<String, ConversionError> {
    attr(element, key, part)?.ok_or_else(|| malformed(Some(part), format!("element lacks {key}")))
}
pub(super) fn required_attr_ns(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    namespace: &[u8],
    key: &str,
    part: &str,
) -> Result<String, ConversionError> {
    optional_attr_ns(reader, element, namespace, key, part)?
        .ok_or_else(|| malformed(Some(part), format!("element lacks relationship attribute {key}")))
}
pub(super) fn optional_attr_ns(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    namespace: &[u8],
    key: &str,
    part: &str,
) -> Result<Option<String>, ConversionError> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| malformed(Some(part), error.to_string()))?;
        let (resolved, local) = reader.resolve_attribute(attribute.key);
        if local.as_ref() == key.as_bytes()
            && matches!(resolved, ResolveResult::Bound(value) if value.as_ref() == namespace)
        {
            return decode_xml_attribute(attribute.value.as_ref(), part).map(Some);
        }
    }
    Ok(None)
}
pub(super) fn signed_attr(
    element: &BytesStart<'_>,
    key: &str,
    part: &str,
) -> Result<i64, ConversionError> {
    let value = required_attr(element, key, part)?
        .parse()
        .map_err(|_| malformed(Some(part), format!("invalid {key} coordinate")))?;
    if !(-MAX_EXACT_EMU..=MAX_EXACT_EMU).contains(&value) {
        return Err(malformed(Some(part), format!("{key} coordinate exceeds exact range")));
    }
    Ok(value)
}
pub(super) fn local(name: &[u8]) -> &str {
    std::str::from_utf8(name.rsplit(|value| *value == b':').next().unwrap_or(name)).unwrap_or("")
}
