use super::super::convert_presentation;
use super::super::schema::{
    A_NS, C_NS, CHART_REL, IMAGE_REL, NOTES_REL, OFFICE_REL, P_NS, R_NS, REL_NS, REL_PREFIX,
    SLIDE_REL, TYPES_NS,
};
use crate::docx::png_crc32;
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, ErrorPolicy, ExecutionContext,
    ExecutionOptions,
};
use std::fmt::Write as _;
use std::future::Future;
use std::io::{Cursor, Read, Write};
use std::task::{Context, Poll, Waker};

pub(super) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut task = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut task) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;")
}

pub(super) fn zip(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    {
        let mut archive = zip::ZipWriter::new(&mut output);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in parts {
            archive.start_file(*name, options).unwrap();
            archive.write_all(bytes).unwrap();
        }
        archive.finish().unwrap();
    }
    output.into_inner()
}

pub(super) fn deflated_zip(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    {
        let mut archive = zip::ZipWriter::new(&mut output);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in parts {
            archive.start_file(*name, options).unwrap();
            archive.write_all(bytes).unwrap();
        }
        archive.finish().unwrap();
    }
    output.into_inner()
}

pub(super) fn zip_with_directories(parts: &[(&str, Vec<u8>)], directories: &[&str]) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    {
        let mut archive = zip::ZipWriter::new(&mut output);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for directory in directories {
            archive.add_directory(*directory, options).unwrap();
        }
        for (name, bytes) in parts {
            archive.start_file(*name, options).unwrap();
            archive.write_all(bytes).unwrap();
        }
        archive.finish().unwrap();
    }
    output.into_inner()
}

pub(super) fn force_zip64_end(mut bytes: Vec<u8>) -> Vec<u8> {
    let end_record =
        bytes.windows(4).rposition(|window| window == b"PK\x05\x06").expect("ZIP end record");
    assert_eq!(end_record + 22, bytes.len());
    let entries = u16::from_le_bytes([bytes[end_record + 10], bytes[end_record + 11]]);
    let central_size =
        u32::from_le_bytes(bytes[end_record + 12..end_record + 16].try_into().unwrap());
    let central_offset =
        u32::from_le_bytes(bytes[end_record + 16..end_record + 20].try_into().unwrap());

    let mut zip64 = Vec::with_capacity(76);
    zip64.extend_from_slice(b"PK\x06\x06");
    zip64.extend_from_slice(&44_u64.to_le_bytes());
    zip64.extend_from_slice(&45_u16.to_le_bytes());
    zip64.extend_from_slice(&45_u16.to_le_bytes());
    zip64.extend_from_slice(&0_u32.to_le_bytes());
    zip64.extend_from_slice(&0_u32.to_le_bytes());
    zip64.extend_from_slice(&u64::from(entries).to_le_bytes());
    zip64.extend_from_slice(&u64::from(entries).to_le_bytes());
    zip64.extend_from_slice(&u64::from(central_size).to_le_bytes());
    zip64.extend_from_slice(&u64::from(central_offset).to_le_bytes());
    zip64.extend_from_slice(b"PK\x06\x07");
    zip64.extend_from_slice(&0_u32.to_le_bytes());
    zip64.extend_from_slice(&u64::try_from(end_record).unwrap().to_le_bytes());
    zip64.extend_from_slice(&1_u32.to_le_bytes());

    bytes[end_record + 8..end_record + 12].fill(0xff);
    bytes[end_record + 12..end_record + 20].fill(0xff);
    let end = bytes.split_off(end_record);
    bytes.extend_from_slice(&zip64);
    bytes.extend_from_slice(&end);
    bytes
}

pub(super) fn mark_zip_entry_symlink(mut bytes: Vec<u8>, target: &str) -> Vec<u8> {
    let mut cursor = 0_usize;
    while cursor + 46 <= bytes.len() {
        if bytes[cursor..].starts_with(b"PK\x01\x02") {
            let name_len =
                usize::from(u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]));
            let extra_len =
                usize::from(u16::from_le_bytes([bytes[cursor + 30], bytes[cursor + 31]]));
            let comment_len =
                usize::from(u16::from_le_bytes([bytes[cursor + 32], bytes[cursor + 33]]));
            let name_start = cursor + 46;
            let name_end = name_start + name_len;
            if name_end <= bytes.len() && &bytes[name_start..name_end] == target.as_bytes() {
                bytes[cursor + 5] = 3;
                let attributes = (0o120_777_u32 << 16).to_le_bytes();
                bytes[cursor + 38..cursor + 42].copy_from_slice(&attributes);
                return bytes;
            }
            cursor = name_end.saturating_add(extra_len).saturating_add(comment_len);
        } else {
            cursor += 1;
        }
    }
    panic!("central directory entry not found")
}

pub(super) fn mark_zip_encrypted(mut bytes: Vec<u8>) -> Vec<u8> {
    let mut cursor = 0_usize;
    while cursor + 10 <= bytes.len() {
        if bytes[cursor..].starts_with(b"PK\x03\x04") {
            bytes[cursor + 6] |= 1;
            cursor += 30;
        } else if bytes[cursor..].starts_with(b"PK\x01\x02") {
            bytes[cursor + 8] |= 1;
            cursor += 46;
        } else {
            cursor += 1;
        }
    }
    bytes
}

pub(super) fn corrupt_stored_entry(mut bytes: Vec<u8>, target: &str) -> Vec<u8> {
    let mut cursor = 0_usize;
    while cursor + 30 <= bytes.len() {
        if bytes[cursor..].starts_with(b"PK\x03\x04") {
            let name_len =
                usize::from(u16::from_le_bytes([bytes[cursor + 26], bytes[cursor + 27]]));
            let extra_len =
                usize::from(u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]));
            let compressed_len = usize::try_from(u32::from_le_bytes([
                bytes[cursor + 18],
                bytes[cursor + 19],
                bytes[cursor + 20],
                bytes[cursor + 21],
            ]))
            .unwrap();
            let name_start = cursor + 30;
            let name_end = name_start + name_len;
            let data_start = name_end + extra_len;
            if &bytes[name_start..name_end] == target.as_bytes() {
                assert!(compressed_len != 0);
                bytes[data_start] ^= 1;
                return bytes;
            }
            cursor = data_start.saturating_add(compressed_len);
        } else {
            cursor += 1;
        }
    }
    panic!("local ZIP entry not found")
}

pub(super) fn corrupt_central_entry_name_utf8(mut bytes: Vec<u8>, target: &str) -> Vec<u8> {
    let mut cursor = 0_usize;
    while cursor + 46 <= bytes.len() {
        if bytes[cursor..].starts_with(b"PK\x01\x02") {
            let name_len =
                usize::from(u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]));
            let name_start = cursor + 46;
            let name_end = name_start + name_len;
            if &bytes[name_start..name_end] == target.as_bytes() {
                bytes[name_start] = 0xff;
                return bytes;
            }
            let extra_len =
                usize::from(u16::from_le_bytes([bytes[cursor + 30], bytes[cursor + 31]]));
            let comment_len =
                usize::from(u16::from_le_bytes([bytes[cursor + 32], bytes[cursor + 33]]));
            cursor = name_end.saturating_add(extra_len).saturating_add(comment_len);
        } else {
            cursor += 1;
        }
    }
    panic!("central ZIP entry not found")
}

pub(super) fn rewrite_part(bytes: &[u8], part: &str, replacement: &[u8]) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut owned = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        if name == part {
            value = replacement.to_vec();
        }
        owned.push((name, value));
    }
    let refs = owned.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    zip(&refs)
}

pub(super) fn append_parts(bytes: &[u8], additional: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut owned = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        owned.push((name, value));
    }
    owned.extend(additional.iter().map(|(name, bytes)| ((*name).to_owned(), bytes.clone())));
    let refs = owned.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    zip(&refs)
}

fn append_png_chunk(output: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    output.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    output.extend_from_slice(&kind);
    output.extend_from_slice(data);
    let mut checked = kind.to_vec();
    checked.extend_from_slice(data);
    output.extend_from_slice(&png_crc32(&checked).to_be_bytes());
}

pub(super) fn valid_png() -> Vec<u8> {
    let mut output = b"\x89PNG\r\n\x1a\n".to_vec();
    append_png_chunk(&mut output, *b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
    append_png_chunk(&mut output, *b"IDAT", &[0x78, 0x9c, 0x63, 0x60, 0, 0, 0, 2, 0, 1]);
    append_png_chunk(&mut output, *b"IEND", &[]);
    output
}

pub(super) fn unique_png(value: u32) -> Vec<u8> {
    let mut output = valid_png();
    let iend = output.split_off(output.len() - 12);
    let mut text = b"id\0".to_vec();
    text.extend_from_slice(&value.to_le_bytes());
    append_png_chunk(&mut output, *b"tEXt", &text);
    output.extend_from_slice(&iend);
    output
}

pub(super) fn large_valid_png(payload_bytes: usize) -> Vec<u8> {
    let mut output = valid_png();
    let iend = output.split_off(output.len() - 12);
    let mut text = Vec::new();
    text.try_reserve_exact(payload_bytes).unwrap();
    text.extend_from_slice(b"large\0");
    text.resize(payload_bytes, b'x');
    append_png_chunk(&mut output, *b"tEXt", &text);
    output.extend_from_slice(&iend);
    output
}

pub(super) fn valid_jpeg() -> Vec<u8> {
    let mut output = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut output)
        .encode(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    output
}

pub(super) fn fixture(main_type: &str, extra: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let types = format!(
        r#"<Types xmlns="{types}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="{main_type}"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>"#,
        types = String::from_utf8_lossy(TYPES_NS)
    );
    let root = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{office}" Target="ppt/presentation.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        office = OFFICE_REL
    );
    let presentation = format!(
        r#"<p:presentation xmlns:p="{p}" xmlns:r="{r}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#,
        p = String::from_utf8_lossy(P_NS),
        r = String::from_utf8_lossy(R_NS)
    );
    let rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{prefix}slide" Target="slides/slide1.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:r="{r}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr b="1" lang="zh-CN"/><a:t>{zh}</a:t></a:r><a:r><a:rPr i="true" lang="ru-RU"/><a:t>{ru}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        r = String::from_utf8_lossy(R_NS),
        zh = xml_escape("你好 – "),
        ru = xml_escape("Привет")
    );
    let mut parts = vec![
        ("[Content_Types].xml", types.into_bytes()),
        ("_rels/.rels", root.into_bytes()),
        ("ppt/presentation.xml", presentation.into_bytes()),
        ("ppt/_rels/presentation.xml.rels", rels.into_bytes()),
        ("ppt/slides/slide1.xml", slide.into_bytes()),
    ];
    parts.extend(extra.iter().cloned());
    zip(&parts)
}

pub(super) fn picture_fixture(images: &[(&str, &str, Vec<u8>)]) -> Vec<u8> {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let mut archive = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
    let mut parts = Vec::<(String, Vec<u8>)>::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        if name == "[Content_Types].xml" {
            let xml = String::from_utf8(value).unwrap();
            value = xml
                .replace(
                    "</Types>",
                    r#"<Default Extension="png" ContentType="image/png"/></Types>"#,
                )
                .into_bytes();
        }
        parts.push((name, value));
    }
    let mut slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:r="{r}"><p:cSld><p:spTree>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        r = String::from_utf8_lossy(R_NS)
    );
    let mut relationships =
        format!(r#"<Relationships xmlns="{}">"#, String::from_utf8_lossy(REL_NS));
    for (index, (id, filename, bytes)) in images.iter().enumerate() {
        write!(
                slide,
                r#"<p:pic><p:nvPicPr><p:cNvPr id="{}" name="{}"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="{}"/></p:blipFill><p:spPr/></p:pic>"#,
                index + 2,
                id,
                id
            )
            .unwrap();
        write!(
            relationships,
            r#"<Relationship Id="{id}" Type="{IMAGE_REL}" Target="../media/{filename}"/>"#,
        )
        .unwrap();
        parts.push((format!("ppt/media/{filename}"), bytes.clone()));
    }
    slide.push_str("</p:spTree></p:cSld></p:sld>");
    relationships.push_str("</Relationships>");
    for (name, value) in &mut parts {
        if name == "ppt/slides/slide1.xml" {
            *value = slide.as_bytes().to_vec();
        }
    }
    parts.push(("ppt/slides/_rels/slide1.xml.rels".into(), relationships.into_bytes()));
    let refs = parts.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    zip(&refs)
}

pub(super) fn retained_lease_fixture() -> Vec<u8> {
    let long_text = xml_escape(&"retained output ".repeat(2_048));
    let types = format!(
        r#"<Types xmlns="{types}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Default Extension="bin" ContentType="application/octet-stream"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/><Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/><Override PartName="/ppt/slides/slide3.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/><Override PartName="/ppt/notesSlides/notesSlide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/><Override PartName="/ppt/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/></Types>"#,
        types = String::from_utf8_lossy(TYPES_NS)
    );
    let root = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="main" Type="{office}" Target="ppt/presentation.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        office = OFFICE_REL
    );
    let presentation = format!(
        r#"<p:presentation xmlns:p="{p}" xmlns:r="{r}"><p:sldIdLst><p:sldId id="256" r:id="slide1"/><p:sldId id="257" r:id="slide2"/><p:sldId id="258" r:id="slide3"/></p:sldIdLst></p:presentation>"#,
        p = String::from_utf8_lossy(P_NS),
        r = String::from_utf8_lossy(R_NS)
    );
    let presentation_rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="slide1" Type="{slide}" Target="slides/slide1.xml"/><Relationship Id="slide2" Type="{slide}" Target="slides/slide2.xml"/><Relationship Id="slide3" Type="{slide}" Target="slides/slide3.xml"/><Relationship Id="macro" Type="{prefix}vbaProject" Target="vba/renamed.bin"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        slide = SLIDE_REL,
        prefix = REL_PREFIX
    );
    let slide1_rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="image" Type="{image}" Target="../media/image1.png"/><Relationship Id="chart" Type="{chart}" Target="../charts/chart1.xml"/><Relationship Id="notes" Type="{notes}" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        image = IMAGE_REL,
        chart = CHART_REL,
        notes = NOTES_REL
    );
    let slide1 = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:c="{c}" xmlns:r="{r}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="3657600" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr b="true" lang="en-US"/><a:t>{long_text}</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:nvPicPr><p:cNvPr id="3" name="Image" descr="retained image"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="1828800"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr></p:pic><p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="4" name="Chart"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="1828800" y="1828800"/><a:ext cx="1828800" cy="914400"/></p:xfrm><a:graphic><a:graphicData><c:chart r:id="chart"/></a:graphicData></a:graphic></p:graphicFrame></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        c = String::from_utf8_lossy(C_NS),
        r = String::from_utf8_lossy(R_NS)
    );
    let slide2 = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="List"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="1828800" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr lvl="0"><a:buChar char="•"/></a:pPr><a:r><a:t>first item</a:t></a:r></a:p><a:p><a:pPr lvl="1"><a:buAutoNum type="arabicPeriod" startAt="2"/></a:pPr><a:r><a:rPr i="1" lang="fr-FR"/><a:t>nested item</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Text"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm rot="2700000"><a:off x="2743200" y="0"/><a:ext cx="1828800" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>rotated retained text</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let slide3 = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" show="false"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Hidden"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>hidden slide text</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let notes = format!(
        r#"<p:notes xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Notes"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>retained speaker notes</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let chart = format!(
        r#"<c:chartSpace xmlns:c="{c}"><c:chart><c:plotArea><c:ser><c:tx><c:v>retained chart</c:v></c:tx><c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>7</c:v></c:pt><c:pt idx="1"><c:v>11</c:v></c:pt></c:numCache></c:numRef></c:val></c:ser></c:plotArea></c:chart></c:chartSpace>"#,
        c = String::from_utf8_lossy(C_NS)
    );
    zip(&[
        ("[Content_Types].xml", types.into_bytes()),
        ("_rels/.rels", root.into_bytes()),
        ("ppt/presentation.xml", presentation.into_bytes()),
        ("ppt/_rels/presentation.xml.rels", presentation_rels.into_bytes()),
        ("ppt/slides/slide1.xml", slide1.into_bytes()),
        ("ppt/slides/_rels/slide1.xml.rels", slide1_rels.into_bytes()),
        ("ppt/slides/slide2.xml", slide2.into_bytes()),
        ("ppt/slides/slide3.xml", slide3.into_bytes()),
        ("ppt/notesSlides/notesSlide1.xml", notes.into_bytes()),
        ("ppt/charts/chart1.xml", chart.into_bytes()),
        ("ppt/media/image1.png", valid_png()),
        ("ppt/vba/renamed.bin", b"must never be decompressed".to_vec()),
    ])
}

pub(super) fn convert(bytes: &[u8]) -> Result<ConverterOutput, ConversionError> {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    convert_presentation(bytes, &options, &context)
}

pub(super) fn convert_strict(bytes: &[u8]) -> Result<ConverterOutput, ConversionError> {
    let options =
        ConversionOptions { error_policy: ErrorPolicy::Strict, ..ConversionOptions::default() };
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    convert_presentation(bytes, &options, &context)
}
