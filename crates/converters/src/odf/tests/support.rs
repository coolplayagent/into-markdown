#![allow(clippy::format_collect)]

use crate::odf::convert_odf;
use crate::odf::model::MANIFEST_NS;
use crate::odf::package::media_type_for;
use crate::odf::raw_zip::{read_u16, read_u32};
use into_markdown_core::{
    ConversionError, ConversionOptions, ConverterOutput, ExecutionContext, ExecutionOptions,
    InputFormat, ResourceLimits,
};
use std::cell::Cell as CounterCell;
use std::io::Cursor;
use std::io::Write;
use zip::write::{ExtendedFileOptions, FileOptions, SimpleFileOptions};

pub(super) const NS: &str = concat!(
    "xmlns:office='urn:oasis:names:tc:opendocument:xmlns:office:1.0' ",
    "xmlns:text='urn:oasis:names:tc:opendocument:xmlns:text:1.0' ",
    "xmlns:table='urn:oasis:names:tc:opendocument:xmlns:table:1.0' ",
    "xmlns:draw='urn:oasis:names:tc:opendocument:xmlns:drawing:1.0' ",
    "xmlns:presentation='urn:oasis:names:tc:opendocument:xmlns:presentation:1.0' ",
    "xmlns:style='urn:oasis:names:tc:opendocument:xmlns:style:1.0' ",
    "xmlns:dc='http://purl.org/dc/elements/1.1/' ",
    "xmlns:fo='urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0' ",
    "xmlns:xlink='http://www.w3.org/1999/xlink' ",
    "xmlns:svg='urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0'"
);

pub(super) fn context_with(mut limits: ResourceLimits) -> ExecutionContext {
    limits.max_memory_bytes = limits.max_memory_bytes.max(64 * 1024);
    ExecutionContext::new(ExecutionOptions::default(), limits)
}

fn manifest(mimetype: &str, extra: &str) -> String {
    format!(
        "<manifest:manifest xmlns:manifest='{MANIFEST_NS}' manifest:version='1.3'><manifest:file-entry manifest:full-path='/' manifest:media-type='{mimetype}'/><manifest:file-entry manifest:full-path='content.xml' manifest:media-type='text/xml'/>{extra}</manifest:manifest>"
    )
}

pub(super) fn package(
    format: InputFormat,
    content: &str,
    extra: &[(&str, &str, &[u8])],
) -> Vec<u8> {
    let mimetype = media_type_for(format).unwrap();
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
        let declarations = extra.iter().map(|(name, media, _)| format!("<manifest:file-entry manifest:full-path='{name}' manifest:media-type='{media}'/>")).collect::<String>();
        writer.start_file("META-INF/manifest.xml", SimpleFileOptions::default()).unwrap();
        writer.write_all(manifest(mimetype, &declarations).as_bytes()).unwrap();
        for (name, _, bytes) in extra {
            writer.start_file(*name, SimpleFileOptions::default()).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

pub(super) fn central_header(bytes: &[u8]) -> usize {
    bytes.windows(4).position(|window| window == b"PK\x01\x02").expect("central directory")
}

pub(super) fn allocation_attempts(
    counter: &'static std::thread::LocalKey<CounterCell<usize>>,
) -> usize {
    counter.with(CounterCell::get)
}

pub(super) fn reset_allocation_attempts(
    counter: &'static std::thread::LocalKey<CounterCell<usize>>,
) {
    counter.with(|value| value.set(0));
}

pub(super) fn add_first_central_comment(bytes: &mut Vec<u8>) {
    let central = central_header(bytes);
    let name_len = usize::from(read_u16(bytes, central + 28).unwrap());
    let extra_len = usize::from(read_u16(bytes, central + 30).unwrap());
    let comment_start = central + 46 + name_len + extra_len;
    bytes.splice(comment_start..comment_start, *b"x");
    bytes[central + 32..central + 34].copy_from_slice(&1_u16.to_le_bytes());
    let eocd = bytes.len() - 22;
    let old_size = read_u32(bytes, eocd + 12).unwrap();
    bytes[eocd + 12..eocd + 16].copy_from_slice(&(old_size + 1).to_le_bytes());
}

pub(super) fn make_raw_name_invalid(bytes: &mut [u8], target: &[u8]) {
    let eocd = bytes.len() - 22;
    let central_start = usize::try_from(read_u32(bytes, eocd + 16).unwrap()).unwrap();
    let mut local = 0_usize;
    while local < central_start {
        let name_len = usize::from(read_u16(bytes, local + 26).unwrap());
        let extra_len = usize::from(read_u16(bytes, local + 28).unwrap());
        let compressed = usize::try_from(read_u32(bytes, local + 18).unwrap()).unwrap();
        let name_start = local + 30;
        if bytes.get(name_start..name_start + name_len) == Some(target) {
            bytes[name_start] = 0xff;
            let flags = read_u16(bytes, local + 6).unwrap() | (1 << 11);
            bytes[local + 6..local + 8].copy_from_slice(&flags.to_le_bytes());
        }
        local = name_start + name_len + extra_len + compressed;
    }
    let mut central = central_start;
    while central < eocd {
        let name_len = usize::from(read_u16(bytes, central + 28).unwrap());
        let extra_len = usize::from(read_u16(bytes, central + 30).unwrap());
        let comment_len = usize::from(read_u16(bytes, central + 32).unwrap());
        let name_start = central + 46;
        if bytes.get(name_start..name_start + name_len) == Some(target) {
            bytes[name_start] = 0xff;
            let flags = read_u16(bytes, central + 8).unwrap() | (1 << 11);
            bytes[central + 8..central + 10].copy_from_slice(&flags.to_le_bytes());
        }
        central = name_start + name_len + extra_len + comment_len;
    }
}

pub(super) fn package_with_directory(content: &str, media_type: &str) -> Vec<u8> {
    package_with_optional_directory(content, media_type, true)
}

pub(super) fn package_with_optional_directory(
    content: &str,
    media_type: &str,
    physical: bool,
) -> Vec<u8> {
    let mimetype = media_type_for(InputFormat::Odt).unwrap();
    let declaration = format!(
        "<manifest:file-entry manifest:full-path='Pictures/' manifest:media-type='{media_type}'/>"
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
        writer.write_all(manifest(mimetype, &declaration).as_bytes()).unwrap();
        if physical {
            writer.add_directory("Pictures/", SimpleFileOptions::default()).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

pub(super) fn package_with_central_extra(
    content: &str,
    extra_on_mimetype: bool,
    header_id: u16,
    payload: Box<[u8]>,
) -> Vec<u8> {
    let mimetype = media_type_for(InputFormat::Odt).unwrap();
    let mut payload = Some(payload);
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        if extra_on_mimetype {
            let mut options = FileOptions::<ExtendedFileOptions>::default()
                .compression_method(zip::CompressionMethod::Stored);
            options.add_extra_data(header_id, payload.take().unwrap(), true).unwrap();
            writer.start_file("mimetype", options).unwrap();
        } else {
            writer
                .start_file(
                    "mimetype",
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
        }
        writer.write_all(mimetype.as_bytes()).unwrap();
        if extra_on_mimetype {
            writer.start_file("content.xml", SimpleFileOptions::default()).unwrap();
        } else {
            let mut options = FileOptions::<ExtendedFileOptions>::default();
            options.add_extra_data(0x5455, Box::from([1_u8, 0, 0, 0, 0]), false).unwrap();
            options.add_extra_data(header_id, payload.take().unwrap(), true).unwrap();
            writer.start_file("content.xml", options).unwrap();
        }
        writer.write_all(content.as_bytes()).unwrap();
        writer.start_file("META-INF/manifest.xml", SimpleFileOptions::default()).unwrap();
        writer.write_all(manifest(mimetype, "").as_bytes()).unwrap();
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

pub(super) fn convert(
    bytes: &[u8],
    format: InputFormat,
    limits: ResourceLimits,
) -> Result<ConverterOutput, ConversionError> {
    convert_odf(
        bytes,
        format,
        &ConversionOptions { limits: limits.clone(), ..ConversionOptions::default() },
        &context_with(limits),
    )
}
