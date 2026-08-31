//! Ordered cells and source references, adapted from diagram-design's three-pass extractor.
use super::{
    budget::{Budget, malformed},
    xml::{self, Kind},
};
use crate::text::LogicalMemory;
use into_markdown_core::{ConversionError, SourceLocator};
use std::{collections::BTreeMap, ops::Range};

pub(super) struct Cell {
    pub attrs: BTreeMap<String, String>,
    pub span: Range<usize>,
    pub source_point: Option<String>,
    pub target_point: Option<String>,
}

impl Cell {
    pub fn attr(&self, key: &str) -> &str {
        self.attrs.get(key).map_or("", String::as_str)
    }
    pub fn id(&self) -> &str {
        self.attr("id")
    }
    pub fn edge(&self) -> bool {
        self.attr("edge") == "1"
    }
    pub fn label(&self) -> &str {
        self.attrs.get("label").map_or_else(|| self.attr("value"), String::as_str)
    }
    pub fn style(&self, key: &str) -> Option<&str> {
        self.attr("style")
            .split(';')
            .filter_map(|v| {
                let (k, value) = v.split_once('=').unwrap_or((v, "1"));
                (k.trim() == key).then_some(value.trim())
            })
            .next_back()
    }
    pub fn locator(
        &self,
        page: u32,
        ordinal: usize,
        offset: Option<usize>,
        payload: &Range<usize>,
    ) -> SourceLocator {
        let range = offset
            .map_or_else(|| payload.clone(), |base| self.span.start + base..self.span.end + base);
        SourceLocator {
            page: Some(page),
            part: Some(format!("drawio/pages/{page}/cells/{}", ordinal + 1)),
            byte_start: Some(range.start as u64),
            byte_end: Some(range.end as u64),
            ..SourceLocator::default()
        }
    }
}

pub(super) struct Model {
    pub cells: Vec<Cell>,
    pub _memory: LogicalMemory,
}

pub(super) fn parse(bytes: &[u8], budget: &mut Budget<'_>) -> Result<Model, ConversionError> {
    let mut memory = LogicalMemory::new(budget.context)?;
    let mut cells = Vec::new();
    let mut wrapper: Option<(BTreeMap<String, String>, usize)> = None;
    let mut active: Option<(Cell, usize)> = None;
    let mut in_object = false;
    let mut root_seen = false;
    xml::scan(bytes, budget, |token, budget| {
        match token.kind {
            Kind::Start(e, empty) => {
                let name = e.name();
                match token.depth {
                    0 if name.as_ref() != b"mxGraphModel" => {
                        return Err(malformed("page payload must have mxGraphModel root"));
                    }
                    1 => {
                        if name.as_ref() != b"root" || root_seen {
                            return Err(malformed(
                                "model requires exactly one root cell container",
                            ));
                        }
                        root_seen = true;
                    }
                    2 if matches!(name.as_ref(), b"object" | b"UserObject") => {
                        if empty {
                            return Err(malformed("user object has no mxCell"));
                        }
                        wrapper = Some((xml::attributes(&e, &mut memory)?, token.start));
                        in_object = true;
                    }
                    2 | 3
                        if name.as_ref() == b"mxCell"
                            && (token.depth == 2 || wrapper.is_some()) =>
                    {
                        if active.is_some() {
                            return Err(malformed("nested mxCell"));
                        }
                        budget.cell()?;
                        let cell =
                            read_cell(&e, wrapper.take(), token.start..token.end, &mut memory)?;
                        if empty {
                            memory.reserve_vec(&mut cells, 1)?;
                            cells.push(cell);
                        } else {
                            active = Some((cell, token.depth));
                        }
                    }
                    3 if in_object && name.as_ref() == b"mxCell" => {
                        return Err(malformed("user object has multiple mxCell children"));
                    }
                    2 => {
                        return Err(malformed(
                            "unsupported root child; expected mxCell or user object",
                        ));
                    }
                    _ => point(&e, active.as_mut().map(|v| &mut v.0), &mut memory)?,
                }
            }
            Kind::End => {
                if active.as_ref().is_some_and(|(_, depth)| *depth == token.depth) {
                    let (mut cell, _) =
                        active.take().ok_or_else(|| malformed("missing active cell"))?;
                    cell.span.end = token.end;
                    memory.reserve_vec(&mut cells, 1)?;
                    cells.push(cell);
                }
                if token.depth == 2 {
                    in_object = false;
                    if wrapper.take().is_some() {
                        return Err(malformed("user object has no mxCell"));
                    }
                }
            }
            Kind::Text(text) if !text.trim().is_empty() => {
                return Err(malformed("unexpected text in graph model"));
            }
            Kind::Text(_) => (),
        }
        Ok(())
    })?;
    if !root_seen {
        return Err(malformed("mxGraphModel has no root"));
    }
    Ok(Model { cells, _memory: memory })
}

fn point(
    e: &quick_xml::events::BytesStart<'_>,
    cell: Option<&mut Cell>,
    memory: &mut LogicalMemory,
) -> Result<(), ConversionError> {
    if e.name().as_ref() != b"mxPoint" {
        return Ok(());
    }
    let Some(cell) = cell else {
        return Ok(());
    };
    let attrs = xml::attributes(e, memory)?;
    let kind = attrs.get("as").map(String::as_str);
    if !matches!(kind, Some("sourcePoint" | "targetPoint")) {
        return Ok(());
    }
    let x = attrs.get("x").map_or("0", String::as_str);
    let y = attrs.get("y").map_or("0", String::as_str);
    if [x, y].iter().any(|v| v.parse::<f64>().map_or(true, |f| !f.is_finite())) {
        return Err(malformed("endpoint coordinates must be finite numbers"));
    }
    memory.charge(x.len() + y.len() + 16)?;
    let value = format!("({x}, {y})");
    if kind == Some("sourcePoint") {
        cell.source_point = Some(value);
    } else {
        cell.target_point = Some(value);
    }
    Ok(())
}

fn read_cell(
    event: &quick_xml::events::BytesStart<'_>,
    wrapper: Option<(BTreeMap<String, String>, usize)>,
    mut span: Range<usize>,
    memory: &mut LogicalMemory,
) -> Result<Cell, ConversionError> {
    let mut attrs = xml::attributes(event, memory)?;
    if let Some((outer, start)) = wrapper {
        span.start = start;
        for (key, value) in outer {
            if matches!(key.as_str(), "id" | "label") {
                attrs.insert(key, value);
            } else {
                attrs.entry(key).or_insert(value);
            }
        }
    }
    Ok(Cell { attrs, span, source_point: None, target_point: None })
}
