use super::super::convert_presentation;
use super::super::geometry::convex_quadrilaterals_overlap;
use super::super::model::{DisplayPoint, Geometry, GroupTransform};
use super::super::schema::{A_NS, P_NS, REL_NS, REL_PREFIX, SEEN_CHILD_EXTENT, SEEN_EXTENT};
use super::super::text::plain_text;
use super::support::{convert, convert_strict, fixture, rewrite_part};
use into_markdown_core::{
    Block, ConversionError, ConversionOptions, ExecutionContext, ExecutionOptions, ListKind,
};

#[test]
#[allow(clippy::too_many_lines)]
fn reads_geometry_order_rotation_lists_tables_and_groups() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree>
            <p:sp><p:nvSpPr><p:cNvPr id="2" name="Late"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm rot="5400000"><a:off x="914400" y="1828800"/><a:ext cx="914400" cy="1828800"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:pPr lvl="2"><a:buChar char="•"/></a:pPr><a:r><a:t>Late</a:t></a:r></a:p><a:p><a:pPr lvl="2"><a:buChar char="•"/></a:pPr><a:r><a:t>Second</a:t></a:r></a:p><a:p><a:pPr lvl="3"><a:buAutoNum type="arabicPeriod" startAt="4"/></a:pPr><a:r><a:t>Nested</a:t></a:r></a:p></p:txBody></p:sp>
            <p:grpSp><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="1828800" cy="1828800"/><a:chOff x="0" y="0"/><a:chExt cx="914400" cy="914400"/></a:xfrm></p:grpSpPr><p:sp><p:nvSpPr><p:cNvPr id="3" name="Early"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="457200"/><a:ext cx="457200" cy="457200"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>Early</a:t></a:r></a:p></p:txBody></p:sp></p:grpSp>
            <p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="4" name="Table"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="0" y="2743200"/><a:ext cx="914400" cy="914400"/></p:xfrm><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>H&#9;inside</a:t></a:r></a:p></a:txBody></a:tc></a:tr><a:tr><a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>V</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>
            </p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let bytes = rewrite_part(&original, "ppt/slides/slide1.xml", slide.as_bytes());
    let output = convert(&bytes).unwrap();
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    assert!(
        matches!(&blocks[0].block, Block::Paragraph(values) if matches!(plain_text(values).as_deref(), Ok("Early")))
    );
    assert!(blocks.iter().any(|node| matches!(&node.block, Block::List { items, .. }
            if items.len() == 2
                && items[0].marker_label.as_deref() == Some("level:2;character:•")
                && matches!(&items[1].blocks[1].block, Block::List { kind: ListKind::Ordered, start: 4, items } if items[0].marker_label.as_deref() == Some("level:3;scheme:arabicPeriod")))));
    assert!(
        blocks
            .iter()
            .any(|node| matches!(&node.block, Block::Table { rows, .. } if rows.len() == 2))
    );
    let table = blocks.iter().find_map(|node| match &node.block {
        Block::Table { rows, .. } => Some(rows),
        _ => None,
    });
    assert!(
        matches!(table, Some(rows) if rows[0].cells.len() == 1 && matches!(&rows[0].cells[0].blocks[0].block, Block::Paragraph(value) if matches!(plain_text(value).as_deref(), Ok("H\tinside"))))
    );
    let list_bounds = blocks
        .iter()
        .find(|node| matches!(node.block, Block::List { .. }))
        .unwrap()
        .provenance
        .locator
        .bounds
        .unwrap();
    assert!((list_bounds.width - 2.0).abs() < f32::EPSILON);
    assert!((list_bounds.height - 1.0).abs() < f32::EPSILON);

    let merged = slide.replacen("<a:tc>", "<a:tc gridSpan=\"2\">", 1);
    let recovered =
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", merged.as_bytes())).unwrap();
    assert!(
        recovered.diagnostics.iter().any(|diagnostic| diagnostic.code == "office.tableNormalized")
    );
    assert!(matches!(
        convert_strict(&rewrite_part(&original, "ppt/slides/slide1.xml", merged.as_bytes())),
        Err(ConversionError::Malformed { .. })
    ));
    for malformed_table in [
        slide.replace("<a:tr><a:tc>", "<a:tr></a:tr><a:tr><a:tc>"),
        slide.replacen("<a:txBody>", "<a:txBody></a:txBody><a:txBody>", 1),
        slide.replacen("<a:tbl>", "<a:tbl></a:tbl><a:tbl>", 1),
    ] {
        assert!(matches!(
            convert_strict(&rewrite_part(
                &original,
                "ppt/slides/slide1.xml",
                malformed_table.as_bytes()
            )),
            Err(ConversionError::Malformed { .. })
        ));
    }
    for unsupported_shape in [
        r#"<p:pic><p:nvPicPr><p:cNvPr id="9" name="Empty"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill/><p:spPr/></p:pic>"#,
        r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="9" name="Unknown"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><a:graphic><a:graphicData/></a:graphic></p:graphicFrame>"#,
    ] {
        let malformed = slide.replace("</p:spTree>", &format!("{unsupported_shape}</p:spTree>"));
        let recovered =
            convert(&rewrite_part(&original, "ppt/slides/slide1.xml", malformed.as_bytes()))
                .unwrap();
        assert!(
            recovered
                .diagnostics
                .iter()
                .any(|diagnostic| { diagnostic.code == "presentation.graphicPlaceholder" })
        );
        assert!(matches!(
            convert_strict(&rewrite_part(&original, "ppt/slides/slide1.xml", malformed.as_bytes())),
            Err(ConversionError::Malformed { .. })
        ));
    }
    for (limit_name, limits) in [
        (
            "max_table_rows",
            into_markdown_core::ResourceLimits {
                max_table_rows: 1,
                ..into_markdown_core::ResourceLimits::default()
            },
        ),
        (
            "max_table_columns",
            into_markdown_core::ResourceLimits {
                max_table_columns: 0,
                ..into_markdown_core::ResourceLimits::default()
            },
        ),
        (
            "max_table_cells",
            into_markdown_core::ResourceLimits {
                max_table_cells: 0,
                ..into_markdown_core::ResourceLimits::default()
            },
        ),
    ] {
        let options = ConversionOptions { limits, ..ConversionOptions::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(matches!(
            convert_presentation(&bytes, &options, &context),
            Err(ConversionError::ResourceLimit { limit, .. }) if limit == limit_name
        ));
    }
    for malformed_list in [
        slide.replace("startAt=\"4\"", "startAt=\"32768\""),
        slide.replace("type=\"arabicPeriod\"", "type=\"invalid\""),
        slide.replace(" type=\"arabicPeriod\"", ""),
        slide.replace("char=\"•\"", "char=\"\""),
        slide.replace("x=\"914400\"", "x=\"9007199254740992\""),
    ] {
        assert!(matches!(
            convert(&rewrite_part(&original, "ppt/slides/slide1.xml", malformed_list.as_bytes())),
            Err(ConversionError::Malformed { .. })
        ));
    }
}

#[test]
fn overlapping_shapes_keep_authoritative_z_order_and_metadata() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let shape = |id: u8, name: &str, x: i64, y: i64, width: i64, height: i64| {
        format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{name}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{width}" cy="{height}"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{name}</a:t></a:r></a:p></p:txBody></p:sp>"#
        )
    };
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree>{c}{b}{a_shape}</p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS),
        // A and C overlap but B sorts between them by y. Their connected component must
        // still recover C(z1) before A(z3); B is a separate geometry component.
        c = shape(2, "C", 0, 182_880, 91_440, 91_440),
        b = shape(3, "B", 1_828_800, 91_440, 91_440, 91_440),
        a_shape = shape(4, "A", 0, 0, 914_400, 914_400),
    );
    let output =
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", slide.as_bytes())).unwrap();
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let values = blocks
        .iter()
        .map(|node| match &node.block {
            Block::Paragraph(inlines) => plain_text(inlines).unwrap(),
            _ => String::new(),
        })
        .collect::<Vec<_>>();
    assert_eq!(values, ["C", "A", "B"]);
    for node in blocks {
        let key = format!("presentation.zOrder.{}", node.id.0);
        let expected = node.id.0.split("-z-").nth(1).unwrap().split('-').next().unwrap();
        assert_eq!(
            output.document.metadata.properties.get(&key).map(String::as_str),
            Some(expected)
        );
    }
}

#[test]
fn sat_overlap_rejects_false_aabb_and_edge_contact_but_accepts_crossing_and_nesting() {
    let diamond = [
        DisplayPoint { x: 0.0, y: -1.0 },
        DisplayPoint { x: 1.0, y: 0.0 },
        DisplayPoint { x: 0.0, y: 1.0 },
        DisplayPoint { x: -1.0, y: 0.0 },
    ];
    let translated =
        |x: f64, y: f64| diamond.map(|point| DisplayPoint { x: point.x + x, y: point.y + y });
    // AABBs overlap in both axes, but a separating diagonal exists.
    assert!(!convex_quadrilaterals_overlap(&diamond, &translated(1.5, 1.5)).unwrap());
    assert!(convex_quadrilaterals_overlap(&diamond, &translated(0.5, 0.5)).unwrap());
    // A single shared edge/point has no visible overlap area.
    assert!(!convex_quadrilaterals_overlap(&diamond, &translated(2.0, 0.0)).unwrap());
    let nested = [
        DisplayPoint { x: -0.1, y: -0.1 },
        DisplayPoint { x: 0.1, y: -0.1 },
        DisplayPoint { x: 0.1, y: 0.1 },
        DisplayPoint { x: -0.1, y: 0.1 },
    ];
    assert!(convex_quadrilaterals_overlap(&diamond, &nested).unwrap());

    let rectangle = [
        DisplayPoint { x: -2.0, y: -2.0 },
        DisplayPoint { x: 2.0, y: -2.0 },
        DisplayPoint { x: 2.0, y: 2.0 },
        DisplayPoint { x: -2.0, y: 2.0 },
    ];
    let line_inside = [
        DisplayPoint { x: -1.0, y: 0.0 },
        DisplayPoint { x: 1.0, y: 0.0 },
        DisplayPoint { x: 1.0, y: 0.0 },
        DisplayPoint { x: -1.0, y: 0.0 },
    ];
    let point_inside = [DisplayPoint { x: 0.0, y: 0.0 }; 4];
    assert!(!convex_quadrilaterals_overlap(&line_inside, &rectangle).unwrap());
    assert!(!convex_quadrilaterals_overlap(&point_inside, &rectangle).unwrap());
    assert!(!convex_quadrilaterals_overlap(&line_inside, &point_inside).unwrap());

    let shape = Geometry { cx: 914_400, cy: 914_400, ..Geometry::default() };
    let extent_collapsed = GroupTransform {
        extent_x: 0,
        extent_y: 914_400,
        child_extent_x: 914_400,
        child_extent_y: 914_400,
        semantic_seen: SEEN_EXTENT | SEEN_CHILD_EXTENT,
        ..GroupTransform::default()
    }
    .apply(shape)
    .unwrap()
    .display_corners()
    .unwrap();
    let paired_zero_extent = GroupTransform {
        extent_x: 0,
        extent_y: 914_400,
        child_extent_x: 0,
        child_extent_y: 914_400,
        semantic_seen: SEEN_EXTENT | SEEN_CHILD_EXTENT,
        ..GroupTransform::default()
    }
    .apply(shape)
    .unwrap()
    .display_corners()
    .unwrap();
    assert!(!convex_quadrilaterals_overlap(&extent_collapsed, &rectangle).unwrap());
    assert!(!convex_quadrilaterals_overlap(&paired_zero_extent, &rectangle).unwrap());
}

#[test]
fn arbitrary_bounds_and_group_rotation_are_center_based() {
    let cases = [
        (0, (1.0, 2.0, 1.0, 2.0)),
        (5_400_000, (0.5, 2.5, 2.0, 1.0)),
        (10_800_000, (1.0, 2.0, 1.0, 2.0)),
        (16_200_000, (0.5, 2.5, 2.0, 1.0)),
    ];
    for (rotation, expected) in cases {
        let bounds = Geometry {
            x: 914_400,
            y: 1_828_800,
            cx: 914_400,
            cy: 1_828_800,
            rotation,
            ..Geometry::default()
        }
        .bounds()
        .unwrap();
        assert!((bounds.x - expected.0).abs() < f32::EPSILON);
        assert!((bounds.y - expected.1).abs() < f32::EPSILON);
        assert!((bounds.width - expected.2).abs() < f32::EPSILON);
        assert!((bounds.height - expected.3).abs() < f32::EPSILON);
    }
    for rotation in [2_700_000, 18_900_000] {
        let bounds = Geometry {
            x: 914_400,
            y: 1_828_800,
            cx: 914_400,
            cy: 1_828_800,
            rotation,
            ..Geometry::default()
        }
        .bounds()
        .unwrap();
        assert!((bounds.x - 0.439_339_82).abs() < 0.000_01);
        assert!((bounds.y - 1.939_339_9).abs() < 0.000_01);
        assert!((bounds.width - 2.121_320_2).abs() < 0.000_01);
        assert!((bounds.height - 2.121_320_2).abs() < 0.000_01);
    }
    let group_cases = [
        (0, (0.0, 0.0, 1.0, 2.0)),
        (5_400_000, (0.0, 0.0, 2.0, 1.0)),
        (10_800_000, (1.0, 0.0, 1.0, 2.0)),
        (16_200_000, (0.0, 1.0, 2.0, 1.0)),
    ];
    for (rotation, expected) in group_cases {
        let grouped = GroupTransform {
            offset_x: 0,
            offset_y: 0,
            extent_x: 1_828_800,
            extent_y: 1_828_800,
            child_extent_x: 914_400,
            child_extent_y: 914_400,
            rotation,
            ..GroupTransform::default()
        }
        .apply(Geometry {
            x: 0,
            y: 0,
            cx: 457_200,
            cy: 914_400,
            rotation: 0,
            ..Geometry::default()
        })
        .unwrap()
        .bounds()
        .unwrap();
        assert!((grouped.x - expected.0).abs() < f32::EPSILON);
        assert!((grouped.y - expected.1).abs() < f32::EPSILON);
        assert!((grouped.width - expected.2).abs() < f32::EPSILON);
        assert!((grouped.height - expected.3).abs() < f32::EPSILON);
    }
    let anisotropic = GroupTransform {
        extent_x: 1_828_800,
        extent_y: 914_400,
        child_extent_x: 914_400,
        child_extent_y: 914_400,
        ..GroupTransform::default()
    }
    .apply(Geometry {
        x: 0,
        y: 0,
        cx: 914_400,
        cy: 1_828_800,
        rotation: 2_700_000,
        ..Geometry::default()
    })
    .unwrap()
    .bounds()
    .unwrap();
    assert!((anisotropic.x - -1.121_320_4).abs() < 0.000_01);
    assert!((anisotropic.y - -0.060_660_172).abs() < 0.000_01);
    assert!((anisotropic.width - 4.242_640_5).abs() < 0.000_01);
    assert!((anisotropic.height - 2.121_320_2).abs() < 0.000_01);
}

#[test]
fn nested_group_rotation_and_flip_fixture_has_final_display_bounds() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree>
            <p:grpSp><p:grpSpPr><a:xfrm rot="18900000" flipV="1"><a:off x="914400" y="1828800"/><a:ext cx="3657600" cy="3657600"/><a:chOff x="0" y="0"/><a:chExt cx="1828800" cy="1828800"/></a:xfrm></p:grpSpPr>
            <p:grpSp><p:grpSpPr><a:xfrm rot="2700000" flipH="1"><a:off x="0" y="0"/><a:ext cx="1828800" cy="1828800"/><a:chOff x="0" y="0"/><a:chExt cx="1828800" cy="1828800"/></a:xfrm></p:grpSpPr>
            <p:sp><p:nvSpPr><p:cNvPr id="2" name="Nested"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm rot="2700000"><a:off x="0" y="0"/><a:ext cx="457200" cy="914400"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>Nested</a:t></a:r></a:p></p:txBody></p:sp>
            </p:grpSp></p:grpSp></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS),
        a = String::from_utf8_lossy(A_NS)
    );
    let output =
        convert(&rewrite_part(&original, "ppt/slides/slide1.xml", slide.as_bytes())).unwrap();
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    let bounds = blocks[0].provenance.locator.bounds.unwrap();
    assert!((bounds.x - 2.939_338_7).abs() < 0.000_01);
    assert!((bounds.y - 1.439_339_9).abs() < 0.000_01);
    assert!((bounds.width - 2.121_320_2).abs() < 0.000_01);
    assert!((bounds.height - 2.121_320_2).abs() < 0.000_01);
}

#[test]
fn unused_large_payload_is_not_materialized() {
    let bytes = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[("unreferenced/huge.bin", vec![7_u8; 2 * 1024 * 1024])],
    );
    let mut options = ConversionOptions::default();
    options.limits.max_decompressed_bytes = 4 * 1024 * 1024;
    options.limits.max_memory_bytes = 4 * 1024 * 1024;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(convert_presentation(&bytes, &options, &context).is_ok());
}

#[test]
fn relationship_type_isolates_renamed_octet_stream_before_read() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[("ppt/media/renamed.dat", vec![0_u8; 2 * 1024 * 1024])],
    );
    let rels = format!(
        r#"<Relationships xmlns="{rels}"><Relationship Id="rId1" Type="{prefix}slide" Target="slides/slide1.xml"/><Relationship Id="evil" Type="{prefix}vbaProject" Target="media/renamed.dat"/></Relationships>"#,
        rels = String::from_utf8_lossy(REL_NS),
        prefix = REL_PREFIX
    );
    let bytes = rewrite_part(&original, "ppt/_rels/presentation.xml.rels", rels.as_bytes());
    let output = convert(&bytes).unwrap();
    assert!(
        output.diagnostics.iter().any(|item| item.code == "presentation.dangerousPartsIgnored")
    );
}
