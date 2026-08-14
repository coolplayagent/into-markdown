#![allow(clippy::needless_raw_string_hashes)]

use super::support::{NS, convert, package};
use into_markdown_core::{Block, ConversionError, InputFormat, ResourceLimits};

#[test]
fn repeat_boundary_is_exact_and_sparse_trailing_cells_are_not_materialized() {
    let content = format!(
        r#"<office:document-content {NS}><office:body><office:spreadsheet><table:table table:name='S'><table:table-row><table:table-cell office:value-type='string' office:string-value='x'/><table:table-cell table:number-columns-repeated='100'/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Ods, &content, &[]);
    let mut limits = ResourceLimits { max_table_columns: 101, ..ResourceLimits::default() };
    let output = convert(&bytes, InputFormat::Ods, limits.clone()).unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    assert_eq!(rows[0].cells.len(), 1);
    limits.max_table_columns = 99;
    let error = convert(&bytes, InputFormat::Ods, limits).unwrap_err();
    assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_table_columns", .. }));

    let mut limits = ResourceLimits { max_table_columns: 100, ..ResourceLimits::default() };
    assert!(matches!(
        convert(&bytes, InputFormat::Ods, limits.clone()),
        Err(ConversionError::ResourceLimit { limit: "max_table_columns", .. })
    ));
    let overflow = content.replace(
        "table:number-columns-repeated='100'",
        "table:number-columns-repeated='18446744073709551615'",
    );
    let overflow = package(InputFormat::Ods, &overflow, &[]);
    limits.max_table_columns = u64::MAX;
    assert!(matches!(
        convert(&overflow, InputFormat::Ods, limits),
        Err(ConversionError::ResourceLimit { limit: "max_table_columns", .. })
    ));
}

#[test]
fn merged_cells_and_row_spans_preserve_grid_without_covered_origins() {
    let content = format!(
        r#"<office:document-content {NS}><office:body><office:spreadsheet><table:table table:name='Merged'><table:table-row><table:table-cell table:number-columns-spanned='2' table:number-rows-spanned='2'><text:p>origin</text:p></table:table-cell><table:covered-table-cell/></table:table-row><table:table-row><table:covered-table-cell table:number-columns-repeated='2'/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let bytes = package(InputFormat::Ods, &content, &[]);
    let output = convert(&bytes, InputFormat::Ods, ResourceLimits::default()).unwrap();
    output.document.validate().unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    assert_eq!(rows[0].cells.len(), 1);
    assert_eq!((rows[0].cells[0].row_span, rows[0].cells[0].column_span), (2, 2));
    assert!(rows[1].cells.is_empty());
}
