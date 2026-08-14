use crate::odf::model::{ParseState, TABLE_NS, limit};
use crate::odf::package::Package;
use crate::odf::semantic::parse_table;
use crate::odf::styles::StyleMap;
use crate::odf::xml::XmlNode;
use into_markdown_core::{
    Block, ConversionError, ConversionOptions, ExecutionContext, SourceLocator,
};

pub(super) fn parse_spreadsheet(
    payload: &XmlNode,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let tables: Vec<_> = payload.children().filter(|child| child.is(TABLE_NS, "table")).collect();
    if u64::try_from(tables.len()).unwrap_or(u64::MAX) > u64::from(options.limits.max_pages) {
        return Err(limit(
            "max_pages",
            format!("{} worksheets > {}", tables.len(), options.limits.max_pages),
        ));
    }
    for (index, table) in tables.into_iter().enumerate() {
        context.checkpoint()?;
        let name = table
            .attr(TABLE_NS, "name")
            .filter(|value| !value.is_empty())
            .map_or_else(|| format!("Sheet {}", index + 1), str::to_owned);
        let locator = SourceLocator {
            sheet: Some(name.clone()),
            part: Some("content.xml".into()),
            ..SourceLocator::default()
        };
        let table_node =
            parse_table(table, styles, package, state, options, context, &locator, Some(&name))?;
        let sheet = state.node(Block::Sheet { name, blocks: vec![table_node] }, locator)?;
        state.document.blocks.push(sheet);
    }
    Ok(())
}
