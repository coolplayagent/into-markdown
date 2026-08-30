use crate::odf::image_validation::image_profile;
use crate::odf::model::{DRAW_NS, ParseState, SVG_NS, XLINK_NS, limit, malformed};
use crate::odf::package::Package;
use crate::odf::paths::canonical_part_name;
use crate::odf::xml::XmlNode;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, ExecutionContext,
    SourceLocator,
};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) fn image_block(
    node: &XmlNode,
    package: &Package,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
    locator: &SourceLocator,
) -> Result<Option<BlockNode>, ConversionError> {
    let mut image_locator = locator.clone();
    image_locator.character_index = Some(state.next_image_anchor);
    state.next_image_anchor = state
        .next_image_anchor
        .checked_add(1)
        .ok_or_else(|| limit("documentNodes", "ODF image anchor index overflow"))?;
    let href = node
        .attr(XLINK_NS, "href")
        .ok_or_else(|| malformed(Some("content.xml"), "draw:image lacks xlink:href"))?;
    if node.attr(XLINK_NS, "type").is_some_and(|value| value != "simple") {
        return Err(malformed(Some("content.xml"), "draw:image xlink:type must be simple"));
    }
    if url::Url::parse(href).is_ok() {
        return Err(malformed(
            Some("content.xml"),
            "external images are forbidden by the offline ODF profile",
        ));
    }
    let href = href.strip_prefix("./").unwrap_or(href);
    let path = canonical_part_name(href, false)?;
    let manifest = package
        .manifest
        .get(&path)
        .ok_or_else(|| malformed(Some(&path), "image is not declared in manifest"))?;
    if super::image_validation::unsupported_media(&manifest.media_type) {
        super::recovery::require_best_effort(options, &path, "unsupported image media")?;
        state.warning("odf.imageOmitted", format!("Unsupported {} image omitted: {path}; placeholder retained, original bytes not exported", manifest.media_type), image_locator.clone());
        state.add_inlines(1)?;
        return Ok(Some(state.node(
            Block::Paragraph(vec![into_markdown_core::Inline::Text {
                value: format!("[Image omitted: {path} ({})]", manifest.media_type),
                marks: vec![],
            }]),
            image_locator,
        )?));
    }
    image_profile(&path, &manifest.media_type)?;
    let bytes =
        package.parts.get(&path).ok_or_else(|| malformed(Some(&path), "image part is missing"))?;
    context.checkpoint()?;
    let digest = format!("{:x}", Sha256::digest(bytes));
    let id = if let Some(id) = state.asset_ids.get(&digest) {
        id.clone()
    } else {
        let bytes_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        state.total_asset_bytes = state
            .total_asset_bytes
            .checked_add(bytes_len)
            .ok_or_else(|| limit("max_total_asset_bytes", "ODF asset total overflow"))?;
        if state.total_asset_bytes > options.limits.max_total_asset_bytes {
            return Err(limit(
                "max_total_asset_bytes",
                format!("{} > {}", state.total_asset_bytes, options.limits.max_total_asset_bytes),
            ));
        }
        let id = AssetId(format!("odf-image-{}", &digest[..20]));
        state.assets.push(Asset {
            id: id.clone(),
            filename: Path::new(&path)
                .file_name()
                .and_then(|value| value.to_str())
                .map(str::to_owned),
            media_type: manifest.media_type.clone(),
            bytes: bytes.clone(),
            external_uri: None,
        });
        state.asset_ids.insert(digest, id.clone());
        id
    };
    let alt = node.attr(DRAW_NS, "name").map(str::to_owned).or_else(|| {
        node.children()
            .find(|child| child.is(SVG_NS, "desc") || child.is(SVG_NS, "title"))
            .map(XmlNode::text)
    });
    Ok(Some(state.node(Block::Image { asset: id, alt }, image_locator)?))
}
