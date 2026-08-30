//! Inert producer metadata encountered in real ODF packages. Semantic content still goes
//! through the regular ODF parsers; this is not a fallback for unknown body elements.
use super::model::{
    DRAW_NS, FO_NS, META_NS, NUMBER_NS, OFFICE_NS, PRESENTATION_NS, STYLE_NS, SVG_NS, TABLE_NS,
    TEXT_NS,
};
use super::xml::{Attr, XmlNode};

pub(super) const LOEXT_NS: &str =
    "urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0";
pub(super) const CALCEXT_NS: &str =
    "urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0";
pub(super) const OFFICE_EXT_NS: &str = "http://openoffice.org/2009/office";
pub(super) const GRDDL_NS: &str = "http://www.w3.org/2003/g/data-view#";
pub(super) const FORM_NS: &str = "urn:oasis:names:tc:opendocument:xmlns:form:1.0";

pub(super) fn producer_namespace(ns: &str) -> bool {
    matches!(ns, LOEXT_NS | CALCEXT_NS | OFFICE_EXT_NS | GRDDL_NS | FORM_NS)
}

pub(super) fn producer_attribute(node: &XmlNode, attr: &Attr) -> bool {
    let local = attr.name.local.as_str();
    match attr.name.ns.as_str() {
        FO_NS if node.is(DRAW_NS, "text-box") => matches!(local, "min-height" | "min-width"),
        GRDDL_NS => {
            local == "transformation"
                && node.name.ns == OFFICE_NS
                && matches!(
                    node.name.local.as_str(),
                    "document-content" | "document-styles" | "document-meta"
                )
        }
        OFFICE_EXT_NS => {
            node.is(STYLE_NS, "text-properties") && matches!(local, "rsid" | "paragraph-rsid")
        }
        CALCEXT_NS => node.is(TABLE_NS, "table-cell") && local == "value-type",
        LOEXT_NS => matches!(
            (node.name.local.as_str(), local),
            ("graphic-properties", "allow-overlap")
                | ("image", "mime-type")
                | ("number" | "scientific-number", "min-decimal-places")
                | ("scientific-number", "exponent-interval" | "forced-exponent-sign")
                | ("page-layout-properties", "margin-gutter")
                | ("paragraph-properties", "contextual-spacing")
                | ("text-properties", "hyphenation-no-caps" | "opacity")
        ),
        FORM_NS => {
            node.is(OFFICE_NS, "forms") && matches!(local, "automatic-focus" | "apply-design-mode")
        }
        _ => false,
    }
}

// These subtrees are layout/style definitions, not document body. The converter already
// ignores their layout while consuming text/list styles separately. All descendants are
// still checked for active content, namespaces and XML/resource limits.
pub(super) fn layout_definition(node: &XmlNode, parent: &XmlNode) -> bool {
    ((parent.is(OFFICE_NS, "document-styles") || parent.is(OFFICE_NS, "master-styles"))
        && node.is(DRAW_NS, "layer-set"))
        || (parent.is(OFFICE_NS, "font-face-decls") && node.is(STYLE_NS, "font-face"))
        || (parent.is(OFFICE_NS, "master-styles")
            && node.name.ns == STYLE_NS
            && matches!(node.name.local.as_str(), "master-page" | "handout-master"))
        || ((parent.is(OFFICE_NS, "styles") || parent.is(OFFICE_NS, "automatic-styles"))
            && ((node.name.ns == STYLE_NS
                && matches!(
                    node.name.local.as_str(),
                    "page-layout" | "default-page-layout" | "presentation-page-layout"
                ))
                || (node.name.ns == DRAW_NS
                    && matches!(
                        node.name.local.as_str(),
                        "gradient"
                            | "fill-image"
                            | "hatch"
                            | "marker"
                            | "stroke-dash"
                            | "layer-set"
                    ))
                || (node.name.ns == TEXT_NS
                    && matches!(
                        node.name.local.as_str(),
                        "outline-style"
                            | "notes-configuration"
                            | "linenumbering-configuration"
                            | "bibliography-configuration"
                    ))))
        || (node.name.ns == STYLE_NS
            && node.name.local.ends_with("-properties")
            && (parent.is(STYLE_NS, "style")
                || parent.is(STYLE_NS, "default-style")
                || (parent.name.ns == TEXT_NS
                    && parent.name.local.starts_with("list-level-style-"))))
        || (node.is(LOEXT_NS, "graphic-properties")
            && (parent.is(STYLE_NS, "default-style") || parent.is(STYLE_NS, "style")))
        || (node.name.ns == NUMBER_NS
            && node.name.local.ends_with("-style")
            && (parent.is(OFFICE_NS, "styles") || parent.is(OFFICE_NS, "automatic-styles")))
        || (node.name.ns == STYLE_NS
            && matches!(
                node.name.local.as_str(),
                "list-level-properties" | "list-level-label-alignment"
            )
            && parent.name.ns == TEXT_NS
            && parent.name.local.starts_with("list-level-"))
        || (parent.is(OFFICE_NS, "meta") && node.is(META_NS, "template"))
        || (parent.is(OFFICE_NS, "text")
            && node.name.ns == TEXT_NS
            && matches!(
                node.name.local.as_str(),
                "sequence-decls" | "user-field-decls" | "variable-decls"
            ))
        || (parent.is(OFFICE_NS, "spreadsheet")
            && node.name.ns == TABLE_NS
            && matches!(node.name.local.as_str(), "calculation-settings" | "named-expressions"))
        || (node.is(STYLE_NS, "map") && parent.is(STYLE_NS, "style"))
        || (parent.name.ns == DRAW_NS
            && node.name.ns == DRAW_NS
            && matches!(node.name.local.as_str(), "enhanced-geometry" | "glue-point"))
        || (parent.is(OFFICE_NS, "presentation")
            && node.name.ns == PRESENTATION_NS
            && matches!(node.name.local.as_str(), "settings" | "footer-decl" | "date-time-decl"))
        || (node.is(DRAW_NS, "page-thumbnail") && parent.is(PRESENTATION_NS, "notes"))
        || (node.is(TEXT_NS, "tracked-changes")
            && parent.is(OFFICE_NS, "text")
            && node.children().next().is_none()
            && node.text().trim().is_empty())
}

pub(super) fn layout_metadata_element(node: &XmlNode) -> bool {
    let local = node.name.local.as_str();
    match node.name.ns.as_str() {
        STYLE_NS => matches!(
            local,
            "background-image"
                | "columns"
                | "default-page-layout"
                | "drawing-page-properties"
                | "footer"
                | "footer-first"
                | "footer-left"
                | "footer-style"
                | "footnote-sep"
                | "handout-master"
                | "header"
                | "header-first"
                | "header-left"
                | "header-style"
                | "header-footer-properties"
                | "list-level-label-alignment"
                | "list-level-properties"
                | "map"
                | "presentation-page-layout"
                | "region-left"
                | "region-right"
                | "ruby-properties"
                | "section-properties"
                | "tab-stop"
                | "tab-stops"
        ),
        DRAW_NS => matches!(
            local,
            "fill-image"
                | "gradient"
                | "hatch"
                | "layer"
                | "layer-set"
                | "marker"
                | "stroke-dash"
                | "page-thumbnail"
                | "enhanced-geometry"
                | "equation"
                | "handle"
                | "glue-point"
        ),
        TEXT_NS => matches!(
            local,
            "outline-style"
                | "outline-level-style"
                | "notes-configuration"
                | "linenumbering-configuration"
                | "linenumbering-separator"
                | "bibliography-configuration"
                | "sort-key"
                | "sequence-decls"
                | "sequence-decl"
                | "user-field-decls"
                | "user-field-decl"
                | "variable-decls"
                | "variable-decl"
                | "tracked-changes"
        ),
        NUMBER_NS => matches!(
            local,
            "am-pm" | "boolean" | "boolean-style" | "scientific-number" | "text-content"
        ),
        TABLE_NS => matches!(
            local,
            "calculation-settings"
                | "iteration"
                | "null-date"
                | "named-expressions"
                | "named-range"
        ),
        PRESENTATION_NS => matches!(
            local,
            "date-time"
                | "date-time-decl"
                | "footer"
                | "footer-decl"
                | "header"
                | "placeholder"
                | "settings"
        ),
        LOEXT_NS => matches!(local, "graphic-properties" | "fill-character" | "text"),
        META_NS => local == "template",
        _ => false,
    }
}

pub(super) fn standard_attribute(node: &XmlNode, attr: &Attr) -> bool {
    let local = attr.name.local.as_str();
    match attr.name.ns.as_str() {
        STYLE_NS if node.is(DRAW_NS, "frame") => matches!(local, "rel-width" | "rel-height"),
        OFFICE_NS if node.is(DRAW_NS, "a") || node.is(TEXT_NS, "a") => local == "name",
        super::model::XML_NS if node.name.ns == DRAW_NS => local == "id",
        STYLE_NS if cached_text_field(node) => local == "data-style-name",
        STYLE_NS if node.is(STYLE_NS, "style") => matches!(
            local,
            "auto-update"
                | "class"
                | "data-style-name"
                | "default-outline-level"
                | "list-style-name"
                | "master-page-name"
                | "next-style-name"
        ),
        STYLE_NS if node.name.ns == TEXT_NS && node.name.local.starts_with("list-level-style-") => {
            matches!(local, "num-letter-sync" | "num-suffix" | "num-prefix")
        }
        TEXT_NS if node.name.ns == TEXT_NS && node.name.local.starts_with("list-level-style-") => {
            local == "style-name"
        }
        TEXT_NS if node.is(TEXT_NS, "list-style") => local == "consecutive-numbering",
        TEXT_NS if node.is(OFFICE_NS, "text") => local == "use-soft-page-breaks",
        TEXT_NS if node.is(TEXT_NS, "p") => local == "cond-style-name",
        TEXT_NS if node.is(TEXT_NS, "conditional-text") => matches!(
            local,
            "condition" | "current-value" | "string-value-if-false" | "string-value-if-true"
        ),
        TEXT_NS if cached_text_field(node) => {
            matches!(
                local,
                "name"
                    | "display"
                    | "description"
                    | "select-page"
                    | "date-value"
                    | "time-value"
                    | "ref-name"
                    | "reference-format"
                    | "formula"
            )
        }
        TEXT_NS if node.is(TEXT_NS, "h") => {
            matches!(local, "is-list-header" | "restart-numbering" | "start-value")
        }
        TEXT_NS if node.name.ns == DRAW_NS => local == "anchor-type",
        TABLE_NS if node.is(TABLE_NS, "table-column") => local == "default-cell-style-name",
        TABLE_NS if node.is(TABLE_NS, "table") => matches!(local, "print" | "is-sub-table"),
        DRAW_NS if node.name.ns == DRAW_NS => matches!(
            local,
            "master-page-name"
                | "text-style-name"
                | "z-index"
                | "layer"
                | "type"
                | "corner-radius"
                | "mime-type"
                | "start-angle"
                | "end-angle"
                | "kind"
                | "page-number"
        ),
        DRAW_NS if node.is(PRESENTATION_NS, "notes") => local == "style-name",
        SVG_NS if node.is(DRAW_NS, "connector") || node.is(DRAW_NS, "line") => {
            matches!(local, "x1" | "x2" | "y1" | "y2")
        }
        PRESENTATION_NS if node.name.ns == DRAW_NS => matches!(
            local,
            "presentation-page-layout-name"
                | "use-date-time-name"
                | "use-footer-name"
                | "placeholder"
                | "user-transformed"
        ),
        META_NS if node.is(META_NS, "document-statistic") => matches!(
            local,
            "cell-count"
                | "character-count"
                | "image-count"
                | "non-whitespace-character-count"
                | "object-count"
                | "page-count"
                | "paragraph-count"
                | "row-count"
                | "table-count"
                | "word-count"
        ),
        _ => false,
    }
}

pub(super) fn cached_text_field(node: &XmlNode) -> bool {
    node.name.ns == TEXT_NS
        && matches!(
            node.name.local.as_str(),
            "user-field-get"
                | "user-field-input"
                | "page-number"
                | "date"
                | "time"
                | "reference-ref"
                | "bookmark-ref"
                | "sequence-ref"
                | "file-name"
                | "sequence"
                | "variable-set"
        )
}

pub(super) fn style_attribute(node: &XmlNode, attr: &Attr) -> bool {
    // Formatting properties do not drive resource access or structural interpretation.
    node.name.ns == STYLE_NS
        && node.name.local.ends_with("-properties")
        && matches!(
            attr.name.ns.as_str(),
            STYLE_NS | FO_NS | DRAW_NS | SVG_NS | TABLE_NS | TEXT_NS | PRESENTATION_NS
        )
}
