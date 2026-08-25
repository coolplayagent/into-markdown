use futures::executor::block_on;
use into_markdown_converters::LegacyOfficeConverter;
use into_markdown_core::{
    ConversionOptions, Converter, ExecutionContext, ExecutionOptions, FormatCandidate, InputFormat,
    ResolvedInput, Services, SourceMetadata,
};
use std::sync::Arc;

pub fn fuzz(bytes: &[u8], format: InputFormat) {
    let converter = LegacyOfficeConverter;
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let input = ResolvedInput {
        bytes: Arc::from(bytes),
        metadata: SourceMetadata {
            name: None,
            media_type: None,
            uri: None,
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        },
    };
    let candidate = FormatCandidate::explicit(format);
    let Ok(plan) = converter.planned_output_bytes(&input, &candidate, &options, &context) else {
        return;
    };
    let Ok(mut reservation) = context.reserve_memory(plan) else { return };
    let Ok(credit) = context.with_memory_credit(&mut reservation) else { return };
    let _ = block_on(converter.convert(
        &input,
        &candidate,
        &options,
        &Services::default(),
        &credit,
    ));
}
