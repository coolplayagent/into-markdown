use super::super::relationships::resolve_target;
use super::super::schema::{A_NS, LAYOUT_REL, P_NS, R_NS, REL_NS, REL_PREFIX, SLIDE_REL};
use super::support::{block_on, convert, fixture, rewrite_part, zip};
use into_markdown_core::{
    Block, ConversionError, ConversionOptions, ExecutionContext, ExecutionOptions, FormatDetector,
    FormatHint, InputFormat, ResolvedInput, SourceMetadata,
};
use into_markdown_render_markdown::render;
use std::io::{Cursor, Read};
use std::sync::Arc;

#[test]
fn converts_title_unicode_slide_and_central_renderer() {
    let bytes = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let output = convert(&bytes).unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(markdown.contains("## Slide 1: 你好 – Привет"));
    assert_eq!(markdown.matches("你好 – Привет").count(), 1);
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    assert!(blocks.is_empty());
    assert_eq!(output.document.blocks[0].provenance.locator.slide, Some(1));
    assert!(
        (output.document.blocks[0].provenance.locator.bounds.unwrap().width - 1.0).abs()
            < f32::EPSILON
    );
    assert_eq!(
        output
            .document
            .metadata
            .properties
            .get(&format!("presentation.languages.{}", output.document.blocks[0].id.0))
            .map(String::as_str),
        Some("ru-RU,zh-CN")
    );

    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let mut archive = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
    let mut slide = String::new();
    archive.by_name("ppt/slides/slide1.xml").unwrap().read_to_string(&mut slide).unwrap();
    let subtitle = slide.replace("type=\"title\"", "type=\"subTitle\"");
    let subtitle_output =
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", subtitle.as_bytes())).unwrap();
    assert!(matches!(
        &subtitle_output.document.blocks[0].block,
        Block::Slide { title: None, blocks, .. }
            if matches!(&blocks[0].block, Block::Paragraph(_))
    ));
    let shape_start = slide.find("<p:sp>").unwrap();
    let shape_end = slide.find("</p:sp>").unwrap() + "</p:sp>".len();
    let title_shape = &slide[shape_start..shape_end];
    let duplicate_title = slide.replace("</p:spTree>", &format!("{title_shape}</p:spTree>"));
    assert!(matches!(
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", duplicate_title.as_bytes())),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn multiple_slide_boundaries_and_hidden_slide_order_are_deterministic() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let presentation = format!(
        r#"<p:presentation xmlns:p="{p}" xmlns:r="{r}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#,
        p = String::from_utf8_lossy(P_NS),
        r = String::from_utf8_lossy(R_NS)
    );
    let relationships = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{prefix}slide" Target="slides/slide1.xml"/><Relationship Id="rId2" Type="{prefix}slide" Target="slides/slide2.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    let mut archive = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
    let mut parts = Vec::<(String, Vec<u8>)>::new();
    let mut second_slide = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        match name.as_str() {
            "[Content_Types].xml" => {
                let insert = r#"<Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#;
                let position = value.len() - "</Types>".len();
                value.splice(position..position, insert.bytes());
            }
            "ppt/presentation.xml" => value = presentation.as_bytes().to_vec(),
            "ppt/_rels/presentation.xml.rels" => {
                value = relationships.as_bytes().to_vec();
            }
            "ppt/slides/slide1.xml" => {
                second_slide = value.clone();
                let text = String::from_utf8(value).unwrap();
                value = text.replacen("<p:sld ", "<p:sld show=\"0\" ", 1).into_bytes();
            }
            _ => {}
        }
        parts.push((name, value));
    }
    parts.push(("ppt/slides/slide2.xml".into(), second_slide));
    let part_refs =
        parts.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    let output = convert(&zip(&part_refs)).unwrap();
    assert_eq!(output.document.blocks.len(), 1);
    assert!(matches!(output.document.blocks[0].block, Block::Slide { number: 2, .. }));
    assert_eq!(output.diagnostics[0].code, "presentation.hiddenSlideSkipped");
    assert_eq!(output.diagnostics[0].locator.as_ref().unwrap().slide, Some(1));
}

#[test]
#[allow(clippy::too_many_lines)]
fn multiple_layouts_apply_distinct_placeholder_geometry() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let presentation = format!(
        r#"<p:presentation xmlns:p="{p}" xmlns:r="{r}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#,
        p = String::from_utf8_lossy(P_NS),
        r = String::from_utf8_lossy(R_NS)
    );
    let presentation_rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{slide}" Target="slides/slide1.xml"/><Relationship Id="rId2" Type="{slide}" Target="slides/slide2.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        slide = SLIDE_REL
    );
    let slide = |text: &str| {
        format!(
            r#"<p:sld xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
            p = String::from_utf8_lossy(P_NS),
            a = String::from_utf8_lossy(A_NS)
        )
    };
    let layout = |x: i64| {
        format!(
            r#"<p:sldLayout xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/></p:txBody></p:sp></p:spTree></p:cSld></p:sldLayout>"#,
            p = String::from_utf8_lossy(P_NS),
            a = String::from_utf8_lossy(A_NS)
        )
    };
    let slide_relationship = |layout_number: u8| {
        format!(
            r#"<Relationships xmlns="{rels}"><Relationship Id="layout" Type="{layout}" Target="../slideLayouts/slideLayout{layout_number}.xml"/></Relationships>"#,
            rels = String::from_utf8_lossy(REL_NS),
            layout = LAYOUT_REL
        )
    };
    let mut archive = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
    let mut parts = Vec::<(String, Vec<u8>)>::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        match name.as_str() {
            "[Content_Types].xml" => {
                let extra = r#"<Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/slideLayouts/slideLayout2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>"#;
                let position = value.len() - "</Types>".len();
                value.splice(position..position, extra.bytes());
            }
            "ppt/presentation.xml" => value = presentation.as_bytes().into(),
            "ppt/_rels/presentation.xml.rels" => {
                value = presentation_rels.as_bytes().into();
            }
            "ppt/slides/slide1.xml" => value = slide("First layout").into_bytes(),
            _ => {}
        }
        parts.push((name, value));
    }
    parts.extend([
        ("ppt/slides/slide2.xml".into(), slide("Second layout").into_bytes()),
        ("ppt/slides/_rels/slide1.xml.rels".into(), slide_relationship(1).into_bytes()),
        ("ppt/slides/_rels/slide2.xml.rels".into(), slide_relationship(2).into_bytes()),
        ("ppt/slideLayouts/slideLayout1.xml".into(), layout(914_400).into_bytes()),
        ("ppt/slideLayouts/slideLayout2.xml".into(), layout(4_572_000).into_bytes()),
    ]);
    let refs = parts.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    let output = convert(&zip(&refs)).unwrap();
    assert_eq!(output.document.blocks.len(), 2);
    for (slide_index, expected_x) in [1.0_f32, 5.0].into_iter().enumerate() {
        let Block::Slide { blocks, .. } = &output.document.blocks[slide_index].block else {
            panic!()
        };
        assert!(blocks.is_empty());
        assert!(
            (output.document.blocks[slide_index].provenance.locator.bounds.unwrap().x - expected_x)
                .abs()
                < f32::EPSILON
        );
    }
}

#[test]
fn all_five_extensions_are_detected_and_converted_by_default_components() {
    let cases = [
        (
            "pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        ),
        ("pptm", "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml"),
        ("ppsx", "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml"),
        ("ppsm", "application/vnd.ms-powerpoint.slideshow.macroEnabled.main+xml"),
        ("potx", "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml"),
    ];
    for (extension, content_type) in cases {
        assert_eq!(InputFormat::from_extension(extension), Some(InputFormat::Pptx));
        let bytes = fixture(content_type, &[]);
        let input = ResolvedInput {
            bytes: Arc::from(bytes.clone()),
            metadata: SourceMetadata {
                name: Some(format!("deck.{extension}")),
                size: u64::try_from(bytes.len()).unwrap(),
                ..SourceMetadata::default()
            },
        };
        let context =
            ExecutionContext::new(ExecutionOptions::default(), ConversionOptions::default().limits);
        let hint_candidates =
            block_on(crate::HintFormatDetector.detect(&input, &FormatHint::default(), &context))
                .unwrap();
        assert!(hint_candidates.iter().any(|value| value.format == InputFormat::Pptx));
        let content_candidates =
            block_on(crate::ContentFormatDetector.detect(&input, &FormatHint::default(), &context))
                .unwrap();
        assert!(content_candidates.iter().any(|value| value.format == InputFormat::Pptx));
        assert!(convert(&bytes).is_ok());
    }
    let empty_template = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml",
        &[],
    );
    let empty_presentation =
        format!(r#"<p:presentation xmlns:p="{}"/>"#, String::from_utf8_lossy(P_NS));
    let empty_template =
        rewrite_part(&empty_template, "ppt/presentation.xml", empty_presentation.as_bytes());
    assert!(convert(&empty_template).unwrap().document.blocks.is_empty());
    assert_eq!(
        crate::planned_formats()
            .iter()
            .find(|format| format.format == InputFormat::Pptx)
            .unwrap()
            .status,
        crate::FormatStatus::Available
    );
}

#[test]
fn macro_payload_is_never_decompressed() {
    let main = "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml";
    let dangerous = [("ppt/vbaProject.bin", vec![0_u8; 2 * 1024 * 1024])];
    let mut bytes = fixture(main, &dangerous);
    // Add an authoritative macro content-type override; the large payload is skipped before open.
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
    let mut parts = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        if name == "[Content_Types].xml" {
            let insert = r#"<Override PartName="/ppt/vbaProject.bin" ContentType="application/vnd.ms-office.vbaProject"/>"#;
            let pos = value.len() - 8;
            value.splice(pos..pos, insert.bytes());
        }
        parts.push((name, value));
    }
    let refs = parts.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    bytes = zip(&refs);
    let output = convert(&bytes).unwrap();
    assert_eq!(output.diagnostics[0].code, "presentation.dangerousPartsIgnored");
}

#[test]
fn rejects_encrypted_unsafe_relationships_and_doctype() {
    assert!(matches!(
        convert(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]),
        Err(ConversionError::Encrypted)
    ));
    let bad = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[("evil", b"<!DOCTYPE sld [<!ENTITY x 'x'>]>".to_vec())],
    );
    // Unreferenced XML is inert; a DOCTYPE in an interpreted slide is rejected below.
    assert!(convert(&bad).is_ok());
    let slide = format!(
        "<!DOCTYPE sld><p:sld xmlns:p=\"{}\"><p:cSld><p:spTree/></p:cSld></p:sld>",
        String::from_utf8_lossy(P_NS)
    );
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let mut archive = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
    let mut owned = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        if name == "ppt/slides/slide1.xml" {
            value = slide.as_bytes().to_vec();
        }
        owned.push((name, value));
    }
    let refs = owned.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    let bytes = zip(&refs);
    assert!(matches!(convert(&bytes), Err(ConversionError::Malformed { .. })));
    for target in [
        "../../../escape",
        "/absolute",
        "//host/object",
        "C:drive",
        "a\\b",
        "a//b",
        "./a",
        "a#fragment",
        "a?query",
    ] {
        assert!(resolve_target("ppt/slides/slide1.xml", target).is_err());
    }
}
