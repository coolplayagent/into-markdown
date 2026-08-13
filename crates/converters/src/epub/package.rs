//! OPF 2/3 package parsing and reference validation.

use super::budget::EpubBudget;
use super::path::{BasePath, Reference};
use super::xml::{self, Attribute, Name};
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::{ConversionError, DocumentMetadata};
use quick_xml::events::Event;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const OPF_NS: &[u8] = b"http://www.idpf.org/2007/opf";
const DC_NS: &[u8] = b"http://purl.org/dc/elements/1.1/";

#[derive(Clone, Debug)]
pub(super) struct ManifestItem {
    pub(super) id: String,
    pub(super) path: String,
    pub(super) media_type: String,
    pub(super) properties: BTreeSet<String>,
    pub(super) fallback: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct SpineItem {
    pub(super) idref: String,
    pub(super) linear: bool,
}

#[derive(Debug)]
pub(super) struct Package {
    pub(super) path: String,
    pub(super) metadata: DocumentMetadata,
    pub(super) manifest: BTreeMap<String, ManifestItem>,
    pub(super) spine: Vec<SpineItem>,
    pub(super) nav_id: Option<String>,
    pub(super) ncx_id: Option<String>,
    pub(super) cover_id: Option<String>,
}

impl Package {
    pub(super) fn item(&self, id: &str) -> Result<&ManifestItem, ConversionError> {
        self.manifest.get(id).ok_or_else(|| xml::malformed(format!("missing manifest ID {id:?}")))
    }

    pub(super) fn fallback_chain(&self, id: &str) -> Result<Vec<&ManifestItem>, ConversionError> {
        let mut output = Vec::new();
        let mut current = Some(id);
        while let Some(id) = current {
            let item = self.item(id)?;
            output.push(item);
            current = item.fallback.as_deref();
        }
        Ok(output)
    }
}

struct Frame {
    name: Name,
    base: BasePath,
    id: Option<String>,
    text: String,
    meta_name: Option<String>,
    meta_content: Option<String>,
    meta_property: Option<String>,
    meta_refines: Option<String>,
}

#[allow(clippy::too_many_lines)] // OPF section and reference invariants are checked in one pass.
pub(super) fn parse(
    package_path: &str,
    bytes: &[u8],
    archive: &SafeArchive<'_, '_>,
    budget: &mut EpubBudget<'_>,
) -> Result<Package, ConversionError> {
    let mut reader = xml::reader(bytes);
    let initial_base = BasePath::document(package_path)?;
    let mut stack = Vec::<Frame>::new();
    let mut root_seen = false;
    let mut sections = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut manifest_paths = BTreeSet::new();
    let mut manifest = BTreeMap::new();
    let mut spine = Vec::new();
    let mut spine_ids = BTreeSet::new();
    let mut metadata = DocumentMetadata::default();
    let mut identifiers = BTreeMap::<String, String>::new();
    let mut unique_identifier = None;
    let mut version = None;
    let mut nav_id = None;
    let mut ncx_id = None;
    let mut cover_id = None;
    let mut modified = None;
    loop {
        let event = reader.read_event().map_err(|error| xml::malformed(error.to_string()))?;
        let empty = matches!(&event, Event::Empty(_));
        let depth = stack
            .len()
            .saturating_add(usize::from(matches!(&event, Event::Start(_) | Event::Empty(_))));
        budget.event(depth)?;
        xml::reject_active(&event)?;
        match event {
            Event::Start(element) | Event::Empty(element) => {
                if stack.is_empty() && root_seen {
                    return Err(xml::malformed("package has multiple root elements"));
                }
                let name = xml::name(&reader, &element)?;
                let attributes = xml::attributes(&reader, &element)?;
                let parent_base = stack.last().map_or(&initial_base, |frame| &frame.base);
                let base = xml::optional(&attributes, Some(xml::XML_NS), b"base")
                    .map_or_else(|| Ok(parent_base.clone()), |value| parent_base.apply(value))?;
                let id = xml::optional(&attributes, None, b"id").map(str::to_owned);
                if let Some(id) = &id
                    && (!valid_id(id) || !ids.insert(id.clone()))
                {
                    return Err(xml::malformed("package contains an invalid or duplicate ID"));
                }
                let depth = stack.len() + 1;
                if depth == 1 {
                    if !name.matches(Some(OPF_NS), b"package") {
                        return Err(xml::malformed("expected OPF package root and namespace"));
                    }
                    let parsed_version =
                        xml::required(&attributes, None, b"version", "package version")?;
                    if !matches!(parsed_version, "2.0" | "3.0" | "3.1" | "3.2" | "3.3") {
                        return Err(xml::malformed("unsupported OPF package version"));
                    }
                    version = Some(parsed_version.to_owned());
                    let identifier = xml::required(
                        &attributes,
                        None,
                        b"unique-identifier",
                        "package unique-identifier",
                    )?;
                    if !xml::valid_ncname(identifier) {
                        return Err(xml::malformed("package unique-identifier is not an NCName"));
                    }
                    unique_identifier = Some(identifier.to_owned());
                    root_seen = true;
                } else if depth == 2 && name.namespace.as_deref() == Some(OPF_NS) {
                    match name.local.as_slice() {
                        b"metadata" | b"manifest" | b"spine" => {
                            if !sections.insert(name.local.clone()) {
                                return Err(xml::malformed("duplicate OPF package section"));
                            }
                            if name.local == b"spine" {
                                ncx_id =
                                    xml::optional(&attributes, None, b"toc").map(str::to_owned);
                            }
                        }
                        b"guide" | b"bindings" | b"collection" => {}
                        _ => return Err(xml::malformed("unexpected OPF package section")),
                    }
                } else if name.matches(Some(OPF_NS), b"item") {
                    require_parent(&stack, b"manifest", "manifest item")?;
                    let item = manifest_item(&attributes, &base, archive)?;
                    if !manifest_paths.insert(item.path.clone()) {
                        return Err(xml::malformed("multiple manifest items use the same path"));
                    }
                    if item.properties.contains("nav") && nav_id.replace(item.id.clone()).is_some()
                    {
                        return Err(xml::malformed("multiple EPUB navigation items"));
                    }
                    if item.properties.contains("cover-image")
                        && cover_id.replace(item.id.clone()).is_some()
                    {
                        return Err(xml::malformed("multiple EPUB cover-image items"));
                    }
                    if manifest.insert(item.id.clone(), item).is_some() {
                        return Err(xml::malformed("duplicate manifest item ID"));
                    }
                } else if name.matches(Some(OPF_NS), b"itemref") {
                    require_parent(&stack, b"spine", "spine itemref")?;
                    let idref = xml::required(&attributes, None, b"idref", "spine itemref idref")?
                        .to_owned();
                    if !spine_ids.insert(idref.clone()) {
                        return Err(xml::malformed("duplicate manifest reference in spine"));
                    }
                    let linear = match xml::optional(&attributes, None, b"linear") {
                        None | Some("yes") => true,
                        Some("no") => false,
                        Some(_) => return Err(xml::malformed("spine linear must be yes or no")),
                    };
                    spine.push(SpineItem { idref, linear });
                }
                if name.namespace.as_deref() == Some(DC_NS) || name.matches(Some(OPF_NS), b"meta") {
                    require_parent(&stack, b"metadata", "package metadata field")?;
                }
                let frame = Frame {
                    name,
                    base,
                    id,
                    text: String::new(),
                    meta_name: xml::optional(&attributes, None, b"name").map(str::to_owned),
                    meta_content: xml::optional(&attributes, None, b"content").map(str::to_owned),
                    meta_property: xml::optional(&attributes, None, b"property").map(str::to_owned),
                    meta_refines: xml::optional(&attributes, None, b"refines").map(str::to_owned),
                };
                if empty {
                    finish_frame(
                        frame,
                        &mut metadata,
                        &mut identifiers,
                        &mut cover_id,
                        &mut modified,
                        budget,
                    )?;
                } else {
                    stack.push(frame);
                }
            }
            Event::End(element) => {
                let frame = stack.pop().ok_or_else(|| xml::malformed("orphan package end tag"))?;
                if xml::end_name(&reader, element.name())? != frame.name {
                    return Err(xml::malformed("package end tag namespace mismatch"));
                }
                finish_frame(
                    frame,
                    &mut metadata,
                    &mut identifiers,
                    &mut cover_id,
                    &mut modified,
                    budget,
                )?;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => {
                let text = xml::decoded_text(&event)?.unwrap_or_default();
                if let Some(frame) = stack.last_mut() {
                    let next = frame.text.len().saturating_add(text.len());
                    budget.field("metadata field", next)?;
                    frame.text.push_str(&text);
                } else if !text.chars().all(char::is_whitespace) {
                    return Err(xml::malformed("character data outside package root"));
                }
            }
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) => {}
            Event::DocType(_) | Event::PI(_) => unreachable!("rejected above"),
        }
    }
    if !root_seen
        || !stack.is_empty()
        || !sections.contains(b"metadata".as_slice())
        || !sections.contains(b"manifest".as_slice())
        || !sections.contains(b"spine".as_slice())
    {
        return Err(xml::malformed("OPF package structure is incomplete"));
    }
    budget.items("manifest", manifest.len())?;
    budget.items("spine", spine.len())?;
    if manifest.is_empty() || spine.is_empty() {
        return Err(xml::malformed("manifest and spine must be non-empty"));
    }
    let unique_identifier =
        unique_identifier.ok_or_else(|| xml::malformed("unique identifier missing"))?;
    let identifier =
        identifiers.get(&unique_identifier).filter(|value| !value.is_empty()).ok_or_else(|| {
            xml::malformed("package unique-identifier does not name a non-empty dc:identifier")
        })?;
    if metadata.title.as_deref().is_none_or(str::is_empty) {
        return Err(xml::malformed("package dc:title is missing"));
    }
    if metadata.properties.get("epub.language").is_none_or(String::is_empty) {
        return Err(xml::malformed("package dc:language is missing"));
    }
    let version = version.ok_or_else(|| xml::malformed("package version missing"))?;
    if version.starts_with('3') {
        let modified = modified
            .as_deref()
            .filter(|value| valid_modified(value))
            .ok_or_else(|| xml::malformed("EPUB 3 dcterms:modified is missing or invalid"))?;
        metadata.properties.insert("epub.meta.dcterms:modified".into(), modified.into());
    }
    metadata.properties.insert("epub.identifier".into(), identifier.clone());
    metadata.properties.insert("epub.version".into(), version);
    validate_references(
        &manifest,
        &spine,
        nav_id.as_deref(),
        ncx_id.as_deref(),
        cover_id.as_deref(),
    )?;
    Ok(Package { path: package_path.into(), metadata, manifest, spine, nav_id, ncx_id, cover_id })
}

fn manifest_item(
    attributes: &[Attribute],
    base: &BasePath,
    archive: &SafeArchive<'_, '_>,
) -> Result<ManifestItem, ConversionError> {
    let id = xml::required(attributes, None, b"id", "manifest item id")?.to_owned();
    let href = xml::required(attributes, None, b"href", "manifest item href")?;
    let Reference::Internal { path, fragment: None } =
        base.resolve(href)?.require_existing(archive)?
    else {
        return Err(xml::malformed("manifest href must be a fragment-free container path"));
    };
    let media_type =
        xml::required(attributes, None, b"media-type", "manifest media-type")?.to_ascii_lowercase();
    if !valid_media_type(&media_type) {
        return Err(xml::malformed("manifest contains an invalid media-type"));
    }
    let properties = xml::optional(attributes, None, b"properties")
        .map_or_else(BTreeSet::new, |value| value.split_whitespace().map(str::to_owned).collect());
    let fallback = xml::optional(attributes, None, b"fallback").map(str::to_owned);
    Ok(ManifestItem { id, path, media_type, properties, fallback })
}

fn finish_frame(
    frame: Frame,
    metadata: &mut DocumentMetadata,
    identifiers: &mut BTreeMap<String, String>,
    cover_id: &mut Option<String>,
    modified: &mut Option<String>,
    budget: &EpubBudget<'_>,
) -> Result<(), ConversionError> {
    let text = normalize(&frame.text);
    budget.field("metadata field", text.len())?;
    if frame.name.namespace.as_deref() == Some(DC_NS) {
        match frame.name.local.as_slice() {
            b"title" if metadata.title.is_none() && !text.is_empty() => metadata.title = Some(text),
            b"creator" if !text.is_empty() => metadata.authors.push(text),
            b"identifier" if !text.is_empty() => {
                if let Some(id) = frame.id {
                    identifiers.insert(id, text);
                }
            }
            b"language" if !text.is_empty() => {
                metadata.properties.entry("epub.language".into()).or_insert(text);
            }
            b"publisher" | b"date" | b"description" | b"subject" if !text.is_empty() => {
                let key = format!("epub.dc.{}", String::from_utf8_lossy(&frame.name.local));
                metadata.properties.entry(key).or_insert(text);
            }
            _ => {}
        }
    } else if frame.name.matches(Some(OPF_NS), b"meta") {
        if frame.meta_name.as_deref() == Some("cover") {
            let value = frame
                .meta_content
                .filter(|value| !value.is_empty())
                .ok_or_else(|| xml::malformed("EPUB 2 cover meta is missing its manifest ID"))?;
            if cover_id.replace(value).is_some() {
                return Err(xml::malformed("multiple EPUB 2 cover metadata entries"));
            }
        } else if let Some(property) = frame.meta_property
            && !text.is_empty()
        {
            if property == "dcterms:modified" && frame.meta_refines.is_none() {
                if modified.replace(text.clone()).is_some() {
                    return Err(xml::malformed("multiple EPUB package modification dates"));
                }
            } else {
                metadata.properties.entry(format!("epub.meta.{property}")).or_insert(text);
            }
        }
    }
    Ok(())
}

fn validate_references(
    manifest: &BTreeMap<String, ManifestItem>,
    spine: &[SpineItem],
    nav_id: Option<&str>,
    ncx_id: Option<&str>,
    cover_id: Option<&str>,
) -> Result<(), ConversionError> {
    for item in manifest.values() {
        if let Some(fallback) = &item.fallback
            && !manifest.contains_key(fallback)
        {
            return Err(xml::malformed("manifest fallback names a missing item"));
        }
        let mut seen = BTreeSet::new();
        let mut current = Some(item.id.as_str());
        while let Some(id) = current {
            if !seen.insert(id) {
                return Err(xml::malformed("manifest fallback chain is cyclic"));
            }
            current = manifest.get(id).and_then(|item| item.fallback.as_deref());
        }
    }
    for item in spine {
        if !manifest.contains_key(&item.idref) {
            return Err(xml::malformed("spine itemref names a missing manifest item"));
        }
    }
    for (label, id) in [("navigation", nav_id), ("NCX", ncx_id), ("cover", cover_id)] {
        if let Some(id) = id
            && !manifest.contains_key(id)
        {
            return Err(xml::malformed(format!("{label} names a missing manifest item")));
        }
    }
    Ok(())
}

fn require_parent(stack: &[Frame], expected: &[u8], label: &str) -> Result<(), ConversionError> {
    if stack.last().is_some_and(|frame| frame.name.matches(Some(OPF_NS), expected)) {
        Ok(())
    } else {
        Err(xml::malformed(format!("{label} has an invalid parent")))
    }
}

fn valid_id(value: &str) -> bool {
    xml::valid_ncname(value)
}

fn valid_media_type(value: &str) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else { return false };
    !kind.is_empty()
        && !subtype.is_empty()
        && kind.chars().chain(subtype.chars()).all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '!' | '#' | '$' | '&' | '^' | '_' | '.' | '+' | '-')
        })
}

fn valid_modified(value: &str) -> bool {
    let Some(value) = value.strip_suffix('Z') else { return false };
    let Some((date, time)) = value.split_once('T') else { return false };
    let mut date = date.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (date.next(), date.next(), date.next(), date.next())
    else {
        return false;
    };
    let mut time = time.split(':');
    let (Some(hour), Some(minute), Some(second), None) =
        (time.next(), time.next(), time.next(), time.next())
    else {
        return false;
    };
    let (whole_second, fraction) =
        second.split_once('.').map_or((second, None), |parts| (parts.0, Some(parts.1)));
    let Some(year) = decimal(year, 4) else { return false };
    let Some(month) = decimal(month, 2) else { return false };
    let Some(day) = decimal(day, 2) else { return false };
    let Some(hour) = decimal(hour, 2) else { return false };
    let Some(minute) = decimal(minute, 2) else { return false };
    let Some(second) = decimal(whole_second, 2) else { return false };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59
        && fraction.is_none_or(|value| {
            !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn decimal(value: &str, width: usize) -> Option<u32> {
    (value.len() == width && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
