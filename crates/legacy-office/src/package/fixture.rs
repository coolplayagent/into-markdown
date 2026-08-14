use crate::NormalizedFormat;
use std::io::{Cursor, Write as _};

pub(crate) fn fixture_package(format: NormalizedFormat) -> Result<Vec<u8>, ()> {
    let (main_part, main_type) = super::expected_authority(format);
    let content_types = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/{main_part}" ContentType="{main_type}"/></Types>"#
    );
    let relationships = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{main_part}"/></Relationships>"#
    );
    let main = match format {
        NormalizedFormat::Docx => {
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#
        }
        NormalizedFormat::Pptx => {
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>"#
        }
        NormalizedFormat::Xlsx => {
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheets/></workbook>"#
        }
    };
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for (name, bytes) in [
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", relationships.as_bytes()),
        (main_part, main.as_bytes()),
    ] {
        writer.start_file(name, options).map_err(|_| ())?;
        writer.write_all(bytes).map_err(|_| ())?;
    }
    writer.finish().map(Cursor::into_inner).map_err(|_| ())
}
