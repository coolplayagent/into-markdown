use crate::workbook::budget::{checked_field_bytes, enforce_grid};
use crate::workbook::cell::parse_cell_range;
use crate::workbook::error::{limit, malformed};
use crate::workbook::model::{BinaryHyperlink, Hyperlink};
use crate::workbook::opc::relationships::{
    Relationship, decode_attr, is_relationship_kind, require_spreadsheet_namespace,
    validate_xml_reference,
};
use crate::workbook::schema::{OFFICE_REL_NS, OFFICE_REL_STRICT_NS};
use into_markdown_core::{ConversionError, ConversionOptions, ErrorPolicy, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use std::collections::BTreeMap;

pub(super) fn parse_sheet_hyperlinks(
    xml: &[u8],
    part: &str,
    relationships: &BTreeMap<String, Relationship>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<Hyperlink>, usize), ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut output = Vec::new();
    let mut omitted = 0_usize;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event) | Event::Empty(event)))
                if event.local_name().as_ref() == b"hyperlink" =>
            {
                if let Err(error) = require_spreadsheet_namespace(&namespace, part) {
                    if options.error_policy == ErrorPolicy::BestEffort {
                        continue;
                    }
                    return Err(error);
                }
                match parse_hyperlink_event(&reader, &event, part, relationships, options) {
                    Ok(hyperlink) => output.push(hyperlink),
                    Err(
                        ConversionError::Malformed { .. } | ConversionError::Unsupported { .. },
                    ) if options.error_policy == ErrorPolicy::BestEffort => {
                        omitted = omitted.saturating_add(1);
                    }
                    Err(error) => return Err(error),
                }
                if u32::try_from(output.len()).unwrap_or(u32::MAX)
                    > options.limits.max_archive_entries
                {
                    return Err(limit("max_archive_entries", "too many hyperlinks"));
                }
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::GeneralRef(reference))) => {
                validate_xml_reference(reference.as_ref(), part)?;
            }
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(
                    Some(part),
                    format!("invalid worksheet hyperlinks: {error}"),
                ));
            }
            _ => {}
        }
    }
    Ok((output, omitted))
}

fn parse_hyperlink_event(
    reader: &quick_xml::reader::NsReader<&[u8]>,
    event: &quick_xml::events::BytesStart<'_>,
    part: &str,
    relationships: &BTreeMap<String, Relationship>,
    options: &ConversionOptions,
) -> Result<Hyperlink, ConversionError> {
    let mut reference = None;
    let mut relationship_id = None;
    let mut location = None;
    let mut label = None;
    let mut tooltip = None;
    for attr in event.attributes().with_checks(false) {
        let attr = attr.map_err(|error| {
            malformed(Some(part), format!("invalid hyperlink attribute: {error}"))
        })?;
        let value = decode_attr(&attr, part)?;
        match attr.key.local_name().as_ref() {
            b"ref" => reference = Some(value),
            b"location" => location = Some(value),
            b"display" => label = Some(value),
            b"tooltip" => tooltip = Some(value),
            b"id"
                if matches!(
                    reader.resolve_attribute(attr.key),
                    (ResolveResult::Bound(namespace), _)
                        if namespace.as_ref() == OFFICE_REL_NS
                            || namespace.as_ref() == OFFICE_REL_STRICT_NS
                ) =>
            {
                relationship_id = Some(value);
            }
            _ => {}
        }
    }
    let reference =
        reference.ok_or_else(|| malformed(Some(part), "hyperlink cell range is missing"))?;
    let (start, end) = parse_cell_range(&reference)?;
    enforce_grid(u64::from(end.0) + 1, u64::from(end.1) + 1, options)?;
    let target = if let Some(id) = relationship_id {
        let relationship = relationships
            .get(&id)
            .ok_or_else(|| malformed(Some(part), format!("missing hyperlink relationship {id}")))?;
        if !relationship.external || !is_relationship_kind(&relationship.kind, "hyperlink") {
            return Err(malformed(Some(part), "invalid hyperlink relationship"));
        }
        safe_hyperlink_target(&relationship.target, location.as_deref(), options)?
    } else {
        let location = location
            .ok_or_else(|| malformed(Some(part), "internal hyperlink location is missing"))?;
        safe_hyperlink_target("", Some(&location), options)?
    };
    checked_field_bytes(
        options,
        "hyperlink target",
        &[u64::try_from(target.len()).unwrap_or(u64::MAX)],
    )?;
    for value in label.iter().chain(tooltip.iter()) {
        if u64::try_from(value.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
            return Err(limit("max_field_bytes", "hyperlink label is too large"));
        }
    }
    Ok(Hyperlink { start, end, target, label })
}

pub(super) fn parse_sheet_drawing_ids(
    xml: &[u8],
    part: &str,
    context: &ExecutionContext,
) -> Result<Vec<String>, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut output = Vec::new();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event) | Event::Empty(event)))
                if event.local_name().as_ref() == b"drawing" =>
            {
                require_spreadsheet_namespace(&namespace, part)?;
                let mut relationship_id = None;
                for attr in event.attributes().with_checks(false) {
                    let attr = attr.map_err(|error| {
                        malformed(Some(part), format!("invalid drawing attribute: {error}"))
                    })?;
                    if attr.key.local_name().as_ref() == b"id"
                        && matches!(
                            reader.resolve_attribute(attr.key),
                            (ResolveResult::Bound(namespace), _)
                                if namespace.as_ref() == OFFICE_REL_NS
                                    || namespace.as_ref() == OFFICE_REL_STRICT_NS
                        )
                        && relationship_id.replace(decode_attr(&attr, part)?).is_some()
                    {
                        return Err(malformed(Some(part), "duplicate drawing relationship id"));
                    }
                }
                let relationship_id = relationship_id
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| malformed(Some(part), "drawing relationship id is missing"))?;
                if !output.is_empty() {
                    return Err(malformed(Some(part), "worksheet contains multiple drawing parts"));
                }
                output.push(relationship_id);
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid worksheet XML: {error}")));
            }
            _ => {}
        }
    }
    Ok(output)
}

pub(super) fn resolve_binary_hyperlinks(
    hyperlinks: Vec<BinaryHyperlink>,
    part: &str,
    relationships: &BTreeMap<String, Relationship>,
    options: &ConversionOptions,
) -> Result<Vec<Hyperlink>, ConversionError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(hyperlinks.len())
        .map_err(|_| limit("max_memory_bytes", "XLSB hyperlink inventory allocation failed"))?;
    for hyperlink in hyperlinks {
        for value in [&hyperlink.location, &hyperlink.tooltip, &hyperlink.display] {
            if u64::try_from(value.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
                return Err(limit("max_field_bytes", "XLSB hyperlink text is too large"));
            }
        }
        let target = if let Some(relationship_id) = hyperlink.relationship_id {
            let relationship = relationships.get(&relationship_id).ok_or_else(|| {
                malformed(
                    Some(part),
                    format!("missing XLSB hyperlink relationship {relationship_id}"),
                )
            })?;
            if !relationship.external || !is_relationship_kind(&relationship.kind, "hyperlink") {
                return Err(malformed(Some(part), "invalid XLSB hyperlink relationship"));
            }
            safe_hyperlink_target(&relationship.target, Some(&hyperlink.location), options)?
        } else {
            safe_hyperlink_target("", Some(&hyperlink.location), options)?
        };
        checked_field_bytes(
            options,
            "XLSB hyperlink target",
            &[u64::try_from(target.len()).unwrap_or(u64::MAX)],
        )?;
        output.push(Hyperlink {
            start: hyperlink.start,
            end: hyperlink.end,
            target,
            label: (!hyperlink.display.is_empty()).then_some(hyperlink.display),
        });
    }
    Ok(output)
}

#[allow(clippy::too_many_lines)] // One linear record-state machine keeps fail-closed ordering visible.
fn safe_hyperlink_target(
    base: &str,
    location: Option<&str>,
    options: &ConversionOptions,
) -> Result<String, ConversionError> {
    if !base.is_empty() {
        let url = url::Url::parse(base).map_err(|_| ConversionError::Unsupported {
            detail: "workbook hyperlink is not an absolute safe URL".into(),
        })?;
        if !matches!(url.scheme(), "http" | "https" | "mailto") {
            return Err(ConversionError::Unsupported {
                detail: format!("workbook hyperlink scheme {} is forbidden", url.scheme()),
            });
        }
    }
    let location = location.filter(|value| !value.is_empty());
    if let Some(location) = location
        && (location.bytes().any(|byte| byte.is_ascii_control()) || location.len() > 2_083)
    {
        return Err(malformed(None, "invalid hyperlink fragment"));
    }
    let base_bytes =
        if base.is_empty() { 1 } else { u64::try_from(base.len()).unwrap_or(u64::MAX) };
    let location_bytes = location
        .map_or(0, |value| u64::try_from(value.trim_start_matches('#').len()).unwrap_or(u64::MAX));
    let separator_bytes = u64::from(location.is_some() && !base.is_empty() && !base.ends_with('#'));
    checked_field_bytes(
        options,
        "hyperlink target",
        &[base_bytes, separator_bytes, location_bytes],
    )?;
    let mut target = if base.is_empty() { "#".to_owned() } else { base.to_owned() };
    if let Some(location) = location {
        if !target.ends_with('#') {
            target.push('#');
        }
        target.push_str(location.trim_start_matches('#'));
    }
    Ok(target)
}

#[cfg(test)]
pub(super) fn safe_hyperlink_target_for_test(
    base: &str,
    location: Option<&str>,
    options: &ConversionOptions,
) -> Result<String, ConversionError> {
    safe_hyperlink_target(base, location, options)
}
