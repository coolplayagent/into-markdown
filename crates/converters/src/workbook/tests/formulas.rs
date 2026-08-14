use super::support::{
    complete_xlsb_formula_container, context, convert, push_xlsb_record, push_xlsb_string,
    xlsb_package, xlsb_package_with_relationships, xlsx_with_parts,
};
use crate::workbook::model::BinaryFormulaContext;
use crate::workbook::preflight::preflight_package;
use crate::workbook::xlsb::sheet::scan_xlsb_sheet;
use into_markdown_core::{Block, ConversionError, ConversionOptions, Inline};

#[test]
fn xlsb_formula_cached_variants_are_scanned_without_execution() {
    let mut sheet = Vec::new();
    push_xlsb_record(&mut sheet, 0x0081, &[]);
    let mut dimension = Vec::new();
    dimension.extend_from_slice(&0_u32.to_le_bytes());
    dimension.extend_from_slice(&0_u32.to_le_bytes());
    dimension.extend_from_slice(&0_u32.to_le_bytes());
    dimension.extend_from_slice(&3_u32.to_le_bytes());
    push_xlsb_record(&mut sheet, 0x0094, &dimension);
    push_xlsb_record(&mut sheet, 0x0091, &[]);
    let mut row = [0_u8; 17];
    row[8..10].copy_from_slice(&300_u16.to_le_bytes());
    push_xlsb_record(&mut sheet, 0x0000, &row);
    let tokens = [0x1e, 1, 0];

    let mut string_formula = vec![0; 8];
    push_xlsb_string(&mut string_formula, "=literal");
    string_formula.extend_from_slice(&0_u16.to_le_bytes());
    string_formula.extend_from_slice(&u32::try_from(tokens.len()).unwrap().to_le_bytes());
    string_formula.extend_from_slice(&tokens);
    push_xlsb_record(&mut sheet, 0x0008, &string_formula);

    let mut numeric_formula = vec![0; 8];
    numeric_formula[0..4].copy_from_slice(&1_u32.to_le_bytes());
    numeric_formula.extend_from_slice(&3_f64.to_le_bytes());
    numeric_formula.extend_from_slice(&0_u16.to_le_bytes());
    numeric_formula.extend_from_slice(&u32::try_from(tokens.len()).unwrap().to_le_bytes());
    numeric_formula.extend_from_slice(&tokens);
    push_xlsb_record(&mut sheet, 0x0009, &numeric_formula);

    let mut bool_formula = vec![0; 8];
    bool_formula[0..4].copy_from_slice(&2_u32.to_le_bytes());
    bool_formula.push(1);
    bool_formula.extend_from_slice(&0_u16.to_le_bytes());
    bool_formula.extend_from_slice(&u32::try_from(tokens.len()).unwrap().to_le_bytes());
    bool_formula.extend_from_slice(&tokens);
    push_xlsb_record(&mut sheet, 0x000a, &bool_formula);

    let mut error_formula = vec![0; 8];
    error_formula[0..4].copy_from_slice(&3_u32.to_le_bytes());
    error_formula.push(0x07);
    error_formula.extend_from_slice(&0_u16.to_le_bytes());
    error_formula.extend_from_slice(&u32::try_from(tokens.len()).unwrap().to_le_bytes());
    error_formula.extend_from_slice(&tokens);
    push_xlsb_record(&mut sheet, 0x000b, &error_formula);
    push_xlsb_record(&mut sheet, 0x0092, &[]);
    push_xlsb_record(&mut sheet, 0x0082, &[]);

    let scan = scan_xlsb_sheet(
        &sheet,
        "xl/worksheets/sheet1.bin",
        Some(BinaryFormulaContext::default()),
        &ConversionOptions::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(scan.formulas, 4);
    assert_eq!(scan.dimensions, Some((0, 3)));
}

#[test]
fn xlsb_unexpanded_formula_tokens_fail_before_calamine() {
    for token in [0x01_u8, 0x20, 0x40, 0x60] {
        let mut sheet = Vec::new();
        push_xlsb_record(&mut sheet, 0x0081, &[]);
        push_xlsb_record(&mut sheet, 0x0094, &[0; 16]);
        push_xlsb_record(&mut sheet, 0x0091, &[]);
        let mut row = [0_u8; 17];
        row[8..10].copy_from_slice(&300_u16.to_le_bytes());
        push_xlsb_record(&mut sheet, 0x0000, &row);
        let mut formula = vec![0; 8];
        formula.extend_from_slice(&3_f64.to_le_bytes());
        formula.extend_from_slice(&0_u16.to_le_bytes());
        formula.extend_from_slice(&1_u32.to_le_bytes());
        formula.push(token);
        push_xlsb_record(&mut sheet, 0x0009, &formula);
        push_xlsb_record(&mut sheet, 0x0092, &[]);
        push_xlsb_record(&mut sheet, 0x0082, &[]);

        let error = scan_xlsb_sheet(
            &sheet,
            "xl/worksheets/sheet1.bin",
            Some(BinaryFormulaContext::default()),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::Unsupported { .. }), "token {token:#04x}");
        let package = xlsb_package(&sheet);
        assert!(
            matches!(
                convert(&package, &ConversionOptions::default()),
                Err(ConversionError::Unsupported { .. })
            ),
            "packaged token {token:#04x} reached the third-party parser"
        );
    }
}

#[test]
fn xlsb_formula_container_records_are_unsupported_in_every_state_before_calamine() {
    for (typ, name) in [(0x01aa_u16, "BrtArrFmla"), (0x01ab, "BrtShrFmla"), (0x01ac, "BrtTable")] {
        for position in ["first", "before-dimension", "sheet-data", "after-sheet-data", "after-end"]
        {
            let sheet = complete_xlsb_formula_container(position, typ, false);
            let error = scan_xlsb_sheet(
                &sheet,
                "xl/worksheets/sheet1.bin",
                Some(BinaryFormulaContext::default()),
                &ConversionOptions::default(),
                &context(),
            )
            .unwrap_err();
            assert!(
                matches!(&error, ConversionError::Unsupported { detail } if detail.contains(name)),
                "{name} at {position}: {error:?}"
            );
        }

        for position in ["sheet-data", "after-sheet-data"] {
            let sheet = complete_xlsb_formula_container(position, typ, true);
            assert!(matches!(
                scan_xlsb_sheet(
                    &sheet,
                    "xl/worksheets/sheet1.bin",
                    Some(BinaryFormulaContext::default()),
                    &ConversionOptions::default(),
                    &context(),
                ),
                Err(ConversionError::Unsupported { .. })
            ));
        }

        let mixed = complete_xlsb_formula_container("sheet-data", typ, false);
        let package = xlsb_package(&mixed);
        let options = ConversionOptions::default();
        let preflight_context = context();
        let error = preflight_package(
            &package,
            &options,
            &preflight_context,
            preflight_context.available_memory_bytes(),
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::Unsupported { .. }));
        assert_eq!(preflight_context.reserved_memory_bytes(), 0);
        assert!(matches!(convert(&package, &options), Err(ConversionError::Unsupported { .. })));
    }
}

#[test]
fn xlsb_formula_container_record_framing_truncation_remains_malformed() {
    for typ in [0x01aa_u16, 0x01ab, 0x01ac] {
        let mut truncated_header = Vec::new();
        push_xlsb_record(&mut truncated_header, typ, &[]);
        truncated_header.pop();
        assert!(matches!(
            scan_xlsb_sheet(
                &truncated_header,
                "xl/worksheets/sheet1.bin",
                None,
                &ConversionOptions::default(),
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));

        let mut truncated_payload = Vec::new();
        push_xlsb_record(&mut truncated_payload, typ, &[0xaa, 0xbb]);
        truncated_payload.pop();
        assert!(matches!(
            scan_xlsb_sheet(
                &truncated_payload,
                "xl/worksheets/sheet1.bin",
                None,
                &ConversionOptions::default(),
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }
}

#[test]
fn formula_hyperlinks_preserve_code_semantics_and_override_cell_marks() {
    let raw_formula = format!("{}2", "1+".repeat(96));
    let rendered_formula = format!("={raw_formula} [cached: 1900-01-03 00:00:00]");
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><dimension ref="A1"/><sheetData><row r="1"><c r="A1" s="1"><f>{raw_formula}</f><v>3</v></c></row></sheetData><hyperlinks><hyperlink ref="A1" r:id="rIdLink"/></hyperlinks></worksheet>"#
    );
    let relationships = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdLink" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.invalid/formula" TargetMode="External"/></Relationships>"#;
    let bytes = xlsx_with_parts(&sheet, Some(relationships), &[]);
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    let Block::Paragraph(content) = &rows[0].cells[0].blocks[0].block else { panic!() };
    assert_eq!(
        content,
        &[Inline::Link {
            target: "https://example.invalid/formula".into(),
            content: vec![Inline::Code(rendered_formula.clone())],
        }]
    );
    assert_eq!(
        output.document.metadata.properties["spreadsheet.formulaStylePolicy"],
        "codeSemanticsOverrideCellMarks"
    );
    let markdown = into_markdown_render_markdown::render(
        &output.document,
        &output.assets,
        &ConversionOptions::default(),
    )
    .unwrap();
    assert!(
        markdown.contains(&format!("[`{rendered_formula}`](<https://example.invalid/formula>)"))
    );

    let mut exact = ConversionOptions::default();
    exact.limits.max_field_bytes = u64::try_from(rendered_formula.len()).unwrap();
    convert(&bytes, &exact).unwrap();
    exact.limits.max_field_bytes -= 1;
    let error = convert(&bytes, &exact).unwrap_err();
    assert!(
        matches!(error, ConversionError::ResourceLimit { limit: "max_field_bytes", .. }),
        "{error:?}"
    );
}

#[test]
fn xlsb_formula_hyperlink_is_code_and_uses_exact_combined_field_limits() {
    let target = format!("https://example.invalid/{}", "x".repeat(96));
    let mut sheet = Vec::new();
    push_xlsb_record(&mut sheet, 0x0081, &[]);
    push_xlsb_record(&mut sheet, 0x0094, &[0; 16]);
    push_xlsb_record(&mut sheet, 0x0091, &[]);
    let mut row = [0_u8; 17];
    row[8..10].copy_from_slice(&300_u16.to_le_bytes());
    push_xlsb_record(&mut sheet, 0x0000, &row);
    let tokens = [0x1e, 1, 0, 0x1e, 2, 0, 0x03];
    let mut formula = vec![0; 8];
    formula.extend_from_slice(&3_f64.to_le_bytes());
    formula.extend_from_slice(&0_u16.to_le_bytes());
    formula.extend_from_slice(&u32::try_from(tokens.len()).unwrap().to_le_bytes());
    formula.extend_from_slice(&tokens);
    push_xlsb_record(&mut sheet, 0x0009, &formula);
    push_xlsb_record(&mut sheet, 0x0092, &[]);
    let mut hyperlink = Vec::new();
    hyperlink.extend_from_slice(&0_u32.to_le_bytes());
    hyperlink.extend_from_slice(&0_u32.to_le_bytes());
    hyperlink.extend_from_slice(&0_u32.to_le_bytes());
    hyperlink.extend_from_slice(&0_u32.to_le_bytes());
    push_xlsb_string(&mut hyperlink, "rIdLink");
    push_xlsb_string(&mut hyperlink, "");
    push_xlsb_string(&mut hyperlink, "formula link");
    push_xlsb_string(&mut hyperlink, "ignored label");
    push_xlsb_record(&mut sheet, 0x01ee, &hyperlink);
    push_xlsb_record(&mut sheet, 0x0082, &[]);
    let relationships = format!(
        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdLink" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{target}" TargetMode="External"/></Relationships>"#
    );
    let bytes = xlsb_package_with_relationships(&sheet, Some(&relationships));
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    let Block::Paragraph(content) = &rows[0].cells[0].blocks[0].block else { panic!() };
    assert_eq!(
        content,
        &[Inline::Link {
            target: target.clone(),
            content: vec![Inline::Code("=1+2 [cached: 3]".into())],
        }]
    );

    let mut exact = ConversionOptions::default();
    exact.limits.max_field_bytes = u64::try_from(target.len()).unwrap();
    convert(&bytes, &exact).unwrap();
    exact.limits.max_field_bytes -= 1;
    let error = convert(&bytes, &exact).unwrap_err();
    assert!(
        matches!(error, ConversionError::ResourceLimit { limit: "max_field_bytes", .. }),
        "{error:?}"
    );
}
