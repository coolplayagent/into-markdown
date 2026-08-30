use crate::docx::{SupportedImage, validate_image_bytes};
use crate::workbook::budget::checked_field_bytes;
use crate::workbook::error::{limit, malformed};
use into_markdown_core::{Asset, AssetId, ConversionError, ConversionOptions, ExecutionContext};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default)]
pub(super) struct ExtractedAssets {
    pub(super) assets: Vec<Asset>,
    pub(super) by_digest: BTreeMap<[u8; 32], (AssetId, usize)>,
    pub(super) validations: Vec<ImageValidation>,
    pub(super) total_bytes: u64,
    pub(super) decode_working_set_peak: u64,
}

#[derive(Debug)]
pub(super) struct ImageValidation {
    pub(super) image: SupportedImage,
    pub(super) asset_index: usize,
    pub(super) part: String,
}

impl ExtractedAssets {
    pub(super) fn add(
        &mut self,
        part: &str,
        media_type: &str,
        bytes: Vec<u8>,
        options: &ConversionOptions,
    ) -> Result<(AssetId, usize), ConversionError> {
        checked_field_bytes(options, "workbook image asset id", &[39])?;
        checked_field_bytes(
            options,
            "workbook image media type",
            &[u64::try_from(media_type.len()).unwrap_or(u64::MAX)],
        )?;
        if let Some(filename) = Path::new(part).file_name().and_then(|value| value.to_str()) {
            checked_field_bytes(
                options,
                "workbook image filename",
                &[u64::try_from(filename.len()).unwrap_or(u64::MAX)],
            )?;
        }
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > options.limits.max_asset_bytes {
            return Err(limit(
                "max_asset_bytes",
                format!("{part}: {size} > {}", options.limits.max_asset_bytes),
            ));
        }
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if let Some((id, index)) = self.by_digest.get(&digest) {
            return Ok((id.clone(), *index));
        }
        self.total_bytes = self
            .total_bytes
            .checked_add(size)
            .ok_or_else(|| limit("max_total_asset_bytes", "workbook asset size overflow"))?;
        if self.total_bytes > options.limits.max_total_asset_bytes {
            return Err(limit(
                "max_total_asset_bytes",
                format!("{} > {}", self.total_bytes, options.limits.max_total_asset_bytes),
            ));
        }
        if self.assets.len() as u64 >= u64::from(options.limits.max_archive_entries) {
            return Err(limit("max_archive_entries", "too many distinct workbook assets"));
        }
        let mut suffix = String::with_capacity(24);
        for byte in &digest[..12] {
            use std::fmt::Write as _;
            write!(&mut suffix, "{byte:02x}").map_err(|_| ConversionError::Internal {
                detail: "could not construct workbook asset identifier".into(),
            })?;
        }
        let id = AssetId(format!("workbook-image-{suffix}"));
        let index = self.assets.len();
        self.assets.push(Asset {
            id: id.clone(),
            filename: Path::new(part)
                .file_name()
                .and_then(|value| value.to_str())
                .map(str::to_owned),
            media_type: media_type.into(),
            bytes,
            external_uri: None,
        });
        self.by_digest.insert(digest, (id.clone(), index));
        Ok((id, index))
    }

    pub(super) fn account_decode_working_set(
        &mut self,
        image: SupportedImage,
        bytes: &[u8],
        part: &str,
    ) -> Result<(), ConversionError> {
        let working_set = match image {
            SupportedImage::Png => 64 * 1024,
            SupportedImage::Jpeg => {
                let (width, height) = validated_jpeg_dimensions(bytes, part)?;
                u64::from(width)
                    .checked_mul(u64::from(height))
                    .and_then(|pixels| pixels.checked_mul(4))
                    .and_then(|pixels| pixels.checked_mul(6))
                    .and_then(|value| {
                        u64::try_from(bytes.len())
                            .ok()
                            .and_then(|size| size.checked_mul(2))
                            .and_then(|size| value.checked_add(size))
                    })
                    .and_then(|value| value.checked_add(256 * 1024))
                    .ok_or_else(|| limit("max_memory_bytes", "JPEG working-set model overflow"))?
            }
        };
        self.decode_working_set_peak = self.decode_working_set_peak.max(working_set);
        Ok(())
    }

    pub(super) fn validate_images(
        &self,
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Vec<AssetId>, ConversionError> {
        let mut invalid = Vec::new();
        for validation in &self.validations {
            context.checkpoint()?;
            let bytes = self
                .assets
                .get(validation.asset_index)
                .ok_or_else(|| ConversionError::Internal {
                    detail: "workbook image validation lost its deduplicated asset".into(),
                })?
                .bytes
                .as_slice();
            match validate_image_bytes(validation.image, bytes, &validation.part, options, context)
            {
                Ok(()) => {}
                Err(ConversionError::Malformed { .. } | ConversionError::Unsupported { .. })
                    if options.error_policy == into_markdown_core::ErrorPolicy::BestEffort =>
                {
                    let asset = &self.assets[validation.asset_index].id;
                    if !invalid.contains(asset) {
                        invalid.push(asset.clone());
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(invalid)
    }

    pub(super) fn remove_invalid(&mut self, invalid: &[AssetId]) {
        self.assets.retain(|asset| !invalid.contains(&asset.id));
        self.total_bytes = self.assets.iter().fold(0_u64, |total, asset| {
            total.saturating_add(u64::try_from(asset.bytes.len()).unwrap_or(u64::MAX))
        });
        self.validations.clear();
        self.by_digest.clear();
    }
}

fn validated_jpeg_dimensions(bytes: &[u8], part: &str) -> Result<(u32, u32), ConversionError> {
    let mut cursor = 2_usize;
    while cursor < bytes.len() {
        if bytes.get(cursor) != Some(&0xff) {
            return Err(malformed(Some(part), "validated JPEG marker is unavailable"));
        }
        while bytes.get(cursor) == Some(&0xff) {
            cursor = cursor.saturating_add(1);
        }
        let marker = *bytes
            .get(cursor)
            .ok_or_else(|| malformed(Some(part), "validated JPEG marker is truncated"))?;
        cursor = cursor.saturating_add(1);
        if marker == 0xd9 {
            break;
        }
        let length_end = cursor
            .checked_add(2)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed(Some(part), "validated JPEG segment is truncated"))?;
        let length = usize::from(u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]));
        let segment_end = cursor
            .checked_add(length)
            .filter(|end| *end <= bytes.len() && length >= 2)
            .ok_or_else(|| malformed(Some(part), "validated JPEG segment is unavailable"))?;
        if marker == 0xc0 {
            let data = &bytes[length_end..segment_end];
            if data.len() < 5 {
                return Err(malformed(Some(part), "validated JPEG frame is truncated"));
            }
            return Ok((
                u32::from(u16::from_be_bytes([data[3], data[4]])),
                u32::from(u16::from_be_bytes([data[1], data[2]])),
            ));
        }
        cursor = segment_end;
    }
    Err(malformed(Some(part), "validated JPEG frame is missing"))
}
