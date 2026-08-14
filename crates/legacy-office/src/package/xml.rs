use super::{MAX_XML_DEPTH, MAX_XML_EVENTS, limit, malformed};
use into_markdown_core::{ConversionError, ExecutionContext};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use std::collections::BTreeSet;

pub(super) fn validate_root_relationships(
    xml: &[u8],
    expected_part: &str,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = true;
    let mut depth = 0_u16;
    let mut events = 0_u64;
    let mut root = false;
    let mut office_documents = 0_u8;
    loop {
        context.checkpoint()?;
        events = events.checked_add(1).ok_or_else(|| malformed("relationships event overflow"))?;
        if events > MAX_XML_EVENTS {
            return Err(limit("normalized_package_xml", "relationships has too many XML events"));
        }
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                depth = depth.checked_add(1).ok_or_else(|| malformed("relationships depth"))?;
                if depth > MAX_XML_DEPTH {
                    return Err(limit("normalized_package_xml", "relationships XML is too deep"));
                }
                if depth == 1 {
                    root = root_namespace(
                        &event,
                        b"Relationships",
                        "http://schemas.openxmlformats.org/package/2006/relationships",
                    )?;
                } else if depth == 2 && event.name().as_ref() == b"Relationship" {
                    inspect_relationship(&event, expected_part, &mut office_documents)?;
                }
            }
            Ok(Event::Empty(event)) => {
                if depth == 1 && event.name().as_ref() == b"Relationship" {
                    inspect_relationship(&event, expected_part, &mut office_documents)?;
                }
            }
            Ok(Event::End(_)) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| malformed("relationships XML is unbalanced"))?;
            }
            Ok(Event::DocType(_)) => return Err(malformed("DTD is forbidden in relationships")),
            Ok(Event::Eof) => break,
            Err(error) => return Err(malformed(format!("invalid relationships XML: {error}"))),
            _ => {}
        }
    }
    if !root || depth != 0 || office_documents != 1 {
        return Err(malformed("root relationships do not uniquely select the expected main part"));
    }
    Ok(())
}

fn inspect_relationship(
    event: &BytesStart<'_>,
    expected_part: &str,
    office_documents: &mut u8,
) -> Result<(), ConversionError> {
    let mut kind = None;
    let mut target = None;
    let mut external = false;
    let mut seen = BTreeSet::new();
    for attribute in event.attributes().with_checks(false) {
        let attribute = attribute.map_err(|_| malformed("invalid relationship attribute"))?;
        if !seen.insert(attribute.key.as_ref().to_vec()) {
            return Err(malformed("duplicate relationship attribute"));
        }
        let value = attribute
            .unescape_value()
            .map_err(|_| malformed("invalid relationship attribute value"))?
            .into_owned();
        match attribute.key.as_ref() {
            b"Type" => kind = Some(value),
            b"Target" => target = Some(value),
            b"TargetMode" => external = value.eq_ignore_ascii_case("External"),
            _ => {}
        }
    }
    let kind = kind.unwrap_or_default();
    if kind.ends_with("/officeDocument") {
        if !matches!(
            kind.as_str(),
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
                | "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument"
        ) || external
            || target.as_deref() != Some(expected_part)
        {
            return Err(malformed("root officeDocument relationship is spoofed or mismatched"));
        }
        *office_documents = office_documents
            .checked_add(1)
            .ok_or_else(|| malformed("too many officeDocument relationships"))?;
    }
    Ok(())
}

pub(super) fn root_namespace(
    event: &BytesStart<'_>,
    name: &[u8],
    namespace: &str,
) -> Result<bool, ConversionError> {
    if event.name().as_ref() != name {
        return Ok(false);
    }
    let mut declared = None;
    let mut seen = BTreeSet::new();
    for attribute in event.attributes().with_checks(false) {
        let attribute = attribute.map_err(|_| malformed("invalid OPC root attribute"))?;
        if !seen.insert(attribute.key.as_ref().to_vec()) {
            return Err(malformed("duplicate OPC root attribute"));
        }
        if attribute.key.as_ref() == b"xmlns" {
            declared = Some(
                attribute
                    .unescape_value()
                    .map_err(|_| malformed("invalid OPC root namespace"))?
                    .into_owned(),
            );
        }
    }
    Ok(declared.as_deref() == Some(namespace))
}
