use super::error::{limit, malformed};
use super::model::{GroupTransform, Package, Relationships, RichStyle, Shape, ShapeRecovery};
use super::relationships::{relationship_by_id, require_content_type, resolve_target};
use super::schema::{
    CHART_REL, EXPLICIT_BULLET, EXPLICIT_LIST_LEVEL, GEOMETRY_EXTENT, GEOMETRY_FLIP_H,
    GEOMETRY_FLIP_V, GEOMETRY_OFFSET, GEOMETRY_ROTATION, IMAGE_REL, R_NS, SEEN_CHILD_EXTENT,
    SEEN_CHILD_OFFSET, SEEN_EXTENT, SEEN_OFFSET, SEEN_PLACEHOLDER, SEEN_TABLE, SEEN_TRANSFORM,
};
use super::xml_base::{
    attr, local, optional_attr_ns, optional_xml_bool, required_attr, required_attr_ns, signed_attr,
};
use crate::docx::supported_image;
use into_markdown_core::{
    ConversionError, ConversionOptions, ErrorPolicy, ExecutionContext, InlineMark, ListKind,
    MAX_DOCUMENT_INLINES,
};
use quick_xml::events::BytesStart;
use quick_xml::reader::NsReader;
use std::collections::HashSet;

pub(super) fn add_parsed_inline(count: &mut usize) -> Result<(), ConversionError> {
    *count = count
        .checked_add(1)
        .ok_or_else(|| limit("max_document_inlines", "parsed inline count overflow"))?;
    if *count > MAX_DOCUMENT_INLINES {
        return Err(limit("max_document_inlines", "PresentationML part exceeds inline budget"));
    }
    Ok(())
}

pub(super) fn append_shape_text(
    destination: &mut String,
    value: &str,
    part: &str,
    max_field_bytes: u64,
) -> Result<(), ConversionError> {
    let next = destination
        .len()
        .checked_add(value.len())
        .ok_or_else(|| limit("max_field_bytes", format!("text length overflow in {part}")))?;
    if u64::try_from(next).unwrap_or(u64::MAX) > max_field_bytes {
        return Err(limit("max_field_bytes", format!("text in {part}")));
    }
    destination.try_reserve(value.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve shape text: {error}"))
    })?;
    destination.push_str(value);
    Ok(())
}

pub(super) fn mark_semantic_once(
    seen: &mut u8,
    flag: u8,
    part: &str,
    duplicate: &'static str,
) -> Result<(), ConversionError> {
    if *seen & flag != 0 {
        return Err(malformed(Some(part), duplicate));
    }
    *seen |= flag;
    Ok(())
}

pub(super) fn apply_group_element(
    element: &BytesStart<'_>,
    part: &str,
    group: &mut GroupTransform,
) -> Result<(), ConversionError> {
    match local(element.name().as_ref()) {
        "cNvPr" => {
            group.hidden |= optional_xml_bool(element, "hidden", part)?.unwrap_or(false);
        }
        "xfrm" => {
            mark_semantic_once(
                &mut group.semantic_seen,
                SEEN_TRANSFORM,
                part,
                "group has multiple transforms",
            )?;
            group.rotation = attr(element, "rot", part)?
                .map(|value| {
                    value
                        .parse::<i32>()
                        .map_err(|_| malformed(Some(part), "invalid group rotation"))
                })
                .transpose()?
                .unwrap_or(0);
            group.flip_h = optional_xml_bool(element, "flipH", part)?.unwrap_or(false);
            group.flip_v = optional_xml_bool(element, "flipV", part)?.unwrap_or(false);
        }
        "off" => {
            mark_semantic_once(
                &mut group.semantic_seen,
                SEEN_OFFSET,
                part,
                "group transform has multiple offsets",
            )?;
            group.offset_x = signed_attr(element, "x", part)?;
            group.offset_y = signed_attr(element, "y", part)?;
        }
        "ext" => {
            mark_semantic_once(
                &mut group.semantic_seen,
                SEEN_EXTENT,
                part,
                "group transform has multiple extents",
            )?;
            group.extent_x = signed_attr(element, "cx", part)?;
            group.extent_y = signed_attr(element, "cy", part)?;
        }
        "chOff" => {
            mark_semantic_once(
                &mut group.semantic_seen,
                SEEN_CHILD_OFFSET,
                part,
                "group transform has multiple child offsets",
            )?;
            group.child_x = signed_attr(element, "x", part)?;
            group.child_y = signed_attr(element, "y", part)?;
        }
        "chExt" => {
            mark_semantic_once(
                &mut group.semantic_seen,
                SEEN_CHILD_EXTENT,
                part,
                "group transform has multiple child extents",
            )?;
            group.child_extent_x = signed_attr(element, "cx", part)?;
            group.child_extent_y = signed_attr(element, "cy", part)?;
        }
        _ => {}
    }
    if [group.extent_x, group.extent_y, group.child_extent_x, group.child_extent_y]
        .iter()
        .any(|value| *value < 0)
    {
        return Err(malformed(Some(part), "negative group extent"));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn apply_shape_element(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    part: &str,
    shape: &mut Option<Shape>,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let Some(shape) = shape.as_mut() else { return Ok(()) };
    match local(element.name().as_ref()) {
        "cNvPr" => {
            shape.hidden |= optional_xml_bool(element, "hidden", part)?.unwrap_or(false);
            if let Some(description) =
                attr(element, "descr", part)?.filter(|value| !value.is_empty())
            {
                shape.alt.get_or_insert(description);
            }
        }
        "ph" => {
            mark_semantic_once(
                &mut shape.semantic_seen,
                SEEN_PLACEHOLDER,
                part,
                "shape has multiple placeholders",
            )?;
            apply_placeholder(element, part, shape)?;
        }
        "xfrm" => {
            if shape.semantic_seen & SEEN_TRANSFORM != 0
                && options.error_policy == ErrorPolicy::BestEffort
            {
                shape.ignore_transform_children = true;
                shape.recoveries.try_reserve(1).map_err(|error| {
                    limit("max_memory_bytes", format!("cannot reserve transform recovery: {error}"))
                })?;
                shape.recoveries.push(ShapeRecovery {
                    code: "office.extensionOmitted",
                    message: "secondary shape transform was ignored".into(),
                });
                return Ok(());
            }
            mark_semantic_once(
                &mut shape.semantic_seen,
                SEEN_TRANSFORM,
                part,
                "shape has multiple transforms",
            )?;
            if let Some(rotation) = attr(element, "rot", part)? {
                shape.geometry.rotation = rotation
                    .parse::<i32>()
                    .map_err(|_| malformed(Some(part), "invalid rotation"))?;
                shape.geometry.presence |= GEOMETRY_ROTATION;
            }
            if let Some(flip_h) = optional_xml_bool(element, "flipH", part)? {
                shape.geometry.flip_h = flip_h;
                shape.geometry.presence |= GEOMETRY_FLIP_H;
            }
            if let Some(flip_v) = optional_xml_bool(element, "flipV", part)? {
                shape.geometry.flip_v = flip_v;
                shape.geometry.presence |= GEOMETRY_FLIP_V;
            }
        }
        "off" => {
            if shape.ignore_transform_children {
                return Ok(());
            }
            mark_semantic_once(
                &mut shape.semantic_seen,
                SEEN_OFFSET,
                part,
                "shape transform has multiple offsets",
            )?;
            shape.geometry.x = signed_attr(element, "x", part)?;
            shape.geometry.y = signed_attr(element, "y", part)?;
            shape.geometry.presence |= GEOMETRY_OFFSET;
        }
        "ext" => {
            if shape.ignore_transform_children {
                return Ok(());
            }
            mark_semantic_once(
                &mut shape.semantic_seen,
                SEEN_EXTENT,
                part,
                "shape transform has multiple extents",
            )?;
            shape.geometry.cx = signed_attr(element, "cx", part)?;
            shape.geometry.cy = signed_attr(element, "cy", part)?;
            shape.geometry.presence |= GEOMETRY_EXTENT;
            if shape.geometry.cx < 0 || shape.geometry.cy < 0 {
                return Err(malformed(Some(part), "negative shape extent"));
            }
        }
        "pPr" => {
            let level = attr(element, "lvl", part)?;
            if level.is_some() {
                shape.paragraph_explicit |= EXPLICIT_LIST_LEVEL;
            }
            shape.level = level
                .map(|value| {
                    let level = value
                        .parse::<u8>()
                        .map_err(|_| malformed(Some(part), "invalid list level"))?;
                    if level > 8 {
                        return Err(malformed(Some(part), "list level must be in 0..=8"));
                    }
                    Ok(level)
                })
                .transpose()?
                .unwrap_or(0);
        }
        "buChar" => {
            let character = required_attr(element, "char", part)?;
            if character.chars().count() != 1 {
                return Err(malformed(Some(part), "bullet character must contain one scalar"));
            }
            shape.bullet = Some(ListKind::Bullet);
            shape.paragraph_explicit |= EXPLICIT_BULLET;
            shape.numbering = Some(character);
        }
        "buAutoNum" => {
            shape.bullet = Some(ListKind::Ordered);
            shape.paragraph_explicit |= EXPLICIT_BULLET;
            shape.list_start = attr(element, "startAt", part)?
                .map(|value| {
                    value.parse::<u64>().map_err(|_| {
                        malformed(Some(part), "invalid automatic-numbering start value")
                    })
                })
                .transpose()?
                .unwrap_or(1);
            if !(1..=32_767).contains(&shape.list_start) {
                return Err(malformed(
                    Some(part),
                    "automatic-numbering start value must be in 1..=32767",
                ));
            }
            let numbering = required_attr(element, "type", part)?;
            if !valid_autonumber_scheme(&numbering) {
                return Err(malformed(Some(part), "invalid automatic-numbering scheme"));
            }
            shape.numbering = Some(numbering);
        }
        "buNone" => {
            shape.bullet = None;
            shape.paragraph_explicit |= EXPLICIT_BULLET;
            shape.numbering = None;
        }
        "buBlip" => {
            if options.error_policy == ErrorPolicy::Strict {
                return Err(malformed(Some(part), "bitmap bullets are not supported"));
            }
            shape.bullet = Some(ListKind::Bullet);
            shape.paragraph_explicit |= EXPLICIT_BULLET;
            shape.numbering = None;
            shape.recoveries.try_reserve(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve bitmap-bullet recovery: {error}"))
            })?;
            shape.recoveries.push(ShapeRecovery {
                code: "office.extensionOmitted",
                message: "bitmap bullet was replaced by a standard bullet".into(),
            });
        }
        "blip" => {
            if optional_attr_ns(reader, element, R_NS, "link", part)?.is_some() {
                if options.error_policy == ErrorPolicy::Strict {
                    return Err(malformed(Some(part), "linked images are not supported"));
                }
                shape.recoveries.try_reserve(1).map_err(|error| {
                    limit(
                        "max_memory_bytes",
                        format!("cannot reserve linked-image recovery: {error}"),
                    )
                })?;
                shape.recoveries.push(ShapeRecovery {
                    code: "office.relationshipOmitted",
                    message: "external image relationship was removed without downloading it"
                        .into(),
                });
                return Ok(());
            }
            let id = required_attr_ns(reader, element, R_NS, "embed", part)?;
            if shape.image.is_some() {
                if options.error_policy == ErrorPolicy::Strict {
                    return Err(malformed(Some(part), "shape has multiple image references"));
                }
                shape.recoveries.try_reserve(1).map_err(|error| {
                    limit(
                        "max_memory_bytes",
                        format!("cannot reserve duplicate-image recovery: {error}"),
                    )
                })?;
                shape.recoveries.push(ShapeRecovery {
                    code: "office.extensionOmitted",
                    message: "duplicate image references were normalized to the final reference"
                        .into(),
                });
                let alt = shape.image.take().and_then(|(_, alt)| alt).or_else(|| shape.alt.take());
                shape.image = Some((id, alt));
                return Ok(());
            }
            let alt = shape.alt.take();
            shape.image = Some((id, alt));
        }
        "chart" => {
            let id = required_attr_ns(reader, element, R_NS, "id", part)?;
            if shape.chart.is_some() {
                return Err(malformed(Some(part), "shape has multiple chart references"));
            }
            if shape.semantic_seen & SEEN_TABLE != 0 {
                return Err(malformed(
                    Some(part),
                    "graphic frame combines chart and table payloads",
                ));
            }
            shape.chart = Some(id);
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn valid_autonumber_scheme(value: &str) -> bool {
    matches!(
        value,
        "alphaLcParenBoth"
            | "alphaUcParenBoth"
            | "alphaLcParenR"
            | "alphaUcParenR"
            | "alphaLcPeriod"
            | "alphaUcPeriod"
            | "arabicParenBoth"
            | "arabicParenR"
            | "arabicPeriod"
            | "arabicPlain"
            | "romanLcParenBoth"
            | "romanUcParenBoth"
            | "romanLcParenR"
            | "romanUcParenR"
            | "romanLcPeriod"
            | "romanUcPeriod"
            | "circleNumDbPlain"
            | "circleNumWdBlackPlain"
            | "circleNumWdWhitePlain"
            | "arabicDbPeriod"
            | "arabicDbPlain"
            | "ea1ChsPeriod"
            | "ea1ChsPlain"
            | "ea1ChtPeriod"
            | "ea1ChtPlain"
            | "ea1JpnChsDbPeriod"
            | "ea1JpnKorPlain"
            | "ea1JpnKorPeriod"
            | "arabic1Minus"
            | "arabic2Minus"
            | "hebrew2Minus"
            | "thaiAlphaPeriod"
            | "thaiAlphaParenR"
            | "thaiAlphaParenBoth"
            | "thaiNumPeriod"
            | "thaiNumParenR"
            | "thaiNumParenBoth"
            | "hindiAlphaPeriod"
            | "hindiNumPeriod"
            | "hindiNumParenR"
            | "hindiAlpha1Period"
    )
}

fn apply_placeholder(
    element: &BytesStart<'_>,
    part: &str,
    shape: &mut Shape,
) -> Result<(), ConversionError> {
    let kind = attr(element, "type", part)?.unwrap_or_else(|| "obj".into());
    if !matches!(
        kind.as_str(),
        "title"
            | "body"
            | "ctrTitle"
            | "subTitle"
            | "dt"
            | "sldNum"
            | "ftr"
            | "hdr"
            | "obj"
            | "chart"
            | "tbl"
            | "clipArt"
            | "dgm"
            | "media"
            | "sldImg"
            | "pic"
    ) {
        return Err(malformed(Some(part), "invalid placeholder type"));
    }
    shape.placeholder_index = attr(element, "idx", part)?
        .map(|value| {
            value.parse::<u32>().map_err(|_| malformed(Some(part), "invalid placeholder index"))
        })
        .transpose()?
        .unwrap_or(0);
    shape.title = matches!(kind.as_str(), "title" | "ctrTitle");
    shape.placeholder = Some(kind);
    Ok(())
}

pub(super) fn reject_merged_table_cell(
    element: &BytesStart<'_>,
    part: &str,
) -> Result<(), ConversionError> {
    for name in ["gridSpan", "rowSpan", "hMerge", "vMerge"] {
        if attr(element, name, part)?.is_some() {
            return Err(malformed(
                Some(part),
                "merged PresentationML table cells are not supported",
            ));
        }
    }
    Ok(())
}

pub(super) fn marks_for_style(style: RichStyle) -> Result<Vec<InlineMark>, ConversionError> {
    let mut marks = Vec::new();
    marks.try_reserve_exact(4).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve rich-text marks: {error}"))
    })?;
    if style.bold.unwrap_or(false) {
        marks.push(InlineMark::Bold);
    }
    if style.italic.unwrap_or(false) {
        marks.push(InlineMark::Italic);
    }
    if style.underline.unwrap_or(false) {
        marks.push(InlineMark::Underline);
    }
    if style.strike.unwrap_or(false) {
        marks.push(InlineMark::Strikethrough);
    }
    Ok(marks)
}

pub(super) fn parse_rich_style(
    element: &BytesStart<'_>,
    part: &str,
) -> Result<RichStyle, ConversionError> {
    let bold = optional_xml_bool(element, "b", part)?;
    let italic = optional_xml_bool(element, "i", part)?;
    let mut underline_style = None;
    if let Some(underline) = attr(element, "u", part)? {
        if !matches!(
            underline.as_str(),
            "none"
                | "words"
                | "sng"
                | "dbl"
                | "heavy"
                | "dotted"
                | "dottedHeavy"
                | "dash"
                | "dashHeavy"
                | "dashLong"
                | "dashLongHeavy"
                | "dotDash"
                | "dotDashHeavy"
                | "dotDotDash"
                | "dotDotDashHeavy"
                | "wavy"
                | "wavyHeavy"
                | "wavyDbl"
        ) {
            return Err(malformed(Some(part), "invalid DrawingML underline value"));
        }
        underline_style = Some(underline != "none");
    }
    let mut strike_style = None;
    if let Some(strike) = attr(element, "strike", part)? {
        if !matches!(strike.as_str(), "noStrike" | "sngStrike" | "dblStrike") {
            return Err(malformed(Some(part), "invalid DrawingML strike value"));
        }
        strike_style = Some(strike != "noStrike");
    }
    Ok(RichStyle { bold, italic, underline: underline_style, strike: strike_style })
}

pub(super) fn record_language(
    element: &BytesStart<'_>,
    part: &str,
    shape: &mut Shape,
) -> Result<(), ConversionError> {
    let Some(language) = attr(element, "lang", part)? else { return Ok(()) };
    let valid = !language.is_empty()
        && language.len() <= 63
        && !language.starts_with('-')
        && !language.ends_with('-')
        && !language.contains("--")
        && language.bytes().all(|value| value.is_ascii_alphanumeric() || value == b'-');
    if !valid {
        return Err(malformed(Some(part), "invalid PresentationML language tag"));
    }
    shape.languages.try_reserve(1).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve shape language: {error}"))
    })?;
    shape.languages.push(language);
    Ok(())
}

pub(super) fn shape_block_count(shapes: &[Shape]) -> Result<usize, ConversionError> {
    shapes.iter().try_fold(0_usize, |total, shape| {
        let fixed = usize::from(shape.image.is_some())
            .checked_add(usize::from(shape.chart.is_some()))
            .and_then(|value| value.checked_add(usize::from(shape.table.is_some())))
            .ok_or_else(|| limit("max_document_nodes", "shape block count overflow"))?;
        total
            .checked_add(fixed)
            .and_then(|value| value.checked_add(shape.paragraphs.len()))
            .ok_or_else(|| limit("max_document_nodes", "slide block count overflow"))
    })
}

pub(super) fn validate_shape_relationships(
    shapes: &[Shape],
    package: &mut Package<'_>,
    relationships: &Relationships,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let chart_index_bytes = u64::try_from(shapes.len())
        .unwrap_or(u64::MAX)
        .checked_mul(160)
        .and_then(|value| value.checked_add(4096))
        .ok_or_else(|| limit("max_memory_bytes", "chart authorization index plan overflow"))?;
    let _chart_index_memory = context.reserve_memory(chart_index_bytes)?;
    let mut authorized_chart_targets = HashSet::<String>::new();
    for (index, shape) in shapes.iter().enumerate() {
        if index.is_multiple_of(256) {
            context.checkpoint()?;
        }
        if let Some((id, _)) = &shape.image {
            let relationship = relationship_by_id(relationships, id).ok_or_else(|| {
                malformed(Some(part), format!("image relationship {id} is missing"))
            })?;
            if relationship.external || relationship.kind != IMAGE_REL {
                return Err(malformed(Some(part), "image relationship has wrong type or mode"));
            }
            let target = resolve_target(part, &relationship.target)?;
            let content_type = package.content_types.content_type(&target).ok_or_else(|| {
                malformed(Some("[Content_Types].xml"), format!("image {target} lacks content type"))
            })?;
            if let Err(error) = supported_image(&target, content_type)
                && !(options.error_policy == ErrorPolicy::BestEffort
                    && matches!(
                        &error,
                        ConversionError::Malformed { .. } | ConversionError::Unsupported { .. }
                    ))
            {
                return Err(error);
            }
            package.authorize_referenced_part(&target)?;
        }
        if let Some(id) = &shape.chart {
            let relationship = relationship_by_id(relationships, id).ok_or_else(|| {
                malformed(Some(part), format!("chart relationship {id} is missing"))
            })?;
            if relationship.external || relationship.kind != CHART_REL {
                return Err(malformed(Some(part), "chart relationship has wrong type or mode"));
            }
            let target = resolve_target(part, &relationship.target)?;
            require_content_type(
                package,
                &target,
                "application/vnd.openxmlformats-officedocument.drawingml.chart+xml",
            )?;
            package.authorize_referenced_part(&target)?;
            if !authorized_chart_targets.contains(&target) {
                authorized_chart_targets.try_reserve(1).map_err(|error| {
                    limit(
                        "max_memory_bytes",
                        format!("cannot reserve chart authorization index: {error}"),
                    )
                })?;
                // Validate relationship metadata even when the owning shape will later be hidden,
                // without opening the chart payload itself.
                package.relationships_optional(&target, options, context)?;
                authorized_chart_targets.insert(target);
            }
        }
    }
    Ok(())
}
