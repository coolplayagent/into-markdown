use crate::workbook::error::{limit, malformed};
use crate::workbook::model::CellCoordinate;
use crate::workbook::opc::relationships::decode_attr;
use crate::workbook::schema::{
    CHART_NS, DRAWINGML_NS, MAX_EXCEL_COLUMNS, MAX_EXCEL_ROWS, OFFICE_REL_NS, OFFICE_REL_STRICT_NS,
    SPREADSHEET_DRAWING_NS,
};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DrawingReferenceKind {
    Image,
    Chart,
}

#[derive(Debug)]
pub(super) struct DrawingReference {
    pub(super) start: CellCoordinate,
    pub(super) end: CellCoordinate,
    pub(super) relationship_id: String,
    pub(super) alt: Option<String>,
    pub(super) kind: DrawingReferenceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawingAnchorKind {
    OneCell,
    TwoCell,
    Absolute,
}

#[derive(Debug)]
struct PendingDrawingAnchor {
    kind: DrawingAnchorKind,
    from: Option<CellCoordinate>,
    to: Option<CellCoordinate>,
    reference: Option<(DrawingReferenceKind, String)>,
    alt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawingPosition {
    From,
    To,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawingCoordinate {
    Row,
    Column,
}

fn resolved_namespace_is(namespace: &ResolveResult<'_>, expected: &[u8]) -> bool {
    matches!(namespace, ResolveResult::Bound(value) if value.as_ref() == expected)
}

#[allow(clippy::too_many_lines)]
pub(super) fn parse_drawing_references(
    xml: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<DrawingReference>, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut output = Vec::new();
    let mut anchor = None::<PendingDrawingAnchor>;
    let mut position = None::<DrawingPosition>;
    let mut position_row = None::<u32>;
    let mut position_column = None::<u32>;
    let mut coordinate = None::<DrawingCoordinate>;
    let mut coordinate_text = String::new();
    let mut depth = 0_u16;
    let mut saw_root = false;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event))) => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| limit("max_nesting_depth", "drawing depth overflow"))?;
                if depth > options.limits.max_nesting_depth {
                    return Err(limit("max_nesting_depth", "drawing is too deeply nested"));
                }
                let local = event.local_name();
                match local.as_ref() {
                    b"wsDr" => {
                        if saw_root
                            || depth != 1
                            || !resolved_namespace_is(&namespace, SPREADSHEET_DRAWING_NS)
                        {
                            return Err(malformed(Some(part), "invalid DrawingML root"));
                        }
                        saw_root = true;
                    }
                    b"oneCellAnchor" | b"twoCellAnchor" | b"absoluteAnchor" => {
                        if !resolved_namespace_is(&namespace, SPREADSHEET_DRAWING_NS)
                            || anchor.is_some()
                        {
                            return Err(malformed(Some(part), "invalid nested drawing anchor"));
                        }
                        anchor = Some(PendingDrawingAnchor {
                            kind: match local.as_ref() {
                                b"oneCellAnchor" => DrawingAnchorKind::OneCell,
                                b"twoCellAnchor" => DrawingAnchorKind::TwoCell,
                                _ => DrawingAnchorKind::Absolute,
                            },
                            from: None,
                            to: None,
                            reference: None,
                            alt: None,
                        });
                    }
                    b"from" | b"to" if anchor.is_some() => {
                        if !resolved_namespace_is(&namespace, SPREADSHEET_DRAWING_NS)
                            || position.is_some()
                        {
                            return Err(malformed(Some(part), "invalid drawing anchor position"));
                        }
                        position = Some(if local.as_ref() == b"from" {
                            DrawingPosition::From
                        } else {
                            DrawingPosition::To
                        });
                        position_row = None;
                        position_column = None;
                    }
                    b"row" | b"col" if position.is_some() => {
                        if !resolved_namespace_is(&namespace, SPREADSHEET_DRAWING_NS)
                            || coordinate.is_some()
                        {
                            return Err(malformed(Some(part), "invalid drawing anchor coordinate"));
                        }
                        coordinate = Some(if local.as_ref() == b"row" {
                            DrawingCoordinate::Row
                        } else {
                            DrawingCoordinate::Column
                        });
                        coordinate_text.clear();
                    }
                    b"cNvPr" if anchor.is_some() => {
                        if !resolved_namespace_is(&namespace, SPREADSHEET_DRAWING_NS) {
                            return Err(malformed(
                                Some(part),
                                "invalid drawing property namespace",
                            ));
                        }
                        capture_drawing_alt(
                            &reader,
                            &event,
                            anchor.as_mut().unwrap(),
                            part,
                            options,
                        )?;
                    }
                    b"blip" if anchor.is_some() => {
                        if !resolved_namespace_is(&namespace, DRAWINGML_NS) {
                            return Err(malformed(Some(part), "invalid image reference namespace"));
                        }
                        capture_drawing_relationship(
                            &reader,
                            &event,
                            anchor.as_mut().unwrap(),
                            DrawingReferenceKind::Image,
                            part,
                        )?;
                    }
                    b"chart" if anchor.is_some() => {
                        if !resolved_namespace_is(&namespace, CHART_NS) {
                            return Err(malformed(Some(part), "invalid chart reference namespace"));
                        }
                        capture_drawing_relationship(
                            &reader,
                            &event,
                            anchor.as_mut().unwrap(),
                            DrawingReferenceKind::Chart,
                            part,
                        )?;
                    }
                    _ => {}
                }
            }
            Ok((namespace, Event::Empty(event))) => {
                let local = event.local_name();
                match local.as_ref() {
                    b"wsDr" => {
                        if saw_root
                            || depth != 0
                            || !resolved_namespace_is(&namespace, SPREADSHEET_DRAWING_NS)
                        {
                            return Err(malformed(Some(part), "invalid DrawingML root"));
                        }
                        saw_root = true;
                    }
                    b"cNvPr" if anchor.is_some() => {
                        if !resolved_namespace_is(&namespace, SPREADSHEET_DRAWING_NS) {
                            return Err(malformed(
                                Some(part),
                                "invalid drawing property namespace",
                            ));
                        }
                        capture_drawing_alt(
                            &reader,
                            &event,
                            anchor.as_mut().unwrap(),
                            part,
                            options,
                        )?;
                    }
                    b"blip" if anchor.is_some() => {
                        if !resolved_namespace_is(&namespace, DRAWINGML_NS) {
                            return Err(malformed(Some(part), "invalid image reference namespace"));
                        }
                        capture_drawing_relationship(
                            &reader,
                            &event,
                            anchor.as_mut().unwrap(),
                            DrawingReferenceKind::Image,
                            part,
                        )?;
                    }
                    b"chart" if anchor.is_some() => {
                        if !resolved_namespace_is(&namespace, CHART_NS) {
                            return Err(malformed(Some(part), "invalid chart reference namespace"));
                        }
                        capture_drawing_relationship(
                            &reader,
                            &event,
                            anchor.as_mut().unwrap(),
                            DrawingReferenceKind::Chart,
                            part,
                        )?;
                    }
                    _ => {}
                }
            }
            Ok((_, Event::Text(text))) if coordinate.is_some() => {
                let value = text.xml_content().map_err(|error| {
                    malformed(Some(part), format!("invalid drawing coordinate: {error}"))
                })?;
                if coordinate_text.len().saturating_add(value.len()) > 10 {
                    return Err(malformed(Some(part), "drawing coordinate is too long"));
                }
                coordinate_text.push_str(&value);
            }
            Ok((_, Event::End(event))) => {
                let local = event.local_name();
                match local.as_ref() {
                    b"row" | b"col" if coordinate.is_some() => {
                        let value = coordinate_text
                            .parse::<u32>()
                            .map_err(|_| malformed(Some(part), "invalid drawing coordinate"))?;
                        match coordinate.take().unwrap() {
                            DrawingCoordinate::Row => {
                                if value >= MAX_EXCEL_ROWS || position_row.replace(value).is_some()
                                {
                                    return Err(malformed(
                                        Some(part),
                                        "invalid drawing row anchor",
                                    ));
                                }
                            }
                            DrawingCoordinate::Column => {
                                if value >= MAX_EXCEL_COLUMNS
                                    || position_column.replace(value).is_some()
                                {
                                    return Err(malformed(
                                        Some(part),
                                        "invalid drawing column anchor",
                                    ));
                                }
                            }
                        }
                    }
                    b"from" | b"to" if position.is_some() => {
                        let cell = position_row
                            .zip(position_column)
                            .ok_or_else(|| malformed(Some(part), "incomplete drawing anchor"))?;
                        let current_position = position.take().unwrap();
                        let pending = anchor.as_mut().ok_or_else(|| {
                            malformed(Some(part), "position outside drawing anchor")
                        })?;
                        let slot = match current_position {
                            DrawingPosition::From => &mut pending.from,
                            DrawingPosition::To => &mut pending.to,
                        };
                        if slot.replace(cell).is_some() {
                            return Err(malformed(Some(part), "duplicate drawing anchor position"));
                        }
                    }
                    b"oneCellAnchor" | b"twoCellAnchor" | b"absoluteAnchor" => {
                        let pending = anchor.take().ok_or_else(|| {
                            malformed(Some(part), "drawing anchor end without start")
                        })?;
                        let (start, end) = match pending.kind {
                            DrawingAnchorKind::OneCell => {
                                let start = pending.from.ok_or_else(|| {
                                    malformed(Some(part), "one-cell anchor lacks from position")
                                })?;
                                if pending.to.is_some() {
                                    return Err(malformed(
                                        Some(part),
                                        "one-cell anchor has to position",
                                    ));
                                }
                                (start, start)
                            }
                            DrawingAnchorKind::TwoCell => {
                                let start = pending.from.ok_or_else(|| {
                                    malformed(Some(part), "two-cell anchor lacks from position")
                                })?;
                                let end = pending.to.ok_or_else(|| {
                                    malformed(Some(part), "two-cell anchor lacks to position")
                                })?;
                                if start.0 > end.0 || start.1 > end.1 {
                                    return Err(malformed(
                                        Some(part),
                                        "reversed drawing anchor range",
                                    ));
                                }
                                (start, end)
                            }
                            DrawingAnchorKind::Absolute => {
                                if pending.from.is_some() || pending.to.is_some() {
                                    return Err(malformed(
                                        Some(part),
                                        "absolute anchor has cell position",
                                    ));
                                }
                                ((0, 0), (0, 0))
                            }
                        };
                        if let Some((kind, relationship_id)) = pending.reference {
                            output.push(DrawingReference {
                                start,
                                end,
                                relationship_id,
                                alt: pending.alt,
                                kind,
                            });
                        }
                    }
                    _ => {}
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| malformed(Some(part), "drawing XML depth underflow"))?;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid drawing XML: {error}")));
            }
            _ => {}
        }
        if output.len() as u64 > options.limits.max_table_cells {
            return Err(limit("max_table_cells", "too many drawing anchors"));
        }
    }
    if !saw_root || depth != 0 || anchor.is_some() || position.is_some() || coordinate.is_some() {
        return Err(malformed(Some(part), "incomplete drawing XML"));
    }
    Ok(output)
}

fn capture_drawing_alt(
    reader: &quick_xml::reader::NsReader<&[u8]>,
    event: &quick_xml::events::BytesStart<'_>,
    anchor: &mut PendingDrawingAnchor,
    part: &str,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let mut candidate = None;
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("drawing property: {error}")))?;
        if !matches!(reader.resolve_attribute(attr.key).0, ResolveResult::Unbound) {
            continue;
        }
        if matches!(attr.key.local_name().as_ref(), b"descr" | b"title" | b"name") {
            let value = decode_attr(&attr, part)?;
            if u64::try_from(value.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
                return Err(limit("max_field_bytes", "drawing alternative text is too large"));
            }
            if !value.is_empty()
                && (candidate.is_none() || attr.key.local_name().as_ref() != b"name")
            {
                candidate = Some(value);
            }
        }
    }
    if candidate.is_some() {
        anchor.alt = candidate;
    }
    Ok(())
}

fn capture_drawing_relationship(
    reader: &quick_xml::reader::NsReader<&[u8]>,
    event: &quick_xml::events::BytesStart<'_>,
    anchor: &mut PendingDrawingAnchor,
    kind: DrawingReferenceKind,
    part: &str,
) -> Result<(), ConversionError> {
    let mut relationship_id = None;
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("drawing relationship: {error}")))?;
        let local = attr.key.local_name();
        if matches!(local.as_ref(), b"embed" | b"link" | b"id")
            && matches!(
                reader.resolve_attribute(attr.key),
                (ResolveResult::Bound(namespace), _)
                    if namespace.as_ref() == OFFICE_REL_NS
                        || namespace.as_ref() == OFFICE_REL_STRICT_NS
            )
        {
            if local.as_ref() == b"link" {
                return Err(ConversionError::Unsupported {
                    detail: format!("external linked drawing is forbidden ({part})"),
                });
            }
            if relationship_id.replace(decode_attr(&attr, part)?).is_some() {
                return Err(malformed(Some(part), "duplicate drawing relationship attribute"));
            }
        }
    }
    let relationship_id = relationship_id
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed(Some(part), "drawing object lacks relationship id"))?;
    if anchor.reference.replace((kind, relationship_id)).is_some() {
        return Err(malformed(Some(part), "drawing anchor references multiple objects"));
    }
    Ok(())
}
