use crate::odf::model::{DC_NS, META_NS, OFFICE_NS, ParseState, malformed};
use crate::odf::xml::{XmlNode, bounded_text, only_child};
use into_markdown_core::{ConversionError, ConversionOptions};

pub(super) fn parse_metadata(
    root: Option<&XmlNode>,
    settings: Option<&XmlNode>,
    state: &mut ParseState,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if let Some(root) = root {
        if !root.is(OFFICE_NS, "document-meta") {
            return Err(malformed(Some("meta.xml"), "unexpected metadata root"));
        }
        let meta = only_child(root, OFFICE_NS, "meta", "meta.xml")?;
        for node in meta.children() {
            let value = bounded_text(node, options, "meta.xml")?;
            if value.is_empty() {
                continue;
            }
            match (node.name.ns.as_str(), node.name.local.as_str()) {
                (DC_NS, "title") => {
                    if state.document.metadata.title.replace(value).is_some() {
                        return Err(malformed(Some("meta.xml"), "duplicate dc:title"));
                    }
                }
                (DC_NS, "creator") | (META_NS, "initial-creator") => {
                    if !state.document.metadata.authors.contains(&value) {
                        state.document.metadata.authors.push(value);
                    }
                }
                (DC_NS, local @ ("subject" | "description" | "language" | "date")) => {
                    state.document.metadata.properties.insert(format!("dc.{local}"), value);
                }
                (
                    META_NS,
                    local @ ("generator" | "creation-date" | "editing-duration" | "keyword"),
                ) => {
                    state.document.metadata.properties.insert(format!("odf.{local}"), value);
                }
                (META_NS, "user-defined") => {
                    let name = node.attr(META_NS, "name").ok_or_else(|| {
                        malformed(Some("meta.xml"), "user-defined metadata lacks name")
                    })?;
                    state.document.metadata.properties.insert(format!("odf.user.{name}"), value);
                }
                _ => {}
            }
        }
    }
    if let Some(settings) = settings {
        if !settings.is(OFFICE_NS, "document-settings") {
            return Err(malformed(Some("settings.xml"), "unexpected settings root"));
        }
        state.document.metadata.properties.insert("odf.settingsPresent".into(), "true".into());
    }
    Ok(())
}
