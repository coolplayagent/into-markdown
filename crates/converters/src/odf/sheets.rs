use crate::odf::model::{ParseState, TABLE_NS, limit};
use crate::odf::package::Package;
use crate::odf::semantic::{parse_drawing, parse_table};
use crate::odf::styles::StyleMap;
use crate::odf::text::ParseMode;
use crate::odf::xml::XmlNode;
use into_markdown_core::{
    Block, ConversionError, ConversionOptions, ErrorPolicy, ExecutionContext, ResourceFailureScope,
    ResourceLimitSource, ResourceRecoveryAction, ResourceRecoveryBoundary, ResourceUnitKind,
    SourceLocator, recovery_diagnostic,
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
    let observed = u64::try_from(tables.len()).unwrap_or(u64::MAX);
    if observed > u64::from(options.limits.max_pages) && options.error_policy == ErrorPolicy::Strict
    {
        return Err(limit(
            "max_pages",
            format!("{} worksheets > {}", tables.len(), options.limits.max_pages),
        ));
    }
    if observed > u64::from(options.limits.max_pages) {
        let first_omitted = usize::try_from(options.limits.max_pages).unwrap_or(usize::MAX);
        let name = tables
            .get(first_omitted)
            .and_then(|table| table.attr(TABLE_NS, "name"))
            .filter(|value| !value.is_empty())
            .map_or_else(
                || format!("Sheet {}", options.limits.max_pages.saturating_add(1)),
                str::to_owned,
            );
        let locator = SourceLocator {
            sheet: Some(name),
            part: Some("content.xml".into()),
            ..Default::default()
        };
        let error = limit(
            "max_pages",
            format!("{} worksheets > {}", tables.len(), options.limits.max_pages),
        );
        let facts = ResourceRecoveryBoundary {
            scope: ResourceFailureScope::Sequence,
            unit: ResourceUnitKind::Sheet,
            locator: Some(&locator),
            rollback_complete: true,
            fallback_retained: true,
            committed_units: u64::from(options.limits.max_pages),
            omitted_units: observed - u64::from(options.limits.max_pages),
            limit_source: ResourceLimitSource::Explicit,
            precise_required: Some(observed),
            raised_limit: None,
        };
        let diagnostic = recovery_diagnostic(
            &error,
            ResourceRecoveryAction::TruncateSequence,
            facts,
            Some(u64::from(options.limits.max_pages)),
        )
        .ok_or(error)?;
        state.diagnostics.push(diagnostic);
    }
    for (index, table) in tables.into_iter().take(options.limits.max_pages as usize).enumerate() {
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
        let empty = matches!(&table_node.block, Block::Table { rows, .. } if rows.iter().all(|row| row.cells.is_empty()));
        let mut blocks = if empty { vec![] } else { vec![table_node] };
        for shapes in table.children().filter(|child| child.is(TABLE_NS, "shapes")) {
            for drawing in shapes.children() {
                blocks.extend(parse_drawing(
                    drawing,
                    styles,
                    package,
                    state,
                    options,
                    context,
                    &locator,
                    ParseMode::Text,
                )?);
            }
        }
        let sheet = state.node(Block::Sheet { name, blocks }, locator)?;
        state.document.blocks.push(sheet);
    }
    Ok(())
}
