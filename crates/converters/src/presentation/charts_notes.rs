use super::error::{limit, malformed};
use super::mce::McSelection;
use super::shape_elements::append_shape_text;
use super::xml::{XmlProfile, preflight_xml};
use super::xml_base::{local, required_attr};
use crate::docx::{decode_cdata, decode_reference, decode_text};
use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, MAX_DOCUMENT_INLINES,
};
use quick_xml::events::Event;
use quick_xml::reader::NsReader;
use std::collections::HashSet;

#[allow(clippy::too_many_lines)]
pub(super) fn parse_chart_text(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<String>, ConversionError> {
    preflight_xml(bytes, part, XmlProfile::Chart, options, context)?;
    let mut reader = NsReader::from_reader(bytes);
    let mut current_value = None::<String>;
    let mut values = Vec::new();
    let mut cache_indexes = HashSet::<u32>::new();
    let mut in_cache = false;
    let mut point_open = false;
    let mut point_has_value = false;
    let mut mc = McSelection::default();
    loop {
        context.checkpoint()?;
        let event =
            reader.read_event().map_err(|error| malformed(Some(part), error.to_string()))?;
        if mc.skip(&reader, &event, part)? {
            continue;
        }
        match event {
            Event::Start(element)
                if matches!(local(element.name().as_ref()), "strCache" | "numCache") =>
            {
                if in_cache {
                    return Err(malformed(Some(part), "nested chart cache"));
                }
                in_cache = true;
                cache_indexes.clear();
            }
            Event::Start(element) if local(element.name().as_ref()) == "pt" => {
                if !in_cache || point_open {
                    return Err(malformed(Some(part), "chart point is outside its cache"));
                }
                let index = required_attr(&element, "idx", part)?
                    .parse::<u32>()
                    .map_err(|_| malformed(Some(part), "invalid chart point index"))?;
                cache_indexes.try_reserve(1).map_err(|error| {
                    limit("max_memory_bytes", format!("cannot reserve chart point index: {error}"))
                })?;
                if !cache_indexes.insert(index) {
                    return Err(malformed(Some(part), "duplicate chart point index"));
                }
                point_open = true;
                point_has_value = false;
            }
            Event::Start(element) if local(element.name().as_ref()) == "v" => {
                if current_value.is_some()
                    || (point_open && point_has_value)
                    || (in_cache && !point_open)
                {
                    return Err(malformed(Some(part), "nested chart cache value"));
                }
                let mut value = String::new();
                value.try_reserve(64).map_err(|error| {
                    limit("max_memory_bytes", format!("chart value allocation: {error}"))
                })?;
                current_value = Some(value);
            }
            Event::Text(value) if current_value.is_some() => {
                let value = decode_text(&value, part)?;
                append_shape_text(
                    current_value.as_mut().expect("present"),
                    &value,
                    part,
                    options.limits.max_field_bytes,
                )?;
            }
            Event::CData(value) if current_value.is_some() => {
                let value = decode_cdata(&value, part)?;
                append_shape_text(
                    current_value.as_mut().expect("present"),
                    &value,
                    part,
                    options.limits.max_field_bytes,
                )?;
            }
            Event::GeneralRef(value) if current_value.is_some() => {
                let value = decode_reference(&value, part)?;
                append_shape_text(
                    current_value.as_mut().expect("present"),
                    &value,
                    part,
                    options.limits.max_field_bytes,
                )?;
            }
            Event::End(element) if local(element.name().as_ref()) == "v" => {
                let value = current_value
                    .take()
                    .ok_or_else(|| malformed(Some(part), "chart value end without start"))?;
                if u64::try_from(value.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
                    return Err(limit("max_field_bytes", format!("chart text in {part}")));
                }
                let projected_inlines =
                    values.len().checked_mul(2).and_then(|count| count.checked_add(1)).ok_or_else(
                        || limit("max_document_inlines", "chart inline count overflow"),
                    )?;
                if projected_inlines > MAX_DOCUMENT_INLINES {
                    return Err(limit("max_document_inlines", "chart cache exceeds inline budget"));
                }
                values.try_reserve(1).map_err(|error| {
                    limit("max_memory_bytes", format!("chart value list allocation: {error}"))
                })?;
                values.push(value);
                if point_open {
                    point_has_value = true;
                }
            }
            Event::End(element) if local(element.name().as_ref()) == "pt" => {
                if !point_open || !point_has_value || current_value.is_some() {
                    return Err(malformed(Some(part), "chart point lacks one complete value"));
                }
                point_open = false;
            }
            Event::End(element)
                if matches!(local(element.name().as_ref()), "strCache" | "numCache") =>
            {
                if !in_cache || point_open {
                    return Err(malformed(Some(part), "chart cache ended with an open point"));
                }
                in_cache = false;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if in_cache || point_open || current_value.is_some() {
        return Err(malformed(Some(part), "chart cache parser ended in an incomplete state"));
    }
    Ok(values)
}
