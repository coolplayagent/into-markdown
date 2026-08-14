use crate::odf::annotations::annotation_text;
use crate::odf::model::{DRAW_NS, OFFICE_NS, ParseState, TEXT_NS, XLINK_NS, limit, malformed};
use crate::odf::styles::{StyleMap, style_marks};
use crate::odf::tables::parse_repeat;
use crate::odf::xml::{XmlContent, XmlNode, bounded_text};
use into_markdown_core::{
    Block, ConversionError, ConversionOptions, Inline, InlineMark, SourceLocator,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ParseMode {
    Text,
    Slide,
    Notes,
    Cell,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn parse_inlines(
    node: &XmlNode,
    styles: &StyleMap,
    state: &mut ParseState,
    options: &ConversionOptions,
    locator: &SourceLocator,
    marks: &[InlineMark],
) -> Result<Vec<Inline>, ConversionError> {
    let mut output = Vec::new();
    for value in &node.content {
        match value {
            XmlContent::Text(text) => push_text(&mut output, text, marks, state, options)?,
            XmlContent::Node(child) if child.is(TEXT_NS, "span") => {
                let marks = style_marks(styles, "text", child.attr(TEXT_NS, "style-name"), marks);
                output.extend(parse_inlines(child, styles, state, options, locator, &marks)?);
            }
            XmlContent::Node(child) if child.is(TEXT_NS, "a") => {
                let href = child
                    .attr(XLINK_NS, "href")
                    .ok_or_else(|| malformed(Some("content.xml"), "text:a lacks xlink:href"))?;
                validate_link(href)?;
                let content = parse_inlines(child, styles, state, options, locator, marks)?;
                state.add_inlines(1)?;
                output.push(Inline::Link { target: href.to_owned(), content });
            }
            XmlContent::Node(child) if child.is(TEXT_NS, "s") => {
                let repeat = parse_repeat(
                    child.attr(TEXT_NS, "c"),
                    "text:c",
                    options.limits.max_field_bytes,
                )?;
                let repeat = usize::try_from(repeat)
                    .map_err(|_| limit("max_field_bytes", "space repeat cannot be represented"))?;
                push_text(&mut output, &" ".repeat(repeat), marks, state, options)?;
            }
            XmlContent::Node(child) if child.is(TEXT_NS, "tab") => {
                push_text(&mut output, "\t", marks, state, options)?;
            }
            XmlContent::Node(child) if child.is(TEXT_NS, "line-break") => {
                state.add_inlines(1)?;
                output.push(Inline::LineBreak);
            }
            XmlContent::Node(child) if child.is(TEXT_NS, "note") => {
                let id = child.attr(TEXT_NS, "id").unwrap_or("note");
                let body = child
                    .children()
                    .find(|value| value.is(TEXT_NS, "note-body"))
                    .ok_or_else(|| malformed(Some("content.xml"), "text:note lacks note-body"))?;
                let note_text = bounded_text(body, options, "content.xml")?;
                state.add_inlines(1)?;
                let paragraph = state.node(
                    Block::Paragraph(vec![Inline::Text { value: note_text, marks: vec![] }]),
                    locator.clone(),
                )?;
                let blocks = vec![paragraph];
                let deferred = state
                    .node(Block::Footnote { label: id.to_owned(), blocks }, locator.clone())?;
                state.deferred.push(deferred);
                state.add_inlines(1)?;
                output.push(Inline::FootnoteReference(id.to_owned()));
            }
            XmlContent::Node(child) if child.is(OFFICE_NS, "annotation") => {
                let text = annotation_text(child, options)?;
                push_text(&mut output, &format!("[{text}]"), marks, state, options)?;
                state.warning(
                    "odf.annotation",
                    "ODF annotation was preserved inline",
                    locator.clone(),
                );
            }
            XmlContent::Node(child) if child.is(OFFICE_NS, "annotation-end") => {}
            XmlContent::Node(child)
                if child.is(TEXT_NS, "bookmark")
                    || child.is(TEXT_NS, "bookmark-start")
                    || child.is(TEXT_NS, "bookmark-end")
                    || child.is(TEXT_NS, "reference-mark")
                    || child.is(TEXT_NS, "reference-mark-start")
                    || child.is(TEXT_NS, "reference-mark-end") => {}
            XmlContent::Node(child) if child.name.ns == TEXT_NS || child.name.ns == OFFICE_NS => {
                output.extend(parse_inlines(child, styles, state, options, locator, marks)?);
            }
            XmlContent::Node(child) if child.name.ns == DRAW_NS => {
                // Block images are handled by parse_drawing. Inline frames have no exact IR shape;
                // retain their alternative text without fetching or executing anything.
                let text = child.text();
                if !text.is_empty() {
                    push_text(&mut output, &text, marks, state, options)?;
                }
            }
            XmlContent::Node(child) => {
                return Err(malformed(
                    Some("content.xml"),
                    format!("unsupported inline element {}:{}", child.name.ns, child.name.local),
                ));
            }
        }
    }
    Ok(output)
}

fn push_text(
    output: &mut Vec<Inline>,
    value: &str,
    marks: &[InlineMark],
    state: &mut ParseState,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if value.is_empty() {
        return Ok(());
    }
    if u64::try_from(value.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
        return Err(limit("max_field_bytes", "ODF text field exceeds configured limit"));
    }
    state.add_inlines(1)?;
    output.push(Inline::Text { value: value.to_owned(), marks: marks.to_vec() });
    Ok(())
}

fn validate_link(value: &str) -> Result<(), ConversionError> {
    if value.starts_with('#') && value.len() > 1 {
        return Ok(());
    }
    let parsed = url::Url::parse(value).map_err(|_| {
        malformed(
            Some("content.xml"),
            "relative or invalid hyperlink is outside the safe ODF profile",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https" | "mailto")
        || parsed.username() != ""
        || parsed.password().is_some()
    {
        return Err(malformed(
            Some("content.xml"),
            "hyperlink uses a forbidden scheme or userinfo",
        ));
    }
    Ok(())
}
