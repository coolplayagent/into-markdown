use super::*;
use into_markdown_core::{ConversionOptions, ErrorCode, ExecutionOptions, Inline};
use std::collections::BTreeMap;

const END: u32 = 0xffff_fffe;
const FREE: u32 = 0xffff_ffff;
const FAT: u32 = 0xffff_fffd;
const NONE: u32 = 0xffff_ffff;

#[path = "compatibility_tests.rs"]
mod compatibility_tests;

#[test]
fn converter_identity_is_stable() {
    assert_eq!(MsgConverter.id(), "builtin.converter.msg");
    assert_eq!(MsgConverter.supported_formats(), &[InputFormat::OutlookMsg]);
}

#[test]
fn plain_message_extracts_headers_time_transport_and_provenance() {
    let bytes = message(vec![], vec![], Some("Plain body"), None, None);
    let output = convert(&bytes).unwrap();
    assert_eq!(output.document.metadata.title.as_deref(), Some("Repository MSG"));
    assert_eq!(output.document.metadata.properties["msg.sender"], "Alice <alice@example.test>");
    assert_eq!(output.document.metadata.properties["msg.to"], "Bob <bob@example.test>");
    assert_eq!(output.document.metadata.properties["msg.time"], "1970-01-01T00:00:00Z");
    assert!(output.document.metadata.properties["msg.transport_headers"].contains("Message-ID"));
    assert!(output.document.blocks.iter().all(|block| {
        block.provenance.locator.part.as_deref().is_some_and(|part| part.starts_with("msg/"))
    }));
    assert!(paragraph_text(&output).contains("Plain body"));
}

#[test]
fn storage_stream_metadata_recovers_only_in_best_effort() {
    let original = message(
        vec![AttachmentFixture::value("notes.txt", "text/plain", None, b"notes".to_vec())],
        vec![],
        Some("Body before attachments"),
        None,
        None,
    );
    let expected = convert(&original).unwrap();
    for (start, size) in [(0, 0), (123_456, 987_654)] {
        let mut bytes = original.clone();
        // This fixture serializes directory sectors before the MiniFAT.
        let directory_end =
            (u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize + 1) * 512;
        for entry in bytes[512..directory_end].chunks_exact_mut(128) {
            if entry[66] == 1 {
                entry[116..120].copy_from_slice(&u32::to_le_bytes(start));
                entry[120..128].copy_from_slice(&u64::to_le_bytes(size));
            }
        }
        let mut options =
            ConversionOptions { error_policy: ErrorPolicy::Strict, ..Default::default() };
        assert_eq!(convert_with(&bytes, &options).unwrap_err().code(), ErrorCode::Malformed);
        options.error_policy = ErrorPolicy::BestEffort;
        let output = convert_with(&bytes, &options).unwrap();
        assert_eq!(output.document, expected.document);
        assert_eq!(output.assets, expected.assets);
        assert_eq!(
            output
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "msg.cfb.storageMetadataIgnored")
                .count(),
            1
        );
        let text = paragraph_text(&output);
        let positions = [
            "Repository MSG",
            "From: Alice",
            "To: Bob",
            "Body before attachments",
            "Attachments",
            "notes.txt",
        ]
        .map(|needle| text.find(needle).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

#[test]
fn html_cid_and_by_value_attachment_are_offline_assets() {
    let attachment =
        AttachmentFixture::value("logo.png", "image/png", Some("logo@example.test"), tiny_png());
    let bytes = message(
        vec![attachment],
        vec![],
        Some("fallback"),
        Some(b"<main><h2>HTML body</h2><img src='cid:logo@example.test' alt='logo'></main>"),
        None,
    );
    let output = convert(&bytes).unwrap();
    assert_eq!(output.document.metadata.properties["msg.body_kind"], "html");
    assert_eq!(output.assets.len(), 1);
    assert_eq!(output.assets[0].bytes, tiny_png());
    assert!(output.assets[0].external_uri.is_none());
    let images = output
        .document
        .blocks
        .iter()
        .filter(|block| matches!(block.block, into_markdown_core::Block::Image { .. }))
        .count();
    assert_eq!(images, 1);
    assert!(!paragraph_text(&output).contains("Attachments"));
}

#[test]
fn cid_resources_require_an_exact_reference_and_an_audited_image() {
    let unreferenced = message(
        vec![AttachmentFixture::value(
            "logo.png",
            "image/png",
            Some("logo@example.test"),
            tiny_png(),
        )],
        vec![],
        None,
        Some(b"<main><p>No inline image</p></main>"),
        None,
    );
    let output = convert(&unreferenced).unwrap();
    assert!(paragraph_text(&output).contains("Attachments"));
    assert!(
        !output
            .document
            .blocks
            .iter()
            .any(|block| matches!(block.block, into_markdown_core::Block::Image { .. }))
    );

    let non_image = message(
        vec![AttachmentFixture::value(
            "notes.txt",
            "text/plain",
            Some("notes@example.test"),
            b"not an image".to_vec(),
        )],
        vec![],
        None,
        Some(b"<main><img src='cid:notes@example.test' alt='notes'></main>"),
        None,
    );
    let output = convert(&non_image).unwrap();
    assert!(paragraph_text(&output).contains("notes"));
    assert!(
        !output
            .document
            .blocks
            .iter()
            .any(|block| matches!(block.block, into_markdown_core::Block::Image { .. }))
    );
    assert!(output.diagnostics.iter().any(|diagnostic| diagnostic.code == "html.cidImageRejected"));
}

#[test]
fn duplicate_content_ids_are_rejected_as_ambiguous() {
    let attachment =
        || AttachmentFixture::value("logo.png", "image/png", Some("logo@example.test"), tiny_png());
    let bytes = message(
        vec![attachment(), attachment()],
        vec![],
        None,
        Some(b"<main><img src='cid:logo@example.test'></main>"),
        None,
    );
    assert_eq!(convert(&bytes).unwrap_err().code(), ErrorCode::Malformed);
}

#[test]
fn attachment_and_nested_message_keep_complete_source_chain() {
    let nested = embedded_message("Nested body");
    let bytes = message(
        vec![
            AttachmentFixture::value("notes.txt", "text/plain", None, b"notes".to_vec()),
            AttachmentFixture::nested("forwarded.msg", nested),
        ],
        vec![],
        Some("Outer body"),
        None,
        None,
    );
    let output = convert(&bytes).unwrap();
    assert_eq!(output.assets.len(), 1);
    assert!(paragraph_text(&output).contains("Nested body"));
    assert!(
        output.document.metadata.properties["msg.attachment.1.source"]
            .contains("__attach_version1.0_#00000000")
    );
    assert!(output.document.blocks.iter().any(|block| {
        block
            .provenance
            .locator
            .part
            .as_deref()
            .is_some_and(|part| part.contains("__attach_version1.0_#00000001/__substg1.0_3701000D"))
    }));
}

#[test]
fn string8_codepage_is_strict_and_supported() {
    let bytes = message(vec![], vec![root_string8(0x1000, b"caf\xe9")], None, None, None);
    let output = convert(&bytes).unwrap();
    assert!(paragraph_text(&output).contains("café"));
}

#[test]
fn rtf_selection_uses_lzfu_then_the_narrow_adapter() {
    struct Fake;
    impl body::BodyAdapter for Fake {
        fn html(
            &self,
            _: &[u8],
            _: &[crate::html::EmbeddedImage],
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<ConverterOutput, ConversionError> {
            panic!("HTML adapter must not be selected")
        }
        fn rtf(
            &self,
            bytes: &[u8],
            options: &ConversionOptions,
            context: &ExecutionContext,
        ) -> Result<ConverterOutput, ConversionError> {
            assert_eq!(bytes, b"{\\rtf1 repository}");
            crate::rtf::convert_rtf_bytes(bytes, options, context)
        }
    }
    let raw = b"{\\rtf1 repository}";
    let envelope = lzfu_uncompressed(raw);
    let bytes = message(vec![], vec![], None, None, Some(&envelope));
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let output = convert_msg(&bytes, &options, &context, &Fake).unwrap();
    assert_eq!(output.document.metadata.properties["msg.body_kind"], "rtf");
}

#[test]
fn builtin_rtf_bodies_reuse_context_without_forging_decoded_byte_offsets() {
    let raw = b"{\\rtf1\\ansi Repository RTF body}";
    for envelope in [lzfu_compressed_literals(raw), lzfu_uncompressed(raw)] {
        let bytes = message(vec![], vec![], None, None, Some(&envelope));
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let output = convert_msg(&bytes, &options, &context, &BuiltinBodyAdapter).unwrap();

        assert_eq!(output.document.metadata.properties["msg.body_kind"], "rtf");
        assert!(paragraph_text(&output).contains("Repository RTF body"));
        assert!(output.leased_memory_for(&context) > 0);
        let sources = output
            .document
            .blocks
            .iter()
            .filter(|block| {
                block
                    .provenance
                    .locator
                    .part
                    .as_deref()
                    .is_some_and(|part| part.ends_with("#10090102"))
            })
            .collect::<Vec<_>>();
        assert!(!sources.is_empty());
        assert!(sources.iter().all(|block| {
            let locator = &block.provenance.locator;
            locator.byte_start.is_none() && locator.byte_end.is_none()
        }));
    }
}

#[test]
fn builtin_rtf_body_preserves_european_font_encodings() {
    let raw = b"{\\rtf1\\ansi{\\fonttbl{\\f0\\fcharset238 CE;}{\\f1\\fcharset204 Cyrillic;}{\\f2\\fcharset999 Unused;}}\\f0 \\'bf\\f1 \\'d3\\uc1\\u1078?}";
    for envelope in [lzfu_compressed_literals(raw), lzfu_uncompressed(raw)] {
        let bytes = message(vec![], vec![], None, None, Some(&envelope));
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let output = convert_msg(&bytes, &options, &context, &BuiltinBodyAdapter).unwrap();
        assert_eq!(output.document.metadata.properties["msg.body_kind"], "rtf");
        assert!(paragraph_text(&output).contains("żУж"));
        assert!(!paragraph_text(&output).contains('\u{fffd}'));
    }
}

#[test]
fn corrupt_and_adversarial_cfb_fail_closed_without_panics() {
    let valid = message(vec![], vec![], Some("body"), None, None);
    let mut cases = vec![valid[..valid.len() - 17].to_vec()];
    let mut fat_cycle = valid.clone();
    let fat_sector = u32::from_le_bytes(fat_cycle[76..80].try_into().unwrap()) as usize;
    let fat_offset = (fat_sector + 1) * 512;
    fat_cycle[fat_offset..fat_offset + 4].copy_from_slice(&0_u32.to_le_bytes());
    cases.push(fat_cycle);
    let mut overlap = valid.clone();
    let stream_entries = overlap[512..]
        .chunks_exact(128)
        .enumerate()
        .filter(|(_, entry)| {
            entry[66] == 2 && u64::from_le_bytes(entry[120..128].try_into().unwrap()) > 0
        })
        .map(|(index, _)| 512 + index * 128)
        .take(2)
        .collect::<Vec<_>>();
    let repeated_start = overlap[stream_entries[0] + 116..stream_entries[0] + 120].to_vec();
    overlap[stream_entries[1] + 116..stream_entries[1] + 120].copy_from_slice(&repeated_start);
    cases.push(overlap);
    let mut directory_cycle = valid.clone();
    let root_child = u32::from_le_bytes(directory_cycle[512 + 76..512 + 80].try_into().unwrap());
    let child_offset = 512 + usize::try_from(root_child).unwrap() * 128;
    directory_cycle[child_offset + 72..child_offset + 76]
        .copy_from_slice(&root_child.to_le_bytes());
    cases.push(directory_cycle);
    let mut minifat_cycle = valid.clone();
    let minifat_sector = u32::from_le_bytes(minifat_cycle[60..64].try_into().unwrap());
    let long_stream = minifat_cycle[512..]
        .chunks_exact(128)
        .find(|entry| {
            entry[66] == 2 && u64::from_le_bytes(entry[120..128].try_into().unwrap()) > 64
        })
        .unwrap();
    let mini_start = u32::from_le_bytes(long_stream[116..120].try_into().unwrap());
    let minifat_offset = (usize::try_from(minifat_sector).unwrap() + 1) * 512
        + usize::try_from(mini_start).unwrap() * 4;
    minifat_cycle[minifat_offset..minifat_offset + 4].copy_from_slice(&mini_start.to_le_bytes());
    cases.push(minifat_cycle);
    for bytes in cases {
        for error_policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
            let options = ConversionOptions { error_policy, ..Default::default() };
            let error = convert_with(&bytes, &options).unwrap_err();
            assert!(
                matches!(error.code(), ErrorCode::Malformed | ErrorCode::ResourceLimit),
                "{error}"
            );
        }
    }
}

#[test]
fn malicious_properties_codepages_names_and_depth_are_rejected() {
    let mut wrong_length = property_unicode(0x2000, "length");
    wrong_length.value[..4].copy_from_slice(&999_u32.to_le_bytes());
    let property_error =
        convert(&message(vec![], vec![wrong_length], Some("body"), None, None)).unwrap_err();
    assert_eq!(property_error.code(), ErrorCode::Malformed);

    let codepage_error =
        convert(&message(vec![], vec![property_long(0x3ffd, 42)], Some("body"), None, None))
            .unwrap_err();
    assert_eq!(codepage_error.code(), ErrorCode::Malformed);

    let unsafe_name = message(
        vec![AttachmentFixture::value("../escape.txt", "text/plain", None, b"data".to_vec())],
        vec![],
        Some("body"),
        None,
        None,
    );
    assert_eq!(convert(&unsafe_name).unwrap_err().code(), ErrorCode::Malformed);

    let nested = message(
        vec![AttachmentFixture::nested("nested.msg", embedded_message("nested"))],
        vec![],
        Some("body"),
        None,
        None,
    );
    let mut options = ConversionOptions::default();
    options.limits.max_nesting_depth = 1;
    assert_eq!(convert_with(&nested, &options).unwrap_err().code(), ErrorCode::ResourceLimit);
}

#[test]
fn limits_cover_input_entries_assets_and_nested_depth() {
    let bytes = message(
        vec![AttachmentFixture::value("large.bin", "application/octet-stream", None, vec![1; 65])],
        vec![],
        Some("body"),
        None,
        None,
    );
    let mut options = ConversionOptions::default();
    options.limits.max_asset_bytes = 64;
    assert_eq!(convert_with(&bytes, &options).unwrap_err().code(), ErrorCode::ResourceLimit);
    options = ConversionOptions::default();
    options.limits.max_archive_entries = 1;
    assert_eq!(convert_with(&bytes, &options).unwrap_err().code(), ErrorCode::ResourceLimit);
}

fn convert(bytes: &[u8]) -> Result<ConverterOutput, ConversionError> {
    convert_with(bytes, &ConversionOptions::default())
}

fn convert_with(
    bytes: &[u8],
    options: &ConversionOptions,
) -> Result<ConverterOutput, ConversionError> {
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    convert_msg(bytes, options, &context, &BuiltinBodyAdapter)
}

fn paragraph_text(output: &ConverterOutput) -> String {
    let mut text = String::new();
    collect_text(&output.document.blocks, &mut text);
    text
}

fn collect_text(blocks: &[into_markdown_core::BlockNode], output: &mut String) {
    for block in blocks {
        match &block.block {
            into_markdown_core::Block::Paragraph(inlines)
            | into_markdown_core::Block::Heading { content: inlines, .. } => {
                for inline in inlines {
                    match inline {
                        Inline::Text { value, .. } => output.push_str(value),
                        Inline::LineBreak => output.push('\n'),
                        _ => {}
                    }
                }
                output.push('\n');
            }
            into_markdown_core::Block::Page { blocks, .. }
            | into_markdown_core::Block::Slide { blocks, .. }
            | into_markdown_core::Block::Sheet { blocks, .. }
            | into_markdown_core::Block::Footnote { blocks, .. } => collect_text(blocks, output),
            _ => {}
        }
    }
}

fn tiny_png() -> Vec<u8> {
    let hex = b"89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360606060000000050001a5f645400000000049454e44ae426082";
    hex.chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[derive(Clone)]
enum AttachmentFixture {
    Value { filename: String, mime: String, cid: Option<String>, bytes: Vec<u8> },
    Nested { filename: String, entries: Vec<TestEntry> },
}

impl AttachmentFixture {
    fn value(filename: &str, mime: &str, cid: Option<&str>, bytes: Vec<u8>) -> Self {
        Self::Value {
            filename: filename.into(),
            mime: mime.into(),
            cid: cid.map(str::to_owned),
            bytes,
        }
    }
    fn nested(filename: &str, entries: Vec<TestEntry>) -> Self {
        Self::Nested { filename: filename.into(), entries }
    }
}

#[derive(Clone)]
struct TestEntry {
    path: Vec<String>,
    bytes: Option<Vec<u8>>,
}

fn message(
    attachments: Vec<AttachmentFixture>,
    extra_root: Vec<PropertyRecord>,
    plain: Option<&str>,
    html: Option<&[u8]>,
    rtf: Option<&[u8]>,
) -> Vec<u8> {
    let attachment_count = attachments.len();
    let supplied_codepage = extra_root.iter().any(|record| record.id == 0x3ffd);
    let mut root_properties = vec![
        property_unicode(0x0037, "Repository MSG"),
        property_unicode(0x0c1a, "Alice"),
        property_unicode(0x0c1f, "alice@example.test"),
        property_time(0x0039, 116_444_736_000_000_000),
        property_unicode(0x007d, "Message-ID: <repository@example.test>\r\nX-Offline: true\r\n"),
    ];
    if !supplied_codepage {
        root_properties.push(property_long(0x3ffd, 1252));
    }
    root_properties.extend(extra_root);
    if let Some(value) = plain {
        root_properties.push(property_unicode(0x1000, value));
    }
    if let Some(value) = html {
        root_properties.push(variable(0x1013, 0x0102, value.to_vec()));
    }
    if let Some(value) = rtf {
        root_properties.push(variable(0x1009, 0x0102, value.to_vec()));
    }
    let mut entries = Vec::new();
    entries.extend(root_property_streams(&root_properties));
    entries.push(TestEntry {
        path: vec!["__properties_version1.0".into()],
        bytes: Some(root_properties_stream(&root_properties, 1, attachment_count)),
    });

    let recipient_path = vec!["__recip_version1.0_#00000000".to_owned()];
    let recipient = vec![
        property_long(0x0c15, 1),
        property_unicode(0x3001, "Bob"),
        property_unicode(0x39fe, "bob@example.test"),
    ];
    entries.push(TestEntry { path: recipient_path.clone(), bytes: None });
    entries.extend(property_streams(&recipient_path, &recipient));
    entries.push(TestEntry {
        path: joined(&recipient_path, "__properties_version1.0"),
        bytes: Some(properties_stream(false, &recipient)),
    });

    for (index, attachment) in attachments.into_iter().enumerate() {
        let base = vec![format!("__attach_version1.0_#{index:08X}")];
        entries.push(TestEntry { path: base.clone(), bytes: None });
        let mut properties = Vec::new();
        match attachment {
            AttachmentFixture::Value { filename, mime, cid, bytes } => {
                properties.push(property_long(0x3705, 1));
                add_unicode(&mut entries, &base, 0x3707, &filename, &mut properties);
                add_unicode(&mut entries, &base, 0x370e, &mime, &mut properties);
                if let Some(cid) = cid {
                    add_unicode(&mut entries, &base, 0x3712, &cid, &mut properties);
                }
                add_binary(&mut entries, &base, 0x3701, &bytes, &mut properties);
            }
            AttachmentFixture::Nested { filename, entries: nested } => {
                properties.push(property_long(0x3705, 5));
                add_unicode(&mut entries, &base, 0x3707, &filename, &mut properties);
                properties.push(property_object(0x3701));
                let object = joined(&base, "__substg1.0_3701000D");
                entries.push(TestEntry { path: object.clone(), bytes: None });
                for mut entry in nested {
                    let mut path = object.clone();
                    path.append(&mut entry.path);
                    entries.push(TestEntry { path, bytes: entry.bytes });
                }
            }
        }
        entries.push(TestEntry {
            path: joined(&base, "__properties_version1.0"),
            bytes: Some(properties_stream(false, &properties)),
        });
    }
    cfb(entries)
}

fn embedded_message(body: &str) -> Vec<TestEntry> {
    let records = vec![property_unicode(0x0037, "Nested"), property_unicode(0x1000, body)];
    let mut entries = Vec::new();
    entries.extend(root_property_streams(&records));
    let mut header = root_properties_stream(&records, 0, 0);
    header.drain(24..32);
    entries.push(TestEntry { path: vec!["__properties_version1.0".into()], bytes: Some(header) });
    entries
}

fn root_string8(id: u16, value: &[u8]) -> PropertyRecord {
    variable(id, 0x001e, value.to_vec())
}

#[derive(Clone)]
struct PropertyRecord {
    id: u16,
    kind: u16,
    value: [u8; 8],
    stream: Option<Vec<u8>>,
}

fn property_unicode(id: u16, value: &str) -> PropertyRecord {
    let bytes = value.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    variable(id, 0x001f, bytes)
}
fn property_long(id: u16, value: i32) -> PropertyRecord {
    let mut raw = [0; 8];
    raw[..4].copy_from_slice(&value.to_le_bytes());
    PropertyRecord { id, kind: 0x0003, value: raw, stream: None }
}
fn property_time(id: u16, value: u64) -> PropertyRecord {
    PropertyRecord { id, kind: 0x0040, value: value.to_le_bytes(), stream: None }
}
fn property_object(id: u16) -> PropertyRecord {
    let mut value = [0; 8];
    value[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    value[4] = 1;
    PropertyRecord { id, kind: 0x000d, value, stream: None }
}
fn variable(id: u16, kind: u16, bytes: Vec<u8>) -> PropertyRecord {
    let mut value = [0; 8];
    let terminator_bytes = match kind {
        0x001e => 1,
        0x001f => 2,
        _ => 0,
    };
    value[..4]
        .copy_from_slice(&u32::try_from(bytes.len() + terminator_bytes).unwrap().to_le_bytes());
    PropertyRecord { id, kind, value, stream: Some(bytes) }
}
fn add_unicode(
    entries: &mut Vec<TestEntry>,
    base: &[String],
    id: u16,
    value: &str,
    records: &mut Vec<PropertyRecord>,
) {
    let record = property_unicode(id, value);
    entries.push(TestEntry {
        path: joined(base, &format!("__substg1.0_{id:04X}001F")),
        bytes: record.stream.clone(),
    });
    records.push(record);
}
fn add_binary(
    entries: &mut Vec<TestEntry>,
    base: &[String],
    id: u16,
    bytes: &[u8],
    records: &mut Vec<PropertyRecord>,
) {
    let record = variable(id, 0x0102, bytes.to_vec());
    entries.push(TestEntry {
        path: joined(base, &format!("__substg1.0_{id:04X}0102")),
        bytes: record.stream.clone(),
    });
    records.push(record);
}
fn property_streams(base: &[String], records: &[PropertyRecord]) -> Vec<TestEntry> {
    records
        .iter()
        .filter_map(|record| {
            record.stream.as_ref().map(|bytes| TestEntry {
                path: joined(base, &format!("__substg1.0_{:04X}{:04X}", record.id, record.kind)),
                bytes: Some(bytes.clone()),
            })
        })
        .collect()
}
fn root_property_streams(records: &[PropertyRecord]) -> Vec<TestEntry> {
    property_streams(&[], records)
}
fn properties_stream(root: bool, records: &[PropertyRecord]) -> Vec<u8> {
    let mut output = vec![0; if root { 32 } else { 8 }];
    for record in records {
        let tag = (u32::from(record.id) << 16) | u32::from(record.kind);
        output.extend_from_slice(&tag.to_le_bytes());
        output.extend_from_slice(&[0; 4]);
        output.extend_from_slice(&record.value);
    }
    output
}

fn root_properties_stream(
    records: &[PropertyRecord],
    recipients: usize,
    attachments: usize,
) -> Vec<u8> {
    let mut output = properties_stream(true, records);
    output[16..20].copy_from_slice(&u32::try_from(recipients).unwrap().to_le_bytes());
    output[20..24].copy_from_slice(&u32::try_from(attachments).unwrap().to_le_bytes());
    output
}
fn joined(base: &[String], name: &str) -> Vec<String> {
    let mut path = base.to_vec();
    path.push(name.into());
    path
}

#[derive(Clone)]
struct DirectoryFixture {
    path: Vec<String>,
    stream: Option<Vec<u8>>,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
}

#[allow(clippy::too_many_lines)] // Test-only CFB serialization keeps mutation offsets visible.
fn cfb(entries: Vec<TestEntry>) -> Vec<u8> {
    let mut paths = BTreeMap::<Vec<String>, Option<Vec<u8>>>::new();
    paths.insert(Vec::new(), None);
    for entry in entries {
        for length in 1..entry.path.len() {
            paths.entry(entry.path[..length].to_vec()).or_insert(None);
        }
        assert!(paths.insert(entry.path, entry.bytes).is_none());
    }
    let mut directory = paths
        .into_iter()
        .map(|(path, stream)| DirectoryFixture {
            path,
            stream,
            left: NONE,
            right: NONE,
            child: NONE,
            start: END,
        })
        .collect::<Vec<_>>();
    directory.sort_by(|left, right| {
        left.path.len().cmp(&right.path.len()).then(left.path.cmp(&right.path))
    });
    assert!(directory[0].path.is_empty());
    for parent in 0..directory.len() {
        if directory[parent].stream.is_some() {
            continue;
        }
        let mut children = directory
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate.path.len() == directory[parent].path.len() + 1
                    && candidate.path.starts_with(&directory[parent].path)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        children.sort_by(|left, right| {
            directory[*left].path.last().cmp(&directory[*right].path.last())
        });
        if let Some(first) = children.first() {
            directory[parent].child = u32::try_from(*first).unwrap();
        }
        for pair in children.windows(2) {
            directory[pair[0]].right = u32::try_from(pair[1]).unwrap();
        }
    }
    let mut mini_data = Vec::new();
    let mut minifat = Vec::new();
    for entry in directory.iter_mut().filter(|entry| entry.stream.is_some()) {
        let bytes = entry.stream.as_ref().unwrap();
        if bytes.is_empty() {
            continue;
        }
        entry.start = u32::try_from(minifat.len()).unwrap();
        let count = bytes.len().div_ceil(64);
        for offset in 0..count {
            let index = minifat.len();
            minifat.push(if offset + 1 == count { END } else { u32::try_from(index + 1).unwrap() });
        }
        mini_data.extend_from_slice(bytes);
        mini_data.resize(mini_data.len().next_multiple_of(64), 0);
    }
    let directory_sectors = (directory.len() * 128).div_ceil(512);
    let minifat_sectors = (minifat.len() * 4).div_ceil(512);
    let root_sectors = mini_data.len().div_ceil(512);
    let minifat_start = u32::try_from(directory_sectors).unwrap();
    let root_start = minifat_start + u32::try_from(minifat_sectors).unwrap();
    let fat_sector = root_start + u32::try_from(root_sectors).unwrap();
    directory[0].start = if root_sectors == 0 { END } else { root_start };
    let root_size = mini_data.len();

    let mut directory_bytes = Vec::new();
    for (index, entry) in directory.iter().enumerate() {
        let mut raw = [0_u8; 128];
        let name = if index == 0 { "Root Entry" } else { entry.path.last().unwrap() };
        let units = name.encode_utf16().chain(std::iter::once(0)).collect::<Vec<_>>();
        assert!(units.len() <= 32);
        for (offset, unit) in units.iter().enumerate() {
            raw[offset * 2..offset * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        raw[64..66].copy_from_slice(&u16::try_from(units.len() * 2).unwrap().to_le_bytes());
        raw[66] = if index == 0 {
            5
        } else if entry.stream.is_some() {
            2
        } else {
            1
        };
        raw[67] = 1;
        raw[68..72].copy_from_slice(&entry.left.to_le_bytes());
        raw[72..76].copy_from_slice(&entry.right.to_le_bytes());
        raw[76..80].copy_from_slice(&entry.child.to_le_bytes());
        raw[116..120].copy_from_slice(&entry.start.to_le_bytes());
        let size = if index == 0 { root_size } else { entry.stream.as_ref().map_or(0, Vec::len) };
        raw[120..128].copy_from_slice(&u64::try_from(size).unwrap().to_le_bytes());
        directory_bytes.extend_from_slice(&raw);
    }
    directory_bytes.resize(directory_sectors * 512, 0);
    let mut minifat_bytes =
        minifat.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<_>>();
    minifat_bytes.resize(minifat_sectors * 512, 0xff);
    mini_data.resize(root_sectors * 512, 0);
    let sector_count = usize::try_from(fat_sector).unwrap() + 1;
    assert!(sector_count <= 128);
    let mut fat_entries = vec![FREE; 128];
    chain(&mut fat_entries, 0, directory_sectors);
    chain(&mut fat_entries, usize::try_from(minifat_start).unwrap(), minifat_sectors);
    chain(&mut fat_entries, usize::try_from(root_start).unwrap(), root_sectors);
    fat_entries[usize::try_from(fat_sector).unwrap()] = FAT;
    let fat_bytes = fat_entries.into_iter().flat_map(u32::to_le_bytes).collect::<Vec<_>>();

    let mut header = [0_u8; 512];
    header[..8].copy_from_slice(CFB_SIGNATURE);
    header[24..26].copy_from_slice(&0x003e_u16.to_le_bytes());
    header[26..28].copy_from_slice(&3_u16.to_le_bytes());
    header[28..30].copy_from_slice(&0xfffe_u16.to_le_bytes());
    header[30..32].copy_from_slice(&9_u16.to_le_bytes());
    header[32..34].copy_from_slice(&6_u16.to_le_bytes());
    header[44..48].copy_from_slice(&1_u32.to_le_bytes());
    header[48..52].copy_from_slice(&0_u32.to_le_bytes());
    header[56..60].copy_from_slice(&4096_u32.to_le_bytes());
    header[60..64]
        .copy_from_slice(&(if minifat_sectors == 0 { END } else { minifat_start }).to_le_bytes());
    header[64..68].copy_from_slice(&u32::try_from(minifat_sectors).unwrap().to_le_bytes());
    header[68..72].copy_from_slice(&END.to_le_bytes());
    header[76..80].copy_from_slice(&fat_sector.to_le_bytes());
    for offset in (80..512).step_by(4) {
        header[offset..offset + 4].copy_from_slice(&FREE.to_le_bytes());
    }
    let mut output = header.to_vec();
    output.extend(directory_bytes);
    output.extend(minifat_bytes);
    output.extend(mini_data);
    output.extend(fat_bytes);
    output
}

fn chain(fat: &mut [u32], start: usize, count: usize) {
    for offset in 0..count {
        fat[start + offset] =
            if offset + 1 == count { END } else { u32::try_from(start + offset + 1).unwrap() };
    }
}

fn lzfu_uncompressed(raw: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&u32::try_from(raw.len() + 12).unwrap().to_le_bytes());
    output.extend_from_slice(&u32::try_from(raw.len()).unwrap().to_le_bytes());
    output.extend_from_slice(&0x414c_454d_u32.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(raw);
    output
}

fn lzfu_compressed_literals(raw: &[u8]) -> Vec<u8> {
    const PRELOAD_LEN: usize = 207;
    let mut payload = Vec::new();
    let mut cursor = 0;
    while raw.len() - cursor >= 8 {
        payload.push(0);
        payload.extend_from_slice(&raw[cursor..cursor + 8]);
        cursor += 8;
    }
    let remaining = raw.len() - cursor;
    payload.push(1 << remaining);
    payload.extend_from_slice(&raw[cursor..]);
    let end = (PRELOAD_LEN + raw.len()) & 0x0fff;
    payload.push(u8::try_from(end >> 4).unwrap());
    payload.push(u8::try_from((end & 0x0f) << 4).unwrap());
    let mut output = Vec::new();
    output.extend_from_slice(&u32::try_from(payload.len() + 12).unwrap().to_le_bytes());
    output.extend_from_slice(&u32::try_from(raw.len()).unwrap().to_le_bytes());
    output.extend_from_slice(&0x7546_5a4c_u32.to_le_bytes());
    output.extend_from_slice(&crc32(&payload).to_le_bytes());
    output.extend_from_slice(&payload);
    output
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    crc
}
