//! Checkpointed XML scanning with borrowed source spans and bounded parser scratch.
use super::budget::{Budget, limit, malformed, owned};
use crate::text::LogicalMemory;
use into_markdown_core::{ConversionError, ExecutionContext};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use std::{borrow::Cow, collections::BTreeMap};

pub(super) enum Kind<'a> {
    Start(BytesStart<'a>, bool),
    End,
    Text(Cow<'a, str>),
}

pub(super) struct Token<'a> {
    pub kind: Kind<'a>,
    pub depth: usize,
    pub start: usize,
    pub end: usize,
}

/// Scan before quick-xml can perform an uninterruptible token-sized operation.
fn preflight(bytes: &[u8], budget: &Budget<'_>) -> Result<(), ConversionError> {
    let mut run = 0;
    for chunk in bytes.chunks(4096) {
        budget.context.checkpoint()?;
        for &byte in chunk {
            run += 1;
            if matches!(byte, b'<' | b'>') {
                run = 0;
            }
            budget.field(run)?;
        }
    }
    let text =
        std::str::from_utf8(bytes).map_err(|e| malformed(format!("XML must be UTF-8: {e}")))?;
    for (i, c) in text.chars().enumerate() {
        if i % 4096 == 0 {
            budget.context.checkpoint()?;
        }
        if !xml_char(c) {
            return Err(malformed("XML contains an invalid character"));
        }
    }
    Ok(())
}

fn xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

pub(super) fn scan<'a>(
    bytes: &'a [u8],
    budget: &mut Budget<'_>,
    mut visit: impl FnMut(Token<'a>, &mut Budget<'_>) -> Result<(), ConversionError>,
) -> Result<(), ConversionError> {
    preflight(bytes, budget)?;
    // Reader's end-name stack and token/attribute validation scratch are bounded by source bytes.
    let _scratch = budget
        .context
        .reserve_memory((bytes.len() as u64).saturating_mul(4).saturating_add(4096))?;
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().check_comments = true;
    reader.config_mut().allow_dangling_amp = false;
    let mut depth = 0;
    let mut roots = 0;
    loop {
        budget.event()?;
        let start = super::budget::size(reader.buffer_position())?;
        let event =
            reader.read_event().map_err(|e| malformed(format!("XML at byte {start}: {e}")))?;
        let end = super::budget::size(reader.buffer_position())?;
        let kind = match event {
            Event::Start(e) | Event::Empty(e) => {
                let empty = bytes.get(end.saturating_sub(2)..end) == Some(b"/>");
                if depth == 0 {
                    roots += 1;
                }
                if roots > 1 {
                    return Err(malformed("XML contains multiple roots"));
                }
                if depth >= usize::from(budget.options.limits.max_nesting_depth) {
                    return Err(limit(
                        "max_nesting_depth",
                        "Drawio XML nesting exceeds request limit",
                    ));
                }
                validate_attributes(&e, budget)?;
                let token = Token { kind: Kind::Start(e, empty), depth, start, end };
                visit(token, budget)?;
                if !empty {
                    depth += 1;
                }
                continue;
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or_else(|| malformed("unmatched XML end"))?;
                Kind::End
            }
            Event::Text(_) | Event::GeneralRef(_) => {
                let raw = std::str::from_utf8(&bytes[start..end])
                    .map_err(|e| malformed(e.to_string()))?;
                let text =
                    quick_xml::escape::unescape(raw).map_err(|e| malformed(e.to_string()))?;
                if !text.chars().all(xml_char) {
                    return Err(malformed("invalid XML character reference"));
                }
                if depth == 0 && !text.trim().is_empty() {
                    return Err(malformed("text outside XML root"));
                }
                Kind::Text(text)
            }
            Event::CData(_) => {
                if depth == 0 {
                    return Err(malformed("CDATA outside XML root"));
                }
                Kind::Text(
                    std::str::from_utf8(&bytes[start + 9..end - 3])
                        .map_err(|e| malformed(e.to_string()))?
                        .into(),
                )
            }
            Event::DocType(_) => {
                return Err(ConversionError::Unsupported {
                    detail: "Drawio DTD and entity declarations are forbidden".into(),
                });
            }
            Event::Eof => break,
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) => continue,
        };
        visit(Token { kind, depth, start, end }, budget)?;
    }
    if depth != 0 || roots != 1 {
        return Err(malformed("incomplete XML document"));
    }
    Ok(())
}

fn validate_attributes(e: &BytesStart<'_>, budget: &mut Budget<'_>) -> Result<(), ConversionError> {
    let mut keys = Vec::new();
    for attr in e.attributes().with_checks(false) {
        budget.event()?;
        if keys.len() >= 4096 {
            return Err(limit("drawio_xml_attributes", "XML element exceeds 4096 attributes"));
        }
        let attr = attr.map_err(|e| malformed(e.to_string()))?;
        budget.field(attr.value.len())?;
        let text = std::str::from_utf8(&attr.value).map_err(|e| malformed(e.to_string()))?;
        if text.contains('<') {
            return Err(malformed("unescaped less-than sign in XML attribute"));
        }
        let decoded = quick_xml::escape::unescape(text).map_err(|e| malformed(e.to_string()))?;
        if !decoded.chars().all(xml_char) {
            return Err(malformed("invalid XML character reference"));
        }
        keys.push(attr.key.0);
    }
    keys.sort_unstable();
    if keys.windows(2).any(|p| p[0] == p[1]) {
        return Err(malformed("duplicate XML attribute"));
    }
    Ok(())
}

pub(super) fn attributes(
    e: &BytesStart<'_>,
    memory: &mut LogicalMemory,
) -> Result<BTreeMap<String, String>, ConversionError> {
    let mut attrs = BTreeMap::new();
    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|e| malformed(e.to_string()))?;
        let key = std::str::from_utf8(attr.key.as_ref()).map_err(|e| malformed(e.to_string()))?;
        let text = std::str::from_utf8(&attr.value).map_err(|e| malformed(e.to_string()))?;
        memory.charge(256 + text.len())?;
        let value =
            quick_xml::escape::unescape(text).map_err(|e| malformed(e.to_string()))?.into_owned();
        attrs.insert(owned(key, memory)?, value);
    }
    Ok(attrs)
}

pub(crate) fn evidence(bytes: &[u8], context: &ExecutionContext) -> Result<bool, ConversionError> {
    context.checkpoint()?;
    let prefix = &bytes[..bytes.len().min(64 * 1024)];
    let mut reader = Reader::from_reader(prefix);
    for _ in 0..256 {
        context.checkpoint()?;
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                return Ok(matches!(e.name().as_ref(), b"mxfile" | b"mxGraphModel"));
            }
            Ok(Event::Decl(_) | Event::Comment(_) | Event::PI(_)) => (),
            Ok(Event::Text(e)) if e.iter().all(u8::is_ascii_whitespace) => (),
            _ => return Ok(false),
        }
    }
    Ok(false)
}
