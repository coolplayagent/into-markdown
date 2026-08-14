use super::*;
use into_markdown_core::{ExecutionOptions, ResourceLimits};
use std::io::{Cursor, Write as _};

fn context() -> ExecutionContext {
    ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
}

#[test]
fn exact_packages_accept_only_the_declared_family() {
    for format in [NormalizedFormat::Docx, NormalizedFormat::Pptx, NormalizedFormat::Xlsx] {
        let bytes = fixture_package(format).unwrap();
        audit(&bytes, format, &context()).unwrap();
        let wrong = if format == NormalizedFormat::Docx {
            NormalizedFormat::Pptx
        } else {
            NormalizedFormat::Docx
        };
        assert!(matches!(audit(&bytes, wrong, &context()), Err(ConversionError::Malformed { .. })));
    }
}

#[test]
fn pk_garbage_trailing_bytes_and_crc_mutation_are_rejected() {
    assert!(audit(b"PKgarbage", NormalizedFormat::Docx, &context()).is_err());
    let mut trailing = fixture_package(NormalizedFormat::Docx).unwrap();
    trailing.push(0);
    assert!(audit(&trailing, NormalizedFormat::Docx, &context()).is_err());
    let mut damaged = fixture_package(NormalizedFormat::Docx).unwrap();
    let data_start = LOCAL_BYTES
        + usize::from(le16(&damaged, 26).unwrap())
        + usize::from(le16(&damaged, 28).unwrap());
    let layout = locate(&damaged).unwrap();
    let compressed = usize::try_from(le32(&damaged, layout.central_start + 20).unwrap()).unwrap();
    damaged[data_start + compressed / 2] ^= 1;
    assert!(audit(&damaged, NormalizedFormat::Docx, &context()).is_err());
}

#[test]
fn encrypted_paths_duplicates_overlap_and_root_confusion_are_rejected() {
    let original = fixture_package(NormalizedFormat::Docx).unwrap();

    let mut encrypted = original.clone();
    let layout = locate(&encrypted).unwrap();
    let local_flags = le16(&encrypted, 6).unwrap() | 1;
    encrypted[6..8].copy_from_slice(&local_flags.to_le_bytes());
    let central_flags = le16(&encrypted, layout.central_start + 8).unwrap() | 1;
    encrypted[layout.central_start + 8..layout.central_start + 10]
        .copy_from_slice(&central_flags.to_le_bytes());
    assert!(matches!(
        audit(&encrypted, NormalizedFormat::Docx, &context()),
        Err(ConversionError::Encrypted)
    ));

    let mut unsafe_path = original.clone();
    for index in find_all(&unsafe_path, b"[Content_Types].xml") {
        unsafe_path[index..index + 3].copy_from_slice(b"../");
    }
    assert!(audit(&unsafe_path, NormalizedFormat::Docx, &context()).is_err());

    let mut duplicate =
        rewrite(&original, None, &[("word/alpha.xml", b"a"), ("word/bravo.xml", b"b")]);
    for index in find_all(&duplicate, b"word/bravo.xml") {
        duplicate[index..index + b"word/alpha.xml".len()].copy_from_slice(b"word/alpha.xml");
    }
    assert!(audit(&duplicate, NormalizedFormat::Docx, &context()).is_err());

    let mut overlap = original.clone();
    let layout = locate(&overlap).unwrap();
    let first_name = usize::from(le16(&overlap, layout.central_start + 28).unwrap());
    let first_extra = usize::from(le16(&overlap, layout.central_start + 30).unwrap());
    let first_comment = usize::from(le16(&overlap, layout.central_start + 32).unwrap());
    let second = layout.central_start + CENTRAL_BYTES + first_name + first_extra + first_comment;
    overlap[second + 42..second + 46].copy_from_slice(&0_u32.to_le_bytes());
    assert!(audit(&overlap, NormalizedFormat::Docx, &context()).is_err());

    let relationships = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;
    let wrong_root = rewrite(&original, Some(("_rels/.rels", relationships)), &[]);
    assert!(audit(&wrong_root, NormalizedFormat::Docx, &context()).is_err());
}

fn rewrite(
    original: &[u8],
    replacement: Option<(&str, &[u8])>,
    extras: &[(&str, &[u8])],
) -> Vec<u8> {
    let mut source = zip::ZipArchive::new(Cursor::new(original)).unwrap();
    let mut entries = Vec::new();
    for index in 0..source.len() {
        let mut file = source.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        if replacement.is_some_and(|(target, _)| target == name) {
            bytes = replacement.unwrap().1.to_vec();
        }
        entries.push((name, bytes));
    }
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for (name, bytes) in &entries {
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    for (name, bytes) in extras {
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn find_all(bytes: &[u8], needle: &[u8]) -> Vec<usize> {
    bytes
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, candidate)| (candidate == needle).then_some(index))
        .collect()
}
