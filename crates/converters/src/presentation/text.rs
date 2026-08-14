use super::error::limit;
use super::model::Shape;
use into_markdown_core::{ConversionError, Inline};

pub(super) fn trim_breaks(inlines: &mut Vec<Inline>) {
    while matches!(inlines.last(), Some(Inline::LineBreak)) {
        inlines.pop();
    }
}

pub(super) fn shape_plain_text(shape: &Shape) -> Result<String, ConversionError> {
    let mut title = String::new();
    for paragraph in &shape.paragraphs {
        let value = plain_text(&paragraph.text)?;
        if value.is_empty() {
            continue;
        }
        let separator = usize::from(!title.is_empty());
        let additional = value
            .len()
            .checked_add(separator)
            .ok_or_else(|| limit("max_memory_bytes", "title capacity overflow"))?;
        title.try_reserve(additional).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve slide title: {error}"))
        })?;
        if !title.is_empty() {
            title.push(' ');
        }
        title.push_str(&value);
    }
    Ok(title)
}

pub(super) fn plain_text(inlines: &[Inline]) -> Result<String, ConversionError> {
    let capacity = inlines.iter().try_fold(0_usize, |total, inline| {
        let length = match inline {
            Inline::Text { value, .. } | Inline::Code(value) => value.len(),
            Inline::LineBreak => 1,
            _ => 0,
        };
        total
            .checked_add(length)
            .ok_or_else(|| limit("max_memory_bytes", "plain-text capacity overflow"))
    })?;
    let mut value = String::new();
    value.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve plain text: {error}"))
    })?;
    for inline in inlines {
        match inline {
            Inline::Text { value: text, .. } | Inline::Code(text) => value.push_str(text),
            Inline::LineBreak => value.push(' '),
            _ => {}
        }
    }
    let leading = value.len().saturating_sub(value.trim_start().len());
    if leading != 0 {
        value.drain(..leading);
    }
    value.truncate(value.trim_end().len());
    Ok(value)
}
