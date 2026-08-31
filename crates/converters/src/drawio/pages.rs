use super::{
    budget::{Budget, limit, malformed},
    xml::{self, Kind},
};
use crate::text::LogicalMemory;
use into_markdown_core::ConversionError;
use std::ops::Range;

pub(super) struct Page {
    pub name: String,
    pub id: String,
    pub span: Range<usize>,
    pub model: Option<Range<usize>>,
    pub payload: String,
    pub payload_span: Range<usize>,
    pub error: Option<&'static str>,
}

pub(super) struct Pages {
    pub pages: Vec<Page>,
    pub _memory: LogicalMemory,
}

pub(super) fn read(bytes: &[u8], budget: &mut Budget<'_>) -> Result<Pages, ConversionError> {
    let mut memory = LogicalMemory::new(budget.context)?;
    let mut pages = Vec::new();
    let mut current: Option<Page> = None;
    let mut bare = false;
    let mut model_start = None;
    xml::scan(bytes, budget, |token, budget| {
        match token.kind {
            Kind::Start(e, empty) if token.depth == 0 => {
                match e.name().as_ref() {
                    b"mxGraphModel" => bare = true,
                    b"mxfile" => (),
                    _ => return Err(malformed("expected mxfile or mxGraphModel root for Drawio")),
                }
                if empty && !bare {
                    return Err(malformed("mxfile contains no pages"));
                }
            }
            Kind::Start(e, empty) if !bare && token.depth == 1 => {
                if e.name().as_ref() != b"diagram" {
                    return Err(malformed("mxfile child must be diagram"));
                }
                if pages.len() as u64 >= u64::from(budget.options.limits.max_pages) {
                    return Err(limit("max_pages", "Drawio page count exceeds request limit"));
                }
                let mut attrs = xml::attributes(&e, &mut memory)?;
                current = Some(Page {
                    name: attrs.remove("name").unwrap_or_default(),
                    id: attrs.remove("id").unwrap_or_default(),
                    span: token.start..token.end,
                    model: None,
                    payload: String::new(),
                    payload_span: token.end..token.end,
                    error: None,
                });
                if empty {
                    finish(&mut current, &mut pages, &mut memory, token.end)?;
                }
            }
            Kind::Start(e, empty) if !bare && token.depth == 2 => {
                let page = current.as_mut().ok_or_else(|| malformed("model outside diagram"))?;
                if e.name().as_ref() != b"mxGraphModel" {
                    page.error = Some("diagram child must be mxGraphModel");
                    return Ok(());
                }
                if page.model.is_some() || model_start.is_some() {
                    page.error = Some("multiple models in one diagram");
                    return Ok(());
                }
                if empty {
                    page.model = Some(token.start..token.end);
                } else {
                    model_start = Some(token.start);
                }
            }
            Kind::End if !bare && token.depth == 2 => {
                if let Some(start) = model_start.take() {
                    current.as_mut().ok_or_else(|| malformed("model outside diagram"))?.model =
                        Some(start..token.end);
                }
            }
            Kind::End if !bare && token.depth == 1 => {
                finish(&mut current, &mut pages, &mut memory, token.end)?;
            }
            Kind::Text(text) if !bare && token.depth == 2 => {
                let page = current.as_mut().ok_or_else(|| malformed("text outside diagram"))?;
                memory.reserve_string(&mut page.payload, text.len())?;
                page.payload.push_str(&text);
                page.payload_span.end = token.end;
            }
            Kind::Text(text) if !bare && token.depth == 1 && !text.trim().is_empty() => {
                return Err(malformed("unexpected text outside diagram"));
            }
            _ => (),
        }
        Ok(())
    })?;
    if bare {
        if budget.options.limits.max_pages == 0 {
            return Err(limit("max_pages", "Drawio contains one page"));
        }
        memory.reserve_vec(&mut pages, 1)?;
        pages.push(Page {
            name: String::new(),
            id: String::new(),
            span: 0..bytes.len(),
            model: Some(0..bytes.len()),
            payload: String::new(),
            payload_span: 0..bytes.len(),
            error: None,
        });
    }
    if pages.is_empty() {
        return Err(malformed("mxfile contains no pages"));
    }
    Ok(Pages { pages, _memory: memory })
}

fn finish(
    current: &mut Option<Page>,
    pages: &mut Vec<Page>,
    memory: &mut LogicalMemory,
    end: usize,
) -> Result<(), ConversionError> {
    let mut page = current.take().ok_or_else(|| malformed("unmatched diagram end"))?;
    page.span.end = end;
    memory.reserve_vec(pages, 1)?;
    pages.push(page);
    Ok(())
}
