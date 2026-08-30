use super::PresentationConverter;
use into_markdown_core::{
    ConversionOptions, ConverterStream, ConverterStreamMode, FormatCandidate, InputFormat,
    ResolvedInput, SourceMetadata, StreamConsumerKind,
};
use std::sync::Arc;

#[test]
fn pptx_collecting_uses_native_stream_independent_of_name() {
    let converter = PresentationConverter;
    let candidate = FormatCandidate::new(InputFormat::Pptx, 1.0, "test");
    for name in [None, Some("ordinary.pptx".into()), Some("unrelated.bin".into())] {
        let input = ResolvedInput {
            bytes: Arc::from(&b"PK\x03\x04"[..]),
            metadata: SourceMetadata { name, size: 4, ..SourceMetadata::default() },
        };
        assert_eq!(
            converter.stream_mode_for(
                &input,
                &candidate,
                &ConversionOptions::default(),
                StreamConsumerKind::Collecting,
            ),
            ConverterStreamMode::Native
        );
    }
}

#[test]
fn compound_file_wrapper_keeps_encrypted_aggregate_error_path() {
    let converter = PresentationConverter;
    let candidate = FormatCandidate::new(InputFormat::Pptx, 1.0, "test");
    let input = ResolvedInput {
        bytes: Arc::from(&super::COMPOUND_FILE_SIGNATURE[..]),
        metadata: SourceMetadata { size: 8, ..SourceMetadata::default() },
    };
    assert_eq!(
        converter.stream_mode_for(
            &input,
            &candidate,
            &ConversionOptions::default(),
            StreamConsumerKind::Collecting,
        ),
        ConverterStreamMode::AggregateAdapter
    );
}
