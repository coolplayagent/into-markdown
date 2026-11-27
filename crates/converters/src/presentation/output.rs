use super::allocation::try_clone_string;
use super::budget::{ASSET_INDEX_ENTRY_CHARGE, MAX_ASSET_DIGEST_CANDIDATES};
use super::charts_notes::parse_chart_text;
use super::error::{limit, malformed};
use super::images::{PresentationImage, asset_digest, find_duplicate_asset};
use super::model::{Package, ParseState, Relationships, Shape};
use super::relationships::{relationship_by_id, require_content_type, resolve_target};
use super::schema::{CHART_REL, IMAGE_REL};
use super::shape_elements::shape_block_count;
use super::tables::table_block;
use super::text::ParagraphSource;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, Diagnostic,
    DiagnosticSeverity, ErrorPolicy, ExecutionContext, Inline, Rect, SourceLocator,
};
use std::fmt::Write as _;
use std::path::Path;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn shapes_to_blocks(
    shapes: Vec<Shape>,
    package: &mut Package<'_>,
    relationships: &Relationships,
    part: &str,
    slide: u32,
    options: &ConversionOptions,
    context: &ExecutionContext,
    state: &mut ParseState,
) -> Result<Vec<BlockNode>, ConversionError> {
    let block_capacity = shape_block_count(&shapes)?;
    let mut result = Vec::new();
    result.try_reserve_exact(block_capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve slide blocks: {error}"))
    })?;
    for mut shape in shapes {
        context.checkpoint()?;
        let bounds = Some(shape.geometry.bounds()?);
        let z_order = shape.z_order;
        let languages = shape.languages;
        for recovery in shape.recoveries {
            state.diagnostics.try_reserve(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve recovery diagnostic: {error}"))
            })?;
            state.diagnostics.push(Diagnostic {
                code: recovery.code.into(),
                severity: DiagnosticSeverity::Warning,
                message: recovery.message,
                locator: Some(SourceLocator {
                    slide: Some(slide),
                    bounds,
                    part: Some(part.into()),
                    ..SourceLocator::default()
                }),
            });
        }
        if let Some((id, alt)) = shape.image {
            let relationship = relationship_by_id(relationships, &id).ok_or_else(|| {
                malformed(Some(part), format!("image relationship {id} is missing"))
            })?;
            if relationship.external || relationship.kind != IMAGE_REL {
                return Err(malformed(Some(part), "image relationship has wrong type or mode"));
            }
            let target = resolve_target(part, &relationship.target)?;
            let content_type = package.content_types.content_type(&target).ok_or_else(|| {
                malformed(Some("[Content_Types].xml"), format!("image {target} lacks content type"))
            })?;
            let image = match PresentationImage::from_part(&target, content_type) {
                Ok(image) => image,
                Err(error)
                    if options.error_policy == ErrorPolicy::BestEffort
                        && recoverable_media_error(&error) =>
                {
                    push_omitted_image(
                        &mut result,
                        state,
                        &target,
                        alt.as_deref(),
                        part,
                        slide,
                        bounds,
                        z_order,
                        &languages,
                        &error,
                    )?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !state.assets_by_part.contains_key(&target) {
                package.load(&target, options, context)?;
                let loaded = package
                    .take_loaded(&target)
                    .ok_or_else(|| malformed(Some(&target), "image part is missing"))?;
                let bytes = loaded.bytes;
                let caption = match image
                    .validate_and_read_text(&bytes, &target, options, context, package)
                {
                    Ok(text) => text,
                    Err(error)
                        if options.error_policy == ErrorPolicy::BestEffort
                            && recoverable_media_error(&error) =>
                    {
                        drop(bytes);
                        package.shrink_memory(loaded.charge)?;
                        push_omitted_image(
                            &mut result,
                            state,
                            &target,
                            alt.as_deref(),
                            part,
                            slide,
                            bounds,
                            z_order,
                            &languages,
                            &error,
                        )?;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if size > options.limits.max_asset_bytes {
                    return Err(limit("max_asset_bytes", format!("asset {target}")));
                }
                state.asset_bytes = state
                    .asset_bytes
                    .checked_add(size)
                    .ok_or_else(|| limit("max_total_asset_bytes", "overflow"))?;
                if state.asset_bytes > options.limits.max_total_asset_bytes {
                    return Err(limit("max_total_asset_bytes", state.asset_bytes.to_string()));
                }
                let digest = asset_digest(&bytes, context)?;
                let duplicate =
                    state.assets_by_digest.get(&digest).map_or(Ok(None), |candidates| {
                        find_duplicate_asset(&state.assets, candidates, &bytes, context)
                    })?;
                let id = if let Some(id) = duplicate {
                    drop(bytes);
                    package.shrink_memory(loaded.charge)?;
                    id
                } else {
                    let candidate_count = state.assets_by_digest.get(&digest).map_or(0, Vec::len);
                    if candidate_count >= MAX_ASSET_DIGEST_CANDIDATES {
                        return Err(limit(
                            "asset_digest_collisions",
                            format!(
                                "more than {MAX_ASSET_DIGEST_CANDIDATES} distinct assets share a digest"
                            ),
                        ));
                    }
                    // Charge before either index can allocate. The package-held reservation stays
                    // live through output construction and is then authenticated and shrunk into
                    // the opaque `ConverterOutput` lease before this converter returns.
                    let retained_bytes = u64::try_from(bytes.capacity()).unwrap_or(u64::MAX);
                    if loaded.charge < retained_bytes {
                        return Err(limit(
                            "max_memory_bytes",
                            format!("asset buffer for {target} exceeds its admitted envelope"),
                        ));
                    }
                    package.shrink_memory(loaded.charge - retained_bytes)?;
                    package.grow_memory(ASSET_INDEX_ENTRY_CHARGE)?;
                    state.assets.try_reserve(1).map_err(|error| {
                        limit("max_memory_bytes", format!("cannot reserve image asset: {error}"))
                    })?;
                    let mut id = String::new();
                    id.try_reserve(48).map_err(|error| {
                        limit(
                            "max_memory_bytes",
                            format!("cannot reserve asset identifier: {error}"),
                        )
                    })?;
                    write!(id, "presentation-asset-{}", state.assets.len() + 1)
                        .map_err(|_| malformed(Some(&target), "cannot format asset identifier"))?;
                    let filename = Path::new(&target)
                        .file_name()
                        .and_then(|value| value.to_str())
                        .map(|value| try_clone_string(value, "asset filename"))
                        .transpose()?;
                    state.assets.push(Asset {
                        id: AssetId(try_clone_string(&id, "asset identifier")?),
                        filename,
                        media_type: image.media_type().into(),
                        bytes,
                        external_uri: None,
                    });
                    let asset_index = state.assets.len() - 1;
                    if !state.assets_by_digest.contains_key(&digest) {
                        state.assets_by_digest.try_reserve(1).map_err(|error| {
                            limit(
                                "max_memory_bytes",
                                format!("cannot reserve asset digest index: {error}"),
                            )
                        })?;
                    }
                    let candidates = state.assets_by_digest.entry(digest).or_default();
                    candidates.try_reserve(1).map_err(|error| {
                        limit(
                            "max_memory_bytes",
                            format!("cannot reserve asset digest candidate: {error}"),
                        )
                    })?;
                    candidates.push(asset_index);
                    id
                };
                package.grow_memory(ASSET_INDEX_ENTRY_CHARGE)?;
                state.assets_by_part.try_reserve(1).map_err(|error| {
                    limit("max_memory_bytes", format!("cannot reserve asset part index: {error}"))
                })?;
                state
                    .assets_by_part
                    .insert(try_clone_string(&target, "asset part path")?, (id, caption));
            }
            let (picture, caption) = image_blocks(state, package, &target, alt)?;
            result.push(state.node(
                picture,
                part,
                slide,
                bounds,
                Some(z_order),
                Some(&languages),
            )?);
            if let Some(caption) = caption {
                result
                    .try_reserve(1)
                    .map_err(|error| limit("max_memory_bytes", error.to_string()))?;
                result.push(state.node(
                    caption,
                    &target,
                    slide,
                    bounds,
                    Some(z_order),
                    Some(&languages),
                )?);
            }
        }
        if let Some(id) = shape.chart {
            let relationship = relationship_by_id(relationships, &id).ok_or_else(|| {
                malformed(Some(part), format!("chart relationship {id} is missing"))
            })?;
            if relationship.external || relationship.kind != CHART_REL {
                return Err(malformed(Some(part), "chart relationship has wrong type or mode"));
            }
            let target = resolve_target(part, &relationship.target)?;
            require_content_type(
                package,
                &target,
                "application/vnd.openxmlformats-officedocument.drawingml.chart+xml",
            )?;
            // Chart relationship metadata was validated before hidden-shape filtering; never
            // treat an external workbook or embedded-package target as chart text.
            let values = {
                let bytes = package.load_for_parse(&target, options, context)?;
                parse_chart_text(bytes, &target, options, context)?
            };
            package.release_parsed(&target)?;
            if !values.is_empty() {
                state.add_inlines(values.len().saturating_mul(2).saturating_sub(1))?;
                let mut chart_inlines = Vec::new();
                let inline_capacity = values.len().saturating_mul(2).saturating_sub(1);
                chart_inlines.try_reserve_exact(inline_capacity).map_err(|error| {
                    limit("max_memory_bytes", format!("cannot reserve chart inlines: {error}"))
                })?;
                for value in values {
                    if !chart_inlines.is_empty() {
                        chart_inlines.push(Inline::LineBreak);
                    }
                    chart_inlines.push(Inline::Text { value, marks: Vec::new() });
                }
                result.push(state.node(
                    Block::Paragraph(chart_inlines),
                    &target,
                    slide,
                    bounds,
                    Some(z_order),
                    Some(&languages),
                )?);
            }
        }
        if let Some(rows) = shape.table {
            let table =
                table_block(rows, part, slide, bounds, z_order, &languages, options, state)?;
            result.push(state.node(table, part, slide, bounds, Some(z_order), Some(&languages))?);
        }
        let source = ParagraphSource { part, slide, bounds, z_order, languages: &languages };
        source.append_blocks(&mut shape.paragraphs, shape.title, &mut result, state)?;
    }
    Ok(result)
}

fn image_blocks(
    state: &mut ParseState,
    package: &mut Package<'_>,
    target: &str,
    alt: Option<String>,
) -> Result<(Block, Option<Block>), ConversionError> {
    let (id, caption) = state
        .assets_by_part
        .get(target)
        .ok_or_else(|| malformed(Some(target), "image part index is missing"))?;
    let picture =
        Block::Image { asset: AssetId(try_clone_string(id, "asset part identifier")?), alt };
    if caption.is_empty() {
        return Ok((picture, None));
    }
    package.grow_memory(caption.len() as u64)?;
    let value = try_clone_string(caption, "EMF source text")?;
    state.add_inlines(1)?;
    Ok((picture, Some(Block::Paragraph(vec![Inline::Text { value, marks: Vec::new() }]))))
}

fn recoverable_media_error(error: &ConversionError) -> bool {
    matches!(error, ConversionError::Malformed { .. } | ConversionError::Unsupported { .. })
}

#[allow(clippy::too_many_arguments)]
fn push_omitted_image(
    blocks: &mut Vec<BlockNode>,
    state: &mut ParseState,
    target: &str,
    alt: Option<&str>,
    source_part: &str,
    slide: u32,
    bounds: Option<Rect>,
    z_order: usize,
    languages: &[String],
    error: &ConversionError,
) -> Result<(), ConversionError> {
    state.diagnostics.try_reserve(1).map_err(|allocation| {
        limit("max_memory_bytes", format!("cannot reserve omitted-media diagnostic: {allocation}"))
    })?;
    state.diagnostics.push(Diagnostic {
        code: "presentation.unsupportedMediaOmitted".into(),
        severity: DiagnosticSeverity::Warning,
        message: format!("media {target} was omitted: {error}"),
        locator: Some(SourceLocator {
            slide: Some(slide),
            bounds,
            part: Some(target.into()),
            ..SourceLocator::default()
        }),
    });
    state.add_inlines(1)?;
    let label = alt.filter(|value| !value.is_empty()).unwrap_or(target);
    blocks.try_reserve(1).map_err(|allocation| {
        limit("max_memory_bytes", format!("cannot reserve omitted-media placeholder: {allocation}"))
    })?;
    blocks.push(state.node(
        Block::Paragraph(vec![Inline::Text {
            value: format!("[Unsupported media: {label}]"),
            marks: Vec::new(),
        }]),
        source_part,
        slide,
        bounds,
        Some(z_order),
        Some(languages),
    )?);
    Ok(())
}
