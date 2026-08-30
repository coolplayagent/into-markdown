use crate::{
    Block, ConversionError, ConversionOptions, ConversionOutcome, ConversionRequest, ErrorCode,
    ErrorPolicy, ExecutionOptions, FormatHint, Inline, InputFormat, InputRef, default_engine,
};
use std::fmt::Write as _;
use std::future::Future;
use std::io::{Cursor, Write as _};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use zip::write::{FullFileOptions, SimpleFileOptions};

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

pub(super) fn epub(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let stored = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    writer.start_file("mimetype", stored).unwrap();
    writer.write_all(b"application/epub+zip").unwrap();
    let deflated = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for (path, bytes) in entries {
        writer.start_file(*path, deflated).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn noncanonical_mimetype_layout(
    entries: &[(&str, &[u8])],
    deflated: bool,
    leading_entry: bool,
    extra: Option<bool>,
) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let stored = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    if leading_entry {
        writer.start_file("leading.txt", stored).unwrap();
        writer.write_all(b"before mimetype").unwrap();
    }
    if let Some(central_only) = extra {
        let mut options = FullFileOptions::default()
            .compression_method(if deflated {
                zip::CompressionMethod::Deflated
            } else {
                zip::CompressionMethod::Stored
            })
            .unix_permissions(0o644);
        options.add_extra_data(0xcafe, Vec::new().into_boxed_slice(), central_only).unwrap();
        writer.start_file("mimetype", options).unwrap();
    } else {
        let options = SimpleFileOptions::default()
            .compression_method(if deflated {
                zip::CompressionMethod::Deflated
            } else {
                zip::CompressionMethod::Stored
            })
            .unix_permissions(0o644);
        writer.start_file("mimetype", options).unwrap();
    }
    writer.write_all(b"application/epub+zip").unwrap();
    let deflated = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for (path, bytes) in entries {
        writer.start_file(*path, deflated).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn epub_with_mimetype_content(entries: &[(&str, &[u8])], content: &[u8]) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let stored = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    writer.start_file("mimetype", stored).unwrap();
    writer.write_all(content).unwrap();
    let deflated = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for (path, bytes) in entries {
        writer.start_file(*path, deflated).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

pub(super) fn convert(bytes: Vec<u8>) -> Result<crate::ConversionResult, ConversionError> {
    convert_with(bytes, ConversionOptions::default(), ExecutionOptions::default())
}

pub(super) fn convert_strict(bytes: Vec<u8>) -> Result<crate::ConversionResult, ConversionError> {
    convert_with(
        bytes,
        ConversionOptions { error_policy: ErrorPolicy::Strict, ..ConversionOptions::default() },
        ExecutionOptions::default(),
    )
}

fn convert_with(
    bytes: Vec<u8>,
    options: ConversionOptions,
    execution: ExecutionOptions,
) -> Result<crate::ConversionResult, ConversionError> {
    let mut request = ConversionRequest::new(InputRef::bytes(bytes, Some("book.epub")));
    request.hint = FormatHint { format: Some(InputFormat::Epub), ..FormatHint::default() };
    request.options = options;
    request.execution = execution;
    block_on(default_engine().unwrap().convert(request))
}

pub(super) fn container() -> &'static [u8] {
    br#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="OPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
}

pub(super) fn epub3_package() -> &'static [u8] {
    br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book-id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="book-id">urn:uuid:original-test-book</dc:identifier><dc:title>Original EPUB Three</dc:title><dc:creator>Example Author</dc:creator><dc:language>en</dc:language><meta property="dcterms:modified">2026-08-13T00:00:00Z</meta></metadata><manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="one" href="text/one.xhtml" media-type="application/xhtml+xml"/><item id="two" href="text/two.xhtml" media-type="application/xhtml+xml"/><item id="extra" href="text/extra.xhtml" media-type="application/xhtml+xml"/><item id="cover" href="images/cover.png" media-type="image/png" properties="cover-image"/><item id="style" href="styles/book.css" media-type="text/css"/></manifest><spine><itemref idref="one"/><itemref idref="extra" linear="no"/><itemref idref="two"/></spine></package>"#
}

pub(super) fn nav3() -> &'static [u8] {
    br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head><body><nav epub:type="toc"><ol><li><a href="text/one.xhtml">One</a><ol><li><a href="text/two.xhtml#target">Two</a></li></ol></li></ol></nav></body></html>"#
}

fn deep_nav(levels: usize) -> String {
    let mut value = String::from(
        "<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\"><body><nav epub:type=\"toc\">",
    );
    for level in 0..levels {
        write!(value, "<ol><li><a href=\"text/one.xhtml\">Level {level}</a>").unwrap();
    }
    for _ in 0..levels {
        value.push_str("</li></ol>");
    }
    value.push_str("</nav></body></html>");
    value
}

pub(super) fn chapter_one() -> &'static [u8] {
    br##"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Chapter One</title></head><body><main><h1>One</h1><p>Alpha <a href="two.xhtml#target">next</a>.</p><img src="../images/cover.png" alt="Cover art"/><p>Detail<a epub:type="noteref" href="#note-one">1</a> repeated<a epub:type="noteref" href="#note-one">1</a></p><aside epub:type="footnote" id="note-one"><p>Original footnote text.</p></aside></main></body></html>"##
}

pub(super) fn chapter_two() -> &'static [u8] {
    br#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter Two</title></head><body><main><h1 id="target">Two</h1><p>Omega</p></main></body></html>"#
}

pub(super) fn epub3_book(package: &[u8], navigation: Option<&[u8]>) -> Vec<u8> {
    let mut entries = vec![
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", package),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>".as_slice(),
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}".as_slice()),
    ];
    if let Some(navigation) = navigation {
        entries.push(("OPS/nav.xhtml", navigation));
    }
    epub(&entries)
}

pub(super) fn epub2_package() -> &'static [u8] {
    br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">urn:uuid:original-epub-two</dc:identifier><dc:title>Original EPUB Two</dc:title><dc:language>en</dc:language><meta name="cover" content="cover"/></metadata><manifest xml:base="text/"><item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/><item id="shell" href="dummy.svg" media-type="image/svg+xml" fallback="one"/><item id="one" href="one.xhtml" media-type="application/xhtml+xml"/><item id="two" href="two.xhtml" media-type="application/xhtml+xml"/><item id="cover" xml:base="../images/" href="cover.png" media-type="image/png"/></manifest><spine toc="ncx"><itemref idref="shell"/><itemref idref="two"/></spine></package>"#
}

pub(super) fn ncx2() -> &'static [u8] {
    br#"<?xml version="1.0"?><ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"><head/><docTitle><text>Original EPUB Two</text></docTitle><navMap><navPoint id="p1" playOrder="1"><navLabel><text>First</text></navLabel><content src="one.xhtml#one"/><navPoint id="p2" playOrder="2"><navLabel><text>Second</text></navLabel><content src="two.xhtml"/></navPoint></navPoint></navMap></ncx>"#
}

pub(super) fn epub2_one() -> &'static [u8] {
    br#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>First</title></head><body xml:base="../images/"><main><h1 id="one">First</h1><img src="cover.png" alt="Cover"/><p>EPUB two first.</p></main></body></html>"#
}

pub(super) const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

#[test]
fn epub3_spine_navigation_links_footnotes_and_referenced_image_are_stable() {
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>skip me</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{background:url(https://example.invalid/x)}"),
    ]);
    let result = convert(bytes).unwrap();
    result.document.validate().unwrap();
    assert_eq!(result.document.metadata.title.as_deref(), Some("Original EPUB Three"));
    let alpha = result.markdown.find("Alpha").unwrap();
    let omega = result.markdown.find("Omega").unwrap();
    assert!(alpha < omega);
    assert!(!result.markdown.contains("skip me"));
    assert_eq!(result.markdown.matches("Original footnote text").count(), 1);
    assert_eq!(result.assets.len(), 1);
    assert_eq!(result.assets[0].media_type, "image/png");
    assert!(result.assets[0].external_uri.is_none());
    assert!(result.diagnostics.iter().any(|item| item.code == "epub.spine.nonLinearSkipped"));
    assert!(!result.diagnostics.iter().any(|item| item.code == "epub.linkedResourcesOmitted"));

    let mut link_targets = Vec::new();
    let mut footnote_references = Vec::new();
    collect_links(&result.document.blocks, &mut link_targets, &mut footnote_references);
    assert!(link_targets.iter().any(|target| target == "OPS/text/two.xhtml#target"));
    assert_eq!(footnote_references, vec!["epub-footnote-000001", "epub-footnote-000001"]);
    assert_eq!(result
        .document
        .blocks
        .iter()
        .filter(|node| matches!(&node.block, Block::Footnote { label, .. } if label == "epub-footnote-000001"))
        .count(), 1);
    assert!(has_nested_list(&result.document.blocks));
}

#[test]
fn only_reachable_styles_and_their_css_dependencies_are_reported_as_omitted() {
    let package = std::str::from_utf8(epub3_package()).unwrap().replace(
        "</manifest>",
        r#"<item id="imported" href="styles/imported.css" media-type="text/css"/><item id="font" href="fonts/book.woff2" media-type="font/woff2"/></manifest>"#,
    );
    let chapter = std::str::from_utf8(chapter_one()).unwrap().replace(
        "<title>Chapter One</title>",
        r#"<title>Chapter One</title><link rel="stylesheet" href="../styles/book.css"/>"#,
    );
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", package.as_bytes()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter.as_bytes()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>skip me</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        (
            "OPS/styles/book.css",
            br#"a{content:"url('../missing-string.woff2')"}/* url('../missing-comment.woff2') */@import url('imported.css');"#,
        ),
        ("OPS/styles/imported.css", b"@font-face{src:url('../fonts/book.woff2')}"),
        ("OPS/fonts/book.woff2", b"font"),
    ]);

    let result = convert(bytes).unwrap();
    let diagnostic = result
        .diagnostics
        .iter()
        .find(|item| item.code == "epub.linkedResourcesOmitted")
        .expect("linked resources must be disclosed");
    assert!(diagnostic.message.contains("2 reachable CSS"));
    assert!(diagnostic.message.contains("1 reachable font"));
    assert_eq!(result.outcome(), ConversionOutcome::Degraded);
}

#[test]
fn large_reachable_css_is_scanned_in_place_under_the_shared_memory_limit() {
    let css = r#"a{content:"url('../missing-string.woff2')"}/* url('../missing-comment.woff2') */"#
        .repeat(8 * 1024);
    let chapter = std::str::from_utf8(chapter_one()).unwrap().replace(
        "<title>Chapter One</title>",
        r#"<title>Chapter One</title><link rel="stylesheet" href="../styles/book.css"/>"#,
    );
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter.as_bytes()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", css.as_bytes()),
    ]);
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = 8 * 1024 * 1024;
    options.limits.max_archive_compression_ratio = 10_000;
    let result = convert_with(bytes, options, ExecutionOptions::default()).unwrap();
    assert!(result.markdown.contains("Alpha"));
    let diagnostic = result
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "epub.linkedResourcesOmitted")
        .unwrap();
    assert!(diagnostic.message.contains("1 reachable CSS"));
    assert!(diagnostic.message.contains("0 reachable font"));
}

#[test]
fn metadata_duplicate_ids_fail_only_when_they_make_retained_relationships_ambiguous() {
    let base = String::from_utf8(epub3_package().to_vec()).unwrap();
    let opaque = base.replace(
        "</metadata>",
        r#"<meta id="opaque" property="schema:first">first</meta><meta id="opaque" property="schema:second">second</meta></metadata>"#,
    );
    let result = convert(epub3_book(opaque.as_bytes(), Some(nav3()))).unwrap();
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "epub.metadataDuplicateIdOmitted")
    );
    assert_eq!(
        result.document.metadata.properties.get("epub.meta.schema:first").map(String::as_str),
        Some("first")
    );
    assert!(!result.document.metadata.properties.contains_key("epub.meta.schema:second"));
    assert_eq!(
        convert_strict(epub3_book(opaque.as_bytes(), Some(nav3()))).unwrap_err().code(),
        ErrorCode::Malformed
    );

    for ambiguous in [
        base.replace(
            "</metadata>",
            r#"<dc:identifier id="book-id">second</dc:identifier></metadata>"#,
        ),
        base.replace(
            "</metadata>",
            r##"<meta id="target" property="schema:first">first</meta><meta id="target" property="schema:second">second</meta><meta property="schema:relation" refines="#target">relation</meta></metadata>"##,
        ),
    ] {
        assert_eq!(
            convert(epub3_book(ambiguous.as_bytes(), Some(nav3()))).unwrap_err().code(),
            ErrorCode::Malformed
        );
    }
}

#[test]
fn matching_epub2_and_epub3_cover_declarations_are_not_conflicts() {
    let epub3 = String::from_utf8(epub3_package().to_vec()).unwrap().replace(
        "</metadata>",
        r#"<meta name="cover" content="cover"/><meta name="cover" content="cover"/></metadata>"#,
    );
    assert_eq!(convert(epub3_book(epub3.as_bytes(), Some(nav3()))).unwrap().assets.len(), 1);

    let epub2 = String::from_utf8(epub2_package().to_vec()).unwrap().replace(
        r#"<meta name="cover" content="cover"/>"#,
        r#"<meta name="cover" content="cover"/><meta name="cover" content="cover"/>"#,
    );
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub2.as_bytes()),
        ("OPS/text/toc.ncx", ncx2()),
        ("OPS/text/dummy.svg", b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
        ("OPS/text/one.xhtml", epub2_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        ("OPS/images/cover.png", PNG),
    ]);
    assert_eq!(convert(bytes).unwrap().assets.len(), 1);
}

#[test]
fn authoritative_outcome_distinguishes_compatibility_audit_from_content_loss() {
    let package = br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="book">outcome-contract</dc:identifier><dc:title>Outcome Contract</dc:title><dc:language>en</dc:language><meta property="dcterms:modified">2026-08-30T00:00:00Z</meta></metadata><manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="chapter"/></spine></package>"#;
    let navigation = br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="chapter.xhtml">Chapter</a></li></ol></nav></body></html>"#;
    let chapter = chapter_two();
    let common = [
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", package),
        ("OPS/nav.xhtml", navigation),
        ("OPS/chapter.xhtml", chapter),
    ];

    let mut audited_entries = common.to_vec();
    audited_entries.push(("META-INF/rights.xml", b"<rights/>"));
    let audited = convert(epub(&audited_entries)).unwrap();
    assert!(audited.diagnostics.iter().any(|item| item.code == "epub.rightsMetadataIgnored"));
    assert_eq!(
        audited.outcome(),
        ConversionOutcome::Complete,
        "unexpected diagnostics: {:?}",
        audited.diagnostics
    );

    let recovered = convert(noncanonical_mimetype_layout(&common, false, true, None)).unwrap();
    assert!(recovered.diagnostics.iter().any(|item| item.code == "epub.mimetypeLayoutRecovered"));
    assert_eq!(recovered.outcome(), ConversionOutcome::Complete);

    let active = br#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Active</title></head><body><main><script>discard()</script><p>Usable content.</p></main></body></html>"#;
    let degraded = convert(epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", package),
        ("OPS/nav.xhtml", navigation),
        ("OPS/chapter.xhtml", active),
    ]))
    .unwrap();
    assert!(degraded.diagnostics.iter().any(|item| item.code == "epub.spine.activeContentRemoved"));
    assert_eq!(degraded.outcome(), ConversionOutcome::Degraded);
}

#[test]
fn large_navigation_drops_raw_storage_and_a_late_spine_nav_is_read_once() {
    let mut state = 0x1234_5678_u32;
    let mut label = String::with_capacity(512 * 1024 + 4);
    for _ in 0..512 * 1024 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        label.push(char::from(b'a' + u8::try_from((state >> 24) % 26).unwrap()));
    }
    label.push_str("TAIL");
    let navigation = format!(
        "<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\"><head><title>Contents</title></head><body><nav epub:type=\"toc\"><ol><li><a href=\"text/one.xhtml\">{label}</a></li></ol></nav></body></html>"
    );
    let package = br#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="book">nav-memory</dc:identifier><dc:title>Navigation Memory</dc:title><dc:language>en</dc:language><meta property="dcterms:modified">2026-08-30T00:00:00Z</meta></metadata><manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="one" href="text/one.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="one"/></spine></package>"#;
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = 32 * 1024 * 1024;
    let outside_spine = convert_with(
        epub(&[
            ("META-INF/container.xml", container()),
            ("OPS/content.opf", package),
            ("OPS/nav.xhtml", navigation.as_bytes()),
            ("OPS/text/one.xhtml", chapter_two()),
        ]),
        options.clone(),
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(outside_spine.markdown.contains("TAIL"));

    let late_package = String::from_utf8(package.to_vec())
        .unwrap()
        .replace("</spine>", "<itemref idref=\"nav\"/></spine>");
    let late_spine = convert_with(
        epub(&[
            ("META-INF/container.xml", container()),
            ("OPS/content.opf", late_package.as_bytes()),
            ("OPS/nav.xhtml", navigation.as_bytes()),
            ("OPS/text/one.xhtml", chapter_two()),
        ]),
        options,
        ExecutionOptions::default(),
    )
    .unwrap();
    assert!(late_spine.markdown.contains("TAIL"));
    assert!(late_spine.markdown.contains("Omega"));
}

#[test]
fn epub2_ncx_xml_base_and_manifest_fallback_are_supported() {
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub2_package()),
        ("OPS/text/toc.ncx", ncx2()),
        ("OPS/text/dummy.svg", b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
        ("OPS/text/one.xhtml", epub2_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        ("OPS/images/cover.png", PNG),
    ]);
    let result = convert(bytes).unwrap();
    assert_eq!(result.document.metadata.title.as_deref(), Some("Original EPUB Two"));
    assert!(result.markdown.contains("EPUB two first"));
    assert!(result.markdown.contains("Omega"));
    assert_eq!(result.assets.len(), 1);
    let mut links = Vec::new();
    let mut footnotes = Vec::new();
    collect_links(&result.document.blocks, &mut links, &mut footnotes);
    assert!(links.iter().any(|target| target == "OPS/text/one.xhtml#one"));
    assert!(has_nested_list(&result.document.blocks));
}

pub(super) fn has_nested_list(blocks: &[crate::BlockNode]) -> bool {
    blocks.iter().any(|node| {
        if let Block::List { items, .. } = &node.block {
            items.iter().any(|item| {
                item.blocks.iter().any(|child| matches!(child.block, Block::List { .. }))
                    || has_nested_list(&item.blocks)
            })
        } else {
            false
        }
    })
}

pub(super) fn collect_links(
    blocks: &[crate::BlockNode],
    links: &mut Vec<String>,
    footnotes: &mut Vec<String>,
) {
    for node in blocks {
        match &node.block {
            Block::Paragraph(inlines)
            | Block::Heading { content: inlines, .. }
            | Block::TimedSegment { content: inlines, .. } => {
                collect_inlines(inlines, links, footnotes);
            }
            Block::List { items, .. } => {
                for item in items {
                    collect_links(&item.blocks, links, footnotes);
                }
            }
            Block::Table { rows, .. } => {
                for cell in rows.iter().flat_map(|row| &row.cells) {
                    collect_links(&cell.blocks, links, footnotes);
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => collect_links(blocks, links, footnotes),
            _ => {}
        }
    }
}

fn collect_inlines(inlines: &[Inline], links: &mut Vec<String>, footnotes: &mut Vec<String>) {
    for inline in inlines {
        match inline {
            Inline::Link { target, content } => {
                links.push(target.clone());
                collect_inlines(content, links, footnotes);
            }
            Inline::FootnoteReference(label) => footnotes.push(label.clone()),
            _ => {}
        }
    }
}

#[test]
fn malformed_mimetype_and_missing_fragment_are_rejected() {
    let mut wrong = epub(&[("META-INF/container.xml", container())]);
    let position = wrong
        .windows(b"application/epub+zip".len())
        .position(|window| window == b"application/epub+zip")
        .unwrap();
    wrong[position] = b'X';
    assert_eq!(convert(wrong).unwrap_err().code(), ErrorCode::Malformed);
    let broken_two = br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><main><h1>Two</h1><p>Omega</p></main></body></html>"#;
    let broken = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", broken_two),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    assert_eq!(convert(broken).unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn best_effort_recovers_noncanonical_mimetype_layout_but_strict_rejects_it() {
    let entries = [
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>".as_slice(),
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}".as_slice()),
    ];
    for bytes in [
        noncanonical_mimetype_layout(&entries, true, false, None),
        noncanonical_mimetype_layout(&entries, false, true, None),
        noncanonical_mimetype_layout(&entries, false, false, Some(true)),
        noncanonical_mimetype_layout(&entries, false, false, Some(false)),
    ] {
        let result = convert(bytes.clone()).unwrap();
        assert!(result.markdown.contains("Alpha"));
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "epub.mimetypeLayoutRecovered")
        );
        assert_eq!(convert_strict(bytes).unwrap_err().code(), ErrorCode::Malformed);
    }

    let crlf = epub_with_mimetype_content(&entries, b"application/epub+zip\r\n");
    let result = convert(crlf.clone()).unwrap();
    assert!(result.markdown.contains("Alpha"));
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "epub.mimetypeLayoutRecovered")
    );
    assert_eq!(convert_strict(crlf).unwrap_err().code(), ErrorCode::Malformed);

    let trailing_space = epub_with_mimetype_content(&entries, b"application/epub+zip ");
    assert_eq!(convert(trailing_space).unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn package_version_metadata_navigation_and_multiple_rootfiles_follow_epub_contracts() {
    let base = String::from_utf8(epub3_package().to_vec()).unwrap();
    let accepted = base.replace("version=\"3.0\"", "version=\"3.3\"");
    assert_eq!(
        convert(epub3_book(accepted.as_bytes(), Some(nav3())))
            .unwrap()
            .document
            .metadata
            .properties
            .get("epub.version")
            .map(String::as_str),
        Some("3.3")
    );

    let invalid = [
        base.replace("version=\"3.0\"", "version=\"3.future\""),
        base.replace(" properties=\"nav\"", ""),
    ];
    for package in invalid {
        assert_eq!(
            convert(epub3_book(package.as_bytes(), Some(nav3()))).unwrap_err().code(),
            ErrorCode::Malformed
        );
    }

    let multiple = br#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="ignored.opf" media-type="application/octet-stream"/><rootfile full-path="OPS/content.opf" media-type="application/oebps-package+xml"/><rootfile full-path="missing.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    let bytes = epub(&[
        ("META-INF/container.xml", multiple),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    assert_eq!(convert(bytes).unwrap_err().code(), ErrorCode::Malformed);

    let too_deep = deep_nav(16);
    assert!(matches!(
        convert(epub3_book(epub3_package(), Some(too_deep.as_bytes()))),
        Err(ConversionError::ResourceLimit { limit: "documentDepth", .. })
    ));
}

#[test]
fn encryption_metadata_distinguishes_font_obfuscation_from_drm() {
    let package = String::from_utf8(epub3_package().to_vec()).unwrap().replace(
        "</manifest>",
        "<item id=\"font\" href=\"fonts/book.otf\" media-type=\"font/otf\"/></manifest>",
    );
    let obfuscation = br#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#"><enc:EncryptedData><enc:EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/><enc:CipherData><enc:CipherReference URI="OPS/fonts/book.otf"/></enc:CipherData></enc:EncryptedData></encryption>"#;
    let allowed = epub(&[
        ("META-INF/container.xml", container()),
        ("META-INF/encryption.xml", obfuscation),
        ("META-INF/rights.xml", b"<rights>inert and untrusted</rights>"),
        ("OPS/content.opf", package.as_bytes()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
        ("OPS/fonts/book.otf", b"OTTOfont"),
    ]);
    let result = convert(allowed).unwrap();
    assert!(result.diagnostics.iter().any(|item| item.code == "epub.fontObfuscationUnsupported"));
    assert!(result.diagnostics.iter().any(|item| item.code == "epub.rightsMetadataIgnored"));
    assert!(result.assets.iter().all(|asset| asset.media_type != "font/otf"));

    let drm = br#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#"><enc:EncryptedData><enc:EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#aes256-cbc"/><enc:CipherData><enc:CipherReference URI="OPS/text/one.xhtml"/></enc:CipherData></enc:EncryptedData></encryption>"#;
    let rejected = epub(&[
        ("META-INF/container.xml", container()),
        ("META-INF/encryption.xml", drm),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    assert!(matches!(convert(rejected), Err(ConversionError::Encrypted)));
}

#[test]
fn package_reference_cycles_missing_ids_and_alias_entries_fail_closed() {
    let base = String::from_utf8(epub3_package().to_vec()).unwrap();
    let cases = [
        base.replace(
            "id=\"one\" href=\"text/one.xhtml\" media-type=\"application/xhtml+xml\"",
            "id=\"one\" href=\"text/one.xhtml\" media-type=\"application/xhtml+xml\" fallback=\"two\"",
        )
        .replace(
            "id=\"two\" href=\"text/two.xhtml\" media-type=\"application/xhtml+xml\"",
            "id=\"two\" href=\"text/two.xhtml\" media-type=\"application/xhtml+xml\" fallback=\"one\"",
        ),
        base.replace("href=\"nav.xhtml\"", "href=\"../../../nav.xhtml\""),
        base.replace("id=\"two\"", "id=\"one\""),
    ];
    for package in cases {
        let bytes = epub(&[
            ("META-INF/container.xml", container()),
            ("OPS/content.opf", package.as_bytes()),
            ("OPS/nav.xhtml", nav3()),
            ("OPS/text/one.xhtml", chapter_one()),
            ("OPS/text/two.xhtml", chapter_two()),
            (
                "OPS/text/extra.xhtml",
                b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
            ),
            ("OPS/images/cover.png", PNG),
            ("OPS/styles/book.css", b"body{}"),
        ]);
        assert_eq!(convert(bytes).unwrap_err().code(), ErrorCode::Malformed);
    }

    let aliases = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/TEXT/ONE.XHTML", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    assert_eq!(convert(aliases).unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn missing_spine_targets_are_omitted_only_when_linear_content_remains() {
    let base = String::from_utf8(epub3_package().to_vec()).unwrap();
    for package in [
        base.replace(
            "<spine><itemref idref=\"one\"/>",
            "<spine><itemref idref=\"missing-before\"/><itemref idref=\"one\"/>",
        ),
        base.replace("idref=\"two\"", "idref=\"missing-after\""),
    ] {
        let bytes = epub3_book(package.as_bytes(), Some(nav3()));
        let result = convert(bytes.clone()).unwrap();
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "epub.spine.missingItemOmitted")
        );
        assert_eq!(convert_strict(bytes).unwrap_err().code(), ErrorCode::Malformed);
    }

    let all_missing = base
        .replace("idref=\"one\"", "idref=\"missing-one\"")
        .replace("idref=\"two\"", "idref=\"missing-two\"");
    assert_eq!(
        convert(epub3_book(all_missing.as_bytes(), Some(nav3()))).unwrap_err().code(),
        ErrorCode::Malformed
    );
}

#[test]
fn empty_navigation_is_omitted_only_in_best_effort() {
    let empty_nav = br#"<?xml version="1.0"?><!DOCTYPE html><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><h2>Contents</h2><ol/></nav></body></html>"#;
    let bytes = epub3_book(epub3_package(), Some(empty_nav));
    let result = convert(bytes.clone()).unwrap();
    assert!(result.markdown.contains("Alpha"));
    assert!(
        result.diagnostics.iter().any(|diagnostic| diagnostic.code == "epub.navigationOmitted")
    );
    assert_eq!(convert_strict(bytes).unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn asset_and_cancellation_budgets_are_request_scoped() {
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", b"body{}"),
    ]);
    let mut options = ConversionOptions::default();
    options.limits.max_asset_bytes = 1;
    assert!(matches!(
        convert_with(bytes.clone(), options, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_asset_bytes", .. })
    ));

    let cancellation = crate::CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        convert_with(
            bytes,
            ConversionOptions::default(),
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
        )
        .unwrap_err()
        .code(),
        ErrorCode::Cancelled
    );
}

#[test]
fn archive_tree_entry_expansion_ratio_and_memory_limits_apply_before_parsing() {
    let large_css = vec![b'x'; 32 * 1024];
    let bytes = epub(&[
        ("META-INF/container.xml", container()),
        ("OPS/content.opf", epub3_package()),
        ("OPS/nav.xhtml", nav3()),
        ("OPS/text/one.xhtml", chapter_one()),
        ("OPS/text/two.xhtml", chapter_two()),
        (
            "OPS/text/extra.xhtml",
            b"<html xmlns='http://www.w3.org/1999/xhtml'><body><p>x</p></body></html>",
        ),
        ("OPS/images/cover.png", PNG),
        ("OPS/styles/book.css", &large_css),
    ]);

    let mut entries = ConversionOptions::default();
    entries.limits.max_archive_entries = 2;
    assert!(matches!(
        convert_with(bytes.clone(), entries, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
    ));

    let mut expanded = ConversionOptions::default();
    expanded.limits.max_decompressed_bytes = 20;
    expanded.limits.max_archive_compression_ratio = 1_000;
    let expanded_error =
        convert_with(bytes.clone(), expanded, ExecutionOptions::default()).unwrap_err();
    assert!(
        matches!(
            &expanded_error,
            ConversionError::ResourceLimit { limit: "max_decompressed_bytes", .. }
        ),
        "{expanded_error:?}"
    );

    let mut ratio = ConversionOptions::default();
    ratio.limits.max_archive_compression_ratio = 1;
    assert!(matches!(
        convert_with(bytes.clone(), ratio, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_archive_compression_ratio", .. })
    ));

    let mut depth = ConversionOptions::default();
    depth.limits.max_archive_depth = 0;
    assert!(matches!(
        convert_with(bytes.clone(), depth, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_archive_depth", .. })
    ));

    let mut nesting = ConversionOptions::default();
    nesting.limits.max_nesting_depth = 7;
    nesting.limits.max_archive_compression_ratio = 1_000;
    assert!(matches!(
        convert_with(bytes.clone(), nesting, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_nesting_depth", .. })
    ));

    let mut memory = ConversionOptions::default();
    memory.limits.max_memory_bytes = 1;
    memory.limits.max_archive_compression_ratio = 1_000;
    assert!(matches!(
        convert_with(bytes, memory, ExecutionOptions::default()),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
}
