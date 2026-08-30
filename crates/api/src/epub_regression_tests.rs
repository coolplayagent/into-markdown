use crate::{ConversionError, ErrorCode};

use super::epub_tests::{
    PNG, chapter_one, chapter_two, container, convert, convert_strict, epub, epub2_one,
    epub2_package, epub3_book, epub3_package, nav3, ncx2,
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
fn noncritical_package_metadata_is_optional_only_in_best_effort() {
    let base = String::from_utf8(epub3_package().to_vec()).unwrap();
    for (needle, expected) in [
        ("<dc:title>Original EPUB Three</dc:title>", "dc:title"),
        ("<dc:language>en</dc:language>", "dc:language"),
        ("<meta property=\"dcterms:modified\">2026-08-13T00:00:00Z</meta>", "dcterms:modified"),
    ] {
        let package = base.replace(needle, "");
        let bytes = epub3_book(package.as_bytes(), Some(nav3()));
        let result = convert(bytes.clone()).unwrap();
        assert!(result.markdown.contains("Alpha"));
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "epub.metadataMissing" && diagnostic.message.contains(expected)
        }));
        assert_eq!(convert_strict(bytes).unwrap_err().code(), ErrorCode::Malformed);
    }
}

#[test]
fn chapter_recovery_is_scoped_after_epub_security_validation() {
    let processing_instruction = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><?ocr artifact?><p>omit me</p></body></html>"#;
    let recovered = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", processing_instruction),
        ("OPS/text/two.xhtml", chapter_two()),
        ("OPS/text/extra.xhtml", chapter_two()),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    let result = convert(recovered.clone()).unwrap();
    assert!(result.markdown.contains("Omega"));
    assert!(!result.markdown.contains("omit me"));
    assert!(
        result.diagnostics.iter().any(|diagnostic| diagnostic.code == "epub.spine.chapterOmitted")
    );
    assert_eq!(convert_strict(recovered).unwrap_err().code(), ErrorCode::Malformed);

    for malicious in [
        br#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "https://example.invalid/xhtml11.dtd"><html xmlns="http://www.w3.org/1999/xhtml"><body><p>DTD</p></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p>&custom;</p></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><a href="../../../escape">escape</a></body></html>"#.as_slice(),
    ] {
        let bytes = epub(&[
            ("META-INF/container.xml", container()),
            ("OPS/content.opf", epub3_package()),
            ("OPS/nav.xhtml", nav3()),
            ("OPS/text/one.xhtml", malicious),
            ("OPS/text/two.xhtml", chapter_two()),
            ("OPS/text/extra.xhtml", chapter_two()),
            ("OPS/images/cover.png", PNG),
            ("OPS/styles/book.css", b"body{}"),
        ]);
        assert_eq!(convert(bytes).unwrap_err().code(), ErrorCode::Malformed);
    }
}

#[test]
fn duplicate_spine_and_all_empty_or_unreadable_chapters_fail_closed() {
    let duplicate = String::from_utf8(epub3_package().to_vec())
        .unwrap()
        .replace("<itemref idref=\"two\"/>", "<itemref idref=\"one\"/><itemref idref=\"two\"/>");
    assert_eq!(
        convert(epub3_book(duplicate.as_bytes(), Some(nav3()))).unwrap_err().code(),
        ErrorCode::Malformed
    );

    let empty = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body/></html>"#;
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", empty),
        ("OPS/text/two.xhtml", empty),
        ("OPS/text/extra.xhtml", empty),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    assert_eq!(convert(bytes).unwrap_err().code(), ErrorCode::Malformed);
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
    let valid = br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head><body><nav epub:type="toc"><h2>Reader <em>contents</em></h2><ol><li><span>Part <u>One</u><img alt=" icon" title=" wrong image title"/><a href="text/two.xhtml"> related</a></span><ol><li><a href="text/one.xhtml">Read <math xmlns="http://www.w3.org/1998/Math/MathML" alt=" equation" alttext=" wrong math alttext" title=" wrong math title"><mrow><mi> wrong math body</mi></mrow></math><svg xmlns="http://www.w3.org/2000/svg" alt=" diagram" alttext=" wrong svg alttext" title=" wrong svg title"><title> wrong svg body</title></svg><math xmlns="http://www.w3.org/1998/Math/MathML"><mrow><mi> plus x</mi></mrow></math><svg xmlns="http://www.w3.org/2000/svg"><title> plus shape</title></svg><picture><source srcset="images/cover.png 1x, images/cover.png 2x"/><img alt=" picture" src="images/cover.png"/></picture><video title=" clip"><source src="images/cover.png"/></video></a></li><li><a href="text/two.xhtml" title="Canvas fallback"><canvas/></a></li></ol></li></ol></nav></body></html>"#;
    let result = convert(epub3_book(epub3_package(), Some(valid))).unwrap();
    assert!(result.markdown.contains("Part One icon related"));
    assert!(result.markdown.contains("Read equation diagram plus x plus shape picture clip"));
    assert!(result.markdown.contains("Canvas fallback"));
    assert!(!result.markdown.contains("wrong math"));
    assert!(!result.markdown.contains("wrong svg"));
    assert!(!result.markdown.contains("wrong image"));
    assert!(super::epub_tests::has_nested_list(&result.document.blocks));

    for invalid in [
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><div><ol><li><a href="text/one.xhtml">One</a></li></ol></div></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><li><a href="text/one.xhtml">One</a></li></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml">One</a><span>Duplicate</span></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml">One</a></li></ol><ol><li><a href="text/two.xhtml">Two</a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><span>Leaf group</span></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><foreign xmlns="urn:invalid">bad</foreign></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><svg xmlns="http://www.w3.org/2000/svg"><script>bad</script></svg></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml" onclick="bad()">One</a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml">Outer <span><a href="text/two.xhtml">nested</a></span></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml">Label<img/></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml">Label<img alt="safe" src="javascript:bad"/></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><picture><source srcset="https://example.invalid/image.png 1x"/><img alt="safe"/></picture></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><button formaction="https://example.invalid/submit">bad</button></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><input value="bad"/></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><select><option>bad</option></select></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><label>bad</label></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><audio controls="controls" title="bad"/></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><video autoplay="autoplay" title="bad"/></a></li></ol></nav></body></html>"#.as_slice(),
        br##"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><img alt="bad" usemap="#map"/></a></li></ol></nav></body></html>"##.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><svg xmlns="http://www.w3.org/2000/svg"><a href="https://example.invalid">bad</a></svg></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><svg xmlns="http://www.w3.org/2000/svg"><rect xmlns:xlink="http://www.w3.org/1999/xlink" xlink:href="https://example.invalid/image.svg"/></svg></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><svg xmlns="http://www.w3.org/2000/svg"><unknown>bad</unknown></svg></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><math xmlns="http://www.w3.org/1998/Math/MathML"><unknown>bad</unknown></math></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><span about="https://example.invalid/about">bad</span></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><span resource="javascript:bad">bad</span></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><span vocab="https://example.invalid/vocab#">bad</span></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><span prefix="schema: https://schema.org/">bad</span></a></li></ol></nav></body></html>"#.as_slice(),
        br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml"><span futurehref="https://example.invalid/future">bad</span></a></li></ol></nav></body></html>"#.as_slice(),
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
