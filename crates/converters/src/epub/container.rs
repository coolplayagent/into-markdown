//! OCF `META-INF/container.xml` parsing.

use super::budget::EpubBudget;
use super::path::{BasePath, Reference};
use super::xml::{self, Name};
use into_markdown_core::ConversionError;
use quick_xml::events::Event;
use std::collections::BTreeSet;

const CONTAINER_NS: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:container";
const PACKAGE_MEDIA_TYPE: &str = "application/oebps-package+xml";

struct Frame {
    name: Name,
    base: BasePath,
}

#[allow(clippy::too_many_lines)] // OCF hierarchy and rootfile order share one streaming state.
pub(super) fn rootfile(
    bytes: &[u8],
    budget: &mut EpubBudget<'_>,
) -> Result<String, ConversionError> {
    let mut reader = xml::reader(bytes);
    let initial_base = BasePath::document("container.xml")?;
    let mut stack = Vec::<Frame>::new();
    let mut root_seen = false;
    let mut rootfiles_seen = false;
    let mut document_events = xml::DocumentEvents::default();
    let mut candidates = Vec::new();
    let mut candidate_paths = BTreeSet::new();
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
                    return Err(xml::malformed("container has multiple root elements"));
                }
                let name = xml::name(&reader, &element)?;
                let attributes = xml::attributes(&reader, &element)?;
                let parent_base = stack.last().map_or(&initial_base, |frame| &frame.base);
                let base = xml::optional(&attributes, Some(xml::XML_NS), b"base")
                    .map_or_else(|| Ok(parent_base.clone()), |value| parent_base.apply(value))?;
                let depth = stack.len() + 1;
                if depth == 1 {
                    if !name.matches(Some(CONTAINER_NS), b"container") {
                        return Err(xml::malformed("expected OCF container root and namespace"));
                    }
                    let version =
                        xml::required(&attributes, None, b"version", "container version")?;
                    if version != "1.0" {
                        return Err(xml::malformed("unsupported OCF container version"));
                    }
                    root_seen = true;
                } else if name.matches(Some(CONTAINER_NS), b"rootfiles") {
                    if depth != 2
                        || !stack.last().is_some_and(|frame| {
                            frame.name.matches(Some(CONTAINER_NS), b"container")
                        })
                        || rootfiles_seen
                    {
                        return Err(xml::malformed("invalid or duplicate rootfiles element"));
                    }
                    rootfiles_seen = true;
                } else if name.matches(Some(CONTAINER_NS), b"rootfile") {
                    if depth != 3
                        || !stack.last().is_some_and(|frame| {
                            frame.name.matches(Some(CONTAINER_NS), b"rootfiles")
                        })
                    {
                        return Err(xml::malformed("rootfile is not inside rootfiles"));
                    }
                    let media = xml::optional(&attributes, None, b"media-type");
                    if media == Some(PACKAGE_MEDIA_TYPE) {
                        let href =
                            xml::required(&attributes, None, b"full-path", "rootfile full-path")?;
                        match base.resolve(href)? {
                            Reference::Internal { path, fragment: None } => {
                                if candidate_paths.insert(path.clone()) {
                                    candidates.push(path);
                                }
                            }
                            _ => {
                                return Err(xml::malformed(
                                    "rootfile must be a fragment-free container path",
                                ));
                            }
                        }
                    }
                } else if name.namespace.as_deref() != Some(CONTAINER_NS) {
                    return Err(xml::malformed("container uses an unexpected element namespace"));
                }
                if !empty {
                    stack.push(Frame { name, base });
                }
            }
            Event::End(element) => {
                let expected = stack.pop().ok_or_else(|| xml::malformed("orphan end tag"))?;
                if xml::end_name(&reader, element.name())? != expected.name {
                    return Err(xml::malformed("end tag namespace mismatch"));
                }
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                let text = xml::decoded_text(&event)?.unwrap_or_default();
                if !text.chars().all(char::is_whitespace) {
                    return Err(xml::malformed("container contains unexpected character data"));
                }
            }
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) => {}
            Event::DocType(_) | Event::PI(_) => unreachable!("rejected above"),
        }
    }
    if !root_seen || !rootfiles_seen || !stack.is_empty() {
        return Err(xml::malformed("container structure is incomplete"));
    }
    match candidates.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(xml::malformed("container has no supported package rootfile")),
        _ => Err(xml::malformed("container has multiple supported package rootfiles")),
    }
}
