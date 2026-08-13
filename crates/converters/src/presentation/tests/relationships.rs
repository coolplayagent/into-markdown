use super::super::convert_presentation;
use super::super::model::Package;
use super::super::relationships::relationship_part;
use super::super::schema::{
    A_NS, C_NS, CHART_REL, IMAGE_REL, LAYOUT_REL, MASTER_REL, MC_NS, NOTES_REL, OFFICE_REL, P_NS,
    R_NS, REL_NS, REL_PREFIX, RELATIONSHIPS_CONTENT_TYPE, SLIDE_REL, THEME_REL, TYPES_NS,
};
use super::super::slides::slide_is_hidden;
use super::super::test_observer::PART_MATERIALIZATIONS;
use super::super::text::plain_text;
use super::support::{append_parts, convert, fixture, rewrite_part, valid_jpeg, valid_png, zip};
use into_markdown_core::{
    Block, CancellationToken, ConversionError, ConversionOptions, ExecutionContext,
    ExecutionOptions,
};
use into_markdown_render_markdown::render;
use std::io::{Cursor, Read};

#[test]
#[allow(clippy::too_many_lines)]
fn layout_master_theme_notes_and_chart_are_authorized_and_extracted() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let types = format!(
        r#"<Types xmlns="{types}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/><Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/><Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/><Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/><Override PartName="/ppt/notesSlides/notesSlide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/><Override PartName="/ppt/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/></Types>"#,
        types = String::from_utf8_lossy(TYPES_NS)
    );
    let slide_rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="layout" Type="{prefix}slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="notes" Type="{prefix}notesSlide" Target="../notesSlides/notesSlide1.xml"/><Relationship Id="chart" Type="{prefix}chart" Target="../charts/chart1.xml"/><Relationship Id="image" Type="{prefix}image" Target="../media/feature.png"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    let layout_rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="master" Type="{prefix}slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    let master_rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="theme" Type="{prefix}theme" Target="../theme/theme1.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    let placeholder = |root: &str, text: &str, x: i64| {
        format!(
            r#"<p:{root} xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Inherited"/><p:cNvSpPr/><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="914400"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:{root}>"#,
            p = String::from_utf8_lossy(P_NS),
            a = String::from_utf8_lossy(A_NS)
        )
    };
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:c="{c}" xmlns:r="{r}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Inherited"/><p:cNvSpPr/><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Hidden" hidden="1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>MUST NOT LEAK</a:t></a:r></a:p></p:txBody></p:sp><p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="4" name="Chart"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><a:graphic><a:graphicData><c:chart r:id="chart"/></a:graphicData></a:graphic></p:graphicFrame><p:pic><p:nvPicPr><p:cNvPr id="5" name="Image" descr="feature"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image"/></p:blipFill><p:spPr/></p:pic></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        c = String::from_utf8_lossy(C_NS),
        r = String::from_utf8_lossy(R_NS)
    );
    let slide = slide
            .replacen(
                "<p:sp><p:nvSpPr><p:cNvPr id=\"2\"",
                "<p:grpSp><p:grpSpPr><a:xfrm><a:off x=\"914400\" y=\"0\"/><a:ext cx=\"914400\" cy=\"914400\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"914400\" cy=\"914400\"/></a:xfrm></p:grpSpPr><p:sp><p:nvSpPr><p:cNvPr id=\"2\"",
                1,
            )
            .replacen(
                "</p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id=\"3\"",
                "</p:txBody></p:sp></p:grpSp><p:sp><p:nvSpPr><p:cNvPr id=\"3\"",
                1,
            )
            .replacen(
                "<a:p/></p:txBody>",
                "<a:p><a:r><a:t>Slide content</a:t></a:r></a:p></p:txBody>",
                1,
            );
    let notes = placeholder("notes", "Speaker note", 0).replacen(
            "</p:spTree>",
            r#"<p:sp><p:nvSpPr><p:cNvPr id="9" name="Slide number"/><p:cNvSpPr/><p:nvPr><p:ph type="sldNum"/></p:nvPr></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>MUST NOT LEAK NOTE PLACEHOLDER</a:t></a:r></a:p></p:txBody></p:sp></p:spTree>"#,
            1,
        );
    let layout = placeholder("sldLayout", "", 1_828_800);
    let master = placeholder("sldMaster", "Master default", 3_657_600).replacen(
        "<a:r><a:t>Master default",
        "<a:r><a:rPr b=\"1\" lang=\"en-US\"/><a:t>Master default",
        1,
    );
    let theme = format!(
        r#"<a:theme xmlns:a="{}" name="Theme"><a:themeElements/></a:theme>"#,
        String::from_utf8_lossy(A_NS)
    );
    let chart = format!(
        r#"<c:chartSpace xmlns:c="{c}"><c:chart><c:plotArea><c:ser><c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>Revenue</c:v></c:pt></c:strCache></c:strRef></c:tx><c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>42</c:v></c:pt></c:numCache></c:numRef></c:val></c:ser><c:ser><c:tx><c:v>Direct title</c:v></c:tx></c:ser></c:plotArea></c:chart></c:chartSpace>"#,
        c = String::from_utf8_lossy(C_NS)
    );
    let mut archive = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
    let mut parts = Vec::<(String, Vec<u8>)>::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        value = match name.as_str() {
            "[Content_Types].xml" => types.as_bytes().to_vec(),
            "ppt/slides/slide1.xml" => slide.as_bytes().to_vec(),
            _ => value,
        };
        parts.push((name, value));
    }
    parts.extend([
        ("ppt/slides/_rels/slide1.xml.rels".into(), slide_rels.as_bytes().to_vec()),
        ("ppt/slideLayouts/slideLayout1.xml".into(), layout.into_bytes()),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(), layout_rels.into_bytes()),
        ("ppt/slideMasters/slideMaster1.xml".into(), master.into_bytes()),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels".into(), master_rels.into_bytes()),
        ("ppt/theme/theme1.xml".into(), theme.into_bytes()),
        ("ppt/notesSlides/notesSlide1.xml".into(), notes.into_bytes()),
        ("ppt/charts/chart1.xml".into(), chart.as_bytes().to_vec()),
        ("ppt/media/feature.png".into(), valid_png()),
    ]);
    let part_refs =
        parts.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    let output = convert(&zip(&part_refs)).unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(markdown.contains("<strong>Slide content</strong>"));
    assert!(!markdown.contains("Master default"));
    assert!(markdown.contains("Speaker notes"));
    assert!(markdown.contains("Speaker note"));
    assert!(markdown.contains("Revenue"));
    assert!(markdown.contains("42"));
    assert!(markdown.contains("Direct title"));
    assert!(!markdown.contains("MUST NOT LEAK"));
    assert!(!markdown.contains("MUST NOT LEAK NOTE PLACEHOLDER"));
    assert_eq!(output.assets.len(), 1);
    assert_eq!(
        output.document.metadata.properties.get("presentation.theme.slide-1").map(String::as_str),
        Some("Theme")
    );
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let inherited = blocks.iter().find(|node| {
            matches!(&node.block, Block::Paragraph(value) if matches!(plain_text(value).as_deref(), Ok("Slide content")))
        }).unwrap();
    assert!((inherited.provenance.locator.bounds.unwrap().x - 3.0).abs() < f32::EPSILON);
    assert_eq!(
        output
            .document
            .metadata
            .properties
            .get(&format!("presentation.languages.{}", inherited.id.0))
            .map(String::as_str),
        Some("en-US")
    );
    for node in blocks.iter().filter(|node| node.provenance.locator.bounds.is_some()) {
        assert!(
            output
                .document
                .metadata
                .properties
                .contains_key(&format!("presentation.zOrder.{}", node.id.0))
        );
    }

    let missing_image_reference = slide.replacen(r#"r:embed="image""#, r#"r:embed="missing""#, 1);
    let hidden_slide_reference =
        missing_image_reference.replacen("<p:sld ", "<p:sld show=\"0\" ", 1);
    assert!(matches!(
        convert(&rewrite_part(
            &zip(&part_refs),
            "ppt/slides/slide1.xml",
            hidden_slide_reference.as_bytes()
        )),
        Err(ConversionError::Malformed { .. })
    ));
    let hidden_shape_reference = missing_image_reference.replacen(
        r#"name="Image" descr="feature""#,
        r#"name="Image" hidden="true" descr="feature""#,
        1,
    );
    assert!(matches!(
        convert(&rewrite_part(
            &zip(&part_refs),
            "ppt/slides/slide1.xml",
            hidden_shape_reference.as_bytes()
        )),
        Err(ConversionError::Malformed { .. })
    ));
    let missing_picture = format!(
        r#"<p:pic><p:nvPicPr><p:cNvPr id="99" name="Missing relation"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip xmlns:r="{}" r:embed="missing"/></p:blipFill><p:spPr/></p:pic>"#,
        String::from_utf8_lossy(R_NS)
    );
    for part_name in [
        "ppt/slideLayouts/slideLayout1.xml",
        "ppt/slideMasters/slideMaster1.xml",
        "ppt/notesSlides/notesSlide1.xml",
    ] {
        let source = parts
            .iter()
            .find(|(name, _)| name == part_name)
            .map(|(_, value)| String::from_utf8(value.clone()).unwrap())
            .unwrap();
        let with_missing_reference =
            source.replacen("</p:spTree>", &format!("{missing_picture}</p:spTree>"), 1);
        assert!(matches!(
            convert(&rewrite_part(&zip(&part_refs), part_name, with_missing_reference.as_bytes())),
            Err(ConversionError::Malformed { .. })
        ));
    }

    let wrong_chart_type =
        slide_rels.replace(&format!("{REL_PREFIX}chart"), &format!("{REL_PREFIX}image"));
    let malicious = rewrite_part(
        &zip(&part_refs),
        "ppt/slides/_rels/slide1.xml.rels",
        wrong_chart_type.as_bytes(),
    );
    assert!(matches!(convert(&malicious), Err(ConversionError::Malformed { .. })));
    let duplicate_chart_reference = slide.replace(
        r#"<c:chart r:id="chart"/>"#,
        r#"<c:chart r:id="missing"/><c:chart r:id="chart"/>"#,
    );
    assert!(matches!(
        convert(&rewrite_part(
            &zip(&part_refs),
            "ppt/slides/slide1.xml",
            duplicate_chart_reference.as_bytes()
        )),
        Err(ConversionError::Malformed { .. })
    ));
    let malformed_chart = chart.replace(
        "<c:pt idx=\"0\"><c:v>42</c:v></c:pt>",
        "<c:pt idx=\"0\"><c:plotArea><c:v>42</c:v></c:plotArea></c:pt>",
    );
    assert!(matches!(
        convert(&rewrite_part(
            &zip(&part_refs),
            "ppt/charts/chart1.xml",
            malformed_chart.as_bytes()
        )),
        Err(ConversionError::Malformed { .. })
    ));
    for malformed_cache in [
        chart.replace("</c:numCache>", "<c:pt idx=\"0\"><c:v>duplicate</c:v></c:pt></c:numCache>"),
        chart.replace("<c:pt idx=\"0\"><c:v>42", "<c:pt><c:v>42"),
        chart.replace("<c:v>42</c:v>", ""),
    ] {
        assert!(matches!(
            convert(&rewrite_part(
                &zip(&part_refs),
                "ppt/charts/chart1.xml",
                malformed_cache.as_bytes()
            )),
            Err(ConversionError::Malformed { .. })
        ));
    }

    let external_relationship = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="external" Type="{prefix}hyperlink" Target="https://example.test/resource" TargetMode="External"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    for relationship_part in ["ppt/charts/_rels/chart1.xml.rels", "ppt/theme/_rels/theme1.xml.rels"]
    {
        let mut related_parts = parts.clone();
        related_parts.push((relationship_part.into(), external_relationship.as_bytes().into()));
        let related_refs = related_parts
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect::<Vec<_>>();
        assert!(matches!(convert(&zip(&related_refs)), Err(ConversionError::Malformed { .. })));
    }
    let mut hidden_chart_parts = parts.clone();
    for (name, value) in &mut hidden_chart_parts {
        if name == "ppt/slides/slide1.xml" {
            let xml = String::from_utf8(std::mem::take(value)).unwrap();
            *value = xml.replace(r#"name="Chart""#, r#"name="Chart" hidden="true""#).into();
        }
    }
    hidden_chart_parts
        .push(("ppt/charts/_rels/chart1.xml.rels".into(), external_relationship.into_bytes()));
    let hidden_chart_refs = hidden_chart_parts
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect::<Vec<_>>();
    assert!(matches!(convert(&zip(&hidden_chart_refs)), Err(ConversionError::Malformed { .. })));
}

#[test]
#[allow(clippy::too_many_lines)]
fn images_require_relation_content_type_extension_bytes_and_deduplicate() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let types = format!(
        r#"<Types xmlns="{types}"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>"#,
        types = String::from_utf8_lossy(TYPES_NS)
    );
    let rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="img1" Type="{prefix}image" Target="../media/a.png"/><Relationship Id="img2" Type="{prefix}image" Target="../media/b.png"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:r="{r}"><p:cSld><p:spTree><p:pic><p:nvPicPr><p:cNvPr id="2" name="A" descr="first"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="img1"/></p:blipFill><p:spPr/></p:pic><p:pic><p:nvPicPr><p:cNvPr id="3" name="B" descr="second"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="img2"/></p:blipFill><p:spPr/></p:pic></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        r = String::from_utf8_lossy(R_NS)
    );
    let mut archive = zip::ZipArchive::new(Cursor::new(original.as_slice())).unwrap();
    let mut parts = Vec::<(String, Vec<u8>)>::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        value = match name.as_str() {
            "[Content_Types].xml" => types.as_bytes().to_vec(),
            "ppt/slides/slide1.xml" => slide.as_bytes().to_vec(),
            _ => value,
        };
        parts.push((name, value));
    }
    let image = valid_png();
    parts.extend([
        ("ppt/slides/_rels/slide1.xml.rels".into(), rels.into_bytes()),
        ("ppt/media/a.png".into(), image.clone()),
        ("ppt/media/b.png".into(), image),
    ]);
    let part_refs =
        parts.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    let output = convert(&zip(&part_refs)).unwrap();
    assert_eq!(output.assets.len(), 1);
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    assert_eq!(blocks.iter().filter(|block| matches!(block.block, Block::Image { .. })).count(), 2);

    let mut mixed_parts = parts.clone();
    for (name, value) in &mut mixed_parts {
        match name.as_str() {
            "[Content_Types].xml" => {
                let xml = String::from_utf8(std::mem::take(value)).unwrap();
                *value = xml
                    .replace(
                        "</Types>",
                        r#"<Default Extension="jpg" ContentType="image/jpeg"/></Types>"#,
                    )
                    .into_bytes();
            }
            "ppt/slides/_rels/slide1.xml.rels" => {
                let xml = String::from_utf8(std::mem::take(value)).unwrap();
                *value = xml.replace("b.png", "b.jpg").into_bytes();
            }
            "ppt/media/b.png" => {
                *name = "ppt/media/b.jpg".into();
                *value = valid_jpeg();
            }
            _ => {}
        }
    }
    let mixed_refs =
        mixed_parts.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    let mixed = convert(&zip(&mixed_refs)).unwrap();
    assert_eq!(mixed.assets.len(), 2);
    assert_eq!(mixed.assets[0].media_type, "image/png");
    assert_eq!(mixed.assets[1].media_type, "image/jpeg");

    let bad = rewrite_part(&zip(&part_refs), "ppt/media/a.png", b"not a png");
    assert!(matches!(convert(&bad), Err(ConversionError::Malformed { .. })));
    let duplicate_image_reference = slide.replace(
        r#"<a:blip r:embed="img1"/>"#,
        r#"<a:blip r:embed="missing"/><a:blip r:embed="img1"/>"#,
    );
    assert!(matches!(
        convert(&rewrite_part(
            &zip(&part_refs),
            "ppt/slides/slide1.xml",
            duplicate_image_reference.as_bytes()
        )),
        Err(ConversionError::Malformed { .. })
    ));
    let linked_image =
        slide.replace(r#"<a:blip r:embed="img1"/>"#, r#"<a:blip r:embed="img1" r:link="img1"/>"#);
    assert!(matches!(
        convert(&rewrite_part(&zip(&part_refs), "ppt/slides/slide1.xml", linked_image.as_bytes())),
        Err(ConversionError::Malformed { .. })
    ));
    let wrong_content_type = rewrite_part(
        &zip(&part_refs),
        "[Content_Types].xml",
        types.replace("image/png", "image/jpeg").as_bytes(),
    );
    assert!(matches!(convert(&wrong_content_type), Err(ConversionError::Malformed { .. })));
    let mut options = ConversionOptions::default();
    options.limits.max_asset_bytes = 1;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        convert_presentation(&zip(&part_refs), &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_asset_bytes", .. })
    ));
    let mut options = ConversionOptions::default();
    options.limits.max_total_asset_bytes = 1;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        convert_presentation(&zip(&part_refs), &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_total_asset_bytes", .. })
    ));
    let mut options = ConversionOptions::default();
    options.limits.max_total_asset_bytes = u64::try_from(valid_png().len()).unwrap();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        convert_presentation(&zip(&part_refs), &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_total_asset_bytes", .. })
    ));
}

#[test]
fn rejects_external_relationship_namespace_and_malformed_mc() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let external = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{prefix}slide" Target="slides/slide1.xml"/><Relationship Id="h" Type="{prefix}hyperlink" Target="https://example.test" TargetMode="External"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    assert!(matches!(
        convert(&rewrite_part(&original, "ppt/_rels/presentation.xml.rels", external.as_bytes())),
        Err(ConversionError::Malformed { .. })
    ));
    let bad_namespace = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="urn:evil"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="X"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>X</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS)
    );
    assert!(matches!(
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", bad_namespace.as_bytes())),
        Err(ConversionError::Malformed { .. })
    ));
    let bad_mc = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:mc="{mc}"><p:cSld><p:spTree><mc:AlternateContent><mc:Choice/></mc:AlternateContent></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        mc = String::from_utf8_lossy(MC_NS)
    );
    assert!(matches!(
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", bad_mc.as_bytes())),
        Err(ConversionError::Malformed { .. })
    ));
    let wrong_parent = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree><a:t>misplaced</a:t></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    assert!(matches!(
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", wrong_parent.as_bytes())),
        Err(ConversionError::Malformed { .. })
    ));
    let duplicate_relationship = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{prefix}slide" Target="slides/slide1.xml"/><Relationship Id="rId1" Type="{prefix}slide" Target="slides/slide1.xml"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    assert!(matches!(
        convert(&rewrite_part(
            &original,
            "ppt/_rels/presentation.xml.rels",
            duplicate_relationship.as_bytes()
        )),
        Err(ConversionError::Malformed { .. })
    ));
    for kind in ["hyperlink", "media", "oleObject", "package", "activeX", "image"] {
        let external_object = format!(
            r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{prefix}slide" Target="slides/slide1.xml"/><Relationship Id="object" Type="{prefix}{kind}" Target="https://example.test/object" TargetMode="External"/></Relationships>"#,
            rels = String::from_utf8_lossy(REL_NS),
            prefix = REL_PREFIX
        );
        assert!(matches!(
            convert(&rewrite_part(
                &original,
                "ppt/_rels/presentation.xml.rels",
                external_object.as_bytes()
            )),
            Err(ConversionError::Malformed { .. })
        ));
    }
}

#[test]
fn xml_references_are_exact_and_mc_choice_never_leaks() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:c="{c}" xmlns:r="{r}" xmlns:mc="{mc}" xmlns:future="urn:future"><p:cSld><p:spTree><mc:AlternateContent><mc:Choice Requires="future"><p:sp><p:nvSpPr><p:cNvPr id="2" name="LEAK"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>LEAK</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:nvPicPr><p:cNvPr id="20" name="LEAK IMAGE"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="choice-image-must-not-resolve"/></p:blipFill><p:spPr/></p:pic><p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="21" name="LEAK CHART"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><a:graphic><a:graphicData><c:chart r:id="choice-chart-must-not-resolve"/></a:graphicData></a:graphic></p:graphicFrame></mc:Choice><mc:Fallback><p:sp><p:nvSpPr><p:cNvPr id="3" name="SAFE &amp;"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>A&amp;&#x42;&#67;</a:t></a:r></a:p></p:txBody></p:sp></mc:Fallback></mc:AlternateContent></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        c = String::from_utf8_lossy(C_NS),
        r = String::from_utf8_lossy(R_NS),
        mc = String::from_utf8_lossy(MC_NS)
    );
    let output =
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", slide.as_bytes())).unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(markdown.contains("A&amp;BC"));
    assert!(!markdown.contains("LEAK"));
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    assert_eq!(blocks.len(), 1);

    for invalid in ["&#1;", "&#xD800;", "&#x110000;", "&#xZZ;", "&custom;"] {
        let bad = slide.replace("A&amp;&#x42;&#67;", invalid);
        assert!(matches!(
            convert(&rewrite_part(&original, "ppt/slides/slide1.xml", bad.as_bytes())),
            Err(ConversionError::Malformed { .. })
        ));
    }
    for invalid_attribute in ["&custom;", "&#0;", "&#xD800;", "&#x110000;", "&#xZZ;"] {
        let bad = slide.replace("SAFE &amp;", invalid_attribute);
        assert!(matches!(
            convert(&rewrite_part(&original, "ppt/slides/slide1.xml", bad.as_bytes())),
            Err(ConversionError::Malformed { .. })
        ));
    }
    for invalid_scalar in ["\0", "\u{1}", "\u{b}", "\u{fffe}"] {
        let bad = slide.replace("A&amp;&#x42;&#67;", invalid_scalar);
        assert!(matches!(
            convert(&rewrite_part(&original, "ppt/slides/slide1.xml", bad.as_bytes())),
            Err(ConversionError::Malformed { .. })
        ));
    }
    let child_inside_text = slide.replace(
        "A&amp;&#x42;&#67;",
        r#"<future:payload xmlns:future="urn:future">MUST NOT LEAK</future:payload>"#,
    );
    assert!(matches!(
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", child_inside_text.as_bytes())),
        Err(ConversionError::Malformed { .. })
    ));
    for invalid_mc in [
        slide.replace("<mc:Choice Requires=\"future\">", "<mc:Choice>"),
        slide.replace("<mc:Choice Requires=\"future\">", "<mc:Choice Requires=\"bad:prefix\">"),
        slide.replace("<mc:Choice Requires=\"future\">", "<mc:Choice Requires=\"missing\">"),
        slide.replace("<mc:Fallback>", "<mc:Fallback Requires=\"future\">"),
        slide.replace(
            "<mc:Choice Requires=\"future\">",
            "<mc:Fallback></mc:Fallback><mc:Choice Requires=\"future\">",
        ),
        slide.replace("<mc:Choice Requires=\"future\">", "<p:sp/><mc:Choice Requires=\"future\">"),
    ] {
        assert!(matches!(
            convert(&rewrite_part(&original, "ppt/slides/slide1.xml", invalid_mc.as_bytes())),
            Err(ConversionError::Malformed { .. })
        ));
    }
}

#[test]
fn mce_selects_first_understood_choice_and_never_materializes_unselected_payload() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[("ppt/media/unselected.png", vec![0_u8; 12 * 1024 * 1024])],
    );
    let rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="unused" Type="{image}" Target="../media/unselected.png"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        image = IMAGE_REL
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:r="{r}" xmlns:mc="{mc}" xmlns:future="urn:future"><p:cSld><p:spTree><mc:AlternateContent><mc:Choice Requires="future"><p:pic><p:nvPicPr><p:cNvPr id="2" name="unused"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="unused"/></p:blipFill><p:spPr/></p:pic></mc:Choice><mc:Choice Requires="p"><p:sp><p:nvSpPr><p:cNvPr id="3" name="selected"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>FIRST SUPPORTED</a:t></a:r></a:p></p:txBody></p:sp></mc:Choice><mc:Choice Requires="a"><p:sp><p:nvSpPr><p:cNvPr id="4" name="later"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>LATER MUST NOT LEAK</a:t></a:r></a:p></p:txBody></p:sp></mc:Choice></mc:AlternateContent><mc:AlternateContent><mc:Choice Requires="future"><p:sp><p:nvSpPr><p:cNvPr id="5" name="outer skip"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>OUTER MUST NOT LEAK</a:t></a:r></a:p></p:txBody></p:sp></mc:Choice><mc:Fallback><mc:AlternateContent><mc:Choice Requires="a"><p:sp><p:nvSpPr><p:cNvPr id="6" name="nested"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>NESTED SUPPORTED</a:t></a:r></a:p></p:txBody></p:sp></mc:Choice><mc:Fallback><p:sp><p:nvSpPr><p:cNvPr id="7" name="nested fallback"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:p><a:r><a:t>NESTED FALLBACK MUST NOT LEAK</a:t></a:r></a:p></p:txBody></p:sp></mc:Fallback></mc:AlternateContent></mc:Fallback></mc:AlternateContent></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        r = String::from_utf8_lossy(R_NS),
        mc = String::from_utf8_lossy(MC_NS),
    );
    let bytes = rewrite_part(&original, "ppt/slides/slide1.xml", slide.as_bytes());
    let bytes = append_parts(&bytes, &[("ppt/slides/_rels/slide1.xml.rels", rels.into_bytes())]);
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = 8 * 1024 * 1024;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    PART_MATERIALIZATIONS.with(|count| count.set(0));
    let output = convert_presentation(&bytes, &options, &context).unwrap();
    let markdown = render(&output.document, &output.assets, &options).unwrap();
    assert!(markdown.contains("FIRST SUPPORTED"));
    assert!(markdown.contains("NESTED SUPPORTED"));
    assert!(!markdown.contains("MUST NOT LEAK"));
    assert!(output.assets.is_empty());
    // Content types, root/main/slide rels and main/slide XML only; never the 12 MiB target.
    assert_eq!(PART_MATERIALIZATIONS.with(std::cell::Cell::get), 6);
    drop(output);
    assert_eq!(context.reserved_memory_bytes(), 0);

    let unsafe_unselected = slide.replace("name=\"unused\"", "name=\"&custom;\"");
    assert!(matches!(
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", unsafe_unselected.as_bytes(),)),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn hidden_slide_scan_honors_cancellation_before_parsing_events() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let context = ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        ConversionOptions::default().limits,
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{}" show="true"><p:cSld><p:spTree/></p:cSld></p:sld>"#,
        String::from_utf8_lossy(P_NS)
    );
    assert!(matches!(
        slide_is_hidden(slide.as_bytes(), "ppt/slides/slide1.xml", &context),
        Err(ConversionError::Cancelled)
    ));
}

#[test]
fn relationship_types_are_exact_and_rels_content_type_precedes_materialization() {
    let owner = "ppt/owner.xml";
    let relationship_name = relationship_part(owner).unwrap();
    let options = {
        let mut value = ConversionOptions::default();
        value.limits.max_decompressed_bytes = 4 * 1024 * 1024;
        value.limits.max_memory_bytes = 1024 * 1024;
        value
    };
    for official in
        [OFFICE_REL, SLIDE_REL, LAYOUT_REL, MASTER_REL, THEME_REL, NOTES_REL, IMAGE_REL, CHART_REL]
    {
        let suffix = official.rsplit('/').next().unwrap();
        let types = format!(
            r#"<Types xmlns="{types}"><Default Extension="rels" ContentType="{rels_type}"/><Default Extension="bin" ContentType="application/octet-stream"/></Types>"#,
            types = String::from_utf8_lossy(TYPES_NS),
            rels_type = RELATIONSHIPS_CONTENT_TYPE
        );
        let relationships = format!(
            r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="https://evil.invalid/custom/{suffix}" Target="../payload.bin"/></Relationships>"#,
            rels = String::from_utf8_lossy(REL_NS)
        );
        let parts = [
            ("[Content_Types].xml", types.into_bytes()),
            (relationship_name.as_str(), relationships.into_bytes()),
            ("payload.bin", vec![7_u8; 2 * 1024 * 1024]),
        ];
        let bytes = zip(&parts);
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut package = Package::open(&bytes, &options, &context).unwrap();
        assert!(matches!(
            package.relationships(owner, &options, &context),
            Err(ConversionError::Malformed { .. })
        ));
        assert!(!package.is_loaded("payload.bin"));
    }

    for relationship_content_type in [None, Some("application/xml")] {
        let rel_default = relationship_content_type.map_or_else(String::new, |value| {
            format!(r#"<Default Extension="rels" ContentType="{value}"/>"#)
        });
        let types = format!(
            r#"<Types xmlns="{types}">{rel_default}<Default Extension="bin" ContentType="application/octet-stream"/></Types>"#,
            types = String::from_utf8_lossy(TYPES_NS)
        );
        let relationships = format!(
            r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{slide}" Target="../payload.bin"/></Relationships>"#,
            rels = String::from_utf8_lossy(REL_NS),
            slide = SLIDE_REL
        );
        let parts = [
            ("[Content_Types].xml", types.into_bytes()),
            (relationship_name.as_str(), relationships.into_bytes()),
            ("payload.bin", vec![7_u8; 2 * 1024 * 1024]),
        ];
        let bytes = zip(&parts);
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut package = Package::open(&bytes, &options, &context).unwrap();
        assert!(matches!(
            package.relationships(owner, &options, &context),
            Err(ConversionError::Malformed { .. })
        ));
        assert!(!package.is_loaded(&relationship_name));
        assert!(!package.is_loaded("payload.bin"));
    }
}

#[test]
fn xml_booleans_and_list_levels_are_strict_for_all_interpreted_shapes() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" show="true"><p:cSld><p:spTree>
            <p:grpSp><p:grpSpPr><a:xfrm flipH="true" flipV="false"><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/><a:chOff x="0" y="0"/><a:chExt cx="914400" cy="914400"/></a:xfrm></p:grpSpPr></p:grpSp>
            <p:sp><p:nvSpPr><p:cNvPr id="2" name="Boolean" hidden="false"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm flipH="1" flipV="0"><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr lvl="8"><a:buChar char="•"/></a:pPr><a:r><a:rPr b="true" i="false" u="sng" strike="noStrike"/><a:t>Boolean text</a:t></a:r></a:p></p:txBody></p:sp>
            </p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let valid = rewrite_part(&original, "ppt/slides/slide1.xml", slide.as_bytes());
    let output = convert(&valid).unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(markdown.contains("Boolean text"));

    let hidden = slide.replace("hidden=\"false\"", "hidden=\"true\"");
    let output =
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", hidden.as_bytes())).unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(!markdown.contains("Boolean text"));

    let hidden_slide = slide.replace("show=\"true\"", "show=\"false\"");
    let output =
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", hidden_slide.as_bytes()))
            .unwrap();
    assert!(output.document.blocks.is_empty());

    let no_bullet = slide.replace(r#"<a:buChar char="•"/>"#, "<a:buNone/>");
    let output =
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", no_bullet.as_bytes())).unwrap();
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    assert!(blocks.iter().any(|node| matches!(node.block, Block::Paragraph(_))));
    assert!(!blocks.iter().any(|node| matches!(node.block, Block::List { .. })));

    for invalid in [
        slide.replace("flipH=\"true\"", "flipH=\"yes\""),
        slide.replace("flipH=\"1\"", "flipH=\"yes\""),
        slide.replace("hidden=\"false\"", "hidden=\"yes\""),
        slide.replace("b=\"true\"", "b=\"yes\""),
        slide.replace("i=\"false\"", "i=\"yes\""),
        slide.replace("u=\"sng\"", "u=\"yes\""),
        slide.replace("strike=\"noStrike\"", "strike=\"yes\""),
        slide.replace("lvl=\"8\"", "lvl=\"9\""),
        slide.replace("lvl=\"8\"", "lvl=\"-1\""),
        slide.replace("show=\"true\"", "show=\"yes\""),
        slide.replace("</a:pPr><a:r>", "</a:pPr><a:pPr/><a:r>"),
        slide.replace("<a:buChar char=\"•\"/>", "<a:buChar char=\"•\"/><a:buChar char=\"•\"/>"),
        slide.replace(
            "<a:rPr b=\"true\" i=\"false\" u=\"sng\" strike=\"noStrike\"/>",
            "<a:rPr/><a:rPr b=\"true\" i=\"false\" u=\"sng\" strike=\"noStrike\"/>",
        ),
        slide.replacen(
            "<a:off x=\"0\" y=\"0\"/>",
            "<a:off x=\"0\" y=\"0\"/><a:off x=\"0\" y=\"0\"/>",
            1,
        ),
        slide.replacen("</a:xfrm></p:spPr>", "</a:xfrm><a:xfrm/></p:spPr>", 1),
        slide.replace(r#"<a:buChar char="•"/>"#, "<a:buBlip/>"),
    ] {
        assert!(matches!(
            convert(&rewrite_part(&original, "ppt/slides/slide1.xml", invalid.as_bytes())),
            Err(ConversionError::Malformed { .. })
        ));
    }
}
