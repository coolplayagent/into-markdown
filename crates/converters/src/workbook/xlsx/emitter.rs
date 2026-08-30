use crate::workbook::budget::{checked_field_bytes, enforce_total_cells};
use crate::workbook::calamine_adapter::append_sheet_extras_for_native;
use crate::workbook::cell::cell_name;
use crate::workbook::error::{limit, malformed};
use crate::workbook::extras::metadata::display_ranges;
use crate::workbook::model::{CellCoordinate, SheetExtras};
use crate::workbook::output::{provenance, stable_id};
use crate::workbook::xlsx::formulas::DisplayProfile;
use crate::workbook::xlsx::regions::{MergeRange, SparseRegion, paginate_region};
use crate::workbook::xlsx::sheet_index::CellToken;
use crate::workbook::xlsx::staging::{StagedCells, StagedReader};
use into_markdown_core::{
    Block, BlockNode, Cell, ConversionError, ConversionOptions, Diagnostic, DiagnosticSeverity,
    Document, ExecutionContext, Inline, MAX_DOCUMENT_NODES, NodeId, SourceLocator, TableAlignment,
    TableRow,
};
use std::collections::BTreeMap;

const TSV_CHUNK_TARGET_BYTES: usize = 4 * 1024 * 1024;
pub(in crate::workbook) const NATIVE_TABLE_NODE_CEILING: u64 = MAX_DOCUMENT_NODES as u64;

#[derive(Clone, Copy, Eq, PartialEq)]
enum EmissionMode {
    Table,
    Tsv,
    HybridMerge,
}

pub(super) struct PreparedSheet {
    pub(super) name: String,
    pub(super) bounds: Option<CellCoordinate>,
    pub(super) regions: Vec<SparseRegion>,
    pub(super) merges: Vec<MergeRange>,
    pub(super) populated_merge_subordinates: bool,
    pub(super) cells: Option<StagedCells>,
    pub(super) physical_cells: u64,
    pub(super) extras: SheetExtras,
}

pub(super) fn emit(
    sheets: Vec<PreparedSheet>,
    display: &DisplayProfile,
    shared_strings: &BTreeMap<u64, String>,
    mut diagnostics: Vec<Diagnostic>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Document, Vec<Diagnostic>), ConversionError> {
    let mut document = Document::default();
    let mut planned_nodes = 0_u64;
    for (sheet_index, mut sheet) in sheets.into_iter().enumerate() {
        context.checkpoint()?;
        document
            .metadata
            .properties
            .insert(format!("spreadsheet.sheet.{sheet_index}.name"), sheet.name.clone());
        document
            .metadata
            .properties
            .insert(format!("spreadsheet.sheet.{sheet_index}.visibility"), "visible".into());
        document
            .metadata
            .properties
            .insert(format!("spreadsheet.sheet.{sheet_index}.type"), "worksheet".into());
        document.metadata.properties.insert(
            format!("spreadsheet.sheet.{sheet_index}.bounds"),
            sheet
                .bounds
                .map_or_else(|| "empty".into(), |end| format!("A1:{}", cell_name(end.0, end.1))),
        );
        append_sheet_metadata(&mut document, &sheet, sheet_index, options, context)?;
        let table_nodes = table_node_upper_bound(&sheet)?;
        let mode = emission_mode(&sheet, table_nodes, planned_nodes);
        append_emission_diagnostics(&mut diagnostics, &sheet, mode);
        let mut blocks = match mode {
            EmissionMode::Table => {
                emit_regions(&mut sheet, display, shared_strings, sheet_index, options, context)?
            }
            EmissionMode::Tsv => emit_tsv_regions(
                &mut sheet,
                display,
                shared_strings,
                sheet_index,
                options,
                context,
            )?,
            EmissionMode::HybridMerge => emit_hybrid_merge_regions(
                &mut sheet,
                display,
                shared_strings,
                sheet_index,
                options,
                context,
            )?,
        };
        append_sheet_extras_for_native(
            &mut blocks,
            &sheet.extras,
            &sheet.name,
            sheet_index,
            options,
        )?;
        let emitted_nodes = actual_block_nodes(&blocks).saturating_add(1);
        planned_nodes = planned_nodes
            .checked_add(emitted_nodes)
            .ok_or_else(|| limit("documentNodes", "native XLSX document node count overflow"))?;
        if planned_nodes > u64::try_from(MAX_DOCUMENT_NODES).unwrap_or(u64::MAX) {
            return Err(limit("documentNodes", format!("{planned_nodes} > {MAX_DOCUMENT_NODES}")));
        }
        document.blocks.push(BlockNode {
            id: NodeId(format!("workbook-sheet-{sheet_index}")),
            block: Block::Sheet { name: sheet.name.clone(), blocks },
            provenance: provenance(&sheet.name, None, None),
        });
    }
    Ok((document, diagnostics))
}

fn emission_mode(sheet: &PreparedSheet, table_nodes: u64, planned_nodes: u64) -> EmissionMode {
    let paged = sheet.regions.len() != 1
        || !regions_are_stream_ordered(&sheet.regions)
        || planned_nodes.saturating_add(table_nodes)
            > u64::try_from(MAX_DOCUMENT_NODES).unwrap_or(u64::MAX);
    match (paged, sheet.merges.is_empty()) {
        (false, _) => EmissionMode::Table,
        (true, true) => EmissionMode::Tsv,
        (true, false) => EmissionMode::HybridMerge,
    }
}

fn append_emission_diagnostics(
    diagnostics: &mut Vec<Diagnostic>,
    sheet: &PreparedSheet,
    mode: EmissionMode,
) {
    if mode != EmissionMode::Table {
        let detail = if mode == EmissionMode::HybridMerge {
            "ordered TSV row chunks with merged ranges retained as bounded HTML tables"
        } else {
            "ordered TSV row chunks"
        };
        diagnostics.push(Diagnostic {
            code: "spreadsheet.largeTablePaged".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "worksheet {} was emitted as {detail} to keep document nodes bounded",
                sheet.name
            ),
            locator: Some(SourceLocator {
                sheet: Some(sheet.name.clone()),
                ..SourceLocator::default()
            }),
        });
    }
    if sheet.populated_merge_subordinates {
        diagnostics.push(Diagnostic {
            code: "spreadsheet.mergeCellsPaged".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "worksheet {} contains populated non-owner merged cells; their values were retained inside the owning HTML table cell",
                sheet.name
            ),
            locator: Some(SourceLocator {
                sheet: Some(sheet.name.clone()),
                ..SourceLocator::default()
            }),
        });
    }
}

fn actual_block_nodes(nodes: &[BlockNode]) -> u64 {
    nodes.iter().fold(0_u64, |total, node| {
        let nested = match &node.block {
            Block::Table { rows, .. } => rows.iter().fold(0_u64, |row_total, row| {
                let cells = row.cells.iter().fold(0_u64, |cell_total, cell| {
                    cell_total.saturating_add(1).saturating_add(actual_block_nodes(&cell.blocks))
                });
                row_total.saturating_add(1).saturating_add(cells)
            }),
            Block::Sheet { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Footnote { blocks, .. } => actual_block_nodes(blocks),
            Block::List { items, .. } => items
                .iter()
                .map(|item| actual_block_nodes(&item.blocks))
                .fold(0_u64, u64::saturating_add),
            _ => 0,
        };
        total.saturating_add(1).saturating_add(nested)
    })
}

fn extras_node_count(sheet: &PreparedSheet) -> u64 {
    u64::try_from(
        sheet.extras.annotations.len()
            + sheet.extras.chart_titles.len()
            + sheet.extras.images.len(),
    )
    .unwrap_or(u64::MAX)
}

fn table_node_upper_bound(sheet: &PreparedSheet) -> Result<u64, ConversionError> {
    table_node_upper_bound_for(&sheet.regions, sheet.physical_cells, extras_node_count(sheet))
}

pub(in crate::workbook) fn table_node_upper_bound_for(
    regions: &[SparseRegion],
    physical_cells: u64,
    extras: u64,
) -> Result<u64, ConversionError> {
    let rows = regions.iter().try_fold(0_u64, |total, region| {
        total
            .checked_add(u64::from(region.last_row - region.first_row) + 1)
            .ok_or_else(|| limit("documentNodes", "native XLSX row node count overflow"))
    })?;
    [
        1,
        u64::try_from(regions.len()).unwrap_or(u64::MAX),
        rows,
        regions_area(regions)?,
        physical_cells,
        extras,
    ]
    .into_iter()
    .try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| limit("documentNodes", "native XLSX table node count overflow"))
    })
}

fn regions_are_stream_ordered(regions: &[SparseRegion]) -> bool {
    regions.windows(2).all(|pair| {
        (pair[0].last_row, pair[0].last_column) < (pair[1].first_row, pair[1].first_column)
    })
}

fn append_sheet_metadata(
    document: &mut Document,
    sheet: &PreparedSheet,
    sheet_index: usize,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if !sheet.extras.hidden_rows.is_empty() {
        document.metadata.properties.insert(
            format!("spreadsheet.sheet.{sheet_index}.hiddenRows"),
            display_ranges(&sheet.extras.hidden_rows, true, options, context)?,
        );
    }
    if !sheet.extras.hidden_columns.is_empty() {
        document.metadata.properties.insert(
            format!("spreadsheet.sheet.{sheet_index}.hiddenColumns"),
            display_ranges(&sheet.extras.hidden_columns, false, options, context)?,
        );
    }
    for (index, image) in sheet.extras.images.iter().enumerate() {
        for (suffix, value) in [
            (
                "anchor",
                format!(
                    "{}:{}",
                    cell_name(image.cell.0, image.cell.1),
                    cell_name(image.end.0, image.end.1)
                ),
            ),
            ("part", image.part.clone()),
            ("target", image.target.clone()),
            ("relationshipId", image.relationship_id.clone()),
        ] {
            document
                .metadata
                .properties
                .insert(format!("spreadsheet.sheet.{sheet_index}.image.{index}.{suffix}"), value);
        }
    }
    for (index, chart) in sheet.extras.chart_titles.iter().enumerate() {
        for (suffix, value) in [
            (
                "anchor",
                format!(
                    "{}:{}",
                    cell_name(chart.cell.0, chart.cell.1),
                    cell_name(chart.end.0, chart.end.1)
                ),
            ),
            ("part", chart.part.clone()),
            ("target", chart.target.clone()),
            ("relationshipId", chart.relationship_id.clone()),
        ] {
            document
                .metadata
                .properties
                .insert(format!("spreadsheet.sheet.{sheet_index}.chart.{index}.{suffix}"), value);
        }
    }
    Ok(())
}

fn emit_tsv_regions(
    sheet: &mut PreparedSheet,
    display: &DisplayProfile,
    shared_strings: &BTreeMap<u64, String>,
    sheet_index: usize,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let staged =
        sheet.cells.take().ok_or_else(|| malformed(None, "worksheet staging owner is missing"))?;
    emit_tsv_tokens(
        sheet,
        TokenStream::new(staged.into_reader()?),
        display,
        shared_strings,
        sheet_index,
        options,
        context,
    )
}

fn emit_tsv_tokens(
    sheet: &PreparedSheet,
    mut tokens: TokenStream,
    display: &DisplayProfile,
    shared_strings: &BTreeMap<u64, String>,
    sheet_index: usize,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let mut next_cell = tokens.next()?;
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut chunk_start = None;
    let mut chunk_index = 0_usize;
    let mut chunk_has_data = false;
    let mut previous_run = None;
    while let Some(token) = next_cell.take() {
        context.checkpoint()?;
        let start = token.coordinate;
        let mut end = start;
        let mut row_text = String::new();
        let value = display.display(&token, shared_strings)?;
        let rendered = render_tsv_token(sheet, &token, &value, options)?;
        append_tsv_value(&mut row_text, &rendered)?;
        next_cell = tokens.next()?;
        while next_cell.as_ref().is_some_and(|next| {
            next.coordinate.0 == start.0 && next.coordinate.1 == end.1.saturating_add(1)
        }) {
            let token = next_cell.take().unwrap();
            row_text.push('\t');
            end = token.coordinate;
            let value = display.display(&token, shared_strings)?;
            let rendered = render_tsv_token(sheet, &token, &value, options)?;
            append_tsv_value(&mut row_text, &rendered)?;
            next_cell = tokens.next()?;
        }
        row_text.push('\n');
        if !text.is_empty() && text.len().saturating_add(row_text.len()) >= TSV_CHUNK_TARGET_BYTES {
            push_tsv_chunk(
                &mut blocks,
                &mut text,
                &mut chunk_start,
                &mut chunk_index,
                sheet,
                sheet_index,
            );
            chunk_has_data = false;
        }
        chunk_start.get_or_insert(start);
        let continues_previous = previous_run.is_some_and(
            |(previous_start, previous_end): (CellCoordinate, CellCoordinate)| {
                start.0 == previous_start.0.saturating_add(1)
                    && start.1 == previous_start.1
                    && end.1 == previous_end.1
            },
        );
        if !chunk_has_data || !continues_previous {
            append_range_fence(&mut text, start, end);
        }
        text.push_str(&row_text);
        chunk_has_data = true;
        previous_run = Some((start, end));
    }
    push_tsv_chunk(&mut blocks, &mut text, &mut chunk_start, &mut chunk_index, sheet, sheet_index);
    Ok(blocks)
}

fn append_range_fence(text: &mut String, start: CellCoordinate, end: CellCoordinate) {
    text.push_str("# range=");
    text.push_str(&cell_name(start.0, start.1));
    text.push(':');
    text.push_str(&cell_name(end.0, end.1));
    text.push('\n');
}

fn push_tsv_chunk(
    blocks: &mut Vec<BlockNode>,
    text: &mut String,
    chunk_start: &mut Option<CellCoordinate>,
    chunk_index: &mut usize,
    sheet: &PreparedSheet,
    sheet_index: usize,
) {
    if text.is_empty() {
        return;
    }
    let start = chunk_start.take().unwrap_or_default();
    blocks.push(BlockNode {
        id: NodeId(format!("workbook-page-{sheet_index}-{chunk_index}")),
        block: Block::Code { language: Some("tsv".into()), text: std::mem::take(text) },
        provenance: provenance(&sheet.name, Some(start.0), Some(start.1)),
    });
    *chunk_index += 1;
}

fn render_tsv_token(
    sheet: &PreparedSheet,
    token: &CellToken,
    cached: &str,
    options: &ConversionOptions,
) -> Result<String, ConversionError> {
    let hyperlink = sheet.extras.hyperlinks.iter().find(|link| {
        token.coordinate.0 >= link.start.0
            && token.coordinate.0 <= link.end.0
            && token.coordinate.1 >= link.start.1
            && token.coordinate.1 <= link.end.1
    });
    if token.formula.is_empty() {
        let label = hyperlink
            .and_then(|link| link.label.as_deref())
            .filter(|label| !label.is_empty())
            .unwrap_or(cached);
        return hyperlink.map_or_else(
            || Ok(label.into()),
            |link| {
                checked_field_bytes(
                    options,
                    "hyperlink rendering",
                    &[
                        4,
                        u64::try_from(label.len()).unwrap_or(u64::MAX),
                        u64::try_from(link.target.len()).unwrap_or(u64::MAX),
                    ],
                )?;
                Ok(format!("[{label}]({})", link.target))
            },
        );
    }
    let formula = token.formula.strip_prefix('=').unwrap_or(&token.formula);
    let parts = if cached.is_empty() {
        vec![1, u64::try_from(formula.len()).unwrap_or(u64::MAX)]
    } else {
        vec![
            1,
            u64::try_from(formula.len()).unwrap_or(u64::MAX),
            11,
            u64::try_from(cached.len()).unwrap_or(u64::MAX),
        ]
    };
    checked_field_bytes(options, "formula and cached-value rendering", &parts)?;
    let mut rendered = if cached.is_empty() {
        format!("={formula}")
    } else {
        format!("={formula} [cached: {cached}]")
    };
    if let Some(link) = hyperlink {
        checked_field_bytes(
            options,
            "formula hyperlink rendering",
            &[
                u64::try_from(rendered.len()).unwrap_or(u64::MAX),
                9,
                u64::try_from(link.target.len()).unwrap_or(u64::MAX),
            ],
        )?;
        rendered.push_str(" [link: ");
        rendered.push_str(&link.target);
        rendered.push(']');
    }
    Ok(rendered)
}

fn append_tsv_value(output: &mut String, value: &str) -> Result<(), ConversionError> {
    output
        .try_reserve(value.len().saturating_mul(2))
        .map_err(|_| limit("max_memory_bytes", "cannot reserve native XLSX TSV field"))?;
    for character in value.chars() {
        match character {
            '\t' => output.push_str("\\t"),
            '\r' => output.push_str("\\r"),
            '\n' => output.push_str("\\n"),
            '\\' => output.push_str("\\\\"),
            '`' => output.push_str("\\`"),
            value => output.push(value),
        }
    }
    Ok(())
}

fn emit_hybrid_merge_regions(
    sheet: &mut PreparedSheet,
    display: &DisplayProfile,
    shared_strings: &BTreeMap<u64, String>,
    sheet_index: usize,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let staged =
        sheet.cells.take().ok_or_else(|| malformed(None, "worksheet staging owner is missing"))?;
    let mut blocks = emit_tsv_tokens(
        sheet,
        TokenStream::new(staged.into_reader()?),
        display,
        shared_strings,
        sheet_index,
        options,
        context,
    )?;
    blocks.extend(emit_merge_html_chunks(sheet, sheet_index, context)?);
    Ok(blocks)
}

fn emit_merge_html_chunks(
    sheet: &PreparedSheet,
    sheet_index: usize,
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let mut ordered = sheet.merges.clone();
    ordered.sort_unstable_by_key(|range| {
        (range.first_row, range.first_column, range.last_row, range.last_column)
    });
    let mut output = Vec::new();
    let mut text = String::new();
    let mut first_coordinate = None;
    for merge_range in ordered {
        context.checkpoint()?;
        if !text.is_empty() && text.len() >= TSV_CHUNK_TARGET_BYTES {
            push_merge_html_chunk(
                &mut output,
                &mut text,
                &mut first_coordinate,
                sheet,
                sheet_index,
            );
        }
        first_coordinate.get_or_insert((merge_range.first_row, merge_range.first_column));
        text.push_str(&cell_name(merge_range.first_row, merge_range.first_column));
        text.push(':');
        text.push_str(&cell_name(merge_range.last_row, merge_range.last_column));
        text.push('\n');
    }
    push_merge_html_chunk(&mut output, &mut text, &mut first_coordinate, sheet, sheet_index);
    Ok(output)
}

fn push_merge_html_chunk(
    output: &mut Vec<BlockNode>,
    text: &mut String,
    first_coordinate: &mut Option<CellCoordinate>,
    sheet: &PreparedSheet,
    sheet_index: usize,
) {
    if text.is_empty() {
        return;
    }
    let coordinate = first_coordinate.take().unwrap_or_default();
    output.push(BlockNode {
        id: NodeId(format!("workbook-merge-{sheet_index}-{}", output.len())),
        block: Block::Code { language: Some("xlsx-merge-html".into()), text: std::mem::take(text) },
        provenance: provenance(&sheet.name, Some(coordinate.0), Some(coordinate.1)),
    });
}

fn emit_regions(
    sheet: &mut PreparedSheet,
    display: &DisplayProfile,
    shared_strings: &BTreeMap<u64, String>,
    sheet_index: usize,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let staged =
        sheet.cells.take().ok_or_else(|| malformed(None, "worksheet staging owner is missing"))?;
    let mut next_tokens = TokenStream::new(staged.into_reader()?);
    let total_area = regions_area(&sheet.regions)?;
    enforce_total_cells(total_area, options)?;
    let mut blocks = Vec::new();
    blocks
        .try_reserve_exact(sheet.regions.len())
        .map_err(|_| limit("max_memory_bytes", "cannot reserve native XLSX table regions"))?;
    let emission = TableEmission { sheet, display, shared_strings, sheet_index, options, context };
    for (region_index, region) in sheet.regions.iter().copied().enumerate() {
        blocks.push(emit_region(&emission, region, region_index, &mut next_tokens)?);
    }
    if next_tokens.next()?.is_some() {
        return Err(malformed(None, "worksheet cell was not assigned to a data region"));
    }
    Ok(blocks)
}

struct TokenStream {
    reader: StagedReader,
    peeked: Option<CellToken>,
}

impl TokenStream {
    fn new(reader: StagedReader) -> Self {
        Self { reader, peeked: None }
    }

    fn peek(&mut self) -> Result<Option<&CellToken>, ConversionError> {
        if self.peeked.is_none() {
            self.peeked = self.reader.next()?;
        }
        Ok(self.peeked.as_ref())
    }

    fn next(&mut self) -> Result<Option<CellToken>, ConversionError> {
        if self.peeked.is_some() { Ok(self.peeked.take()) } else { self.reader.next() }
    }

    #[cfg(test)]
    fn buffered_cells(&self) -> usize {
        usize::from(self.peeked.is_some())
    }
}

struct TableEmission<'a> {
    sheet: &'a PreparedSheet,
    display: &'a DisplayProfile,
    shared_strings: &'a BTreeMap<u64, String>,
    sheet_index: usize,
    options: &'a ConversionOptions,
    context: &'a ExecutionContext,
}

fn emit_region(
    emission: &TableEmission<'_>,
    region: SparseRegion,
    region_index: usize,
    next_tokens: &mut TokenStream,
) -> Result<BlockNode, ConversionError> {
    emission.context.checkpoint()?;
    let width = u64::from(region.last_column - region.first_column) + 1;
    let height = u64::from(region.last_row - region.first_row) + 1;
    if width > emission.options.limits.max_table_columns {
        return Err(limit(
            "max_table_columns",
            format!("{width} > {}", emission.options.limits.max_table_columns),
        ));
    }
    if height > emission.options.limits.max_table_rows {
        return Err(limit(
            "max_table_rows",
            format!("{height} > {}", emission.options.limits.max_table_rows),
        ));
    }
    let mut rows = Vec::new();
    rows.try_reserve_exact(usize::try_from(height).unwrap_or(usize::MAX))
        .map_err(|_| limit("max_memory_bytes", "cannot reserve native XLSX rows"))?;
    let mut merge_owners = BTreeMap::<CellCoordinate, (usize, usize)>::new();
    for page in paginate_region(region, &emission.sheet.merges)? {
        for row in page.first_row..=page.last_row {
            emit_table_row(emission, region, row, &mut rows, next_tokens, &mut merge_owners)?;
        }
    }
    Ok(BlockNode {
        id: NodeId(format!("workbook-table-{}-{region_index}", emission.sheet_index)),
        block: Block::Table {
            rows,
            alignments: vec![TableAlignment::None; usize::try_from(width).unwrap_or(usize::MAX)],
        },
        provenance: provenance(
            &emission.sheet.name,
            Some(region.first_row),
            Some(region.first_column),
        ),
    })
}

fn emit_table_row(
    emission: &TableEmission<'_>,
    region: SparseRegion,
    row: u32,
    rows: &mut Vec<TableRow>,
    next_tokens: &mut TokenStream,
    merge_owners: &mut BTreeMap<CellCoordinate, (usize, usize)>,
) -> Result<(), ConversionError> {
    emission.context.checkpoint()?;
    let width = usize::try_from(region.last_column - region.first_column + 1).unwrap_or(usize::MAX);
    let mut cells = Vec::new();
    cells
        .try_reserve_exact(width)
        .map_err(|_| limit("max_memory_bytes", "cannot reserve native XLSX cells"))?;
    let row_index = rows.len();
    rows.push(TableRow { cells });
    for column in region.first_column..=region.last_column {
        let coordinate = (row, column);
        let merge = merge_at(&emission.sheet.merges, coordinate);
        if merge.is_some_and(|range| (range.first_row, range.first_column) != coordinate) {
            if next_tokens.peek()?.is_some_and(|cell| cell.coordinate < coordinate) {
                return Err(malformed(None, "staged worksheet cells are out of region order"));
            }
            if next_tokens.peek()?.is_some_and(|cell| cell.coordinate == coordinate) {
                let subordinate = next_tokens.next()?.expect("peeked staged merge subordinate");
                let range = merge.expect("subordinate coordinate has a merge");
                let owner = (range.first_row, range.first_column);
                let (owner_row, owner_cell) =
                    merge_owners.get(&owner).copied().ok_or_else(|| {
                        malformed(None, "merged-cell owner precedes no staged region")
                    })?;
                append_merge_subordinate(
                    emission,
                    &mut rows[owner_row].cells[owner_cell].blocks,
                    &subordinate,
                )?;
            }
            continue;
        }
        if next_tokens.peek()?.is_some_and(|cell| cell.coordinate < coordinate) {
            return Err(malformed(None, "staged worksheet cells are out of region order"));
        }
        let token = if next_tokens.peek()?.is_some_and(|cell| cell.coordinate == coordinate) {
            next_tokens.next()?
        } else {
            None
        };
        let blocks = token.map_or_else(
            || Ok::<Vec<BlockNode>, ConversionError>(Vec::new()),
            |token| cell_blocks_for_emission(emission, &token),
        )?;
        let (row_span, column_span) = merge.map_or((1, 1), |range| {
            (range.last_row - range.first_row + 1, range.last_column - range.first_column + 1)
        });
        let cell_index = rows[row_index].cells.len();
        rows[row_index].cells.push(Cell { row_span, column_span, header: false, blocks });
        if merge.is_some() {
            merge_owners.insert(coordinate, (row_index, cell_index));
        }
    }
    Ok(())
}

fn cell_blocks_for_emission(
    emission: &TableEmission<'_>,
    token: &CellToken,
) -> Result<Vec<BlockNode>, ConversionError> {
    cell_blocks(
        emission.sheet,
        emission.display,
        emission.shared_strings,
        emission.sheet_index,
        token,
        emission.options,
    )
}

fn append_merge_subordinate(
    emission: &TableEmission<'_>,
    blocks: &mut Vec<BlockNode>,
    subordinate: &CellToken,
) -> Result<(), ConversionError> {
    let mut retained = cell_blocks_for_emission(emission, subordinate)?;
    if let Some(BlockNode { block: Block::Paragraph(inlines), .. }) = retained.first_mut() {
        inlines.insert(
            0,
            Inline::Text {
                value: format!(
                    "{}: ",
                    cell_name(subordinate.coordinate.0, subordinate.coordinate.1)
                ),
                marks: Vec::new(),
            },
        );
    }
    blocks.extend(retained);
    Ok(())
}

fn cell_blocks(
    sheet: &PreparedSheet,
    display: &DisplayProfile,
    shared_strings: &BTreeMap<u64, String>,
    sheet_index: usize,
    token: &CellToken,
    options: &ConversionOptions,
) -> Result<Vec<BlockNode>, ConversionError> {
    let (row, column) = token.coordinate;
    let cached = display.display(token, shared_strings)?;
    let hyperlink = sheet.extras.hyperlinks.iter().find(|link| {
        row >= link.start.0 && row <= link.end.0 && column >= link.start.1 && column <= link.end.1
    });
    let marks = sheet.extras.cell_marks.get(&(row, column)).cloned().unwrap_or_default();
    let inline = if !token.formula.is_empty() {
        formula_inline(&token.formula, &cached, hyperlink, options)?
    } else if cached.starts_with(['=', '+', '-', '@']) {
        Inline::Code(cached)
    } else {
        let text = Inline::Text {
            value: hyperlink
                .and_then(|link| link.label.clone())
                .filter(|label| !label.is_empty())
                .unwrap_or(cached),
            marks,
        };
        hyperlink.map_or(text.clone(), |link| Inline::Link {
            target: link.target.clone(),
            content: vec![text],
        })
    };
    if matches!(&inline, Inline::Text { value, .. } if value.is_empty()) {
        return Ok(Vec::new());
    }
    Ok(vec![BlockNode {
        id: stable_id("cell", sheet_index, row, column),
        block: Block::Paragraph(vec![inline]),
        provenance: provenance(&sheet.name, Some(row), Some(column)),
    }])
}

fn formula_inline(
    formula: &str,
    cached: &str,
    hyperlink: Option<&crate::workbook::model::Hyperlink>,
    options: &ConversionOptions,
) -> Result<Inline, ConversionError> {
    let formula = formula.strip_prefix('=').unwrap_or(formula);
    let mut parts = vec![1, u64::try_from(formula.len()).unwrap_or(u64::MAX)];
    if !cached.is_empty() {
        parts.extend([11, u64::try_from(cached.len()).unwrap_or(u64::MAX)]);
    }
    checked_field_bytes(options, "formula and cached-value rendering", &parts)?;
    let code = Inline::Code(if cached.is_empty() {
        format!("={formula}")
    } else {
        format!("={formula} [cached: {cached}]")
    });
    Ok(hyperlink.map_or(code.clone(), |link| Inline::Link {
        target: link.target.clone(),
        content: vec![code],
    }))
}

fn merge_at(merges: &[MergeRange], coordinate: CellCoordinate) -> Option<MergeRange> {
    merges.iter().copied().find(|range| {
        coordinate.0 >= range.first_row
            && coordinate.0 <= range.last_row
            && coordinate.1 >= range.first_column
            && coordinate.1 <= range.last_column
    })
}

fn region_area(region: SparseRegion) -> Result<u64, ConversionError> {
    (u64::from(region.last_row - region.first_row) + 1)
        .checked_mul(u64::from(region.last_column - region.first_column) + 1)
        .ok_or_else(|| limit("max_table_cells", "native XLSX region area overflow"))
}

fn regions_area(regions: &[SparseRegion]) -> Result<u64, ConversionError> {
    regions.iter().try_fold(0_u64, |total, region| {
        total
            .checked_add(region_area(*region)?)
            .ok_or_else(|| limit("max_table_cells", "native XLSX region cell count overflow"))
    })
}

#[cfg(test)]
mod tests {
    use super::{TokenStream, regions_are_stream_ordered};
    use crate::workbook::xlsx::regions::SparseRegion;
    use crate::workbook::xlsx::sheet_index::{CellToken, CellValueToken};
    use crate::workbook::xlsx::staging::stage;
    use into_markdown_core::{ExecutionContext, ExecutionOptions, ResourceLimits};

    fn region(first_row: u32, last_row: u32, first_column: u32, last_column: u32) -> SparseRegion {
        SparseRegion {
            first_row,
            last_row,
            first_column,
            last_column,
            occupied_cells: 1,
            contains_merge: false,
        }
    }

    #[test]
    fn interleaved_rectangles_require_sparse_tsv_emission() {
        assert!(!regions_are_stream_ordered(&[region(0, 3, 0, 1), region(0, 3, 4, 5),]));
        assert!(regions_are_stream_ordered(&[region(0, 3, 0, 1), region(4, 7, 0, 1),]));
    }

    #[test]
    fn staged_token_stream_keeps_only_one_lookahead_cell() {
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let cells = (0..4_096)
            .map(|column| CellToken {
                coordinate: (0, column),
                value: CellValueToken::Raw(column.to_string()),
                formula: String::new(),
                cell_type: "n".into(),
                style_index: None,
            })
            .collect::<Vec<_>>();
        let staged = stage(&cells, &context).unwrap();
        let telemetry = staged.telemetry_handle();
        let mut stream = TokenStream::new(staged.into_reader().unwrap());

        assert_eq!(stream.buffered_cells(), 0);
        assert_eq!(stream.peek().unwrap().unwrap().coordinate, (0, 0));
        assert_eq!(stream.peek().unwrap().unwrap().coordinate, (0, 0));
        assert_eq!(stream.buffered_cells(), 1);
        assert_eq!(telemetry.lock().unwrap().reads, 1);
        assert_eq!(stream.next().unwrap().unwrap().coordinate, (0, 0));
        assert_eq!(stream.buffered_cells(), 0);
        assert_eq!(telemetry.lock().unwrap().reads, 1);
        assert_eq!(stream.next().unwrap().unwrap().coordinate, (0, 1));
        assert_eq!(telemetry.lock().unwrap().reads, 2);
    }
}
