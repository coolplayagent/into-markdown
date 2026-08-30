//! EPUB 3 navigation document and EPUB 2 NCX parsing.

mod label_policy;

use super::budget::EpubBudget;
use super::path::{BasePath, Reference};
use super::xml::{self, Name};
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::ConversionError;
use quick_xml::events::Event;
use std::collections::BTreeSet;

use label_policy::XHTML_NS;

const EPUB_NS: &[u8] = b"http://www.idpf.org/2007/ops";
const NCX_NS: &[u8] = b"http://www.daisy.org/z3986/2005/ncx/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NavEntry {
    pub(super) label: String,
    pub(super) target: Option<String>,
    pub(super) depth: usize,
}

#[derive(Default)]
pub(super) struct Navigation {
    pub(super) source_path: String,
    pub(super) entries: Vec<NavEntry>,
    pub(super) resource_paths: BTreeSet<String>,
}

#[allow(clippy::struct_excessive_bools)] // Streaming XHTML context is explicit and non-recursive.
struct Frame {
    name: Name,
    base: BasePath,
    in_toc: bool,
    list_depth: usize,
    href: Option<Reference>,
    group_label: bool,
    text: String,
    suppressed_text: bool,
    label_sources: u8,
    nested_lists: u8,
    list_items: usize,
    toc_root: bool,
    headings: u8,
    span_label: bool,
    label_fallback: Option<String>,
    embedded: bool,
    authoritative_replacement: bool,
    missing_alternative: bool,
    child_elements: usize,
}

#[allow(clippy::too_many_lines)] // Navigation XML is validated in one streaming state machine.
pub(super) fn parse_nav(
    path: &str,
    bytes: &[u8],
    archive: &SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
) -> Result<Navigation, ConversionError> {
    let mut reader = xml::reader(bytes);
    let initial_base = BasePath::document(path)?;
    let mut stack = Vec::<Frame>::new();
    let mut root_seen = false;
    let mut toc_seen = false;
    let mut entries = Vec::new();
    let mut resource_paths = BTreeSet::new();
    let mut document_events = xml::DocumentEvents::default();
    loop {
        let event = reader.read_event().map_err(|error| xml::malformed(error.to_string()))?;
        let empty = matches!(&event, Event::Empty(_));
        let depth = stack
            .len()
            .saturating_add(usize::from(matches!(&event, Event::Start(_) | Event::Empty(_))));
        budget.event(depth)?;
        document_events.validate(&event, root_seen, !stack.is_empty(), xml::DoctypePolicy::Html)?;
        match event {
            Event::PI(_) => unreachable!("rejected above"),
            Event::Start(element) | Event::Empty(element) => {
                if stack.is_empty() && root_seen {
                    return Err(xml::malformed("navigation has multiple root elements"));
                }
                let name = xml::name(&reader, &element)?;
                let attributes = xml::attributes(&reader, &element)?;
                let parent_base = stack.last().map_or(&initial_base, |frame| &frame.base);
                let base = xml::optional(&attributes, Some(xml::XML_NS), b"base")
                    .map_or_else(|| Ok(parent_base.clone()), |value| parent_base.apply(value))?;
                if stack.is_empty() {
                    if !name.matches(Some(XHTML_NS), b"html") {
                        return Err(xml::malformed("navigation root is not XHTML html"));
                    }
                    root_seen = true;
                }
                if !label_policy::is_known_namespace(name.namespace.as_deref()) {
                    return Err(xml::malformed("navigation contains an unsupported namespace"));
                }
                if name.matches(Some(XHTML_NS), b"link")
                    && let Some(href) = xml::optional(&attributes, None, b"href")
                    && let Reference::Internal { path, .. } =
                        base.resolve(href)?.require_existing(archive)?
                {
                    resource_paths.insert(path);
                }
                let parent_in_toc = stack.last().is_some_and(|frame| frame.in_toc);
                let declared_toc = name.matches(Some(XHTML_NS), b"nav")
                    && xml::optional(&attributes, Some(EPUB_NS), b"type")
                        .is_some_and(|value| value.split_whitespace().any(|token| token == "toc"));
                if declared_toc {
                    if toc_seen {
                        return Err(xml::malformed("multiple EPUB toc navigation elements"));
                    }
                    toc_seen = true;
                }
                let in_toc = parent_in_toc || declared_toc;
                if in_toc && !declared_toc {
                    let nested_anchor = name.matches(Some(XHTML_NS), b"a")
                        && stack.iter().any(|frame| frame.name.matches(Some(XHTML_NS), b"a"));
                    let parent = stack
                        .last_mut()
                        .ok_or_else(|| xml::malformed("navigation toc has no parent"))?;
                    let allowed = if parent.name.matches(Some(XHTML_NS), b"nav") {
                        if name.matches(Some(XHTML_NS), b"ol") {
                            parent.nested_lists = parent.nested_lists.saturating_add(1);
                            parent.nested_lists == 1
                        } else if label_policy::is_heading(&name) {
                            parent.headings = parent.headings.saturating_add(1);
                            parent.headings == 1 && parent.nested_lists == 0
                        } else {
                            false
                        }
                    } else if parent.name.matches(Some(XHTML_NS), b"ol") {
                        if name.matches(Some(XHTML_NS), b"li") {
                            parent.list_items = parent.list_items.saturating_add(1);
                            true
                        } else {
                            false
                        }
                    } else if parent.name.matches(Some(XHTML_NS), b"li") {
                        if name.matches(Some(XHTML_NS), b"a")
                            || name.matches(Some(XHTML_NS), b"span")
                        {
                            parent.label_sources = parent.label_sources.saturating_add(1);
                            parent.span_label = name.matches(Some(XHTML_NS), b"span");
                            parent.label_sources == 1
                        } else if name.matches(Some(XHTML_NS), b"ol") {
                            parent.nested_lists = parent.nested_lists.saturating_add(1);
                            parent.label_sources == 1 && parent.nested_lists == 1
                        } else {
                            false
                        }
                    } else if label_policy::is_label_content(&parent.name) {
                        let allowed =
                            !nested_anchor && label_policy::is_child_allowed(&parent.name, &name);
                        if allowed {
                            parent.child_elements = parent.child_elements.saturating_add(1);
                        }
                        allowed
                    } else {
                        false
                    };
                    if !allowed {
                        return Err(xml::malformed(
                            "EPUB toc must follow nav/ol/li/(a|span) direct-child grammar",
                        ));
                    }
                    if label_policy::is_label_content(&name) {
                        label_policy::validate_element(&name, &attributes, &base, budget)?;
                    }
                }
                let mut list_depth = stack.last().map_or(0, |frame| frame.list_depth);
                if in_toc && name.namespace.as_deref() == Some(XHTML_NS) && name.local == b"ol" {
                    list_depth = list_depth.saturating_add(1);
                }
                let direct_label = in_toc
                    && stack.last().is_some_and(|parent| {
                        parent.name.matches(Some(XHTML_NS), b"li")
                            && label_policy::is_label_container(&name)
                    });
                let href = if direct_label && name.matches(Some(XHTML_NS), b"a") {
                    let href = xml::required(&attributes, None, b"href", "navigation href")?;
                    let reference = base.resolve(href)?;
                    if matches!(reference, Reference::External(_)) {
                        return Err(xml::malformed(
                            "EPUB toc links must remain inside the EPUB container",
                        ));
                    }
                    Some(reference.require_existing(archive)?)
                } else {
                    if in_toc
                        && name.matches(Some(XHTML_NS), b"a")
                        && let Some(value) = xml::optional(&attributes, None, b"href")
                    {
                        base.resolve(value)?;
                    }
                    None
                };
                let group_label = direct_label && name.matches(Some(XHTML_NS), b"span");
                let suppressed_text = stack.last().is_some_and(|frame| frame.suppressed_text);
                let mut text = String::new();
                let alternative =
                    in_toc.then(|| label_policy::replacement_text(&name, &attributes)).flatten();
                if let Some(alternative) = alternative {
                    append_text(&mut text, alternative, budget)?;
                }
                let label_fallback = if direct_label {
                    xml::optional(&attributes, None, b"title")
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_owned)
                } else {
                    None
                };
                if let Some(value) = &label_fallback {
                    budget.field("navigation label", value.len())?;
                }
                let embedded = label_policy::is_embedded(&name);
                let frame = Frame {
                    name,
                    base,
                    in_toc,
                    list_depth,
                    href,
                    group_label,
                    text,
                    suppressed_text,
                    label_sources: 0,
                    nested_lists: 0,
                    list_items: 0,
                    toc_root: declared_toc,
                    headings: 0,
                    span_label: false,
                    label_fallback,
                    embedded,
                    authoritative_replacement: alternative.is_some(),
                    missing_alternative: false,
                    child_elements: 0,
                };
                if empty {
                    if in_toc
                        && matches!(frame.name.local.as_slice(), b"br" | b"wbr")
                        && let Some(parent) = stack.last_mut()
                        && label_policy::is_label_content(&parent.name)
                    {
                        append_label_text(parent, " ", budget)?;
                    }
                    close_nav_frame(frame, &mut stack, &mut entries, budget)?;
                } else {
                    stack.push(frame);
                }
            }
            Event::End(element) => {
                let frame =
                    stack.pop().ok_or_else(|| xml::malformed("orphan navigation end tag"))?;
                if xml::end_name(&reader, element.name())? != frame.name {
                    return Err(xml::malformed("navigation end tag namespace mismatch"));
                }
                close_nav_frame(frame, &mut stack, &mut entries, budget)?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                let text = xml::decoded_text(&event)?.unwrap_or_default();
                if let Some(frame) = stack.last_mut() {
                    if frame.in_toc && !frame.suppressed_text && !frame.authoritative_replacement {
                        if frame.name.matches(Some(XHTML_NS), b"nav")
                            || frame.name.matches(Some(XHTML_NS), b"ol")
                            || frame.name.matches(Some(XHTML_NS), b"li")
                        {
                            if !text.chars().all(char::is_whitespace) {
                                return Err(xml::malformed(
                                    "EPUB toc structural elements contain direct text",
                                ));
                            }
                            continue;
                        }
                        let next = frame.text.len().saturating_add(text.len());
                        budget.field("navigation label", next)?;
                        frame.text.push_str(&text);
                    }
                } else if !text.chars().all(char::is_whitespace) {
                    return Err(xml::malformed("character data outside navigation root"));
                }
            }
            Event::Eof => break,
            Event::DocType(_) | Event::Comment(_) | Event::Decl(_) => {}
        }
    }
    if !root_seen || !toc_seen || !stack.is_empty() || entries.is_empty() {
        return Err(xml::malformed("EPUB navigation toc is missing or incomplete"));
    }
    budget.items("navigation", entries.len())?;
    Ok(Navigation { source_path: path.into(), entries, resource_paths })
}

fn close_nav_frame(
    mut frame: Frame,
    stack: &mut [Frame],
    entries: &mut Vec<NavEntry>,
    budget: &EpubBudget<'_>,
) -> Result<(), ConversionError> {
    if frame.in_toc && label_policy::is_label_content(&frame.name) {
        label_policy::validate_child_count(&frame.name, frame.child_elements)?;
    }
    let missing_alternative = frame.missing_alternative
        || frame.embedded && !frame.authoritative_replacement && normalize(&frame.text).is_empty();
    if frame.in_toc
        && label_policy::is_label_content(&frame.name)
        && frame.href.is_none()
        && !frame.group_label
        && let Some(parent) = stack.last_mut()
        && label_policy::is_label_content(&parent.name)
        && !parent.authoritative_replacement
    {
        append_label_text(parent, &frame.text, budget)?;
        parent.missing_alternative |= missing_alternative;
    }
    frame.missing_alternative = missing_alternative;
    finish_nav_frame(frame, entries, budget)
}

fn finish_nav_frame(
    frame: Frame,
    entries: &mut Vec<NavEntry>,
    budget: &EpubBudget<'_>,
) -> Result<(), ConversionError> {
    if frame.toc_root && frame.nested_lists != 1 {
        return Err(xml::malformed("EPUB toc nav must contain exactly one direct ol"));
    }
    if frame.in_toc && frame.name.matches(Some(XHTML_NS), b"ol") && frame.list_items == 0 {
        return Err(xml::malformed("EPUB toc ol must contain at least one direct li"));
    }
    if frame.in_toc && frame.name.matches(Some(XHTML_NS), b"li") && frame.label_sources != 1 {
        return Err(xml::malformed("EPUB toc li must contain exactly one label source"));
    }
    if frame.in_toc
        && frame.name.matches(Some(XHTML_NS), b"li")
        && frame.span_label
        && frame.nested_lists != 1
    {
        return Err(xml::malformed("EPUB toc span labels must introduce a nested ol"));
    }
    if frame.href.is_some() || frame.group_label {
        if frame.missing_alternative && frame.label_fallback.is_none() {
            return Err(xml::malformed(
                "navigation label embedded content has no text alternative",
            ));
        }
        let label = if frame.missing_alternative {
            normalize(frame.label_fallback.as_deref().unwrap_or_default())
        } else {
            let text = normalize(&frame.text);
            if text.is_empty() {
                normalize(frame.label_fallback.as_deref().unwrap_or_default())
            } else {
                text
            }
        };
        if label.is_empty() {
            return Err(xml::malformed("navigation link has no label"));
        }
        budget.field("navigation label", label.len())?;
        let target = frame.href.map(|reference| reference.canonical_target());
        entries.push(NavEntry { label, target, depth: frame.list_depth.saturating_sub(1) });
    }
    Ok(())
}

fn append_label_text(
    frame: &mut Frame,
    text: &str,
    budget: &EpubBudget<'_>,
) -> Result<(), ConversionError> {
    append_text(&mut frame.text, text, budget)
}

fn append_text(
    output: &mut String,
    text: &str,
    budget: &EpubBudget<'_>,
) -> Result<(), ConversionError> {
    let next = output.len().saturating_add(text.len());
    budget.field("navigation label", next)?;
    output.push_str(text);
    Ok(())
}

struct NcxFrame {
    name: Name,
    base: BasePath,
}

#[derive(Default)]
struct NcxPending {
    label: String,
    target: Option<String>,
}

#[allow(clippy::too_many_lines)] // NCX ordering and identity checks share one streaming pass.
pub(super) fn parse_ncx(
    path: &str,
    bytes: &[u8],
    archive: &SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
) -> Result<Navigation, ConversionError> {
    let mut reader = xml::reader(bytes);
    let initial_base = BasePath::document(path)?;
    let mut stack = Vec::<NcxFrame>::new();
    let mut points = Vec::<usize>::new();
    let mut pending = Vec::<NcxPending>::new();
    let mut entries = Vec::<NavEntry>::new();
    let mut ids = BTreeSet::new();
    let mut play_orders = BTreeSet::new();
    let mut root_seen = false;
    let mut nav_map_seen = false;
    let mut text_depth = None::<usize>;
    let mut document_events = xml::DocumentEvents::default();
    loop {
        let event = reader.read_event().map_err(|error| xml::malformed(error.to_string()))?;
        let empty = matches!(&event, Event::Empty(_));
        let depth = stack
            .len()
            .saturating_add(usize::from(matches!(&event, Event::Start(_) | Event::Empty(_))));
        budget.event(depth)?;
        document_events.validate(
            &event,
            root_seen,
            !stack.is_empty(),
            xml::DoctypePolicy::Forbidden,
        )?;
        match event {
            Event::Start(element) | Event::Empty(element) => {
                if stack.is_empty() && root_seen {
                    return Err(xml::malformed("NCX has multiple root elements"));
                }
                let name = xml::name(&reader, &element)?;
                let attributes = xml::attributes(&reader, &element)?;
                let parent_base = stack.last().map_or(&initial_base, |frame| &frame.base);
                let base = xml::optional(&attributes, Some(xml::XML_NS), b"base")
                    .map_or_else(|| Ok(parent_base.clone()), |value| parent_base.apply(value))?;
                if stack.is_empty() {
                    if !name.matches(Some(NCX_NS), b"ncx") {
                        return Err(xml::malformed("NCX root or namespace is invalid"));
                    }
                    root_seen = true;
                } else if name.matches(Some(NCX_NS), b"navMap") {
                    if nav_map_seen {
                        return Err(xml::malformed("duplicate NCX navMap"));
                    }
                    if stack.len() != 1 {
                        return Err(xml::malformed("NCX navMap is not a direct ncx child"));
                    }
                    nav_map_seen = true;
                } else if name.matches(Some(NCX_NS), b"navPoint") {
                    let inside_nav_map =
                        stack.iter().any(|frame| frame.name.matches(Some(NCX_NS), b"navMap"));
                    if !inside_nav_map {
                        return Err(xml::malformed("NCX navPoint is outside navMap"));
                    }
                    let id = xml::required(&attributes, None, b"id", "NCX navPoint id")?;
                    if !xml::valid_ncname(id) || !ids.insert(id.to_owned()) {
                        return Err(xml::malformed("duplicate NCX navPoint ID"));
                    }
                    if let Some(order) = xml::optional(&attributes, None, b"playOrder") {
                        let order = order
                            .parse::<u64>()
                            .map_err(|_| xml::malformed("invalid NCX playOrder"))?;
                        if order == 0 || !play_orders.insert(order) {
                            return Err(xml::malformed("duplicate or zero NCX playOrder"));
                        }
                    }
                    let index = pending.len();
                    pending.push(NcxPending::default());
                    entries.push(NavEntry {
                        label: String::new(),
                        target: None,
                        depth: points.len(),
                    });
                    points.push(index);
                } else if name.matches(Some(NCX_NS), b"content") {
                    if let Some(index) = points.last().copied() {
                        let src = xml::required(&attributes, None, b"src", "NCX content src")?;
                        let target =
                            base.resolve(src)?.require_existing(archive)?.canonical_target();
                        if pending[index].target.replace(target).is_some() {
                            return Err(xml::malformed("duplicate NCX content in navPoint"));
                        }
                    }
                } else if name.matches(Some(NCX_NS), b"text")
                    && !empty
                    && !points.is_empty()
                    && stack.iter().rev().any(|frame| frame.name.matches(Some(NCX_NS), b"navLabel"))
                {
                    text_depth = Some(stack.len() + 1);
                }
                if !empty {
                    stack.push(NcxFrame { name, base });
                }
            }
            Event::End(element) => {
                let frame = stack.pop().ok_or_else(|| xml::malformed("orphan NCX end tag"))?;
                if xml::end_name(&reader, element.name())? != frame.name {
                    return Err(xml::malformed("NCX end tag namespace mismatch"));
                }
                if frame.name.matches(Some(NCX_NS), b"text") {
                    text_depth = None;
                }
                if frame.name.matches(Some(NCX_NS), b"navPoint") {
                    points.pop().ok_or_else(|| xml::malformed("NCX navPoint stack underflow"))?;
                }
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                let text = xml::decoded_text(&event)?.unwrap_or_default();
                if text_depth.is_some() {
                    let index = *points
                        .last()
                        .ok_or_else(|| xml::malformed("NCX label outside navPoint"))?;
                    let next = pending[index].label.len().saturating_add(text.len());
                    budget.field("NCX label", next)?;
                    pending[index].label.push_str(&text);
                } else if stack.is_empty() && !text.chars().all(char::is_whitespace) {
                    return Err(xml::malformed("character data outside NCX root"));
                }
            }
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) => {}
            Event::DocType(_) | Event::PI(_) => unreachable!("rejected above"),
        }
    }
    if !root_seen || !nav_map_seen || !stack.is_empty() || !points.is_empty() || pending.is_empty()
    {
        return Err(xml::malformed("NCX navigation is missing or incomplete"));
    }
    for (entry, value) in entries.iter_mut().zip(pending) {
        entry.label = normalize(&value.label);
        entry.target =
            Some(value.target.ok_or_else(|| xml::malformed("NCX navPoint has no content"))?);
        if entry.label.is_empty() {
            return Err(xml::malformed("NCX label is empty"));
        }
    }
    budget.items("NCX navigation", entries.len())?;
    Ok(Navigation { source_path: path.into(), entries, resource_paths: BTreeSet::new() })
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
