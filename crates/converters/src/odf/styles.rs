use crate::odf::model::{FO_NS, ListLevelSpec, OFFICE_NS, STYLE_NS, TEXT_NS, malformed};
use crate::odf::xml::XmlNode;
use into_markdown_core::{ConversionError, InlineMark};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct StyleKey {
    family: String,
    name: String,
}

pub(super) type StyleMap = BTreeMap<StyleKey, StyleSpec>;

#[derive(Clone, Debug, Default)]
pub(super) struct StyleSpec {
    marks: Vec<InlineMark>,
    parent: Option<StyleKey>,
}

#[derive(Default)]
pub(super) struct StyleCatalog {
    pub(super) text: StyleMap,
    pub(super) lists: BTreeMap<String, BTreeMap<u8, ListLevelSpec>>,
}

pub(super) fn validate_document_versions(
    manifest_version: &str,
    content: &XmlNode,
    styles: Option<&XmlNode>,
    metadata: Option<&XmlNode>,
    settings: Option<&XmlNode>,
) -> Result<(), ConversionError> {
    let content_version = content.attr(OFFICE_NS, "version").unwrap_or(manifest_version);
    if content_version != manifest_version {
        return Err(malformed(
            Some("content.xml"),
            "office:version disagrees with META-INF/manifest.xml",
        ));
    }
    for (part, root) in [("styles.xml", styles), ("meta.xml", metadata), ("settings.xml", settings)]
    {
        if root
            .and_then(|root| root.attr(OFFICE_NS, "version"))
            .is_some_and(|version| version != content_version)
        {
            return Err(malformed(
                Some(part),
                "office:version disagrees with content.xml and the manifest",
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn collect_styles(
    styles_root: Option<&XmlNode>,
    content: &XmlNode,
) -> Result<StyleCatalog, ConversionError> {
    let mut sources = Vec::new();
    if let Some(root) = styles_root {
        for container in root.children() {
            if container.is(OFFICE_NS, "styles") {
                sources.push((container, 0_u8, "styles.xml"));
            } else if container.is(OFFICE_NS, "automatic-styles") {
                sources.push((container, 1_u8, "styles.xml"));
            }
        }
    }
    for container in content.children().filter(|node| node.is(OFFICE_NS, "automatic-styles")) {
        sources.push((container, 2_u8, "content.xml"));
    }

    let mut ranked_styles: BTreeMap<StyleKey, (u8, StyleSpec)> = BTreeMap::new();
    let mut ranked_lists: BTreeMap<String, (u8, BTreeMap<u8, ListLevelSpec>)> = BTreeMap::new();
    for (container, rank, part) in sources {
        for node in container.children() {
            if node.is(TEXT_NS, "list-style") {
                let name = node
                    .attr(STYLE_NS, "name")
                    .ok_or_else(|| malformed(Some(part), "text:list-style lacks style:name"))?;
                if name.is_empty() {
                    return Err(malformed(Some(part), "list style name is empty"));
                }
                let mut levels = BTreeMap::new();
                for level in node.children() {
                    let ordered = if level.is(TEXT_NS, "list-level-style-number") {
                        true
                    } else if level.is(TEXT_NS, "list-level-style-bullet") {
                        false
                    } else {
                        return Err(malformed(
                            Some(part),
                            "text:list-style has an unsupported direct child",
                        ));
                    };
                    let number = level
                        .attr(TEXT_NS, "level")
                        .ok_or_else(|| malformed(Some(part), "list level is missing"))?
                        .parse::<u8>()
                        .map_err(|_| malformed(Some(part), "invalid text:level"))?;
                    if number == 0 {
                        return Err(malformed(Some(part), "text:level must be positive"));
                    }
                    let start = level
                        .attr(TEXT_NS, "start-value")
                        .unwrap_or("1")
                        .parse::<u64>()
                        .map_err(|_| malformed(Some(part), "invalid list start-value"))?;
                    if start == 0 || start > u64::from(u32::MAX) {
                        return Err(malformed(Some(part), "list start-value is out of range"));
                    }
                    if ordered && level.attr(STYLE_NS, "num-format").is_none() {
                        return Err(malformed(
                            Some(part),
                            "numbered list level lacks style:num-format",
                        ));
                    }
                    if !ordered {
                        level
                            .attr(TEXT_NS, "bullet-char")
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                malformed(Some(part), "bullet list level lacks text:bullet-char")
                            })?;
                    }
                    if levels.insert(number, ListLevelSpec { ordered, start }).is_some() {
                        return Err(malformed(Some(part), "duplicate list level"));
                    }
                }
                if levels.is_empty() {
                    return Err(malformed(Some(part), "empty text:list-style definition"));
                }
                match ranked_lists.get(name) {
                    Some((existing_rank, _)) if *existing_rank == rank => {
                        return Err(malformed(
                            Some(part),
                            "duplicate text:list-style at the same style origin",
                        ));
                    }
                    Some((existing_rank, _)) if *existing_rank > rank => {}
                    _ => {
                        ranked_lists.insert(name.to_owned(), (rank, levels));
                    }
                }
                continue;
            }
            if !node.is(STYLE_NS, "style") {
                continue;
            }
            let Some(name) = node.attr(STYLE_NS, "name") else { continue };
            if name.is_empty() {
                return Err(malformed(Some(part), "style name is empty"));
            }
            let family = node
                .attr(STYLE_NS, "family")
                .filter(|family| !family.is_empty())
                .ok_or_else(|| malformed(Some(part), "style:style lacks style:family"))?;
            let key = StyleKey { family: family.to_owned(), name: name.to_owned() };
            let mut spec = StyleSpec {
                parent: node
                    .attr(STYLE_NS, "parent-style-name")
                    .map(|parent| StyleKey { family: family.to_owned(), name: parent.to_owned() }),
                ..StyleSpec::default()
            };
            for properties in node.children().filter(|child| child.is(STYLE_NS, "text-properties"))
            {
                if properties.attr(FO_NS, "font-weight").is_some_and(|value| {
                    value.eq_ignore_ascii_case("bold")
                        || value.parse::<u16>().is_ok_and(|value| value >= 600)
                }) {
                    spec.marks.push(InlineMark::Bold);
                }
                if properties
                    .attr(FO_NS, "font-style")
                    .is_some_and(|value| value.eq_ignore_ascii_case("italic"))
                {
                    spec.marks.push(InlineMark::Italic);
                }
                if properties
                    .attr(STYLE_NS, "text-underline-style")
                    .is_some_and(|value| value != "none")
                {
                    spec.marks.push(InlineMark::Underline);
                }
                if properties
                    .attr(STYLE_NS, "text-line-through-style")
                    .is_some_and(|value| value != "none")
                {
                    spec.marks.push(InlineMark::Strikethrough);
                }
                if let Some(position) = properties.attr(STYLE_NS, "text-position") {
                    if position.starts_with("super") {
                        spec.marks.push(InlineMark::Superscript);
                    } else if position.starts_with("sub") {
                        spec.marks.push(InlineMark::Subscript);
                    }
                }
            }
            spec.marks.sort();
            spec.marks.dedup();
            match ranked_styles.get(&key) {
                Some((existing_rank, _)) if *existing_rank == rank => {
                    return Err(malformed(
                        Some(part),
                        format!("duplicate style family/name {family}/{name}"),
                    ));
                }
                Some((existing_rank, _)) if *existing_rank > rank => {}
                _ => {
                    ranked_styles.insert(key, (rank, spec));
                }
            }
        }
    }
    let styles: StyleMap = ranked_styles.into_iter().map(|(key, (_, spec))| (key, spec)).collect();
    let lists = ranked_lists.into_iter().map(|(name, (_, levels))| (name, levels)).collect();
    // Resolve cycles now so a hostile style graph cannot recurse during content parsing.
    for key in styles.keys() {
        let mut seen = BTreeSet::new();
        let mut current = Some(key.clone());
        while let Some(value) = current {
            if !seen.insert(value.clone()) {
                return Err(malformed(Some("styles.xml"), "cyclic style inheritance"));
            }
            current = styles.get(&value).and_then(|style| style.parent.clone());
        }
    }
    Ok(StyleCatalog { text: styles, lists })
}

pub(super) fn style_marks(
    styles: &StyleMap,
    family: &str,
    name: Option<&str>,
    inherited: &[InlineMark],
) -> Vec<InlineMark> {
    let mut marks = inherited.to_vec();
    let mut chain = Vec::new();
    let mut current =
        name.map(|name| StyleKey { family: family.to_owned(), name: name.to_owned() });
    while let Some(value) = current.take() {
        if let Some(style) = styles.get(&value) {
            chain.push(style);
            current.clone_from(&style.parent);
        } else {
            break;
        }
    }
    for style in chain.into_iter().rev() {
        marks.extend(style.marks.iter().copied());
    }
    marks.sort();
    marks.dedup();
    marks
}
