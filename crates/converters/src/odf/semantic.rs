use crate::odf::annotations::annotation_text;
use crate::odf::geometry::{Transform, drawing_bounds, parse_transform, transform_bounds};
use crate::odf::images::image_block;
use crate::odf::model::{
    DRAW_NS, OFFICE_NS, PRESENTATION_NS, ParseState, TABLE_NS, TEXT_NS, XML_NS, limit, malformed,
};
use crate::odf::package::Package;
use crate::odf::styles::{StyleMap, style_marks};
use crate::odf::tables::{
    cell_has_content, cell_semantic_value, parse_odf_bool, parse_repeat, parse_span,
};
use crate::odf::text::{ParseMode, parse_inlines};
use crate::odf::xml::XmlNode;
use into_markdown_core::{
    Block, BlockNode, Cell, CellRef, ConversionError, ConversionOptions, ExecutionContext, Inline,
    ListItem, ListKind, MAX_TABLE_COLUMNS, SourceLocator, TableRow,
};
use std::collections::BTreeSet;

#[allow(clippy::too_many_arguments)]
pub(super) fn parse_blocks(
    container: &XmlNode,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    locator: &SourceLocator,
    mode: ParseMode,
    list_level: u8,
) -> Result<Vec<BlockNode>, ConversionError> {
    let mut blocks = Vec::new();
    for (index, child) in container.children().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        if child.is(TEXT_NS, "p") || child.is(TEXT_NS, "h") {
            let level = child
                .attr(TEXT_NS, "outline-level")
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(1)
                .clamp(1, 6);
            let marks = style_marks(styles, "paragraph", child.attr(TEXT_NS, "style-name"), &[]);
            let inlines = parse_inlines(child, styles, state, options, locator, &marks)?;
            if inlines.is_empty() && mode != ParseMode::Cell {
                continue;
            }
            let block = if child.is(TEXT_NS, "h") {
                Block::Heading { level, content: inlines }
            } else {
                Block::Paragraph(inlines)
            };
            blocks.push(state.node(block, locator.clone())?);
        } else if child.is(TEXT_NS, "list") {
            blocks.extend(parse_list(
                child, styles, package, state, options, context, locator, mode, list_level,
            )?);
        } else if child.is(TABLE_NS, "table") {
            blocks
                .push(parse_table(child, styles, package, state, options, context, locator, None)?);
        } else if child.is(DRAW_NS, "frame")
            || child.name.ns == DRAW_NS
                && matches!(
                    child.name.local.as_str(),
                    "custom-shape" | "rect" | "ellipse" | "line" | "connector"
                )
        {
            blocks.extend(parse_drawing(
                child, styles, package, state, options, context, locator, mode,
            )?);
        } else if child.is(TEXT_NS, "section")
            || child.is(DRAW_NS, "text-box")
            || child.is(PRESENTATION_NS, "notes")
        {
            blocks.extend(parse_blocks(
                child, styles, package, state, options, context, locator, mode, list_level,
            )?);
        } else if child.is(OFFICE_NS, "annotation") {
            let text = annotation_text(child, options)?;
            state.warning(
                "odf.annotation",
                "ODF annotation was preserved as visible text",
                locator.clone(),
            );
            state.add_inlines(1)?;
            blocks.push(state.node(
                Block::Paragraph(vec![Inline::Text { value: text, marks: vec![] }]),
                locator.clone(),
            )?);
        } else if child.is(OFFICE_NS, "annotation-end") {
            // Pairing and non-crossing structure was authenticated before semantic parsing.
        } else if child.is(TABLE_NS, "table-column")
            || child.is(TABLE_NS, "table-columns")
            || child.is(TABLE_NS, "table-header-columns")
            || child.is(TABLE_NS, "table-header-rows")
            || child.is(TEXT_NS, "soft-page-break")
            || child.name.ns == OFFICE_NS
                && matches!(child.name.local.as_str(), "forms" | "event-listeners")
        {
            if child.is(OFFICE_NS, "event-listeners") {
                return Err(malformed(
                    Some("content.xml"),
                    "event listeners are outside the safe ODF profile",
                ));
            }
        } else if !child.text().trim().is_empty() {
            return Err(malformed(
                Some("content.xml"),
                format!("unsupported semantic element {}:{}", child.name.ns, child.name.local),
            ));
        }
    }
    Ok(blocks)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn parse_list(
    node: &XmlNode,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    locator: &SourceLocator,
    mode: ParseMode,
    level: u8,
) -> Result<Vec<BlockNode>, ConversionError> {
    let style_name = node
        .attr(TEXT_NS, "style-name")
        .or_else(|| state.active_list_styles.last().map(String::as_str))
        .ok_or_else(|| {
            malformed(
                Some("content.xml"),
                "top-level text:list lacks text:style-name and has no inherited list identity",
            )
        })?
        .to_owned();
    let spec = state
        .list_styles
        .get(&style_name)
        .and_then(|levels| levels.get(&level))
        .cloned()
        .ok_or_else(|| {
            malformed(Some("content.xml"), format!("unknown list style {style_name} level {level}"))
        })?;
    let continue_numbering =
        parse_odf_bool(node.attr(TEXT_NS, "continue-numbering"), "text:continue-numbering")?;
    let continue_list = node.attr(TEXT_NS, "continue-list");
    if continue_numbering && continue_list.is_some() {
        return Err(malformed(
            Some("content.xml"),
            "text:list cannot use both continue-numbering and continue-list",
        ));
    }
    let mut start = if let Some(id) = continue_list {
        let (ordered, next) = state.list_sequences.get(id).copied().ok_or_else(|| {
            malformed(Some("content.xml"), format!("continue-list target {id} is unknown"))
        })?;
        if ordered != spec.ordered {
            return Err(malformed(Some("content.xml"), "continue-list changes list kind"));
        }
        next
    } else if continue_numbering {
        let (ordered, next) =
            state.last_list_sequences.get(&(style_name.clone(), level)).copied().ok_or_else(
                || malformed(Some("content.xml"), "no prior list sequence to continue"),
            )?;
        if ordered != spec.ordered {
            return Err(malformed(Some("content.xml"), "continued list changes list kind"));
        }
        next
    } else {
        spec.start
    };
    state.active_list_styles.push(style_name.clone());
    let parsed = (|| {
        let mut headers = Vec::new();
        let mut items = Vec::new();
        let mut sequence_items = 0_u64;
        let mut saw_item = false;
        for child in node.children() {
            context.checkpoint()?;
            if !child.is(TEXT_NS, "list-item") && !child.is(TEXT_NS, "list-header") {
                return Err(malformed(Some("content.xml"), "text:list contains an invalid child"));
            }
            let next_level = level
                .checked_add(1)
                .ok_or_else(|| limit("max_nesting_depth", "list nesting level overflow"))?;
            if child.is(TEXT_NS, "list-header") {
                if saw_item {
                    return Err(malformed(
                        Some("content.xml"),
                        "text:list-header must precede all marked list items",
                    ));
                }
                // list-header is prefix content with no marker; keep it before the IR List rather
                // than fabricating a marker-bearing ListItem.
                headers.extend(parse_blocks(
                    child, styles, package, state, options, context, locator, mode, next_level,
                )?);
                continue;
            }
            saw_item = true;
            if let Some(value) = child.attr(TEXT_NS, "start-value") {
                if !spec.ordered || !items.is_empty() {
                    return Err(malformed(
                        Some("content.xml"),
                        "text:list-item start-value is only representable on the first ordered item",
                    ));
                }
                start = value
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0 && u32::try_from(*value).is_ok())
                    .ok_or_else(|| {
                        malformed(Some("content.xml"), "invalid list-item start-value")
                    })?;
            }
            sequence_items = sequence_items
                .checked_add(1)
                .ok_or_else(|| limit("max_field_bytes", "list sequence overflow"))?;
            items.push(ListItem {
                checked: None,
                marker_label: None,
                blocks: parse_blocks(
                    child, styles, package, state, options, context, locator, mode, next_level,
                )?,
            });
        }
        Ok::<_, ConversionError>((headers, items, sequence_items))
    })();
    let popped = state.active_list_styles.pop();
    if popped.as_deref() != Some(style_name.as_str()) {
        return Err(ConversionError::Internal {
            detail: "ODF list style stack lost identity".into(),
        });
    }
    let (mut blocks, items, sequence_items) = parsed?;
    let next = start
        .checked_add(sequence_items)
        .ok_or_else(|| limit("max_field_bytes", "list sequence overflow"))?;
    state.last_list_sequences.insert((style_name.clone(), level), (spec.ordered, next));
    if let Some(id) = node.attr(XML_NS, "id")
        && state.list_sequences.insert(id.to_owned(), (spec.ordered, next)).is_some()
    {
        return Err(malformed(Some("content.xml"), "duplicate xml:id for text:list"));
    }
    blocks.push(state.node(
        Block::List {
            kind: if spec.ordered { ListKind::Ordered } else { ListKind::Bullet },
            start,
            items,
        },
        locator.clone(),
    )?);
    Ok(blocks)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn parse_table(
    node: &XmlNode,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    locator: &SourceLocator,
    sheet: Option<&str>,
) -> Result<BlockNode, ConversionError> {
    let mut declared_columns = 0_u64;
    let mut columns = Vec::new();
    for child in node.children() {
        if child.is(TABLE_NS, "table-column") {
            columns.push(child);
        } else if child.is(TABLE_NS, "table-columns") || child.is(TABLE_NS, "table-header-columns")
        {
            columns.extend(child.children().filter(|column| column.is(TABLE_NS, "table-column")));
        }
    }
    for column in columns {
        declared_columns = declared_columns
            .checked_add(parse_repeat(
                column.attr(TABLE_NS, "number-columns-repeated"),
                "table:number-columns-repeated",
                options.limits.max_table_columns,
            )?)
            .ok_or_else(|| limit("max_table_columns", "ODF declared column count overflow"))?;
    }
    if declared_columns > options.limits.max_table_columns
        || declared_columns > u64::try_from(MAX_TABLE_COLUMNS).unwrap_or(u64::MAX)
    {
        return Err(limit(
            "max_table_columns",
            format!("{declared_columns} exceeds configured/IR column limit"),
        ));
    }
    let row_nodes: Vec<_> = node
        .children()
        .filter(|child| {
            child.is(TABLE_NS, "table-row")
                || child.is(TABLE_NS, "table-header-rows")
                || child.is(TABLE_NS, "table-rows")
        })
        .collect();
    let mut rows = Vec::new();
    let mut represented_widths = Vec::new();
    let mut occupancy = Vec::<u32>::new();
    for row_container in row_nodes {
        let candidates: Vec<&XmlNode> = if row_container.is(TABLE_NS, "table-row") {
            vec![row_container]
        } else {
            row_container.children().filter(|child| child.is(TABLE_NS, "table-row")).collect()
        };
        let header = row_container.is(TABLE_NS, "table-header-rows");
        for row in candidates {
            let repeat = parse_repeat(
                row.attr(TABLE_NS, "number-rows-repeated"),
                "table:number-rows-repeated",
                options.limits.max_table_rows,
            )?;
            state.table_rows = state
                .table_rows
                .checked_add(repeat)
                .ok_or_else(|| limit("max_table_rows", "ODF row count overflow"))?;
            if state.table_rows > options.limits.max_table_rows {
                return Err(limit(
                    "max_table_rows",
                    format!("{} > {}", state.table_rows, options.limits.max_table_rows),
                ));
            }
            for repetition in 0..repeat {
                context.checkpoint()?;
                let row_index = u32::try_from(rows.len())
                    .map_err(|_| limit("max_table_rows", "row coordinate cannot be represented"))?;
                let (cells, logical_width, represented_width) = parse_table_row(
                    row,
                    row_index,
                    styles,
                    package,
                    state,
                    options,
                    context,
                    locator,
                    sheet,
                    &mut occupancy,
                    header,
                )?;
                if logical_width > options.limits.max_table_columns {
                    return Err(limit(
                        "max_table_columns",
                        format!(
                            "row {} width exceeds {}",
                            rows.len() + 1,
                            options.limits.max_table_columns
                        ),
                    ));
                }
                if declared_columns != 0 && logical_width > declared_columns {
                    return Err(malformed(
                        Some("content.xml"),
                        format!(
                            "row {} logical width {logical_width} exceeds {declared_columns} declared columns",
                            rows.len() + 1
                        ),
                    ));
                }
                rows.push(TableRow { cells });
                represented_widths.push(represented_width);
                if repetition % 1024 == 0 {
                    context.checkpoint()?;
                }
            }
        }
    }
    if occupancy.iter().any(|remaining| *remaining != 0) {
        return Err(malformed(Some("content.xml"), "table row span extends beyond the final row"));
    }
    // Sparse trailing repeats participate in logical coordinates/bounds, but are not retained
    // merely to widen the IR table. A later semantic cell makes the gap non-trailing and it is
    // represented normally.
    let width = represented_widths.iter().copied().max().unwrap_or(0);
    for (row, current) in rows.iter_mut().zip(represented_widths) {
        for _ in current..width {
            state.table_cells = state
                .table_cells
                .checked_add(1)
                .ok_or_else(|| limit("max_table_cells", "ODF padding cell count overflow"))?;
            if state.table_cells > options.limits.max_table_cells {
                return Err(limit(
                    "max_table_cells",
                    format!("{} > {}", state.table_cells, options.limits.max_table_cells),
                ));
            }
            row.cells.push(Cell { row_span: 1, column_span: 1, header: false, blocks: vec![] });
        }
    }
    state.node(Block::Table { rows, alignments: vec![] }, locator.clone())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn parse_table_row(
    row: &XmlNode,
    row_index: u32,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    base_locator: &SourceLocator,
    sheet: Option<&str>,
    occupancy: &mut Vec<u32>,
    header: bool,
) -> Result<(Vec<Cell>, u64, u64), ConversionError> {
    let children: Vec<_> = row
        .children()
        .filter(|child| {
            child.is(TABLE_NS, "table-cell") || child.is(TABLE_NS, "covered-table-cell")
        })
        .collect();
    let mut cells = Vec::new();
    let mut column = 0_u64;
    let mut represented_width = 0_u64;
    let mut horizontal_covered = BTreeSet::new();
    for (position, child) in children.iter().enumerate() {
        let repeat = parse_repeat(
            child.attr(TABLE_NS, "number-columns-repeated"),
            "table:number-columns-repeated",
            options.limits.max_table_columns,
        )?;
        let repeat_end = column
            .checked_add(repeat)
            .ok_or_else(|| limit("max_table_columns", "cell offset + repeat overflow"))?;
        if repeat_end > options.limits.max_table_columns
            || repeat_end > u64::try_from(MAX_TABLE_COLUMNS).unwrap_or(u64::MAX)
        {
            return Err(limit(
                "max_table_columns",
                format!("cell offset {column} + repeat {repeat} ends at {repeat_end}"),
            ));
        }
        let covered = child.is(TABLE_NS, "covered-table-cell");
        if covered {
            for _ in 0..repeat {
                let index = usize::try_from(column).map_err(|_| {
                    limit("max_table_columns", "covered-cell coordinate cannot be represented")
                })?;
                let vertically_covered =
                    occupancy.get(index).is_some_and(|remaining| *remaining > 0);
                let horizontally_covered = horizontal_covered.remove(&column);
                if !horizontally_covered && !vertically_covered {
                    return Err(malformed(
                        Some("content.xml"),
                        "covered-table-cell has no spanning origin",
                    ));
                }
                if vertically_covered && !horizontally_covered {
                    represented_width = represented_width.checked_add(1).ok_or_else(|| {
                        limit("max_table_columns", "represented table width overflow")
                    })?;
                }
                column = column.checked_add(1).ok_or_else(|| {
                    limit("max_table_columns", "covered-cell coordinate overflow")
                })?;
            }
            continue;
        }
        let semantic = cell_semantic_value(child)?;
        let meaningful =
            cell_has_content(child) || semantic.cached.is_some() || semantic.formula.is_some();
        // Trailing repeated empty cells are sparse coordinate padding and need not be retained.
        let materialize = if !meaningful && position + 1 == children.len() { 0 } else { repeat };
        for _ in 0..materialize {
            let col = u32::try_from(column)
                .map_err(|_| limit("max_table_columns", "cell coordinate cannot be represented"))?;
            let column_index = usize::try_from(column)
                .map_err(|_| limit("max_table_columns", "cell coordinate cannot be represented"))?;
            if occupancy.get(column_index).is_some_and(|remaining| *remaining > 0)
                || horizontal_covered.contains(&column)
            {
                return Err(malformed(Some("content.xml"), "origin cell overlaps a spanning cell"));
            }
            let mut locator = base_locator.clone();
            locator.sheet = sheet.map(str::to_owned);
            locator.cell = sheet.map(|_| CellRef { row: row_index, column: col });
            let mut blocks = parse_blocks(
                child,
                styles,
                package,
                state,
                options,
                context,
                &locator,
                ParseMode::Cell,
                1,
            )?;
            if blocks.is_empty()
                && let Some(value) = semantic.cached.clone()
            {
                if u64::try_from(value.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
                    return Err(limit(
                        "max_field_bytes",
                        "ODF cell value exceeds configured limit",
                    ));
                }
                state.add_inlines(1)?;
                let paragraph = state.node(
                    Block::Paragraph(vec![Inline::Text { value, marks: vec![] }]),
                    locator.clone(),
                )?;
                blocks.push(paragraph);
            }
            if let Some(formula) = semantic.formula.clone() {
                blocks.push(state.node(
                    Block::Code { language: Some("openformula".into()), text: formula },
                    locator.clone(),
                )?);
            }
            let row_span = parse_span(
                child.attr(TABLE_NS, "number-rows-spanned"),
                "table:number-rows-spanned",
            )?;
            let column_span = parse_span(
                child.attr(TABLE_NS, "number-columns-spanned"),
                "table:number-columns-spanned",
            )?;
            let span_end = column
                .checked_add(u64::from(column_span))
                .ok_or_else(|| limit("max_table_columns", "ODF column span overflow"))?;
            if span_end > options.limits.max_table_columns
                || span_end > u64::try_from(MAX_TABLE_COLUMNS).unwrap_or(u64::MAX)
            {
                return Err(limit("max_table_columns", format!("spanned cell ends at {span_end}")));
            }
            let span_end_usize = usize::try_from(span_end)
                .map_err(|_| limit("max_table_columns", "ODF column span cannot be represented"))?;
            if occupancy.len() < span_end_usize {
                occupancy.resize(span_end_usize, 0);
            }
            for covered_column in column + 1..span_end {
                horizontal_covered.insert(covered_column);
            }
            for slot in &mut occupancy[column_index..span_end_usize] {
                if *slot != 0 {
                    return Err(malformed(
                        Some("content.xml"),
                        "spanned cell overlaps a previous row span",
                    ));
                }
                *slot = row_span;
            }
            state.table_cells = state
                .table_cells
                .checked_add(1)
                .ok_or_else(|| limit("max_table_cells", "ODF cell count overflow"))?;
            if state.table_cells > options.limits.max_table_cells {
                return Err(limit(
                    "max_table_cells",
                    format!("{} > {}", state.table_cells, options.limits.max_table_cells),
                ));
            }
            cells.push(Cell { row_span, column_span, header, blocks });
            represented_width = represented_width
                .checked_add(u64::from(column_span))
                .ok_or_else(|| limit("max_table_columns", "represented table width overflow"))?;
            column = column
                .checked_add(1)
                .ok_or_else(|| limit("max_table_columns", "ODF cell coordinate overflow"))?;
        }
        let omitted = repeat.checked_sub(materialize).ok_or_else(|| ConversionError::Internal {
            detail: "ODF sparse repeat accounting underflow".into(),
        })?;
        if omitted != 0 {
            let start = usize::try_from(column).map_err(|_| {
                limit("max_table_columns", "sparse repeat coordinate cannot be represented")
            })?;
            let end = usize::try_from(repeat_end).map_err(|_| {
                limit("max_table_columns", "sparse repeat coordinate cannot be represented")
            })?;
            if occupancy.iter().take(end).skip(start).any(|slot| *slot > 0)
                || horizontal_covered.range(column..repeat_end).next().is_some()
            {
                return Err(malformed(
                    Some("content.xml"),
                    "sparse repeated cell overlaps a spanning origin",
                ));
            }
            column = repeat_end;
        }
        if column > options.limits.max_table_columns
            || column > u64::try_from(MAX_TABLE_COLUMNS).unwrap_or(u64::MAX)
        {
            return Err(limit(
                "max_table_columns",
                format!("{column} exceeds configured/IR column limit"),
            ));
        }
    }
    if !horizontal_covered.is_empty() {
        return Err(malformed(
            Some("content.xml"),
            "spanned cell is missing covered-table-cell markers",
        ));
    }
    for remaining in occupancy.iter_mut() {
        *remaining = remaining.saturating_sub(1);
    }
    Ok((cells, column, represented_width))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn parse_drawing(
    node: &XmlNode,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    base_locator: &SourceLocator,
    mode: ParseMode,
) -> Result<Vec<BlockNode>, ConversionError> {
    parse_drawing_transformed(
        node,
        styles,
        package,
        state,
        options,
        context,
        base_locator,
        mode,
        Transform::IDENTITY,
    )
}

#[allow(clippy::too_many_arguments)]
fn parse_drawing_transformed(
    node: &XmlNode,
    styles: &StyleMap,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    base_locator: &SourceLocator,
    mode: ParseMode,
    parent_transform: Transform,
) -> Result<Vec<BlockNode>, ConversionError> {
    context.checkpoint()?;
    let transform = parent_transform.then(parse_transform(node.attr(DRAW_NS, "transform"))?)?;
    let mut locator = base_locator.clone();
    if let Some(bounds) = drawing_bounds(node)? {
        locator.bounds = Some(transform_bounds(bounds, transform)?);
    }
    let rotation = transform.b.atan2(transform.a).to_degrees();
    if !rotation.is_finite() {
        return Err(malformed(Some("content.xml"), "non-finite drawing rotation"));
    }
    if rotation.abs() > f32::EPSILON {
        locator.rotation_degrees = Some((rotation % 360.0 + 360.0) % 360.0);
    }
    let mut blocks = Vec::new();
    if node.is(DRAW_NS, "image") {
        if let Some(block) = image_block(node, package, state, options, context, &locator)? {
            blocks.push(block);
        }
        return Ok(blocks);
    }
    for child in node.children() {
        context.checkpoint()?;
        if child.is(DRAW_NS, "image") {
            if let Some(block) = image_block(child, package, state, options, context, &locator)? {
                blocks.push(block);
            }
        } else if child.is(DRAW_NS, "text-box") || child.is(PRESENTATION_NS, "notes") {
            blocks.extend(parse_blocks(
                child, styles, package, state, options, context, &locator, mode, 1,
            )?);
        } else if child.is(TABLE_NS, "table") {
            blocks.push(parse_table(
                child, styles, package, state, options, context, &locator, None,
            )?);
        } else if child.name.ns == DRAW_NS || child.name.ns == PRESENTATION_NS {
            blocks.extend(parse_drawing_transformed(
                child, styles, package, state, options, context, &locator, mode, transform,
            )?);
        } else if child.is(TEXT_NS, "p") || child.is(TEXT_NS, "h") || child.is(TEXT_NS, "list") {
            blocks.extend(parse_blocks(
                node, styles, package, state, options, context, &locator, mode, 1,
            )?);
            break;
        } else if !child.text().trim().is_empty() {
            return Err(malformed(
                Some("content.xml"),
                format!("unsupported drawing child {}:{}", child.name.ns, child.name.local),
            ));
        }
    }
    Ok(blocks)
}
