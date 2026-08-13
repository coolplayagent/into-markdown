use crate::odf::model::{MANIFEST_NS, malformed};
use crate::odf::paths::canonical_part_name;
use crate::odf::xml::{XmlNode, contains_element};
use into_markdown_core::ConversionError;
use std::collections::BTreeMap;

#[derive(Debug)]
pub(super) struct ManifestEntry {
    pub(super) media_type: String,
}

pub(super) fn parse_manifest(
    root: &XmlNode,
    expected: &str,
) -> Result<(BTreeMap<String, ManifestEntry>, String), ConversionError> {
    if !root.is(MANIFEST_NS, "manifest") {
        return Err(malformed(
            Some("META-INF/manifest.xml"),
            "unexpected manifest root or namespace",
        ));
    }
    let version = root.attr(MANIFEST_NS, "version").unwrap_or("1.2");
    if !matches!(version, "1.2" | "1.3") {
        return Err(malformed(Some("META-INF/manifest.xml"), "unsupported ODF manifest version"));
    }
    if root.attrs.iter().any(|attr| attr.name.ns != MANIFEST_NS || attr.name.local != "version") {
        return Err(malformed(
            Some("META-INF/manifest.xml"),
            "manifest root contains an unsupported attribute",
        ));
    }
    let mut result = BTreeMap::new();
    for child in root.children() {
        if child.is(MANIFEST_NS, "encryption-data")
            || contains_element(child, MANIFEST_NS, "encryption-data")
        {
            return Err(ConversionError::Encrypted);
        }
        if !child.is(MANIFEST_NS, "file-entry") {
            return Err(malformed(
                Some("META-INF/manifest.xml"),
                "manifest contains an unsupported direct child",
            ));
        }
        if child.children().next().is_some() {
            return Err(malformed(
                Some("META-INF/manifest.xml"),
                "manifest file-entry contains unsupported nested relationship data",
            ));
        }
        if child.attrs.iter().any(|attr| {
            attr.name.ns != MANIFEST_NS
                || !matches!(attr.name.local.as_str(), "full-path" | "media-type" | "version")
        }) {
            return Err(malformed(
                Some("META-INF/manifest.xml"),
                "manifest file-entry contains an unsupported attribute",
            ));
        }
        let raw_path = child.attr(MANIFEST_NS, "full-path").ok_or_else(|| {
            malformed(Some("META-INF/manifest.xml"), "file-entry lacks full-path")
        })?;
        let path = if raw_path == "/" {
            "/".to_owned()
        } else {
            canonical_part_name(raw_path, raw_path.ends_with('/'))?
        };
        let media_type = child.attr(MANIFEST_NS, "media-type").unwrap_or("").to_owned();
        if child.attr(MANIFEST_NS, "version").is_some_and(|entry_version| entry_version != version)
        {
            return Err(malformed(
                Some("META-INF/manifest.xml"),
                "manifest file-entry version disagrees with the manifest root",
            ));
        }
        if result.insert(path.clone(), ManifestEntry { media_type }).is_some() {
            return Err(malformed(
                Some("META-INF/manifest.xml"),
                format!("duplicate manifest path {path}"),
            ));
        }
    }
    if result.get("/").map(|entry| entry.media_type.as_str()) != Some(expected) {
        return Err(malformed(
            Some("META-INF/manifest.xml"),
            "manifest package media type disagrees with mimetype",
        ));
    }
    Ok((result, version.to_owned()))
}
