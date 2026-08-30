//! Static-document projection of optional ODF features. XML is fully parsed and
//! integrity/resource checked before this pass; omitted code is never executed/exported.
use super::compatibility::FORM_NS;
use super::model::{DRAW_NS, OFFICE_NS, ParseState, TEXT_NS, malformed, part_locator};
use super::xml::{XmlContent, XmlNode};
use into_markdown_core::{ConversionError, ConversionOptions, ErrorPolicy, ExecutionContext};

pub(super) const SCRIPT_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:script:1.0";
pub(super) const ANIM_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:animation:1.0";
pub(super) const SMIL_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:smil-compatible:1.0";

pub(super) fn optional_namespace(ns: &str) -> bool {
    matches!(ns, SCRIPT_NS | ANIM_NS | SMIL_NS | FORM_NS)
}

pub(super) fn require_best_effort(
    options: &ConversionOptions,
    part: &str,
    detail: &str,
) -> Result<(), ConversionError> {
    if options.error_policy == ErrorPolicy::Strict {
        return Err(malformed(Some(part), detail));
    }
    Ok(())
}

pub(super) fn ensure_static_body(state: &ParseState) -> Result<(), ConversionError> {
    fn has_body(node: &into_markdown_core::BlockNode) -> bool {
        use into_markdown_core::Block;
        match &node.block {
            Block::Slide { title, blocks, .. } => {
                title.as_ref().is_some_and(|text| !text.trim().is_empty())
                    || blocks.iter().any(has_body)
            }
            Block::Sheet { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Footnote { blocks, .. } => blocks.iter().any(has_body),
            Block::List { items, .. } => items.iter().any(|item| item.blocks.iter().any(has_body)),
            Block::Table { rows, .. } => {
                rows.iter().any(|row| row.cells.iter().any(|cell| cell.blocks.iter().any(has_body)))
            }
            Block::Paragraph(content) | Block::Heading { content, .. } => !content.is_empty(),
            _ => true,
        }
    }
    if state.diagnostics.iter().any(|diagnostic| diagnostic.code == "odf.scriptsOmitted")
        && !state.document.blocks.iter().any(has_body)
    {
        return Err(malformed(
            Some("content.xml"),
            "script-bearing document has no recoverable static body",
        ));
    }
    Ok(())
}

pub(super) fn project_static_content(
    node: &mut XmlNode,
    part: &str,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    manifest: &std::collections::BTreeMap<String, super::manifest::ManifestEntry>,
) -> Result<(), ConversionError> {
    context.checkpoint()?;
    project_decoration(node, part, state, options)?;
    project_cached_display(node, part, state, options)?;
    project_untyped_replacement(node, part, state, options, manifest)?;
    let parent = &node.name;
    let mut failure = None;
    node.content.retain_mut(|content| {
        let XmlContent::Node(child) = content else { return true };
        let omission = optional_subtree(child, &parent.ns, &parent.local);
        if let Some((code, detail)) = omission {
            if let Err(error) = require_best_effort(options, part, detail) {
                failure = Some(error);
                return true;
            }
            state.warning(code, detail, part_locator(part));
            return false;
        }
        if failure.is_none()
            && let Err(error) =
                project_static_content(child, part, state, options, context, manifest)
        {
            failure = Some(error);
        }
        true
    });
    failure.map_or(Ok(()), Err)?;
    super::sparse::trim_empty_tail(node, options, state)
}

fn project_cached_display(
    node: &mut XmlNode,
    part: &str,
    state: &mut ParseState,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if node.name.ns != TEXT_NS {
        return Ok(());
    }
    if matches!(
        node.name.local.as_str(),
        "table-of-content"
            | "bibliography"
            | "alphabetical-index"
            | "illustration-index"
            | "table-index"
            | "user-index"
    ) {
        require_best_effort(
            options,
            part,
            "generated index retains its cached body, not generator definitions",
        )?;
        node.content.retain(
            |value| matches!(value, XmlContent::Node(child) if child.is(TEXT_NS, "index-body")),
        );
        node.name.local = "section".into();
        node.attrs.clear();
        for value in &mut node.content {
            if let XmlContent::Node(child) = value {
                child.name.local = "section".into();
                child.attrs.clear();
            }
        }
        state.warning(
            "odf.cachedIndex",
            "Generated index retains its cached body; index was not regenerated",
            part_locator(part),
        );
    } else if node.is(TEXT_NS, "index-title") {
        node.name.local = "section".into();
        node.attrs.clear();
    } else if node.is(TEXT_NS, "bibliography-mark")
        || (super::compatibility::cached_text_field(node)
            && (node.attrs.iter().any(|attr| {
                attr.name.ns == super::model::STYLE_NS
                    && matches!(
                        attr.name.local.as_str(),
                        "data-style-name" | "num-format" | "num-letter-sync"
                    )
            }) || node.attr(OFFICE_NS, "value-type").is_some()))
    {
        require_best_effort(
            options,
            part,
            "field retains cached display text without evaluating formatting/expressions",
        )?;
        node.name.local = "span".into();
        node.attrs.retain(|attr| attr.name.ns == TEXT_NS && attr.name.local == "style-name");
        state.warning(
            "odf.cachedField",
            "Field retains cached display text; formatting/expressions were not evaluated",
            part_locator(part),
        );
    }
    Ok(())
}

fn project_decoration(
    node: &mut XmlNode,
    part: &str,
    state: &mut ParseState,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if node.is(DRAW_NS, "circle") {
        node.name.local = "ellipse".into();
    }
    if node.is(DRAW_NS, "path") {
        require_best_effort(
            options,
            part,
            "vector path geometry cannot be represented in Markdown",
        )?;
        node.name.local = "custom-shape".into();
        node.attrs.retain(|attr| {
            !(attr.name.ns == super::model::SVG_NS
                && matches!(attr.name.local.as_str(), "d" | "viewBox"))
        });
        state.warning(
            "odf.pathGeometryOmitted",
            "Vector path geometry omitted; associated static text retained",
            part_locator(part),
        );
    } else if node.is(TEXT_NS, "list-level-style-image") {
        require_best_effort(options, part, "image list marker replaced with plain bullet")?;
        node.name.local = "list-level-style-bullet".into();
        node.attrs.retain(|attr| attr.name.ns != super::model::XLINK_NS);
        node.attrs.push(super::xml::Attr {
            name: super::xml::Name { ns: TEXT_NS.into(), local: "bullet-char".into() },
            value: "•".into(),
        });
        state.warning(
            "odf.imageListMarker",
            "Image list marker represented as plain bullet; list content retained",
            part_locator(part),
        );
    }
    Ok(())
}

fn project_untyped_replacement(
    node: &mut XmlNode,
    part: &str,
    state: &mut ParseState,
    options: &ConversionOptions,
    manifest: &std::collections::BTreeMap<String, super::manifest::ManifestEntry>,
) -> Result<(), ConversionError> {
    if !node.is(DRAW_NS, "frame")
        || !node
            .children()
            .any(|child| child.is(DRAW_NS, "object") || child.is(DRAW_NS, "object-ole"))
    {
        return Ok(());
    }
    for value in &mut node.content {
        let XmlContent::Node(image) = value else { continue };
        if !image.is(DRAW_NS, "image") {
            continue;
        }
        let Some(href) = image.attr(super::model::XLINK_NS, "href") else { continue };
        let path =
            super::paths::canonical_part_name(href.strip_prefix("./").unwrap_or(href), false)?;
        if manifest.get(&path).is_some_and(|entry| entry.media_type.is_empty()) {
            require_best_effort(
                options,
                part,
                "embedded object has an untyped replacement graphic",
            )?;
            image.name.local = "text-box".into();
            image.attrs.clear();
            image.content = vec![XmlContent::Node(XmlNode {
                name: super::xml::Name { ns: TEXT_NS.into(), local: "p".into() },
                attrs: vec![],
                content: vec![XmlContent::Text(format!("[Object replacement omitted: {path}]"))],
            })];
            state.warning("odf.objectReplacementOmitted", format!("Untyped embedded-object replacement omitted: {path}; no bytes interpreted/exported"), part_locator(part));
        }
    }
    Ok(())
}

pub(super) fn drawing_placeholder(
    node: &XmlNode,
    state: &mut ParseState,
    options: &ConversionOptions,
    locator: &into_markdown_core::SourceLocator,
) -> Result<Option<into_markdown_core::BlockNode>, ConversionError> {
    if node.name.ns != DRAW_NS
        || !matches!(
            node.name.local.as_str(),
            "custom-shape" | "ellipse" | "rect" | "line" | "connector"
        )
    {
        return Ok(None);
    }
    require_best_effort(
        options,
        "content.xml",
        "drawing without text requires a geometry placeholder",
    )?;
    let label = node
        .children()
        .find(|child| child.is(DRAW_NS, "enhanced-geometry"))
        .and_then(|child| child.attr(DRAW_NS, "type"))
        .unwrap_or(&node.name.local);
    state.warning(
        "odf.drawingPlaceholder",
        format!(
            "Static drawing {label} has no text/image representation; geometry placeholder retained"
        ),
        locator.clone(),
    );
    state.add_inlines(1)?;
    state
        .node(
            into_markdown_core::Block::Paragraph(vec![into_markdown_core::Inline::Text {
                value: format!("[Drawing omitted: {label}]"),
                marks: vec![],
            }]),
            locator.clone(),
        )
        .map(Some)
}

fn optional_subtree(
    node: &XmlNode,
    parent_ns: &str,
    parent_local: &str,
) -> Option<(&'static str, &'static str)> {
    if node.name.ns == TEXT_NS
        && matches!(
            node.name.local.as_str(),
            "alphabetical-index-mark"
                | "alphabetical-index-mark-start"
                | "alphabetical-index-mark-end"
                | "toc-mark"
                | "toc-mark-start"
                | "toc-mark-end"
        )
        && parent_ns == TEXT_NS
    {
        return Some((
            "odf.indexMarkerOmitted",
            "Index generator marker omitted; visible text and cached index body retained",
        ));
    }
    if node.is(OFFICE_NS, "scripts")
        && parent_ns == OFFICE_NS
        && parent_local == "document-content"
        && node.children().next().is_some()
        || node.is(OFFICE_NS, "event-listeners") && matches!(parent_ns, DRAW_NS | TEXT_NS)
    {
        return Some((
            "odf.scriptsOmitted",
            "Optional scripts/event listeners omitted; no code executed or exported; static body retained",
        ));
    }
    if node.is(OFFICE_NS, "forms")
        && (parent_ns == OFFICE_NS || parent_ns == DRAW_NS || parent_ns == super::model::TABLE_NS)
        && node.children().next().is_some()
    {
        return Some((
            "odf.formsOmitted",
            "Optional form definitions/controls omitted; static body retained",
        ));
    }
    if node.is(ANIM_NS, "par") && parent_ns == DRAW_NS && parent_local == "page" {
        return Some((
            "odf.animationOmitted",
            "Animation timeline omitted; static slide content retained",
        ));
    }
    if node.is(TEXT_NS, "tracked-changes")
        && (parent_ns == OFFICE_NS && parent_local == "text"
            || parent_ns == TEXT_NS && parent_local == "section")
        && node.children().next().is_some()
        || node.name.ns == TEXT_NS
            && matches!(node.name.local.as_str(), "change" | "change-start" | "change-end")
            && parent_ns == TEXT_NS
    {
        return Some((
            "odf.trackedChanges",
            "Revision history/markers omitted; current body text retained, deleted history not inserted",
        ));
    }
    if node.name.ns == DRAW_NS
        && matches!(node.name.local.as_str(), "object" | "object-ole")
        && parent_ns == DRAW_NS
        && parent_local == "frame"
    {
        return Some((
            "odf.embeddedObjectOmitted",
            "Optional embedded object omitted without opening/executing it; replacement image and static body retained",
        ));
    }
    None
}
