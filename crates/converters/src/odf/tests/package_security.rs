#![allow(clippy::needless_raw_string_hashes)]

use super::support::{
    NS, add_first_central_comment, allocation_attempts, central_header, context_with, convert,
    make_raw_name_invalid, package, package_with_central_extra, package_with_directory,
    reset_allocation_attempts,
};
use crate::odf::model::MANIFEST_NS;
use crate::odf::package::{Package, REACHABLE_IMAGE_ALLOCATION_ATTEMPTS, media_type_for};
use crate::odf::raw_zip::{
    RAW_LAYOUT_ALLOCATION_ATTEMPTS, conservative_vec_capacity, crc32_ieee, reachable_image_peak,
    validate_raw_zip_name, validate_zip_directory_layout,
};
use image::ImageFormat;
use into_markdown_core::{
    ConversionError, ConversionOptions, ExecutionContext, ExecutionOptions, InputFormat,
    ResourceLimits,
};
use std::io::Cursor;
use std::io::Write;
use zip::write::SimpleFileOptions;

#[test]
fn encrypted_manifest_dtd_traversal_and_decompression_limit_fail_closed() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>x</text:p></office:text></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let mut limits = ResourceLimits { max_decompressed_bytes: 10, ..ResourceLimits::default() };
    assert!(matches!(
        convert(&bytes, InputFormat::Odt, limits.clone()),
        Err(ConversionError::ResourceLimit { limit: "max_decompressed_bytes", .. })
    ));
    limits.max_decompressed_bytes = ResourceLimits::default().max_decompressed_bytes;
    let dtd = package(InputFormat::Odt, "<!DOCTYPE x><x/>", &[]);
    assert!(matches!(
        convert(&dtd, InputFormat::Odt, limits.clone()),
        Err(ConversionError::Malformed { .. })
    ));

    let mimetype = media_type_for(InputFormat::Odt).unwrap();
    let encrypted_manifest = format!(
        "<manifest:manifest xmlns:manifest='{MANIFEST_NS}'><manifest:file-entry manifest:full-path='/' manifest:media-type='{mimetype}'/><manifest:file-entry manifest:full-path='content.xml' manifest:media-type='text/xml'><manifest:encryption-data/></manifest:file-entry></manifest:manifest>"
    );
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        writer
            .start_file(
                "mimetype",
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(mimetype.as_bytes()).unwrap();
        writer.start_file("content.xml", SimpleFileOptions::default()).unwrap();
        writer.write_all(content.as_bytes()).unwrap();
        writer.start_file("META-INF/manifest.xml", SimpleFileOptions::default()).unwrap();
        writer.write_all(encrypted_manifest.as_bytes()).unwrap();
        writer.finish().unwrap();
    }
    assert!(matches!(
        convert(&cursor.into_inner(), InputFormat::Odt, limits.clone()),
        Err(ConversionError::Encrypted)
    ));

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        writer
            .start_file(
                "mimetype",
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(mimetype.as_bytes()).unwrap();
        writer.start_file("../escape", SimpleFileOptions::default()).unwrap();
        writer.write_all(b"x").unwrap();
        writer.finish().unwrap();
    }
    assert!(matches!(
        convert(&cursor.into_inner(), InputFormat::Odt, limits),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn mimetype_local_header_is_bound_before_any_xml_or_other_part() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>x</text:p></office:text></office:body></office:document-content>"
    );
    let valid = package(InputFormat::Odt, &content, &[]);

    let mut descriptor = valid.clone();
    descriptor[6] |= 1 << 3;
    assert!(matches!(
        convert(&descriptor, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "mimetype"
    ));

    let mut local_extra = valid.clone();
    local_extra[28..30].copy_from_slice(&1_u16.to_le_bytes());
    assert!(matches!(
        convert(&local_extra, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "mimetype"
    ));

    let mut central_crc = valid.clone();
    let central = central_header(&central_crc);
    central_crc[central + 16] ^= 0x80;
    assert!(matches!(
        convert(&central_crc, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "mimetype"
    ));

    let mut central_flags = valid;
    let central = central_header(&central_flags);
    central_flags[central + 8] |= 1 << 3;
    assert!(matches!(
        convert(&central_flags, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "mimetype"
    ));

    let mut duplicate_eocd = package(InputFormat::Odt, &content, &[]);
    duplicate_eocd[10..14].copy_from_slice(b"PK\x05\x06");
    assert!(matches!(
        convert(&duplicate_eocd, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let mut trailing = package(InputFormat::Odt, &content, &[]);
    trailing.extend_from_slice(b"PK\x01\x02");
    assert!(matches!(
        convert(&trailing, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let mut preamble = vec![0];
    preamble.extend(package(InputFormat::Odt, &content, &[]));
    assert!(matches!(
        convert(&preamble, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn raw_zip_names_extras_comments_and_index_allocations_fail_closed() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>x</text:p></office:text></office:body></office:document-content>"
    );
    let mut invalid_utf8 = package(InputFormat::Odt, &content, &[]);
    make_raw_name_invalid(&mut invalid_utf8, b"content.xml");
    assert!(matches!(
        convert(&invalid_utf8, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    assert!(validate_raw_zip_name("é.xml".as_bytes(), 0, "test").is_err());
    assert_eq!(validate_raw_zip_name("é.xml".as_bytes(), 1 << 11, "test").unwrap(), "é.xml");

    let mut unicode_payload = vec![1];
    // The safe writer validates the field before the eventual raw filename is known; raw
    // package validation rejects 0x7075 before ZipArchive can interpret it.
    unicode_payload.extend_from_slice(&crc32_ieee(b"").to_le_bytes());
    unicode_payload.extend_from_slice(b"renamed.xml");
    let unicode_extra =
        package_with_central_extra(&content, false, 0x7075, unicode_payload.into_boxed_slice());
    assert!(matches!(
        convert(&unicode_extra, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { .. })
    ));

    let mimetype_extra = package_with_central_extra(&content, true, 0xcafe, Box::from([1_u8]));
    assert!(matches!(
        convert(&mimetype_extra, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "mimetype"
    ));
    let mut mimetype_comment = package(InputFormat::Odt, &content, &[]);
    add_first_central_comment(&mut mimetype_comment);
    assert!(matches!(
        convert(&mimetype_comment, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "mimetype"
    ));

    let normal = package(InputFormat::Odt, &content, &[]);
    let execution = ExecutionContext::new(
        ExecutionOptions::default(),
        ResourceLimits { max_memory_bytes: 1, ..ResourceLimits::default() },
    );
    reset_allocation_attempts(&RAW_LAYOUT_ALLOCATION_ATTEMPTS);
    assert!(matches!(
        validate_zip_directory_layout(
            &normal,
            ResourceLimits::default().max_archive_entries,
            1,
            &execution
        ),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(allocation_attempts(&RAW_LAYOUT_ALLOCATION_ATTEMPTS), 0);

    let mut forged_count = normal;
    let eocd = forged_count.len() - 22;
    forged_count[eocd + 8..eocd + 10].copy_from_slice(&50_000_u16.to_le_bytes());
    forged_count[eocd + 10..eocd + 12].copy_from_slice(&50_000_u16.to_le_bytes());
    reset_allocation_attempts(&RAW_LAYOUT_ALLOCATION_ATTEMPTS);
    assert!(matches!(
        validate_zip_directory_layout(&forged_count, 100, u64::MAX, &execution),
        Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
    ));
    assert_eq!(allocation_attempts(&RAW_LAYOUT_ALLOCATION_ATTEMPTS), 0);
}

#[test]
fn reachable_image_memory_is_authenticated_before_read_and_decoder_creation() {
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(1, 1).write_to(&mut png, ImageFormat::Png).unwrap();
    let content = format!(
        r#"<office:document-content {NS}><office:body><office:text><draw:frame><draw:image xlink:type='simple' xlink:href='Pictures/a.png'/></draw:frame></office:text></office:body></office:document-content>"#
    );
    let bytes =
        package(InputFormat::Odt, &content, &[("Pictures/a.png", "image/png", png.get_ref())]);
    let options = ConversionOptions::default();
    let inspection = context_with(ResourceLimits::default());
    let base = Package::open(
        &bytes,
        InputFormat::Odt,
        &options,
        &inspection,
        inspection.available_memory_bytes(),
    )
    .unwrap()
    .logical_peak;
    let declared = u64::try_from(png.get_ref().len()).unwrap();
    let exact =
        reachable_image_peak(base, conservative_vec_capacity(declared).unwrap(), declared).unwrap();

    reset_allocation_attempts(&REACHABLE_IMAGE_ALLOCATION_ATTEMPTS);
    let output = convert(
        &bytes,
        InputFormat::Odt,
        ResourceLimits { max_memory_bytes: exact, ..ResourceLimits::default() },
    )
    .unwrap();
    assert_eq!(output.assets.len(), 1);
    assert_eq!(allocation_attempts(&REACHABLE_IMAGE_ALLOCATION_ATTEMPTS), 1);

    reset_allocation_attempts(&REACHABLE_IMAGE_ALLOCATION_ATTEMPTS);
    assert!(matches!(
        convert(
            &bytes,
            InputFormat::Odt,
            ResourceLimits { max_memory_bytes: exact - 1, ..ResourceLimits::default() }
        ),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(allocation_attempts(&REACHABLE_IMAGE_ALLOCATION_ATTEMPTS), 0);
}

#[test]
fn unreferenced_image_size_does_not_enter_working_set_but_crc_is_stream_checked() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>x</text:p></office:text></office:body></office:document-content>"
    );
    let small_payload = vec![0x5a; 1];
    let large_payload = vec![0x5a; 2 * 1024 * 1024];
    let small = package(
        InputFormat::Odt,
        &content,
        &[("Pictures/unused.png", "image/png", &small_payload)],
    );
    let large = package(
        InputFormat::Odt,
        &content,
        &[("Pictures/unused.png", "image/png", &large_payload)],
    );
    let options = ConversionOptions::default();
    let small_context = context_with(ResourceLimits::default());
    let large_context = context_with(ResourceLimits::default());
    let small_peak = Package::open(
        &small,
        InputFormat::Odt,
        &options,
        &small_context,
        small_context.available_memory_bytes(),
    )
    .unwrap()
    .logical_peak;
    let large_peak = Package::open(
        &large,
        InputFormat::Odt,
        &options,
        &large_context,
        large_context.available_memory_bytes(),
    )
    .unwrap()
    .logical_peak;
    assert_eq!(small_peak, large_peak);
    assert!(convert(&large, InputFormat::Odt, ResourceLimits::default()).is_ok());
    let exact_limits = ResourceLimits { max_memory_bytes: small_peak, ..ResourceLimits::default() };
    assert!(convert(&small, InputFormat::Odt, exact_limits.clone()).is_ok());
    assert!(convert(&large, InputFormat::Odt, exact_limits.clone()).is_ok());
    let low_limits = ResourceLimits { max_memory_bytes: small_peak - 1, ..exact_limits };
    assert!(matches!(
        convert(&large, InputFormat::Odt, low_limits),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));

    let mut corrupt = large;
    let mut archive = zip::ZipArchive::new(Cursor::new(corrupt.as_slice())).unwrap();
    let image_index = (0..archive.len())
        .find(|index| archive.by_index_raw(*index).unwrap().name() == "Pictures/unused.png")
        .unwrap();
    let data_start =
        usize::try_from(archive.by_index_raw(image_index).unwrap().data_start()).unwrap();
    drop(archive);
    corrupt[data_start] ^= 0xff;
    assert!(matches!(
        convert(&corrupt, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "Pictures/unused.png"
    ));
}

#[test]
fn directory_entries_are_manifest_bound_empty_and_inert() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>x</text:p></office:text></office:body></office:document-content>"
    );
    let valid = package_with_directory(&content, "");
    assert!(convert(&valid, InputFormat::Odt, ResourceLimits::default()).is_ok());
    let typed = package_with_directory(&content, "image/png");
    assert!(matches!(
        convert(&typed, InputFormat::Odt, ResourceLimits::default()),
        Err(ConversionError::Malformed { part: Some(part), .. }) if part == "Pictures/"
    ));
    for media_type in ["", "application/xml", "application/vnd.sun.star.oleobject"] {
        let object = package(
            InputFormat::Odt,
            &content,
            &[("Object 1/content.xml", media_type, b"<object/>")],
        );
        assert!(matches!(
            convert(&object, InputFormat::Odt, ResourceLimits::default()),
            Err(ConversionError::Malformed { .. })
        ));
    }
}
