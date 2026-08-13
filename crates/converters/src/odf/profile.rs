use crate::odf::model::{
    CONFIG_NS, DC_NS, DRAW_NS, FO_NS, META_NS, NUMBER_NS, OFFICE_NS, PRESENTATION_NS, STYLE_NS,
    SVG_NS, TABLE_NS, TEXT_NS, XLINK_NS, XML_NS, malformed,
};
use crate::odf::xml::{Attr, XmlNode};
use into_markdown_core::ConversionError;

#[derive(Clone, Copy)]
pub(super) enum OdfXmlPart {
    Content,
    Styles,
    Meta,
    Settings,
}

pub(super) fn validate_tree_profile(
    root: &XmlNode,
    profile: OdfXmlPart,
    part: &str,
) -> Result<(), ConversionError> {
    let root_ok = match profile {
        OdfXmlPart::Content => root.is(OFFICE_NS, "document-content"),
        OdfXmlPart::Styles => root.is(OFFICE_NS, "document-styles"),
        OdfXmlPart::Meta => root.is(OFFICE_NS, "document-meta"),
        OdfXmlPart::Settings => root.is(OFFICE_NS, "document-settings"),
    };
    if !root_ok {
        return Err(malformed(Some(part), "XML root is outside the selected ODF part profile"));
    }
    validate_profile_node(root, None, profile, part)
}

fn validate_profile_node(
    node: &XmlNode,
    parent: Option<&XmlNode>,
    profile: OdfXmlPart,
    part: &str,
) -> Result<(), ConversionError> {
    let forbidden = matches!(
        node.name.local.as_str(),
        "scripts"
            | "script"
            | "event-listeners"
            | "event-listener"
            | "object"
            | "object-ole"
            | "plugin"
            | "applet"
            | "floating-frame"
            | "form"
            | "forms"
            | "document-signatures"
            | "dde-link"
    );
    if forbidden || !allowed_profile_element(node, profile) {
        return Err(malformed(
            Some(part),
            format!(
                "element {}:{} is outside the closed ODF profile",
                node.name.ns, node.name.local
            ),
        ));
    }
    if let Some(parent) = parent {
        validate_profile_parent(parent, node, profile, part)?;
    }
    for attr in &node.attrs {
        if !allowed_profile_attribute(node, attr) {
            return Err(malformed(
                Some(part),
                format!(
                    "attribute {}:{} on {}:{} is outside the closed ODF profile",
                    attr.name.ns, attr.name.local, node.name.ns, node.name.local
                ),
            ));
        }
    }
    for child in node.children() {
        validate_profile_node(child, Some(node), profile, part)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn allowed_profile_element(node: &XmlNode, profile: OdfXmlPart) -> bool {
    let local = node.name.local.as_str();
    match profile {
        OdfXmlPart::Content | OdfXmlPart::Styles => match node.name.ns.as_str() {
            OFFICE_NS => matches!(
                local,
                "document-content"
                    | "document-styles"
                    | "font-face-decls"
                    | "automatic-styles"
                    | "styles"
                    | "master-styles"
                    | "body"
                    | "text"
                    | "spreadsheet"
                    | "presentation"
                    | "annotation"
                    | "annotation-end"
            ),
            TEXT_NS => matches!(
                local,
                "p" | "h"
                    | "span"
                    | "a"
                    | "s"
                    | "tab"
                    | "line-break"
                    | "list"
                    | "list-item"
                    | "list-header"
                    | "section"
                    | "soft-page-break"
                    | "note"
                    | "note-citation"
                    | "note-body"
                    | "bookmark"
                    | "bookmark-start"
                    | "bookmark-end"
                    | "reference-mark"
                    | "reference-mark-start"
                    | "reference-mark-end"
                    | "list-style"
                    | "list-level-style-number"
                    | "list-level-style-bullet"
                    | "list-level-properties"
                    | "list-level-label-alignment"
            ),
            TABLE_NS => matches!(
                local,
                "table"
                    | "table-column"
                    | "table-columns"
                    | "table-header-columns"
                    | "table-row"
                    | "table-rows"
                    | "table-header-rows"
                    | "table-cell"
                    | "covered-table-cell"
            ),
            DRAW_NS => matches!(
                local,
                "page"
                    | "frame"
                    | "text-box"
                    | "image"
                    | "g"
                    | "custom-shape"
                    | "rect"
                    | "ellipse"
                    | "line"
                    | "connector"
            ),
            PRESENTATION_NS => local == "notes",
            STYLE_NS => matches!(
                local,
                "style"
                    | "default-style"
                    | "text-properties"
                    | "paragraph-properties"
                    | "table-properties"
                    | "table-row-properties"
                    | "table-column-properties"
                    | "table-cell-properties"
                    | "graphic-properties"
                    | "page-layout"
                    | "page-layout-properties"
                    | "master-page"
                    | "font-face"
            ),
            DC_NS => matches!(local, "creator" | "date"),
            NUMBER_NS => matches!(
                local,
                "number-style"
                    | "currency-style"
                    | "percentage-style"
                    | "date-style"
                    | "time-style"
                    | "text-style"
                    | "number"
                    | "currency-symbol"
                    | "day"
                    | "month"
                    | "year"
                    | "hours"
                    | "minutes"
                    | "seconds"
                    | "text"
            ),
            SVG_NS => matches!(local, "title" | "desc"),
            _ => false,
        },
        OdfXmlPart::Meta => match node.name.ns.as_str() {
            OFFICE_NS => matches!(local, "document-meta" | "meta"),
            DC_NS => matches!(
                local,
                "title" | "creator" | "subject" | "description" | "language" | "date"
            ),
            META_NS => matches!(
                local,
                "generator"
                    | "initial-creator"
                    | "creation-date"
                    | "editing-duration"
                    | "editing-cycles"
                    | "keyword"
                    | "user-defined"
                    | "document-statistic"
            ),
            _ => false,
        },
        OdfXmlPart::Settings => match node.name.ns.as_str() {
            OFFICE_NS => matches!(local, "document-settings" | "settings"),
            CONFIG_NS => matches!(
                local,
                "config-item-set"
                    | "config-item-map-indexed"
                    | "config-item-map-named"
                    | "config-item-map-entry"
                    | "config-item"
            ),
            _ => false,
        },
    }
}

#[allow(clippy::match_same_arms, clippy::too_many_lines, clippy::unnested_or_patterns)]
fn validate_profile_parent(
    parent: &XmlNode,
    child: &XmlNode,
    profile: OdfXmlPart,
    part: &str,
) -> Result<(), ConversionError> {
    let text_block_parent = parent.is(OFFICE_NS, "text")
        || parent.is(TEXT_NS, "section")
        || parent.is(TEXT_NS, "list-item")
        || parent.is(TEXT_NS, "list-header")
        || parent.is(TEXT_NS, "note-body")
        || parent.is(TABLE_NS, "table-cell")
        || parent.is(DRAW_NS, "text-box")
        || parent.is(PRESENTATION_NS, "notes")
        || parent.is(OFFICE_NS, "annotation");
    let inline_parent = parent.is(TEXT_NS, "p")
        || parent.is(TEXT_NS, "h")
        || parent.is(TEXT_NS, "span")
        || parent.is(TEXT_NS, "a")
        || parent.is(TEXT_NS, "note-citation");
    let style_container =
        parent.is(OFFICE_NS, "styles") || parent.is(OFFICE_NS, "automatic-styles");
    let allowed = match (child.name.ns.as_str(), child.name.local.as_str()) {
        (OFFICE_NS, "font-face-decls") => {
            parent.is(OFFICE_NS, "document-content") || parent.is(OFFICE_NS, "document-styles")
        }
        (OFFICE_NS, "automatic-styles") => {
            parent.is(OFFICE_NS, "document-content") || parent.is(OFFICE_NS, "document-styles")
        }
        (OFFICE_NS, "styles" | "master-styles") => parent.is(OFFICE_NS, "document-styles"),
        (OFFICE_NS, "body") => parent.is(OFFICE_NS, "document-content"),
        (OFFICE_NS, "text" | "spreadsheet" | "presentation") => parent.is(OFFICE_NS, "body"),
        (OFFICE_NS, "meta") => parent.is(OFFICE_NS, "document-meta"),
        (OFFICE_NS, "settings") => parent.is(OFFICE_NS, "document-settings"),
        (OFFICE_NS, "annotation") => text_block_parent || inline_parent,
        (OFFICE_NS, "annotation-end") => text_block_parent || inline_parent,
        (TEXT_NS, "p" | "h") => text_block_parent,
        (
            TEXT_NS,
            "span"
            | "a"
            | "s"
            | "tab"
            | "line-break"
            | "note"
            | "bookmark"
            | "bookmark-start"
            | "bookmark-end"
            | "reference-mark"
            | "reference-mark-start"
            | "reference-mark-end",
        ) => inline_parent,
        (TEXT_NS, "list" | "section" | "soft-page-break") => text_block_parent,
        (TEXT_NS, "list-item" | "list-header") => parent.is(TEXT_NS, "list"),
        (TEXT_NS, "note-citation" | "note-body") => parent.is(TEXT_NS, "note"),
        (TEXT_NS, "list-style") => style_container,
        (TEXT_NS, "list-level-style-number" | "list-level-style-bullet") => {
            parent.is(TEXT_NS, "list-style")
        }
        (TEXT_NS, "list-level-properties") => {
            parent.is(TEXT_NS, "list-level-style-number")
                || parent.is(TEXT_NS, "list-level-style-bullet")
        }
        (TEXT_NS, "list-level-label-alignment") => parent.is(TEXT_NS, "list-level-properties"),
        (TABLE_NS, "table") => {
            text_block_parent
                || parent.is(OFFICE_NS, "spreadsheet")
                || parent.is(OFFICE_NS, "presentation")
                || parent.is(DRAW_NS, "frame")
        }
        (TABLE_NS, "table-columns" | "table-header-columns") => parent.is(TABLE_NS, "table"),
        (TABLE_NS, "table-column") => {
            parent.is(TABLE_NS, "table")
                || parent.is(TABLE_NS, "table-columns")
                || parent.is(TABLE_NS, "table-header-columns")
        }
        (TABLE_NS, "table-rows" | "table-header-rows") => parent.is(TABLE_NS, "table"),
        (TABLE_NS, "table-row") => {
            parent.is(TABLE_NS, "table")
                || parent.is(TABLE_NS, "table-rows")
                || parent.is(TABLE_NS, "table-header-rows")
        }
        (TABLE_NS, "table-cell" | "covered-table-cell") => parent.is(TABLE_NS, "table-row"),
        (DRAW_NS, "page") => parent.is(OFFICE_NS, "presentation"),
        (DRAW_NS, "frame" | "g" | "custom-shape" | "rect" | "ellipse" | "line" | "connector") => {
            parent.is(DRAW_NS, "page") || parent.is(DRAW_NS, "g") || text_block_parent
        }
        (DRAW_NS, "text-box") => {
            parent.name.ns == DRAW_NS
                && matches!(
                    parent.name.local.as_str(),
                    "frame" | "custom-shape" | "rect" | "ellipse"
                )
        }
        (DRAW_NS, "image") => parent.is(DRAW_NS, "frame") || parent.is(DRAW_NS, "g"),
        (PRESENTATION_NS, "notes") => parent.is(DRAW_NS, "page"),
        (STYLE_NS, "style" | "default-style" | "page-layout") => style_container,
        (
            STYLE_NS,
            "text-properties"
            | "paragraph-properties"
            | "table-properties"
            | "table-row-properties"
            | "table-column-properties"
            | "table-cell-properties"
            | "graphic-properties",
        ) => parent.is(STYLE_NS, "style") || parent.is(STYLE_NS, "default-style"),
        (STYLE_NS, "page-layout-properties") => parent.is(STYLE_NS, "page-layout"),
        (STYLE_NS, "master-page") => parent.is(OFFICE_NS, "master-styles"),
        (STYLE_NS, "font-face") => parent.is(OFFICE_NS, "font-face-decls"),
        (
            NUMBER_NS,
            "number-style" | "currency-style" | "percentage-style" | "date-style" | "time-style"
            | "text-style",
        ) => style_container,
        (
            NUMBER_NS,
            "number" | "currency-symbol" | "day" | "month" | "year" | "hours" | "minutes"
            | "seconds" | "text",
        ) => parent.name.ns == NUMBER_NS && parent.name.local.ends_with("style"),
        (SVG_NS, "title" | "desc") => parent.name.ns == DRAW_NS,
        (DC_NS, "creator" | "date") => {
            parent.is(OFFICE_NS, "meta") || parent.is(OFFICE_NS, "annotation")
        }
        (DC_NS, _) | (META_NS, _) => parent.is(OFFICE_NS, "meta"),
        (CONFIG_NS, "config-item-set") => parent.is(OFFICE_NS, "settings"),
        (CONFIG_NS, "config-item-map-indexed" | "config-item-map-named" | "config-item") => {
            parent.is(CONFIG_NS, "config-item-set") || parent.is(CONFIG_NS, "config-item-map-entry")
        }
        (CONFIG_NS, "config-item-map-entry") => {
            parent.is(CONFIG_NS, "config-item-map-indexed")
                || parent.is(CONFIG_NS, "config-item-map-named")
        }
        _ => false,
    };
    let part_allows = match profile {
        OdfXmlPart::Content => !parent.is(OFFICE_NS, "document-styles"),
        OdfXmlPart::Styles => !parent.is(OFFICE_NS, "document-content"),
        OdfXmlPart::Meta | OdfXmlPart::Settings => true,
    };
    if allowed && part_allows {
        Ok(())
    } else {
        Err(malformed(
            Some(part),
            format!(
                "{}:{} is not permitted directly under {}:{} in this ODF profile",
                child.name.ns, child.name.local, parent.name.ns, parent.name.local
            ),
        ))
    }
}

#[allow(clippy::too_many_lines)]
fn allowed_profile_attribute(node: &XmlNode, attr: &Attr) -> bool {
    let local = attr.name.local.as_str();
    match attr.name.ns.as_str() {
        OFFICE_NS if local == "version" => matches!(
            node.name.local.as_str(),
            "document-content" | "document-styles" | "document-meta" | "document-settings"
        ),
        OFFICE_NS if local == "name" => {
            node.is(OFFICE_NS, "annotation") || node.is(OFFICE_NS, "annotation-end")
        }
        OFFICE_NS => {
            node.is(TABLE_NS, "table-cell")
                && matches!(
                    local,
                    "value-type"
                        | "string-value"
                        | "value"
                        | "date-value"
                        | "time-value"
                        | "boolean-value"
                        | "currency"
                )
        }
        TEXT_NS if local == "style-name" => {
            matches!(node.name.local.as_str(), "p" | "h" | "span" | "list" | "section")
        }
        TEXT_NS if local == "outline-level" => node.is(TEXT_NS, "h"),
        TEXT_NS if local == "c" => node.is(TEXT_NS, "s"),
        TEXT_NS if matches!(local, "continue-numbering" | "continue-list") => {
            node.is(TEXT_NS, "list")
        }
        TEXT_NS if local == "start-value" => {
            node.is(TEXT_NS, "list-item") || node.is(TEXT_NS, "list-level-style-number")
        }
        TEXT_NS if matches!(local, "level" | "display-levels") => {
            node.is(TEXT_NS, "list-level-style-number")
                || node.is(TEXT_NS, "list-level-style-bullet")
        }
        TEXT_NS if local == "bullet-char" => node.is(TEXT_NS, "list-level-style-bullet"),
        TEXT_NS if matches!(local, "id" | "note-class") => node.is(TEXT_NS, "note"),
        TEXT_NS if matches!(local, "name" | "ref-name") => matches!(
            node.name.local.as_str(),
            "bookmark"
                | "bookmark-start"
                | "bookmark-end"
                | "reference-mark"
                | "reference-mark-start"
                | "reference-mark-end"
        ),
        TABLE_NS if local == "name" => node.is(TABLE_NS, "table"),
        TABLE_NS if local == "style-name" => {
            matches!(
                node.name.local.as_str(),
                "table" | "table-column" | "table-row" | "table-cell" | "covered-table-cell"
            ) && node.name.ns == TABLE_NS
        }
        TABLE_NS if local == "number-columns-repeated" => {
            node.is(TABLE_NS, "table-column")
                || node.is(TABLE_NS, "table-cell")
                || node.is(TABLE_NS, "covered-table-cell")
        }
        TABLE_NS if local == "number-rows-repeated" => node.is(TABLE_NS, "table-row"),
        TABLE_NS
            if matches!(local, "number-columns-spanned" | "number-rows-spanned" | "formula") =>
        {
            node.is(TABLE_NS, "table-cell")
        }
        DRAW_NS => {
            node.name.ns == DRAW_NS && matches!(local, "name" | "style-name" | "transform" | "id")
        }
        PRESENTATION_NS => node.name.ns == DRAW_NS && matches!(local, "class" | "style-name"),
        STYLE_NS if matches!(local, "name" | "family" | "parent-style-name" | "display-name") => {
            node.is(STYLE_NS, "style")
                || node.is(STYLE_NS, "default-style")
                || node.is(TEXT_NS, "list-style")
                || node.is(STYLE_NS, "page-layout")
        }
        STYLE_NS if matches!(local, "num-format" | "num-prefix" | "num-suffix") => {
            node.is(TEXT_NS, "list-level-style-number")
        }
        STYLE_NS
            if matches!(
                local,
                "text-underline-style" | "text-line-through-style" | "text-position"
            ) =>
        {
            node.is(STYLE_NS, "text-properties")
        }
        STYLE_NS if matches!(local, "font-name" | "page-layout-name") => node.name.ns == STYLE_NS,
        FO_NS => {
            node.name.ns == STYLE_NS
                && matches!(
                    local,
                    "font-weight"
                        | "font-style"
                        | "font-size"
                        | "color"
                        | "background-color"
                        | "text-align"
                        | "margin-left"
                        | "margin-right"
                        | "text-indent"
                        | "break-before"
                        | "break-after"
                )
        }
        SVG_NS => {
            node.name.ns == DRAW_NS && matches!(local, "x" | "y" | "width" | "height" | "viewBox")
        }
        XLINK_NS => {
            (node.is(TEXT_NS, "a") || node.is(DRAW_NS, "image"))
                && matches!(local, "href" | "type" | "show" | "actuate")
        }
        META_NS => node.is(META_NS, "user-defined") && matches!(local, "name" | "value-type"),
        CONFIG_NS => node.name.ns == CONFIG_NS && matches!(local, "name" | "type"),
        NUMBER_NS => {
            matches!(
                local,
                "decimal-places" | "min-integer-digits" | "style" | "language" | "country"
            ) && node.name.ns == NUMBER_NS
        }
        XML_NS if local == "id" => node.is(TEXT_NS, "list") || node.is(TEXT_NS, "note"),
        XML_NS if local == "lang" => {
            matches!(node.name.ns.as_str(), TEXT_NS | DC_NS | META_NS | STYLE_NS)
        }
        _ => false,
    }
}
