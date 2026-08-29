//! Checked resource accounting shared by scanners and renderers.

use crate::workbook::error::limit;
use crate::workbook::model::SheetExtras;
use into_markdown_core::{
    ConversionError, ConversionOptions, MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS,
};
use std::collections::BTreeMap;

pub(super) fn checked_field_bytes(
    options: &ConversionOptions,
    field: &str,
    parts: &[u64],
) -> Result<u64, ConversionError> {
    let bytes = parts.iter().try_fold(0_u64, |total, part| {
        total
            .checked_add(*part)
            .ok_or_else(|| limit("max_field_bytes", format!("{field} size overflow")))
    })?;
    if bytes > options.limits.max_field_bytes {
        return Err(limit(
            "max_field_bytes",
            format!("{field} requires {bytes} > {}", options.limits.max_field_bytes),
        ));
    }
    Ok(bytes)
}

pub(super) fn enforce_grid(
    rows: u64,
    columns: u64,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if rows > options.limits.max_table_rows {
        return Err(limit("max_table_rows", format!("{rows} > {}", options.limits.max_table_rows)));
    }
    let column_limit = options.limits.max_table_columns.min(MAX_TABLE_COLUMNS as u64);
    if columns > column_limit {
        return Err(limit("max_table_columns", format!("{columns} > {column_limit}")));
    }
    let cells = rows
        .checked_mul(columns)
        .ok_or_else(|| limit("max_table_cells", "worksheet cell count overflow"))?;
    if cells > options.limits.max_table_cells {
        return Err(limit(
            "max_table_cells",
            format!("{cells} > {}", options.limits.max_table_cells),
        ));
    }
    Ok(())
}

pub(super) fn enforce_total_cells(
    cells: u64,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if cells > options.limits.max_table_cells {
        return Err(limit(
            "max_table_cells",
            format!("{cells} > {}", options.limits.max_table_cells),
        ));
    }
    Ok(())
}

pub(super) fn requires_paged_grid(rows: u64, columns: u64) -> bool {
    let cells = rows.saturating_mul(columns);
    let ir_limit = u64::try_from(MAX_DOCUMENT_NODES.saturating_sub(64) / 2).unwrap_or(u64::MAX);
    cells > ir_limit
}

pub(super) fn extras_retained_memory(
    extras: &BTreeMap<String, SheetExtras>,
    sheet_parts: &BTreeMap<String, String>,
) -> Result<u64, ConversionError> {
    let mut strings = 0_u64;
    let mut structures = 0_u64;
    for (name, part) in sheet_parts {
        strings = strings
            .checked_add(u64::try_from(name.len()).unwrap_or(u64::MAX))
            .and_then(|value| value.checked_add(u64::try_from(part.len()).unwrap_or(u64::MAX)))
            .ok_or_else(|| limit("max_memory_bytes", "sheet authority memory overflow"))?;
    }
    for sheet in extras.values() {
        structures = structures
            .checked_add(u64::try_from(sheet.hyperlinks.len()).unwrap_or(u64::MAX))
            .and_then(|value| {
                value.checked_add(u64::try_from(sheet.annotations.len()).unwrap_or(u64::MAX))
            })
            .and_then(|value| {
                value.checked_add(u64::try_from(sheet.chart_titles.len()).unwrap_or(u64::MAX))
            })
            .and_then(|value| {
                value.checked_add(u64::try_from(sheet.images.len()).unwrap_or(u64::MAX))
            })
            .and_then(|value| {
                value.checked_add(u64::try_from(sheet.cell_marks.len()).unwrap_or(u64::MAX))
            })
            .and_then(|value| {
                value.checked_add(u64::try_from(sheet.hidden_rows.len()).unwrap_or(u64::MAX))
            })
            .and_then(|value| {
                value.checked_add(u64::try_from(sheet.hidden_columns.len()).unwrap_or(u64::MAX))
            })
            .ok_or_else(|| limit("max_memory_bytes", "worksheet extras count overflow"))?;
        for hyperlink in &sheet.hyperlinks {
            strings = strings
                .checked_add(u64::try_from(hyperlink.target.len()).unwrap_or(u64::MAX))
                .and_then(|value| {
                    value.checked_add(
                        hyperlink
                            .label
                            .as_ref()
                            .map_or(0, |label| u64::try_from(label.len()).unwrap_or(u64::MAX)),
                    )
                })
                .ok_or_else(|| limit("max_memory_bytes", "hyperlink memory overflow"))?;
        }
        for annotation in &sheet.annotations {
            strings = strings
                .checked_add(u64::try_from(annotation.text.len()).unwrap_or(u64::MAX))
                .and_then(|value| {
                    value.checked_add(
                        annotation
                            .author
                            .as_ref()
                            .map_or(0, |author| u64::try_from(author.len()).unwrap_or(u64::MAX)),
                    )
                })
                .ok_or_else(|| limit("max_memory_bytes", "annotation memory overflow"))?;
        }
        for chart in &sheet.chart_titles {
            strings = strings
                .checked_add(u64::try_from(chart.title.len()).unwrap_or(u64::MAX))
                .and_then(|value| {
                    value.checked_add(u64::try_from(chart.part.len()).unwrap_or(u64::MAX))
                })
                .and_then(|value| {
                    value.checked_add(u64::try_from(chart.target.len()).unwrap_or(u64::MAX))
                })
                .and_then(|value| {
                    value
                        .checked_add(u64::try_from(chart.relationship_id.len()).unwrap_or(u64::MAX))
                })
                .ok_or_else(|| limit("max_memory_bytes", "chart-title memory overflow"))?;
        }
        for image in &sheet.images {
            strings = strings
                .checked_add(u64::try_from(image.asset.0.len()).unwrap_or(u64::MAX))
                .and_then(|value| {
                    value.checked_add(u64::try_from(image.part.len()).unwrap_or(u64::MAX))
                })
                .and_then(|value| {
                    value.checked_add(u64::try_from(image.target.len()).unwrap_or(u64::MAX))
                })
                .and_then(|value| {
                    value
                        .checked_add(u64::try_from(image.relationship_id.len()).unwrap_or(u64::MAX))
                })
                .and_then(|value| {
                    value.checked_add(
                        image
                            .alt
                            .as_ref()
                            .map_or(0, |alt| u64::try_from(alt.len()).unwrap_or(u64::MAX)),
                    )
                })
                .ok_or_else(|| limit("max_memory_bytes", "image-anchor memory overflow"))?;
        }
    }
    structures
        .checked_mul(512)
        .and_then(|value| strings.checked_mul(4).and_then(|strings| value.checked_add(strings)))
        .ok_or_else(|| limit("max_memory_bytes", "worksheet extras memory overflow"))
}
