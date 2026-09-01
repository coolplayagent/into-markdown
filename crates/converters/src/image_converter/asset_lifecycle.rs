use super::format::RasterFormat;
use into_markdown_core::{
    Asset, AssetId, ConversionError, ConversionOptions, ExecutionContext, ResolvedInput,
    ResourceReservation,
};

pub(super) struct Inventory {
    pub(super) total_bytes: u64,
    pub(super) original_id: AssetId,
    pub(super) assets: Vec<Asset>,
    pub(super) leases: Vec<ResourceReservation>,
}

pub(super) fn original(
    input: &ResolvedInput,
    format: RasterFormat,
    retain: bool,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Inventory, ConversionError> {
    let source_bytes = u64::try_from(input.bytes.len())
        .map_err(|_| limit("max_asset_bytes", "image source length is not representable"))?;
    let total_bytes = if retain { source_bytes } else { 0 };
    if total_bytes > options.limits.max_asset_bytes {
        return Err(limit(
            "max_asset_bytes",
            format!("original image {total_bytes} exceeds max_asset_bytes"),
        ));
    }
    if total_bytes > options.limits.max_total_asset_bytes {
        return Err(limit(
            "max_total_asset_bytes",
            format!("original image {total_bytes} exceeds max_total_asset_bytes"),
        ));
    }
    let memory = retain.then(|| context.reserve_memory(total_bytes)).transpose()?;
    let original_id = AssetId("image-original".into());
    Ok(Inventory {
        total_bytes,
        assets: vec![Asset {
            id: original_id.clone(),
            filename: Some(format!("source.{}", format.extension())),
            media_type: format.media_type().into(),
            bytes: if retain { input.bytes.to_vec() } else { Vec::new() },
            external_uri: None,
        }],
        original_id,
        leases: memory.into_iter().collect(),
    })
}

fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}
