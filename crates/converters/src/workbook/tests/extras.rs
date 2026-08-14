use super::support::{
    context, convert, image_xlsx, push_xlsb_record, push_xlsb_string, xlsx, xlsx_with_parts,
};
use crate::workbook::calamine_adapter::{append_sheet_extras_for_test, validate_extras_fields};
use crate::workbook::extras::metadata::{display_ranges, push_compact_range};
use crate::workbook::extras::{
    parse_binary_comments_for_test, parse_sheet_cell_metadata_for_test,
    safe_hyperlink_target_for_test,
};
use crate::workbook::model::{Annotation, ChartTitle, Hyperlink, SheetExtras};
use crate::workbook::schema::MAX_EXCEL_ROWS;
use crate::workbook::xlsb::sheet::scan_xlsb_sheet;
use base64::Engine as _;
use into_markdown_core::{
    Block, CellRef, ConversionError, ConversionOptions, ExecutionContext, Inline,
};
use std::collections::BTreeMap;

#[test]
fn xlsb_comments_require_complete_unique_containers_and_required_richstr_form() {
    let mut comments = Vec::new();
    push_xlsb_record(&mut comments, 0x0274, &[]);
    push_xlsb_record(&mut comments, 0x0276, &[]);
    let mut author = Vec::new();
    push_xlsb_string(&mut author, "Alice");
    push_xlsb_record(&mut comments, 0x0278, &author);
    push_xlsb_record(&mut comments, 0x0277, &[]);
    push_xlsb_record(&mut comments, 0x0279, &[]);
    let mut begin = vec![0; 36];
    begin[0..4].copy_from_slice(&0_u32.to_le_bytes());
    push_xlsb_record(&mut comments, 0x027b, &begin);
    let mut rich = vec![1];
    push_xlsb_string(&mut rich, "rich");
    rich.extend_from_slice(&1_u32.to_le_bytes());
    rich.extend_from_slice(&0_u32.to_le_bytes());
    push_xlsb_record(&mut comments, 0x027d, &rich);
    push_xlsb_record(&mut comments, 0x027c, &[]);
    push_xlsb_record(&mut comments, 0x027a, &[]);
    push_xlsb_record(&mut comments, 0x0275, &[]);
    let parsed = parse_binary_comments_for_test(
        &comments,
        "xl/comments1.bin",
        &ConversionOptions::default(),
        &context(),
    )
    .unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].text, "rich");

    let mut duplicate_text = comments.clone();
    let end_comment = duplicate_text.windows(3).position(|value| value == [0xfc, 0x04, 0]).unwrap();
    let mut text_record = Vec::new();
    push_xlsb_record(&mut text_record, 0x027d, &rich);
    duplicate_text.splice(end_comment..end_comment, text_record);
    assert!(matches!(
        parse_binary_comments_for_test(
            &duplicate_text,
            "xl/comments1.bin",
            &ConversionOptions::default(),
            &context()
        ),
        Err(ConversionError::Malformed { .. })
    ));

    let mut plain = vec![0];
    push_xlsb_string(&mut plain, "plain");
    let invalid_plain = comments
        .windows(rich.len())
        .position(|window| window == rich)
        .map(|start| {
            let mut bytes = comments.clone();
            bytes.splice(start..start + rich.len(), plain);
            bytes
        })
        .unwrap();
    assert!(matches!(
        parse_binary_comments_for_test(
            &invalid_plain,
            "xl/comments1.bin",
            &ConversionOptions::default(),
            &context()
        ),
        Err(ConversionError::Malformed { .. })
    ));

    let truncated = &comments[..comments.len() - 3];
    assert!(matches!(
        parse_binary_comments_for_test(
            truncated,
            "xl/comments1.bin",
            &ConversionOptions::default(),
            &context()
        ),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn hidden_metadata_is_compacted_and_field_limited_before_rendering() {
    let mut adjacent = Vec::new();
    let mut adjacent_bytes = 0_u64;
    push_compact_range(
        &mut adjacent,
        &mut adjacent_bytes,
        (0, 0),
        true,
        "xl/worksheets/sheet1.xml",
        &ConversionOptions::default(),
    )
    .unwrap();
    push_compact_range(
        &mut adjacent,
        &mut adjacent_bytes,
        (1, 2),
        true,
        "xl/worksheets/sheet1.xml",
        &ConversionOptions::default(),
    )
    .unwrap();
    assert_eq!(adjacent, [(0, 2)]);
    assert_eq!(adjacent_bytes, 3);

    let xml = br#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cols><col min="1" max="1" hidden="1"/><col min="3" max="4" hidden="1"/></cols><sheetData><row r="1" hidden="1"/><row r="3" hidden="1"/></sheetData></worksheet>"#;
    let mut metadata_exact = ConversionOptions::default();
    metadata_exact.limits.max_field_bytes = 5;
    let (_, xml_rows, xml_columns) = parse_sheet_cell_metadata_for_test(
        xml,
        "xl/worksheets/sheet1.xml",
        &[],
        &metadata_exact,
        &context(),
    )
    .unwrap();
    assert_eq!(xml_rows, [(0, 0), (2, 2)]);
    assert_eq!(xml_columns, [(0, 0), (2, 3)]);
    let mut xml_low = ConversionOptions::default();
    xml_low.limits.max_field_bytes = 4;
    assert!(matches!(
        parse_sheet_cell_metadata_for_test(
            xml,
            "xl/worksheets/sheet1.xml",
            &[],
            &xml_low,
            &context(),
        ),
        Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
    ));

    let mut sheet = Vec::new();
    push_xlsb_record(&mut sheet, 0x0081, &[]);
    let mut dimension = Vec::new();
    dimension.extend_from_slice(&0_u32.to_le_bytes());
    dimension.extend_from_slice(&2_u32.to_le_bytes());
    dimension.extend_from_slice(&0_u32.to_le_bytes());
    dimension.extend_from_slice(&3_u32.to_le_bytes());
    push_xlsb_record(&mut sheet, 0x0094, &dimension);
    for (first, last) in [(0_u32, 0_u32), (2, 3)] {
        let mut column = [0_u8; 18];
        column[0..4].copy_from_slice(&first.to_le_bytes());
        column[4..8].copy_from_slice(&last.to_le_bytes());
        column[16..18].copy_from_slice(&1_u16.to_le_bytes());
        push_xlsb_record(&mut sheet, 0x003c, &column);
    }
    push_xlsb_record(&mut sheet, 0x0091, &[]);
    for row_index in [0_u32, 2] {
        let mut row = [0_u8; 17];
        row[0..4].copy_from_slice(&row_index.to_le_bytes());
        row[8..10].copy_from_slice(&300_u16.to_le_bytes());
        row[11] = 0x10;
        push_xlsb_record(&mut sheet, 0x0000, &row);
    }
    push_xlsb_record(&mut sheet, 0x0092, &[]);
    push_xlsb_record(&mut sheet, 0x0082, &[]);
    let binary =
        scan_xlsb_sheet(&sheet, "xl/worksheets/sheet1.bin", None, &metadata_exact, &context())
            .unwrap();
    assert_eq!(binary.hidden_rows, xml_rows);
    assert_eq!(binary.hidden_columns, xml_columns);
    let mut binary_low = ConversionOptions::default();
    binary_low.limits.max_field_bytes = 4;
    assert!(matches!(
        scan_xlsb_sheet(&sheet, "xl/worksheets/sheet1.bin", None, &binary_low, &context(),),
        Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
    ));

    for rows in [&xml_rows, &binary.hidden_rows] {
        let mut exact = ConversionOptions::default();
        exact.limits.max_field_bytes = 3;
        assert_eq!(display_ranges(rows, true, &exact, &context()).unwrap(), "1,3");
        exact.limits.max_field_bytes = 2;
        assert!(matches!(
            display_ranges(rows, true, &exact, &context()),
            Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
        ));
    }
    for columns in [&xml_columns, &binary.hidden_columns] {
        let mut exact = ConversionOptions::default();
        exact.limits.max_field_bytes = 5;
        assert_eq!(display_ranges(columns, false, &exact, &context()).unwrap(), "A,C:D");
        exact.limits.max_field_bytes = 4;
        assert!(matches!(
            display_ranges(columns, false, &exact, &context()),
            Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
        ));
    }

    let cancellation = into_markdown_core::CancellationToken::new();
    cancellation.cancel();
    let cancelled = ExecutionContext::new(
        into_markdown_core::ExecutionOptions {
            cancellation,
            ..into_markdown_core::ExecutionOptions::default()
        },
        into_markdown_core::ResourceLimits::default(),
    );
    assert!(matches!(
        display_ranges(&[(0, MAX_EXCEL_ROWS - 1)], true, &ConversionOptions::default(), &cancelled),
        Err(ConversionError::Cancelled)
    ));
}

#[test]
fn final_owned_fields_enforce_the_rendered_utf8_limit_exactly() {
    let raw = format!("{}1", "1+".repeat(127));
    let sheet = format!(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData><row r="1"><c r="A1"><f>{raw}</f><v>3</v></c></row></sheetData></worksheet>"#,
    );
    let bytes = xlsx(&sheet);
    let rendered = format!("={raw} [cached: 3]");
    let mut exact = ConversionOptions::default();
    exact.limits.max_field_bytes = u64::try_from(rendered.len()).unwrap();
    convert(&bytes, &exact).unwrap();
    exact.limits.max_field_bytes -= 1;
    assert!(matches!(
        convert(&bytes, &exact),
        Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
    ));

    let mut extras = BTreeMap::new();
    extras.insert(
        "Sheet1".into(),
        SheetExtras {
            annotations: vec![Annotation {
                cell: (0, 0),
                text: "body".into(),
                author: Some("author".into()),
            }],
            chart_titles: vec![ChartTitle {
                cell: (0, 0),
                end: (0, 0),
                title: "title".into(),
                part: "d".into(),
                target: "c".into(),
                relationship_id: "r".into(),
            }],
            hyperlinks: vec![Hyperlink {
                start: (0, 0),
                end: (0, 0),
                target: "https://e.invalid".into(),
                label: Some("link".into()),
            }],
            ..SheetExtras::default()
        },
    );
    let comment = "Comment A1 (author): body";
    let mut exact = ConversionOptions::default();
    exact.limits.max_field_bytes = u64::try_from(comment.len()).unwrap();
    validate_extras_fields(&extras, &exact).unwrap();
    let mut blocks = Vec::new();
    append_sheet_extras_for_test(&mut blocks, &extras["Sheet1"], "Sheet1", 0, &exact).unwrap();
    exact.limits.max_field_bytes -= 1;
    assert!(matches!(
        append_sheet_extras_for_test(&mut Vec::new(), &extras["Sheet1"], "Sheet1", 0, &exact),
        Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
    ));

    let chart = SheetExtras {
        chart_titles: vec![ChartTitle {
            cell: (0, 0),
            end: (0, 0),
            title: "title".into(),
            part: "d".into(),
            target: "c".into(),
            relationship_id: "r".into(),
        }],
        ..SheetExtras::default()
    };
    let mut exact = ConversionOptions::default();
    exact.limits.max_field_bytes = u64::try_from("Chart: title".len()).unwrap();
    append_sheet_extras_for_test(&mut Vec::new(), &chart, "Sheet1", 0, &exact).unwrap();
    exact.limits.max_field_bytes -= 1;
    assert!(matches!(
        append_sheet_extras_for_test(&mut Vec::new(), &chart, "Sheet1", 0, &exact),
        Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
    ));

    let target = "https://e.invalid#A1";
    let mut exact = ConversionOptions::default();
    exact.limits.max_field_bytes = u64::try_from(target.len()).unwrap();
    assert_eq!(
        safe_hyperlink_target_for_test("https://e.invalid", Some("A1"), &exact).unwrap(),
        target
    );
    exact.limits.max_field_bytes -= 1;
    assert!(matches!(
        safe_hyperlink_target_for_test("https://e.invalid", Some("A1"), &exact),
        Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
    ));
}

#[test]
fn xlsx_extracts_safe_hyperlinks_comments_and_chart_titles() {
    let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><dimension ref="A1"/><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>OpenAI</t></is></c></row></sheetData><hyperlinks><hyperlink ref="A1" r:id="rId1" display="safe"/></hyperlinks><drawing r:id="rId3"/></worksheet>"#;
    let relationships = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.invalid/book" TargetMode="External"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#;
    let comments = r#"<?xml version="1.0"?><comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><authors><author>Alice</author></authors><commentList><comment ref="A1" authorId="0"><text><t>reviewed</t></text></comment></commentList></comments>"#;
    let drawing = r#"<?xml version="1.0"?><xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:oneCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:row>0</xdr:row></xdr:from><xdr:graphicFrame><c:chart r:id="rId1"/></xdr:graphicFrame><xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#;
    let drawing_relationships = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/></Relationships>"#;
    let chart = r#"<?xml version="1.0"?><c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:title><c:tx><c:rich><a:p><a:r><a:t>Revenue</a:t></a:r></a:p></c:rich></c:tx></c:title></c:chart></c:chartSpace>"#;
    let bytes = xlsx_with_parts(
        sheet,
        Some(relationships),
        &[
            ("xl/comments1.xml", comments),
            ("xl/drawings/drawing1.xml", drawing),
            ("xl/drawings/_rels/drawing1.xml.rels", drawing_relationships),
            ("xl/charts/chart1.xml", chart),
        ],
    );
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    let Block::Paragraph(link) = &rows[0].cells[0].blocks[0].block else { panic!() };
    assert!(matches!(
        &link[0],
        Inline::Link { target, content }
            if target == "https://example.invalid/book"
                && matches!(&content[0], Inline::Text { value, .. } if value == "safe")
    ));
    assert!(blocks.iter().any(|node| {
        matches!(&node.block, Block::Paragraph(content)
            if matches!(&content[0], Inline::Text { value, .. }
                if value == "Comment A1 (Alice): reviewed"))
    }));
    assert!(blocks.iter().any(|node| {
        matches!(&node.block, Block::Heading { content, .. }
            if matches!(&content[0], Inline::Text { value, .. }
                if value == "Chart: Revenue"))
    }));
    let chart = blocks.iter().find(|node| matches!(node.block, Block::Heading { .. })).unwrap();
    assert_eq!(chart.provenance.locator.part.as_deref(), Some("xl/drawings/drawing1.xml"));
    assert_eq!(chart.provenance.locator.cell, Some(CellRef { row: 0, column: 0 }));
    assert_eq!(
        output.document.metadata.properties["spreadsheet.sheet.0.chart.0.target"],
        "xl/charts/chart1.xml"
    );
    assert_eq!(
        output.document.metadata.properties["spreadsheet.sheet.0.chart.0.relationshipId"],
        "rId1"
    );

    let unsafe_relationships =
        relationships.replace("https://example.invalid/book", "javascript:alert(1)");
    let unsafe_bytes = xlsx_with_parts(sheet, Some(&unsafe_relationships), &[]);
    assert!(matches!(
        convert(&unsafe_bytes, &ConversionOptions::default()),
        Err(ConversionError::Unsupported { .. })
    ));
}

#[test]
fn xlsx_preserves_each_referenced_image_anchor_and_omits_orphan_media() {
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    let bytes = image_xlsx(&png);
    let output = convert(&bytes, &ConversionOptions::default()).unwrap();
    assert_eq!(output.assets.len(), 1, "identical media is stored once");
    assert_eq!(
        output.document.metadata.properties["spreadsheet.mediaBytes"],
        png.len().to_string()
    );
    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let images = blocks
        .iter()
        .filter_map(|node| match &node.block {
            Block::Image { asset, alt } => Some((asset, alt.as_deref())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 2, "each physical anchor remains represented");
    assert_eq!(images[0].0, images[1].0);
    assert_eq!(images[0].1, Some("first"));
    assert_eq!(images[1].1, Some("second"));
    assert_eq!(output.document.metadata.properties["spreadsheet.sheet.0.image.1.anchor"], "B2:C4");
    assert_eq!(
        output.document.metadata.properties["spreadsheet.sheet.0.image.1.part"],
        "xl/drawings/drawing1.xml"
    );
    assert_eq!(
        output.document.metadata.properties["spreadsheet.sheet.0.image.1.target"],
        "xl/media/image1.png"
    );
    assert_eq!(
        output.document.metadata.properties["spreadsheet.sheet.0.image.1.relationshipId"],
        "rIdImage"
    );
    let image_nodes =
        blocks.iter().filter(|node| matches!(node.block, Block::Image { .. })).collect::<Vec<_>>();
    assert_eq!(image_nodes[1].provenance.locator.part.as_deref(), Some("xl/drawings/drawing1.xml"));
    assert_eq!(image_nodes[1].provenance.locator.cell, Some(CellRef { row: 1, column: 1 }));
    assert!(output.assets.iter().all(|asset| asset.filename.as_deref() != Some("orphan.png")));
}
