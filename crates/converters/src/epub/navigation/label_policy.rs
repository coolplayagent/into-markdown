//! Inert EPUB navigation-label content and replacement-text policy.

use super::super::budget::EpubBudget;
use super::super::path::{BasePath, Reference};
use super::super::xml::{self, Attribute, Name};
use into_markdown_core::ConversionError;

pub(super) const XHTML_NS: &[u8] = b"http://www.w3.org/1999/xhtml";
const MATHML_NS: &[u8] = b"http://www.w3.org/1998/Math/MathML";
const SVG_NS: &[u8] = b"http://www.w3.org/2000/svg";
const XLINK_NS: &[u8] = b"http://www.w3.org/1999/xlink";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UriSyntax {
    Single,
    WhitespaceList,
    Srcset,
    RdfaPrefix,
    RdfaTerms,
}

#[derive(Clone, Copy, Debug)]
struct UriAttributeRule {
    element_namespace: Option<&'static [u8]>,
    elements: &'static [&'static [u8]],
    attribute_namespace: Option<&'static [u8]>,
    attributes: &'static [&'static [u8]],
    syntax: UriSyntax,
}

// Empty `elements` means that the rule is common to every allowed label element. Keeping the
// element-specific URI surface here makes additions reviewable instead of relying on name
// heuristics to discover standard attributes one at a time.
const URI_ATTRIBUTE_MATRIX: &[UriAttributeRule] = &[
    UriAttributeRule {
        element_namespace: None,
        elements: &[],
        attribute_namespace: None,
        attributes: &[b"about", b"itemid", b"resource", b"vocab"],
        syntax: UriSyntax::Single,
    },
    UriAttributeRule {
        element_namespace: None,
        elements: &[],
        attribute_namespace: None,
        attributes: &[b"itemtype"],
        syntax: UriSyntax::WhitespaceList,
    },
    UriAttributeRule {
        element_namespace: None,
        elements: &[],
        attribute_namespace: None,
        attributes: &[b"prefix"],
        syntax: UriSyntax::RdfaPrefix,
    },
    UriAttributeRule {
        element_namespace: None,
        elements: &[],
        attribute_namespace: None,
        attributes: &[b"datatype", b"itemprop", b"property", b"rel", b"rev", b"role", b"typeof"],
        syntax: UriSyntax::RdfaTerms,
    },
    UriAttributeRule {
        element_namespace: Some(XHTML_NS),
        elements: &[b"a", b"area", b"link"],
        attribute_namespace: None,
        attributes: &[b"href"],
        syntax: UriSyntax::Single,
    },
    UriAttributeRule {
        element_namespace: Some(XHTML_NS),
        elements: &[b"a", b"area"],
        attribute_namespace: None,
        attributes: &[b"ping"],
        syntax: UriSyntax::WhitespaceList,
    },
    UriAttributeRule {
        element_namespace: Some(XHTML_NS),
        elements: &[b"audio", b"img", b"source", b"track", b"video"],
        attribute_namespace: None,
        attributes: &[b"src"],
        syntax: UriSyntax::Single,
    },
    UriAttributeRule {
        element_namespace: Some(XHTML_NS),
        elements: &[b"video"],
        attribute_namespace: None,
        attributes: &[b"poster"],
        syntax: UriSyntax::Single,
    },
    UriAttributeRule {
        element_namespace: Some(XHTML_NS),
        elements: &[b"img", b"source"],
        attribute_namespace: None,
        attributes: &[b"srcset"],
        syntax: UriSyntax::Srcset,
    },
    UriAttributeRule {
        element_namespace: Some(XHTML_NS),
        elements: &[b"link"],
        attribute_namespace: None,
        attributes: &[b"imagesrcset"],
        syntax: UriSyntax::Srcset,
    },
    UriAttributeRule {
        element_namespace: Some(XHTML_NS),
        elements: &[b"img"],
        attribute_namespace: None,
        attributes: &[b"longdesc", b"lowsrc", b"usemap"],
        syntax: UriSyntax::Single,
    },
    UriAttributeRule {
        element_namespace: Some(XHTML_NS),
        elements: &[b"del", b"ins", b"q"],
        attribute_namespace: None,
        attributes: &[b"cite"],
        syntax: UriSyntax::Single,
    },
    UriAttributeRule {
        element_namespace: Some(MATHML_NS),
        elements: &[b"math"],
        attribute_namespace: None,
        attributes: &[b"altimg", b"cdgroup"],
        syntax: UriSyntax::Single,
    },
    UriAttributeRule {
        element_namespace: None,
        elements: &[],
        attribute_namespace: Some(XLINK_NS),
        attributes: &[b"arcrole", b"href", b"role"],
        syntax: UriSyntax::Single,
    },
];

const INERT_XLINK_ATTRIBUTES: &[&[u8]] = &[b"actuate", b"show", b"title", b"type"];

pub(super) fn is_known_namespace(namespace: Option<&[u8]>) -> bool {
    matches!(namespace, None | Some(XHTML_NS | MATHML_NS | SVG_NS))
}

pub(super) fn is_label_container(name: &Name) -> bool {
    name.matches(Some(XHTML_NS), b"a") || name.matches(Some(XHTML_NS), b"span")
}

pub(super) fn is_heading(name: &Name) -> bool {
    name.namespace.as_deref() == Some(XHTML_NS)
        && matches!(name.local.as_slice(), b"h1" | b"h2" | b"h3" | b"h4" | b"h5" | b"h6")
}

pub(super) fn is_label_content(name: &Name) -> bool {
    is_label_container(name)
        || is_heading(name)
        || is_html_phrasing(name)
        || is_html_auxiliary(name)
        || is_mathml(name)
        || is_svg(name)
}

pub(super) fn is_child_allowed(parent: &Name, child: &Name) -> bool {
    match parent.namespace.as_deref() {
        Some(XHTML_NS) => html_child_allowed(parent, child),
        Some(MATHML_NS) => mathml_child_allowed(parent, child),
        Some(SVG_NS) => svg_child_allowed(parent, child),
        _ => false,
    }
}

pub(super) fn replacement_text<'a>(name: &Name, attributes: &'a [Attribute]) -> Option<&'a str> {
    is_embedded(name).then(|| {
        [b"alt".as_slice(), b"alttext", b"title"]
            .into_iter()
            .filter_map(|local| xml::optional(attributes, None, local))
            .find(|value| !value.trim().is_empty())
    })?
}

pub(super) fn is_embedded(name: &Name) -> bool {
    name.matches(Some(MATHML_NS), b"math")
        || name.matches(Some(SVG_NS), b"svg")
        || name.namespace.as_deref() == Some(XHTML_NS)
            && matches!(name.local.as_slice(), b"audio" | b"canvas" | b"img" | b"video")
}

pub(super) fn validate_element(
    name: &Name,
    attributes: &[Attribute],
    base: &BasePath,
    budget: &mut EpubBudget<'_>,
) -> Result<(), ConversionError> {
    if !is_label_content(name) {
        return Err(xml::malformed("unsupported element in EPUB navigation label"));
    }
    if attributes.iter().any(|attribute| {
        attribute.namespace.is_none()
            && (attribute.local.starts_with(b"on")
                || matches!(attribute.local.as_slice(), b"srcdoc" | b"style"))
    }) {
        return Err(xml::malformed("active attributes are forbidden in EPUB navigation labels"));
    }
    if name.namespace.as_deref() == Some(XHTML_NS) {
        validate_html_conditions(name, attributes)?;
    }
    validate_url_attributes(name, attributes, base, budget)
}

pub(super) fn validate_child_count(name: &Name, count: usize) -> Result<(), ConversionError> {
    if name.namespace.as_deref() != Some(MATHML_NS) {
        return Ok(());
    }
    let valid = match name.local.as_slice() {
        b"mfrac" | b"mroot" | b"msub" | b"msup" | b"munder" | b"mover" => count == 2,
        b"msubsup" | b"munderover" => count == 3,
        b"msqrt" | b"mrow" | b"mstyle" | b"merror" | b"mpadded" | b"mphantom" | b"mfenced"
        | b"menclose" | b"mtd" => count > 0,
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(xml::malformed("MathML navigation label has an invalid child count"))
    }
}

fn is_html_phrasing(name: &Name) -> bool {
    name.namespace.as_deref() == Some(XHTML_NS)
        && matches!(
            name.local.as_slice(),
            b"a" | b"abbr"
                | b"area"
                | b"audio"
                | b"b"
                | b"bdi"
                | b"bdo"
                | b"br"
                | b"canvas"
                | b"cite"
                | b"code"
                | b"data"
                | b"datalist"
                | b"del"
                | b"dfn"
                | b"em"
                | b"i"
                | b"img"
                | b"ins"
                | b"kbd"
                | b"link"
                | b"map"
                | b"mark"
                | b"meta"
                | b"meter"
                | b"noscript"
                | b"output"
                | b"picture"
                | b"progress"
                | b"q"
                | b"ruby"
                | b"rp"
                | b"rt"
                | b"s"
                | b"samp"
                | b"slot"
                | b"small"
                | b"span"
                | b"strong"
                | b"sub"
                | b"sup"
                | b"time"
                | b"u"
                | b"var"
                | b"video"
                | b"wbr"
        )
}

fn is_html_auxiliary(name: &Name) -> bool {
    name.namespace.as_deref() == Some(XHTML_NS)
        && matches!(name.local.as_slice(), b"option" | b"source" | b"track")
}

fn html_child_allowed(parent: &Name, child: &Name) -> bool {
    let generic = is_html_phrasing(child) && child.local != b"area"
        || child.matches(Some(MATHML_NS), b"math")
        || child.matches(Some(SVG_NS), b"svg");
    match parent.local.as_slice() {
        b"area" | b"br" | b"img" | b"link" | b"meta" | b"source" | b"track" | b"wbr" => false,
        b"picture" => {
            child.matches(Some(XHTML_NS), b"source") || child.matches(Some(XHTML_NS), b"img")
        }
        b"datalist" => generic || child.matches(Some(XHTML_NS), b"option"),
        b"audio" | b"video" => {
            generic
                || child.matches(Some(XHTML_NS), b"source")
                || child.matches(Some(XHTML_NS), b"track")
        }
        b"map" => generic || child.matches(Some(XHTML_NS), b"area"),
        b"rt" | b"option" => is_text_level_html(child),
        _ => generic,
    }
}

fn is_text_level_html(name: &Name) -> bool {
    is_html_phrasing(name)
        && !matches!(
            name.local.as_slice(),
            b"area" | b"audio" | b"canvas" | b"datalist" | b"img" | b"map" | b"picture" | b"video"
        )
}

fn is_mathml(name: &Name) -> bool {
    name.namespace.as_deref() == Some(MATHML_NS)
        && matches!(
            name.local.as_slice(),
            b"math"
                | b"mrow"
                | b"mi"
                | b"mn"
                | b"mo"
                | b"mtext"
                | b"ms"
                | b"mspace"
                | b"mfrac"
                | b"msqrt"
                | b"mroot"
                | b"mstyle"
                | b"merror"
                | b"mpadded"
                | b"mphantom"
                | b"mfenced"
                | b"menclose"
                | b"msub"
                | b"msup"
                | b"msubsup"
                | b"munder"
                | b"mover"
                | b"munderover"
                | b"mtable"
                | b"mtr"
                | b"mtd"
                | b"semantics"
                | b"annotation"
        )
}

fn mathml_child_allowed(parent: &Name, child: &Name) -> bool {
    if !is_mathml(child) {
        return false;
    }
    match parent.local.as_slice() {
        b"mi" | b"mn" | b"mo" | b"mtext" | b"ms" | b"mspace" | b"annotation" => false,
        b"mtable" => child.local == b"mtr",
        b"mtr" => child.local == b"mtd",
        b"math" => child.local != b"math",
        _ => !matches!(child.local.as_slice(), b"math" | b"mtr"),
    }
}

fn is_svg(name: &Name) -> bool {
    name.namespace.as_deref() == Some(SVG_NS)
        && matches!(
            name.local.as_slice(),
            b"svg"
                | b"title"
                | b"desc"
                | b"g"
                | b"defs"
                | b"metadata"
                | b"circle"
                | b"ellipse"
                | b"line"
                | b"path"
                | b"polygon"
                | b"polyline"
                | b"rect"
                | b"text"
                | b"tspan"
        )
}

fn svg_child_allowed(parent: &Name, child: &Name) -> bool {
    if !is_svg(child) || child.local == b"svg" {
        return false;
    }
    match parent.local.as_slice() {
        b"svg" | b"g" | b"defs" => true,
        b"text" | b"tspan" => child.local == b"tspan",
        _ => false,
    }
}

fn validate_html_conditions(name: &Name, attributes: &[Attribute]) -> Result<(), ConversionError> {
    if matches!(name.local.as_slice(), b"input" | b"select" | b"label") {
        return Err(xml::malformed("interactive HTML is forbidden in EPUB navigation labels"));
    }
    if matches!(name.local.as_slice(), b"audio" | b"video")
        && has_any(attributes, &[b"autoplay".as_slice(), b"controls"])
    {
        return Err(xml::malformed("interactive media is forbidden in EPUB navigation labels"));
    }
    if name.local == b"img" && has_any(attributes, &[b"ismap".as_slice(), b"usemap"]) {
        return Err(xml::malformed("interactive images are forbidden in EPUB navigation labels"));
    }
    if matches!(name.local.as_slice(), b"link" | b"meta")
        && xml::optional(attributes, None, b"itemprop").is_none()
    {
        return Err(xml::malformed("navigation label metadata requires itemprop"));
    }
    Ok(())
}

fn has_any(attributes: &[Attribute], names: &[&[u8]]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.namespace.is_none() && names.contains(&attribute.local.as_slice())
    })
}

fn validate_url_attributes(
    name: &Name,
    attributes: &[Attribute],
    base: &BasePath,
    budget: &mut EpubBudget<'_>,
) -> Result<(), ConversionError> {
    for attribute in attributes {
        let _scratch = budget.navigation_url_value(attribute.value.len())?;
        if contains_ascii_case_insensitive(attribute.value.as_bytes(), b"url(") {
            return Err(xml::malformed("CSS-style URLs are forbidden in EPUB navigation labels"));
        }
        if attribute.namespace.as_deref() == Some(xml::XML_NS) && attribute.local == b"base" {
            // The caller already applied and confined xml:base before label validation.
        } else if let Some(syntax) = uri_syntax(name, attribute) {
            match syntax {
                UriSyntax::Single => validate_internal(base, &attribute.value, budget)?,
                UriSyntax::WhitespaceList => {
                    for value in attribute.value.split_whitespace() {
                        validate_internal(base, value, budget)?;
                    }
                }
                UriSyntax::Srcset => validate_srcset(base, &attribute.value, budget)?,
                UriSyntax::RdfaPrefix => validate_rdfa_prefix(base, &attribute.value, budget)?,
                UriSyntax::RdfaTerms => validate_rdfa_terms(base, &attribute.value, budget)?,
            }
        } else if attribute.namespace.as_deref() == Some(XLINK_NS) {
            if !INERT_XLINK_ATTRIBUTES.contains(&attribute.local.as_slice()) {
                return Err(xml::malformed(
                    "unknown XLink attribute is forbidden in EPUB navigation labels",
                ));
            }
        } else if is_declared_uri_attribute(attribute)
            || potentially_url_bearing_name(&attribute.local)
        {
            return Err(xml::malformed(
                "unknown URL-like attribute is forbidden in EPUB navigation labels",
            ));
        }
    }
    Ok(())
}

fn uri_syntax(name: &Name, attribute: &Attribute) -> Option<UriSyntax> {
    URI_ATTRIBUTE_MATRIX.iter().find_map(|rule| {
        let element_matches =
            rule.element_namespace.is_none() || rule.element_namespace == name.namespace.as_deref();
        let local_matches =
            rule.elements.is_empty() || rule.elements.contains(&name.local.as_slice());
        (element_matches
            && local_matches
            && rule.attribute_namespace == attribute.namespace.as_deref()
            && rule.attributes.contains(&attribute.local.as_slice()))
        .then_some(rule.syntax)
    })
}

fn is_declared_uri_attribute(attribute: &Attribute) -> bool {
    URI_ATTRIBUTE_MATRIX.iter().any(|rule| {
        rule.attribute_namespace == attribute.namespace.as_deref()
            && rule.attributes.contains(&attribute.local.as_slice())
    })
}

fn validate_srcset(
    base: &BasePath,
    value: &str,
    budget: &mut EpubBudget<'_>,
) -> Result<(), ConversionError> {
    let mut count = 0_usize;
    for candidate in value.split(',') {
        let url = candidate
            .split_whitespace()
            .next()
            .ok_or_else(|| xml::malformed("empty navigation srcset candidate"))?;
        validate_internal(base, url, budget)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| xml::malformed("navigation srcset candidate count overflowed"))?;
    }
    if count == 0 {
        return Err(xml::malformed("empty navigation srcset"));
    }
    Ok(())
}

fn validate_rdfa_prefix(
    base: &BasePath,
    value: &str,
    budget: &mut EpubBudget<'_>,
) -> Result<(), ConversionError> {
    let mut tokens = value.split_whitespace();
    let mut mappings = 0_usize;
    while let Some(prefix) = tokens.next() {
        budget.navigation_url_token(prefix.len())?;
        if !prefix.ends_with(':')
            || prefix.len() == 1
            || !prefix[..prefix.len() - 1]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(xml::malformed("invalid RDFa prefix mapping in EPUB navigation label"));
        }
        let iri = tokens.next().ok_or_else(|| xml::malformed("RDFa prefix mapping has no IRI"))?;
        validate_internal(base, iri, budget)?;
        mappings = mappings
            .checked_add(1)
            .ok_or_else(|| xml::malformed("RDFa prefix mapping count overflowed"))?;
    }
    if mappings == 0 {
        return Err(xml::malformed("empty RDFa prefix mapping in EPUB navigation label"));
    }
    Ok(())
}

fn validate_rdfa_terms(
    base: &BasePath,
    value: &str,
    budget: &mut EpubBudget<'_>,
) -> Result<(), ConversionError> {
    let mut count = 0_usize;
    for token in value.split_whitespace() {
        budget.navigation_url_token(token.len())?;
        if token.contains(':') || token.starts_with(['/', '.', '#']) {
            validate_internal_counted(base, token)?;
        }
        count =
            count.checked_add(1).ok_or_else(|| xml::malformed("RDFa token count overflowed"))?;
    }
    if count == 0 {
        return Err(xml::malformed("empty RDFa token list in EPUB navigation label"));
    }
    Ok(())
}

fn validate_internal(
    base: &BasePath,
    value: &str,
    budget: &mut EpubBudget<'_>,
) -> Result<(), ConversionError> {
    budget.navigation_url_token(value.len())?;
    validate_internal_counted(base, value)
}

fn validate_internal_counted(base: &BasePath, value: &str) -> Result<(), ConversionError> {
    if matches!(base.resolve(value)?, Reference::External(_)) {
        return Err(xml::malformed("navigation label URL must remain inside the EPUB container"));
    }
    Ok(())
}

fn contains_ascii_case_insensitive(value: &[u8], needle: &[u8]) -> bool {
    value.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

fn potentially_url_bearing_name(local: &[u8]) -> bool {
    [
        b"href".as_slice(),
        b"src",
        b"url",
        b"uri",
        b"iri",
        b"action",
        b"resource",
        b"vocab",
        b"location",
        b"schema",
    ]
    .into_iter()
    .any(|part| local.windows(part.len()).any(|window| window.eq_ignore_ascii_case(part)))
}

#[cfg(test)]
#[path = "label_policy_tests.rs"]
mod tests;
