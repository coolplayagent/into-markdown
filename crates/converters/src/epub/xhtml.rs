//! XHTML container-reference rewriting and footnote discovery.

use super::budget::EpubBudget;
use super::path::BasePath;
use super::xml::{self, Name};
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use quick_xml::Writer;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use std::collections::{BTreeMap, BTreeSet};

const XHTML_NS: &[u8] = b"http://www.w3.org/1999/xhtml";
const EPUB_NS: &[u8] = b"http://www.idpf.org/2007/ops";

#[derive(Clone, Debug)]
pub(super) struct Footnote {
    pub(super) target: String,
    pub(super) text: String,
}

pub(super) struct PreparedXhtml {
    pub(super) bytes: Vec<u8>,
    pub(super) references: BTreeMap<String, String>,
    pub(super) internal_targets: BTreeSet<String>,
    pub(super) anchors: BTreeSet<String>,
    pub(super) footnotes: Vec<Footnote>,
    pub(super) _memory: ResourceReservation,
}

struct Frame {
    name: Name,
    base: BasePath,
    footnote: Option<FootnoteBuilder>,
    suppressed_text: bool,
    suppressed_output: bool,
}

struct FootnoteBuilder {
    target: String,
    text: String,
}

#[allow(clippy::too_many_lines)] // Rewriting and footnote extraction share the namespace stack.
pub(super) fn prepare(
    path: &str,
    bytes: &[u8],
    archive: &SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
    context: &ExecutionContext,
) -> Result<PreparedXhtml, ConversionError> {
    let equals =
        bytes.iter().fold(0_usize, |count, byte| count.saturating_add(usize::from(*byte == b'=')));
    let planned = bytes
        .len()
        .checked_mul(3)
        .and_then(|value| value.checked_add(equals.saturating_mul(64)))
        .and_then(|value| value.checked_add(64 * 1024))
        .ok_or_else(|| memory_limit("XHTML rewrite plan overflowed"))?;
    let mut memory = context.reserve_memory(u64::try_from(planned).unwrap_or(u64::MAX))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(planned)
        .map_err(|error| memory_limit(format!("reserve XHTML rewrite: {error}")))?;
    let actual = u64::try_from(output.capacity()).unwrap_or(u64::MAX);
    let planned_u64 = u64::try_from(planned).unwrap_or(u64::MAX);
    if actual > planned_u64 {
        memory.grow(actual - planned_u64)?;
    } else if planned_u64 > actual {
        memory.shrink(planned_u64 - actual)?;
    }
    let charged = actual;
    let mut writer = Writer::new(output);
    let mut reader = xml::reader(bytes);
    let initial_base = BasePath::document(path)?;
    let mut stack = Vec::<Frame>::new();
    let mut root_seen = false;
    let mut declaration_seen = false;
    let mut doctype_seen = false;
    let mut references = BTreeMap::new();
    let mut internal_targets = BTreeSet::new();
    let mut anchors = BTreeSet::new();
    let mut footnotes = Vec::new();
    loop {
        let event = reader.read_event().map_err(|error| xml::malformed(error.to_string()))?;
        let empty = matches!(&event, Event::Empty(_));
        let depth = stack
            .len()
            .saturating_add(usize::from(matches!(&event, Event::Start(_) | Event::Empty(_))));
        budget.event(depth)?;
        match event {
            Event::DocType(value)
                if !root_seen
                    && stack.is_empty()
                    && !doctype_seen
                    && value.as_ref().eq_ignore_ascii_case(b"html") =>
            {
                doctype_seen = true;
                writer.write_event(Event::DocType(value.into_owned())).map_err(write_error)?;
            }
            Event::DocType(_) | Event::PI(_) => {
                return Err(xml::malformed("active DTD or processing instruction in XHTML"));
            }
            Event::Start(element) | Event::Empty(element) => {
                if stack.is_empty() && root_seen {
                    return Err(xml::malformed("XHTML has multiple root elements"));
                }
                let name = xml::name(&reader, &element)?;
                let attributes = xml::attributes(&reader, &element)?;
                let parent_base = stack.last().map_or(&initial_base, |frame| &frame.base);
                let base = xml::optional(&attributes, Some(xml::XML_NS), b"base")
                    .map_or_else(|| Ok(parent_base.clone()), |value| parent_base.apply(value))?;
                if stack.is_empty() {
                    if !name.matches(Some(XHTML_NS), b"html") {
                        return Err(xml::malformed("spine document root is not XHTML html"));
                    }
                    root_seen = true;
                }
                if name.matches(Some(XHTML_NS), b"base") {
                    return Err(xml::malformed("HTML base elements are forbidden in EPUB XHTML"));
                }
                if let Some(id) = xml::optional(&attributes, None, b"id") {
                    let target = initial_base.resolve(&format!("#{id}"))?.canonical_target();
                    if !anchors.insert(target) {
                        return Err(xml::malformed("duplicate XHTML anchor identity"));
                    }
                }
                let types = xml::optional(&attributes, Some(EPUB_NS), b"type")
                    .unwrap_or_default()
                    .split_whitespace()
                    .collect::<BTreeSet<_>>();
                let is_footnote = types.contains("footnote") || types.contains("endnote");
                if is_footnote && stack.iter().any(|frame| frame.footnote.is_some()) {
                    return Err(xml::malformed("nested EPUB footnote definitions are invalid"));
                }
                let footnote = if is_footnote {
                    let id = xml::required(&attributes, None, b"id", "footnote id")?;
                    Some(FootnoteBuilder {
                        target: initial_base.resolve(&format!("#{id}"))?.canonical_target(),
                        text: String::new(),
                    })
                } else {
                    None
                };
                let suppressed_text = stack.last().is_some_and(|frame| frame.suppressed_text)
                    || name.namespace.as_deref() == Some(XHTML_NS)
                        && matches!(name.local.as_slice(), b"script" | b"style");
                let suppressed_output =
                    is_footnote || stack.last().is_some_and(|frame| frame.suppressed_output);
                if !suppressed_output {
                    let rewritten = rewrite_element(
                        &reader,
                        &element,
                        &name,
                        &base,
                        archive,
                        &mut references,
                        &mut internal_targets,
                    )?;
                    writer
                        .write_event(if empty {
                            Event::Empty(rewritten)
                        } else {
                            Event::Start(rewritten)
                        })
                        .map_err(write_error)?;
                }
                let frame = Frame { name, base, footnote, suppressed_text, suppressed_output };
                if empty {
                    finish_frame(frame, &mut footnotes, budget)?;
                } else {
                    stack.push(frame);
                }
            }
            Event::End(element) => {
                let frame = stack.pop().ok_or_else(|| xml::malformed("orphan XHTML end tag"))?;
                if xml::end_name(&reader, element.name())? != frame.name {
                    return Err(xml::malformed("XHTML end tag namespace mismatch"));
                }
                if !frame.suppressed_output {
                    writer.write_event(Event::End(element.into_owned())).map_err(write_error)?;
                }
                finish_frame(frame, &mut footnotes, budget)?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                let decoded = xml::decoded_text(&event)?.unwrap_or_default();
                if stack.is_empty() && !decoded.chars().all(char::is_whitespace) {
                    return Err(xml::malformed("character data outside XHTML root"));
                }
                let suppressed = stack.last().is_some_and(|frame| frame.suppressed_text);
                if !decoded.is_empty()
                    && let Some(frame) =
                        stack.iter_mut().rev().find(|frame| frame.footnote.is_some())
                    && !suppressed
                    && let Some(footnote) = frame.footnote.as_mut()
                {
                    let next = footnote.text.len().saturating_add(decoded.len());
                    budget.field("footnote", next)?;
                    footnote.text.push_str(&decoded);
                }
                if !stack.last().is_some_and(|frame| frame.suppressed_output) {
                    writer.write_event(event.into_owned()).map_err(write_error)?;
                }
            }
            Event::Eof => break,
            Event::Comment(value) => {
                if !stack.last().is_some_and(|frame| frame.suppressed_output) {
                    writer.write_event(Event::Comment(value.into_owned())).map_err(write_error)?;
                }
            }
            Event::Decl(value)
                if !root_seen && stack.is_empty() && !declaration_seen && !doctype_seen =>
            {
                declaration_seen = true;
                writer.write_event(Event::Decl(value.into_owned())).map_err(write_error)?;
            }
            Event::Decl(_) => return Err(xml::malformed("misplaced or duplicate XML declaration")),
        }
    }
    if !root_seen || !stack.is_empty() {
        return Err(xml::malformed("XHTML root is missing or incomplete"));
    }
    let mut output = writer.into_inner();
    let actual = u64::try_from(output.capacity()).unwrap_or(u64::MAX);
    if actual > charged {
        memory.grow(actual - charged)?;
    } else if charged > actual {
        memory.shrink(charged - actual)?;
    }
    output.shrink_to_fit();
    let shrunk = u64::try_from(output.capacity()).unwrap_or(u64::MAX);
    if actual > shrunk {
        memory.shrink(actual - shrunk)?;
    }
    Ok(PreparedXhtml {
        bytes: output,
        references,
        internal_targets,
        anchors,
        footnotes,
        _memory: memory,
    })
}

fn rewrite_element(
    reader: &quick_xml::reader::NsReader<&[u8]>,
    element: &BytesStart<'_>,
    name: &Name,
    base: &BasePath,
    archive: &SafeArchive<'_, '_>,
    references: &mut BTreeMap<String, String>,
    internal_targets: &mut BTreeSet<String>,
) -> Result<BytesStart<'static>, ConversionError> {
    let element_name = element.name();
    let qname = std::str::from_utf8(element_name.as_ref())
        .map_err(|_| xml::malformed("XHTML element QName is not UTF-8"))?;
    let mut output = BytesStart::new(qname.to_owned());
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| xml::malformed(error.to_string()))?;
        let raw = std::str::from_utf8(attribute.key.as_ref())
            .map_err(|_| xml::malformed("XHTML attribute QName is not UTF-8"))?;
        if raw == "xmlns" || raw.starts_with("xmlns:") {
            let value = attribute
                .decode_and_unescape_value(reader.decoder())
                .map_err(|error| xml::malformed(error.to_string()))?;
            output.push_attribute((raw, value.as_ref()));
            continue;
        }
        let (namespace, local) = reader.resolve_attribute(attribute.key);
        if matches!(namespace, ResolveResult::Unknown(_)) {
            return Err(xml::malformed("unbound XHTML attribute prefix"));
        }
        let mut value = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(|error| xml::malformed(error.to_string()))?
            .into_owned();
        let rewrite = name.namespace.as_deref() == Some(XHTML_NS)
            && (name.local == b"a" && local.as_ref() == b"href"
                || name.local == b"img" && local.as_ref() == b"src")
            && matches!(namespace, ResolveResult::Unbound);
        if rewrite {
            let reference = base.resolve(&value)?.require_existing(archive)?;
            if let Some(synthetic) = reference.synthetic_url()? {
                let canonical = reference.canonical_target();
                if references
                    .insert(synthetic.clone(), canonical.clone())
                    .is_some_and(|old| old != canonical)
                {
                    return Err(xml::malformed("synthetic XHTML reference alias collision"));
                }
                internal_targets.insert(canonical);
                value = synthetic;
            }
        }
        output.push_attribute((raw, value.as_str()));
    }
    Ok(output.into_owned())
}

fn finish_frame(
    frame: Frame,
    footnotes: &mut Vec<Footnote>,
    budget: &EpubBudget<'_>,
) -> Result<(), ConversionError> {
    if let Some(footnote) = frame.footnote {
        let text = footnote.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return Err(xml::malformed("footnote has no visible text"));
        }
        budget.field("footnote", text.len())?;
        footnotes.push(Footnote { target: footnote.target, text });
    }
    Ok(())
}

fn write_error(error: impl std::fmt::Display) -> ConversionError {
    memory_limit(format!("write rewritten XHTML: {error}"))
}

fn memory_limit(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}
