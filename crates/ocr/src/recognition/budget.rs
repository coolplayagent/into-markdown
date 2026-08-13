//! Recognition configuration and request-allocation accounting.

use super::{
    MAX_CROP_PIXELS, MAX_DECODED_BYTES, MAX_OUTPUT_TIMESTEPS, MAX_REGIONS, MAX_TENSOR_ELEMENTS,
    RecognitionConfig, limit, ocr,
};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation, Tensor};

pub(super) fn validate_config(config: &RecognitionConfig) -> Result<(), ConversionError> {
    if config.max_regions == 0
        || config.max_regions > MAX_REGIONS
        || config.max_batch_size == 0
        || config.max_batch_size > 8
        || config.max_crop_pixels == 0
        || config.max_crop_pixels > MAX_CROP_PIXELS
        || config.max_tensor_elements == 0
        || config.max_tensor_elements > MAX_TENSOR_ELEMENTS
        || config.max_output_timesteps == 0
        || config.max_output_timesteps > MAX_OUTPUT_TIMESTEPS
        || config.max_decoded_bytes == 0
        || config.max_decoded_bytes > MAX_DECODED_BYTES
    {
        return Err(ocr("invalidRecognitionConfig"));
    }
    Ok(())
}

pub(super) fn validate_language_hint(hint: Option<&str>) -> Result<Option<&str>, ConversionError> {
    match hint {
        None => Ok(None),
        Some("zh" | "zh-Hans" | "zh-Hant" | "en") => Ok(hint),
        Some(_) => Err(ocr("unsupportedRecognitionLanguage")),
    }
}

pub(super) fn reserve_vec<T>(
    capacity: usize,
    context: &ExecutionContext,
) -> Result<ResourceReservation, ConversionError> {
    let bytes =
        capacity.checked_mul(std::mem::size_of::<T>()).ok_or_else(|| limit("recognitionMemory"))?;
    context.reserve_memory(to_u64(bytes)?)
}

pub(super) fn reserve_tensors(
    tensors: &Vec<Tensor>,
    context: &ExecutionContext,
) -> Result<ResourceReservation, ConversionError> {
    let mut bytes = tensors
        .capacity()
        .checked_mul(std::mem::size_of::<Tensor>())
        .ok_or_else(|| limit("recognitionOutputMemory"))?;
    for tensor in tensors {
        bytes = bytes
            .checked_add(
                tensor
                    .shape
                    .capacity()
                    .checked_mul(std::mem::size_of::<usize>())
                    .ok_or_else(|| limit("recognitionOutputMemory"))?,
            )
            .and_then(|value| {
                tensor
                    .values
                    .capacity()
                    .checked_mul(std::mem::size_of::<f32>())
                    .and_then(|tensor_bytes| value.checked_add(tensor_bytes))
            })
            .ok_or_else(|| limit("recognitionOutputMemory"))?;
    }
    context.reserve_memory(to_u64(bytes)?)
}

pub(super) fn to_u64(value: usize) -> Result<u64, ConversionError> {
    u64::try_from(value).map_err(|_| limit("recognitionMemory"))
}
