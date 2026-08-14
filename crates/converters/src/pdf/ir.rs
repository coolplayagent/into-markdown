use super::{
    Block, BlockNode, Character, ConversionError, Inline, NodeId, PROVIDER_ID, PageInfo,
    Provenance, ProvenanceKind, Rect, normalize_rect, page_locator, resource,
};

pub(super) fn text_block(
    page: u32,
    info: &PageInfo,
    characters: &[Character],
) -> Result<BlockNode, ConversionError> {
    let mut inlines = Vec::new();
    let inline_capacity = allocation_capacity_bound(characters.len())?;
    inlines
        .try_reserve_exact(inline_capacity)
        .map_err(|_| resource("max_memory_bytes", "source text allocation failed"))?;
    if inlines.capacity() > inline_capacity {
        return Err(resource("max_memory_bytes", "source text capacity exceeded its plan"));
    }
    for character in characters {
        let mut value = String::new();
        value
            .try_reserve_exact(4)
            .map_err(|_| resource("max_memory_bytes", "source character allocation failed"))?;
        if value.capacity() > allocation_capacity_bound(4)? {
            return Err(resource(
                "max_memory_bytes",
                "source character capacity exceeded its plan",
            ));
        }
        value.push(character.value);
        inlines.push(Inline::SourceText {
            value,
            marks: Vec::new(),
            provenance: Box::new(provenance(
                page,
                Some(normalize_rect(character.bounds, info)?),
                Some(character),
                info,
            )?),
        });
    }
    Ok(BlockNode {
        id: NodeId(format!("pdf-page-{page}-native-text")),
        block: Block::Paragraph(inlines),
        provenance: provenance(page, None, None, info)?,
    })
}

pub(super) fn provenance(
    page: u32,
    bounds: Option<Rect>,
    character: Option<&Character>,
    info: &PageInfo,
) -> Result<Provenance, ConversionError> {
    let mut locator = page_locator(page, info);
    locator.bounds = bounds;
    if let Some(character) = character {
        locator.character_index = Some(character.index);
        locator.font_name = character
            .font_name
            .as_ref()
            .map(|name| try_clone_bounded(name, "font name clone"))
            .transpose()?;
        locator.font_size = Some(character.font_size);
        locator.rotation_degrees =
            Some((character.angle_degrees + f32::from(info.rotation_degrees)).rem_euclid(360.0));
    }
    Ok(Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: try_clone_bounded(PROVIDER_ID, "provenance provider")?,
        locator,
        confidence: None,
    })
}

pub(super) fn allocation_capacity_bound(length: usize) -> Result<usize, ConversionError> {
    if length == 0 {
        return Ok(0);
    }
    length
        .checked_mul(2)
        .map(|value| value.max(4))
        .ok_or_else(|| resource("max_memory_bytes", "allocation capacity bound overflow"))
}

pub(super) fn try_clone_bounded(
    value: &str,
    detail: &'static str,
) -> Result<String, ConversionError> {
    into_markdown_pdfium::fixed_string(value).map_err(|_| resource("max_memory_bytes", detail))
}

pub(super) fn character_ir_allocation_bytes(count: u32) -> Result<u64, ConversionError> {
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    let inline_capacity = allocation_capacity_bound(count)?;
    let per_character = std::mem::size_of::<Provenance>()
        .checked_add(allocation_capacity_bound(4)?)
        .and_then(|value| value.checked_add(allocation_capacity_bound(PROVIDER_ID.len()).ok()?))
        .ok_or_else(|| resource("max_memory_bytes", "character IR memory overflow"))?;
    let node_id_length = "pdf-page--native-text"
        .len()
        .checked_add(10)
        .ok_or_else(|| resource("max_memory_bytes", "character node ID overflow"))?;
    let bytes = inline_capacity
        .checked_mul(std::mem::size_of::<Inline>())
        .and_then(|value| value.checked_add(count.checked_mul(per_character)?))
        .and_then(|value| value.checked_add(allocation_capacity_bound(node_id_length).ok()?))
        .and_then(|value| value.checked_add(allocation_capacity_bound(PROVIDER_ID.len()).ok()?))
        .and_then(|value| value.checked_add(2 * std::mem::size_of::<BlockNode>()))
        .ok_or_else(|| resource("max_memory_bytes", "character IR memory overflow"))?;
    u64::try_from(bytes)
        .map_err(|_| resource("max_memory_bytes", "character IR memory does not fit u64"))
}

pub(super) fn character_working_set_bytes(
    materialization_bytes: u64,
    retained_font_bytes: u64,
    count: u32,
) -> Result<u64, ConversionError> {
    materialization_bytes
        .checked_add(retained_font_bytes)
        .and_then(|bytes| bytes.checked_add(character_ir_allocation_bytes(count).ok()?))
        .ok_or_else(|| resource("max_memory_bytes", "character working set overflow"))
}
