use crate::odf::model::{DRAW_NS, PRESENTATION_NS, ParseState, limit};
use crate::odf::package::Package;
use crate::odf::semantic::{parse_blocks, parse_drawing};
use crate::odf::styles::StyleMap;
use crate::odf::text::ParseMode;
use crate::odf::xml::XmlNode;
use into_markdown_core::{
    Block, ConversionError, ConversionOptions, ExecutionContext, Inline, InlineMark, SourceLocator,
};

pub(super) fn parse_presentation(
    payload: &XmlNode,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let pages: Vec<_> = payload.children().filter(|child| child.is(DRAW_NS, "page")).collect();
    if u32::try_from(pages.len()).unwrap_or(u32::MAX) > options.limits.max_pages {
        return Err(limit(
            "max_pages",
            format!("{} slides > {}", pages.len(), options.limits.max_pages),
        ));
    }
    for (index, page) in pages.into_iter().enumerate() {
        context.checkpoint()?;
        let number = u32::try_from(index + 1)
            .map_err(|_| limit("max_pages", "slide number cannot be represented"))?;
        let locator = SourceLocator {
            slide: Some(number),
            part: Some("content.xml".into()),
            ..SourceLocator::default()
        };
        let mut title = None;
        let mut blocks = Vec::new();
        for shape in page.children() {
            context.checkpoint()?;
            if shape.is(PRESENTATION_NS, "notes") {
                let mut note_blocks = parse_blocks(
                    shape,
                    styles,
                    package,
                    state,
                    options,
                    context,
                    &locator,
                    ParseMode::Notes,
                    1,
                )?;
                if !note_blocks.is_empty() {
                    state.add_inlines(1)?;
                    blocks.push(state.node(
                        Block::Paragraph(vec![Inline::Text {
                            value: "Speaker notes".into(),
                            marks: vec![InlineMark::Bold],
                        }]),
                        locator.clone(),
                    )?);
                    blocks.append(&mut note_blocks);
                }
                continue;
            }
            let is_title =
                shape.attr(PRESENTATION_NS, "class").is_some_and(|value| value == "title");
            if is_title {
                let value = shape.text().trim().to_owned();
                if !value.is_empty() && title.is_none() {
                    title = Some(value);
                    for child in shape.children().filter(|child| !child.is(DRAW_NS, "text-box")) {
                        blocks.extend(parse_drawing(
                            child,
                            styles,
                            package,
                            state,
                            options,
                            context,
                            &locator,
                            ParseMode::Slide,
                        )?);
                    }
                    continue;
                }
            }
            blocks.extend(parse_drawing(
                shape,
                styles,
                package,
                state,
                options,
                context,
                &locator,
                ParseMode::Slide,
            )?);
        }
        let slide = state.node(Block::Slide { number, title, blocks }, locator)?;
        state.document.blocks.push(slide);
    }
    Ok(())
}
