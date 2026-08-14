use super::budget::MAX_XML_EVENTS;
use super::error::{limit, malformed};
use super::mce::McSelection;
use super::model::{GroupTransform, RichStyle, Shape, ShapeKind, TextParagraph};
use super::schema::{EXPLICIT_BULLET, EXPLICIT_LIST_LEVEL, R_NS, SEEN_TABLE};
use super::shape_elements::{
    add_parsed_inline, append_shape_text, apply_group_element, apply_shape_element,
    mark_semantic_once, marks_for_style, parse_rich_style, record_language,
    reject_merged_table_cell,
};
use super::text::trim_breaks;
use super::xml::{XmlProfile, preflight_xml};
use super::xml_base::{local, optional_xml_bool, required_attr_ns};
use crate::docx::{decode_cdata, decode_reference, decode_text};
use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, Inline, MAX_DOCUMENT_NODES,
    MAX_TABLE_COLUMNS,
};
use quick_xml::events::Event;
use quick_xml::reader::NsReader;

pub(super) struct SlideReference {
    pub(super) relationship_id: String,
}

pub(super) fn parse_slide_order(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<SlideReference>, ConversionError> {
    let mut reader = NsReader::from_reader(bytes);
    let mut result = Vec::new();
    let capacity = bytes.len().min(usize::try_from(options.limits.max_pages).unwrap_or(usize::MAX));
    result.try_reserve(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve slide order: {error}"))
    })?;
    let mut mc = McSelection::default();
    loop {
        context.checkpoint()?;
        let event =
            reader.read_event().map_err(|error| malformed(Some(part), error.to_string()))?;
        if mc.skip(&reader, &event, part)? {
            continue;
        }
        match event {
            Event::Start(element) | Event::Empty(element)
                if local(element.name().as_ref()) == "sldId" =>
            {
                if u32::try_from(result.len()).unwrap_or(u32::MAX) >= options.limits.max_pages {
                    return Err(limit("max_pages", "presentation slide count exceeds budget"));
                }
                let relationship_id = required_attr_ns(&reader, &element, R_NS, "id", part)?;
                result.push(SlideReference { relationship_id });
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(result)
}

pub(super) fn slide_is_hidden(
    bytes: &[u8],
    part: &str,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let mut reader = NsReader::from_reader(bytes);
    let mut events = 0_usize;
    loop {
        context.checkpoint()?;
        events = events
            .checked_add(1)
            .ok_or_else(|| limit("xml_events", "hidden-slide scan event count overflow"))?;
        if events > MAX_XML_EVENTS {
            return Err(limit("xml_events", format!("hidden-slide scan for {part}")));
        }
        match reader.read_event().map_err(|error| malformed(Some(part), error.to_string()))? {
            Event::Start(element) if local(element.name().as_ref()) == "sld" => {
                return Ok(optional_xml_bool(&element, "show", part)?.is_some_and(|show| !show));
            }
            Event::Eof => return Err(malformed(Some(part), "slide root is missing")),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn parse_shapes(
    bytes: &[u8],
    part: &str,
    profile: XmlProfile,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<Shape>, ConversionError> {
    preflight_xml(bytes, part, profile, options, context)?;
    let mut reader = NsReader::from_reader(bytes);
    let mut shapes = Vec::new();
    shapes.try_reserve(16).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve initial shapes for {part}: {error}"))
    })?;
    let mut shape = None::<Shape>;
    let mut marks = Vec::new();
    marks.try_reserve(6).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve run marks for {part}: {error}"))
    })?;
    let mut paragraph_default_marks = Vec::new();
    paragraph_default_marks.try_reserve(6).map_err(|error| {
        limit(
            "max_memory_bytes",
            format!("cannot reserve paragraph default marks for {part}: {error}"),
        )
    })?;
    let mut run_style = RichStyle::default();
    let mut paragraph_default_style = RichStyle::default();
    let mut text = String::new();
    text.try_reserve(256).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve text buffer for {part}: {error}"))
    })?;
    let mut paragraph_depth = 0_usize;
    let mut paragraph_properties_seen = false;
    let mut paragraph_bullet_seen = false;
    let mut paragraph_default_properties_seen = false;
    let mut run_properties_seen = false;
    let mut capturing_text = false;
    let mut row = None::<Vec<Vec<Inline>>>;
    let mut cell = None::<Vec<Inline>>;
    let mut cell_text_body_seen = false;
    let mut table_rows = Vec::<Vec<Vec<Inline>>>::new();
    let mut table_cell_count = 0_u64;
    let mut groups = Vec::<GroupTransform>::new();
    let mut mc = McSelection::default();
    let mut z_order = 0_usize;
    let mut parsed_inlines = 0_usize;
    loop {
        context.checkpoint()?;
        let event =
            reader.read_event().map_err(|error| malformed(Some(part), error.to_string()))?;
        if mc.skip(&reader, &event, part)? {
            continue;
        }
        match event {
            Event::Start(element) => match local(element.name().as_ref()) {
                "grpSp" => {
                    groups.try_reserve(1).map_err(|error| {
                        limit("max_memory_bytes", format!("cannot reserve group stack: {error}"))
                    })?;
                    groups.push(GroupTransform::default());
                }
                kind @ ("sp" | "pic" | "graphicFrame") if shape.is_none() => {
                    z_order = z_order
                        .checked_add(1)
                        .ok_or_else(|| limit("max_document_nodes", "z-order overflow"))?;
                    let kind = match kind {
                        "pic" => ShapeKind::Picture,
                        "graphicFrame" => ShapeKind::GraphicFrame,
                        _ => ShapeKind::Text,
                    };
                    shape = Some(Shape { kind, z_order, list_start: 1, ..Shape::default() });
                }
                "p" if shape.is_some() => {
                    if paragraph_depth == 0 {
                        paragraph_properties_seen = false;
                        paragraph_bullet_seen = false;
                        paragraph_default_properties_seen = false;
                        paragraph_default_marks.clear();
                        paragraph_default_style = RichStyle::default();
                        if cell.is_none() {
                            let current = shape.as_mut().expect("present");
                            current.level = 0;
                            current.bullet = None;
                            current.paragraph_explicit = 0;
                            current.list_start = 1;
                            current.numbering = None;
                        }
                    }
                    paragraph_depth = paragraph_depth.saturating_add(1);
                }
                "r" if shape.is_some() => {
                    run_properties_seen = false;
                    run_style = RichStyle::default();
                    marks = marks_for_style(run_style.with_lower(paragraph_default_style))?;
                }
                "rPr" if shape.is_some() => {
                    if run_properties_seen {
                        return Err(malformed(Some(part), "run has multiple rPr elements"));
                    }
                    run_properties_seen = true;
                    run_style = parse_rich_style(&element, part)?;
                    marks = marks_for_style(run_style.with_lower(paragraph_default_style))?;
                    record_language(&element, part, shape.as_mut().expect("present"))?;
                }
                "defRPr" if shape.is_some() => {
                    if paragraph_default_properties_seen {
                        return Err(malformed(
                            Some(part),
                            "paragraph has multiple defRPr elements",
                        ));
                    }
                    paragraph_default_properties_seen = true;
                    paragraph_default_style = parse_rich_style(&element, part)?;
                    paragraph_default_marks = marks_for_style(paragraph_default_style)?;
                    record_language(&element, part, shape.as_mut().expect("present"))?;
                }
                "t" if shape.is_some() => {
                    if capturing_text {
                        return Err(malformed(Some(part), "nested DrawingML text element"));
                    }
                    text.clear();
                    capturing_text = true;
                }
                "tr" if shape.is_some() => {
                    if row.is_some() {
                        return Err(malformed(Some(part), "nested table row"));
                    }
                    if u64::try_from(table_rows.len()).unwrap_or(u64::MAX)
                        >= options.limits.max_table_rows
                    {
                        return Err(limit("max_table_rows", "PresentationML table row budget"));
                    }
                    row = Some(Vec::new());
                }
                "tc" if row.is_some() => {
                    reject_merged_table_cell(&element, part)?;
                    if cell.is_some() {
                        return Err(malformed(Some(part), "nested table cell"));
                    }
                    let columns = row.as_ref().map_or(0, Vec::len);
                    if columns >= MAX_TABLE_COLUMNS
                        || u64::try_from(columns).unwrap_or(u64::MAX)
                            >= options.limits.max_table_columns
                    {
                        return Err(limit(
                            "max_table_columns",
                            "PresentationML table column budget",
                        ));
                    }
                    if table_cell_count >= options.limits.max_table_cells {
                        return Err(limit("max_table_cells", "PresentationML table cell budget"));
                    }
                    table_cell_count = table_cell_count
                        .checked_add(1)
                        .ok_or_else(|| limit("max_table_cells", "table cell count overflow"))?;
                    cell = Some(Vec::new());
                    cell_text_body_seen = false;
                }
                "txBody" if cell.is_some() => {
                    if cell_text_body_seen {
                        return Err(malformed(Some(part), "table cell has multiple text bodies"));
                    }
                    cell_text_body_seen = true;
                }
                "tbl" if shape.is_some() => {
                    let current = shape.as_mut().expect("present");
                    mark_semantic_once(
                        &mut current.semantic_seen,
                        SEEN_TABLE,
                        part,
                        "graphic frame has multiple tables",
                    )?;
                    if current.chart.is_some() {
                        return Err(malformed(
                            Some(part),
                            "graphic frame combines chart and table payloads",
                        ));
                    }
                }
                "pPr" if shape.is_some() => {
                    if paragraph_properties_seen {
                        return Err(malformed(Some(part), "paragraph has multiple pPr elements"));
                    }
                    paragraph_properties_seen = true;
                    apply_shape_element(&reader, &element, part, &mut shape)?;
                }
                "buChar" | "buAutoNum" | "buNone" | "buBlip" if shape.is_some() => {
                    if paragraph_bullet_seen {
                        return Err(malformed(
                            Some(part),
                            "paragraph has multiple bullet definitions",
                        ));
                    }
                    paragraph_bullet_seen = true;
                    apply_shape_element(&reader, &element, part, &mut shape)?;
                }
                _ if shape.is_none() && !groups.is_empty() => {
                    apply_group_element(&element, part, groups.last_mut().expect("present"))?;
                }
                _ => apply_shape_element(&reader, &element, part, &mut shape)?,
            },
            Event::Empty(element) => match local(element.name().as_ref()) {
                "sp" | "pic" | "graphicFrame" if shape.is_none() => {
                    return Err(malformed(Some(part), "empty shape container is invalid"));
                }
                "tr" if shape.is_some() => {
                    return Err(malformed(Some(part), "table row has no cells"));
                }
                "tc" if row.is_some() => {
                    return Err(malformed(Some(part), "empty table cell is invalid"));
                }
                "txBody" if cell.is_some() => {
                    return Err(malformed(Some(part), "empty table-cell text body is invalid"));
                }
                "rPr" if shape.is_some() => {
                    if run_properties_seen {
                        return Err(malformed(Some(part), "run has multiple rPr elements"));
                    }
                    run_properties_seen = true;
                    run_style = parse_rich_style(&element, part)?;
                    marks = marks_for_style(run_style.with_lower(paragraph_default_style))?;
                    record_language(&element, part, shape.as_mut().expect("present"))?;
                }
                "defRPr" if shape.is_some() => {
                    if paragraph_default_properties_seen {
                        return Err(malformed(
                            Some(part),
                            "paragraph has multiple defRPr elements",
                        ));
                    }
                    paragraph_default_properties_seen = true;
                    paragraph_default_style = parse_rich_style(&element, part)?;
                    paragraph_default_marks = marks_for_style(paragraph_default_style)?;
                    record_language(&element, part, shape.as_mut().expect("present"))?;
                }
                "br" if shape.is_some() => {
                    add_parsed_inline(&mut parsed_inlines)?;
                    let destination = if let Some(cell) = cell.as_mut() {
                        cell
                    } else {
                        &mut shape.as_mut().expect("present").text
                    };
                    destination.try_reserve(1).map_err(|error| {
                        limit("max_memory_bytes", format!("cannot reserve line break: {error}"))
                    })?;
                    destination.push(Inline::LineBreak);
                    if cell.is_none() {
                        let current = shape.as_mut().expect("present");
                        current.run_styles.try_reserve(1).map_err(|error| {
                            limit(
                                "max_memory_bytes",
                                format!("cannot reserve line-break style: {error}"),
                            )
                        })?;
                        current.run_styles.push(RichStyle::default());
                    }
                }
                "tbl" if shape.is_some() => {
                    let current = shape.as_mut().expect("present");
                    mark_semantic_once(
                        &mut current.semantic_seen,
                        SEEN_TABLE,
                        part,
                        "graphic frame has multiple tables",
                    )?;
                }
                _ if shape.is_none() && !groups.is_empty() => {
                    apply_group_element(&element, part, groups.last_mut().expect("present"))?;
                }
                "pPr" if shape.is_some() => {
                    if paragraph_properties_seen {
                        return Err(malformed(Some(part), "paragraph has multiple pPr elements"));
                    }
                    paragraph_properties_seen = true;
                    apply_shape_element(&reader, &element, part, &mut shape)?;
                }
                "buChar" | "buAutoNum" | "buNone" | "buBlip" if shape.is_some() => {
                    if paragraph_bullet_seen {
                        return Err(malformed(
                            Some(part),
                            "paragraph has multiple bullet definitions",
                        ));
                    }
                    paragraph_bullet_seen = true;
                    apply_shape_element(&reader, &element, part, &mut shape)?;
                }
                _ => apply_shape_element(&reader, &element, part, &mut shape)?,
            },
            Event::Text(value) if shape.is_some() && capturing_text => {
                append_shape_text(
                    &mut text,
                    &decode_text(&value, part)?,
                    part,
                    options.limits.max_field_bytes,
                )?;
            }
            Event::CData(value) if shape.is_some() && capturing_text => {
                append_shape_text(
                    &mut text,
                    &decode_cdata(&value, part)?,
                    part,
                    options.limits.max_field_bytes,
                )?;
            }
            Event::GeneralRef(value) if shape.is_some() && capturing_text => {
                append_shape_text(
                    &mut text,
                    &decode_reference(&value, part)?,
                    part,
                    options.limits.max_field_bytes,
                )?;
            }
            Event::End(element) => match local(element.name().as_ref()) {
                "t" if shape.is_some() => {
                    if !capturing_text {
                        return Err(malformed(Some(part), "text end without text start"));
                    }
                    capturing_text = false;
                    if u64::try_from(text.len()).unwrap_or(u64::MAX)
                        > options.limits.max_field_bytes
                    {
                        return Err(limit("max_field_bytes", format!("text in {part}")));
                    }
                    if text.is_empty() {
                        continue;
                    }
                    add_parsed_inline(&mut parsed_inlines)?;
                    let mut inline_marks = Vec::new();
                    inline_marks.try_reserve_exact(marks.len()).map_err(|error| {
                        limit("max_memory_bytes", format!("cannot reserve inline marks: {error}"))
                    })?;
                    inline_marks.extend_from_slice(&marks);
                    let inline =
                        Inline::Text { value: std::mem::take(&mut text), marks: inline_marks };
                    if let Some(cell) = cell.as_mut() {
                        cell.try_reserve(1).map_err(|error| {
                            limit(
                                "max_memory_bytes",
                                format!("cannot reserve table inline: {error}"),
                            )
                        })?;
                        cell.push(inline);
                    } else {
                        shape.as_mut().expect("present").text.try_reserve(1).map_err(|error| {
                            limit(
                                "max_memory_bytes",
                                format!("cannot reserve shape inline: {error}"),
                            )
                        })?;
                        shape.as_mut().expect("present").text.push(inline);
                        shape.as_mut().expect("present").run_styles.try_reserve(1).map_err(
                            |error| {
                                limit(
                                    "max_memory_bytes",
                                    format!("cannot reserve run style: {error}"),
                                )
                            },
                        )?;
                        shape.as_mut().expect("present").run_styles.push(run_style);
                    }
                }
                "p" if shape.is_some() => {
                    paragraph_depth = paragraph_depth.checked_sub(1).ok_or_else(|| {
                        malformed(Some(part), "paragraph end without paragraph start")
                    })?;
                    if let Some(cell) = cell.as_mut() {
                        if paragraph_depth != 0 || !cell.is_empty() {
                            add_parsed_inline(&mut parsed_inlines)?;
                            cell.try_reserve(1).map_err(|error| {
                                limit(
                                    "max_memory_bytes",
                                    format!("cannot reserve table line break: {error}"),
                                )
                            })?;
                            cell.push(Inline::LineBreak);
                        }
                    } else if paragraph_depth == 0 {
                        let current = shape.as_mut().expect("present");
                        trim_breaks(&mut current.text);
                        while current.run_styles.len() > current.text.len() {
                            current.run_styles.pop();
                        }
                        let retain_style =
                            matches!(profile, XmlProfile::Layout | XmlProfile::Master)
                                && (current.bullet.is_some()
                                    || current.paragraph_explicit != 0
                                    || current.numbering.is_some()
                                    || !paragraph_default_marks.is_empty()
                                    || !paragraph_default_style.is_absent());
                        if !current.text.is_empty() || retain_style {
                            if current.paragraphs.len() >= MAX_DOCUMENT_NODES {
                                return Err(limit(
                                    "max_document_nodes",
                                    "shape paragraph count exceeds IR budget",
                                ));
                            }
                            current.paragraphs.try_reserve(1).map_err(|error| {
                                limit(
                                    "max_memory_bytes",
                                    format!("cannot reserve text paragraph: {error}"),
                                )
                            })?;
                            current.paragraphs.push(TextParagraph {
                                text: std::mem::take(&mut current.text),
                                default_marks: std::mem::take(&mut paragraph_default_marks),
                                default_style: paragraph_default_style,
                                run_styles: std::mem::take(&mut current.run_styles),
                                level: current.level,
                                level_explicit: current.paragraph_explicit & EXPLICIT_LIST_LEVEL
                                    != 0,
                                bullet: current.bullet,
                                bullet_explicit: current.paragraph_explicit & EXPLICIT_BULLET != 0,
                                start: current.list_start.max(1),
                                numbering: current.numbering.take(),
                            });
                        }
                    }
                }
                "tc" if cell.is_some() => {
                    if !cell_text_body_seen {
                        return Err(malformed(Some(part), "table cell lacks a text body"));
                    }
                    trim_breaks(cell.as_mut().expect("cell present"));
                    row.as_mut().expect("row present").try_reserve(1).map_err(|error| {
                        limit("max_memory_bytes", format!("cannot reserve table cell: {error}"))
                    })?;
                    row.as_mut().expect("row present").push(cell.take().expect("cell present"));
                }
                "tr" if row.is_some() => {
                    if row.as_ref().is_some_and(Vec::is_empty) {
                        return Err(malformed(Some(part), "table row has no cells"));
                    }
                    table_rows.try_reserve(1).map_err(|error| {
                        limit("max_memory_bytes", format!("cannot reserve table row: {error}"))
                    })?;
                    table_rows.push(row.take().expect("row present"));
                }
                "sp" | "pic" | "graphicFrame" if shape.is_some() => {
                    let mut completed = shape.take().expect("present");
                    if !completed.text.is_empty() || paragraph_depth != 0 {
                        return Err(malformed(Some(part), "shape ended with incomplete paragraph"));
                    }
                    if capturing_text {
                        return Err(malformed(Some(part), "shape ended with incomplete text"));
                    }
                    if completed.kind == ShapeKind::Picture && completed.image.is_none() {
                        return Err(malformed(Some(part), "picture has no embedded image"));
                    }
                    let table_seen = completed.semantic_seen & SEEN_TABLE != 0;
                    if completed.kind == ShapeKind::GraphicFrame
                        && completed.chart.is_none()
                        && !table_seen
                    {
                        return Err(malformed(
                            Some(part),
                            "graphic frame has no supported chart or table payload",
                        ));
                    }
                    completed.languages.sort_unstable();
                    completed.languages.dedup();
                    completed.pending_groups.try_reserve_exact(groups.len()).map_err(|error| {
                        limit(
                            "max_memory_bytes",
                            format!("cannot reserve shape group transforms: {error}"),
                        )
                    })?;
                    completed.pending_groups.extend_from_slice(&groups);
                    if table_seen {
                        if table_rows.is_empty() {
                            return Err(malformed(Some(part), "table has no rows"));
                        }
                        completed.table = Some(std::mem::take(&mut table_rows));
                        table_cell_count = 0;
                    }
                    if shapes.len() >= MAX_DOCUMENT_NODES {
                        return Err(limit("max_document_nodes", "shape count exceeds IR budget"));
                    }
                    shapes.try_reserve(1).map_err(|error| {
                        limit(
                            "max_memory_bytes",
                            format!("cannot reserve completed shape: {error}"),
                        )
                    })?;
                    shapes.push(completed);
                }
                "grpSp" => {
                    groups.pop().ok_or_else(|| malformed(Some(part), "group end without start"))?;
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(shapes)
}
