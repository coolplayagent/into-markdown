use super::candidate_index::{CandidateBuffer, CandidateGrouping};
use into_markdown_core::{
    AssetMode, ConversionError, ConversionOptions, ConverterOutput, EnrichmentPlan,
    ExecutionContext, ResourceReservation,
};

pub(super) fn attach_optional_memory(
    output: &mut ConverterOutput,
    context: &ExecutionContext,
    memory: Option<ResourceReservation>,
) -> Result<(), ConversionError> {
    if let Some(memory) = memory {
        output.attach_memory_reservation(context, memory)?;
    }
    Ok(())
}

pub(super) fn bounded_dynamic_plan(
    single_image_peak: u64,
    context: &ExecutionContext,
) -> Result<EnrichmentPlan, ConversionError> {
    let available = context.available_memory_bytes();
    if single_image_peak > available {
        return Err(ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: format!(
                "embedded OCR requires a {single_image_peak}-byte single-image peak; {available} bytes remain"
            ),
        });
    }
    // The finite request envelope lets sparse OCR results grow by their actual
    // size without multiplying every provider maximum across the document.
    Ok(EnrichmentPlan::Reserve(available))
}

pub(super) fn discard_group_payloads(
    output: &mut ConverterOutput,
    candidates: &CandidateBuffer,
    grouping: &CandidateGrouping,
    group_index: usize,
    options: &ConversionOptions,
) {
    if options.output.asset_mode != AssetMode::Omit {
        return;
    }
    for (candidate_index, candidate) in candidates.iter().enumerate() {
        if grouping.membership[candidate_index] == group_index {
            output.assets[candidate.asset_index].bytes = Vec::new();
        }
    }
}
