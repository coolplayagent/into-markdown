//! Unified embedded-visual OCR between container conversion and Markdown rendering.

use crate::image_converter::{decode, encode, envelope, format};
use image::{AnimationDecoder, ImageDecoder};
use into_markdown_core::{
    AssetId, Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, ConverterOutput,
    Diagnostic, DiagnosticSeverity, EnrichmentPlan, ExecutionContext, Inline, InputFormat, NodeId,
    OcrInputIdentity, OcrPolicy, OutputEnricher, Provenance, ResourceReservation, Services,
    SourceLocator, estimate_validation_working_set,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

const PROVIDER: &str = "builtin.enricher.embedded-visual-ocr";
const UNSUPPORTED_CODE: &str = "embeddedVisualOcr.unsupportedVisual";

/// Default post-converter enrichment for locally extracted raster assets.
#[derive(Debug, Default)]
pub struct EmbeddedVisualOcrEnricher;

impl OutputEnricher for EmbeddedVisualOcrEnricher {
    fn id(&self) -> &'static str {
        PROVIDER
    }

    fn planned_enrichment_bytes(
        &self,
        output: &ConverterOutput,
        _converter_id: &str,
        input_format: InputFormat,
        options: &ConversionOptions,
        services: &Services,
        context: &ExecutionContext,
    ) -> Result<EnrichmentPlan, ConversionError> {
        plan_enrichment(output, input_format, options, services, context)
    }

    fn enrich<'a>(
        &'a self,
        output: ConverterOutput,
        _converter_id: &'a str,
        format: InputFormat,
        options: &'a ConversionOptions,
        services: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { enrich(output, format, options, services, context).await })
    }
}

fn checked_add(total: &mut u64, bytes: u64, detail: &'static str) -> Result<(), ConversionError> {
    *total = total.checked_add(bytes).ok_or_else(|| resource("max_memory_bytes", detail))?;
    Ok(())
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 24 {
        return Some((
            u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        ));
    }
    if (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) && bytes.len() >= 10 {
        return Some((
            u32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
            u32::from(u16::from_le_bytes([bytes[8], bytes[9]])),
        ));
    }
    if bytes.starts_with(b"BM") && bytes.len() >= 26 {
        let width = i32::from_le_bytes(bytes[18..22].try_into().ok()?).unsigned_abs();
        let height = i32::from_le_bytes(bytes[22..26].try_into().ok()?).unsigned_abs();
        return Some((width, height));
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        match bytes.get(12..16)? {
            b"VP8X" if bytes.len() >= 30 => {
                let width = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]) + 1;
                let height = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]) + 1;
                return Some((width, height));
            }
            b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2f => {
                let width = 1 + u32::from(bytes[21]) + (u32::from(bytes[22] & 0x3f) << 8);
                let height = 1
                    + u32::from(bytes[22] >> 6)
                    + (u32::from(bytes[23]) << 2)
                    + (u32::from(bytes[24] & 0x0f) << 10);
                return Some((width, height));
            }
            b"VP8 " if bytes.len() >= 30 && bytes[23..26] == [0x9d, 0x01, 0x2a] => {
                let width = u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff);
                let height = u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff);
                return Some((width, height));
            }
            _ => {}
        }
    }
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut offset = 2_usize;
        while offset.checked_add(9)? <= bytes.len() {
            if bytes[offset] != 0xff {
                offset += 1;
                continue;
            }
            let marker = bytes[offset + 1];
            offset += 2;
            if matches!(marker, 0xd8 | 0xd9 | 0x01) {
                continue;
            }
            let length =
                usize::from(u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?));
            if length < 2 || offset.checked_add(length)? > bytes.len() {
                return None;
            }
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                let height =
                    u32::from(u16::from_be_bytes(bytes[offset + 3..offset + 5].try_into().ok()?));
                let width =
                    u32::from(u16::from_be_bytes(bytes[offset + 5..offset + 7].try_into().ok()?));
                return Some((width, height));
            }
            offset += length;
        }
    }
    tiff_dimensions(bytes)
}

fn tiff_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let (little, big_tiff) = match bytes.get(..4)? {
        b"II*\0" => (true, false),
        b"MM\0*" => (false, false),
        b"II+\0" => (true, true),
        b"MM\0+" => (false, true),
        _ => return None,
    };
    let read_u16 = |slice: &[u8]| {
        let value: [u8; 2] = slice.try_into().ok()?;
        Some(if little { u16::from_le_bytes(value) } else { u16::from_be_bytes(value) })
    };
    let read_u32 = |slice: &[u8]| {
        let value: [u8; 4] = slice.try_into().ok()?;
        Some(if little { u32::from_le_bytes(value) } else { u32::from_be_bytes(value) })
    };
    let read_u64 = |slice: &[u8]| {
        let value: [u8; 8] = slice.try_into().ok()?;
        Some(if little { u64::from_le_bytes(value) } else { u64::from_be_bytes(value) })
    };
    let (ifd, entries, entries_start, entry_size) = if big_tiff {
        if read_u16(bytes.get(4..6)?)? != 8 || read_u16(bytes.get(6..8)?)? != 0 {
            return None;
        }
        let ifd = usize::try_from(read_u64(bytes.get(8..16)?)?).ok()?;
        let entries = usize::try_from(read_u64(bytes.get(ifd..ifd.checked_add(8)?)?)?).ok()?;
        (ifd, entries, 8_usize, 20_usize)
    } else {
        let ifd = usize::try_from(read_u32(bytes.get(4..8)?)?).ok()?;
        let entries = usize::from(read_u16(bytes.get(ifd..ifd.checked_add(2)?)?)?);
        (ifd, entries, 2_usize, 12_usize)
    };
    let mut width = None;
    let mut height = None;
    for index in 0..entries {
        let start = ifd.checked_add(entries_start)?.checked_add(index.checked_mul(entry_size)?)?;
        let entry = bytes.get(start..start.checked_add(entry_size)?)?;
        let tag = read_u16(&entry[..2])?;
        if !matches!(tag, 256 | 257) {
            continue;
        }
        let kind = read_u16(&entry[2..4])?;
        let count =
            if big_tiff { read_u64(&entry[4..12])? } else { u64::from(read_u32(&entry[4..8])?) };
        if count != 1 {
            return None;
        }
        let value_offset = if big_tiff { 12 } else { 8 };
        let value = match (kind, big_tiff) {
            (3, _) => u32::from(read_u16(&entry[value_offset..value_offset + 2])?),
            (4, _) => read_u32(&entry[value_offset..value_offset + 4])?,
            (16, true) => u32::try_from(read_u64(&entry[value_offset..value_offset + 8])?).ok()?,
            _ => return None,
        };
        if tag == 256 {
            width = Some(value);
        } else {
            height = Some(value);
        }
    }
    Some((width?, height?))
}

/// Allocation-free with respect to image pixels: only envelope dimensions and
/// provider-declared bounds are inspected before the engine reserves this peak.
#[allow(clippy::too_many_lines)]
fn plan_enrichment(
    output: &ConverterOutput,
    input_format: InputFormat,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<EnrichmentPlan, ConversionError> {
    if options.ocr.policy == OcrPolicy::Off
        || input_format == InputFormat::Image
        || !eligible_container(input_format)
    {
        return Ok(EnrichmentPlan::Skip);
    }
    let mut raster_references = 0_u32;
    for_each_visual_reference(&output.document.blocks, context, &mut |asset_id| {
        let Some(asset) = find_asset(&output.assets, asset_id, context)? else {
            return Err(ConversionError::Internal {
                detail: format!("image node references missing asset {}", asset_id.0),
            });
        };
        if asset.bytes.is_empty() || asset.external_uri.is_some() || !supported_raster(asset) {
            return Ok(());
        }
        if auto_excluded_raster(asset, options, context)? {
            return Ok(());
        }
        raster_references = raster_references.checked_add(1).ok_or_else(|| {
            resource("max_archive_entries", "embedded visual reference count is not representable")
        })?;
        if raster_references > options.limits.max_archive_entries {
            return Err(resource(
                "max_archive_entries",
                "embedded visual references exceed the request limit",
            ));
        }
        Ok(())
    })?;
    if raster_references == 0 {
        return Ok(EnrichmentPlan::Skip);
    }
    let mut references = 0_u32;
    let mut total = 0_u64;
    for_each_visual_reference(&output.document.blocks, context, &mut |asset_id| {
        let Some(asset) = find_asset(&output.assets, asset_id, context)? else {
            return Err(ConversionError::Internal {
                detail: format!("image node references missing asset {}", asset_id.0),
            });
        };
        if asset.bytes.is_empty()
            || asset.external_uri.is_some()
            || !supported_raster(asset)
            || candidate_dimensions_for_policy(asset, options, context)?.is_none()
        {
            return Ok(());
        }
        references = references.checked_add(1).ok_or_else(|| {
            resource("max_archive_entries", "eligible visual reference count is not representable")
        })?;
        // Each eligible reference can clone OCR IR, evidence, and diagnostics,
        // even when its source bytes share one provider request.
        checked_add(&mut total, 128 * 1024, "embedded OCR reference plan overflow")?;
        Ok(())
    })?;
    if references == 0 {
        return Ok(EnrichmentPlan::Skip);
    }
    let mut candidates = 0_u32;
    let mut candidate_bytes = 0_u64;
    let existing_nodes = count_document_nodes(&output.document.blocks, context)?;
    let mut planned_added_nodes = 0_usize;
    let mut provider_working_peak = 0_u64;
    for (index, asset) in output.assets.iter().enumerate() {
        context.checkpoint()?;
        if asset.bytes.is_empty()
            || asset.external_uri.is_some()
            || !supported_raster(asset)
            || !has_visual_reference(&output.document.blocks, &asset.id, context)?
        {
            continue;
        }
        if candidate_dimensions_for_policy(asset, options, context)?.is_none() {
            continue;
        }
        candidate_bytes = candidate_bytes
            .checked_add(u64::try_from(asset.bytes.len()).map_err(|_| {
                resource("max_total_asset_bytes", "embedded visual byte count is not representable")
            })?)
            .ok_or_else(|| resource("max_total_asset_bytes", "embedded visual bytes overflow"))?;
        if candidate_bytes > options.limits.max_total_asset_bytes {
            return Err(resource(
                "max_total_asset_bytes",
                "embedded visual bytes exceed the request limit",
            ));
        }
        if first_referenced_asset_with_bytes(output, index, context)? {
            candidates = candidates
                .checked_add(1)
                .ok_or_else(|| resource("max_pages", "embedded OCR candidate count overflow"))?;
            if candidates > options.limits.max_pages {
                return Err(resource(
                    "max_pages",
                    "embedded OCR candidate requests exceed the request limit",
                ));
            }
        }
    }
    if candidates == 0 {
        return Ok(EnrichmentPlan::Skip);
    }

    // Only consult provider plans after every knowable input count and envelope
    // bound has passed, so a provider cannot observe a request that preflight
    // must reject.
    // Account hash/map work once per eligible AssetId, but normalization and
    // provider output once per unique byte identity.
    for (index, asset) in output.assets.iter().enumerate() {
        context.checkpoint()?;
        if asset.bytes.is_empty()
            || asset.external_uri.is_some()
            || !supported_raster(asset)
            || !has_visual_reference(&output.document.blocks, &asset.id, context)?
        {
            continue;
        }
        checked_add(
            &mut total,
            u64::try_from(asset.bytes.len())
                .map_err(|_| resource("max_memory_bytes", "hash/cache plan overflow"))?,
            "hash/cache plan overflow",
        )?;
        if !first_referenced_asset_with_bytes(output, index, context)? {
            continue;
        }
        let dimensions = candidate_dimensions_for_policy(asset, options, context)?;
        let normalized_peak = if let Some((width, height)) = dimensions {
            u64::from(width)
                .checked_mul(u64::from(height))
                .and_then(|pixels| pixels.checked_mul(32))
                .and_then(|bytes| bytes.checked_add(64 * 1024))
                .ok_or_else(|| resource("max_memory_bytes", "normalization plan overflow"))?
        } else {
            64 * 1024
        };
        checked_add(&mut total, normalized_peak, "normalization working-set plan overflow")?;
        let (provider_bound, provider_working, provider_regions) =
            match (services.ocr.as_deref(), dimensions) {
                (_, None) => (0, 0, 0),
                (Some(engine), Some((width, height))) => {
                    match engine.planned_normalized_png_output(width, height, options, context) {
                        Ok(plan) => (
                            plan.max_retained_bytes(),
                            plan.max_working_bytes(),
                            plan.max_regions(),
                        ),
                        Err(ConversionError::ComponentUnavailable { .. })
                            if options.ocr.policy == OcrPolicy::Auto =>
                        {
                            (0, 0, 0)
                        }
                        Err(error) => return Err(error),
                    }
                }
                (None, Some(_)) if options.ocr.policy == OcrPolicy::Auto => (0, 0, 0),
                (None, Some(_)) => {
                    return Err(ConversionError::ComponentUnavailable {
                        component: "ocr".into(),
                        detail: "no OCR engine is configured".into(),
                    });
                }
            };
        provider_working_peak = provider_working_peak.max(provider_working);
        let reference_copies = visual_reference_count_for_bytes(output, &asset.bytes, context)?;
        let added_nodes = usize::try_from(provider_regions)
            .unwrap_or(usize::MAX)
            .checked_mul(usize::try_from(reference_copies).unwrap_or(usize::MAX))
            .ok_or_else(|| resource("documentNodes", "embedded OCR node preflight overflow"))?;
        planned_added_nodes = planned_added_nodes
            .checked_add(added_nodes)
            .ok_or_else(|| resource("documentNodes", "embedded OCR node preflight overflow"))?;
        if existing_nodes
            .checked_add(planned_added_nodes)
            .ok_or_else(|| resource("documentNodes", "embedded OCR node preflight overflow"))?
            > into_markdown_core::MAX_DOCUMENT_NODES
        {
            return Err(resource(
                "documentNodes",
                "embedded OCR output can exceed the document node limit",
            ));
        }
        let asset_copies = referenced_asset_count_for_bytes(output, &asset.bytes, context)?;
        let retained_copies = reference_copies
            .checked_add(asset_copies)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| resource("max_memory_bytes", "OCR retained-copy count overflow"))?;
        checked_add(
            &mut total,
            provider_bound
                .checked_mul(retained_copies)
                .ok_or_else(|| resource("max_memory_bytes", "OCR output plan overflow"))?,
            "OCR output plan overflow",
        )?;
    }
    checked_add(&mut total, provider_working_peak, "OCR provider working-set plan overflow")?;
    let (occupied_id_bytes, collision_scratch) =
        planned_node_id_working_set(&output.document.blocks, context)?;
    checked_add(&mut total, occupied_id_bytes, "occupied OCR node ID plan overflow")?;
    checked_add(&mut total, collision_scratch, "OCR node ID collision scratch overflow")?;
    checked_add(
        &mut total,
        estimate_validation_working_set(&output.document, &output.assets, &output.diagnostics)?,
        "embedded OCR validation plan overflow",
    )?;
    Ok(EnrichmentPlan::Reserve(total))
}

fn candidate_dimensions_for_policy(
    asset: &into_markdown_core::Asset,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Option<(u32, u32)>, ConversionError> {
    match validated_candidate_dimensions(asset, options, context) {
        Ok(dimensions) => Ok(Some(dimensions)),
        Err(error) if options.ocr.policy == OcrPolicy::Auto && auto_skippable_raster(&error) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn auto_excluded_raster(
    asset: &into_markdown_core::Asset,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    if options.ocr.policy != OcrPolicy::Auto {
        return Ok(false);
    }
    // Classification must not consume OCR reference limits: otherwise an
    // animated/multi-page raster that Auto excludes could fail merely because
    // the request deliberately set those OCR-stage limits low.
    let mut classification_options = options.clone();
    classification_options.limits.max_archive_entries = u32::MAX;
    Ok(candidate_dimensions_for_policy(asset, &classification_options, context)?.is_none())
}

fn count_document_nodes(
    nodes: &[BlockNode],
    context: &ExecutionContext,
) -> Result<usize, ConversionError> {
    let mut total = 0_usize;
    for node in nodes {
        context.checkpoint()?;
        total = total
            .checked_add(1)
            .ok_or_else(|| resource("documentNodes", "document node count overflow"))?;
        let nested = match &node.block {
            Block::List { items, .. } => {
                let mut count = 0_usize;
                for item in items {
                    count = count
                        .checked_add(count_document_nodes(&item.blocks, context)?)
                        .ok_or_else(|| resource("documentNodes", "document node count overflow"))?;
                }
                count
            }
            Block::Table { rows, .. } => {
                let mut count = 0_usize;
                for row in rows {
                    for cell in &row.cells {
                        count = count
                            .checked_add(count_document_nodes(&cell.blocks, context)?)
                            .ok_or_else(|| {
                                resource("documentNodes", "document node count overflow")
                            })?;
                    }
                }
                count
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => count_document_nodes(blocks, context)?,
            _ => 0,
        };
        total = total
            .checked_add(nested)
            .ok_or_else(|| resource("documentNodes", "document node count overflow"))?;
    }
    Ok(total)
}

fn visual_reference_count_for_bytes(
    output: &ConverterOutput,
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    let mut count = 0_u64;
    for_each_visual_reference(&output.document.blocks, context, &mut |asset_id| {
        let Some(asset) = find_asset(&output.assets, asset_id, context)? else {
            return Err(ConversionError::Internal {
                detail: format!("image node references missing asset {}", asset_id.0),
            });
        };
        if !asset.bytes.is_empty()
            && asset.external_uri.is_none()
            && supported_raster(asset)
            && asset.bytes == bytes
        {
            count = count.checked_add(1).ok_or_else(|| {
                resource("max_archive_entries", "embedded visual reference count overflow")
            })?;
        }
        Ok(())
    })?;
    Ok(count)
}

fn referenced_asset_count_for_bytes(
    output: &ConverterOutput,
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    let mut count = 0_u64;
    for asset in &output.assets {
        context.checkpoint()?;
        if asset.bytes == bytes
            && !asset.bytes.is_empty()
            && asset.external_uri.is_none()
            && supported_raster(asset)
            && has_visual_reference(&output.document.blocks, &asset.id, context)?
        {
            count = count.checked_add(1).ok_or_else(|| {
                resource("max_archive_entries", "embedded visual asset count overflow")
            })?;
        }
    }
    Ok(count)
}

fn planned_node_id_working_set(
    nodes: &[BlockNode],
    context: &ExecutionContext,
) -> Result<(u64, u64), ConversionError> {
    let (total, maximum) = planned_node_id_inventory(nodes, context)?;
    Ok((
        total,
        maximum
            .checked_add(64)
            .ok_or_else(|| resource("max_memory_bytes", "OCR node ID scratch plan overflow"))?,
    ))
}

fn planned_node_id_inventory(
    nodes: &[BlockNode],
    context: &ExecutionContext,
) -> Result<(u64, u64), ConversionError> {
    let mut total = 0_u64;
    let mut maximum = 0_u64;
    for node in nodes {
        context.checkpoint()?;
        let length = u64::try_from(node.id.0.len())
            .map_err(|_| resource("max_memory_bytes", "node ID length is not representable"))?;
        checked_add(
            &mut total,
            length.checked_add(128).ok_or_else(|| {
                resource("max_memory_bytes", "occupied node ID entry plan overflow")
            })?,
            "occupied node ID set plan overflow",
        )?;
        maximum = maximum.max(length);
        let nested = match &node.block {
            Block::List { items, .. } => {
                let mut nested_total = 0_u64;
                let mut nested_max = 0_u64;
                for item in items {
                    let (item_total, item_max) = planned_node_id_inventory(&item.blocks, context)?;
                    checked_add(
                        &mut nested_total,
                        item_total,
                        "occupied node ID set plan overflow",
                    )?;
                    nested_max = nested_max.max(item_max);
                }
                (nested_total, nested_max)
            }
            Block::Table { rows, .. } => {
                let mut nested_total = 0_u64;
                let mut nested_max = 0_u64;
                for row in rows {
                    for cell in &row.cells {
                        let (cell_total, cell_max) =
                            planned_node_id_inventory(&cell.blocks, context)?;
                        checked_add(
                            &mut nested_total,
                            cell_total,
                            "occupied node ID set plan overflow",
                        )?;
                        nested_max = nested_max.max(cell_max);
                    }
                }
                (nested_total, nested_max)
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => planned_node_id_inventory(blocks, context)?,
            _ => (0, 0),
        };
        checked_add(&mut total, nested.0, "occupied node ID set plan overflow")?;
        maximum = maximum.max(nested.1);
    }
    Ok((total, maximum))
}

fn has_visual_reference(
    nodes: &[BlockNode],
    asset_id: &AssetId,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let mut found = false;
    for_each_visual_reference(nodes, context, &mut |candidate| {
        if candidate == asset_id {
            found = true;
        }
        Ok(())
    })?;
    Ok(found)
}

/// Returns true for the first referenced eligible `AssetId` carrying these exact
/// bytes. Exact byte equality is the OCR input identity; no set allocation is
/// needed before the engine reserves the enrichment plan.
fn first_referenced_asset_with_bytes(
    output: &ConverterOutput,
    index: usize,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let asset = &output.assets[index];
    for prior in &output.assets[..index] {
        context.checkpoint()?;
        if prior.bytes == asset.bytes
            && !prior.bytes.is_empty()
            && prior.external_uri.is_none()
            && supported_raster(prior)
            && has_visual_reference(&output.document.blocks, &prior.id, context)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validated_candidate_dimensions(
    asset: &into_markdown_core::Asset,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(u32, u32), ConversionError> {
    if asset.bytes.starts_with(b"GIF87a") || asset.bytes.starts_with(b"GIF89a") {
        if !asset.media_type.eq_ignore_ascii_case("image/gif") {
            return Err(ConversionError::Malformed {
                part: asset.filename.clone(),
                detail: "embedded raster media type disagrees with its GIF envelope".into(),
            });
        }
        let ((width, height), frames) = crate::epub::image::preflight_info(
            &asset.bytes,
            "image/gif",
            asset.filename.as_deref().unwrap_or("embedded GIF"),
            context,
        )?;
        if frames != 1 {
            return Err(ConversionError::Unsupported {
                detail: "animated or multi-page embedded raster is not eligible for OCR".into(),
            });
        }
        let decoded_bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| resource("max_decompressed_bytes", "GIF pixel size overflow"))?;
        if width == 0 || height == 0 || decoded_bytes > options.limits.max_decompressed_bytes {
            return Err(resource("max_decompressed_bytes", "GIF dimensions exceed request limits"));
        }
        return Ok((width, height));
    }
    let raster =
        format::detect(&asset.bytes, context)?.ok_or_else(|| ConversionError::Malformed {
            part: asset.filename.clone(),
            detail: "declared raster asset has an unrecognized envelope".into(),
        })?;
    let declared_matches = asset.media_type.eq_ignore_ascii_case(raster.media_type())
        || raster == format::RasterFormat::Jpeg
            && asset.media_type.eq_ignore_ascii_case("image/jpg")
        || raster == format::RasterFormat::Tiff
            && asset.media_type.eq_ignore_ascii_case("image/x-tiff");
    if !declared_matches {
        return Err(ConversionError::Malformed {
            part: asset.filename.clone(),
            detail: "embedded raster media type disagrees with its byte envelope".into(),
        });
    }
    let summary = envelope::preflight_validate(raster, &asset.bytes, &options.limits, context)?;
    if summary.frames != 1 || summary.animated {
        return Err(ConversionError::Unsupported {
            detail: "animated or multi-page embedded raster is not eligible for OCR".into(),
        });
    }
    let (width, height) =
        image_dimensions(&asset.bytes).ok_or_else(|| ConversionError::Malformed {
            part: asset.filename.clone(),
            detail: "embedded raster dimensions are unavailable".into(),
        })?;
    let decoded_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| resource("max_decompressed_bytes", "embedded visual pixel size overflow"))?;
    if decoded_bytes > options.limits.max_decompressed_bytes {
        return Err(resource(
            "max_decompressed_bytes",
            "embedded visual pixels exceed the request limit",
        ));
    }
    Ok((width, height))
}

fn find_asset<'a>(
    assets: &'a [into_markdown_core::Asset],
    asset_id: &AssetId,
    context: &ExecutionContext,
) -> Result<Option<&'a into_markdown_core::Asset>, ConversionError> {
    for (index, asset) in assets.iter().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        if asset.id == *asset_id {
            return Ok(Some(asset));
        }
    }
    Ok(None)
}

fn for_each_visual_reference(
    nodes: &[BlockNode],
    context: &ExecutionContext,
    visit: &mut impl FnMut(&AssetId) -> Result<(), ConversionError>,
) -> Result<(), ConversionError> {
    for node in nodes {
        context.checkpoint()?;
        match &node.block {
            Block::Image { asset, .. } => visit(asset)?,
            Block::List { items, .. } => {
                for item in items {
                    for_each_visual_reference(&item.blocks, context, visit)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        for_each_visual_reference(&cell.blocks, context, visit)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                for_each_visual_reference(blocks, context, visit)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone)]
struct VisualRef {
    asset: AssetId,
    provenance: Provenance,
}

#[derive(Clone, Default)]
struct CachedContribution {
    nodes: Vec<BlockNode>,
    diagnostics: Vec<Diagnostic>,
}

struct NormalizedImage {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    _leases: Vec<ResourceReservation>,
}

#[allow(clippy::too_many_lines)] // Discovery, OCR, and publication share one transaction.
async fn enrich(
    mut output: ConverterOutput,
    input_format: InputFormat,
    options: &ConversionOptions,
    services: &Services,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    if options.ocr.policy == OcrPolicy::Off
        || input_format == InputFormat::Image
        || !eligible_container(input_format)
    {
        return Ok(output);
    }
    let mut references = Vec::new();
    collect_references(&output.document.blocks, &mut references, context)?;
    if references.is_empty() {
        return Ok(output);
    }

    let mut assets = BTreeMap::new();
    for (index, asset) in output.assets.iter().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        assets.insert(asset.id.clone(), asset.clone());
    }
    let mut eligible_reference_count = 0_u32;
    for (index, reference) in references.iter().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        let Some(asset) = assets.get(&reference.asset) else {
            return Err(ConversionError::Internal {
                detail: format!("image node references missing asset {}", reference.asset.0),
            });
        };
        if asset.bytes.is_empty() || asset.external_uri.is_some() || !supported_raster(asset) {
            continue;
        }
        if auto_excluded_raster(asset, options, context)? {
            continue;
        }
        eligible_reference_count = eligible_reference_count.checked_add(1).ok_or_else(|| {
            resource("max_archive_entries", "embedded visual reference count is not representable")
        })?;
        if eligible_reference_count > options.limits.max_archive_entries {
            return Err(resource(
                "max_archive_entries",
                "embedded visual references exceed the request limit",
            ));
        }
    }
    let mut hashes_by_asset = BTreeMap::new();
    let mut unique = BTreeMap::<[u8; 32], AssetId>::new();
    let mut referenced_assets = BTreeSet::new();
    let mut compressed_bytes = 0_u64;
    for (index, reference) in references.iter().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        let Some(asset) = assets.get(&reference.asset) else {
            return Err(ConversionError::Internal {
                detail: format!("image node references missing asset {}", reference.asset.0),
            });
        };
        if asset.bytes.is_empty() || asset.external_uri.is_some() || !supported_raster(asset) {
            continue;
        }
        if auto_excluded_raster(asset, options, context)? {
            continue;
        }
        if !referenced_assets.insert(reference.asset.clone()) {
            continue;
        }
        compressed_bytes = compressed_bytes
            .checked_add(u64::try_from(asset.bytes.len()).map_err(|_| {
                resource("max_total_asset_bytes", "embedded visual byte count is not representable")
            })?)
            .ok_or_else(|| resource("max_total_asset_bytes", "embedded visual bytes overflow"))?;
        if compressed_bytes > options.limits.max_total_asset_bytes {
            return Err(resource(
                "max_total_asset_bytes",
                "embedded visual bytes exceed the request limit",
            ));
        }
        let digest = checkpointed_sha256(&asset.bytes, context)?;
        hashes_by_asset.insert(reference.asset.clone(), digest);
        unique.entry(digest).or_insert_with(|| reference.asset.clone());
    }

    let unique_count = u32::try_from(unique.len())
        .map_err(|_| resource("max_pages", "OCR request count is not representable"))?;
    if unique_count > options.limits.max_pages {
        return Err(resource("max_pages", "embedded OCR requests exceed the request limit"));
    }

    let mut cache = BTreeMap::<[u8; 32], CachedContribution>::new();
    for (ordinal, (digest, asset_id)) in unique.into_iter().enumerate() {
        context.checkpoint()?;
        let asset = assets.get(&asset_id).ok_or_else(|| ConversionError::Internal {
            detail: "embedded OCR asset inventory changed during enrichment".into(),
        })?;
        let normalized = match normalize(asset, options, context) {
            Ok(value) => value,
            Err(error)
                if options.ocr.policy == OcrPolicy::Auto
                    && auto_degradable_normalization(&error) =>
            {
                cache.insert(
                    digest,
                    CachedContribution {
                        nodes: Vec::new(),
                        diagnostics: vec![visual_diagnostic(None, error.to_string())],
                    },
                );
                continue;
            }
            Err(error) => return Err(error),
        };
        let identity = OcrInputIdentity::try_new(
            Sha256::digest(&normalized.bytes).into(),
            normalized.width,
            normalized.height,
            0,
        )?;
        let normalized_plan = match services.ocr.as_deref() {
            Some(engine) => engine.planned_normalized_png_output(
                normalized.width,
                normalized.height,
                options,
                context,
            ),
            None => Err(ConversionError::ComponentUnavailable {
                component: "ocr".into(),
                detail: "no OCR engine is configured".into(),
            }),
        };
        match normalized_plan {
            Ok(_) => {}
            Err(error)
                if options.ocr.policy == OcrPolicy::Auto
                    && matches!(error, ConversionError::ComponentUnavailable { .. }) =>
            {
                cache.insert(
                    digest,
                    CachedContribution {
                        nodes: Vec::new(),
                        diagnostics: vec![visual_diagnostic(None, error.to_string())],
                    },
                );
                continue;
            }
            Err(error) => return Err(error),
        }
        let mut contribution = crate::image_converter::ocr::recognize_for_input(
            &normalized.bytes,
            u32::try_from(ordinal + 1)
                .map_err(|_| resource("max_pages", "OCR page ordinal overflow"))?,
            normalized.width,
            normalized.height,
            identity,
            options,
            services,
            context,
        )
        .await?;
        if let Some(memory) = contribution.memory.take() {
            output.attach_memory_reservation(context, memory)?;
        }
        cache.insert(
            digest,
            CachedContribution { nodes: contribution.nodes, diagnostics: contribution.diagnostics },
        );
    }

    let mut contributions_by_asset = BTreeMap::new();
    for (index, (asset, digest)) in hashes_by_asset.into_iter().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        if let Some(contribution) = cache.get(&digest) {
            contributions_by_asset.insert(asset, contribution.clone());
        }
    }
    let mut occupied_ids = BTreeSet::new();
    collect_node_ids(&output.document.blocks, &mut occupied_ids, context)?;
    output.document.blocks = rebuild_nodes(
        std::mem::take(&mut output.document.blocks),
        &contributions_by_asset,
        &mut occupied_ids,
        context,
    )?;
    for (reference_index, reference) in references.iter().enumerate() {
        if reference_index % 256 == 0 {
            context.checkpoint()?;
        }
        if let Some(contribution) = contributions_by_asset.get(&reference.asset) {
            for (diagnostic_index, diagnostic) in contribution.diagnostics.iter().enumerate() {
                if diagnostic_index % 256 == 0 {
                    context.checkpoint()?;
                }
                let mut diagnostic = diagnostic.clone();
                diagnostic.locator = Some(remapped_locator(&reference.provenance.locator));
                output.diagnostics.push(diagnostic);
            }
        }
    }
    if input_format == InputFormat::Pdf && !contributions_by_asset.is_empty() {
        output = crate::pdf_ocr::reconstruct_enriched_pdf(output, options, context)?;
    }
    Ok(output)
}

fn auto_degradable_normalization(error: &ConversionError) -> bool {
    matches!(error, ConversionError::ComponentUnavailable { .. }) || auto_skippable_raster(error)
}

fn auto_skippable_raster(error: &ConversionError) -> bool {
    matches!(
        error,
        ConversionError::Unsupported { detail }
            if detail == "animated or multi-page embedded raster is not eligible for OCR"
                || detail == "animated embedded GIF is not eligible for OCR"
    )
}

fn eligible_container(format: InputFormat) -> bool {
    matches!(
        format,
        InputFormat::Pdf
            | InputFormat::Doc
            | InputFormat::Docx
            | InputFormat::Ppt
            | InputFormat::Pptx
            | InputFormat::Xls
            | InputFormat::Xlsx
            | InputFormat::Odt
            | InputFormat::Ods
            | InputFormat::Odp
            | InputFormat::Rtf
            | InputFormat::Epub
            | InputFormat::Html
            | InputFormat::Ipynb
            | InputFormat::Zip
            | InputFormat::OutlookMsg
    )
}

fn supported_raster(asset: &into_markdown_core::Asset) -> bool {
    [
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/gif",
        "image/webp",
        "image/bmp",
        "image/tiff",
        "image/x-tiff",
    ]
    .iter()
    .any(|candidate| asset.media_type.eq_ignore_ascii_case(candidate))
}

fn normalize(
    asset: &into_markdown_core::Asset,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<NormalizedImage, ConversionError> {
    if asset.bytes.starts_with(b"GIF87a") || asset.bytes.starts_with(b"GIF89a") {
        if asset.media_type.eq_ignore_ascii_case("image/gif") {
            return normalize_gif(&asset.bytes, asset.filename.as_deref(), options, context);
        }
        return Err(ConversionError::Malformed {
            part: asset.filename.clone(),
            detail: "embedded raster media type disagrees with its GIF envelope".into(),
        });
    }
    let raster =
        format::detect(&asset.bytes, context)?.ok_or_else(|| ConversionError::Malformed {
            part: asset.filename.clone(),
            detail: "declared raster asset has an unrecognized envelope".into(),
        })?;
    let declared = asset.media_type.to_ascii_lowercase();
    let declared = match declared.as_str() {
        "image/jpg" => "image/jpeg",
        "image/x-tiff" => "image/tiff",
        value => value,
    };
    if raster.media_type() != declared {
        return Err(ConversionError::Malformed {
            part: asset.filename.clone(),
            detail: "embedded raster media type disagrees with its byte envelope".into(),
        });
    }
    let summary = envelope::validate(raster, &asset.bytes, &options.limits, context)?;
    if summary.frames != 1 || summary.animated {
        return Err(ConversionError::Unsupported {
            detail: "animated or multi-page embedded raster is not eligible for OCR".into(),
        });
    }
    let decoded = decode::decode(raster, &asset.bytes, summary, &options.limits, context)?;
    let frame = decoded.frames.first().ok_or_else(|| ConversionError::Malformed {
        part: asset.filename.clone(),
        detail: "embedded raster decoder returned no frame".into(),
    })?;
    if frame.pixels.pixels().all(|pixel| pixel.0[3] == 0) {
        return Err(ConversionError::Unsupported {
            detail: "fully transparent embedded raster is not eligible for OCR".into(),
        });
    }
    let width = frame.pixels.width();
    let height = frame.pixels.height();
    let encoded = encode::png(&frame.pixels, true, &options.limits, context)?;
    let (bytes, memory) = encoded.into_parts();
    Ok(NormalizedImage { bytes, width, height, _leases: vec![memory] })
}

fn normalize_gif(
    bytes: &[u8],
    part: Option<&str>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<NormalizedImage, ConversionError> {
    if bytes.len() < 10 {
        return Err(ConversionError::Malformed {
            part: None,
            detail: "embedded GIF header is truncated".into(),
        });
    }
    let width = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    let height = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    let working = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(20))
        .ok_or_else(|| resource("max_memory_bytes", "GIF working set overflow"))?;
    let decoded = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| resource("max_decompressed_bytes", "GIF decoded size overflow"))?;
    if width == 0 || height == 0 || decoded > options.limits.max_decompressed_bytes {
        return Err(resource("max_decompressed_bytes", "GIF dimensions exceed request limits"));
    }
    if working > options.limits.max_memory_bytes {
        return Err(resource("max_memory_bytes", "GIF working set exceeds request limits"));
    }
    crate::epub::image::validate(
        bytes,
        "image/gif",
        part.unwrap_or("embedded GIF"),
        &options.limits,
        context,
    )?;
    let memory = context.reserve_memory(working)?;
    let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(|error| {
        ConversionError::Malformed { part: None, detail: format!("invalid embedded GIF: {error}") }
    })?;
    if decoder.dimensions() != (width, height) {
        return Err(ConversionError::Malformed {
            part: None,
            detail: "GIF decoder dimensions disagree with the header".into(),
        });
    }
    let frames = decoder.into_frames().take(2).collect::<Result<Vec<_>, _>>().map_err(|error| {
        ConversionError::Malformed {
            part: None,
            detail: format!("invalid embedded GIF frames: {error}"),
        }
    })?;
    if frames.len() != 1 {
        return Err(ConversionError::Unsupported {
            detail: "animated embedded GIF is not eligible for OCR".into(),
        });
    }
    let pixels = frames.into_iter().next().expect("one frame").into_buffer();
    if pixels.pixels().all(|pixel| pixel.0[3] == 0) {
        return Err(ConversionError::Unsupported {
            detail: "fully transparent embedded GIF is not eligible for OCR".into(),
        });
    }
    let encoded = encode::png(&pixels, true, &options.limits, context)?;
    let (bytes, png_memory) = encoded.into_parts();
    Ok(NormalizedImage { bytes, width, height, _leases: vec![memory, png_memory] })
}

fn checkpointed_sha256(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<[u8; 32], ConversionError> {
    let mut digest = Sha256::new();
    for chunk in bytes.chunks(4096) {
        context.checkpoint()?;
        digest.update(chunk);
    }
    Ok(digest.finalize().into())
}

fn collect_references(
    nodes: &[BlockNode],
    output: &mut Vec<VisualRef>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for (index, node) in nodes.iter().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        match &node.block {
            Block::Image { asset, .. } => {
                output
                    .push(VisualRef { asset: asset.clone(), provenance: node.provenance.clone() });
            }
            Block::List { items, .. } => {
                for item in items {
                    collect_references(&item.blocks, output, context)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        collect_references(&cell.blocks, output, context)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => collect_references(blocks, output, context)?,
            _ => {}
        }
    }
    Ok(())
}

fn rebuild_nodes(
    nodes: Vec<BlockNode>,
    cache: &BTreeMap<AssetId, CachedContribution>,
    occupied_ids: &mut BTreeSet<NodeId>,
    context: &ExecutionContext,
) -> Result<Vec<BlockNode>, ConversionError> {
    let mut rebuilt = Vec::new();
    for (index, mut node) in nodes.into_iter().enumerate() {
        if index % 256 == 0 {
            context.checkpoint()?;
        }
        match &mut node.block {
            Block::List { items, .. } => {
                for item in items {
                    item.blocks = rebuild_nodes(
                        std::mem::take(&mut item.blocks),
                        cache,
                        occupied_ids,
                        context,
                    )?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &mut row.cells {
                        cell.blocks = rebuild_nodes(
                            std::mem::take(&mut cell.blocks),
                            cache,
                            occupied_ids,
                            context,
                        )?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                *blocks = rebuild_nodes(std::mem::take(blocks), cache, occupied_ids, context)?;
            }
            _ => {}
        }
        let contribution = match &node.block {
            Block::Image { asset, .. } => cache.get(asset).cloned(),
            _ => None,
        };
        let source_id = node.id.clone();
        let source_provenance = node.provenance.clone();
        rebuilt.push(node);
        if let Some(contribution) = contribution {
            for (ocr_index, template) in contribution.nodes.into_iter().enumerate() {
                if ocr_index % 256 == 0 {
                    context.checkpoint()?;
                }
                let fresh_id = fresh_ocr_node_id(&source_id, ocr_index + 1, occupied_ids, context)?;
                rebuilt.push(remap_ocr_node(template, fresh_id, &source_provenance, context)?);
            }
        }
    }
    Ok(rebuilt)
}

fn fresh_ocr_node_id(
    source_id: &NodeId,
    mut suffix: usize,
    occupied_ids: &mut BTreeSet<NodeId>,
    context: &ExecutionContext,
) -> Result<NodeId, ConversionError> {
    let mut collisions = 0_u64;
    loop {
        if collisions.is_multiple_of(256) {
            context.checkpoint()?;
        }
        let candidate = NodeId(format!("{}::ocr::{suffix}", source_id.0));
        if occupied_ids.insert(candidate.clone()) {
            return Ok(candidate);
        }
        collisions = collisions.checked_add(1).ok_or_else(|| {
            resource("max_archive_entries", "OCR node ID collision count overflow")
        })?;
        suffix = suffix
            .checked_add(1)
            .ok_or_else(|| resource("max_archive_entries", "OCR node ID suffix overflow"))?;
    }
}

fn remap_ocr_node(
    mut node: BlockNode,
    fresh_id: NodeId,
    source: &Provenance,
    context: &ExecutionContext,
) -> Result<BlockNode, ConversionError> {
    node.id = fresh_id;
    let ocr_locator = node.provenance.locator.clone();
    node.provenance.locator = remapped_ocr_locator(&source.locator, &ocr_locator);
    if let Block::Paragraph(inlines) = &mut node.block {
        for (inline_index, inline) in inlines.iter_mut().enumerate() {
            if inline_index % 256 == 0 {
                context.checkpoint()?;
            }
            if let Inline::OcrText { provenance, evidence, .. } = inline {
                let inline_locator = provenance.locator.clone();
                provenance.locator = remapped_ocr_locator(&source.locator, &inline_locator);
                evidence.page = source.locator.page.or(source.locator.slide).unwrap_or(1);
                if let Some((source_bounds, image_width, image_height)) =
                    coordinate_frame(&source.locator, &inline_locator)
                {
                    for (region_index, region) in evidence.regions.iter_mut().enumerate() {
                        if region_index % 256 == 0 {
                            context.checkpoint()?;
                        }
                        for (point_index, point) in region.polygon.iter_mut().enumerate() {
                            if point_index % 256 == 0 {
                                context.checkpoint()?;
                            }
                            point.x = source_bounds.x + point.x * source_bounds.width / image_width;
                            point.y =
                                source_bounds.y + point.y * source_bounds.height / image_height;
                        }
                    }
                }
                provenance.locator.bounds = evidence_bounds(&evidence.regions);
            }
        }
    }
    Ok(node)
}

fn evidence_bounds(
    regions: &[into_markdown_core::OcrSourceRegion],
) -> Option<into_markdown_core::Rect> {
    (!regions.is_empty()).then(|| {
        let minimum_x = regions
            .iter()
            .flat_map(|region| region.polygon.iter())
            .map(|point| point.x)
            .fold(f32::INFINITY, f32::min);
        let minimum_y = regions
            .iter()
            .flat_map(|region| region.polygon.iter())
            .map(|point| point.y)
            .fold(f32::INFINITY, f32::min);
        let maximum_x = regions
            .iter()
            .flat_map(|region| region.polygon.iter())
            .map(|point| point.x)
            .fold(f32::NEG_INFINITY, f32::max);
        let maximum_y = regions
            .iter()
            .flat_map(|region| region.polygon.iter())
            .map(|point| point.y)
            .fold(f32::NEG_INFINITY, f32::max);
        into_markdown_core::Rect {
            x: minimum_x,
            y: minimum_y,
            width: maximum_x - minimum_x,
            height: maximum_y - minimum_y,
        }
    })
}

fn collect_node_ids(
    nodes: &[BlockNode],
    output: &mut BTreeSet<NodeId>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    for node in nodes {
        context.checkpoint()?;
        output.insert(node.id.clone());
        match &node.block {
            Block::List { items, .. } => {
                for item in items {
                    collect_node_ids(&item.blocks, output, context)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        collect_node_ids(&cell.blocks, output, context)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => collect_node_ids(blocks, output, context)?,
            _ => {}
        }
    }
    Ok(())
}

fn coordinate_frame(
    source: &SourceLocator,
    ocr: &SourceLocator,
) -> Option<(into_markdown_core::Rect, f32, f32)> {
    let bounds = source.bounds?;
    let width = ocr.page_width?;
    let height = ocr.page_height?;
    (bounds.x.is_finite()
        && bounds.y.is_finite()
        && bounds.width.is_finite()
        && bounds.height.is_finite()
        && bounds.width > 0.0
        && bounds.height > 0.0
        && width.is_finite()
        && height.is_finite()
        && width > 0.0
        && height > 0.0)
        .then_some((bounds, width, height))
}

fn remapped_ocr_locator(source: &SourceLocator, ocr: &SourceLocator) -> SourceLocator {
    let mut locator = remapped_locator(source);
    locator.page = source.page.or(source.slide).or(Some(1));
    if let (Some((frame, width, height)), Some(bounds)) =
        (coordinate_frame(source, ocr), ocr.bounds)
    {
        locator.page_width = source.page_width.or(ocr.page_width);
        locator.page_height = source.page_height.or(ocr.page_height);
        locator.bounds = Some(into_markdown_core::Rect {
            x: frame.x + bounds.x * frame.width / width,
            y: frame.y + bounds.y * frame.height / height,
            width: bounds.width * frame.width / width,
            height: bounds.height * frame.height / height,
        });
    } else {
        locator.page_width = ocr.page_width;
        locator.page_height = ocr.page_height;
        locator.bounds = ocr.bounds;
    }
    locator
}

fn remapped_locator(source: &SourceLocator) -> SourceLocator {
    let mut locator = source.clone();
    locator.byte_start = None;
    locator.byte_end = None;
    locator.character_index = None;
    locator
}

fn visual_diagnostic(locator: Option<SourceLocator>, detail: String) -> Diagnostic {
    Diagnostic {
        code: UNSUPPORTED_CODE.into(),
        severity: DiagnosticSeverity::Warning,
        message: detail,
        locator,
    }
}

fn resource(limit: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit, detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Frame, ImageFormat, Rgba, RgbaImage, codecs::gif::GifEncoder};
    use into_markdown_core::{
        Asset, BoundOcrResult, CancellationToken, Document, ExecutionOptions, OcrEngine,
        OcrEvidenceStage, OcrEvidenceStep, OcrOutputPlan, OcrRecognition, OcrRegion, OcrRequest,
        OcrResult, ProvenanceKind,
    };
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    struct SourceBoundOcr {
        calls: AtomicUsize,
        plans: AtomicUsize,
        planned_bytes: u64,
        planned_working_bytes: u64,
        corrupt_identity: bool,
    }

    struct LegacyBoundOcr(AtomicUsize);

    struct EntryGuardOcr {
        plans: AtomicUsize,
        calls: AtomicUsize,
        regions: u32,
    }

    impl OcrEngine for EntryGuardOcr {
        fn id(&self) -> &'static str {
            "test.ocr.entry-guard"
        }

        fn recognize<'a>(
            &'a self,
            _: OcrRequest<'a>,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { unreachable!("rejected preflight must not recognize") })
        }

        fn planned_bound_output(
            &self,
            _: OcrRequest<'_>,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            unreachable!("embedded OCR uses the normalized-PNG plan")
        }

        fn planned_normalized_png_output(
            &self,
            _: u32,
            _: u32,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            self.plans.fetch_add(1, Ordering::SeqCst);
            OcrOutputPlan::try_new(u64::from(self.regions) * 256 + 128, self.regions, 128)
        }

        fn recognize_bound<'a>(
            &'a self,
            _: OcrRequest<'a>,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { unreachable!("rejected preflight must not recognize") })
        }
    }

    impl OcrEngine for LegacyBoundOcr {
        fn id(&self) -> &'static str {
            "test.ocr.legacy-bound"
        }

        fn recognize<'a>(
            &'a self,
            _: OcrRequest<'a>,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
            Box::pin(async {
                unreachable!("provider without normalized preflight must be skipped")
            })
        }

        fn planned_bound_output(
            &self,
            _: OcrRequest<'_>,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            OcrOutputPlan::try_new(1024 * 1024, 1, 128)
        }

        fn recognize_bound<'a>(
            &'a self,
            _: OcrRequest<'a>,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                unreachable!("provider without normalized preflight must be skipped")
            })
        }
    }

    impl OcrEngine for SourceBoundOcr {
        fn id(&self) -> &'static str {
            "test.ocr.source-bound"
        }

        fn recognize<'a>(
            &'a self,
            _: OcrRequest<'a>,
            _: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrResult, ConversionError>> {
            Box::pin(async { unreachable!("embedded OCR requires a bound result") })
        }

        fn planned_bound_output(
            &self,
            _: OcrRequest<'_>,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            OcrOutputPlan::try_new(16 * 1024, 1, 128)
        }

        fn planned_normalized_png_output(
            &self,
            _: u32,
            _: u32,
            _: &ConversionOptions,
            _: &ExecutionContext,
        ) -> Result<OcrOutputPlan, ConversionError> {
            self.plans.fetch_add(1, Ordering::SeqCst);
            OcrOutputPlan::try_new_with_working(
                self.planned_bytes,
                self.planned_working_bytes,
                1,
                128,
            )
        }

        fn recognize_bound<'a>(
            &'a self,
            request: OcrRequest<'a>,
            context: &'a ExecutionContext,
        ) -> BoxFuture<'a, Result<OcrRecognition, ConversionError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut digest: [u8; 32] = Sha256::digest(request.image).into();
            if self.corrupt_identity {
                digest[0] ^= 0xff;
            }
            let image = image::load_from_memory(request.image).expect("normalized PNG");
            let identity = OcrInputIdentity::try_new(digest, image.width(), image.height(), 0);
            Box::pin(async move {
                context.checkpoint()?;
                let bound = BoundOcrResult::try_new_for_input(
                    OcrResult {
                        regions: vec![OcrRegion {
                            text: "embedded words".into(),
                            polygon: [(0.0, 0.0), (2.0, 0.0), (2.0, 1.0), (0.0, 1.0)],
                            confidence: 0.97,
                        }],
                        provider: "test.ocr.recognizer".into(),
                    },
                    vec![0.98],
                    vec![
                        OcrEvidenceStep {
                            stage: OcrEvidenceStage::Detection,
                            provider: "test.ocr.detector".into(),
                            model: Some("detector-sha256".into()),
                        },
                        OcrEvidenceStep {
                            stage: OcrEvidenceStage::Recognition,
                            provider: "test.ocr.recognizer".into(),
                            model: Some("recognizer-sha256".into()),
                        },
                    ],
                    identity?,
                )?;
                Ok(OcrRecognition::Bound(bound))
            })
        }
    }

    fn png() -> Vec<u8> {
        let pixels = RgbaImage::from_fn(3, 2, |x, y| {
            Rgba([u8::try_from(x * 40).unwrap(), u8::try_from(y * 80).unwrap(), 30, 255])
        });
        let mut cursor = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels).write_to(&mut cursor, ImageFormat::Png).unwrap();
        cursor.into_inner()
    }

    fn gif(frame_count: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = GifEncoder::new(&mut bytes);
        for value in 0..frame_count {
            encoder
                .encode_frame(Frame::new(RgbaImage::from_pixel(
                    1,
                    1,
                    Rgba([u8::try_from(value).unwrap(), 2, 3, 255]),
                )))
                .unwrap();
        }
        drop(encoder);
        bytes
    }

    #[test]
    fn big_tiff_dimensions_are_read_from_long8_ifd_entries() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II+\0");
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u64.to_le_bytes());
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        for (tag, value) in [(256_u16, 640_u64), (257_u16, 480_u64)] {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&16_u16.to_le_bytes());
            bytes.extend_from_slice(&1_u64.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(tiff_dimensions(&bytes), Some((640, 480)));
    }

    #[test]
    fn gif_decoded_limit_uses_rgba_bytes_and_memory_uses_working_set() {
        let bytes = gif(1);
        let mut options = ConversionOptions::default();
        options.limits.max_decompressed_bytes = 4;
        options.limits.max_memory_bytes = 1024 * 1024;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let normalized = normalize_gif(&bytes, Some("one.gif"), &options, &context).unwrap();
        assert_eq!((normalized.width, normalized.height), (1, 1));

        options.limits.max_decompressed_bytes = 3;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(matches!(
            normalize_gif(&bytes, Some("one.gif"), &options, &context),
            Err(ConversionError::ResourceLimit { limit: "max_decompressed_bytes", .. })
        ));

        options.limits.max_decompressed_bytes = 4;
        options.limits.max_memory_bytes = 19;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(matches!(
            normalize_gif(&bytes, Some("one.gif"), &options, &context),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
    }

    fn source_bound_ocr(corrupt_identity: bool) -> Arc<SourceBoundOcr> {
        source_bound_ocr_with_plan(corrupt_identity, 16 * 1024)
    }

    fn source_bound_ocr_with_plan(
        corrupt_identity: bool,
        planned_bytes: u64,
    ) -> Arc<SourceBoundOcr> {
        source_bound_ocr_with_working_plan(corrupt_identity, planned_bytes, 0)
    }

    fn source_bound_ocr_with_working_plan(
        corrupt_identity: bool,
        planned_bytes: u64,
        planned_working_bytes: u64,
    ) -> Arc<SourceBoundOcr> {
        Arc::new(SourceBoundOcr {
            calls: AtomicUsize::new(0),
            plans: AtomicUsize::new(0),
            planned_bytes,
            planned_working_bytes,
            corrupt_identity,
        })
    }

    fn provenance(part: &str) -> Provenance {
        Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "test.converter".into(),
            locator: SourceLocator {
                page: Some(2),
                part: Some(part.into()),
                bounds: Some(into_markdown_core::Rect {
                    x: 10.0,
                    y: 20.0,
                    width: 30.0,
                    height: 20.0,
                }),
                page_width: Some(600.0),
                page_height: Some(800.0),
                byte_start: Some(4),
                byte_end: Some(12),
                ..SourceLocator::default()
            },
            confidence: Some(1.0),
        }
    }

    fn image_node(id: &str, asset: &str, part: &str) -> BlockNode {
        let mut source = provenance(part);
        if id.ends_with('b') {
            source.locator.bounds =
                Some(into_markdown_core::Rect { x: 10.0, y: 60.0, width: 30.0, height: 20.0 });
        }
        BlockNode {
            id: NodeId(id.into()),
            block: Block::Image { asset: AssetId(asset.into()), alt: None },
            provenance: source,
        }
    }

    fn output() -> ConverterOutput {
        let bytes = png();
        let mut page_provenance = provenance("word/document.xml");
        page_provenance.provider = "builtin.converter.pdfium".into();
        ConverterOutput::new(
            Document {
                blocks: vec![BlockNode {
                    id: NodeId("page-2".into()),
                    block: Block::Page {
                        number: 2,
                        blocks: vec![
                            image_node("image-a", "asset-a", "word/media/a.png"),
                            image_node("image-b", "asset-b", "word/media/b.png"),
                        ],
                    },
                    provenance: page_provenance,
                }],
                ..Document::default()
            },
            vec![
                Asset {
                    id: AssetId("asset-a".into()),
                    filename: Some("a.png".into()),
                    media_type: "image/png".into(),
                    bytes: bytes.clone(),
                    external_uri: None,
                },
                Asset {
                    id: AssetId("asset-b".into()),
                    filename: Some("b.png".into()),
                    media_type: "image/png".into(),
                    bytes,
                    external_uri: None,
                },
            ],
            vec![],
        )
    }

    #[test]
    fn auto_policy_skips_animated_gif_before_provider_planning() {
        let ocr = Arc::new(EntryGuardOcr {
            plans: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            regions: 1,
        });
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut source = output();
        let animated = gif(2);
        for asset in &mut source.assets {
            asset.bytes = animated.clone();
            asset.filename = Some(format!("{}.gif", asset.id.0));
            asset.media_type = "image/gif".into();
        }
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        options.limits.max_archive_entries = 0;
        options.limits.max_total_asset_bytes = 0;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());

        assert!(matches!(
            plan_enrichment(&source, InputFormat::Docx, &options, &services, &context).unwrap(),
            EnrichmentPlan::Skip
        ));
        assert_eq!(ocr.plans.load(Ordering::SeqCst), 0);
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);

        options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(matches!(
            plan_enrichment(&source, InputFormat::Docx, &options, &services, &context),
            Err(ConversionError::Unsupported { .. })
        ));
    }

    #[test]
    fn auto_policy_excluded_raster_does_not_consume_enrichment_limits() {
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let eligible = png();
        let excluded = gif(2);
        let mut source = output();
        let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else {
            panic!("page expected")
        };
        blocks.truncate(1);
        for index in 0..16 {
            blocks.push(image_node(
                &format!("excluded-{index}"),
                "asset-b",
                "word/media/animated.gif",
            ));
        }
        source.assets[0].bytes = eligible.clone();
        source.assets[1].bytes = excluded;
        source.assets[1].filename = Some("animated.gif".into());
        source.assets[1].media_type = "image/gif".into();

        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        options.limits.max_archive_entries = 8;
        options.limits.max_total_asset_bytes = u64::try_from(eligible.len()).unwrap();
        options.limits.max_pages = 1;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());

        let enriched =
            block_on(enrich(source, InputFormat::Docx, &options, &services, &context)).unwrap();
        assert_eq!(ocr.plans.load(Ordering::SeqCst), 1);
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 1);
        assert!(enriched.diagnostics.is_empty());
        let Block::Page { blocks, .. } = &enriched.document.blocks[0].block else {
            panic!("page expected")
        };
        assert_eq!(blocks.len(), 18);
        assert_eq!(
            blocks
                .iter()
                .filter(
                    |node| matches!(&node.block, Block::Image { asset, .. } if asset.0 == "asset-b")
                )
                .count(),
            16
        );
    }

    #[test]
    fn duplicate_bytes_are_recognized_once_and_remapped_per_reference() {
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let enriched =
            block_on(enrich(output(), InputFormat::Pdf, &options, &services, &context)).unwrap();
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 1);
        let Block::Page { blocks, .. } = &enriched.document.blocks[0].block else {
            panic!("page expected")
        };
        assert_eq!(blocks.len(), 4);
        let first = blocks
            .iter()
            .find(|node| {
                matches!(
                    &node.block,
                    Block::Paragraph(inlines)
                        if inlines.iter().any(|inline| matches!(inline,
                            Inline::OcrText { provenance, .. }
                                if provenance.locator.part.as_deref() == Some("word/media/a.png")))
                )
            })
            .unwrap();
        let second = blocks
            .iter()
            .find(|node| {
                matches!(
                    &node.block,
                    Block::Paragraph(inlines)
                        if inlines.iter().any(|inline| matches!(inline,
                            Inline::OcrText { provenance, .. }
                                if provenance.locator.part.as_deref() == Some("word/media/b.png")))
                )
            })
            .unwrap();
        let Block::Paragraph(inlines) = &first.block else { panic!("OCR paragraph expected") };
        let Inline::OcrText { value, evidence, provenance, .. } = &inlines[0] else {
            panic!("OCR inline expected")
        };
        assert_eq!(provenance.locator.part.as_deref(), Some("word/media/a.png"));
        assert_eq!(provenance.locator.byte_start, None);
        let Block::Paragraph(second_inlines) = &second.block else {
            panic!("OCR paragraph expected")
        };
        let Inline::OcrText { provenance: second_provenance, .. } = &second_inlines[0] else {
            panic!("OCR inline expected")
        };
        assert_eq!(second_provenance.locator.part.as_deref(), Some("word/media/b.png"));
        assert_eq!(value, "embedded words");
        assert_eq!(evidence.page, 2);
        assert_eq!(
            evidence.regions[0].polygon[2],
            into_markdown_core::SourcePoint { x: 30.0, y: 30.0 }
        );
        assert_eq!(
            provenance.locator.bounds,
            Some(into_markdown_core::Rect { x: 10.0, y: 20.0, width: 20.0, height: 10.0 })
        );
    }

    #[test]
    fn remapped_locator_bounds_are_derived_from_the_published_evidence_points() {
        let regions = vec![into_markdown_core::OcrSourceRegion {
            source_index: 0,
            polygon: [
                into_markdown_core::SourcePoint { x: 0.1, y: 7.3 },
                into_markdown_core::SourcePoint { x: 9.7, y: 1.2 },
                into_markdown_core::SourcePoint { x: 8.4, y: 11.9 },
                into_markdown_core::SourcePoint { x: 0.3, y: 10.1 },
            ],
            detection_confidence: 0.9,
            recognition_confidence: 0.8,
        }];
        let bounds = evidence_bounds(&regions).unwrap();
        let points = regions[0].polygon;
        let minimum_x = points.iter().map(|point| point.x).fold(f32::INFINITY, f32::min);
        let minimum_y = points.iter().map(|point| point.y).fold(f32::INFINITY, f32::min);
        let maximum_x = points.iter().map(|point| point.x).fold(f32::NEG_INFINITY, f32::max);
        let maximum_y = points.iter().map(|point| point.y).fold(f32::NEG_INFINITY, f32::max);
        assert_eq!(
            bounds,
            into_markdown_core::Rect {
                x: minimum_x,
                y: minimum_y,
                width: maximum_x - minimum_x,
                height: maximum_y - minimum_y,
            }
        );
    }

    #[test]
    fn duplicate_references_increase_the_preflight_plan() {
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let duplicate = output();
        let mut single = output();
        let Block::Page { blocks, .. } = &mut single.document.blocks[0].block else {
            panic!("page expected")
        };
        blocks.truncate(1);
        let EnrichmentPlan::Reserve(single_plan) =
            plan_enrichment(&single, InputFormat::Docx, &options, &services, &context).unwrap()
        else {
            panic!("active plan expected")
        };
        let EnrichmentPlan::Reserve(duplicate_plan) =
            plan_enrichment(&duplicate, InputFormat::Docx, &options, &services, &context).unwrap()
        else {
            panic!("active plan expected")
        };
        assert!(duplicate_plan >= single_plan + 16 * 1024 + 4 * 1024);
    }

    #[test]
    fn repeated_references_share_page_limit_and_provider_request() {
        let ocr = source_bound_ocr_with_plan(false, 512 * 1024);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut source = output();
        let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else {
            panic!("page expected")
        };
        *blocks = (0..64)
            .map(|index| {
                image_node(
                    &format!("repeated-{index}"),
                    "asset-a",
                    &format!("word/media/repeated-{index}.png"),
                )
            })
            .collect();
        let asset_bytes = u64::try_from(source.assets[0].bytes.len()).unwrap();
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        options.limits.max_archive_entries = 64;
        options.limits.max_pages = 1;
        options.limits.max_total_asset_bytes = asset_bytes;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());

        let EnrichmentPlan::Reserve(plan) =
            plan_enrichment(&source, InputFormat::Docx, &options, &services, &context).unwrap()
        else {
            panic!("active plan expected")
        };
        assert_eq!(ocr.plans.load(Ordering::SeqCst), 1);
        assert!(plan >= 512 * 1024 * 66);
        let low_context = ExecutionContext::new(
            ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: plan - 1,
                ..options.limits.clone()
            },
        );
        let error = low_context.reserve_memory(plan).unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
        assert_eq!(low_context.reserved_memory_bytes(), 0);
        let enriched =
            block_on(enrich(source, InputFormat::Docx, &options, &services, &context)).unwrap();

        assert_eq!(ocr.calls.load(Ordering::SeqCst), 1);
        let Block::Page { blocks, .. } = &enriched.document.blocks[0].block else {
            panic!("page expected")
        };
        assert_eq!(blocks.len(), 128);
        drop(enriched);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn provider_working_peak_is_reserved_once_before_recognition() {
        let retained = 64 * 1024;
        let working = 8 * 1024 * 1024;
        let ocr = source_bound_ocr_with_working_plan(false, retained, working);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let EnrichmentPlan::Reserve(plan) =
            plan_enrichment(&output(), InputFormat::Docx, &options, &services, &context).unwrap()
        else {
            panic!("active plan expected")
        };
        assert!(plan >= working + retained * 2);

        let low_context = ExecutionContext::new(
            ExecutionOptions::default(),
            into_markdown_core::ResourceLimits {
                max_memory_bytes: plan - 1,
                ..options.limits.clone()
            },
        );
        assert!(matches!(
            low_context.reserve_memory(plan),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reference_limit_fails_during_preflight_without_provider_or_leases() {
        let ocr = Arc::new(EntryGuardOcr {
            plans: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            regions: 1,
        });
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        options.limits.max_archive_entries = 1;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());

        let error = plan_enrichment(&output(), InputFormat::Docx, &options, &services, &context)
            .unwrap_err();

        assert!(
            matches!(&error, ConversionError::ResourceLimit { limit: "max_archive_entries", .. }),
            "{error:?}"
        );
        assert_eq!(ocr.plans.load(Ordering::SeqCst), 0);
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn provider_region_bound_is_multiplied_by_eligible_references_before_recognition() {
        let ocr = Arc::new(EntryGuardOcr {
            plans: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            regions: u32::try_from(into_markdown_core::MAX_DOCUMENT_NODES / 2).unwrap(),
        });
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());

        let error = plan_enrichment(&output(), InputFormat::Docx, &options, &services, &context)
            .unwrap_err();

        assert!(matches!(error, ConversionError::ResourceLimit { limit: "documentNodes", .. }));
        assert_eq!(ocr.plans.load(Ordering::SeqCst), 1);
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    #[test]
    fn all_known_input_limits_fail_before_provider_planning_or_leases() {
        for (expected, configure) in [
            (
                "max_total_asset_bytes",
                (|options: &mut ConversionOptions| options.limits.max_total_asset_bytes = 1)
                    as fn(&mut ConversionOptions),
            ),
            (
                "max_pages",
                (|options: &mut ConversionOptions| options.limits.max_pages = 0)
                    as fn(&mut ConversionOptions),
            ),
            (
                "max_decompressed_bytes",
                (|options: &mut ConversionOptions| options.limits.max_decompressed_bytes = 1)
                    as fn(&mut ConversionOptions),
            ),
        ] {
            let ocr = Arc::new(EntryGuardOcr {
                plans: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                regions: 1,
            });
            let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
            let mut options = ConversionOptions::default();
            options.ocr.policy = OcrPolicy::Always;
            configure(&mut options);
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());

            let error =
                plan_enrichment(&output(), InputFormat::Docx, &options, &services, &context)
                    .unwrap_err();

            assert!(
                matches!(error, ConversionError::ResourceLimit { limit, .. } if limit == expected),
                "expected {expected}, got {error}"
            );
            assert_eq!(ocr.plans.load(Ordering::SeqCst), 0, "{expected}");
            assert_eq!(ocr.calls.load(Ordering::SeqCst), 0, "{expected}");
            assert_eq!(context.reserved_memory_bytes(), 0, "{expected}");
        }
    }

    #[test]
    fn node_id_preprocess_propagates_cancel_and_timeout_without_partial_state() {
        let nested = vec![BlockNode {
            id: NodeId("large-page".into()),
            block: Block::Page {
                number: 1,
                blocks: (0..1_024)
                    .map(|index| BlockNode {
                        id: NodeId(format!("nested-{index}")),
                        block: Block::Rule,
                        provenance: provenance("large/document.xml"),
                    })
                    .collect(),
            },
            provenance: provenance("large/document.xml"),
        }];
        let original = nested.clone();

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ConversionOptions::default().limits,
        );
        let mut cancelled_ids = BTreeSet::new();
        assert!(matches!(
            collect_node_ids(&nested, &mut cancelled_ids, &cancelled),
            Err(ConversionError::Cancelled)
        ));
        assert!(cancelled_ids.is_empty());
        assert_eq!(nested, original);
        assert_eq!(cancelled.reserved_memory_bytes(), 0);

        let timed_out = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::ZERO),
                ..ExecutionOptions::default()
            },
            ConversionOptions::default().limits,
        );
        let mut timed_out_ids = BTreeSet::new();
        assert!(matches!(
            collect_node_ids(&nested, &mut timed_out_ids, &timed_out),
            Err(ConversionError::Timeout)
        ));
        assert!(timed_out_ids.is_empty());
        assert_eq!(nested, original);
        assert_eq!(timed_out.reserved_memory_bytes(), 0);
    }

    #[test]
    fn node_id_collision_scan_propagates_cancel_and_timeout_transactionally() {
        let source = NodeId("crowded".into());
        let occupied = (1..=1_024)
            .map(|suffix| NodeId(format!("crowded::ocr::{suffix}")))
            .collect::<BTreeSet<_>>();

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ConversionOptions::default().limits,
        );
        let mut cancelled_ids = occupied.clone();
        assert!(matches!(
            fresh_ocr_node_id(&source, 1, &mut cancelled_ids, &cancelled),
            Err(ConversionError::Cancelled)
        ));
        assert_eq!(cancelled_ids, occupied);
        assert_eq!(cancelled.reserved_memory_bytes(), 0);

        let timed_out = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::ZERO),
                ..ExecutionOptions::default()
            },
            ConversionOptions::default().limits,
        );
        let mut timed_out_ids = occupied.clone();
        assert!(matches!(
            fresh_ocr_node_id(&source, 1, &mut timed_out_ids, &timed_out),
            Err(ConversionError::Timeout)
        ));
        assert_eq!(timed_out_ids, occupied);
        assert_eq!(timed_out.reserved_memory_bytes(), 0);
    }

    #[test]
    fn generated_ocr_ids_are_stable_and_never_collide() {
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let mut source = output();
        let Block::Page { blocks, .. } = &mut source.document.blocks[0].block else {
            panic!("page expected")
        };
        blocks.insert(
            0,
            BlockNode {
                id: NodeId("image-a::ocr::1".into()),
                block: Block::Rule,
                provenance: provenance("existing"),
            },
        );
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let enriched =
            block_on(enrich(source, InputFormat::Docx, &options, &services, &context)).unwrap();
        let Block::Page { blocks, .. } = &enriched.document.blocks[0].block else {
            panic!("page expected")
        };
        let ids = blocks.iter().map(|node| node.id.0.as_str()).collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), blocks.len());
        assert!(ids.contains("image-a::ocr::1"));
        assert!(ids.contains("image-a::ocr::2"));
    }

    #[test]
    fn off_and_non_container_formats_are_exact_no_ops() {
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        for (format, policy) in
            [(InputFormat::Docx, OcrPolicy::Off), (InputFormat::Json, OcrPolicy::Always)]
        {
            let before = output();
            let expected_document = before.document.clone();
            let expected_assets = before.assets.clone();
            let expected_diagnostics = before.diagnostics.clone();
            let mut options = ConversionOptions::default();
            options.ocr.policy = policy;
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let after = block_on(enrich(before, format, &options, &services, &context)).unwrap();
            assert_eq!(after.document, expected_document);
            assert_eq!(after.assets, expected_assets);
            assert_eq!(after.diagnostics, expected_diagnostics);
        }
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn every_supported_container_uses_the_same_source_bound_stage() {
        let formats = [
            InputFormat::Pdf,
            InputFormat::Doc,
            InputFormat::Docx,
            InputFormat::Ppt,
            InputFormat::Pptx,
            InputFormat::Xls,
            InputFormat::Xlsx,
            InputFormat::Odt,
            InputFormat::Ods,
            InputFormat::Odp,
            InputFormat::Rtf,
            InputFormat::Epub,
            InputFormat::Html,
            InputFormat::Ipynb,
            InputFormat::Zip,
            InputFormat::OutlookMsg,
        ];
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        for format in formats {
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let enriched = block_on(enrich(output(), format, &options, &services, &context))
                .unwrap_or_else(|error| panic!("{format:?}: {error}"));
            let Block::Page { blocks, .. } = &enriched.document.blocks[0].block else {
                panic!("{format:?}: page expected")
            };
            assert_eq!(blocks.len(), 4, "{format:?}");
        }
        assert_eq!(ocr.calls.load(Ordering::SeqCst), formats.len());
    }

    #[test]
    fn arbitrary_data_formats_and_non_local_assets_are_never_guessed() {
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        for format in [
            InputFormat::Csv,
            InputFormat::Json,
            InputFormat::Xml,
            InputFormat::Text,
            InputFormat::Feed,
            InputFormat::Markdown,
            InputFormat::Image,
        ] {
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let before = output();
            let expected = before.document.clone();
            let after = block_on(enrich(before, format, &options, &services, &context)).unwrap();
            assert_eq!(after.document, expected, "{format:?}");
        }

        let mut remote = output();
        remote.assets[0].external_uri = Some("https://example.invalid/image.png".into());
        remote.assets[0].bytes.clear();
        remote.assets[1].media_type = "image/svg+xml".into();
        let expected = remote.document.clone();
        let mut no_candidate_options = options.clone();
        no_candidate_options.limits.max_archive_entries = 0;
        no_candidate_options.limits.max_pages = 0;
        no_candidate_options.limits.max_total_asset_bytes = 0;
        let no_candidate_context =
            ExecutionContext::new(ExecutionOptions::default(), no_candidate_options.limits.clone());
        assert_eq!(
            plan_enrichment(
                &remote,
                InputFormat::Html,
                &no_candidate_options,
                &services,
                &no_candidate_context,
            )
            .unwrap(),
            EnrichmentPlan::Skip
        );
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let after =
            block_on(enrich(remote, InputFormat::Html, &options, &services, &context)).unwrap();
        assert_eq!(after.document, expected);

        for policy in [OcrPolicy::Always, OcrPolicy::Auto] {
            let mut mismatched = output();
            mismatched.assets[0].media_type = "image/jpeg".into();
            options.ocr.policy = policy;
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let error =
                block_on(enrich(mismatched, InputFormat::Docx, &options, &services, &context))
                    .unwrap_err();
            assert!(matches!(error, ConversionError::Malformed { .. }), "{policy:?}");
        }
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ocr_result_is_rejected_when_it_is_bound_to_different_image_bytes() {
        let ocr = source_bound_ocr(true);
        let services = Services { ocr: Some(ocr), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let error = block_on(enrich(output(), InputFormat::Docx, &options, &services, &context))
            .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Ocr);
        assert!(error.to_string().contains("does not match the normalized source image"));
    }

    #[test]
    fn resource_limits_and_cancellation_stop_before_ocr_publication() {
        let ocr = source_bound_ocr(false);
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        options.limits.max_total_asset_bytes = 1;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let error = block_on(enrich(output(), InputFormat::Docx, &options, &services, &context))
            .unwrap_err();
        assert!(matches!(
            error,
            ConversionError::ResourceLimit { limit: "max_total_asset_bytes", .. }
        ));

        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Always;
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            options.limits.clone(),
        );
        let error = block_on(enrich(output(), InputFormat::Docx, &options, &services, &context))
            .unwrap_err();
        assert_eq!(error.code(), into_markdown_core::ErrorCode::Cancelled);
        assert_eq!(ocr.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn auto_propagates_normalization_limits_cancellation_and_timeout() {
        let services = Services::default();
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        options.limits.max_decompressed_bytes = 1;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let error = block_on(enrich(output(), InputFormat::Docx, &options, &services, &context))
            .unwrap_err();
        assert!(matches!(
            error,
            ConversionError::ResourceLimit { limit: "max_decompressed_bytes", .. }
        ));

        let bytes = png();
        let asset = Asset {
            id: AssetId("direct".into()),
            filename: Some("direct.png".into()),
            media_type: "image/png".into(),
            bytes,
            external_uri: None,
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            ConversionOptions::default().limits,
        );
        assert!(matches!(
            normalize(&asset, &ConversionOptions::default(), &cancelled),
            Err(ConversionError::Cancelled)
        ));

        let timed_out = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::ZERO),
                ..ExecutionOptions::default()
            },
            ConversionOptions::default().limits,
        );
        assert!(matches!(
            normalize(&asset, &ConversionOptions::default(), &timed_out),
            Err(ConversionError::Timeout)
        ));
    }

    #[test]
    fn auto_degrades_missing_normalized_provider_plan_but_always_fails() {
        let ocr = Arc::new(LegacyBoundOcr(AtomicUsize::new(0)));
        let services = Services { ocr: Some(ocr.clone()), ..Services::default() };
        let mut options = ConversionOptions::default();
        options.ocr.policy = OcrPolicy::Auto;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(matches!(
            plan_enrichment(&output(), InputFormat::Docx, &options, &services, &context).unwrap(),
            EnrichmentPlan::Reserve(_)
        ));
        let enriched =
            block_on(enrich(output(), InputFormat::Docx, &options, &services, &context)).unwrap();
        assert_eq!(ocr.0.load(Ordering::SeqCst), 0);
        assert!(
            enriched
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("normalized-PNG output bound"))
        );

        options.ocr.policy = OcrPolicy::Always;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert!(matches!(
            plan_enrichment(&output(), InputFormat::Docx, &options, &services, &context),
            Err(ConversionError::ComponentUnavailable { .. })
        ));
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
