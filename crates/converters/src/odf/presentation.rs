use crate::odf::model::{DRAW_NS, PRESENTATION_NS, ParseState, limit};
use crate::odf::package::Package;
use crate::odf::semantic::{parse_blocks, parse_drawing};
use crate::odf::styles::StyleCatalog;
use crate::odf::text::ParseMode;
use crate::odf::xml::XmlNode;
use into_markdown_core::{
    Block, ConversionError, ConversionOptions, ExecutionContext, Inline, SourceLocator,
};

pub(super) fn parse_presentation(
    payload: &XmlNode,
    catalog: &StyleCatalog<'_>,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let styles = &catalog.text;
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
                append_notes(state, &mut blocks, &mut note_blocks, &locator)?;
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
        blocks.extend(master_text(page, catalog, package, state, options, context, &locator)?);
        let slide = state.node(Block::Slide { number, title, blocks }, locator)?;
        state.document.blocks.push(slide);
    }
    Ok(())
}

fn master_text(
    page: &XmlNode,
    catalog: &StyleCatalog<'_>,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    locator: &SourceLocator,
) -> Result<Vec<into_markdown_core::BlockNode>, ConversionError> {
    let mut blocks = Vec::new();
    if let Some(master) =
        page.attr(DRAW_NS, "master-page-name").and_then(|name| catalog.masters.get(name))
    {
        let locator = SourceLocator { part: Some("styles.xml".into()), ..locator.clone() };
        for frame in master.children().filter(|node| {
            node.is(DRAW_NS, "frame")
                && node
                    .attr(PRESENTATION_NS, "class")
                    .is_some_and(|class| matches!(class, "header" | "footer"))
        }) {
            if !frame.text().trim().is_empty() {
                blocks.extend(parse_drawing(
                    frame,
                    &catalog.text,
                    package,
                    state,
                    options,
                    context,
                    &locator,
                    ParseMode::Slide,
                )?);
                state.warning(
                    "odf.masterText",
                    "Static header/footer text from the referenced slide master was retained",
                    locator.clone(),
                );
            }
        }
    }
    Ok(blocks)
}

fn append_notes(
    state: &mut ParseState,
    blocks: &mut Vec<into_markdown_core::BlockNode>,
    note_blocks: &mut Vec<into_markdown_core::BlockNode>,
    locator: &SourceLocator,
) -> Result<(), ConversionError> {
    if into_markdown_core::speaker_notes::has_visible_content(
        note_blocks,
        into_markdown_core::AssetMode::Extract,
    ) {
        state.add_inlines(1)?;
        let mut heading = state.node(
            Block::Heading {
                level: 3,
                content: vec![Inline::Text { value: "Speaker notes".into(), marks: Vec::new() }],
            },
            locator.clone(),
        )?;
        into_markdown_core::speaker_notes::mark_heading(&mut heading)?;
        for block in note_blocks.iter_mut() {
            into_markdown_core::speaker_notes::mark_body(block)?;
        }
        blocks.push(heading);
        blocks.append(note_blocks);
    }
    Ok(())
}
