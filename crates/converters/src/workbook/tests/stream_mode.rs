use super::super::WorkbookConverter;
use into_markdown_core::{
    ConversionOptions, Converter, ConverterStream, ConverterStreamMode, ExecutionContext,
    ExecutionOptions, FormatCandidate, InputFormat, ResolvedInput, SourceMetadata,
    StreamConsumerKind,
};
use std::sync::Arc;

fn input(bytes: usize, name: Option<&str>) -> ResolvedInput {
    let bytes: Arc<[u8]> = vec![b'x'; bytes].into();
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
fn collecting_xlsx_is_native_for_every_payload_size_and_name() {
    let converter = WorkbookConverter;
    let candidate = FormatCandidate::new(InputFormat::Xlsx, 1.0, "test");
    for (bytes, name) in [
        (0, None),
        (4 * 1024, Some("small.xlsx")),
        (256 * 1024, Some("ordinary.xlsx")),
        (2 * 1024 * 1024, Some("unrelated.bin")),
    ] {
        assert_eq!(
            converter.stream_mode_for(
                &input(bytes, name),
                &candidate,
                &ConversionOptions::default(),
                StreamConsumerKind::Collecting,
            ),
            ConverterStreamMode::Native
        );
    }
}

#[test]
fn output_plan_uses_available_credit_without_parsing_input() {
    let converter = WorkbookConverter;
    let candidate = FormatCandidate::new(InputFormat::Xlsx, 1.0, "test");
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert_eq!(
        converter
            .planned_output_bytes(
                &input(3, Some("not-a-package.xlsx")),
                &candidate,
                &options,
                &context,
            )
            .unwrap(),
        context.available_memory_bytes()
    );
}
