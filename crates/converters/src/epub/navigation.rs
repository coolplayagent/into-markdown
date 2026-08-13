//! EPUB 3 navigation document and EPUB 2 NCX parsing.

use super::budget::EpubBudget;
use super::path::{BasePath, Reference};
use super::xml::{self, Name};
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::ConversionError;
use quick_xml::events::Event;
use std::collections::BTreeSet;

const XHTML_NS: &[u8] = b"http://www.w3.org/1999/xhtml";
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
                if name.namespace.as_deref().is_some_and(|namespace| namespace != XHTML_NS) {
                    return Err(xml::malformed("navigation contains a foreign element namespace"));
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
                    let parent = stack
                        .last_mut()
                        .ok_or_else(|| xml::malformed("navigation toc has no parent"))?;
                    let allowed = if parent.name.matches(Some(XHTML_NS), b"nav") {
                        if name.matches(Some(XHTML_NS), b"ol") {
                            parent.nested_lists = parent.nested_lists.saturating_add(1);
                            parent.nested_lists == 1
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
                            parent.label_sources == 1
                        } else if name.matches(Some(XHTML_NS), b"ol") {
                            parent.nested_lists = parent.nested_lists.saturating_add(1);
                            parent.label_sources == 1 && parent.nested_lists == 1
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !allowed {
                        return Err(xml::malformed(
                            "EPUB toc must follow nav/ol/li/(a|span) direct-child grammar",
                        ));
                    }
                }
                let mut list_depth = stack.last().map_or(0, |frame| frame.list_depth);
                if in_toc && name.namespace.as_deref() == Some(XHTML_NS) && name.local == b"ol" {
                    list_depth = list_depth.saturating_add(1);
                }
                let href = if in_toc && name.matches(Some(XHTML_NS), b"a") {
                    let href = xml::required(&attributes, None, b"href", "navigation href")?;
                    Some(base.resolve(href)?.require_existing(archive)?)
                } else {
                    None
                };
                let group_label = in_toc
                    && name.matches(Some(XHTML_NS), b"span")
                    && stack
                        .last()
                        .is_some_and(|parent| parent.name.matches(Some(XHTML_NS), b"li"));
                let suppressed_text = stack.last().is_some_and(|frame| frame.suppressed_text)
                    || name.namespace.as_deref() == Some(XHTML_NS)
                        && matches!(name.local.as_slice(), b"script" | b"style");
                let frame = Frame {
                    name,
                    base,
                    in_toc,
                    list_depth,
                    href,
                    group_label,
                    text: String::new(),
                    suppressed_text,
                    label_sources: 0,
                    nested_lists: 0,
                    list_items: 0,
                    toc_root: declared_toc,
                };
                if empty {
                    finish_nav_frame(frame, &mut entries, budget)?;
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
                finish_nav_frame(frame, &mut entries, budget)?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                let text = xml::decoded_text(&event)?.unwrap_or_default();
                if let Some(frame) = stack.last_mut() {
                    if frame.in_toc && !frame.suppressed_text {
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
    Ok(Navigation { source_path: path.into(), entries })
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
    if frame.href.is_some() || frame.group_label {
        let label = normalize(&frame.text);
        if label.is_empty() {
            return Err(xml::malformed("navigation link has no label"));
        }
        budget.field("navigation label", label.len())?;
        let target = frame.href.map(|reference| reference.canonical_target());
        entries.push(NavEntry { label, target, depth: frame.list_depth.saturating_sub(1) });
    }
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
    Ok(Navigation { source_path: path.into(), entries })
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
