use super::error::{limit, malformed};
use super::mce::McSelection;
use super::model::{Package, Relationship, Relationships};
use super::schema::{
    CHART_REL, IMAGE_REL, LAYOUT_REL, MASTER_REL, NOTES_REL, OFFICE_REL, SLIDE_REL, THEME_REL,
};
use super::xml::{XmlProfile, preflight_xml};
use super::xml_base::{attr, local, required_attr};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::reader::NsReader;
use std::cmp::Ordering;
use std::path::{Component, Path};

pub(super) fn parse_relationships(
    bytes: &[u8],
    owner: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Relationships, ConversionError> {
    let part = relationship_part(owner)?;
    preflight_xml(bytes, &part, XmlProfile::Relationships, options, context)?;
    let mut reader = NsReader::from_reader(bytes);
    let mut result = Vec::new();
    let mut mc = McSelection::default();
    loop {
        context.checkpoint()?;
        let event =
            reader.read_event().map_err(|error| malformed(Some(&part), error.to_string()))?;
        if mc.skip(&reader, &event, &part)? {
            continue;
        }
        match event {
            Event::Start(element) | Event::Empty(element)
                if local(element.name().as_ref()) == "Relationship" =>
            {
                let id = required_attr(&element, "Id", &part)?;
                let target = required_attr(&element, "Target", &part)?;
                let kind = required_attr(&element, "Type", &part)?;
                reject_spoofed_relationship_type(&kind, &part)?;
                let mode = attr(&element, "TargetMode", &part)?;
                let external = match mode.as_deref() {
                    None | Some("Internal") => false,
                    Some("External") => true,
                    Some(_) => return Err(malformed(Some(&part), "unsupported TargetMode")),
                };
                if !external && !internal_hyperlink_fragment(&kind, &target) {
                    resolve_target(owner, &target)?;
                }
                result.try_reserve(1).map_err(|error| {
                    limit("max_memory_bytes", format!("cannot reserve relationship: {error}"))
                })?;
                result.push(Relationship { id, target, kind, external });
            }
            Event::Eof => break,
            _ => {}
        }
    }
    result.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    if result.windows(2).any(|values| values[0].id == values[1].id) {
        return Err(malformed(Some(&part), "duplicate relationship Id"));
    }
    Ok(result)
}

pub(super) fn internal_hyperlink_fragment(kind: &str, target: &str) -> bool {
    kind.ends_with("/hyperlink")
        && target.strip_prefix('#').is_some_and(|fragment| {
            !fragment.is_empty()
                && !fragment
                    .chars()
                    .any(|value| value.is_control() || matches!(value, '\\' | '/' | ':'))
        })
}

fn reject_spoofed_relationship_type(value: &str, part: &str) -> Result<(), ConversionError> {
    for official in
        [OFFICE_REL, SLIDE_REL, LAYOUT_REL, MASTER_REL, THEME_REL, NOTES_REL, IMAGE_REL, CHART_REL]
    {
        let local = official.rsplit('/').next().expect("official type has local name");
        if value != official && value.rsplit('/').next() == Some(local) {
            return Err(malformed(
                Some(part),
                format!("relationship type spoofing official local name {local}"),
            ));
        }
    }
    Ok(())
}

pub(super) fn require_content_type(
    package: &Package,
    part: &str,
    expected: &str,
) -> Result<(), ConversionError> {
    if package.content_types.content_type(part) == Some(expected) {
        Ok(())
    } else {
        Err(malformed(
            Some("[Content_Types].xml"),
            format!("part {part} has unexpected content type"),
        ))
    }
}

pub(super) fn relationship_by_id<'a>(
    relationships: &'a Relationships,
    id: &str,
) -> Option<&'a Relationship> {
    relationships
        .binary_search_by(|relationship| relationship.id.as_str().cmp(id))
        .ok()
        .map(|index| &relationships[index])
}

pub(super) fn unique_internal<'a>(
    relationships: &'a Relationships,
    kind: &str,
    owner: &str,
) -> Result<Option<&'a Relationship>, ConversionError> {
    let mut values = relationships
        .iter()
        .filter(|relationship| !relationship.external && relationship.kind == kind);
    let value = values.next();
    if values.next().is_some() {
        Err(malformed(
            Some(&relationship_part(owner)?),
            format!("multiple relationships of type {kind}"),
        ))
    } else {
        Ok(value)
    }
}

pub(super) fn unique_relationship<'a>(
    relationships: &'a Relationships,
    relationship_type: &str,
    owner: &str,
) -> Result<Option<&'a Relationship>, ConversionError> {
    let mut values = relationships
        .iter()
        .filter(|relationship| !relationship.external && relationship.kind == relationship_type);
    let value = values.next();
    if values.next().is_some() {
        Err(malformed(
            Some(&relationship_part(owner)?),
            format!("multiple relationships of type {relationship_type}"),
        ))
    } else {
        Ok(value)
    }
}

pub(super) fn is_presentation_main_type(value: &str) -> bool {
    matches!(
        value,
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"
            | "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml"
            | "application/vnd.ms-powerpoint.slideshow.macroEnabled.main+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml"
    )
}

pub(super) fn dangerous_content_type(value: &str) -> bool {
    ascii_contains_ignore_case(value, "vbaproject")
        || ascii_contains_ignore_case(value, "vbadata")
        || ascii_contains_ignore_case(value, "activex")
        || ascii_contains_ignore_case(value, "oleobject")
        || value.eq_ignore_ascii_case("application/vnd.openxmlformats-officedocument.oleobject")
        || value.eq_ignore_ascii_case("application/vnd.openxmlformats-officedocument.package")
        || ascii_contains_ignore_case(value, "macroenabled.template")
}

pub(super) fn dangerous_relationship_type(value: &str) -> bool {
    ascii_contains_ignore_case(value, "vbaproject")
        || ascii_contains_ignore_case(value, "vbadata")
        || ascii_contains_ignore_case(value, "activex")
        || ascii_ends_with_ignore_case(value, "/oleobject")
        || ascii_ends_with_ignore_case(value, "/package")
}

pub(super) fn ascii_contains_ignore_case(value: &str, needle: &str) -> bool {
    value
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn ascii_ends_with_ignore_case(value: &str, suffix: &str) -> bool {
    value
        .as_bytes()
        .get(value.len().saturating_sub(suffix.len())..)
        .is_some_and(|ending| ending.eq_ignore_ascii_case(suffix.as_bytes()))
}

pub(super) fn ascii_case_cmp(left: &str, right: &str) -> Ordering {
    for (left, right) in left.bytes().zip(right.bytes()) {
        let order = left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase());
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

pub(super) fn validate_part_name(name: &str) -> Result<(), ConversionError> {
    if name.is_empty() || name.starts_with('/') || name.contains(['\\', '\0', ':', '?', '#']) {
        return Err(malformed(Some(name), "unsafe ZIP part name"));
    }
    if name.split('/').any(|value| value.is_empty() || matches!(value, "." | "..")) {
        return Err(malformed(Some(name), "unsafe ZIP part path"));
    }
    if Path::new(name).components().any(|value| !matches!(value, Component::Normal(_))) {
        return Err(malformed(Some(name), "unsafe ZIP part path"));
    }
    Ok(())
}

pub(super) fn validate_compression_ratio(
    part: &str,
    expanded: u64,
    compressed: u64,
) -> Result<(), ConversionError> {
    if (compressed == 0 && expanded > 1024)
        || (compressed != 0 && expanded / compressed.max(1) > 1_000)
    {
        Err(limit("archive_compression_ratio", format!("part {part}")))
    } else {
        Ok(())
    }
}

pub(super) fn resolve_target<'a>(
    owner: &'a str,
    target: &'a str,
) -> Result<String, ConversionError> {
    if target.is_empty() || target.contains(['\\', '\0', ':', '?', '#']) {
        return Err(malformed(Some(owner), "unsafe internal relationship target"));
    }
    let package_absolute = target.starts_with('/');
    let target = target.strip_prefix('/').unwrap_or(target);
    if target.is_empty()
        || target.starts_with('/')
        || target.split('/').any(|segment| segment.is_empty() || segment == ".")
    {
        return Err(malformed(Some(owner), "unsafe internal relationship target"));
    }
    let owner_directory = if package_absolute {
        ""
    } else {
        owner.rsplit_once('/').map_or("", |(directory, _)| directory)
    };
    let planned_segments = owner_directory
        .split('/')
        .filter(|value| !value.is_empty())
        .count()
        .checked_add(target.split('/').count())
        .ok_or_else(|| limit("max_memory_bytes", "relationship segment count overflow"))?;
    let mut segments = Vec::<&str>::new();
    segments.try_reserve(planned_segments).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve relationship path: {error}"))
    })?;
    segments.extend(owner_directory.split('/').filter(|value| !value.is_empty()));
    for segment in target.split('/') {
        match segment {
            ".." => {
                if segments.pop().is_none() {
                    return Err(malformed(Some(owner), "relationship escapes package root"));
                }
            }
            value => segments.push(value),
        }
    }
    if segments.is_empty() {
        return Err(malformed(Some(owner), "empty relationship target"));
    }
    let output_len =
        segments.iter().try_fold(segments.len().saturating_sub(1), |total, value| {
            total
                .checked_add(value.len())
                .ok_or_else(|| limit("max_memory_bytes", "relationship path length overflow"))
        })?;
    let mut output = String::new();
    output.try_reserve_exact(output_len).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve relationship target: {error}"))
    })?;
    for (index, segment) in segments.into_iter().enumerate() {
        if index != 0 {
            output.push('/');
        }
        output.push_str(segment);
    }
    Ok(output)
}

pub(super) fn relationship_part(owner: &str) -> Result<String, ConversionError> {
    let (directory, file) = owner.rsplit_once('/').unwrap_or(("", owner));
    let capacity = owner
        .len()
        .checked_add("_rels/.rels".len())
        .ok_or_else(|| limit("max_memory_bytes", "relationship part capacity overflow"))?;
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve relationship part: {error}"))
    })?;
    if directory.is_empty() {
        output.push_str("_rels/");
    } else {
        output.push_str(directory);
        output.push_str("/_rels/");
    }
    output.push_str(file);
    output.push_str(".rels");
    Ok(output)
}
