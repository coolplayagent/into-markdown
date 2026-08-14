use super::support::{context, convert, xlsx};
use crate::workbook::xlsx::sheet::scan_xlsx_sheet;
use into_markdown_core::{Block, ConversionError, ConversionOptions, Inline, InlineMark};

#[test]
fn xlsx_preserves_types_formula_cache_merge_and_bounds() {
    let bytes = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:C3"/><cols><col min="3" max="3" hidden="1"/></cols><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>name</t></is></c><c r="B1" t="b"><v>1</v></c><c r="C1"><v>42.5</v></c></row><row r="2" hidden="1"><c r="A2" s="1"><v>45292</v></c><c r="B2"><f>SUM(1,2)</f><v>3</v></c><c r="C2" t="inlineStr"><is><t>=cmd</t></is></c></row><row r="3"><c r="B3" t="inlineStr"><is><t>merged</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="B3:C3"/></mergeCells></worksheet>"#,
    );
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.bounds"], "A1:C3");
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    let Block::Paragraph(formula) = &rows[1].cells[1].blocks[0].block else { panic!() };
    assert_eq!(formula, &[Inline::Code("=SUM(1,2) [cached: 3]".into())]);
    let Block::Paragraph(date) = &rows[1].cells[0].blocks[0].block else { panic!() };
    assert!(matches!(&date[0], Inline::Text { value, .. } if value.starts_with("2024-01-01")));
    assert!(matches!(&date[0], Inline::Text { marks, .. }
        if marks == &[InlineMark::Bold, InlineMark::Italic]));
    let Block::Paragraph(injection) = &rows[1].cells[2].blocks[0].block else { panic!() };
    assert_eq!(injection, &[Inline::Code("=cmd".into())]);
    assert_eq!(rows[2].cells[1].column_span, 2);
    assert_eq!(rows[2].cells.len(), 2);
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.hiddenRows"], "2");
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.hiddenColumns"], "C");
    let rendered = into_markdown_render_markdown::render(
        &output.document,
        &output.assets,
        &ConversionOptions::default(),
    )
    .unwrap();
    assert!(rendered.contains("`=SUM(1,2) [cached: 3]`"));
    assert!(rendered.contains("`=cmd`"));
    assert!(!rendered.lines().any(|line| line.starts_with("=cmd")));
}

#[test]
fn xlsx_array_and_shared_formula_states_are_explicit_and_fail_closed() {
    let array = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B2"/><sheetData><row r="1"><c r="A1"><f t="array" ref="A1:B2">ROW(A1:B2)</f><v>1</v></c></row></sheetData></worksheet>"#,
    );
    let output = convert(&array, &ConversionOptions::default()).unwrap();
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.bounds"], "A1:B2");
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    let Block::Paragraph(formula) = &rows[0].cells[0].blocks[0].block else { panic!() };
    assert_eq!(formula, &[Inline::Code("=ROW(A1:B2) [cached: 1]".into())]);

    let valid_shared = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A2"/><sheetData><row r="1"><c r="A1"><f t="shared" si="7" ref="A1:A2">A1+1</f><v>2</v></c></row><row r="2"><c r="A2"><f t="shared" si="7"/><v>3</v></c></row></sheetData></worksheet>"#;
    let (bounds, _, inventory) = scan_xlsx_sheet(
        valid_shared.as_bytes(),
        "xl/worksheets/sheet1.xml",
        &ConversionOptions::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(bounds, Some((1, 0)));
    assert_eq!(inventory.shared_formula_slots, 8);
    let output = convert(&xlsx(valid_shared), &ConversionOptions::default()).unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    let Block::Paragraph(derived) = &rows[1].cells[0].blocks[0].block else { panic!() };
    assert_eq!(derived, &[Inline::Code("=A2+1 [cached: 3]".into())]);

    for invalid in [
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1"/></c></row></sheetData></worksheet>"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1" ref="A1:A1"></f></c></row></sheetData></worksheet>"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="array" ref="A1:A1"></f></c></row></sheetData></worksheet>"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1" ref="A1:A2">1</f></c></row></sheetData></worksheet>"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1" ref="A1:A2">1</f></c><c r="B1"><f t="shared" si="1"/></c></row></sheetData></worksheet>"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1" ref="A1:A1">1</f></c><c r="B1"><f t="shared" si="1" ref="B1:B1">2</f></c></row></sheetData></worksheet>"#,
    ] {
        assert!(matches!(
            scan_xlsx_sheet(
                invalid.as_bytes(),
                "xl/worksheets/sheet1.xml",
                &ConversionOptions::default(),
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }

    let data_table = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="dataTable" ref="A1:B2">TABLE(A1,B1)</f></c></row></sheetData></worksheet>"#;
    assert!(matches!(
        scan_xlsx_sheet(
            data_table,
            "xl/worksheets/sheet1.xml",
            &ConversionOptions::default(),
            &context(),
        ),
        Err(ConversionError::Unsupported { .. })
    ));
}
