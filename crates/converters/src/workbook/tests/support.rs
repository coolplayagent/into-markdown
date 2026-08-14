use crate::workbook::opc::package::has_extension;
use crate::workbook::orchestrator::convert_workbook;
use crate::workbook::schema::{
    CHART_CT, DRAWING_CT, MAX_EXCEL_COLUMNS, MAX_EXCEL_ROWS, PACKAGE_REL_CT, ROOT_OFFICE_DOCUMENT,
    SPREADSHEET_BINARY_MAIN, SPREADSHEET_MAIN, XLSB_STYLES_CT, XLSB_WORKSHEET_CT, XML_COMMENTS_CT,
    XML_STYLES_CT, XML_WORKSHEET_CT,
};
use into_markdown_core::{ConversionError, ConversionOptions, ConverterOutput, ExecutionContext};
use std::fmt::Write as _;
use std::io::{Cursor, Read, Write as _};
use zip::write::SimpleFileOptions;

pub(super) fn context() -> ExecutionContext {
    ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits::default(),
    )
}

pub(super) fn limited_context(memory: u64) -> ExecutionContext {
    ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        into_markdown_core::ResourceLimits {
            max_memory_bytes: memory,
            ..into_markdown_core::ResourceLimits::default()
        },
    )
}

pub(super) fn convert_with_credit(
    bytes: &[u8],
    options: &ConversionOptions,
    root: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let plan = root.available_memory_bytes();
    let mut parent = root.reserve_memory(plan)?;
    let output = {
        let credit = root.with_memory_credit(&mut parent)?;
        convert_workbook(bytes, options, &credit)
    };
    drop(parent);
    assert_eq!(root.reserved_memory_bytes(), 0);
    output
}

pub(super) fn convert(
    bytes: &[u8],
    options: &ConversionOptions,
) -> Result<ConverterOutput, ConversionError> {
    let root = ExecutionContext::new(
        into_markdown_core::ExecutionOptions::default(),
        options.limits.clone(),
    );
    convert_with_credit(bytes, options, &root)
}

pub(super) fn package(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut output);
        for (name, data) in parts {
            zip.start_file(
                *name,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    output.into_inner()
}

pub(super) fn package_bytes(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut output);
        for (name, data) in parts {
            zip.start_file(
                *name,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }
    output.into_inner()
}

pub(super) fn push_xlsb_record(output: &mut Vec<u8>, typ: u16, payload: &[u8]) {
    fn push_varint(output: &mut Vec<u8>, mut value: u32) {
        loop {
            let mut byte = u8::try_from(value & 0x7f).unwrap();
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            output.push(byte);
            if value == 0 {
                break;
            }
        }
    }
    push_varint(output, u32::from(typ));
    push_varint(output, u32::try_from(payload.len()).unwrap());
    output.extend_from_slice(payload);
}

pub(super) fn push_xlsb_string(output: &mut Vec<u8>, value: &str) {
    let units = value.encode_utf16().collect::<Vec<_>>();
    output.extend_from_slice(&u32::try_from(units.len()).unwrap().to_le_bytes());
    for unit in units {
        output.extend_from_slice(&unit.to_le_bytes());
    }
}

pub(super) fn rewrite_package(
    bytes: &[u8],
    replacements: &[(&str, String)],
    additions: &[(&str, String)],
) -> Vec<u8> {
    let mut source = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut output = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut output);
        for index in 0..source.len() {
            let mut entry = source.by_index(index).unwrap();
            let name = entry.name().to_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            if let Some((_, replacement)) =
                replacements.iter().find(|(candidate, _)| *candidate == name)
            {
                data = replacement.as_bytes().to_vec();
            }
            writer
                .start_file(
                    name,
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
            writer.write_all(&data).unwrap();
        }
        for (name, data) in additions {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
            writer.write_all(data.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }
    output.into_inner()
}

pub(super) fn add_binary_parts(bytes: &[u8], additions: &[(&str, &[u8])]) -> Vec<u8> {
    let mut source = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut output = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut output);
        for index in 0..source.len() {
            let mut entry = source.by_index(index).unwrap();
            let name = entry.name().to_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            writer
                .start_file(
                    name,
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
            writer.write_all(&data).unwrap();
        }
        for (name, data) in additions {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }
    output.into_inner()
}

pub(super) fn xlsx(sheet: &str) -> Vec<u8> {
    xlsx_with_parts(sheet, None, &[])
}

pub(super) fn xlsb_package_with_relationships(
    sheet: &[u8],
    sheet_relationships: Option<&str>,
) -> Vec<u8> {
    let mut workbook = Vec::new();
    push_xlsb_record(&mut workbook, 0x0099, &0_u64.to_le_bytes());
    let mut bundle = Vec::new();
    bundle.extend_from_slice(&0_u32.to_le_bytes());
    bundle.extend_from_slice(&1_u32.to_le_bytes());
    push_xlsb_string(&mut bundle, "rId1");
    push_xlsb_string(&mut bundle, "Stale");
    push_xlsb_record(&mut workbook, 0x009c, &bundle);
    push_xlsb_record(&mut workbook, 0x0090, &[]);
    push_xlsb_record(&mut workbook, 0x009d, &[]);

    let mut styles = Vec::new();
    push_xlsb_record(&mut styles, 0x0267, &0_u32.to_le_bytes());
    push_xlsb_record(&mut styles, 0x0268, &[]);
    push_xlsb_record(&mut styles, 0x0269, &1_u32.to_le_bytes());
    push_xlsb_record(&mut styles, 0x002f, &[0; 16]);
    push_xlsb_record(&mut styles, 0x026a, &[]);

    let content_types = format!(
        r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="{PACKAGE_REL_CT}"/><Default Extension="bin" ContentType="{SPREADSHEET_BINARY_MAIN}"/><Override PartName="/xl/worksheets/sheet1.bin" ContentType="{XLSB_WORKSHEET_CT}"/><Override PartName="/xl/styles.bin" ContentType="{XLSB_STYLES_CT}"/></Types>"#
    );
    let root_rels = format!(
        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{ROOT_OFFICE_DOCUMENT}" Target="xl/workbook.bin"/></Relationships>"#
    );
    let workbook_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.bin"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.bin"/></Relationships>"#;
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", root_rels.as_bytes()),
        ("xl/workbook.bin", &workbook),
        ("xl/_rels/workbook.bin.rels", workbook_rels.as_bytes()),
        ("xl/styles.bin", &styles),
        ("xl/worksheets/sheet1.bin", sheet),
    ];
    if let Some(relationships) = sheet_relationships {
        parts.push(("xl/worksheets/_rels/sheet1.bin.rels", relationships.as_bytes()));
    }
    package_bytes(&parts)
}

pub(super) fn xlsb_package(sheet: &[u8]) -> Vec<u8> {
    xlsb_package_with_relationships(sheet, None)
}

pub(super) fn stale_dimension_xlsb() -> Vec<u8> {
    let mut sheet = Vec::new();
    push_xlsb_record(&mut sheet, 0x0081, &[]);
    let dimension = [
        0_u32.to_le_bytes(),
        (MAX_EXCEL_ROWS - 1).to_le_bytes(),
        0_u32.to_le_bytes(),
        (MAX_EXCEL_COLUMNS - 1).to_le_bytes(),
    ]
    .concat();
    push_xlsb_record(&mut sheet, 0x0094, &dimension);
    push_xlsb_record(&mut sheet, 0x0091, &[]);
    push_xlsb_record(&mut sheet, 0x0092, &[]);
    push_xlsb_record(&mut sheet, 0x0082, &[]);
    xlsb_package(&sheet)
}

pub(super) fn xlsx_with_parts(
    sheet: &str,
    sheet_relationships: Option<&str>,
    extras: &[(&str, &str)],
) -> Vec<u8> {
    let mut overrides = String::new();
    for (name, _) in extras {
        let content_type = if has_extension(name, "rels") {
            continue;
        } else if name.ends_with("comments1.xml") {
            XML_COMMENTS_CT
        } else if name.contains("/charts/") {
            CHART_CT
        } else if name.contains("/drawings/") {
            DRAWING_CT
        } else {
            continue;
        };
        write!(&mut overrides, r#"<Override PartName="/{name}" ContentType="{content_type}"/>"#)
            .unwrap();
    }
    let content_types = format!(
        r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>{overrides}</Types>"#
    );
    let mut parts = vec![
        ("[Content_Types].xml", content_types.as_str()),
        (
            "_rels/.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Visible" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        ),
        (
            "xl/styles.xml",
            r#"<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="0"/><fonts count="1"><font><b/><i/></font></fonts><fills count="1"><fill><patternFill patternType="none"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0"/></cellStyleXfs><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14" fontId="0" applyNumberFormat="1"/></cellXfs></styleSheet>"#,
        ),
        ("xl/worksheets/sheet1.xml", sheet),
    ];
    if let Some(relationships) = sheet_relationships {
        parts.push(("xl/worksheets/_rels/sheet1.xml.rels", relationships));
    }
    parts.extend_from_slice(extras);
    package(&parts)
}

pub(super) fn image_xlsx(png: &[u8]) -> Vec<u8> {
    let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><dimension ref="C4"/><sheetData/><drawing r:id="rId1"/></worksheet>"#;
    let sheet_relationships = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/></Relationships>"#;
    let drawing = r#"<?xml version="1.0"?><xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><xdr:oneCellAnchor><xdr:from><xdr:col>0</xdr:col><xdr:row>0</xdr:row></xdr:from><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="1" name="Picture" descr="first"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="rIdImage"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:oneCellAnchor><xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>1</xdr:row></xdr:from><xdr:to><xdr:col>2</xdr:col><xdr:row>3</xdr:row></xdr:to><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="Picture 2" descr="second"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="rIdImage"/></xdr:blipFill></xdr:pic><xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#;
    let drawing_relationships = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/></Relationships>"#;
    let base = xlsx_with_parts(
        sheet,
        Some(sheet_relationships),
        &[
            ("xl/drawings/drawing1.xml", drawing),
            ("xl/drawings/_rels/drawing1.xml.rels", drawing_relationships),
        ],
    );
    let content_types = format!(
        r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="{PACKAGE_REL_CT}"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/xl/workbook.xml" ContentType="{SPREADSHEET_MAIN}"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="{XML_WORKSHEET_CT}"/><Override PartName="/xl/styles.xml" ContentType="{XML_STYLES_CT}"/><Override PartName="/xl/drawings/drawing1.xml" ContentType="{DRAWING_CT}"/></Types>"#
    );
    let base = rewrite_package(&base, &[("[Content_Types].xml", content_types)], &[]);
    add_binary_parts(&base, &[("xl/media/image1.png", png), ("xl/media/orphan.png", png)])
}

pub(super) fn ordinary_xlsb_formula_record() -> Vec<u8> {
    let tokens = [0x1e, 1, 0, 0x1e, 2, 0, 0x03];
    let mut formula = vec![0; 8];
    formula.extend_from_slice(&3_f64.to_le_bytes());
    formula.extend_from_slice(&0_u16.to_le_bytes());
    formula.extend_from_slice(&u32::try_from(tokens.len()).unwrap().to_le_bytes());
    formula.extend_from_slice(&tokens);
    formula
}

pub(super) fn complete_xlsb_formula_container(
    position: &str,
    typ: u16,
    duplicate: bool,
) -> Vec<u8> {
    let mut sheet = Vec::new();
    if position == "first" {
        push_xlsb_record(&mut sheet, typ, &[0xaa]);
        return sheet;
    }
    push_xlsb_record(&mut sheet, 0x0081, &[]);
    if position == "before-dimension" {
        push_xlsb_record(&mut sheet, typ, &[0xaa]);
        return sheet;
    }
    push_xlsb_record(&mut sheet, 0x0094, &[0; 16]);
    push_xlsb_record(&mut sheet, 0x0091, &[]);
    let mut row = [0_u8; 17];
    row[8..10].copy_from_slice(&300_u16.to_le_bytes());
    push_xlsb_record(&mut sheet, 0x0000, &row);
    push_xlsb_record(&mut sheet, 0x0009, &ordinary_xlsb_formula_record());
    if position == "sheet-data" {
        push_xlsb_record(&mut sheet, typ, &[0xaa]);
        if duplicate {
            push_xlsb_record(&mut sheet, typ, &[0xbb]);
        }
        return sheet;
    }
    push_xlsb_record(&mut sheet, 0x0092, &[]);
    if position == "after-sheet-data" {
        push_xlsb_record(&mut sheet, typ, &[0xaa]);
        if duplicate {
            push_xlsb_record(&mut sheet, typ, &[0xbb]);
        }
        return sheet;
    }
    push_xlsb_record(&mut sheet, 0x0082, &[]);
    push_xlsb_record(&mut sheet, typ, &[0xaa]);
    sheet
}
