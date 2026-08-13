//! Reference-driven EPUB image extraction.

use super::package::{ManifestItem, Package};
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConverterOutput, ExecutionContext,
    ResourceReservation,
};
use std::collections::BTreeMap;

pub(super) struct ResourceStore {
    assets: Vec<Asset>,
    by_path: BTreeMap<String, AssetId>,
    memory: Vec<ResourceReservation>,
    retained_bytes: u64,
    max_asset_bytes: u64,
    max_total_asset_bytes: u64,
}

pub(super) struct CoverResource {
    pub(super) id: AssetId,
    pub(super) path: String,
}

impl ResourceStore {
    pub(super) fn new(options: &into_markdown_core::ConversionOptions) -> Self {
        Self {
            assets: Vec::new(),
            by_path: BTreeMap::new(),
            memory: Vec::new(),
            retained_bytes: 0,
            max_asset_bytes: options.limits.max_asset_bytes,
            max_total_asset_bytes: options.limits.max_total_asset_bytes,
        }
    }

    pub(super) fn bind_chapter_images(
        &mut self,
        output: &mut ConverterOutput,
        references: &BTreeMap<String, String>,
        package: &Package,
        archive: &mut SafeArchive<'_, '_>,
    ) -> Result<(), ConversionError> {
        let mut replacements = BTreeMap::<String, AssetId>::new();
        let mut retained = Vec::new();
        for asset in std::mem::take(&mut output.assets) {
            let Some(uri) = asset.external_uri.as_deref() else {
                retained.push(asset);
                continue;
            };
            let Some(target) = references.get(uri) else {
                retained.push(asset);
                continue;
            };
            if target.contains('#') {
                return Err(malformed("image resource reference contains a fragment"));
            }
            let id = self.ensure_image(target, package, archive)?;
            replacements.insert(asset.id.0, id);
        }
        output.assets = retained;
        rewrite_image_ids(&mut output.document.blocks, &replacements)?;
        Ok(())
    }

    pub(super) fn cover(
        &mut self,
        package: &Package,
        archive: &mut SafeArchive<'_, '_>,
    ) -> Result<Option<CoverResource>, ConversionError> {
        let Some(id) = package.cover_id.as_deref() else { return Ok(None) };
        let path = &package.item(id)?.path;
        self.ensure_image(path, package, archive)
            .map(|id| Some(CoverResource { id, path: path.clone() }))
    }

    pub(super) fn finish(
        mut self,
        output: &mut ConverterOutput,
        context: &ExecutionContext,
    ) -> Result<(), ConversionError> {
        output.assets.append(&mut self.assets);
        for memory in self.memory {
            output.attach_memory_reservation(context, memory)?;
        }
        Ok(())
    }

    fn ensure_image(
        &mut self,
        requested_path: &str,
        package: &Package,
        archive: &mut SafeArchive<'_, '_>,
    ) -> Result<AssetId, ConversionError> {
        if let Some(id) = self.by_path.get(requested_path).cloned() {
            return Ok(id);
        }
        let item = package
            .manifest
            .values()
            .find(|item| item.path == requested_path)
            .ok_or_else(|| malformed("XHTML image is not declared in the manifest"))?;
        let selected = select_safe_image(package, item)?;
        if let Some(id) = self.by_path.get(&selected.path).cloned() {
            self.by_path.insert(requested_path.into(), id.clone());
            return Ok(id);
        }
        let info =
            archive.info(&selected.path).ok_or_else(|| malformed("selected image is missing"))?;
        if info.expanded_size > self.max_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: format!("EPUB image {} is {} bytes", selected.path, info.expanded_size),
            });
        }
        let next_total = self.retained_bytes.checked_add(info.expanded_size).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: "EPUB retained asset total overflowed".into(),
            }
        })?;
        if next_total > self.max_total_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: format!("EPUB retained asset total would be {next_total} bytes"),
            });
        }
        let entry = archive.read(&selected.path)?;
        verify_image(&entry.bytes, &selected.media_type)?;
        let (bytes, memory) = entry.into_parts();
        let id = AssetId(format!("epub-resource-{:06}", self.assets.len() + 1));
        self.assets.push(Asset {
            id: id.clone(),
            filename: selected.path.rsplit('/').next().map(str::to_owned),
            media_type: selected.media_type.clone(),
            bytes,
            external_uri: None,
        });
        self.memory.push(memory);
        self.retained_bytes = next_total;
        self.by_path.insert(selected.path.clone(), id.clone());
        self.by_path.insert(requested_path.into(), id.clone());
        Ok(id)
    }
}

fn select_safe_image<'a>(
    package: &'a Package,
    item: &'a ManifestItem,
) -> Result<&'a ManifestItem, ConversionError> {
    package
        .fallback_chain(&item.id)?
        .into_iter()
        .find(|candidate| safe_image_media(&candidate.media_type))
        .ok_or_else(|| ConversionError::Unsupported {
            detail: format!("EPUB image {} has no safe raster fallback", item.path),
        })
}

fn safe_image_media(media_type: &str) -> bool {
    matches!(media_type, "image/png" | "image/jpeg" | "image/gif" | "image/webp")
}

fn verify_image(bytes: &[u8], media_type: &str) -> Result<(), ConversionError> {
    let valid = match media_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ConversionError::Malformed {
            part: None,
            detail: format!("EPUB image bytes do not match declared media type {media_type}"),
        })
    }
}

fn rewrite_image_ids(
    nodes: &mut [BlockNode],
    replacements: &BTreeMap<String, AssetId>,
) -> Result<(), ConversionError> {
    for node in nodes {
        match &mut node.block {
            Block::Image { asset, .. } => {
                if let Some(replacement) = replacements.get(&asset.0) {
                    *asset = replacement.clone();
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    rewrite_image_ids(&mut item.blocks, replacements)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &mut row.cells {
                        rewrite_image_ids(&mut cell.blocks, replacements)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => rewrite_image_ids(blocks, replacements)?,
            _ => {}
        }
    }
    Ok(())
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: None, detail: format!("EPUB resource: {}", detail.into()) }
}
