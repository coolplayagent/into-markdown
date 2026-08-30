use super::support::{context, convert, push_xlsb_record, xlsx};
use crate::workbook::model::BinaryFormulaContext;
use crate::workbook::schema::{MAX_EXCEL_COLUMNS, MAX_EXCEL_ROWS};
use crate::workbook::xlsb::sheet::scan_xlsb_sheet;
use crate::workbook::xlsb::tables::{scan_binary_shared_strings, scan_binary_style_counts};
use crate::workbook::xlsb::validate_xlsb_formula_tokens_for_test;
use crate::workbook::xlsb::workbook::scan_binary_workbook_surface;
use calamine::Dimensions;
use into_markdown_core::{ConversionError, ConversionOptions, ErrorPolicy};

#[test]
fn worksheet_bounds_use_authenticated_actual_coordinates() {
    let without_dimension = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="2"><c r="B2"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    let output = convert(&without_dimension, &ConversionOptions::default()).unwrap();
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.bounds"], "A1:B2");

    let underreported = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData><row r="2"><c r="B2"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    let corrected = convert(&underreported, &ConversionOptions::default()).unwrap();
    assert!(
        corrected
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "spreadsheet.dimension.corrected")
    );
    let strict =
        ConversionOptions { error_policy: ErrorPolicy::Strict, ..ConversionOptions::default() };
    assert!(matches!(convert(&underreported, &strict), Err(ConversionError::Malformed { .. })));

    let exact = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A2"/><sheetData><row r="2"><c r="A2"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    let mut options = ConversionOptions::default();
    options.limits.max_table_rows = 1;
    assert!(matches!(
        convert(&exact, &options),
        Err(ConversionError::ResourceLimit { limit: "max_table_rows", .. })
    ));
    options.limits.max_table_rows = 2;
    assert!(convert(&exact, &options).is_ok());

    let stale_with_cell = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:XFD1048576"/><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#,
    );
    let output = convert(&stale_with_cell, &ConversionOptions::default()).unwrap();
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.bounds"], "A1:A1");

    let stale_empty = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:XFD1048576"/><sheetData/></worksheet>"#,
    );
    let output = convert(&stale_empty, &ConversionOptions::default()).unwrap();
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.bounds"], "empty");
}

#[test]
fn xlsb_sheet_scanner_rejects_truncation_order_and_cached_type_corruption() {
    let options = ConversionOptions::default();
    let context = context();
    for corrupt in
        [vec![0x94, 0x81], vec![0x91, 0x01, 0, 0x92, 0x01, 0], vec![0x94, 0x01, 16, 0, 0, 0]]
    {
        assert!(matches!(
            scan_xlsb_sheet(&corrupt, "xl/worksheets/sheet1.bin", None, &options, &context),
            Err(ConversionError::Malformed { .. })
        ));
    }

    let mut invalid_bool = vec![0x81, 0x01, 0, 0x94, 0x01, 16];
    invalid_bool.extend_from_slice(&0_u32.to_le_bytes());
    invalid_bool.extend_from_slice(&0_u32.to_le_bytes());
    invalid_bool.extend_from_slice(&0_u32.to_le_bytes());
    invalid_bool.extend_from_slice(&0_u32.to_le_bytes());
    invalid_bool.extend_from_slice(&[0x91, 0x01, 0]);
    let mut row = [0_u8; 17];
    row[8..10].copy_from_slice(&300_u16.to_le_bytes());
    invalid_bool.extend_from_slice(&[0x00, 17]);
    invalid_bool.extend_from_slice(&row);
    invalid_bool.extend_from_slice(&[0x04, 9]);
    invalid_bool.extend_from_slice(&[0; 8]);
    invalid_bool.push(2);
    invalid_bool.extend_from_slice(&[0x92, 0x01, 0]);
    assert!(matches!(
        scan_xlsb_sheet(&invalid_bool, "xl/worksheets/sheet1.bin", None, &options, &context),
        Err(ConversionError::Malformed { .. })
    ));

    let mut duplicate_dimension = Vec::new();
    push_xlsb_record(&mut duplicate_dimension, 0x0081, &[]);
    let dimension = [0_u8; 16];
    push_xlsb_record(&mut duplicate_dimension, 0x0094, &dimension);
    push_xlsb_record(&mut duplicate_dimension, 0x0094, &dimension);
    assert!(matches!(
        scan_xlsb_sheet(&duplicate_dimension, "xl/worksheets/sheet1.bin", None, &options, &context),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn xlsb_parser_driving_collections_are_exact_before_calamine() {
    let options = ConversionOptions::default();
    let context = context();

    let mut oversized_sst = Vec::new();
    let mut declaration = Vec::new();
    declaration.extend_from_slice(&u32::MAX.to_le_bytes());
    declaration.extend_from_slice(&u32::MAX.to_le_bytes());
    push_xlsb_record(&mut oversized_sst, 0x009f, &declaration);
    assert!(matches!(
        scan_binary_shared_strings(&oversized_sst, &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_table_cells", .. })
    ));

    let mut mismatched_sst = Vec::new();
    let mut one = Vec::new();
    one.extend_from_slice(&1_u32.to_le_bytes());
    one.extend_from_slice(&1_u32.to_le_bytes());
    push_xlsb_record(&mut mismatched_sst, 0x009f, &one);
    push_xlsb_record(&mut mismatched_sst, 0x00a0, &[]);
    assert!(matches!(
        scan_binary_shared_strings(&mismatched_sst, &options, &context),
        Err(ConversionError::Malformed { .. })
    ));

    let mut oversized_styles = Vec::new();
    push_xlsb_record(&mut oversized_styles, 0x0267, &0_u32.to_le_bytes());
    push_xlsb_record(&mut oversized_styles, 0x0268, &[]);
    push_xlsb_record(&mut oversized_styles, 0x0269, &u32::MAX.to_le_bytes());
    assert!(matches!(
        scan_binary_style_counts(&oversized_styles, &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_table_cells", .. })
    ));

    let mut mismatched_styles = Vec::new();
    push_xlsb_record(&mut mismatched_styles, 0x0267, &0_u32.to_le_bytes());
    push_xlsb_record(&mut mismatched_styles, 0x0268, &[]);
    push_xlsb_record(&mut mismatched_styles, 0x0269, &1_u32.to_le_bytes());
    push_xlsb_record(&mut mismatched_styles, 0x026a, &[]);
    assert!(matches!(
        scan_binary_style_counts(&mismatched_styles, &options, &context),
        Err(ConversionError::Malformed { .. })
    ));

    let mut oversized_externals = Vec::new();
    push_xlsb_record(&mut oversized_externals, 0x0099, &[0; 8]);
    push_xlsb_record(&mut oversized_externals, 0x0090, &[]);
    push_xlsb_record(&mut oversized_externals, 0x016a, &u32::MAX.to_le_bytes());
    assert!(matches!(
        scan_binary_workbook_surface(&oversized_externals, &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_table_cells", .. })
    ));

    let mut exact_externals = Vec::new();
    push_xlsb_record(&mut exact_externals, 0x0099, &[0; 8]);
    push_xlsb_record(&mut exact_externals, 0x0090, &[]);
    let mut one_external = 1_u32.to_le_bytes().to_vec();
    one_external.extend_from_slice(&[0; 12]);
    push_xlsb_record(&mut exact_externals, 0x016a, &one_external);
    push_xlsb_record(&mut exact_externals, 0x009d, &[]);
    let (inventory, formula_context) =
        scan_binary_workbook_surface(&exact_externals, &options, &context).unwrap();
    assert_eq!(inventory.external_sheet_slots, 1);
    assert_eq!(formula_context.external_sheets, 1);
}

#[test]
fn xlsb_record_scanner_preserves_dimensions_and_merges() {
    let mut bytes = vec![0x81, 0x01, 0, 0x94, 0x01, 16];
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&3_u32.to_le_bytes());
    bytes.extend_from_slice(&[0x91, 0x01, 0]);
    bytes.extend_from_slice(&[0x92, 0x01, 0]);
    bytes.extend_from_slice(&[0xb0, 0x01, 16]);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u32.to_le_bytes());
    bytes.extend_from_slice(&[0xa6, 0x04, 12, 4, 0, 0, 0, b'r', 0, b'I', 0, b'd', 0, b'1', 0]);
    bytes.extend_from_slice(&[0x82, 0x01, 0]);
    let context = context();
    let scan = scan_xlsb_sheet(
        &bytes,
        "xl/worksheets/sheet1.bin",
        None,
        &ConversionOptions::default(),
        &context,
    )
    .unwrap();
    assert_eq!(scan.dimensions, Some((1, 2)));
    assert_eq!(scan.merges, vec![Dimensions { start: (0, 0), end: (1, 2) }]);
    assert_eq!(scan.drawing_relationship_ids, ["rId1"]);
}

#[test]
fn xlsb_stale_dimensions_do_not_expand_actual_capacity() {
    fn stale_sheet(with_cell: bool) -> Vec<u8> {
        let mut sheet = Vec::new();
        push_xlsb_record(&mut sheet, 0x0081, &[]);
        let mut dimension = Vec::new();
        dimension.extend_from_slice(&0_u32.to_le_bytes());
        dimension.extend_from_slice(&(MAX_EXCEL_ROWS - 1).to_le_bytes());
        dimension.extend_from_slice(&0_u32.to_le_bytes());
        dimension.extend_from_slice(&(MAX_EXCEL_COLUMNS - 1).to_le_bytes());
        push_xlsb_record(&mut sheet, 0x0094, &dimension);
        push_xlsb_record(&mut sheet, 0x0091, &[]);
        if with_cell {
            push_xlsb_record(&mut sheet, 0x0000, &[0; 17]);
            push_xlsb_record(&mut sheet, 0x0001, &[0; 8]);
        }
        push_xlsb_record(&mut sheet, 0x0092, &[]);
        push_xlsb_record(&mut sheet, 0x0082, &[]);
        sheet
    }

    let scan = scan_xlsb_sheet(
        &stale_sheet(true),
        "xl/worksheets/sheet1.bin",
        None,
        &ConversionOptions::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(scan.dimensions, Some((0, 0)));
    let scan = scan_xlsb_sheet(
        &stale_sheet(false),
        "xl/worksheets/sheet1.bin",
        None,
        &ConversionOptions::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(scan.dimensions, None);
}

#[test]
fn xlsb_external_name_tokens_are_rejected_for_all_classes() {
    for token in [0x39, 0x59, 0x79] {
        let error = validate_xlsb_formula_tokens_for_test(
            &[token, 0, 0, 0, 0, 0, 0],
            BinaryFormulaContext::default(),
            "xl/worksheets/sheet1.bin",
            &ConversionOptions::default(),
            &context(),
            0,
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::Unsupported { .. }));
    }
}
