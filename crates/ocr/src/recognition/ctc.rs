//! Strict PP-OCR CTC output validation and deterministic decoding.

use super::preprocess::CropPlan;
use super::{BLANK, CLASSES, RecognitionConfig, RecognizedText, limit, ocr, to_u64};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation, Tensor};

pub(super) fn decode_output(
    outputs: &[Tensor],
    batch: &[(usize, CropPlan)],
    characters: &[String],
    config: &RecognitionConfig,
    context: &ExecutionContext,
    reservation: &mut ResourceReservation,
) -> Result<Vec<RecognizedText>, ConversionError> {
    if outputs.len() != 1 {
        return Err(ocr("recognitionOutputCountMismatch"));
    }
    let output = &outputs[0];
    if output.shape.len() != 3
        || output.shape[0] != batch.len()
        || output.shape[2] != CLASSES
        || output.shape[1] == 0
        || output.shape[1] > config.max_output_timesteps
    {
        return Err(ocr("recognitionOutputShapeMismatch"));
    }
    let expected = batch
        .len()
        .checked_mul(output.shape[1])
        .and_then(|value| value.checked_mul(CLASSES))
        .ok_or_else(|| limit("recognitionOutputElements"))?;
    if output.values.len() != expected || output.values.iter().any(|value| !value.is_finite()) {
        return Err(ocr("invalidRecognitionOutput"));
    }
    if output.values.iter().any(|value| !(0.0..=1.0).contains(value)) {
        return Err(ocr("invalidRecognitionProbability"));
    }
    let decoded_peak = batch
        .len()
        .checked_mul(output.shape[1])
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| limit("recognitionDecodedBytes"))?
        .min(config.max_decoded_bytes);
    reservation.grow(to_u64(decoded_peak)?)?;
    let mut decoded = Vec::new();
    decoded.try_reserve_exact(batch.len()).map_err(|_| limit("recognitionMemory"))?;
    let mut total_bytes = 0_usize;
    for (batch_index, &(source_index, _)) in batch.iter().enumerate() {
        context.checkpoint()?;
        let mut text = String::new();
        let mut probability_sum = 0.0_f64;
        let mut kept = 0_usize;
        let mut previous = usize::MAX;
        for timestep in 0..output.shape[1] {
            if timestep % 32 == 0 {
                context.checkpoint()?;
            }
            let start = (batch_index * output.shape[1] + timestep) * CLASSES;
            let scores = &output.values[start..start + CLASSES];
            let mut best = 0_usize;
            for index in 1..CLASSES {
                if scores[index] > scores[best] {
                    best = index;
                }
            }
            if best != previous && best != BLANK {
                let character =
                    characters.get(best - 1).ok_or_else(|| ocr("invalidCharacterIndex"))?;
                total_bytes = total_bytes
                    .checked_add(character.len())
                    .ok_or_else(|| limit("recognitionDecodedBytes"))?;
                if total_bytes > config.max_decoded_bytes {
                    return Err(limit("recognitionDecodedBytes"));
                }
                text.try_reserve_exact(character.len()).map_err(|_| limit("recognitionMemory"))?;
                text.push_str(character);
                probability_sum += f64::from(scores[best]);
                kept += 1;
            }
            previous = best;
        }
        let confidence = if kept == 0 { 0.0 } else { (probability_sum / kept as f64) as f32 };
        decoded.push(RecognizedText { source_index, text, confidence });
    }
    Ok(decoded)
}
