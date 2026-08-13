use crate::odf::model::{OFFICE_NS, ParseState, malformed, part_locator};
use crate::odf::package::Package;
use crate::odf::presentation::parse_presentation;
use crate::odf::semantic::parse_blocks;
use crate::odf::sheets::parse_spreadsheet;
use crate::odf::styles::StyleMap;
use crate::odf::text::ParseMode;
use crate::odf::xml::{XmlNode, only_child};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext, InputFormat};

pub(super) fn parse_content(
    root: &XmlNode,
    format: InputFormat,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if !root.is(OFFICE_NS, "document-content") {
        return Err(malformed(Some("content.xml"), "unexpected content root"));
    }
    let version = root.attr(OFFICE_NS, "version").unwrap_or(&package.odf_version);
    if !matches!(version, "1.2" | "1.3") {
        return Err(malformed(Some("content.xml"), "unsupported ODF content version"));
    }
    state.document.metadata.properties.insert("odf.version".into(), version.into());
    for child in root.children() {
        if child.is(OFFICE_NS, "scripts") {
            return Err(malformed(Some("content.xml"), "office:scripts is forbidden"));
        }
        if !(child.is(OFFICE_NS, "font-face-decls")
            || child.is(OFFICE_NS, "automatic-styles")
            || child.is(OFFICE_NS, "body"))
        {
            return Err(malformed(
                Some("content.xml"),
                format!(
                    "unsupported document-content child {}:{}",
                    child.name.ns, child.name.local
                ),
            ));
        }
    }
    let body = only_child(root, OFFICE_NS, "body", "content.xml")?;
    let (local, expected) = match format {
        InputFormat::Odt => ("text", InputFormat::Odt),
        InputFormat::Ods => ("spreadsheet", InputFormat::Ods),
        InputFormat::Odp => ("presentation", InputFormat::Odp),
        _ => {
            return Err(ConversionError::Internal {
                detail: "non-ODF content dispatched to ODF parser".into(),
            });
        }
    };
    let payload = only_child(body, OFFICE_NS, local, "content.xml")?;
    if body.children().any(|child| !std::ptr::eq(child, payload)) {
        return Err(malformed(
            Some("content.xml"),
            "office:body contains a second semantic payload",
        ));
    }
    match expected {
        InputFormat::Odt => {
            let locator = part_locator("content.xml");
            state.document.blocks = parse_blocks(
                payload,
                styles,
                package,
                state,
                options,
                context,
                &locator,
                ParseMode::Text,
                1,
            )?;
        }
        InputFormat::Ods => parse_spreadsheet(payload, styles, package, state, options, context)?,
        InputFormat::Odp => parse_presentation(payload, styles, package, state, options, context)?,
        _ => unreachable!(),
    }
    Ok(())
}
