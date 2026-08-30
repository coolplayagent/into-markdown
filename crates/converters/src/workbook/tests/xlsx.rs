use super::support::{context, convert, package, xlsx};
use crate::workbook::xlsx::sheet_index::read_layout;
use into_markdown_core::{Block, ConversionError, ConversionOptions, Inline, InlineMark};

#[test]
fn native_xlsx_reuses_one_prepared_layout_and_one_data_pass_per_sheet() {
    let bytes = package(&[
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="root" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Beta" sheetId="2" r:id="rBeta"/><sheet name="Alpha" sheetId="1" r:id="rAlpha"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rAlpha" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="sst" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/><Relationship Id="rBeta" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#,
        ),
        (
            "xl/worksheets/sheet2.xml",
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#,
        ),
        (
            "xl/sharedStrings.xml",
            r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>shared</t></si></sst>"#,
        ),
    ]);
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    let names = output
        .document
        .blocks
        .iter()
        .filter_map(|node| match &node.block {
            Block::Sheet { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(names, ["Beta", "Alpha"]);
    let properties = &output.document.metadata.properties;
    assert_eq!(properties["spreadsheet.native.workbookPasses"], "1");
    assert_eq!(properties["spreadsheet.native.layoutPasses"], "2");
    assert_eq!(properties["spreadsheet.native.dataPasses"], "2");
    assert_eq!(properties["spreadsheet.native.stylePasses"], "0");
    assert_eq!(properties["spreadsheet.native.sharedStringPasses"], "1");
    assert_eq!(properties["spreadsheet.native.stagingReads"], "2");
    assert_eq!(properties["spreadsheet.native.stagingSeeks"], "2");
    assert!(output.diagnostics.is_empty(), "normal native XLSX must remain complete");
}

#[test]
fn ordinary_tables_keep_table_semantics_and_sparse_substitution_is_explicit() {
    let mut rows = String::new();
    for row in 1..=100 {
        use std::fmt::Write as _;
        write!(&mut rows, "<row r=\"{row}\">").unwrap();
        for column in 0..10 {
            let cell = crate::workbook::cell::cell_name(row - 1, column);
            write!(&mut rows, "<c r=\"{cell}\"><v>{row}</v></c>").unwrap();
        }
        rows.push_str("</row>");
    }
    let worksheet = format!(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:J100"/><sheetData>{rows}</sheetData></worksheet>"#
    );
    let output = convert(&xlsx(&worksheet), &ConversionOptions::default()).unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    assert!(matches!(&blocks[0].block, Block::Table { .. }));
    assert!(output.diagnostics.is_empty());

    let sparse = xlsx(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>first</v></c></row><row r="1048576"><c r="XFD1048576"><v>last</v></c></row></sheetData></worksheet>"#,
    );
    let output = convert(&sparse, &ConversionOptions::default()).unwrap();
    assert!(output.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "spreadsheet.largeTablePaged"
            && diagnostic.severity == into_markdown_core::DiagnosticSeverity::Warning
    }));
}

#[test]
fn populated_non_owner_merge_cells_use_exact_paged_representation() {
    let bytes = xlsx(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>owner</t></is></c><c r="B1" t="inlineStr"><is><t>subordinate</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
    );
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    assert!(output.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "spreadsheet.mergeCellsPaged"
            && diagnostic.severity == into_markdown_core::DiagnosticSeverity::Warning
    }));
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    assert_eq!(rows[0].cells[0].column_span, 2);
    assert_eq!(rows[0].cells[0].blocks.len(), 2);
    let rendered = into_markdown_render_markdown::render(
        &output.document,
        &output.assets,
        &ConversionOptions::default(),
    )
    .unwrap();
    assert!(rendered.contains("<td colspan=\"2\">owner<br>B1: subordinate</td>"));
    assert!(!rendered.contains("# merges="));
}

#[test]
fn sparse_merged_sheet_uses_bounded_html_spans_without_merge_directives() {
    let bytes = xlsx(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>merged</t></is></c></row><row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>distant</t></is></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
    );
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    let rendered = into_markdown_render_markdown::render(
        &output.document,
        &output.assets,
        &ConversionOptions::default(),
    )
    .unwrap();
    assert!(rendered.contains("<th scope=\"row\">A1:B1</th><td colspan=\"2\"></td>"));
    assert!(rendered.contains("merged"));
    assert!(rendered.contains("distant"));
    assert!(!rendered.contains("merge-series"));
    assert!(!rendered.contains("data-span"));
}
use std::io::Cursor;

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
    assert!(rendered.contains("<code>=SUM(1,2) [cached: 3]</code>"));
    assert!(rendered.contains("<code>=cmd</code>"));
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
    let layout = read_layout(
        Cursor::new(valid_shared.as_bytes()),
        "xl/worksheets/sheet1.xml",
        &ConversionOptions::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(layout.bounds, Some((1, 0)));
    assert_eq!(layout.shared_formula_slots, 1);
    let output = convert(&xlsx(valid_shared), &ConversionOptions::default()).unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    let Block::Paragraph(derived) = &rows[1].cells[0].blocks[0].block else { panic!() };
    assert_eq!(derived, &[Inline::Code("=A2+1 [cached: 3]".into())]);

    for recoverable in [
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1"/><v>cached</v></c></row></sheetData></worksheet>"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1"/></c></row></sheetData></worksheet>"#,
    ] {
        let layout = read_layout(
            Cursor::new(recoverable.as_bytes()),
            "xl/worksheets/sheet1.xml",
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(layout.diagnostics[0].code, "spreadsheet.sharedFormula.omitted");
        let output = convert(&xlsx(recoverable), &ConversionOptions::default()).unwrap();
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "spreadsheet.sharedFormula.omitted")
        );
        let rendered = into_markdown_render_markdown::render(
            &output.document,
            &output.assets,
            &ConversionOptions::default(),
        )
        .unwrap();
        assert_eq!(rendered.contains("cached"), recoverable.contains("<v>cached</v>"));
        let strict = ConversionOptions {
            error_policy: into_markdown_core::ErrorPolicy::Strict,
            ..ConversionOptions::default()
        };
        assert!(matches!(
            read_layout(
                Cursor::new(recoverable.as_bytes()),
                "xl/worksheets/sheet1.xml",
                &strict,
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }

    for invalid in [
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1" ref="A1:A1"></f></c></row></sheetData></worksheet>"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="array" ref="A1:A1"></f></c></row></sheetData></worksheet>"#,
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="shared" si="1" ref="A1:A2">1</f></c><c r="B1"><f t="shared" si="1"/></c></row></sheetData></worksheet>"#,
    ] {
        assert!(matches!(
            read_layout(
                Cursor::new(invalid.as_bytes()),
                "xl/worksheets/sheet1.xml",
                &ConversionOptions::default(),
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }

    let data_table = br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f t="dataTable" ref="A1:B2">TABLE(A1,B1)</f></c></row></sheetData></worksheet>"#;
    assert!(matches!(
        read_layout(
            Cursor::new(data_table),
            "xl/worksheets/sheet1.xml",
            &ConversionOptions::default(),
            &context(),
        ),
        Err(ConversionError::Unsupported { .. })
    ));
}

#[test]
fn xlsx_table_part_range_is_a_real_data_region() {
    let bytes = package(&[
        (
            "[Content_Types].xml",
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/tables/table1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="root" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Table" sheetId="1" r:id="sheet"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="sheet" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><dimension ref="A1"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>only value</t></is></c></row></sheetData><tableParts count="1"><tablePart r:id="table"/></tableParts></worksheet>"#,
        ),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="table" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#,
        ),
        (
            "xl/tables/table1.xml",
            r#"<table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Table1" displayName="Table1" ref="A1:C3"><autoFilter ref="A1:C3"/><tableColumns count="3"><tableColumn id="1" name="A"/><tableColumn id="2" name="B"/><tableColumn id="3" name="C"/></tableColumns></table>"#,
        ),
    ]);
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.bounds"], "A1:C3");
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.cells.len() == 3));
}
