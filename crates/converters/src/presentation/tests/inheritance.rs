use super::super::budget::{ASSET_HASH_CHUNK_BYTES, MAX_ASSET_DIGEST_CANDIDATES};
use super::super::images::{asset_digest, find_duplicate_asset};
use super::super::model::{
    Geometry, GroupTransform, PlaceholderClass, PlaceholderKey, RichStyle, Shape, ShapeStyle,
    TextParagraph,
};
use super::super::schema::{
    A_NS, GEOMETRY_EXTENT, GEOMETRY_FLIP_H, GEOMETRY_FLIP_V, GEOMETRY_OFFSET, GEOMETRY_ROTATION,
    IMAGE_REL, MC_NS, P_NS, R_NS, REL_NS, RELATIONSHIPS_CONTENT_TYPE, TYPES_NS,
};
use super::super::slides::parse_shapes;
use super::super::styles::{
    MasterTextSection, apply_inheritance, apply_pending_group_transforms,
    layout_styles_from_shapes, master_styles_from_shapes, merge_master_text_styles,
    parse_master_text_styles, placeholder_class,
};
use super::super::xml::{XmlProfile, preflight_xml};
use super::support::{convert, fixture, rewrite_part, unique_png};
use into_markdown_core::{
    Asset, AssetId, CancellationToken, ConversionError, ConversionOptions, ExecutionContext,
    ExecutionOptions, Inline, InlineMark, ListKind,
};
use std::collections::HashMap;

#[test]
fn hidden_layout_and_master_placeholders_are_inherited() {
    for hidden_in_layout in [true, false] {
        let mut shapes = vec![Shape {
            placeholder: Some("body".into()),
            paragraphs: vec![TextParagraph {
                text: vec![Inline::Text { value: "placeholder".into(), marks: Vec::new() }],
                ..TextParagraph::default()
            }],
            ..Shape::default()
        }];
        let style = ShapeStyle { hidden: true, ..ShapeStyle::default() };
        let mut layout = Vec::new();
        let mut master = Vec::new();
        let key = PlaceholderKey { index: 0, class: PlaceholderClass::Body };
        if hidden_in_layout {
            layout.push((key, style));
        } else {
            layout
                .push((key, ShapeStyle { class: PlaceholderClass::Body, ..ShapeStyle::default() }));
            master.push((PlaceholderClass::Body, style));
        }
        apply_inheritance(&mut shapes, &layout, &master).unwrap();
        assert!(shapes[0].hidden);
    }

    let mut shapes = vec![Shape {
        placeholder: Some("body".into()),
        paragraphs: vec![TextParagraph { bullet_explicit: true, ..TextParagraph::default() }],
        ..Shape::default()
    }];
    let style = ShapeStyle {
        paragraphs: vec![TextParagraph {
            bullet: Some(ListKind::Ordered),
            bullet_explicit: true,
            start: 7,
            ..TextParagraph::default()
        }],
        ..ShapeStyle::default()
    };
    apply_inheritance(
        &mut shapes,
        &vec![(PlaceholderKey { index: 0, class: PlaceholderClass::Body }, style)],
        &Vec::new(),
    )
    .unwrap();
    assert_eq!(shapes[0].paragraphs[0].bullet, None);

    let mut shapes = vec![Shape {
        placeholder: Some("body".into()),
        paragraphs: vec![TextParagraph {
            text: vec![Inline::Text { value: "layered".into(), marks: Vec::new() }],
            ..TextParagraph::default()
        }],
        ..Shape::default()
    }];
    let layout = vec![(
        PlaceholderKey { index: 0, class: PlaceholderClass::Body },
        ShapeStyle {
            paragraphs: vec![TextParagraph {
                level: 2,
                level_explicit: true,
                ..TextParagraph::default()
            }],
            languages: vec!["fr-FR".into()],
            ..ShapeStyle::default()
        },
    )];
    let master = vec![(
        PlaceholderClass::Body,
        ShapeStyle {
            paragraphs: vec![TextParagraph {
                default_marks: vec![InlineMark::Bold],
                bullet: Some(ListKind::Ordered),
                bullet_explicit: true,
                start: 7,
                ..TextParagraph::default()
            }],
            languages: vec!["en-US".into()],
            ..ShapeStyle::default()
        },
    )];
    apply_inheritance(&mut shapes, &layout, &master).unwrap();
    assert_eq!(shapes[0].paragraphs[0].level, 2);
    assert_eq!(shapes[0].paragraphs[0].bullet, Some(ListKind::Ordered));
    assert_eq!(shapes[0].paragraphs[0].start, 7);
    assert_eq!(shapes[0].languages, ["en-US", "fr-FR"]);
    assert!(matches!(
        &shapes[0].paragraphs[0].text[0],
        Inline::Text { marks, .. } if marks == &[InlineMark::Bold]
    ));
}

#[test]
fn placeholder_indexes_disambiguate_layout_and_hidden_groups_propagate() {
    let layout = format!(
        r#"<p:sldLayout xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree>
            <p:grpSp><p:nvGrpSpPr><p:cNvPr id="1" name="Hidden group" hidden="true"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>
            <p:sp><p:nvSpPr><p:cNvPr id="2" name="First body"/><p:cNvSpPr/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="914400" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr lvl="2"><a:buAutoNum type="arabicPeriod" startAt="3"/><a:defRPr b="true" lang="fr-FR"/></a:pPr></a:p></p:txBody></p:sp></p:grpSp>
            <p:sp><p:nvSpPr><p:cNvPr id="3" name="Second body"/><p:cNvSpPr/><p:nvPr><p:ph type="body" idx="2"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="1828800" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody></p:sp>
            </p:spTree></p:cSld></p:sldLayout>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let styles = layout_styles_from_shapes(
        parse_shapes(
            layout.as_bytes(),
            "ppt/slideLayouts/slideLayout1.xml",
            XmlProfile::Layout,
            &options,
            &context,
        )
        .unwrap(),
        "ppt/slideLayouts/slideLayout1.xml",
    )
    .unwrap();
    assert_eq!(styles.len(), 2);
    let mut shapes = vec![
        Shape {
            placeholder: Some("body".into()),
            placeholder_index: 1,
            paragraphs: vec![TextParagraph {
                text: vec![Inline::Text { value: "First".into(), marks: Vec::new() }],
                ..TextParagraph::default()
            }],
            ..Shape::default()
        },
        Shape { placeholder: Some("body".into()), placeholder_index: 2, ..Shape::default() },
    ];
    apply_inheritance(&mut shapes, &styles, &Vec::new()).unwrap();
    apply_pending_group_transforms(&mut shapes).unwrap();
    assert!(shapes[0].hidden);
    assert!(!shapes[1].hidden);
    assert_eq!(shapes[0].geometry.x, 914_400);
    assert_eq!(shapes[1].geometry.x, 1_828_800);
    assert_eq!(shapes[0].languages, ["fr-FR"]);
    assert_eq!(shapes[0].paragraphs[0].level, 2);
    assert_eq!(shapes[0].paragraphs[0].bullet, Some(ListKind::Ordered));
    assert_eq!(shapes[0].paragraphs[0].start, 3);
    assert_eq!(shapes[0].paragraphs[0].numbering.as_deref(), Some("arabicPeriod"));
    assert!(matches!(
        &shapes[0].paragraphs[0].text[0],
        Inline::Text { marks, .. } if marks == &[InlineMark::Bold]
    ));

    for malformed_layout in [
        layout.replace("idx=\"2\"", "idx=\"-1\""),
        layout.replace("idx=\"2\"", "idx=\"1\""),
        layout.replace("type=\"body\" idx=\"2\"", "type=\"unknown\" idx=\"2\""),
        layout.replace("hidden=\"true\"", "hidden=\"yes\""),
        layout.replace(
            r#"<p:ph type="body" idx="2"/>"#,
            r#"<p:ph type="body" idx="2"/><p:ph type="body" idx="2"/>"#,
        ),
    ] {
        assert!(matches!(
            parse_shapes(
                malformed_layout.as_bytes(),
                "ppt/slideLayouts/slideLayout1.xml",
                XmlProfile::Layout,
                &options,
                &context,
            )
            .and_then(|shapes| {
                layout_styles_from_shapes(shapes, "ppt/slideLayouts/slideLayout1.xml")
            }),
            Err(ConversionError::Malformed { .. })
        ));
    }
}

#[test]
fn placeholder_layering_projects_layout_type_to_master_class() {
    assert_eq!(placeholder_class("title"), PlaceholderClass::Title);
    assert_eq!(placeholder_class("ctrTitle"), PlaceholderClass::Title);
    for body in ["obj", "body", "subTitle", "chart", "tbl", "pic", "media", "dgm", "clipArt"] {
        assert_eq!(placeholder_class(body), PlaceholderClass::Body);
    }
    assert_eq!(placeholder_class("dt"), PlaceholderClass::Date);
    assert_eq!(placeholder_class("ftr"), PlaceholderClass::Footer);
    assert_eq!(placeholder_class("sldNum"), PlaceholderClass::SlideNumber);

    let mut shapes = vec![
        Shape { placeholder: Some("obj".into()), ..Shape::default() },
        Shape { placeholder: Some("body".into()), placeholder_index: 7, ..Shape::default() },
    ];
    let layout = vec![
        (
            PlaceholderKey { index: 0, class: PlaceholderClass::Body },
            ShapeStyle {
                geometry: Some(Geometry {
                    x: 10,
                    y: 20,
                    presence: GEOMETRY_OFFSET,
                    ..Geometry::default()
                }),
                class: PlaceholderClass::Body,
                ..ShapeStyle::default()
            },
        ),
        (
            PlaceholderKey { index: 7, class: PlaceholderClass::Body },
            ShapeStyle {
                geometry: Some(Geometry {
                    x: 70,
                    y: 80,
                    presence: GEOMETRY_OFFSET,
                    ..Geometry::default()
                }),
                // The layout's type, not the slide or master idx, selects the master class.
                class: PlaceholderClass::Title,
                ..ShapeStyle::default()
            },
        ),
    ];
    let master = vec![
        (
            PlaceholderClass::Title,
            ShapeStyle {
                geometry: Some(Geometry {
                    cx: 700,
                    cy: 800,
                    presence: GEOMETRY_EXTENT,
                    ..Geometry::default()
                }),
                class: PlaceholderClass::Title,
                ..ShapeStyle::default()
            },
        ),
        (
            PlaceholderClass::Body,
            ShapeStyle {
                geometry: Some(Geometry {
                    cx: 100,
                    cy: 200,
                    presence: GEOMETRY_EXTENT,
                    ..Geometry::default()
                }),
                class: PlaceholderClass::Body,
                ..ShapeStyle::default()
            },
        ),
    ];
    apply_inheritance(&mut shapes, &layout, &master).unwrap();
    assert_eq!((shapes[0].geometry.x, shapes[0].geometry.cx), (10, 100));
    assert_eq!((shapes[1].geometry.x, shapes[1].geometry.cx), (70, 700));

    let ambiguous = vec![
        Shape { placeholder: Some("body".into()), placeholder_index: 1, ..Shape::default() },
        Shape { placeholder: Some("chart".into()), placeholder_index: 99, ..Shape::default() },
    ];
    assert!(matches!(
        master_styles_from_shapes(ambiguous, "ppt/slideMasters/slideMaster1.xml"),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn transform_presence_inherits_each_property_before_nested_groups() {
    let mut shapes = vec![Shape {
        placeholder: Some("body".into()),
        geometry: Geometry {
            cx: 0,
            cy: 0,
            rotation: 0,
            flip_h: false,
            presence: GEOMETRY_EXTENT | GEOMETRY_ROTATION | GEOMETRY_FLIP_H,
            ..Geometry::default()
        },
        pending_groups: vec![GroupTransform {
            offset_x: 100,
            offset_y: 200,
            extent_x: 2_000,
            extent_y: 2_000,
            child_extent_x: 1_000,
            child_extent_y: 1_000,
            ..GroupTransform::default()
        }],
        ..Shape::default()
    }];
    let layout = vec![(
        PlaceholderKey { index: 0, class: PlaceholderClass::Body },
        ShapeStyle {
            geometry: Some(Geometry {
                x: 0,
                y: 50,
                rotation: 900,
                flip_h: true,
                presence: GEOMETRY_OFFSET | GEOMETRY_ROTATION | GEOMETRY_FLIP_H,
                ..Geometry::default()
            }),
            class: PlaceholderClass::Body,
            ..ShapeStyle::default()
        },
    )];
    let master = vec![(
        PlaceholderClass::Body,
        ShapeStyle {
            geometry: Some(Geometry {
                x: 500,
                y: 600,
                cx: 700,
                cy: 800,
                rotation: 1_800,
                flip_h: true,
                flip_v: true,
                presence: GEOMETRY_OFFSET
                    | GEOMETRY_EXTENT
                    | GEOMETRY_ROTATION
                    | GEOMETRY_FLIP_H
                    | GEOMETRY_FLIP_V,
                ..Geometry::default()
            }),
            class: PlaceholderClass::Body,
            ..ShapeStyle::default()
        },
    )];
    apply_inheritance(&mut shapes, &layout, &master).unwrap();
    assert_eq!((shapes[0].geometry.x, shapes[0].geometry.y), (0, 50));
    assert_eq!((shapes[0].geometry.cx, shapes[0].geometry.cy), (0, 0));
    assert_eq!(shapes[0].geometry.rotation, 0);
    assert!(!shapes[0].geometry.flip_h);
    assert!(shapes[0].geometry.flip_v);
    apply_pending_group_transforms(&mut shapes).unwrap();
    let corners = shapes[0].geometry.display_corners().unwrap();
    assert!(corners.iter().all(|point| point.x.is_finite() && point.y.is_finite()));
    assert!(shapes[0].pending_groups.is_empty());
}

#[test]
fn master_text_styles_and_explicit_false_rich_properties_layer_by_level() {
    let master_xml = format!(
        r#"<p:sldMaster xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree/></p:cSld><p:txStyles><p:titleStyle><a:lvl1pPr><a:defRPr b="true" i="true"/></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr><a:defRPr b="true" i="true"/></a:lvl1pPr><a:lvl2pPr><a:buAutoNum type="arabicPeriod" startAt="4"/><a:defRPr u="sng"/></a:lvl2pPr></p:bodyStyle><p:otherStyle><a:lvl1pPr><a:defRPr strike="sngStrike"/></a:lvl1pPr></p:otherStyle></p:txStyles></p:sldMaster>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    preflight_xml(
        master_xml.as_bytes(),
        "ppt/slideMasters/slideMaster1.xml",
        XmlProfile::Master,
        &options,
        &context,
    )
    .unwrap();
    let parsed = parse_master_text_styles(
        master_xml.as_bytes(),
        "ppt/slideMasters/slideMaster1.xml",
        &options,
        &context,
    )
    .unwrap();
    let mut master = Vec::new();
    merge_master_text_styles(&mut master, parsed, "ppt/slideMasters/slideMaster1.xml").unwrap();
    let layout = vec![(
        PlaceholderKey { index: 0, class: PlaceholderClass::Body },
        ShapeStyle {
            paragraphs: vec![TextParagraph {
                default_style: RichStyle { italic: Some(false), ..RichStyle::default() },
                ..TextParagraph::default()
            }],
            class: PlaceholderClass::Body,
            ..ShapeStyle::default()
        },
    )];
    let mut shapes = vec![Shape {
        placeholder: Some("body".into()),
        paragraphs: vec![
            TextParagraph {
                text: vec![Inline::Text { value: "off".into(), marks: Vec::new() }],
                run_styles: vec![RichStyle { bold: Some(false), ..RichStyle::default() }],
                ..TextParagraph::default()
            },
            TextParagraph {
                text: vec![Inline::Text { value: "level two".into(), marks: Vec::new() }],
                run_styles: vec![RichStyle::default()],
                level: 1,
                level_explicit: true,
                ..TextParagraph::default()
            },
        ],
        ..Shape::default()
    }];
    apply_inheritance(&mut shapes, &layout, &master).unwrap();
    assert!(matches!(
        &shapes[0].paragraphs[0].text[0],
        Inline::Text { marks, .. } if marks.is_empty()
    ));
    assert_eq!(shapes[0].paragraphs[1].bullet, Some(ListKind::Ordered));
    assert_eq!(shapes[0].paragraphs[1].start, 4);
    assert!(matches!(
        &shapes[0].paragraphs[1].text[0],
        Inline::Text { marks, .. } if marks == &[InlineMark::Underline]
    ));
}

#[test]
fn master_default_paragraph_properties_layer_below_level_properties() {
    let master_xml = format!(
        r#"<p:sldMaster xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree/></p:cSld><p:txStyles><p:titleStyle><a:defPPr><a:defRPr b="true"/></a:defPPr><a:lvl1pPr><a:defRPr i="true"/></a:lvl1pPr></p:titleStyle><p:bodyStyle/><p:otherStyle/></p:txStyles></p:sldMaster>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    preflight_xml(
        master_xml.as_bytes(),
        "ppt/slideMasters/slideMaster1.xml",
        XmlProfile::Master,
        &options,
        &context,
    )
    .unwrap();
    let parsed = parse_master_text_styles(
        master_xml.as_bytes(),
        "ppt/slideMasters/slideMaster1.xml",
        &options,
        &context,
    )
    .unwrap();
    let title = parsed.iter().find(|(section, _)| *section == MasterTextSection::Title).unwrap();
    assert_eq!(title.1.len(), 1);
    assert_eq!(title.1[0].default_style.bold, Some(true));
    assert_eq!(title.1[0].default_style.italic, Some(true));

    let invalid_order = master_xml.replace(
        r#"<a:defPPr><a:defRPr b="true"/></a:defPPr><a:lvl1pPr><a:defRPr i="true"/></a:lvl1pPr>"#,
        r#"<a:lvl1pPr><a:defRPr i="true"/></a:lvl1pPr><a:defPPr><a:defRPr b="true"/></a:defPPr>"#,
    );
    assert!(matches!(
        parse_master_text_styles(
            invalid_order.as_bytes(),
            "ppt/slideMasters/slideMaster1.xml",
            &options,
            &context,
        ),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn shape_extension_payload_does_not_overwrite_geometry_extent() {
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:future="urn:future"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="1" name="Text"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="10" y="20"/><a:ext cx="30" cy="40"/></a:xfrm><a:extLst><a:ext uri="reviewed"><future:value/></a:ext></a:extLst></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>text</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let shapes = parse_shapes(
        slide.as_bytes(),
        "ppt/slides/slide1.xml",
        XmlProfile::Slide,
        &options,
        &context,
    )
    .unwrap();
    assert_eq!(shapes.len(), 1);
    assert_eq!((shapes[0].geometry.x, shapes[0].geometry.y), (10, 20));
    assert_eq!((shapes[0].geometry.cx, shapes[0].geometry.cy), (30, 40));
}

#[test]
fn shape_list_styles_do_not_require_master_text_styles() {
    let master_xml = format!(
        r#"<p:sldMaster xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle><a:lvl1pPr><a:buNone/></a:lvl1pPr></a:lstStyle><a:p/></p:txBody></p:sp></p:spTree></p:cSld></p:sldMaster>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let parsed = parse_master_text_styles(
        master_xml.as_bytes(),
        "ppt/slideMasters/slideMaster1.xml",
        &options,
        &context,
    )
    .unwrap();
    assert!(parsed.is_empty());
}

#[test]
fn master_text_style_cardinality_follows_selected_mce_branch() {
    let part = "ppt/slideMasters/slideMaster1.xml";
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let master = |body: &str, extra_namespaces: &str| {
        format!(
            r#"<p:sldMaster xmlns:p="{p}" xmlns:a="{a}" {extra_namespaces}><p:cSld><p:spTree/></p:cSld>{body}</p:sldMaster>"#,
            p = String::from_utf8_lossy(P_NS),
            a = String::from_utf8_lossy(A_NS),
        )
    };
    let malformed_preflight = |xml: &str| {
        assert!(matches!(
            preflight_xml(xml.as_bytes(), part, XmlProfile::Master, &options, &context,),
            Err(ConversionError::Malformed { .. })
        ));
    };

    let duplicate_section = master("<p:txStyles><p:titleStyle/><p:titleStyle/></p:txStyles>", "");
    malformed_preflight(&duplicate_section);
    assert!(matches!(
        parse_master_text_styles(duplicate_section.as_bytes(), part, &options, &context),
        Err(ConversionError::Malformed { .. })
    ));

    let duplicate_tx_styles = master(
        "<p:txStyles><p:titleStyle/></p:txStyles><p:txStyles><p:bodyStyle/></p:txStyles>",
        "",
    );
    malformed_preflight(&duplicate_tx_styles);
    assert!(matches!(
        parse_master_text_styles(duplicate_tx_styles.as_bytes(), part, &options, &context),
        Err(ConversionError::Malformed { .. })
    ));

    let namespaces =
        format!(r#"xmlns:mc="{}" xmlns:future="urn:future""#, String::from_utf8_lossy(MC_NS));
    let selected_duplicate = master(
        r#"<mc:AlternateContent><mc:Choice Requires="p"><p:txStyles><p:titleStyle/></p:txStyles><p:txStyles><p:bodyStyle/></p:txStyles></mc:Choice><mc:Fallback><p:txStyles><p:otherStyle/></p:txStyles></mc:Fallback></mc:AlternateContent>"#,
        &namespaces,
    );
    malformed_preflight(&selected_duplicate);

    let unselected_duplicate = master(
        r#"<mc:AlternateContent><mc:Choice Requires="future"><p:txStyles><p:titleStyle/><p:titleStyle/></p:txStyles><p:txStyles/></mc:Choice><mc:Fallback><p:txStyles><p:bodyStyle/></p:txStyles></mc:Fallback></mc:AlternateContent>"#,
        &namespaces,
    );
    preflight_xml(unselected_duplicate.as_bytes(), part, XmlProfile::Master, &options, &context)
        .unwrap();
    let selected =
        parse_master_text_styles(unselected_duplicate.as_bytes(), part, &options, &context)
            .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].0, MasterTextSection::Body);

    // MCE selection never suppresses the safety preflight of an unsupported branch.
    let unsafe_unselected =
        unselected_duplicate.replace("<p:titleStyle/>", "<p:titleStyle marker=\"&custom;\"/>");
    malformed_preflight(&unsafe_unselected);

    // Keep the merge boundary defensive even for a caller that bypasses XML parsing.
    assert!(matches!(
        merge_master_text_styles(
            &mut Vec::new(),
            vec![(MasterTextSection::Title, Vec::new()), (MasterTextSection::Title, Vec::new()),],
            part,
        ),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn asset_digest_index_bounds_collision_work_and_honors_cancellation() {
    const IMAGE_COUNT: u32 = 128;
    let context =
        ExecutionContext::new(ExecutionOptions::default(), ConversionOptions::default().limits);
    let mut assets = Vec::new();
    let mut candidates = Vec::new();
    for value in 0..MAX_ASSET_DIGEST_CANDIDATES {
        assets.push(Asset {
            id: AssetId(format!("candidate-{value}")),
            filename: None,
            media_type: "image/png".into(),
            bytes: vec![u8::try_from(value).unwrap(); 16],
            external_uri: None,
        });
        candidates.push(value);
    }
    assert_eq!(find_duplicate_asset(&assets, &candidates, &[255; 16], &context).unwrap(), None);
    assert_eq!(
        find_duplicate_asset(&assets, &candidates, &[63; 16], &context).unwrap().as_deref(),
        Some("candidate-63")
    );
    candidates.push(0);
    assert!(matches!(
        find_duplicate_asset(&assets, &candidates, &[255; 16], &context),
        Err(ConversionError::ResourceLimit { limit: "asset_digest_collisions", .. })
    ));

    let mut unique = HashMap::<[u8; 32], usize>::new();
    unique.try_reserve(4096).unwrap();
    for value in 0_u32..4096 {
        let digest = asset_digest(&value.to_le_bytes(), &context).unwrap();
        assert!(unique.insert(digest, 1).is_none());
    }
    assert_eq!(unique.len(), 4096);

    let mut extra = Vec::<(String, Vec<u8>)>::new();
    let mut relationships =
        format!(r#"<Relationships xmlns="{}">"#, String::from_utf8_lossy(REL_NS));
    let mut pictures = String::new();
    for value in 0..IMAGE_COUNT {
        let relationship = format!(
            r#"<Relationship Id="image{value}" Type="{IMAGE_REL}" Target="../media/image{value}.png"/>"#
        );
        relationships.push_str(&relationship);
        let picture = format!(
            r#"<p:pic><p:nvPicPr><p:cNvPr id="{}" name="Image {value}"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="image{value}"/></p:blipFill><p:spPr/></p:pic>"#,
            value + 2
        );
        pictures.push_str(&picture);
        extra.push((format!("ppt/media/image{value}.png"), unique_png(value)));
    }
    relationships.push_str("</Relationships>");
    extra.push(("ppt/slides/_rels/slide1.xml.rels".into(), relationships.into_bytes()));
    let extra_refs =
        extra.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    let image_package = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &extra_refs,
    );
    let types = format!(
        r#"<Types xmlns="{types}"><Default Extension="rels" ContentType="{rels}"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>"#,
        types = String::from_utf8_lossy(TYPES_NS),
        rels = RELATIONSHIPS_CONTENT_TYPE,
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:r="{r}"><p:cSld><p:spTree>{pictures}</p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        r = String::from_utf8_lossy(R_NS),
    );
    let image_package = rewrite_part(
        &rewrite_part(&image_package, "[Content_Types].xml", types.as_bytes()),
        "ppt/slides/slide1.xml",
        slide.as_bytes(),
    );
    let output = convert(&image_package).unwrap();
    assert_eq!(output.assets.len(), usize::try_from(IMAGE_COUNT).unwrap());

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = ExecutionContext::new(
        ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        ConversionOptions::default().limits,
    );
    assert!(matches!(
        asset_digest(&vec![0; ASSET_HASH_CHUNK_BYTES * 2], &cancelled),
        Err(ConversionError::Cancelled)
    ));
    assert!(matches!(
        find_duplicate_asset(&assets, &[0], &[0; 16], &cancelled),
        Err(ConversionError::Cancelled)
    ));
}
