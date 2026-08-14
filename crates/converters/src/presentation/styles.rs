use super::allocation::try_clone_string;
use super::error::{limit, malformed};
use super::mce::McSelection;
use super::model::{
    Geometry, Package, PlaceholderClass, PlaceholderKey, Relationships, RichStyle, Shape,
    ShapeStyle, TextParagraph,
};
use super::relationships::{require_content_type, resolve_target, unique_relationship};
use super::schema::{
    GEOMETRY_EXTENT, GEOMETRY_FLIP_H, GEOMETRY_FLIP_V, GEOMETRY_OFFSET, GEOMETRY_ROTATION,
    LAYOUT_REL, MASTER_REL, THEME_REL,
};
use super::shape_elements::{
    marks_for_style, parse_rich_style, valid_autonumber_scheme, validate_shape_relationships,
};
use super::slides::parse_shapes;
use super::xml::{XmlProfile, preflight_xml};
use super::xml_base::{attr, level_paragraph, local, required_attr};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext, Inline, ListKind};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::NsReader;
use std::cmp::Ordering;

type PlaceholderStyles = Vec<(PlaceholderKey, ShapeStyle)>;
type MasterPlaceholderStyles = Vec<(PlaceholderClass, ShapeStyle)>;

pub(super) fn inherited_styles(
    package: &mut Package<'_>,
    slide: &str,
    relationships: &Relationships,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(PlaceholderStyles, MasterPlaceholderStyles, Option<String>), ConversionError> {
    let Some(layout_rel) = unique_relationship(relationships, LAYOUT_REL, slide)? else {
        return Ok((Vec::new(), Vec::new(), None));
    };
    let layout_part = resolve_target(slide, &layout_rel.target)?;
    require_content_type(
        package,
        &layout_part,
        "application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml",
    )?;
    let layout_rels = package.relationships_optional(&layout_part, options, context)?;
    let layout_shapes = {
        let bytes = package.load_for_parse(&layout_part, options, context)?;
        parse_shapes(bytes, &layout_part, XmlProfile::Layout, options, context)?
    };
    package.release_parsed(&layout_part)?;
    validate_shape_relationships(
        &layout_shapes,
        package,
        &layout_rels,
        &layout_part,
        options,
        context,
    )?;
    let layout = layout_styles_from_shapes(layout_shapes, &layout_part)?;
    let mut theme_name = None;
    let master =
        if let Some(master_rel) = unique_relationship(&layout_rels, MASTER_REL, &layout_part)? {
            let master_part = resolve_target(&layout_part, &master_rel.target)?;
            require_content_type(
                package,
                &master_part,
                "application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml",
            )?;
            let master_rels = package.relationships_optional(&master_part, options, context)?;
            let (master_shapes, master_text_styles) = {
                let bytes = package.load_for_parse(&master_part, options, context)?;
                (
                    parse_shapes(bytes, &master_part, XmlProfile::Master, options, context)?,
                    parse_master_text_styles(bytes, &master_part, options, context)?,
                )
            };
            package.release_parsed(&master_part)?;
            validate_shape_relationships(
                &master_shapes,
                package,
                &master_rels,
                &master_part,
                options,
                context,
            )?;
            let mut master_styles = master_styles_from_shapes(master_shapes, &master_part)?;
            merge_master_text_styles(&mut master_styles, master_text_styles, &master_part)?;
            if let Some(theme_rel) = unique_relationship(&master_rels, THEME_REL, &master_part)? {
                let theme_part = resolve_target(&master_part, &theme_rel.target)?;
                require_content_type(
                    package,
                    &theme_part,
                    "application/vnd.openxmlformats-officedocument.theme+xml",
                )?;
                // Theme relationships are not content sources for the Markdown IR, but they are
                // still reachable OPC metadata and must pass the same external/dangerous policy.
                package.relationships_optional(&theme_part, options, context)?;
                theme_name = {
                    let bytes = package.load_for_parse(&theme_part, options, context)?;
                    preflight_xml(bytes, &theme_part, XmlProfile::Theme, options, context)?;
                    parse_theme_name(bytes, &theme_part, options, context)?
                };
                package.release_parsed(&theme_part)?;
            }
            master_styles
        } else {
            Vec::new()
        };
    Ok((layout, master, theme_name))
}

fn parse_theme_name(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Option<String>, ConversionError> {
    let mut reader = NsReader::from_reader(bytes);
    loop {
        context.checkpoint()?;
        match reader.read_event().map_err(|error| malformed(Some(part), error.to_string()))? {
            Event::Start(element) if local(element.name().as_ref()) == "theme" => {
                let name = attr(&element, "name", part)?;
                if name.as_ref().is_some_and(|value| {
                    u64::try_from(value.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes
                }) {
                    return Err(limit("max_field_bytes", format!("theme name in {part}")));
                }
                return Ok(name);
            }
            Event::Eof => return Err(malformed(Some(part), "theme root is missing")),
            _ => {}
        }
    }
}

pub(super) fn layout_styles_from_shapes(
    shapes: Vec<Shape>,
    part: &str,
) -> Result<PlaceholderStyles, ConversionError> {
    let mut result = Vec::new();
    for shape in shapes {
        if let Some(placeholder) = shape.placeholder.as_deref() {
            let class = placeholder_class(placeholder);
            result.try_reserve(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve placeholder style: {error}"))
            })?;
            result.push((
                PlaceholderKey { index: shape.placeholder_index },
                ShapeStyle {
                    geometry: Some(shape.geometry),
                    pending_groups: shape.pending_groups,
                    paragraphs: shape.paragraphs,
                    title: shape.title,
                    hidden: shape.hidden,
                    languages: shape.languages,
                    class,
                },
            ));
        }
    }
    result.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if result.windows(2).any(|values| values[0].0 == values[1].0) {
        return Err(malformed(Some(part), "duplicate placeholder index"));
    }
    Ok(result)
}

pub(super) fn master_styles_from_shapes(
    shapes: Vec<Shape>,
    part: &str,
) -> Result<MasterPlaceholderStyles, ConversionError> {
    let mut result = Vec::new();
    for shape in shapes {
        if let Some(placeholder) = shape.placeholder.as_deref() {
            let class = placeholder_class(placeholder);
            result.try_reserve(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve master placeholder: {error}"))
            })?;
            result.push((
                class,
                ShapeStyle {
                    geometry: Some(shape.geometry),
                    pending_groups: shape.pending_groups,
                    paragraphs: shape.paragraphs,
                    title: shape.title,
                    hidden: shape.hidden,
                    languages: shape.languages,
                    class,
                },
            ));
        }
    }
    result.sort_unstable_by_key(|(class, _)| *class);
    if result.windows(2).any(|values| values[0].0 == values[1].0) {
        return Err(malformed(
            Some(part),
            "multiple master placeholders project to the same normative class",
        ));
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MasterTextSection {
    Title,
    Body,
    Other,
}

type MasterTextStyles = Vec<(MasterTextSection, Vec<TextParagraph>)>;

#[allow(clippy::too_many_lines)]
pub(super) fn parse_master_text_styles(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<MasterTextStyles, ConversionError> {
    let mut reader = NsReader::from_reader(bytes);
    let mut result = Vec::new();
    result.try_reserve_exact(3).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve master text styles: {error}"))
    })?;
    let mut section = None::<MasterTextSection>;
    let mut levels = Vec::<TextParagraph>::new();
    levels.try_reserve_exact(9).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve master style levels: {error}"))
    })?;
    let mut current = None::<TextParagraph>;
    let mut mc = McSelection::default();
    let mut tx_styles_seen = false;
    let mut sections_seen = 0_u8;
    loop {
        context.checkpoint()?;
        let event =
            reader.read_event().map_err(|error| malformed(Some(part), error.to_string()))?;
        if mc.skip(&reader, &event, part)? {
            continue;
        }
        match event {
            Event::Start(element) => {
                let qualified_name = element.name();
                let local_name = local(qualified_name.as_ref());
                match local_name {
                    "txStyles" => {
                        if tx_styles_seen {
                            return Err(malformed(
                                Some(part),
                                "master has multiple txStyles elements",
                            ));
                        }
                        tx_styles_seen = true;
                    }
                    "titleStyle" | "bodyStyle" | "otherStyle" => {
                        if section.is_some() {
                            return Err(malformed(Some(part), "nested master text style section"));
                        }
                        let parsed_section = match local_name {
                            "titleStyle" => MasterTextSection::Title,
                            "bodyStyle" => MasterTextSection::Body,
                            _ => MasterTextSection::Other,
                        };
                        mark_master_text_section(&mut sections_seen, parsed_section, part)?;
                        section = Some(parsed_section);
                        levels.clear();
                    }
                    _ if section.is_some() => {
                        if let Some(level) = level_paragraph(local_name.as_bytes()) {
                            if current.is_some() {
                                return Err(malformed(Some(part), "nested master text level"));
                            }
                            current = Some(TextParagraph {
                                level,
                                level_explicit: true,
                                start: 1,
                                ..TextParagraph::default()
                            });
                        } else if local_name == "defRPr" {
                            let paragraph = current.as_mut().ok_or_else(|| {
                                malformed(Some(part), "master defRPr is outside a level")
                            })?;
                            if !paragraph.default_style.is_absent() {
                                return Err(malformed(
                                    Some(part),
                                    "master text level has multiple defRPr elements",
                                ));
                            }
                            paragraph.default_style = parse_rich_style(&element, part)?;
                            paragraph.default_marks = marks_for_style(paragraph.default_style)?;
                        } else if matches!(local_name, "buChar" | "buAutoNum" | "buNone" | "buBlip")
                        {
                            apply_master_bullet(
                                &element,
                                part,
                                current.as_mut().ok_or_else(|| {
                                    malformed(Some(part), "master bullet is outside a level")
                                })?,
                            )?;
                        }
                    }
                    _ => {}
                }
            }
            Event::Empty(element) => {
                let qualified_name = element.name();
                let local_name = local(qualified_name.as_ref());
                if local_name == "txStyles" {
                    if tx_styles_seen {
                        return Err(malformed(Some(part), "master has multiple txStyles elements"));
                    }
                    tx_styles_seen = true;
                } else if matches!(local_name, "titleStyle" | "bodyStyle" | "otherStyle") {
                    let parsed_section = match local_name {
                        "titleStyle" => MasterTextSection::Title,
                        "bodyStyle" => MasterTextSection::Body,
                        _ => MasterTextSection::Other,
                    };
                    mark_master_text_section(&mut sections_seen, parsed_section, part)?;
                    result.push((parsed_section, Vec::new()));
                } else if section.is_some() {
                    if let Some(level) = level_paragraph(local_name.as_bytes()) {
                        levels.try_reserve(1).map_err(|error| {
                            limit(
                                "max_memory_bytes",
                                format!("cannot reserve master level: {error}"),
                            )
                        })?;
                        levels.push(TextParagraph {
                            level,
                            level_explicit: true,
                            start: 1,
                            ..TextParagraph::default()
                        });
                    } else if local_name == "defRPr" {
                        let paragraph = current.as_mut().ok_or_else(|| {
                            malformed(Some(part), "master defRPr is outside a level")
                        })?;
                        if !paragraph.default_style.is_absent() {
                            return Err(malformed(
                                Some(part),
                                "master text level has multiple defRPr elements",
                            ));
                        }
                        paragraph.default_style = parse_rich_style(&element, part)?;
                        paragraph.default_marks = marks_for_style(paragraph.default_style)?;
                    } else if matches!(local_name, "buChar" | "buAutoNum" | "buNone" | "buBlip") {
                        apply_master_bullet(
                            &element,
                            part,
                            current.as_mut().ok_or_else(|| {
                                malformed(Some(part), "master bullet is outside a level")
                            })?,
                        )?;
                    }
                }
            }
            Event::End(element) => {
                let qualified_name = element.name();
                let local_name = local(qualified_name.as_ref());
                if level_paragraph(local_name.as_bytes()).is_some() {
                    levels.try_reserve(1).map_err(|error| {
                        limit("max_memory_bytes", format!("cannot reserve master level: {error}"))
                    })?;
                    levels.push(current.take().ok_or_else(|| {
                        malformed(Some(part), "master text level end without start")
                    })?);
                } else if matches!(local_name, "titleStyle" | "bodyStyle" | "otherStyle") {
                    if current.is_some() {
                        return Err(malformed(
                            Some(part),
                            "master text style has incomplete level",
                        ));
                    }
                    levels.sort_unstable_by_key(|paragraph| paragraph.level);
                    if levels.windows(2).any(|pair| pair[0].level == pair[1].level) {
                        return Err(malformed(Some(part), "duplicate master text style level"));
                    }
                    result.push((
                        section.take().ok_or_else(|| {
                            malformed(Some(part), "master text style end without start")
                        })?,
                        std::mem::take(&mut levels),
                    ));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if section.is_some() || current.is_some() {
        return Err(malformed(Some(part), "incomplete master text style"));
    }
    if u64::try_from(result.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
        return Err(limit("max_field_bytes", "master text style count"));
    }
    Ok(result)
}

fn mark_master_text_section(
    seen: &mut u8,
    section: MasterTextSection,
    part: &str,
) -> Result<(), ConversionError> {
    let flag = match section {
        MasterTextSection::Title => 1,
        MasterTextSection::Body => 2,
        MasterTextSection::Other => 4,
    };
    if *seen & flag != 0 {
        return Err(malformed(Some(part), "master txStyles has a duplicate section"));
    }
    *seen |= flag;
    Ok(())
}

fn apply_master_bullet(
    element: &BytesStart<'_>,
    part: &str,
    paragraph: &mut TextParagraph,
) -> Result<(), ConversionError> {
    if paragraph.bullet_explicit {
        return Err(malformed(Some(part), "master text level has multiple bullet definitions"));
    }
    paragraph.bullet_explicit = true;
    match local(element.name().as_ref()) {
        "buChar" => {
            let character = required_attr(element, "char", part)?;
            if character.chars().count() != 1 {
                return Err(malformed(Some(part), "bullet character must contain one scalar"));
            }
            paragraph.bullet = Some(ListKind::Bullet);
            paragraph.numbering = Some(character);
        }
        "buAutoNum" => {
            paragraph.bullet = Some(ListKind::Ordered);
            paragraph.start = attr(element, "startAt", part)?
                .map(|value| {
                    value.parse::<u64>().map_err(|_| {
                        malformed(Some(part), "invalid automatic-numbering start value")
                    })
                })
                .transpose()?
                .unwrap_or(1);
            if !(1..=32_767).contains(&paragraph.start) {
                return Err(malformed(
                    Some(part),
                    "automatic-numbering start value must be in 1..=32767",
                ));
            }
            let numbering = required_attr(element, "type", part)?;
            if !valid_autonumber_scheme(&numbering) {
                return Err(malformed(Some(part), "invalid automatic-numbering scheme"));
            }
            paragraph.numbering = Some(numbering);
        }
        "buBlip" => return Err(malformed(Some(part), "bitmap bullets are not supported")),
        _ => {}
    }
    Ok(())
}

pub(super) fn merge_master_text_styles(
    styles: &mut MasterPlaceholderStyles,
    text_styles: MasterTextStyles,
    part: &str,
) -> Result<(), ConversionError> {
    let mut sections_seen = 0_u8;
    for (section, _) in &text_styles {
        mark_master_text_section(&mut sections_seen, *section, part)?;
    }
    for (section, paragraphs) in text_styles {
        let classes: &[PlaceholderClass] = match section {
            MasterTextSection::Title => &[PlaceholderClass::Title],
            MasterTextSection::Body => &[PlaceholderClass::Body],
            MasterTextSection::Other => &[
                PlaceholderClass::Date,
                PlaceholderClass::Footer,
                PlaceholderClass::SlideNumber,
                PlaceholderClass::Header,
            ],
        };
        for class in classes {
            if let Some(style) = styles.iter_mut().find(|(candidate, _)| candidate == class) {
                apply_inherited_text_style(&mut style.1.paragraphs, &paragraphs)?;
                if style.1.paragraphs.is_empty() {
                    style.1.paragraphs = try_clone_text_paragraphs(&paragraphs)?;
                }
            } else {
                styles.try_reserve(1).map_err(|error| {
                    limit(
                        "max_memory_bytes",
                        format!("cannot reserve projected master text style: {error}"),
                    )
                })?;
                styles.push((
                    *class,
                    ShapeStyle {
                        paragraphs: try_clone_text_paragraphs(&paragraphs)?,
                        class: *class,
                        ..ShapeStyle::default()
                    },
                ));
            }
        }
    }
    styles.sort_unstable_by_key(|(class, _)| *class);
    if styles.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(malformed(Some(part), "ambiguous projected master text style"));
    }
    Ok(())
}

fn try_clone_text_paragraphs(
    source: &[TextParagraph],
) -> Result<Vec<TextParagraph>, ConversionError> {
    let mut result = Vec::new();
    result.try_reserve_exact(source.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve inherited text styles: {error}"))
    })?;
    for paragraph in source {
        let mut text = Vec::new();
        text.try_reserve_exact(paragraph.text.len()).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve inherited style text: {error}"))
        })?;
        for inline in &paragraph.text {
            match inline {
                Inline::Text { value, marks } => {
                    let mut cloned_marks = Vec::new();
                    cloned_marks.try_reserve_exact(marks.len()).map_err(|error| {
                        limit(
                            "max_memory_bytes",
                            format!("cannot reserve inherited style marks: {error}"),
                        )
                    })?;
                    cloned_marks.extend_from_slice(marks);
                    text.push(Inline::Text {
                        value: try_clone_string(value, "inherited style text")?,
                        marks: cloned_marks,
                    });
                }
                Inline::LineBreak => text.push(Inline::LineBreak),
                _ => {
                    return Err(ConversionError::Internal {
                        detail: "PresentationML text-style template contains unsupported inline"
                            .into(),
                    });
                }
            }
        }
        let mut default_marks = Vec::new();
        default_marks.try_reserve_exact(paragraph.default_marks.len()).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve default style marks: {error}"))
        })?;
        default_marks.extend_from_slice(&paragraph.default_marks);
        let mut run_styles = Vec::new();
        run_styles.try_reserve_exact(paragraph.run_styles.len()).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve inherited run styles: {error}"))
        })?;
        run_styles.extend_from_slice(&paragraph.run_styles);
        result.push(TextParagraph {
            text,
            default_marks,
            default_style: paragraph.default_style,
            run_styles,
            level: paragraph.level,
            level_explicit: paragraph.level_explicit,
            bullet: paragraph.bullet,
            bullet_explicit: paragraph.bullet_explicit,
            start: paragraph.start,
            numbering: paragraph
                .numbering
                .as_deref()
                .map(|value| try_clone_string(value, "inherited numbering"))
                .transpose()?,
        });
    }
    Ok(result)
}

pub(super) fn placeholder_class(value: &str) -> PlaceholderClass {
    match value {
        "title" | "ctrTitle" => PlaceholderClass::Title,
        "dt" => PlaceholderClass::Date,
        "ftr" => PlaceholderClass::Footer,
        "sldNum" => PlaceholderClass::SlideNumber,
        "hdr" => PlaceholderClass::Header,
        _ => PlaceholderClass::Body,
    }
}

pub(super) fn apply_inheritance(
    shapes: &mut [Shape],
    layout: &PlaceholderStyles,
    master: &MasterPlaceholderStyles,
) -> Result<(), ConversionError> {
    for shape in shapes {
        if shape.placeholder.is_none() {
            continue;
        }
        let key = PlaceholderKey { index: shape.placeholder_index };
        let layout_style = placeholder_style(layout, &key);
        let master_style = layout_style.and_then(|style| master_style(master, style.class));
        if let Some(geometry) = layout_style.and_then(|style| style.geometry) {
            inherit_geometry(&mut shape.geometry, geometry);
        }
        if let Some(geometry) = master_style.and_then(|style| style.geometry) {
            inherit_geometry(&mut shape.geometry, geometry);
        }
        if shape.pending_groups.is_empty() {
            let inherited_groups = layout_style
                .filter(|style| !style.pending_groups.is_empty())
                .or_else(|| master_style.filter(|style| !style.pending_groups.is_empty()))
                .map(|style| style.pending_groups.as_slice())
                .unwrap_or_default();
            shape.pending_groups.try_reserve_exact(inherited_groups.len()).map_err(|error| {
                limit(
                    "max_memory_bytes",
                    format!("cannot reserve inherited group transforms: {error}"),
                )
            })?;
            shape.pending_groups.extend_from_slice(inherited_groups);
        }
        if let Some(style) = layout_style {
            if !style.paragraphs.is_empty() {
                apply_inherited_text_style(&mut shape.paragraphs, &style.paragraphs)?;
            }
            merge_sorted_languages(&mut shape.languages, &style.languages)?;
        }
        if let Some(style) = master_style {
            if !style.paragraphs.is_empty() {
                apply_inherited_text_style(&mut shape.paragraphs, &style.paragraphs)?;
            }
            merge_sorted_languages(&mut shape.languages, &style.languages)?;
        }
        shape.title |= layout_style.is_some_and(|style| style.title);
        shape.title |= master_style.is_some_and(|style| style.title);
        shape.hidden |= layout_style.is_some_and(|style| style.hidden);
        shape.hidden |= master_style.is_some_and(|style| style.hidden);
    }
    Ok(())
}

fn master_style(styles: &MasterPlaceholderStyles, class: PlaceholderClass) -> Option<&ShapeStyle> {
    styles
        .binary_search_by_key(&class, |(candidate, _)| *candidate)
        .ok()
        .map(|index| &styles[index].1)
}

fn inherit_geometry(target: &mut Geometry, inherited: Geometry) {
    if target.presence & GEOMETRY_OFFSET == 0 && inherited.presence & GEOMETRY_OFFSET != 0 {
        target.x = inherited.x;
        target.y = inherited.y;
        target.presence |= GEOMETRY_OFFSET;
    }
    if target.presence & GEOMETRY_EXTENT == 0 && inherited.presence & GEOMETRY_EXTENT != 0 {
        target.cx = inherited.cx;
        target.cy = inherited.cy;
        target.presence |= GEOMETRY_EXTENT;
    }
    if target.presence & GEOMETRY_ROTATION == 0 && inherited.presence & GEOMETRY_ROTATION != 0 {
        target.rotation = inherited.rotation;
        target.presence |= GEOMETRY_ROTATION;
    }
    if target.presence & GEOMETRY_FLIP_H == 0 && inherited.presence & GEOMETRY_FLIP_H != 0 {
        target.flip_h = inherited.flip_h;
        target.presence |= GEOMETRY_FLIP_H;
    }
    if target.presence & GEOMETRY_FLIP_V == 0 && inherited.presence & GEOMETRY_FLIP_V != 0 {
        target.flip_v = inherited.flip_v;
        target.presence |= GEOMETRY_FLIP_V;
    }
}

fn placeholder_style<'a>(
    styles: &'a PlaceholderStyles,
    key: &PlaceholderKey,
) -> Option<&'a ShapeStyle> {
    styles.binary_search_by(|(candidate, _)| candidate.cmp(key)).ok().map(|index| &styles[index].1)
}

pub(super) fn apply_pending_group_transforms(shapes: &mut [Shape]) -> Result<(), ConversionError> {
    for shape in shapes {
        apply_pending_group_transform(shape)?;
    }
    Ok(())
}

fn apply_pending_group_transform(shape: &mut Shape) -> Result<(), ConversionError> {
    for group in shape.pending_groups.iter().copied().rev() {
        shape.hidden |= group.hidden;
        shape.geometry = group.apply(shape.geometry)?;
    }
    shape.pending_groups.clear();
    Ok(())
}

fn apply_inherited_text_style(
    paragraphs: &mut [TextParagraph],
    inherited: &[TextParagraph],
) -> Result<(), ConversionError> {
    for paragraph in paragraphs.iter_mut() {
        let Some(style) = inherited
            .iter()
            .find(|style| style.level_explicit && style.level == paragraph.level)
            .or_else(|| inherited.get(usize::from(paragraph.level)))
            .or_else(|| inherited.first())
        else {
            break;
        };
        if !paragraph.level_explicit && style.level_explicit {
            paragraph.level = style.level;
            paragraph.level_explicit = true;
        }
        if !paragraph.bullet_explicit && style.bullet_explicit {
            paragraph.bullet = style.bullet;
            paragraph.bullet_explicit = true;
            paragraph.start = style.start;
            paragraph.numbering = style
                .numbering
                .as_deref()
                .map(|numbering| try_clone_string(numbering, "inherited numbering scheme"))
                .transpose()?;
        }
        let mut inherited_style = style.default_style;
        inherited_style.inherit(RichStyle::from_marks(&style.default_marks));
        if inherited_style.is_absent()
            && let Some(run) = style.run_styles.first().copied()
        {
            inherited_style = run;
        }
        if inherited_style.is_absent()
            && let Some(marks) = style.text.iter().find_map(|inline| match inline {
                Inline::Text { marks, .. } if !marks.is_empty() => Some(marks.as_slice()),
                _ => None,
            })
        {
            inherited_style = RichStyle::from_marks(marks);
        }
        paragraph.default_style.inherit(RichStyle::from_marks(&paragraph.default_marks));
        paragraph.default_style.inherit(inherited_style);
        paragraph.default_marks = marks_for_style(paragraph.default_style)?;
        for (index, inline) in paragraph.text.iter_mut().enumerate() {
            if let Inline::Text { marks, .. } = inline {
                let run = paragraph.run_styles.get(index).copied().unwrap_or_default();
                *marks = marks_for_style(run.with_lower(paragraph.default_style))?;
            }
        }
    }
    Ok(())
}

fn merge_sorted_languages(
    destination: &mut Vec<String>,
    inherited: &[String],
) -> Result<(), ConversionError> {
    if inherited.is_empty() {
        return Ok(());
    }
    let capacity = destination
        .len()
        .checked_add(inherited.len())
        .ok_or_else(|| limit("max_memory_bytes", "inherited language count overflow"))?;
    let mut merged = Vec::new();
    merged.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve inherited languages: {error}"))
    })?;
    let mut existing = std::mem::take(destination).into_iter().peekable();
    let mut inherited_index = 0_usize;
    while existing.peek().is_some() || inherited_index < inherited.len() {
        match (existing.peek(), inherited.get(inherited_index)) {
            (Some(left), Some(right)) => match left.as_str().cmp(right.as_str()) {
                Ordering::Less => merged.push(existing.next().expect("peeked language")),
                Ordering::Equal => {
                    merged.push(existing.next().expect("peeked language"));
                    inherited_index += 1;
                }
                Ordering::Greater => {
                    merged.push(try_clone_string(right, "inherited language")?);
                    inherited_index += 1;
                }
            },
            (Some(_), None) => merged.push(existing.next().expect("peeked language")),
            (None, Some(right)) => {
                merged.push(try_clone_string(right, "inherited language")?);
                inherited_index += 1;
            }
            (None, None) => break,
        }
    }
    *destination = merged;
    Ok(())
}
