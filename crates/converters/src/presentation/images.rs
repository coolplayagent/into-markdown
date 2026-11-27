use super::budget::{ASSET_HASH_CHUNK_BYTES, MAX_ASSET_DIGEST_CANDIDATES};
use super::error::{limit, malformed};
use crate::docx::{SupportedImage, supported_image, validate_image_bytes};
use into_markdown_core::{Asset, ConversionError, ConversionOptions, ExecutionContext};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone, Copy)]
pub(super) enum PresentationImage {
    Raster(SupportedImage),
    Emf,
}

impl PresentationImage {
    pub(super) fn from_part(part: &str, declared: &str) -> Result<Self, ConversionError> {
        let extension = Path::new(part).extension().and_then(|value| value.to_str());
        if extension.is_some_and(|value| value.eq_ignore_ascii_case("emf"))
            && matches!(declared, "image/emf" | "image/x-emf")
        {
            return Ok(Self::Emf);
        }
        supported_image(part, declared).map(Self::Raster)
    }

    pub(super) fn media_type(self) -> &'static str {
        match self {
            Self::Raster(image) => image.media_type(),
            Self::Emf => "image/emf",
        }
    }

    pub(super) fn validate_and_read_text(
        self,
        bytes: &[u8],
        part: &str,
        options: &ConversionOptions,
        context: &ExecutionContext,
        package: &mut super::model::Package<'_>,
    ) -> Result<String, ConversionError> {
        match self {
            Self::Raster(image) => {
                validate_image_bytes(image, bytes, part, options, context)?;
                Ok(String::new())
            }
            Self::Emf => super::emf::source_text(bytes, part, context, package),
        }
    }
}

pub(super) fn asset_digest(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<[u8; 32], ConversionError> {
    context.checkpoint()?;
    let mut hasher = Sha256::new();
    for chunk in bytes.chunks(ASSET_HASH_CHUNK_BYTES) {
        context.checkpoint()?;
        hasher.update(chunk);
    }
    Ok(hasher.finalize().into())
}

pub(super) fn find_duplicate_asset(
    assets: &[Asset],
    candidates: &[usize],
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<String>, ConversionError> {
    context.checkpoint()?;
    if candidates.len() > MAX_ASSET_DIGEST_CANDIDATES {
        return Err(limit(
            "asset_digest_collisions",
            format!("{} candidates > {MAX_ASSET_DIGEST_CANDIDATES}", candidates.len()),
        ));
    }
    for index in candidates {
        context.checkpoint()?;
        let asset = assets
            .get(*index)
            .ok_or_else(|| malformed(None, "asset digest index is inconsistent"))?;
        if asset.bytes.len() != bytes.len() {
            continue;
        }
        let mut equal = true;
        for (left, right) in
            asset.bytes.chunks(ASSET_HASH_CHUNK_BYTES).zip(bytes.chunks(ASSET_HASH_CHUNK_BYTES))
        {
            context.checkpoint()?;
            if left != right {
                equal = false;
                break;
            }
        }
        if equal {
            let mut id = String::new();
            id.try_reserve_exact(asset.id.0.len()).map_err(|error| {
                limit(
                    "max_memory_bytes",
                    format!("cannot reserve duplicate asset identifier: {error}"),
                )
            })?;
            id.push_str(&asset.id.0);
            return Ok(Some(id));
        }
    }
    Ok(None)
}
