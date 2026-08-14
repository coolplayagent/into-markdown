use crate::odf::image_validation::image_profile;
use crate::odf::manifest::ManifestEntry;
use crate::odf::model::{DC_NS, DRAW_NS, OFFICE_NS, XLINK_NS, limit, malformed};
use crate::odf::paths::canonical_part_name;
use crate::odf::xml::{XmlContent, XmlNode, bounded_text};
use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, MAX_DOCUMENT_NODES,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn validate_ranged_annotations(
    root: &XmlNode,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    fn visit(
        node: &XmlNode,
        options: &ConversionOptions,
        context: &ExecutionContext,
        visited: &mut usize,
        annotation_count: &mut usize,
        active: &mut Vec<String>,
        seen: &mut BTreeSet<String>,
    ) -> Result<(), ConversionError> {
        *visited = visited
            .checked_add(1)
            .ok_or_else(|| limit("documentNodes", "annotation scan node count overflow"))?;
        if (*visited).is_multiple_of(256) {
            context.checkpoint()?;
        }
        if node.is(OFFICE_NS, "annotation") {
            validate_annotation_metadata(node, options)?;
            if let Some(name) = node.attr(OFFICE_NS, "name") {
                let length = u64::try_from(name.len()).unwrap_or(u64::MAX);
                if name.is_empty() || length > options.limits.max_field_bytes {
                    return Err(malformed(
                        Some("content.xml"),
                        "ranged annotation has an empty or oversized office:name",
                    ));
                }
                *annotation_count = annotation_count
                    .checked_add(1)
                    .ok_or_else(|| limit("documentNodes", "ranged annotation count overflow"))?;
                if *annotation_count > MAX_DOCUMENT_NODES || !seen.insert(name.to_owned()) {
                    return Err(malformed(
                        Some("content.xml"),
                        "duplicate or excessive ranged annotation name",
                    ));
                }
                active.push(name.to_owned());
            }
        } else if node.is(OFFICE_NS, "annotation-end") {
            if node.content.iter().any(|content| {
                matches!(content, XmlContent::Node(_))
                    || matches!(content, XmlContent::Text(text) if !text.chars().all(char::is_whitespace))
            }) {
                return Err(malformed(
                    Some("content.xml"),
                    "office:annotation-end must be an empty range terminator",
                ));
            }
            let name =
                node.attr(OFFICE_NS, "name").filter(|name| !name.is_empty()).ok_or_else(|| {
                    malformed(Some("content.xml"), "office:annotation-end lacks office:name")
                })?;
            match active.last() {
                Some(current) if current == name => {
                    active.pop();
                }
                Some(_) if active.iter().any(|current| current == name) => {
                    return Err(malformed(
                        Some("content.xml"),
                        "crossing ranged annotations are outside the supported profile",
                    ));
                }
                _ => {
                    return Err(malformed(
                        Some("content.xml"),
                        "annotation-end is dangling or refers to an already closed range",
                    ));
                }
            }
        }
        for child in node.children() {
            visit(child, options, context, visited, annotation_count, active, seen)?;
        }
        Ok(())
    }

    let mut visited = 0_usize;
    let mut annotation_count = 0_usize;
    let mut active = Vec::new();
    let mut seen = BTreeSet::new();
    visit(root, options, context, &mut visited, &mut annotation_count, &mut active, &mut seen)?;
    if active.is_empty() {
        Ok(())
    } else {
        Err(malformed(
            Some("content.xml"),
            "ranged annotation start has no matching annotation-end",
        ))
    }
}

fn validate_annotation_metadata(
    annotation: &XmlNode,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if annotation.content.iter().any(
        |content| matches!(content, XmlContent::Text(text) if !text.chars().all(char::is_whitespace)),
    ) {
        return Err(malformed(
            Some("content.xml"),
            "annotation direct text must be represented by a safe text block",
        ));
    }
    for (namespace, local) in [(DC_NS, "creator"), (DC_NS, "date")] {
        let mut value = None;
        for child in annotation.children().filter(|child| child.is(namespace, local)) {
            if value.replace(child).is_some() {
                return Err(malformed(
                    Some("content.xml"),
                    format!("annotation has duplicate dc:{local}"),
                ));
            }
        }
        if let Some(value) = value {
            bounded_text(value, options, "content.xml")?;
        }
    }
    Ok(())
}

pub(super) fn annotation_text(
    annotation: &XmlNode,
    options: &ConversionOptions,
) -> Result<String, ConversionError> {
    validate_annotation_metadata(annotation, options)?;
    let creator = annotation
        .children()
        .find(|child| child.is(DC_NS, "creator"))
        .map(|child| bounded_text(child, options, "content.xml"))
        .transpose()?;
    let date = annotation
        .children()
        .find(|child| child.is(DC_NS, "date"))
        .map(|child| bounded_text(child, options, "content.xml"))
        .transpose()?;
    let mut body = String::new();
    for child in annotation
        .children()
        .filter(|child| !child.is(DC_NS, "creator") && !child.is(DC_NS, "date"))
    {
        child.append_text(&mut body);
    }
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
        return Err(limit("max_field_bytes", "ODF annotation body exceeds configured field limit"));
    }
    let mut label = String::from("Comment");
    if let Some(creator) = creator.filter(|value| !value.trim().is_empty()) {
        label.push_str(" by ");
        label.push_str(creator.trim());
    }
    if let Some(date) = date.filter(|value| !value.trim().is_empty()) {
        label.push_str(" (");
        label.push_str(date.trim());
        label.push(')');
    }
    label.push(':');
    if !body.trim().is_empty() {
        label.push(' ');
        label.push_str(body.trim());
    }
    Ok(label)
}

pub(super) fn collect_image_anchors(
    root: &XmlNode,
    manifest: &BTreeMap<String, ManifestEntry>,
) -> Result<BTreeSet<String>, ConversionError> {
    let mut nodes = vec![root];
    let mut anchors = BTreeSet::new();
    while let Some(node) = nodes.pop() {
        nodes.extend(node.children());
        if !node.is(DRAW_NS, "image") {
            continue;
        }
        let href = node
            .attr(XLINK_NS, "href")
            .ok_or_else(|| malformed(Some("content.xml"), "draw:image lacks xlink:href"))?;
        if node.attr(XLINK_NS, "type").is_some_and(|value| value != "simple")
            || node.attr(XLINK_NS, "show").is_some()
            || node.attr(XLINK_NS, "actuate").is_some()
        {
            return Err(malformed(
                Some("content.xml"),
                "draw:image must use only an inert simple package relationship",
            ));
        }
        if url::Url::parse(href).is_ok() || href.starts_with('#') {
            return Err(malformed(
                Some("content.xml"),
                "external or fragment images are forbidden",
            ));
        }
        let path = canonical_part_name(href.strip_prefix("./").unwrap_or(href), false)?;
        let media_type = &manifest
            .get(&path)
            .ok_or_else(|| malformed(Some(&path), "image anchor is not manifest-bound"))?
            .media_type;
        image_profile(&path, media_type)?;
        anchors.insert(path);
    }
    Ok(anchors)
}
