use super::support::{NS, convert, package};
use crate::odf::compatibility::{GRDDL_NS, LOEXT_NS};
use into_markdown_core::{Block, ConversionError, ConversionOptions, InputFormat, ResourceLimits};
use into_markdown_render_markdown::render;
use std::io::Cursor;

#[test]
fn producer_hints_fonts_and_empty_scripts_preserve_body_and_marks() {
    let content = format!(
        "<office:document-content {NS} xmlns:g='{GRDDL_NS}' xmlns:lo='{LOEXT_NS}' g:transformation='https://example.invalid/never-fetch' office:version='1.0'><office:scripts/><office:font-face-decls><style:font-face style:name='A' svg:font-family='Arial'/></office:font-face-decls><office:automatic-styles><style:style style:name='P' style:family='paragraph' style:master-page-name='Standard'><style:text-properties fo:font-weight='bold' lo:opacity='100%'/></style:style></office:automatic-styles><office:body><office:text><text:p text:style-name='P'>before<text:soft-page-break/>after</text:p></office:text></office:body></office:document-content>"
    );
    let meta = format!(
        "<office:document-meta {NS} office:version='1.1'><office:meta><dc:title>Title</dc:title><meta:document-statistic xmlns:meta='urn:oasis:names:tc:opendocument:xmlns:meta:1.0' meta:word-count='2'/></office:meta></office:document-meta>"
    );
    let bytes = package(InputFormat::Odt, &content, &[("meta.xml", "text/xml", meta.as_bytes())]);
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    output.document.validate().unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(
        markdown.contains("before") && markdown.contains("after") && markdown.contains("<strong>")
    );
    assert!(output.diagnostics.iter().any(|d| d.code == "odf.layoutMetadata"));
}

#[test]
fn compatibility_does_not_hide_active_or_unknown_content_in_layout_definitions() {
    for layout in [
        "<office:automatic-styles><style:style style:name='s' style:family='text'><style:text-properties><office:body/></style:text-properties></style:style></office:automatic-styles>",
        "<office:automatic-styles><style:style style:name='s' style:family='text'><style:text-properties><office:event-listeners/></style:text-properties></style:style></office:automatic-styles>",
    ] {
        let content = format!(
            "<office:document-content {NS}>{layout}<office:body><office:text><text:p>body</text:p></office:text></office:body></office:document-content>"
        );
        assert!(matches!(
            convert(
                &package(InputFormat::Odt, &content, &[]),
                InputFormat::Odt,
                ResourceLimits::default()
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }
    let content = format!(
        "<office:document-content {NS} xmlns:lo='{LOEXT_NS}'><office:body><office:text><text:p lo:unknown='x'>body</text:p></office:text></office:body></office:document-content>"
    );
    assert!(
        convert(
            &package(InputFormat::Odt, &content, &[]),
            InputFormat::Odt,
            ResourceLimits::default()
        )
        .is_err()
    );
}

#[test]
fn duplicate_styles_must_be_identical_not_merely_have_equal_text_marks() {
    let style = "<style:style style:name='same' style:family='graphic'><style:graphic-properties draw:fill='none'/></style:style>";
    for identical in [true, false] {
        let other = if identical { style.to_owned() } else { style.replace("'none'", "'solid'") };
        let content = format!(
            "<office:document-content {NS}><office:automatic-styles>{style}{other}</office:automatic-styles><office:body><office:text><text:p>body</text:p></office:text></office:body></office:document-content>"
        );
        let result = convert(
            &package(InputFormat::Odt, &content, &[]),
            InputFormat::Odt,
            ResourceLimits::default(),
        );
        if identical {
            assert!(result.unwrap().diagnostics.iter().any(|d| d.code == "odf.identicalStyle"));
        } else {
            assert!(matches!(result, Err(ConversionError::Malformed { .. })));
        }
    }
}

#[test]
fn inline_image_anchor_preserves_bytes_marks_and_text_image_text_order() {
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>before<text:span><draw:frame svg:width='0.2in' svg:height='0.2in'><svg:title>caption</svg:title><draw:image xlink:href='Pictures/a.png' xlink:type='simple' xlink:show='embed' xlink:actuate='onLoad'/></draw:frame></text:span>after</text:p></office:text></office:body></office:document-content>"
    );
    let bytes =
        package(InputFormat::Odt, &content, &[("Pictures/a.png", "image/png", png.get_ref())]);
    let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
    output.document.validate().unwrap();
    assert_eq!(output.assets.len(), 1);
    assert_eq!(output.assets[0].bytes, *png.get_ref());
    assert_eq!(output.document.blocks.len(), 3);
    assert!(matches!(&output.document.blocks[0].block, Block::Paragraph(_)));
    assert!(
        matches!(&output.document.blocks[1].block, Block::Image { alt: Some(alt), .. } if alt == "caption")
    );
    assert!(output.document.blocks[1].provenance.locator.bounds.is_none());
    assert!(matches!(&output.document.blocks[2].block, Block::Paragraph(_)));
    for bad in [
        content.replace("0.2in", "-1in"),
        content.replace("Pictures/a.png", "https://example.invalid/a.png"),
    ] {
        let bytes =
            package(InputFormat::Odt, &bad, &[("Pictures/a.png", "image/png", png.get_ref())]);
        assert!(convert(&bytes, InputFormat::Odt, ResourceLimits::default()).is_err());
    }
}

#[test]
fn note_body_drawing_stays_inside_one_footnote_with_source_order() {
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
    for class in ["footnote", "endnote"] {
        let content = format!(
            "<office:document-content {NS}><office:body><office:text><text:p>body-before<text:span><text:note text:id='n1' text:note-class='{class}'><text:note-citation>1</text:note-citation><text:note-body><text:p>note-before<text:span><draw:frame><draw:image xlink:href='Pictures/a.png'/></draw:frame></text:span>note-after</text:p></text:note-body></text:note></text:span>body-after</text:p></office:text></office:body></office:document-content>"
        );
        let bytes =
            package(InputFormat::Odt, &content, &[("Pictures/a.png", "image/png", png.get_ref())]);
        let output = convert(&bytes, InputFormat::Odt, ResourceLimits::default()).unwrap();
        output.document.validate().unwrap();
        assert_eq!(output.document.blocks.len(), 2);
        let Block::Paragraph(body) = &output.document.blocks[0].block else { panic!() };
        assert_eq!(body.iter().filter(|inline| matches!(inline, into_markdown_core::Inline::FootnoteReference(id) if id == "n1")).count(), 1);
        let Block::Footnote { label, blocks } = &output.document.blocks[1].block else { panic!() };
        assert_eq!(label, "n1");
        assert_eq!(blocks.len(), 3);
        assert!(
            matches!(&blocks[0].block, Block::Paragraph(inlines) if matches!(inlines.as_slice(), [into_markdown_core::Inline::Text { value, .. }] if value == "note-before"))
        );
        assert!(
            matches!(&blocks[1].block, Block::Image { asset, .. } if *asset == output.assets[0].id)
        );
        assert!(
            matches!(&blocks[2].block, Block::Paragraph(inlines) if matches!(inlines.as_slice(), [into_markdown_core::Inline::Text { value, .. }] if value == "note-after"))
        );
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].bytes, *png.get_ref());
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert_eq!(markdown.matches("[^fn-6e31]:").count(), 1);
        assert_eq!(markdown.matches("[^fn-6e31]").count(), 2);
        let plain = markdown.replace("\\-", "-");
        let before = plain.find("note-before").unwrap();
        let image = plain.find("![").unwrap();
        let after = plain.find("note-after").unwrap();
        assert!(before < image && image < after, "{markdown}");
    }
}

#[test]
fn empty_sheet_does_not_emit_invalid_zero_column_table() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:spreadsheet><table:table table:name='Data'><table:table-row><table:table-cell><text:p>value</text:p></table:table-cell></table:table-row></table:table><table:table table:name='Empty'><table:table-row><table:table-cell/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"
    );
    let output = convert(
        &package(InputFormat::Ods, &content, &[]),
        InputFormat::Ods,
        ResourceLimits::default(),
    )
    .unwrap();
    output.document.validate().unwrap();
    assert!(
        matches!(&output.document.blocks[1].block, Block::Sheet { blocks, .. } if blocks.is_empty())
    );
}

#[test]
fn referenced_master_footer_supplies_real_text_not_an_empty_slide_shell() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:presentation><draw:page draw:master-page-name='Default'/></office:presentation></office:body></office:document-content>"
    );
    let styles = format!(
        "<office:document-styles {NS}><office:master-styles><style:master-page style:name='Default'><draw:frame presentation:class='footer'><draw:text-box><text:p>Actual master footer</text:p></draw:text-box></draw:frame></style:master-page></office:master-styles></office:document-styles>"
    );
    let bytes =
        package(InputFormat::Odp, &content, &[("styles.xml", "text/xml", styles.as_bytes())]);
    let output = convert(&bytes, InputFormat::Odp, ResourceLimits::default()).unwrap();
    let markdown = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(markdown.contains("Actual master footer"));
    let Block::Slide { blocks, .. } = &output.document.blocks[0].block else { panic!() };
    assert_eq!(blocks[0].provenance.locator.part.as_deref(), Some("styles.xml"));
}
