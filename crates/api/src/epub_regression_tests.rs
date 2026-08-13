use crate::{ConversionError, ErrorCode};

use super::epub_tests::{
    PNG, chapter_one, chapter_two, container, convert, epub, epub2_one, epub2_package, epub3_book,
    epub3_package, nav3, ncx2,
};

#[test]
fn navigation_group_labels_and_ncx_optional_lists_are_supported() {
    let grouped = br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head><body><nav epub:type="toc"><ol><li><span>Part One</span><ol><li><a href="text/one.xhtml">One</a></li></ol></li></ol></nav></body></html>"#;
    let result = convert(epub3_book(epub3_package(), Some(grouped))).unwrap();
    assert!(result.markdown.contains("Part One"));
    assert!(result.markdown.contains("One"));

    let ncx = String::from_utf8(ncx2().to_vec()).unwrap().replace(
        "</ncx>",
        "<pageList><navLabel><text>Pages</text></navLabel><pageTarget id=\"page-one\" value=\"1\" type=\"normal\" playOrder=\"3\"><navLabel><text>1</text></navLabel><content src=\"one.xhtml#one\"/></pageTarget></pageList><navList><navLabel><text>Landmarks</text></navLabel><navTarget id=\"landmark\" playOrder=\"4\"><navLabel><text>Start</text></navLabel><content src=\"one.xhtml#one\"/></navTarget></navList></ncx>",
    );
    let result = convert(epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub2_package()),
        ("OPS/text/toc.ncx", ncx.as_bytes()),
        ("OPS/text/dummy.svg", b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
        ("OPS/text/one.xhtml", epub2_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        ("OPS/images/cover.png", PNG),
    ]))
    .unwrap();
    assert!(result.markdown.contains("First"));
    assert!(!result.markdown.contains("Landmarks"));
}

#[test]
fn xml_base_dot_segments_and_unicode_ncnames_resolve_portably() {
    let package = String::from_utf8(epub3_package().to_vec())
        .unwrap()
        .replace("<manifest>", "<manifest xml:base=\".\">")
        .replace("unique-identifier=\"book-id\"", "unique-identifier=\"图书\"")
        .replace("id=\"book-id\"", "id=\"图书\"");
    let result = convert(epub3_book(package.as_bytes(), Some(nav3()))).unwrap();
    assert_eq!(result.document.metadata.title.as_deref(), Some("Original EPUB Three"));

    let missing = package.replace("href=\"text/two.xhtml\"", "href=\"text/missing.xhtml\"");
    assert_eq!(
        convert(epub3_book(missing.as_bytes(), Some(nav3()))).unwrap_err().code(),
        ErrorCode::Malformed
    );
}

#[test]
fn xml_characters_and_xhtml_document_boundaries_are_strict() {
    let package = String::from_utf8(epub3_package().to_vec()).unwrap();
    for invalid in ["&#0;", "&#1;", "&#xB;", "&#xD800;", "&#xFFFE;"] {
        let value = package.replace("Original EPUB Three", &format!("Bad{invalid}Title"));
        assert_eq!(
            convert(epub3_book(value.as_bytes(), Some(nav3()))).unwrap_err().code(),
            ErrorCode::Malformed,
            "accepted {invalid}"
        );
    }
    let valid = package.replace("Original EPUB Three", "Good&#9;&#10;&#13;&#x10000;Title");
    assert!(convert(epub3_book(valid.as_bytes(), Some(nav3()))).is_ok());

    for chapter in [
        b"LEADING<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>"
            .as_slice(),
        b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>TRAILING"
            .as_slice(),
    ] {
        let bytes = epub(&[
            ("META-INF/container.xml", container()),
            ("OPS/content.opf", epub3_package()),
            ("OPS/nav.xhtml", nav3()),
            ("OPS/text/one.xhtml", chapter),
            ("OPS/text/two.xhtml", chapter_two()),
            ("OPS/text/extra.xhtml", chapter_two()),
            ("OPS/images/cover.png", PNG),
            ("OPS/styles/book.css", b"body{}"),
        ]);
        assert_eq!(convert(bytes).unwrap_err().code(), ErrorCode::Malformed);
    }
}

#[test]
fn internal_reference_tokens_cannot_be_forged_by_external_links() {
    let reserved = String::from_utf8(chapter_one().to_vec()).unwrap().replace(
        "href=\"two.xhtml#target\"",
        "href=\"https://epub.invalid/OPS/text/two.xhtml#target\"",
    );
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", reserved.as_bytes()),
        ("OPS/text/two.xhtml", chapter_two()),
        ("OPS/text/extra.xhtml", chapter_two()),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    assert_eq!(convert(bytes).unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn percent_encoded_unicode_fragments_match_normalized_anchor_identity() {
    let chapter = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p><a href="two.xhtml#caf%C3%A9">Encoded</a><a href="two.xhtml#cafe%CC%81">Decomposed</a></p></body></html>"#;
    let target = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><h2 id="target">Navigation</h2><p id="café">Target</p></body></html>"#;
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter),
        ("OPS/text/two.xhtml", target.as_bytes()),
        ("OPS/text/extra.xhtml", chapter_two()),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    let result = convert(bytes).unwrap();
    let mut links = Vec::new();
    let mut footnotes = Vec::new();
    super::epub_tests::collect_links(&result.document.blocks, &mut links, &mut footnotes);
    assert_eq!(links.iter().filter(|target| *target == "OPS/text/two.xhtml#café").count(), 2);
}

#[test]
fn retained_rasters_require_complete_payloads_and_bounded_pixels() {
    let truncated = &PNG[..PNG.len() - 4];
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        ("OPS/text/extra.xhtml", chapter_two()),
        ("OPS/images/cover.png", truncated),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    assert_eq!(convert(bytes).unwrap_err().code(), ErrorCode::Malformed);

    let mut oversized = PNG.to_vec();
    oversized[16..20].copy_from_slice(&100_000_u32.to_be_bytes());
    oversized[20..24].copy_from_slice(&100_000_u32.to_be_bytes());
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        ("OPS/text/extra.xhtml", chapter_two()),
        ("OPS/images/cover.png", &oversized),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    assert!(matches!(convert(bytes), Err(ConversionError::ResourceLimit { .. })));
}

#[test]
fn epub3_toc_requires_direct_children_and_one_label_per_item() {
    let valid = br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head><body><nav epub:type="toc"><ol><li><span>Part</span><ol><li><a href="text/one.xhtml">One</a></li></ol></li></ol></nav></body></html>"#;
    assert!(convert(epub3_book(epub3_package(), Some(valid))).is_ok());

    for invalid in [
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><div><ol><li><a href="text/one.xhtml">One</a></li></ol></div></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><li><a href="text/one.xhtml">One</a></li></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml">One</a><span>Duplicate</span></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml">One</a></li></ol><ol><li><a href="text/two.xhtml">Two</a></li></ol></nav></body></html>"#.as_slice(),
    ] {
        assert_eq!(
            convert(epub3_book(epub3_package(), Some(invalid))).unwrap_err().code(),
            ErrorCode::Malformed
        );
    }
}

#[test]
fn every_epub_xml_document_enforces_prolog_and_character_rules() {
    for container_xml in [
        b"<!--\x01--><container xmlns='urn:oasis:names:tc:opendocument:xmlns:container' version='1.0'><rootfiles><rootfile full-path='OPS/content.opf' media-type='application/oebps-package+xml'/></rootfiles></container>".as_slice(),
        b"<container xmlns='urn:oasis:names:tc:opendocument:xmlns:container' version='1.0'><?xml version='1.0'?><rootfiles><rootfile full-path='OPS/content.opf' media-type='application/oebps-package+xml'/></rootfiles></container>".as_slice(),
        b"<container xmlns='urn:oasis:names:tc:opendocument:xmlns:container' version='1.0'><!DOCTYPE html><rootfiles><rootfile full-path='OPS/content.opf' media-type='application/oebps-package+xml'/></rootfiles></container>".as_slice(),
    ] {
        let bytes = epub(&[
            ("META-INF/container.xml", container_xml),
            ("OPS/content.opf", epub3_package()),
            ("OPS/nav.xhtml", nav3()),
            ("OPS/text/one.xhtml", chapter_one()),
            ("OPS/text/two.xhtml", chapter_two()),
            ("OPS/text/extra.xhtml", chapter_two()),
            ("OPS/images/cover.png", PNG),
            ("OPS/styles/book.css", b"body{}"),
        ]);
        assert_eq!(convert(bytes).unwrap_err().code(), ErrorCode::Malformed);
    }
}
