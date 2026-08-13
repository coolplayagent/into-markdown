//! Fixed upstream model and character-table authority.

use super::{BLANK, CLASSES, MAX_WIDTH, MODEL_ID, SCALE, limit, ocr};
use into_markdown_core::{ConversionError, ExecutionContext, ResourceReservation};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Authority {
    pub schema_version: u32,
    pub model_id: String,
    pub upstream_repository: String,
    pub upstream_commit: String,
    pub preprocess_reference: String,
    pub postprocess_reference: String,
    pub runtime_archive_url: String,
    pub runtime_archive_size: u64,
    pub runtime_archive_sha256: String,
    pub runtime_model_member: String,
    pub runtime_model_size: u64,
    pub runtime_model_sha256: String,
    pub runtime_config_member: String,
    pub runtime_config_size: u64,
    pub runtime_config_sha256: String,
    pub character_table_url: String,
    pub character_table_size: u64,
    pub character_table_sha256: String,
    pub character_table_entries: usize,
    pub classes: usize,
    pub blank_index: usize,
    pub append_space: bool,
    pub license: String,
    pub ir_version: u64,
    pub opset_domain: String,
    pub opset_version: u64,
    pub input_name: String,
    pub input_dtype: String,
    pub input_shape: [String; 4],
    pub output_name: String,
    pub output_dtype: String,
    pub output_shape: [String; 3],
    pub input_color: String,
    pub crop_reference: String,
    pub perspective_interpolation: String,
    pub perspective_border: String,
    pub vertical_rotation: String,
    pub vertical_ratio_threshold: f64,
    pub resize_interpolation: String,
    pub normalization_scale: f64,
    pub normalization_mean: f32,
    pub normalization_standard_deviation: f32,
    pub maximum_width: usize,
    pub quality_corpus: String,
    pub quality_groups: Vec<QualityGroup>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct QualityGroup {
    pub group: String,
    pub evaluated_characters: usize,
    pub observed_errors: usize,
    pub maximum_cer: f64,
}

pub(crate) fn authority() -> Result<Authority, ConversionError> {
    let value: Authority = serde_json::from_str(include_str!(
        "../../../../models/ppocrv6-tiny-recognizer-authority.json"
    ))
    .map_err(|_| ocr("invalidRecognizerAuthority"))?;
    if value.schema_version != 1
        || value.model_id != MODEL_ID
        || value.upstream_repository != "https://github.com/PaddlePaddle/PaddleOCR"
        || value.upstream_commit != "2661c7c0ef5c613e8f93c6e93b2e052399f0f854"
        || value.preprocess_reference != "tools/infer/predict_rec.py"
        || value.postprocess_reference != "ppocr/postprocess/rec_postprocess.py"
        || value.runtime_archive_url
            != "https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/paddle3.0.0/PP-OCRv6_tiny_rec_onnx_infer.tar"
        || value.runtime_archive_size != 4_526_080
        || value.runtime_archive_sha256
            != "1e13b22717b1edd89d4cde4fda272b6c17d5b505c97c2baea99da1a3a2d54b29"
        || value.runtime_model_member != "PP-OCRv6_tiny_rec_onnx_infer/inference.onnx"
        || value.runtime_model_size != 4_462_639
        || value.runtime_model_sha256
            != "9ef676d6ed3c88256a2d92c640c44f25b0c40947e111b14b8be8f594091563e6"
        || value.runtime_config_member != "PP-OCRv6_tiny_rec_onnx_infer/inference.yml"
        || value.runtime_config_size != 55_571
        || value.runtime_config_sha256
            != "66170210bad538e83fff3c4a3867e547d6bf20b50d64b20347c4b913f3034ea1"
        || value.character_table_url
            != "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/2661c7c0ef5c613e8f93c6e93b2e052399f0f854/ppocr/utils/dict/ppocrv6_tiny_dict.txt"
        || value.character_table_size != 27_156
        || value.character_table_sha256
            != "c5cbe34ef40c29c4df07ed012bf96569cb69a2d2a01a07027e9f13cb832bd9cd"
        || value.character_table_entries != 6904
        || value.classes != CLASSES
        || value.blank_index != BLANK
        || !value.append_space
        || value.license != "Apache-2.0"
        || value.ir_version != 6
        || !value.opset_domain.is_empty()
        || value.opset_version != 11
        || value.input_name != "x"
        || value.input_dtype != "float32"
        || value.input_shape != ["N", "3", "48", "W"]
        || value.output_name != "fetch_name_0"
        || value.output_dtype != "float32"
        || value.output_shape != ["N", "T", "6906"]
        || value.input_color != "BGR"
        || value.crop_reference != "tools/infer/utility.py"
        || value.perspective_interpolation != "OpenCV-INTER_CUBIC"
        || value.perspective_border != "OpenCV-BORDER_REPLICATE"
        || value.vertical_rotation != "numpy-rot90-counterclockwise"
        || value.vertical_ratio_threshold.to_bits() != 1.5_f64.to_bits()
        || value.resize_interpolation != "OpenCV-INTER_LINEAR"
        || (value.normalization_scale as f32).to_bits() != SCALE.to_bits()
        || value.normalization_mean.to_bits() != 0.5_f32.to_bits()
        || value.normalization_standard_deviation.to_bits() != 0.5_f32.to_bits()
        || value.maximum_width != MAX_WIDTH
        || value.quality_corpus != "fixtures/manifest.json#ocr_quality"
        || value.quality_groups
            != [
                QualityGroup {
                    group: "simplified".into(),
                    evaluated_characters: 65,
                    observed_errors: 0,
                    maximum_cer: 0.05,
                },
                QualityGroup {
                    group: "traditional".into(),
                    evaluated_characters: 65,
                    observed_errors: 6,
                    maximum_cer: 0.10,
                },
                QualityGroup {
                    group: "english".into(),
                    evaluated_characters: 185,
                    observed_errors: 1,
                    maximum_cer: 0.05,
                },
                QualityGroup {
                    group: "mixed".into(),
                    evaluated_characters: 116,
                    observed_errors: 1,
                    maximum_cer: 0.08,
                },
            ]
    {
        return Err(ocr("recognizerAuthorityDrift"));
    }
    Ok(value)
}

pub(super) fn validate_authority() -> Result<(), ConversionError> {
    authority().map(drop)
}

pub(super) fn load_characters(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<(std::sync::Arc<[String]>, std::sync::Arc<ResourceReservation>), ConversionError> {
    let expected = authority()?;
    if bytes.len() as u64 != expected.character_table_size
        || format!("{:x}", Sha256::digest(bytes)) != expected.character_table_sha256
    {
        return Err(ocr("characterTableHashMismatch"));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| ocr("invalidCharacterTable"))?;
    if source.contains('\r') || !source.ends_with('\n') {
        return Err(ocr("invalidCharacterTable"));
    }
    let requested_vec_bytes = (CLASSES - 1)
        .checked_mul(std::mem::size_of::<String>())
        .ok_or_else(|| limit("recognitionMemory"))?;
    let mut reservation = context.reserve_memory(super::budget::to_u64(requested_vec_bytes)?)?;
    let mut result = Vec::new();
    result.try_reserve_exact(CLASSES - 1).map_err(|_| limit("recognitionMemory"))?;
    let actual_vec_bytes = result
        .capacity()
        .checked_mul(std::mem::size_of::<String>())
        .ok_or_else(|| limit("recognitionMemory"))?;
    if actual_vec_bytes > requested_vec_bytes {
        reservation.grow(super::budget::to_u64(actual_vec_bytes - requested_vec_bytes)?)?;
    }
    let requested_entry_bytes = expected
        .character_table_entries
        .checked_mul(std::mem::size_of::<&str>())
        .ok_or_else(|| limit("recognitionMemory"))?;
    let mut entry_reservation =
        context.reserve_memory(super::budget::to_u64(requested_entry_bytes)?)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(expected.character_table_entries)
        .map_err(|_| limit("recognitionMemory"))?;
    let actual_entry_bytes = entries
        .capacity()
        .checked_mul(std::mem::size_of::<&str>())
        .ok_or_else(|| limit("recognitionMemory"))?;
    if actual_entry_bytes > requested_entry_bytes {
        entry_reservation
            .grow(super::budget::to_u64(actual_entry_bytes - requested_entry_bytes)?)?;
    }
    for line in source.lines() {
        if line.is_empty() {
            return Err(ocr("invalidCharacterTable"));
        }
        entries.push(line);
        reservation.grow(super::budget::to_u64(line.len())?)?;
        let mut character = String::new();
        character.try_reserve_exact(line.len()).map_err(|_| limit("recognitionMemory"))?;
        if character.capacity() > line.len() {
            reservation.grow(super::budget::to_u64(character.capacity() - line.len())?)?;
        }
        character.push_str(line);
        result.push(character);
    }
    if result.len() != expected.character_table_entries {
        return Err(ocr("invalidCharacterTable"));
    }
    entries.sort_unstable();
    if entries.windows(2).any(|pair| pair[0] == pair[1]) || entries.binary_search(&" ").is_ok() {
        return Err(ocr("invalidCharacterTable"));
    }
    reservation.grow(1)?;
    let mut space = String::new();
    space.try_reserve_exact(1).map_err(|_| limit("recognitionMemory"))?;
    if space.capacity() > 1 {
        reservation.grow(super::budget::to_u64(space.capacity() - 1)?)?;
    }
    space.push(' ');
    result.push(space);

    let vec_bytes = result
        .capacity()
        .checked_mul(std::mem::size_of::<String>())
        .ok_or_else(|| limit("recognitionMemory"))?;
    let slice_bytes = result
        .len()
        .checked_mul(std::mem::size_of::<String>())
        .ok_or_else(|| limit("recognitionMemory"))?;
    reservation.grow(super::budget::to_u64(slice_bytes)?)?;
    let result = std::sync::Arc::<[String]>::from(result);
    reservation.shrink(super::budget::to_u64(vec_bytes)?)?;
    Ok((result, std::sync::Arc::new(reservation)))
}
