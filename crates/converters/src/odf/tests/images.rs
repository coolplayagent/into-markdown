#![allow(clippy::needless_raw_string_hashes)]

use super::support::{NS, convert, package};
use image::ImageFormat;
use into_markdown_core::{ConversionError, InputFormat, ResourceLimits};
use std::io::Cursor;

#[test]
fn package_images_are_fully_decoded_deduplicated_and_external_images_rejected() {
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 2).write_to(&mut png, ImageFormat::Png).unwrap();
    let content = format!(
        r#"<office:document-content {NS}><office:body><office:text><draw:frame><draw:image xlink:type='simple' xlink:href='Pictures/a.png'/></draw:frame><draw:frame><draw:image xlink:type='simple' xlink:href='./Pictures/a.png'/></draw:frame></office:text></office:body></office:document-content>"#
    );
    let bytes =
        package(InputFormat::Odt, &content, &[("Pictures/a.png", "image/png", png.get_ref())]);
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    output.document.validate().unwrap();
    assert_eq!(output.assets.len(), 1);
    assert_eq!(output.document.blocks.len(), 2);
    assert_ne!(
        output.document.blocks[0].provenance.locator,
        output.document.blocks[1].provenance.locator
    );

    let external = format!(
        r#"<office:document-content {NS}><office:body><office:text><draw:frame><draw:image xlink:type='simple' xlink:href='https://example.invalid/a.png'/></draw:frame></office:text></office:body></office:document-content>"#
    );
    let external = package(InputFormat::Odt, &external, &[]);
    assert!(matches!(
        convert(&external, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let bad_bytes =
        package(InputFormat::Odt, &content, &[("Pictures/a.png", "image/png", b"not a png")]);
    assert!(matches!(
        convert(&bad_bytes, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "Pictures/a.png"
    ));

    let mismatched =
        package(InputFormat::Odt, &content, &[("Pictures/a.jpg", "image/png", png.get_ref())]);
    assert!(matches!(
        convert(&mismatched, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
}
