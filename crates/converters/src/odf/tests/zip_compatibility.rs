use super::support::{
    NS, convert, package, package_with_central_extra, package_with_optional_directory,
};
use crate::odf::raw_zip::{read_u16, read_u32};
use into_markdown_core::{ConversionError, InputFormat, ResourceLimits};
use std::io::Cursor;

fn document() -> String {
    format!(
        "<office:document-content {NS}><office:body><office:text><text:p>kept text</text:p></office:text></office:body></office:document-content>"
    )
}

// Make a seek-written fixture equivalent to a streaming producer. The ZIP crate still
// supplies the payload, CRC and central directory; only the descriptor is inserted here.
fn with_descriptor(bytes: Vec<u8>, signed: bool) -> (Vec<u8>, usize) {
    named_descriptor(bytes, signed, "content.xml")
}

pub(super) fn named_descriptor(mut bytes: Vec<u8>, signed: bool, name: &str) -> (Vec<u8>, usize) {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
    let entry = zip.by_name(name).unwrap();
    let local = usize::try_from(entry.header_start()).unwrap();
    let central = usize::try_from(entry.central_header_start()).unwrap();
    let end = usize::try_from(entry.data_start() + entry.compressed_size()).unwrap();
    let mut descriptor = Vec::new();
    if signed {
        descriptor.extend_from_slice(&0x0807_4b50_u32.to_le_bytes());
    }
    descriptor.extend_from_slice(&entry.crc32().to_le_bytes());
    descriptor.extend_from_slice(&u32::try_from(entry.compressed_size()).unwrap().to_le_bytes());
    descriptor.extend_from_slice(&u32::try_from(entry.size()).unwrap().to_le_bytes());
    drop(entry);
    drop(zip);
    let flags = read_u16(&bytes, local + 6).unwrap() | 8;
    bytes[local + 6..local + 8].copy_from_slice(&flags.to_le_bytes());
    bytes[local + 14..local + 26].fill(0);
    bytes[central + 8..central + 10].copy_from_slice(&flags.to_le_bytes());
    let delta = u32::try_from(descriptor.len()).unwrap();
    bytes.splice(end..end, descriptor);
    let eocd = bytes.len() - 22;
    let start = read_u32(&bytes, eocd + 16).unwrap() + delta;
    bytes[eocd + 16..eocd + 20].copy_from_slice(&start.to_le_bytes());
    let mut central = usize::try_from(start).unwrap();
    while central < eocd {
        let offset = read_u32(&bytes, central + 42).unwrap();
        if usize::try_from(offset).unwrap() >= end {
            bytes[central + 42..central + 46].copy_from_slice(&(offset + delta).to_le_bytes());
        }
        central += 46
            + usize::from(read_u16(&bytes, central + 28).unwrap())
            + usize::from(read_u16(&bytes, central + 30).unwrap())
            + usize::from(read_u16(&bytes, central + 32).unwrap());
    }
    (bytes, end)
}

#[test]
fn ordinary_members_accept_both_descriptor_encodings_and_keep_crc_binding() {
    for signed in [false, true] {
        let (bytes, descriptor) =
            with_descriptor(package(InputFormat::Odt, &document(), &[]), signed);
        assert!(convert(&bytes, InputFormat::Odt, ResourceLimits::default()).is_ok());
        for field in [0, 4, 8] {
            let mut corrupt = bytes.clone();
            corrupt[descriptor + usize::from(signed) * 4 + field] ^= 1;
            assert!(matches!(
                convert(&corrupt, InputFormat::Odt, ResourceLimits::default()),
                Err(ConversionError::Malformed { .. })
            ));
        }
        let mut corrupt = bytes;
        // Changing the actual payload is still caught by the ZIP reader's CRC/decompressor.
        let mut zip = zip::ZipArchive::new(Cursor::new(corrupt.as_slice())).unwrap();
        let offset = usize::try_from(zip.by_name("content.xml").unwrap().data_start()).unwrap();
        drop(zip);
        corrupt[offset] ^= 0xff;
        assert!(matches!(
            convert(&corrupt, InputFormat::Odt, ResourceLimits::default()),
            Err(ConversionError::Malformed { .. })
        ));
    }
}

#[test]
fn ordinary_members_accept_ntfs_and_unknown_metadata_extras() {
    let mut ntfs = vec![0; 4];
    ntfs.extend_from_slice(&1_u16.to_le_bytes());
    ntfs.extend_from_slice(&24_u16.to_le_bytes());
    ntfs.extend_from_slice(&[0; 24]);
    for (id, payload) in [(0x000a, ntfs), (0xcafe, vec![1, 2, 3])] {
        // The writer reserves NTFS for a feature we do not enable. Write an inert same-size
        // field, then set its wire ID; no dependency/features are needed for this read test.
        let mut bytes =
            package_with_central_extra(&document(), false, 0xcafe, payload.into_boxed_slice());
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.as_slice())).unwrap();
        let central =
            usize::try_from(zip.by_name("content.xml").unwrap().central_header_start()).unwrap();
        drop(zip);
        let mut extra = central + 46 + usize::from(read_u16(&bytes, central + 28).unwrap());
        while read_u16(&bytes, extra).unwrap() != 0xcafe {
            extra += 4 + usize::from(read_u16(&bytes, extra + 2).unwrap());
        }
        bytes[extra..extra + 2].copy_from_slice(&u16::to_le_bytes(id));
        let (bytes, _) = with_descriptor(bytes, true);
        assert!(convert(&bytes, InputFormat::Odt, ResourceLimits::default()).is_ok());
    }
}

#[test]
fn metadata_parts_are_not_images_but_image_references_still_require_image_media() {
    let extra = [
        ("manifest.rdf", "application/rdf+xml", b"<rdf/>".as_slice()),
        ("layout-cache", "application/binary", b"cache".as_slice()),
    ];
    let bytes = package(InputFormat::Odt, &document(), &extra);
    assert!(convert(&bytes, InputFormat::Odt, ResourceLimits::default()).is_ok());
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><draw:frame><draw:image xlink:type='simple' xlink:href='layout-cache'/></draw:frame></office:text></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odt, &content, &extra);
    assert!(matches!(
        convert(&bytes, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn manifest_directories_do_not_require_physical_zip_records() {
    for physical in [false, true] {
        let bytes = package_with_optional_directory(
            &document(),
            "application/vnd.sun.xml.ui.configuration",
            physical,
        );
        assert!(convert(&bytes, InputFormat::Odt, ResourceLimits::default()).is_ok());
    }
}
