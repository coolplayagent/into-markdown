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
    // One sheet wrapper, one table, one node per row and cell, and at most one
    // paragraph for every cell. This is the retained IR upper bound, not a
    // proxy based only on cell count: a tall one-column sheet can otherwise
    // cross the document limit while remaining well below the old 2x-cell
    // threshold.
    1_u64.saturating_add(unpaged_grid_nodes(rows, columns))
        > u64::try_from(MAX_DOCUMENT_NODES).unwrap_or(u64::MAX)
}

pub(super) fn requires_paged_workbook(
    bounds: impl IntoIterator<Item = (u32, u32)>,
    sheet_count: u64,
    extra_nodes: u64,
) -> bool {
    let retained_nodes = bounds.into_iter().fold(
        sheet_count.saturating_add(extra_nodes),
        |total, (last_row, last_column)| {
            total.saturating_add(unpaged_grid_nodes(
                u64::from(last_row) + 1,
                u64::from(last_column) + 1,
            ))
        },
    );
    retained_nodes > u64::try_from(MAX_DOCUMENT_NODES).unwrap_or(u64::MAX)
}

fn unpaged_grid_nodes(rows: u64, columns: u64) -> u64 {
    let cells = rows.saturating_mul(columns);
    1_u64.saturating_add(rows).saturating_add(cells.saturating_mul(2))
}

pub(super) fn extras_node_count(extras: &BTreeMap<String, SheetExtras>) -> u64 {
    extras.values().fold(0_u64, |total, sheet| {
        total.saturating_add(
            u64::try_from(
                sheet
                    .annotations
                    .len()
                    .saturating_add(sheet.chart_titles.len())
                    .saturating_add(sheet.images.len()),
            )
            .unwrap_or(u64::MAX),
        )
    })
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

#[cfg(test)]
mod tests {
    use super::{requires_paged_grid, requires_paged_workbook};

    #[test]
    fn paging_uses_the_complete_retained_grid_node_bound() {
        assert!(!requires_paged_grid(33_332, 1));
        assert!(requires_paged_grid(33_333, 1));
        assert!(!requires_paged_grid(2, 24_999));
        assert!(requires_paged_grid(2, 25_000));
    }

    #[test]
    fn paging_accounts_for_all_sheets_before_emission() {
        assert!(!requires_paged_grid(20_000, 1));
        assert!(requires_paged_workbook([(19_999, 0), (19_999, 0)], 2, 0));
        assert!(!requires_paged_workbook([(19_999, 0)], 1, 0));
    }
}
