use super::*;
use crate::merge::geometry::{RegionGeometry, line_compatible, polygon_overlap_ratio};
use into_markdown_core::OcrPolicy;

#[test]
fn geometry_accepts_horizontal_vertical_and_rotated_quadrilaterals() {
    let horizontal = RegionGeometry::from_polygon(polygon(0.0, 0.0, 80.0, 12.0)).unwrap();
    let vertical = RegionGeometry::from_polygon(polygon(0.0, 0.0, 12.0, 80.0)).unwrap();
    let rotated =
        RegionGeometry::from_polygon([(10.0, 10.0), (80.0, 50.0), (74.0, 61.0), (4.0, 21.0)])
            .unwrap();
    assert!(horizontal.angle_degrees < 1.0);
    assert!((vertical.angle_degrees - 90.0).abs() < 1.0);
    assert!((rotated.angle_degrees - 29.7).abs() < 1.0);
}

#[test]
fn line_compatibility_uses_projected_geometry_not_language() {
    let first =
        RegionGeometry::from_polygon([(10.0, 10.0), (50.0, 30.0), (45.0, 40.0), (5.0, 20.0)])
            .unwrap();
    let second =
        RegionGeometry::from_polygon([(52.0, 31.0), (92.0, 51.0), (87.0, 61.0), (47.0, 41.0)])
            .unwrap();
    assert!(line_compatible(&first, &second));
    assert!(polygon_overlap_ratio(&first.polygon, &first.polygon) > 0.999);
}

#[test]
fn vertical_regions_merge_in_stable_source_geometry_order() {
    let detection = detection(&[
        (polygon(20.0, 20.0, 12.0, 50.0), 0.98),
        (polygon(20.0, 72.0, 12.0, 50.0), 0.98),
    ]);
    let recognition = recognition(&[(1, "下", 0.95), (0, "上", 0.95)]);
    let output = merge_document(
        page_document(Vec::new()),
        &[input(&detection, &recognition)],
        &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
        &context(),
    )
    .unwrap();
    assert_eq!(merged_text(&output.document), "上下");
}

#[test]
fn language_hint_controls_horizontal_spacing_without_changing_geometry() {
    let detection = detection(&[
        (polygon(20.0, 20.0, 30.0, 16.0), 0.98),
        (polygon(60.0, 20.0, 30.0, 16.0), 0.98),
    ]);
    for (hint, parts, expected) in [
        (Some("zh-Hans"), ["中", "文"], "中文"),
        (Some("zh-Hans"), ["hello", "world"], "hello world"),
        (Some("en"), ["hello", "world"], "hello world"),
        (None, ["中", "word"], "中word"),
        (Some("en"), ["中", "word"], "中word"),
    ] {
        let mut recognition = recognition(&[(0, parts[0], 0.95), (1, parts[1], 0.95)]);
        recognition.language_hint = hint.map(Arc::from);
        let output = merge_document(
            page_document(Vec::new()),
            &[input(&detection, &recognition)],
            &MergeConfig { policy: OcrPolicy::Always, ..MergeConfig::default() },
            &context(),
        )
        .unwrap();
        assert_eq!(merged_text(&output.document), expected);
    }
}

#[test]
fn degenerate_and_self_crossing_polygons_are_rejected() {
    for invalid in [
        [(0.0, 0.0), (1.0, 1.0), (2.0, 2.0), (3.0, 3.0)],
        [(0.0, 0.0), (10.0, 10.0), (0.0, 10.0), (10.0, 0.0)],
    ] {
        assert!(RegionGeometry::from_polygon(invalid).is_none());
    }
}
