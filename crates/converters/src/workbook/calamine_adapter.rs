use crate::workbook::LegacyXlsHints;
use crate::workbook::budget::{
    checked_field_bytes, enforce_grid, extras_node_count, requires_paged_grid,
    requires_paged_workbook,
};
use crate::workbook::cell::{cell_name, within};
use crate::workbook::error::{limit, malformed, map_calamine};
use crate::workbook::extras::metadata::display_ranges;
use crate::workbook::legacy_xls_emit::{
    LegacyHintCursor, PagedSheet, append_digest, formatted_numeric, paged_tsv_blocks,
    serialized_merges,
};
use crate::workbook::model::{CellCoordinate, Hyperlink, SheetExtras};
use crate::workbook::output::{data_text, provenance, stable_id};
use crate::workbook::xlsb::merges::extract_xlsb_merges;
use calamine::{Data, Dimensions, Range, Reader, SheetType, SheetVisible, Xls, Xlsb};
use into_markdown_core::{
    Block, BlockNode, Cell, ConversionError, ConversionOptions, ConverterOutput, Diagnostic,
    DiagnosticSeverity, Document, ExecutionContext, Inline, NodeId, SourceLocator, TableAlignment,
    TableRow,
};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek};

pub(crate) fn convert_xls(
    bytes: &[u8],
    hints: &LegacyXlsHints,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let mut workbook = Xls::new(Cursor::new(bytes)).map_err(|error| map_calamine("XLS", error))?;
    context.checkpoint()?;
    let sheets = workbook.sheets_metadata().to_vec();
    let mut merges = BTreeMap::new();
    for sheet in &sheets {
        context.checkpoint()?;
        if sheet.typ == SheetType::WorkSheet {
            let value = workbook
                .merge_cells_by_sheet_name(&sheet.name)
                .map_err(|error| map_calamine("XLS merged cells", error))?;
            merges.insert(sheet.name.clone(), value);
        }
    }
    convert_reader(
        &mut workbook,
        ReaderInputs {
            sheets: &sheets,
            merges: &merges,
            extras: &BTreeMap::new(),
            authenticated_bounds: &hints.authenticated_bounds,
            legacy_hints: Some(hints),
        },
        options,
        context,
    )
}

pub(super) fn convert_xlsb(
    bytes: &[u8],
    sheet_parts: &BTreeMap<String, String>,
    sheet_bounds: &BTreeMap<String, CellCoordinate>,
    extras: &BTreeMap<String, SheetExtras>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    // In particular, `Xlsb::new` and `worksheet_formula` cannot be interrupted
    // mid-call. The first-party BIFF12 scan has already authenticated every
    // parser-driving length and the BrtWsDim-driven formula allocation.
    context.checkpoint()?;
    let mut workbook =
        Xlsb::new(Cursor::new(bytes)).map_err(|error| map_calamine("XLSB", error))?;
    context.checkpoint()?;
    let sheets = workbook.sheets_metadata().to_vec();
    let merges = extract_xlsb_merges(bytes, &sheets, sheet_parts, options, context)?;
    convert_reader(
        &mut workbook,
        ReaderInputs {
            sheets: &sheets,
            merges: &merges,
            extras,
            authenticated_bounds: sheet_bounds,
            legacy_hints: None,
        },
        options,
        context,
    )
}

#[derive(Clone, Copy)]
struct ReaderInputs<'a> {
    sheets: &'a [calamine::Sheet],
    merges: &'a BTreeMap<String, Vec<Dimensions>>,
    extras: &'a BTreeMap<String, SheetExtras>,
    authenticated_bounds: &'a BTreeMap<String, CellCoordinate>,
    legacy_hints: Option<&'a LegacyXlsHints>,
}

#[allow(clippy::too_many_lines)]
fn convert_reader<RS, R, E>(
    workbook: &mut R,
    inputs: ReaderInputs<'_>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError>
where
    RS: Read + Seek,
    R: Reader<RS, Error = E>,
    E: std::fmt::Debug + From<std::io::Error>,
{
    let ReaderInputs { sheets, merges, extras, authenticated_bounds, legacy_hints } = inputs;
    let mut document = Document::default();
    let mut diagnostics = Vec::new();
    let mut sheet_blocks = Vec::<(String, Vec<BlockNode>)>::new();
    let paged_workbook = requires_paged_workbook(
        authenticated_bounds.values().copied(),
        u64::try_from(sheets.len()).unwrap_or(u64::MAX),
        extras_node_count(extras),
    );
    let mut legacy = LegacyHintCursor::new(legacy_hints);
    for (sheet_index, sheet) in sheets.iter().enumerate() {
        context.checkpoint()?;
        document
            .metadata
            .properties
            .insert(format!("spreadsheet.sheet.{sheet_index}.name"), sheet.name.clone());
        document.metadata.properties.insert(
            format!("spreadsheet.sheet.{sheet_index}.visibility"),
            match sheet.visible {
                SheetVisible::Visible => "visible",
                SheetVisible::Hidden => "hidden",
                SheetVisible::VeryHidden => "veryHidden",
            }
            .into(),
        );
        document.metadata.properties.insert(
            format!("spreadsheet.sheet.{sheet_index}.type"),
            match sheet.typ {
                SheetType::WorkSheet => "worksheet",
                SheetType::DialogSheet => "dialogSheet",
                SheetType::MacroSheet => "macroSheet",
                SheetType::ChartSheet => "chartSheet",
                SheetType::Vba => "vba",
            }
            .into(),
        );
        let sheet_extras = extras.get(&sheet.name).cloned().unwrap_or_default();
        if !sheet_extras.hidden_rows.is_empty() {
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.hiddenRows"),
                display_ranges(&sheet_extras.hidden_rows, true, options, context)?,
            );
        }
        if !sheet_extras.hidden_columns.is_empty() {
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.hiddenColumns"),
                display_ranges(&sheet_extras.hidden_columns, false, options, context)?,
            );
        }
        for (image_index, image) in sheet_extras.images.iter().enumerate() {
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.image.{image_index}.anchor"),
                format!(
                    "{}:{}",
                    cell_name(image.cell.0, image.cell.1),
                    cell_name(image.end.0, image.end.1)
                ),
            );
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.image.{image_index}.part"),
                image.part.clone(),
            );
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.image.{image_index}.target"),
                image.target.clone(),
            );
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.image.{image_index}.relationshipId"),
                image.relationship_id.clone(),
            );
        }
        for (chart_index, chart) in sheet_extras.chart_titles.iter().enumerate() {
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.chart.{chart_index}.anchor"),
                format!(
                    "{}:{}",
                    cell_name(chart.cell.0, chart.cell.1),
                    cell_name(chart.end.0, chart.end.1)
                ),
            );
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.chart.{chart_index}.part"),
                chart.part.clone(),
            );
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.chart.{chart_index}.target"),
                chart.target.clone(),
            );
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.chart.{chart_index}.relationshipId"),
                chart.relationship_id.clone(),
            );
        }
        if sheet.typ != SheetType::WorkSheet {
            return Err(ConversionError::Unsupported {
                detail: format!(
                    "Calamine reported non-worksheet sheet type {:?} for {}",
                    sheet.typ, sheet.name
                ),
            });
        }

        context.checkpoint()?;
        let values = workbook
            .worksheet_range(&sheet.name)
            .map_err(|error| map_calamine("worksheet values", error))?;
        context.checkpoint()?;
        let formulas = workbook
            .worksheet_formula(&sheet.name)
            .map_err(|error| map_calamine("worksheet formulae", error))?;
        context.checkpoint()?;
        let sheet_merges = merges.get(&sheet.name).cloned().unwrap_or_default();
        let bounds = if legacy_hints
            .is_some_and(|hints| hints.authenticated_empty_sheets.contains(&sheet.name))
        {
            None
        } else {
            combined_bounds(
                &values,
                &formulas,
                &sheet_merges,
                &sheet_extras,
                authenticated_bounds.get(&sheet.name).copied(),
            )
        };
        let mut blocks = Vec::new();
        if let Some((last_row, last_column)) = bounds {
            enforce_grid(u64::from(last_row) + 1, u64::from(last_column) + 1, options)?;
            document.metadata.properties.insert(
                format!("spreadsheet.sheet.{sheet_index}.bounds"),
                format!("A1:{}", cell_name(last_row, last_column)),
            );
            let paged_sheet = paged_workbook
                || requires_paged_grid(u64::from(last_row) + 1, u64::from(last_column) + 1);
            if paged_sheet && !sheet_merges.is_empty() {
                document.metadata.properties.insert(
                    format!("spreadsheet.sheet.{sheet_index}.mergedRanges"),
                    serialized_merges(&sheet_merges, last_row, last_column, options, context)?,
                );
            }
            if paged_sheet {
                blocks = paged_tsv_blocks(
                    &PagedSheet {
                        values: &values,
                        formulas: &formulas,
                        name: &sheet.name,
                        index: sheet_index,
                        last_row,
                        last_column,
                    },
                    &mut legacy,
                    options,
                    context,
                )?;
                diagnostics.push(Diagnostic {
                    code: "spreadsheet.largeTablePaged".into(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!(
                        "worksheet {} was emitted as ordered TSV page blocks to keep IR bounded",
                        sheet.name
                    ),
                    locator: Some(SourceLocator {
                        sheet: Some(sheet.name.clone()),
                        ..SourceLocator::default()
                    }),
                });
            } else {
                let mut merge_index =
                    MergeIndex::new(&sheet_merges, last_row, last_column, context)?;
                let mut hyperlink_index = HyperlinkIndex::new(&sheet_extras.hyperlinks, context)?;
                let mut rows = Vec::new();
                rows.try_reserve_exact(
                    usize::try_from(last_row + 1).map_err(|_| {
                        limit("max_memory_bytes", "worksheet row inventory overflow")
                    })?,
                )
                .map_err(|_| {
                    limit("max_memory_bytes", "worksheet row inventory allocation failed")
                })?;
                for row in 0..=last_row {
                    context.checkpoint()?;
                    merge_index.prepare_row(row, context)?;
                    hyperlink_index.prepare_row(row, context)?;
                    let mut cells = Vec::new();
                    for column in 0..=last_column {
                        if column.trailing_zeros() >= 8 {
                            context.checkpoint()?;
                        }
                        let merge = merge_index.at(row, column);
                        if merge.is_some_and(|merge| merge.start != (row, column)) {
                            continue;
                        }
                        let value = values.get_value((row, column)).unwrap_or(&Data::Empty);
                        let parsed_formula =
                            formulas.get_value((row, column)).map_or("", String::as_str);
                        let formula_hint = legacy.formula_hint_at(sheet_index, row, column);
                        let formula = formula_hint
                            .and_then(|hint| hint.value.as_deref())
                            .unwrap_or(parsed_formula);
                        let recovered_cache = legacy.formula_cache_at(sheet_index, row, column);
                        let parsed_cache = data_text(value);
                        let format = legacy.cell_format_at(sheet_index, row, column);
                        let formatted_cache =
                            format.and_then(|code| formatted_numeric(value, code));
                        let cached = recovered_cache
                            .or(formatted_cache.as_deref())
                            .unwrap_or(parsed_cache.as_ref());
                        let cached_bytes = u64::try_from(cached.len()).unwrap_or(u64::MAX);
                        let formula_bytes = u64::try_from(formula.len()).unwrap_or(u64::MAX);
                        if cached_bytes.max(formula_bytes) > options.limits.max_field_bytes {
                            return Err(limit(
                                "max_field_bytes",
                                format!(
                                    "{}!{} exceeds field limit",
                                    sheet.name,
                                    cell_name(row, column)
                                ),
                            ));
                        }
                        let hyperlink = hyperlink_index.at(column);
                        let marks = sheet_extras
                            .cell_marks
                            .get(&(row, column))
                            .cloned()
                            .unwrap_or_default();
                        let inlines = if !formula.is_empty() {
                            let raw = formula.strip_prefix('=').unwrap_or(formula);
                            let rendered_bytes = if cached.is_empty() {
                                checked_field_bytes(
                                    options,
                                    "formula rendering",
                                    &[
                                        1,
                                        u64::try_from(raw.len()).unwrap_or(u64::MAX),
                                        formula_hint.map_or(0, |_| 82),
                                    ],
                                )?
                            } else {
                                checked_field_bytes(
                                    options,
                                    "formula and cached-value rendering",
                                    &[
                                        1,
                                        u64::try_from(raw.len()).unwrap_or(u64::MAX),
                                        formula_hint.map_or(0, |_| 82),
                                        11,
                                        cached_bytes,
                                    ],
                                )?
                            };
                            debug_assert!(rendered_bytes <= options.limits.max_field_bytes);
                            let rendered_capacity =
                                usize::try_from(rendered_bytes).map_err(|_| {
                                    limit(
                                        "max_memory_bytes",
                                        "formula rendering is not representable",
                                    )
                                })?;
                            let mut rendered = String::with_capacity(rendered_capacity);
                            rendered.push('=');
                            rendered.push_str(raw);
                            if let Some(hint) = formula_hint {
                                rendered.push_str(" [biff-sha256:");
                                append_digest(&mut rendered, &hint.token_sha256);
                                rendered.push(']');
                            }
                            if !cached.is_empty() {
                                rendered.push_str(" [cached: ");
                                rendered.push_str(cached);
                                rendered.push(']');
                            }
                            let code = Inline::Code(rendered);
                            if let Some(link) = hyperlink {
                                checked_field_bytes(
                                    options,
                                    "formula hyperlink target",
                                    &[u64::try_from(link.target.len()).unwrap_or(u64::MAX)],
                                )?;
                                // Formula text is always atomic Code. Cell style
                                // marks cannot be attached to Code in the unified IR
                                // and deliberately do not weaken that inert semantic;
                                // a hyperlink wraps the Code so neither fact is lost.
                                vec![Inline::Link {
                                    target: link.target.clone(),
                                    content: vec![code],
                                }]
                            } else {
                                vec![code]
                            }
                        } else if let Some(link) = hyperlink {
                            checked_field_bytes(
                                options,
                                "hyperlink target",
                                &[u64::try_from(link.target.len()).unwrap_or(u64::MAX)],
                            )?;
                            let label = link
                                .label
                                .clone()
                                .filter(|value| !value.is_empty())
                                .unwrap_or_else(|| cached.to_owned());
                            vec![Inline::Link {
                                target: link.target.clone(),
                                content: vec![Inline::Text { value: label, marks }],
                            }]
                        } else if cached.starts_with(['=', '+', '-', '@']) {
                            // Spreadsheet-control prefixes remain literal code and
                            // cannot become a formula if exported by a downstream UI.
                            vec![Inline::Code(cached.to_owned())]
                        } else if cached.is_empty() {
                            Vec::new()
                        } else {
                            vec![Inline::Text { value: cached.to_owned(), marks }]
                        };
                        let cell_provenance = provenance(&sheet.name, Some(row), Some(column));
                        let cell_blocks = if inlines.is_empty() {
                            Vec::new()
                        } else {
                            vec![BlockNode {
                                id: stable_id("cell", sheet_index, row, column),
                                block: Block::Paragraph(inlines),
                                provenance: cell_provenance,
                            }]
                        };
                        let (row_span, column_span) = merge.map_or((1, 1), |merge| {
                            (merge.end.0 - merge.start.0 + 1, merge.end.1 - merge.start.1 + 1)
                        });
                        cells.push(Cell {
                            row_span,
                            column_span,
                            header: false,
                            blocks: cell_blocks,
                        });
                    }
                    rows.push(TableRow { cells });
                }
                blocks.push(BlockNode {
                    id: NodeId(format!("workbook-table-{sheet_index}")),
                    block: Block::Table {
                        rows,
                        alignments: vec![
                            TableAlignment::None;
                            usize::try_from(last_column + 1).map_err(|_| limit(
                                "max_table_columns",
                                "column count overflow"
                            ))?
                        ],
                    },
                    provenance: provenance(&sheet.name, None, None),
                });
            }
        } else {
            document
                .metadata
                .properties
                .insert(format!("spreadsheet.sheet.{sheet_index}.bounds"), "empty".into());
        }
        append_sheet_extras(&mut blocks, &sheet_extras, &sheet.name, sheet_index, options)?;
        sheet_blocks.push((sheet.name.clone(), blocks));
    }

    let assets = Vec::new();

    for (sheet_index, (name, blocks)) in sheet_blocks.into_iter().enumerate() {
        document.blocks.push(BlockNode {
            id: NodeId(format!("workbook-sheet-{sheet_index}")),
            block: Block::Sheet { name: name.clone(), blocks },
            provenance: provenance(&name, None, None),
        });
    }
    Ok(ConverterOutput::new(document, assets, diagnostics))
}

fn combined_bounds(
    values: &Range<Data>,
    formulas: &Range<String>,
    merges: &[Dimensions],
    extras: &SheetExtras,
    authenticated: Option<CellCoordinate>,
) -> Option<(u32, u32)> {
    values
        .end()
        .into_iter()
        .chain(formulas.end())
        .chain(merges.iter().map(|dimension| dimension.end))
        .chain(extras.hyperlinks.iter().map(|hyperlink| hyperlink.end))
        .chain(extras.annotations.iter().map(|annotation| annotation.cell))
        .chain(extras.chart_titles.iter().map(|chart| chart.end))
        .chain(extras.images.iter().map(|image| image.end))
        .chain(extras.hidden_rows.iter().map(|range| (range.1, 0)))
        .chain(extras.hidden_columns.iter().map(|range| (0, range.1)))
        .chain(authenticated)
        .reduce(|left, right| (left.0.max(right.0), left.1.max(right.1)))
}

fn append_sheet_extras(
    blocks: &mut Vec<BlockNode>,
    extras: &SheetExtras,
    sheet_name: &str,
    sheet_index: usize,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    for (annotation_index, annotation) in extras.annotations.iter().enumerate() {
        let cell = cell_name(annotation.cell.0, annotation.cell.1);
        let mut parts = vec![
            8,
            u64::try_from(cell.len()).unwrap_or(u64::MAX),
            2,
            u64::try_from(annotation.text.len()).unwrap_or(u64::MAX),
        ];
        if let Some(author) = &annotation.author {
            parts.extend([2, u64::try_from(author.len()).unwrap_or(u64::MAX), 1]);
        }
        checked_field_bytes(options, "rendered comment", &parts)?;
        let label = annotation.author.as_deref().map_or_else(
            || format!("Comment {cell}"),
            |author| format!("Comment {cell} ({author})"),
        );
        blocks.push(BlockNode {
            id: NodeId(format!("workbook-comment-{sheet_index}-{annotation_index}")),
            block: Block::Paragraph(vec![Inline::Text {
                value: format!("{label}: {}", annotation.text),
                marks: Vec::new(),
            }]),
            provenance: provenance(sheet_name, Some(annotation.cell.0), Some(annotation.cell.1)),
        });
    }
    for (chart_index, chart) in extras.chart_titles.iter().enumerate() {
        checked_field_bytes(
            options,
            "rendered chart title",
            &[7, u64::try_from(chart.title.len()).unwrap_or(u64::MAX)],
        )?;
        let mut chart_provenance = provenance(sheet_name, Some(chart.cell.0), Some(chart.cell.1));
        chart_provenance.locator.part = Some(chart.part.clone());
        blocks.push(BlockNode {
            id: NodeId(format!("workbook-chart-{sheet_index}-{chart_index}")),
            block: Block::Heading {
                level: 3,
                content: vec![Inline::Text {
                    value: format!("Chart: {}", chart.title),
                    marks: Vec::new(),
                }],
            },
            provenance: chart_provenance,
        });
    }
    for (image_index, image) in extras.images.iter().enumerate() {
        if let Some(alt) = &image.alt {
            checked_field_bytes(
                options,
                "image alternative text",
                &[u64::try_from(alt.len()).unwrap_or(u64::MAX)],
            )?;
        }
        let mut image_provenance = provenance(sheet_name, Some(image.cell.0), Some(image.cell.1));
        image_provenance.locator.part = Some(image.part.clone());
        blocks.push(BlockNode {
            id: NodeId(format!("workbook-image-{sheet_index}-{image_index}")),
            block: Block::Image { asset: image.asset.clone(), alt: image.alt.clone() },
            provenance: image_provenance,
        });
    }
    Ok(())
}

pub(super) fn append_sheet_extras_for_native(
    blocks: &mut Vec<BlockNode>,
    extras: &SheetExtras,
    sheet_name: &str,
    sheet_index: usize,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    append_sheet_extras(blocks, extras, sheet_name, sheet_index, options)
}

#[cfg(test)]
pub(super) fn append_sheet_extras_for_test(
    blocks: &mut Vec<BlockNode>,
    extras: &SheetExtras,
    sheet_name: &str,
    sheet_index: usize,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    append_sheet_extras_for_native(blocks, extras, sheet_name, sheet_index, options)
}

pub(super) fn validate_extras_fields(
    extras: &BTreeMap<String, SheetExtras>,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    for sheet in extras.values() {
        for hyperlink in &sheet.hyperlinks {
            checked_field_bytes(
                options,
                "hyperlink target",
                &[u64::try_from(hyperlink.target.len()).unwrap_or(u64::MAX)],
            )?;
            if let Some(label) = &hyperlink.label {
                checked_field_bytes(
                    options,
                    "hyperlink label",
                    &[u64::try_from(label.len()).unwrap_or(u64::MAX)],
                )?;
            }
        }
        for annotation in &sheet.annotations {
            checked_field_bytes(
                options,
                "comment text",
                &[u64::try_from(annotation.text.len()).unwrap_or(u64::MAX)],
            )?;
            if let Some(author) = &annotation.author {
                checked_field_bytes(
                    options,
                    "comment author",
                    &[u64::try_from(author.len()).unwrap_or(u64::MAX)],
                )?;
            }
        }
        for chart in &sheet.chart_titles {
            checked_field_bytes(
                options,
                "rendered chart title",
                &[7, u64::try_from(chart.title.len()).unwrap_or(u64::MAX)],
            )?;
            for (label, value) in [
                ("chart anchor part", &chart.part),
                ("chart target part", &chart.target),
                ("chart relationship id", &chart.relationship_id),
            ] {
                checked_field_bytes(
                    options,
                    label,
                    &[u64::try_from(value.len()).unwrap_or(u64::MAX)],
                )?;
            }
        }
        for image in &sheet.images {
            for (label, value) in [
                ("image anchor part", &image.part),
                ("image target part", &image.target),
                ("image relationship id", &image.relationship_id),
                ("image asset id", &image.asset.0),
            ] {
                checked_field_bytes(
                    options,
                    label,
                    &[u64::try_from(value.len()).unwrap_or(u64::MAX)],
                )?;
            }
            if let Some(alt) = &image.alt {
                checked_field_bytes(
                    options,
                    "image alternative text",
                    &[u64::try_from(alt.len()).unwrap_or(u64::MAX)],
                )?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct MergeSpan {
    start: CellCoordinate,
    end: CellCoordinate,
}

struct MergeIndex {
    starts: BTreeMap<u32, Vec<MergeSpan>>,
    ends: BTreeMap<u32, Vec<MergeSpan>>,
    active: BTreeMap<u32, MergeSpan>,
}

impl MergeIndex {
    fn new(
        merges: &[Dimensions],
        last_row: u32,
        last_column: u32,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let mut output =
            Self { starts: BTreeMap::new(), ends: BTreeMap::new(), active: BTreeMap::new() };
        for dimension in merges {
            context.checkpoint()?;
            if dimension.start.0 > dimension.end.0
                || dimension.start.1 > dimension.end.1
                || dimension.end.0 > last_row
                || dimension.end.1 > last_column
            {
                return Err(malformed(None, "merged range lies outside worksheet bounds"));
            }
            if dimension.start == dimension.end {
                continue;
            }
            let span = MergeSpan { start: dimension.start, end: dimension.end };
            output.starts.entry(span.start.0).or_default().push(span);
            output.ends.entry(span.end.0.saturating_add(1)).or_default().push(span);
        }
        Ok(output)
    }

    fn prepare_row(&mut self, row: u32, context: &ExecutionContext) -> Result<(), ConversionError> {
        if let Some(ending) = self.ends.remove(&row) {
            for span in ending {
                context.checkpoint()?;
                if self.active.remove(&span.start.1).is_none() {
                    return Err(malformed(None, "invalid merged-range sweep state"));
                }
            }
        }
        if let Some(mut starting) = self.starts.remove(&row) {
            starting.sort_unstable_by_key(|span| span.start.1);
            for span in starting {
                context.checkpoint()?;
                if self
                    .active
                    .range(..=span.end.1)
                    .next_back()
                    .is_some_and(|(_, active)| active.end.1 >= span.start.1)
                {
                    return Err(malformed(None, "overlapping merged ranges"));
                }
                if self.active.insert(span.start.1, span).is_some() {
                    return Err(malformed(None, "overlapping merged ranges"));
                }
            }
        }
        Ok(())
    }

    fn at(&self, row: u32, column: u32) -> Option<MergeSpan> {
        self.active
            .range(..=column)
            .next_back()
            .map(|(_, span)| *span)
            .filter(|span| within((row, column), span.start, span.end))
    }
}

struct HyperlinkIndex<'a> {
    links: &'a [Hyperlink],
    starts: BTreeMap<u32, Vec<usize>>,
    ends: BTreeMap<u32, Vec<usize>>,
    active: BTreeMap<u32, usize>,
}

impl<'a> HyperlinkIndex<'a> {
    fn new(links: &'a [Hyperlink], context: &ExecutionContext) -> Result<Self, ConversionError> {
        let mut output =
            Self { links, starts: BTreeMap::new(), ends: BTreeMap::new(), active: BTreeMap::new() };
        for (index, link) in links.iter().enumerate() {
            context.checkpoint()?;
            output.starts.entry(link.start.0).or_default().push(index);
            output.ends.entry(link.end.0.saturating_add(1)).or_default().push(index);
        }
        Ok(output)
    }

    fn prepare_row(&mut self, row: u32, context: &ExecutionContext) -> Result<(), ConversionError> {
        if let Some(ending) = self.ends.remove(&row) {
            for index in ending {
                context.checkpoint()?;
                let link = &self.links[index];
                if self.active.remove(&link.start.1) != Some(index) {
                    return Err(malformed(None, "invalid hyperlink sweep state"));
                }
            }
        }
        if let Some(mut starting) = self.starts.remove(&row) {
            starting.sort_unstable_by_key(|index| self.links[*index].start.1);
            for index in starting {
                context.checkpoint()?;
                let link = &self.links[index];
                if self
                    .active
                    .range(..=link.end.1)
                    .next_back()
                    .is_some_and(|(_, active)| self.links[*active].end.1 >= link.start.1)
                    || self.active.insert(link.start.1, index).is_some()
                {
                    return Err(malformed(None, "overlapping hyperlink ranges"));
                }
            }
        }
        Ok(())
    }

    fn at(&self, column: u32) -> Option<&'a Hyperlink> {
        self.active
            .range(..=column)
            .next_back()
            .map(|(_, index)| &self.links[*index])
            .filter(|link| column <= link.end.1)
    }
}

#[cfg(test)]
pub(super) fn assert_range_indexes_for_test(
    context: &ExecutionContext,
    cancelled: &ExecutionContext,
) {
    test_support::assert_range_indexes(context, cancelled);
}

#[cfg(test)]
#[path = "calamine_adapter_test_support.rs"]
mod test_support;
