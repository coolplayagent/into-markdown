//! EPUB encryption metadata classification.

use super::budget::EpubBudget;
use super::package::Package;
use super::path::{BasePath, Reference};
use super::xml::{self, Name};
use into_markdown_core::ConversionError;
use quick_xml::events::Event;
use std::collections::BTreeSet;

const CONTAINER_NS: &[u8] = b"urn:oasis:names:tc:opendocument:xmlns:container";
const XMLENC_NS: &[u8] = b"http://www.w3.org/2001/04/xmlenc#";
const IDPF_FONT: &str = "http://www.idpf.org/2008/embedding";
const ADOBE_FONT: &str = "http://ns.adobe.com/pdf/enc#RC";

#[derive(Default)]
pub(super) struct EncryptionPolicy {
    pub(super) unavailable_fonts: BTreeSet<String>,
}

#[derive(Default)]
struct Record {
    algorithm: Option<String>,
    path: Option<String>,
}

struct Frame {
    name: Name,
    base: BasePath,
}

#[allow(clippy::too_many_lines)] // Keep the XML Encryption state machine auditable in one pass.
pub(super) fn parse(
    bytes: &[u8],
    package: &Package,
    budget: &mut EpubBudget<'_>,
) -> Result<EncryptionPolicy, ConversionError> {
    let mut reader = xml::reader(bytes);
    let initial_base = BasePath::document("encryption.xml")?;
    let mut stack = Vec::<Frame>::new();
    let mut records = Vec::<Record>::new();
    let mut current = None::<Record>;
    let mut root_seen = false;
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
                let name = xml::name(&reader, &element)?;
                let attributes = xml::attributes(&reader, &element)?;
                let parent_base = stack.last().map_or(&initial_base, |frame| &frame.base);
                let base = xml::optional(&attributes, Some(xml::XML_NS), b"base")
                    .map_or_else(|| Ok(parent_base.clone()), |value| parent_base.apply(value))?;
                if stack.is_empty() {
                    if root_seen || !name.matches(Some(CONTAINER_NS), b"encryption") {
                        return Err(xml::malformed("expected one OCF encryption root"));
                    }
                    root_seen = true;
                } else if name.matches(Some(XMLENC_NS), b"EncryptedData") {
                    if current.replace(Record::default()).is_some() {
                        return Err(xml::malformed("nested EncryptedData is forbidden"));
                    }
                } else if name.matches(Some(XMLENC_NS), b"EncryptionMethod") {
                    let record = current.as_mut().ok_or_else(|| {
                        xml::malformed("EncryptionMethod is outside EncryptedData")
                    })?;
                    let algorithm = xml::required(
                        &attributes,
                        None,
                        b"Algorithm",
                        "EncryptionMethod Algorithm",
                    )?;
                    if record.algorithm.replace(algorithm.to_owned()).is_some() {
                        return Err(xml::malformed("duplicate EncryptionMethod"));
                    }
                } else if name.matches(Some(XMLENC_NS), b"CipherReference") {
                    let record = current.as_mut().ok_or_else(|| {
                        xml::malformed("CipherReference is outside EncryptedData")
                    })?;
                    let uri = xml::required(&attributes, None, b"URI", "CipherReference URI")?;
                    let Reference::Internal { path, fragment: None } = base.resolve(uri)? else {
                        return Err(xml::malformed(
                            "CipherReference must be a fragment-free container path",
                        ));
                    };
                    if record.path.replace(path).is_some() {
                        return Err(xml::malformed("duplicate CipherReference"));
                    }
                }
                if !empty {
                    stack.push(Frame { name, base });
                } else if name.matches(Some(XMLENC_NS), b"EncryptedData") {
                    records.push(current.take().unwrap_or_default());
                }
            }
            Event::End(element) => {
                let frame =
                    stack.pop().ok_or_else(|| xml::malformed("orphan encryption end tag"))?;
                if xml::end_name(&reader, element.name())? != frame.name {
                    return Err(xml::malformed("encryption end tag namespace mismatch"));
                }
                if frame.name.matches(Some(XMLENC_NS), b"EncryptedData") {
                    records.push(current.take().ok_or_else(|| {
                        xml::malformed("EncryptedData classifier state is missing")
                    })?);
                }
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                let text = xml::decoded_text(&event)?.unwrap_or_default();
                if !text.chars().all(char::is_whitespace) {
                    return Err(xml::malformed("unexpected encryption character data"));
                }
            }
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) => {}
            Event::DocType(_) | Event::PI(_) => unreachable!("rejected above"),
        }
    }
    if !root_seen || !stack.is_empty() || current.is_some() {
        return Err(xml::malformed("encryption structure is incomplete"));
    }
    let mut policy = EncryptionPolicy::default();
    for record in records {
        let algorithm =
            record.algorithm.ok_or_else(|| xml::malformed("encryption algorithm missing"))?;
        let path = record.path.ok_or_else(|| xml::malformed("encrypted resource path missing"))?;
        let item = package
            .manifest
            .values()
            .find(|item| item.path == path)
            .ok_or_else(|| xml::malformed("encrypted resource is absent from the manifest"))?;
        if matches!(algorithm.as_str(), IDPF_FONT | ADOBE_FONT) && is_font(&item.media_type) {
            policy.unavailable_fonts.insert(path);
        } else {
            return Err(ConversionError::Encrypted);
        }
    }
    Ok(policy)
}

fn is_font(media_type: &str) -> bool {
    matches!(
        media_type,
        "font/ttf"
            | "font/otf"
            | "font/woff"
            | "font/woff2"
            | "application/font-sfnt"
            | "application/font-woff"
            | "application/vnd.ms-opentype"
    )
}
