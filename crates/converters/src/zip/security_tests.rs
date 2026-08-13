use super::archive::{Archive, constructor_calls, reset_constructor_calls};
use super::budget::ArchiveBudget;
use super::merge::rewrite_nodes;
use into_markdown_core::{
    AssetId, Block, BlockNode, ConversionError, ConversionOptions, ErrorCode, ExecutionContext,
    ExecutionOptions, Inline, NodeId, Provenance, ProvenanceKind, ResourceLimits, SourceLocator,
};
use std::collections::BTreeMap;
use std::io::{Cursor, Write as _};
use zip::write::SimpleFileOptions;

fn stored(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    for (name, bytes) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn open(bytes: &[u8], options: &ConversionOptions) -> Result<(), ConversionError> {
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut budget = ArchiveBudget::new(options, &context);
    Archive::open(bytes, 1, &mut budget).map(|_| ())
}

fn central_offsets(bytes: &[u8]) -> Vec<usize> {
    bytes
        .windows(4)
        .enumerate()
        .filter_map(|(offset, value)| (value == b"PK\x01\x02").then_some(offset))
        .collect()
}

fn local_offset(bytes: &[u8], central: usize) -> usize {
    usize::try_from(u32::from_le_bytes(bytes[central + 42..central + 46].try_into().unwrap()))
        .unwrap()
}

#[test]
fn encrypted_symlink_and_special_members_are_rejected() {
    let mut encrypted = stored(&[("file.txt", b"secret")]);
    let central = central_offsets(&encrypted)[0];
    let local = local_offset(&encrypted, central);
    encrypted[local + 6..local + 8].copy_from_slice(&1_u16.to_le_bytes());
    encrypted[central + 8..central + 10].copy_from_slice(&1_u16.to_le_bytes());
    assert_eq!(
        open(&encrypted, &ConversionOptions::default()).unwrap_err().code(),
        ErrorCode::Encrypted
    );

    for mode in [0o120_777_u32, 0o010_644_u32] {
        let mut special = stored(&[("node", b"target")]);
        let central = central_offsets(&special)[0];
        special[central + 38..central + 42].copy_from_slice(&(mode << 16).to_le_bytes());
        assert_eq!(
            open(&special, &ConversionOptions::default()).unwrap_err().code(),
            ErrorCode::Malformed
        );
    }
}

#[test]
fn local_central_confusion_and_physical_overlap_are_rejected() {
    let mut confused = stored(&[("name.txt", b"data")]);
    let central = central_offsets(&confused)[0];
    let local = local_offset(&confused, central);
    confused[local + 30] ^= 1;
    assert_eq!(
        open(&confused, &ConversionOptions::default()).unwrap_err().code(),
        ErrorCode::Malformed
    );

    let mut overlapping = stored(&[("a.txt", b"aaaa"), ("b.txt", b"bbbb")]);
    let central = central_offsets(&overlapping)[0];
    let local = local_offset(&overlapping, central);
    let expanded = u32::from_le_bytes(overlapping[central + 24..central + 28].try_into().unwrap());
    let forged_compressed = expanded + 8;
    overlapping[local + 18..local + 22].copy_from_slice(&forged_compressed.to_le_bytes());
    overlapping[central + 20..central + 24].copy_from_slice(&forged_compressed.to_le_bytes());
    assert_eq!(
        open(&overlapping, &ConversionOptions::default()).unwrap_err().code(),
        ErrorCode::Malformed
    );
}

#[test]
fn entry_tree_and_single_member_limits_are_preflighted() {
    let two = stored(&[("a.txt", b"a"), ("b.txt", b"b")]);
    let mut entry_options = ConversionOptions::default();
    entry_options.limits.max_archive_entries = 1;
    assert!(matches!(
        open(&two, &entry_options),
        Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
    ));

    let large = stored(&[("large.txt", b"12345")]);
    let mut member_options = ConversionOptions::default();
    member_options.limits.max_archive_entry_bytes = 4;
    assert!(matches!(
        open(&large, &member_options),
        Err(ConversionError::ResourceLimit { limit: "max_archive_entry_bytes", .. })
    ));

    let mut memory_options = ConversionOptions::default();
    memory_options.limits.max_memory_bytes = 255;
    assert!(matches!(
        open(&large, &memory_options),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
}

#[test]
fn duplicate_raw_records_are_rejected_before_the_zip_constructor() {
    let mut duplicate = stored(&[("a.txt", b"FIRST"), ("b.txt", b"SECOND")]);
    let central = central_offsets(&duplicate)[1];
    let local = local_offset(&duplicate, central);
    duplicate[central + 46] = b'a';
    duplicate[local + 30] = b'a';
    reset_constructor_calls();
    assert_eq!(
        open(&duplicate, &ConversionOptions::default()).unwrap_err().code(),
        ErrorCode::Malformed
    );
    assert_eq!(constructor_calls(), 0);
}

#[test]
fn raw_aliases_and_file_prefixes_are_rejected_before_the_zip_constructor() {
    let aliases = stored(&[("A.txt", b"one"), ("a.TXT", b"two")]);
    reset_constructor_calls();
    assert_eq!(
        open(&aliases, &ConversionOptions::default()).unwrap_err().code(),
        ErrorCode::Malformed
    );
    assert_eq!(constructor_calls(), 0);

    let prefixes = stored(&[("root", b"file"), ("root/child.txt", b"child")]);
    reset_constructor_calls();
    assert_eq!(
        open(&prefixes, &ConversionOptions::default()).unwrap_err().code(),
        ErrorCode::Malformed
    );
    assert_eq!(constructor_calls(), 0);
}

#[test]
fn large_central_directory_is_budgeted_before_the_zip_constructor() {
    let large = many_empty_entries(50_000);

    let mut entry_options = ConversionOptions::default();
    entry_options.limits.max_archive_entries = 49_999;
    reset_constructor_calls();
    assert!(matches!(
        open(&large, &entry_options),
        Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
    ));
    assert_eq!(constructor_calls(), 0);

    let mut memory_options = ConversionOptions::default();
    memory_options.limits.max_memory_bytes = 16 * 1024 * 1024;
    reset_constructor_calls();
    assert!(matches!(
        open(&large, &memory_options),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(constructor_calls(), 0);
}

#[test]
fn exact_raw_metadata_budget_allows_a_small_archive_constructor() {
    let small = stored(&[("small.txt", b"small")]);
    let exact = super::raw_central::planned_memory(&small).unwrap();
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = exact;
    reset_constructor_calls();
    open(&small, &options).unwrap();
    assert_eq!(constructor_calls(), 1);

    options.limits.max_memory_bytes = exact - 1;
    reset_constructor_calls();
    assert!(matches!(
        open(&small, &options),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(constructor_calls(), 0);
}

#[test]
fn signed_data_descriptor_is_checked_against_the_central_directory() {
    let valid = descriptor_archive("stream.txt", b"streamed");
    open(&valid, &ConversionOptions::default()).unwrap();

    let mut corrupted = valid;
    let descriptor = corrupted.windows(4).position(|value| value == b"PK\x07\x08").unwrap();
    corrupted[descriptor + 4] ^= 1;
    assert_eq!(
        open(&corrupted, &ConversionOptions::default()).unwrap_err().code(),
        ErrorCode::Malformed
    );
}

#[test]
fn recursive_merge_rewrites_inline_provenance_and_footnote_identity() {
    let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
    let mut memory = context.reserve_memory(0).unwrap();
    let provenance = Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "fixture".into(),
        locator: SourceLocator { part: Some("inner".into()), ..SourceLocator::default() },
        confidence: Some(1.0),
    };
    let node_provenance = Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: "fixture".into(),
        locator: SourceLocator::default(),
        confidence: Some(1.0),
    };
    let mut nodes = vec![
        BlockNode {
            id: NodeId("node".into()),
            block: Block::Paragraph(vec![
                Inline::SourceText {
                    value: "x".into(),
                    marks: vec![],
                    provenance: Box::new(provenance),
                },
                Inline::FootnoteReference("note".into()),
            ]),
            provenance: node_provenance.clone(),
        },
        BlockNode {
            id: NodeId("image".into()),
            block: Block::Image { asset: AssetId("asset".into()), alt: None },
            provenance: node_provenance,
        },
    ];
    let asset_ids = BTreeMap::from([("asset".into(), "zip-7-asset-asset".into())]);
    rewrite_nodes(&mut nodes, "zip-7", "nested.zip/file.pdf", &asset_ids, &mut memory).unwrap();
    let Block::Paragraph(inlines) = &nodes[0].block else { panic!("paragraph expected") };
    let Inline::SourceText { provenance, .. } = &inlines[0] else { panic!("source text expected") };
    assert_eq!(provenance.locator.part.as_deref(), Some("nested.zip/file.pdf/inner"));
    assert!(
        matches!(&inlines[1], Inline::FootnoteReference(label) if label == "zip-7-footnote-note")
    );
    assert_eq!(nodes[0].id.0, "zip-7-node-node");
    assert!(matches!(
        &nodes[1].block,
        Block::Image { asset, .. } if asset.0 == "zip-7-asset-asset"
    ));
}

fn descriptor_archive(name: &str, payload: &[u8]) -> Vec<u8> {
    let crc = crc32(payload);
    let size = u32::try_from(payload.len()).unwrap();
    let name_len = u16::try_from(name.len()).unwrap();
    let mut output = Vec::new();
    le32(&mut output, 0x0403_4b50);
    le16(&mut output, 20);
    le16(&mut output, 8);
    le16(&mut output, 0);
    le16(&mut output, 0);
    le16(&mut output, 0);
    le32(&mut output, 0);
    le32(&mut output, 0);
    le32(&mut output, 0);
    le16(&mut output, name_len);
    le16(&mut output, 0);
    output.extend_from_slice(name.as_bytes());
    output.extend_from_slice(payload);
    le32(&mut output, 0x0807_4b50);
    le32(&mut output, crc);
    le32(&mut output, size);
    le32(&mut output, size);

    let central_start = u32::try_from(output.len()).unwrap();
    le32(&mut output, 0x0201_4b50);
    le16(&mut output, 0x0314);
    le16(&mut output, 20);
    le16(&mut output, 8);
    le16(&mut output, 0);
    le16(&mut output, 0);
    le16(&mut output, 0);
    le32(&mut output, crc);
    le32(&mut output, size);
    le32(&mut output, size);
    le16(&mut output, name_len);
    le16(&mut output, 0);
    le16(&mut output, 0);
    le16(&mut output, 0);
    le16(&mut output, 0);
    le32(&mut output, 0o100_644 << 16);
    le32(&mut output, 0);
    output.extend_from_slice(name.as_bytes());
    let central_size = u32::try_from(output.len()).unwrap() - central_start;

    le32(&mut output, 0x0605_4b50);
    le16(&mut output, 0);
    le16(&mut output, 0);
    le16(&mut output, 1);
    le16(&mut output, 1);
    le32(&mut output, central_size);
    le32(&mut output, central_start);
    le16(&mut output, 0);
    output
}

fn many_empty_entries(count: u16) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    for index in 0..count {
        writer.start_file(format!("f{index:05}.txt"), options).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 { crc >> 1 } else { (crc >> 1) ^ 0xedb8_8320 };
        }
    }
    !crc
}

fn le16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn le32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}
