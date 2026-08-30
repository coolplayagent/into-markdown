use super::support::{
    context, convert, package, package_bytes, push_xlsb_record, push_xlsb_string, rewrite_package,
    xlsx,
};
use crate::workbook::schema::{
    PACKAGE_REL_CT, ROOT_OFFICE_DOCUMENT, ROOT_OFFICE_DOCUMENT_STRICT, SPREADSHEET_BINARY_MAIN,
    SPREADSHEET_MACRO_TEMPLATE_MAIN, SPREADSHEET_MAIN, SPREADSHEET_TEMPLATE_MAIN,
    XLSB_CHARTSHEET_CT, XLSB_DIALOGSHEET_CT, XLSB_MACROSHEET_CT, XML_CHARTSHEET_CT,
    XML_DIALOGSHEET_CT, XML_MACROSHEET_CT, XML_WORKSHEET_CT,
};
use crate::workbook::xlsx::tables::{scan_xml_shared_strings, scan_xml_style_counts};
use into_markdown_core::{ConversionError, ConversionOptions, ErrorPolicy};

#[test]
fn dtd_fails_before_workbook_parse() {
    let dtd = xlsx(
        r#"<?xml version="1.0"?><!DOCTYPE worksheet [<!ENTITY x "boom">]><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData/></worksheet>"#,
    );
    assert!(matches!(
        convert(&dtd, &ConversionOptions::default()),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn parser_driving_declarations_are_bounded_before_calamine() {
    let parser_context = context();
    let duplicate_sst = br#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" uniqueCount="0"></sst><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" uniqueCount="0"></sst>"#;
    assert!(matches!(
        scan_xml_shared_strings(duplicate_sst, &ConversionOptions::default(), &parser_context),
        Err(ConversionError::Malformed { .. })
    ));
    let duplicate_styles = br#"<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="0"/><numFmts count="0"/><cellXfs count="0"/></styleSheet>"#;
    assert!(matches!(
        scan_xml_style_counts(duplicate_styles, &ConversionOptions::default(), &parser_context),
        Err(ConversionError::Malformed { .. })
    ));

    let base = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData/></worksheet>"#,
    );
    let styles = r#"<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cellXfs count="1000001"/></styleSheet>"#;
    let oversized_styles = rewrite_package(&base, &[("xl/styles.xml", styles.to_owned())], &[]);
    assert!(matches!(
        convert(&oversized_styles, &ConversionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_table_cells", .. })
    ));

    let content_types = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>"#;
    let workbook_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#;
    let oversized_sst = r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" uniqueCount="18446744073709551615"/>"#;
    let oversized_strings = rewrite_package(
        &base,
        &[
            ("[Content_Types].xml", content_types.to_owned()),
            ("xl/_rels/workbook.xml.rels", workbook_rels.to_owned()),
        ],
        &[("xl/sharedStrings.xml", oversized_sst.to_owned())],
    );
    assert!(matches!(
        convert(&oversized_strings, &ConversionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_table_cells", .. })
    ));
}

#[test]
fn opc_root_and_content_type_authority_is_fail_closed() {
    let base = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData/></worksheet>"#,
    );
    let duplicate_root = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/><Relationship Id="rId2" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    let duplicate = rewrite_package(&base, &[("_rels/.rels", duplicate_root.to_owned())], &[]);
    assert!(matches!(
        convert(&duplicate, &ConversionOptions::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let spoofed_root = duplicate_root
        .replace(ROOT_OFFICE_DOCUMENT, "https://attacker.invalid/relationships/officeDocument")
        .replace(ROOT_OFFICE_DOCUMENT_STRICT, "https://attacker.invalid/strict/officeDocument");
    let spoofed = rewrite_package(&base, &[("_rels/.rels", spoofed_root)], &[]);
    assert!(matches!(
        convert(&spoofed, &ConversionOptions::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let external_root = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="https://example.invalid/workbook.xml" TargetMode="External"/></Relationships>"#;
    let external = rewrite_package(&base, &[("_rels/.rels", external_root.to_owned())], &[]);
    assert!(matches!(
        convert(&external, &ConversionOptions::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let redirected_root = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/redirect.xml"/></Relationships>"#;
    let redirected_types = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/redirect.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#;
    let redirected = rewrite_package(
        &base,
        &[
            ("_rels/.rels", redirected_root.to_owned()),
            ("[Content_Types].xml", redirected_types.to_owned()),
        ],
        &[(
            "xl/redirect.xml",
            r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#.to_owned(),
        )],
    );
    assert!(matches!(
        convert(&redirected, &ConversionOptions::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let wrong_content_type = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/octet-stream"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#;
    let wrong_type =
        rewrite_package(&base, &[("[Content_Types].xml", wrong_content_type.to_owned())], &[]);
    assert!(matches!(
        convert(&wrong_type, &ConversionOptions::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let orphan_content_type = wrong_content_type.replace(
        "</Types>",
        r#"<Override PartName="/xl/orphan.xml" ContentType="application/xml"/></Types>"#,
    );
    let orphan = rewrite_package(&base, &[("[Content_Types].xml", orphan_content_type)], &[]);
    assert!(matches!(
        convert(&orphan, &ConversionOptions::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let unused_default = wrong_content_type.replace(
        "</Types>",
        r#"<Default Extension="fntdata" ContentType="application/x-fontdata"/></Types>"#,
    );
    let unused_default = unused_default.replace(
        r#"ContentType="application/octet-stream""#,
        r#"ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml""#,
    );
    assert!(
        convert(
            &rewrite_package(&base, &[("[Content_Types].xml", unused_default)], &[]),
            &ConversionOptions::default(),
        )
        .is_ok()
    );

    let workbook_with_extension = r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:x15="http://schemas.microsoft.com/office/spreadsheetml/2010/11/main"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets><extLst><ext uri="fixture"><x15:workbookPr/></ext></extLst></workbook>"#;
    assert!(
        convert(
            &rewrite_package(
                &base,
                &[("xl/workbook.xml", workbook_with_extension.to_owned())],
                &[],
            ),
            &ConversionOptions::default(),
        )
        .is_ok()
    );
}

#[test]
fn xml_workbook_template_content_types_are_accepted() {
    let base = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData/></worksheet>"#,
    );
    for content_type in [SPREADSHEET_TEMPLATE_MAIN, SPREADSHEET_MACRO_TEMPLATE_MAIN] {
        let template = rewrite_package(
            &base,
            &[(
                "[Content_Types].xml",
                format!(
                    r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="{PACKAGE_REL_CT}"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="{content_type}"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="{XML_WORKSHEET_CT}"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#
                ),
            )],
            &[],
        );
        assert!(convert(&template, &ConversionOptions::default()).is_ok(), "{content_type}");
    }
}

#[test]
fn duplicate_logical_sheet_targets_are_rejected() {
    let base = xlsx(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData/></worksheet>"#,
    );
    let workbook = r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="One" sheetId="1" r:id="rId1"/><sheet name="Two" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
    let relationships = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
    let duplicate = rewrite_package(
        &base,
        &[
            ("xl/workbook.xml", workbook.to_owned()),
            ("xl/_rels/workbook.xml.rels", relationships.to_owned()),
        ],
        &[],
    );
    assert!(matches!(
        convert(&duplicate, &ConversionOptions::default()),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn nonworksheet_sheet_relationships_are_consistently_unsupported_before_calamine() {
    for (relationship_kind, directory, content_type, root) in [
        ("chartsheet", "chartsheets", XML_CHARTSHEET_CT, "chartsheet"),
        ("dialogsheet", "dialogsheets", XML_DIALOGSHEET_CT, "dialogsheet"),
        ("macrosheet", "macrosheets", XML_MACROSHEET_CT, "macrosheet"),
    ] {
        let sheet_part = format!("xl/{directory}/sheet1.xml");
        let content_types = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="{PACKAGE_REL_CT}"/><Override PartName="/xl/workbook.xml" ContentType="{SPREADSHEET_MAIN}"/><Override PartName="/{sheet_part}" ContentType="{content_type}"/></Types>"#
        );
        let workbook_relationships = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{relationship_kind}" Target="{directory}/sheet1.xml"/></Relationships>"#
        );
        let sheet = format!(
            r#"<?xml version="1.0"?><{root} xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#
        );
        let bytes = package(&[
            ("[Content_Types].xml", content_types.as_str()),
            (
                "_rels/.rels",
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Unsupported" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            ("xl/_rels/workbook.xml.rels", workbook_relationships.as_str()),
            (sheet_part.as_str(), sheet.as_str()),
        ]);
        let best_effort = convert(&bytes, &ConversionOptions::default()).unwrap();
        assert!(
            best_effort
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "spreadsheet.extension.omitted")
        );
        let strict =
            ConversionOptions { error_policy: ErrorPolicy::Strict, ..ConversionOptions::default() };
        let error = convert(&bytes, &strict).unwrap_err();
        assert!(
            matches!(error, ConversionError::Unsupported { .. }),
            "{relationship_kind}: {error:?}"
        );

        let wrong_content_types = content_types.replace(content_type, XML_WORKSHEET_CT);
        let wrong = package(&[
            ("[Content_Types].xml", wrong_content_types.as_str()),
            (
                "_rels/.rels",
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Unsupported" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            ("xl/_rels/workbook.xml.rels", workbook_relationships.as_str()),
            (sheet_part.as_str(), sheet.as_str()),
        ]);
        assert!(matches!(
            convert(&wrong, &ConversionOptions::default()),
            Err(ConversionError::Malformed { .. })
        ));
    }

    for (relationship_kind, directory, content_type) in [
        ("chartsheet", "chartsheets", XLSB_CHARTSHEET_CT),
        ("dialogsheet", "dialogsheets", XLSB_DIALOGSHEET_CT),
        ("macrosheet", "macrosheets", XLSB_MACROSHEET_CT),
    ] {
        let mut workbook = Vec::new();
        push_xlsb_record(&mut workbook, 0x0099, &0_u64.to_le_bytes());
        let mut bundle = Vec::new();
        bundle.extend_from_slice(&0_u32.to_le_bytes());
        bundle.extend_from_slice(&1_u32.to_le_bytes());
        push_xlsb_string(&mut bundle, "rId1");
        push_xlsb_string(&mut bundle, "Unsupported");
        push_xlsb_record(&mut workbook, 0x009c, &bundle);
        push_xlsb_record(&mut workbook, 0x0090, &[]);
        push_xlsb_record(&mut workbook, 0x009d, &[]);
        let sheet_part = format!("xl/{directory}/sheet1.bin");
        let content_types = format!(
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="{PACKAGE_REL_CT}"/><Default Extension="bin" ContentType="{SPREADSHEET_BINARY_MAIN}"/><Override PartName="/{sheet_part}" ContentType="{content_type}"/></Types>"#
        );
        let root_relationships = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{ROOT_OFFICE_DOCUMENT}" Target="xl/workbook.bin"/></Relationships>"#
        );
        let workbook_relationships = format!(
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{relationship_kind}" Target="{directory}/sheet1.bin"/></Relationships>"#
        );
        let bytes = package_bytes(&[
            ("[Content_Types].xml", content_types.as_bytes()),
            ("_rels/.rels", root_relationships.as_bytes()),
            ("xl/workbook.bin", &workbook),
            ("xl/_rels/workbook.bin.rels", workbook_relationships.as_bytes()),
            (sheet_part.as_str(), &[]),
        ]);
        let error = convert(&bytes, &ConversionOptions::default()).unwrap_err();
        assert!(
            matches!(error, ConversionError::Unsupported { .. }),
            "binary {relationship_kind}: {error:?}"
        );
    }
}
