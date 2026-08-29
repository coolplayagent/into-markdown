use super::allocation::try_clone_string;
use super::budget::{ASSET_INDEX_ENTRY_CHARGE, MAX_ASSET_DIGEST_CANDIDATES};
use super::charts_notes::parse_chart_text;
use super::error::{limit, malformed};
use super::images::{asset_digest, find_duplicate_asset};
use super::model::{Package, ParseState, Relationships, Shape, TextParagraph};
use super::relationships::{relationship_by_id, require_content_type, resolve_target};
use super::schema::{CHART_REL, IMAGE_REL};
use super::shape_elements::shape_block_count;
use super::tables::table_block;
use crate::docx::{supported_image, validate_image_bytes};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, ConversionError, ConversionOptions, Diagnostic,
    DiagnosticSeverity, ErrorPolicy, ExecutionContext, Inline, ListItem, ListKind, Rect,
    SourceLocator,
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
    for shape in shapes {
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
            let image = match supported_image(&target, content_type) {
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
            let asset_id = if let Some(id) = state.assets_by_part.get(&target) {
                try_clone_string(id, "asset part identifier")?
            } else {
                let validation = {
                    let bytes = package.load(&target, options, context)?;
                    validate_image_bytes(image, bytes, &target, options, context)
                };
                if let Err(error) = validation {
                    if options.error_policy == ErrorPolicy::BestEffort
                        && recoverable_media_error(&error)
                    {
                        let loaded = package
                            .take_loaded(&target)
                            .ok_or_else(|| malformed(Some(&target), "image part is missing"))?;
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
                    return Err(error);
                }
                let loaded = package
                    .take_loaded(&target)
                    .ok_or_else(|| malformed(Some(&target), "image part is missing"))?;
                let bytes = loaded.bytes;
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
                        .map(|value| {
                            let mut owned = String::new();
                            owned.try_reserve_exact(value.len()).map_err(|error| {
                                limit(
                                    "max_memory_bytes",
                                    format!("cannot reserve asset filename: {error}"),
                                )
                            })?;
                            owned.push_str(value);
                            Ok::<String, ConversionError>(owned)
                        })
                        .transpose()?;
                    state.assets.push(Asset {
                        id: AssetId(try_clone_string(&id, "asset identifier")?),
                        filename,
                        media_type: image.media_type().into(),
                        bytes,
                        external_uri: None,
                    });
                    let asset_index = state.assets.len() - 1;
                    if let Some(candidates) = state.assets_by_digest.get_mut(&digest) {
                        candidates.try_reserve(1).map_err(|error| {
                            limit(
                                "max_memory_bytes",
                                format!("cannot reserve asset digest candidate: {error}"),
                            )
                        })?;
                        candidates.push(asset_index);
                    } else {
                        state.assets_by_digest.try_reserve(1).map_err(|error| {
                            limit(
                                "max_memory_bytes",
                                format!("cannot reserve asset digest index: {error}"),
                            )
                        })?;
                        let mut candidates = Vec::new();
                        candidates.try_reserve_exact(1).map_err(|error| {
                            limit(
                                "max_memory_bytes",
                                format!("cannot reserve asset digest candidate: {error}"),
                            )
                        })?;
                        candidates.push(asset_index);
                        state.assets_by_digest.insert(digest, candidates);
                    }
                    id
                };
                package.grow_memory(ASSET_INDEX_ENTRY_CHARGE)?;
                state.assets_by_part.try_reserve(1).map_err(|error| {
                    limit("max_memory_bytes", format!("cannot reserve asset part index: {error}"))
                })?;
                state.assets_by_part.insert(target, try_clone_string(&id, "asset part index")?);
                id
            };
            result.push(state.node(
                Block::Image { asset: AssetId(asset_id), alt },
                part,
                slide,
                bounds,
                Some(z_order),
                Some(&languages),
            )?);
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
        let mut paragraphs = shape.paragraphs;
        let mut paragraph_index = 0_usize;
        while paragraph_index < paragraphs.len() {
            if !shape.title && paragraphs[paragraph_index].bullet.is_some() {
                let level = paragraphs[paragraph_index].level;
                result.push(build_list_level(
                    &mut paragraphs,
                    &mut paragraph_index,
                    level,
                    part,
                    slide,
                    bounds,
                    z_order,
                    &languages,
                    state,
                )?);
                continue;
            }
            let paragraph = &mut paragraphs[paragraph_index];
            state.add_inlines(paragraph.text.len())?;
            let text = std::mem::take(&mut paragraph.text);
            let block = if shape.title {
                Block::Heading { level: 3, content: text }
            } else {
                Block::Paragraph(text)
            };
            result.push(state.node(block, part, slide, bounds, Some(z_order), Some(&languages))?);
            paragraph_index += 1;
        }
    }
    Ok(result)
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

#[allow(clippy::too_many_arguments)]
fn build_list_level(
    paragraphs: &mut [TextParagraph],
    index: &mut usize,
    level: u8,
    part: &str,
    slide: u32,
    bounds: Option<Rect>,
    z_order: usize,
    languages: &[String],
    state: &mut ParseState,
) -> Result<BlockNode, ConversionError> {
    let kind = paragraphs[*index]
        .bullet
        .ok_or_else(|| malformed(Some(part), "list builder started at a plain paragraph"))?;
    let start = paragraphs[*index].start;
    let mut items = Vec::<ListItem>::new();
    while *index < paragraphs.len() {
        let paragraph_level = paragraphs[*index].level;
        let Some(paragraph_kind) = paragraphs[*index].bullet else { break };
        if paragraph_level < level || (paragraph_level == level && paragraph_kind != kind) {
            break;
        }
        if paragraph_level > level {
            let last = items
                .last_mut()
                .ok_or_else(|| malformed(Some(part), "nested list lacks a parent item"))?;
            let nested = build_list_level(
                paragraphs,
                index,
                paragraph_level,
                part,
                slide,
                bounds,
                z_order,
                languages,
                state,
            )?;
            last.blocks.try_reserve(1).map_err(|error| {
                limit("max_memory_bytes", format!("cannot reserve nested list: {error}"))
            })?;
            last.blocks.push(nested);
            continue;
        }
        state.add_inlines(paragraphs[*index].text.len())?;
        let text = std::mem::take(&mut paragraphs[*index].text);
        let numbering = paragraphs[*index].numbering.take();
        let paragraph = state.node(
            Block::Paragraph(text),
            part,
            slide,
            bounds,
            Some(z_order),
            Some(languages),
        )?;
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(2).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve list item blocks: {error}"))
        })?;
        blocks.push(paragraph);
        items.try_reserve(1).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve list item: {error}"))
        })?;
        items.push(ListItem {
            checked: None,
            marker_label: Some(list_marker(level, kind, numbering.as_deref())?),
            blocks,
        });
        *index += 1;
    }
    state.node(
        Block::List { kind, start, items },
        part,
        slide,
        bounds,
        Some(z_order),
        Some(languages),
    )
}

fn list_marker(
    level: u8,
    kind: ListKind,
    numbering: Option<&str>,
) -> Result<String, ConversionError> {
    let capacity = 8_usize
        .checked_add(numbering.map_or(0, |value| value.len().saturating_add(8)))
        .ok_or_else(|| limit("max_memory_bytes", "list marker capacity overflow"))?;
    let mut marker = String::new();
    marker.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve list marker: {error}"))
    })?;
    write!(marker, "level:{level}")
        .map_err(|_| malformed(None, "cannot format list level marker"))?;
    if let Some(numbering) = numbering {
        marker.push_str(match kind {
            ListKind::Bullet => ";character:",
            ListKind::Ordered => ";scheme:",
            ListKind::Task => ";marker:",
        });
        marker.push_str(numbering);
    }
    Ok(marker)
}
