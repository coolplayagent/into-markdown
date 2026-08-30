use super::super::WorkbookConverter;
use into_markdown_core::{
    ConversionOptions, ConverterStream, ConverterStreamMode, FormatCandidate, InputFormat,
    ResolvedInput, SourceMetadata, StreamConsumerKind,
};
use std::sync::Arc;
use std::{io::Cursor, io::Write};

fn input(worksheet_bytes: usize, unrelated_bytes: usize, name: Option<&str>) -> ResolvedInput {
    let mut output = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut output);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("xl/worksheets/sheet1.xml", options).unwrap();
        writer.write_all(&vec![b'x'; worksheet_bytes]).unwrap();
        writer.start_file("customXml/unrelated.bin", options).unwrap();
        writer.write_all(&vec![b'p'; unrelated_bytes]).unwrap();
        writer.finish().unwrap();
    }
    let bytes: Arc<[u8]> = output.into_inner().into();
    ResolvedInput {
        bytes: Arc::clone(&bytes),
        metadata: SourceMetadata {
            name: name.map(str::to_owned),
            size: u64::try_from(bytes.len()).unwrap(),
            ..SourceMetadata::default()
        },
    }
}

#[test]
fn collecting_keeps_small_workbooks_on_real_aggregate_fallback() {
    let converter = WorkbookConverter;
    let candidate = FormatCandidate::new(InputFormat::Xlsx, 1.0, "test");
    assert_eq!(
        converter.stream_mode_for(
            &input(4 * 1024, 0, Some("small.xlsx")),
            &candidate,
            &ConversionOptions::default(),
            StreamConsumerKind::Collecting,
        ),
        ConverterStreamMode::AggregateAdapter
    );
}

#[test]
fn collecting_native_selection_depends_on_worksheet_payload_not_name() {
    let converter = WorkbookConverter;
    let candidate = FormatCandidate::new(InputFormat::Xlsx, 1.0, "test");
    for name in [None, Some("ordinary.xlsx"), Some("unrelated.bin")] {
        assert_eq!(
            converter.stream_mode_for(
                &input(256 * 1024, 0, name),
                &candidate,
                &ConversionOptions::default(),
                StreamConsumerKind::Collecting,
            ),
            ConverterStreamMode::Native
        );
    }
}

#[test]
fn unrelated_zip_padding_cannot_select_native_collecting() {
    let converter = WorkbookConverter;
    let candidate = FormatCandidate::new(InputFormat::Xlsx, 1.0, "test");
    assert_eq!(
        converter.stream_mode_for(
            &input(4 * 1024, 2 * 1024 * 1024, Some("padded.xlsx")),
            &candidate,
            &ConversionOptions::default(),
            StreamConsumerKind::Collecting,
        ),
        ConverterStreamMode::AggregateAdapter
    );
}
