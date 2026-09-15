//! Restore exact chapter bytes when nested HTML recovery retained its prepared input.
use super::spine::Chapter;
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::{AssetId, Block, ConversionError, ExecutionContext};
use sha2::{Digest, Sha256};

pub(super) fn append(
    chapters: &mut Vec<Chapter>,
    mut chapter: Chapter,
    archive: &mut SafeArchive<'_, '_>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    restore(&mut chapter, archive, context)?;
    chapters.push(chapter);
    Ok(())
}

fn restore(
    chapter: &mut Chapter,
    archive: &mut SafeArchive<'_, '_>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if !chapter.output.diagnostics.iter().any(|d| d.code == "conversion.recovery.originalFile") {
        return Ok(());
    }
    let Some(old_id) = chapter.output.document.blocks.iter().find_map(|node| match &node.block {
        Block::Image { asset, alt: Some(alt) } if alt == "Original source — complete file" => {
            Some(asset.clone())
        }
        _ => None,
    }) else {
        return Err(ConversionError::Internal {
            detail: "EPUB original chapter recovery has no attachment reference".into(),
        });
    };
    let source = archive.read_for_recovery(&chapter.path)?;
    let (bytes, memory) = source.into_parts();
    let _metadata_memory = context.reserve_memory(4096)?;
    let id = AssetId(format!("source-{:x}", Sha256::digest(&bytes)));
    let asset =
        chapter.output.assets.iter_mut().find(|asset| asset.id == old_id).ok_or_else(|| {
            ConversionError::Internal {
                detail: "EPUB original chapter attachment is missing".into(),
            }
        })?;
    asset.bytes = bytes;
    asset.id = id.clone();
    asset.filename = Some(format!("{}.html", id.0));
    for node in &mut chapter.output.document.blocks {
        if let Block::Image { asset, .. } = &mut node.block
            && asset == &old_id
        {
            *asset = id.clone();
        }
    }
    chapter.output.attach_memory_reservation(context, memory)?;
    Ok(())
}
