//! Deterministic GitHub-Flavored Markdown rendering for the unified IR.
//!
//! This crate deliberately renders asset references only. Writing extracted
//! assets remains the caller's responsibility.

mod fixed_alloc;

use base64::Engine as _;
use into_markdown_core::{
    Asset, AssetMode, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Document, ExecutionContext, Inline, InlineMark, ListItem, ListKind, MarkdownRenderer,
    TableAlignment, TableRow, canonical_external_asset_uri,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use fixed_alloc::{ExactString, FixedSlots};

/// Deterministic renderer occupying the single built-in GFM renderer slot.
#[derive(Debug, Default)]
pub struct GfmRenderer;

/// Immutable routing decision shared by Markdown rendering and output writers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetPlan {
    entries: Vec<PlannedAsset>,
    by_id: BTreeMap<String, PlannedAssetReference>,
}

/// One physical extracted resource. Several document asset IDs may share it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAsset {
    /// Asset IDs whose identical content and representation share this entry.
    pub asset_ids: Vec<String>,
    /// Index of the authoritative bytes in the caller's asset slice.
    pub source_index: usize,
    /// Safe portable basename.
    pub filename: String,
    /// Renderer-visible URI for this entry.
    pub uri: String,
    /// Normalized media type.
    pub media_type: String,
    /// Uncompressed byte length.
    pub size: u64,
    /// Complete lowercase SHA-256 content digest.
    pub sha256: String,
}

/// Per-ID lookup result retained by [`AssetPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAssetReference {
    /// Index in [`AssetPlan::entries`], absent for an external-only asset.
    pub entry_index: Option<usize>,
    /// Index in the original asset slice.
    pub source_index: usize,
}

impl AssetPlan {
    /// Physical entries in deterministic filename order.
    #[must_use]
    pub fn entries(&self) -> &[PlannedAsset] {
        &self.entries
    }

    /// Resolve a document-scoped asset ID.
    #[must_use]
    pub fn reference(&self, id: &str) -> Option<&PlannedAssetReference> {
        self.by_id.get(id)
    }

    /// Resolve the extracted URI for an ID with bytes.
    #[must_use]
    pub fn uri(&self, id: &str) -> Option<&str> {
        self.reference(id)
            .and_then(|reference| reference.entry_index)
            .map(|index| self.entries[index].uri.as_str())
    }
}

/// Allocate the cross-platform filename used for one extracted asset.
///
/// The basename is a fixed SHA-256 digest of the UTF-8 asset ID. A suggested
/// filename contributes only an ASCII alphanumeric extension of at most 16
/// bytes. The result is bounded ASCII, is never a Windows reserved name, and
/// remains unique when filenames are compared case-insensitively.
#[must_use]
pub fn asset_filename(asset_id: &str, suggested_filename: Option<&str>) -> String {
    let digest = Sha256::digest(asset_id.as_bytes());
    let mut filename = String::with_capacity(6 + digest.len() * 2 + 17);
    filename.push_str("asset-");
    for byte in digest {
        filename.push(hex_digit(byte >> 4, false));
        filename.push(hex_digit(byte & 0x0f, false));
    }
    if let Some(extension) = safe_extension(suggested_filename) {
        filename.push('.');
        filename.push_str(&extension);
    }
    filename
}

/// Plan validated, content-addressed resources before rendering or writing.
///
/// Physical entries are deduplicated only when their complete bytes, normalized
/// media type, and safe extension agree. The complete 256-bit digest is kept in
/// every filename; a second complete digest separates representation semantics.
///
/// # Errors
///
/// Returns a stable conversion error for invalid IR, asset inventories, URI
/// prefixes, metadata conflicts, hash collisions, or resource-limit excess.
#[allow(clippy::too_many_lines)]
pub fn plan_assets(
    document: &Document,
    assets: &[Asset],
    options: &ConversionOptions,
) -> Result<AssetPlan, ConversionError> {
    document.validate().map_err(|error| ConversionError::Internal {
        detail: format!(
            "asset planner received invalid document IR ({} at {}): {}",
            error.code.as_str(),
            error.path,
            error.detail
        ),
    })?;
    validate_asset_uri_prefix(options.output.asset_uri_prefix.as_deref())?;

    let mut by_id = BTreeMap::new();
    let mut groups: BTreeMap<String, (usize, String, Option<String>, Vec<String>)> =
        BTreeMap::new();
    let mut total = 0_u64;
    for (source_index, asset) in assets.iter().enumerate() {
        let id = asset.id.0.as_str();
        if id.trim().is_empty() {
            return Err(render_error("asset inventory contains an empty asset ID"));
        }
        if by_id.contains_key(id) {
            return Err(render_error(format!("duplicate asset ID {id}")));
        }
        let media_type = normalize_media_type(&asset.media_type)?;
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: format!("asset {id} size cannot be represented as u64"),
            })?;
        if size > options.limits.max_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: format!("asset {id}: {size} > {}", options.limits.max_asset_bytes),
            });
        }
        total = total.checked_add(size).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_total_asset_bytes",
            detail: "asset byte count overflowed".into(),
        })?;
        if total > options.limits.max_total_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: format!("{total} > {}", options.limits.max_total_asset_bytes),
            });
        }
        if asset.bytes.is_empty() {
            validate_external_uri(asset.external_uri.as_deref(), id)?;
            by_id.insert(id.to_owned(), PlannedAssetReference { entry_index: None, source_index });
            continue;
        }
        let content_hash = sha256_hex(&asset.bytes);
        let extension = media_type_extension(&media_type);
        match groups.get_mut(&content_hash) {
            Some((planned_source, planned_media_type, _, ids)) => {
                if assets[*planned_source].bytes != asset.bytes {
                    return Err(asset_plan_error(
                        "contentHashCollision",
                        "SHA-256 collision between distinct asset bytes",
                    ));
                }
                if planned_media_type != &media_type {
                    return Err(asset_plan_error(
                        "assetMetadataConflict",
                        format!(
                            "identical asset bytes have conflicting media types {planned_media_type} and {media_type}"
                        ),
                    ));
                }
                ids.push(id.to_owned());
            }
            None => {
                groups.insert(
                    content_hash,
                    (source_index, media_type, extension, vec![id.to_owned()]),
                );
            }
        }
        by_id.insert(id.to_owned(), PlannedAssetReference { entry_index: None, source_index });
    }

    let mut entries = groups
        .into_iter()
        .map(|(sha256, (source_index, media_type, _extension, mut asset_ids))| {
            asset_ids.sort();
            let filename = asset_filename_from_sha256(&sha256, &media_type)?;
            let uri = join_uri_prefix(options.output.asset_uri_prefix.as_deref(), &filename);
            Ok(PlannedAsset {
                asset_ids,
                source_index,
                filename,
                uri,
                media_type,
                size: u64::try_from(assets[source_index].bytes.len()).unwrap_or(u64::MAX),
                sha256,
            })
        })
        .collect::<Result<Vec<_>, ConversionError>>()?;
    entries.sort_by(|left, right| left.filename.cmp(&right.filename));
    for (entry_index, entry) in entries.iter().enumerate() {
        for id in &entry.asset_ids {
            let Some(reference) = by_id.get_mut(id) else {
                return Err(render_error(format!("planned asset ID disappeared: {id}")));
            };
            reference.entry_index = Some(entry_index);
        }
    }
    let plan = AssetPlan { entries, by_id };
    let mut referenced = BTreeSet::new();
    validate_planned_references(&document.blocks, &plan, &mut referenced)?;
    for id in referenced {
        let reference = plan
            .reference(id)
            .ok_or_else(|| render_error(format!("planned image reference disappeared: {id}")))?;
        let source = &assets[reference.source_index];
        let external_only = source.bytes.is_empty() && source.external_uri.is_some();
        if options.output.asset_mode != AssetMode::Omit && plan.uri(id).is_none() && !external_only
        {
            return Err(render_error(format!(
                "asset {id} has no bytes for {} mode",
                match options.output.asset_mode {
                    AssetMode::Extract => "extract",
                    AssetMode::Embed => "embed",
                    AssetMode::Omit => unreachable!(),
                }
            )));
        }
    }
    Ok(plan)
}

/// Build the stable content-addressed filename for a validated SHA-256 digest
/// and media type.
///
/// # Errors
///
/// Returns a rendering error when the digest is not 64 lowercase hexadecimal
/// bytes or the media type is invalid.
pub fn asset_filename_from_sha256(
    sha256: &str,
    media_type: &str,
) -> Result<String, ConversionError> {
    if sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(render_error("asset SHA-256 digest is not canonical lowercase hexadecimal"));
    }
    let media_type = normalize_media_type(media_type)?;
    let extension = media_type_extension(&media_type);
    let mut filename = String::with_capacity(6 + sha256.len() + 17);
    filename.push_str("asset-");
    filename.push_str(sha256);
    if let Some(extension) = extension {
        filename.push('.');
        filename.push_str(&extension);
    }
    Ok(filename)
}

impl MarkdownRenderer for GfmRenderer {
    fn id(&self) -> &'static str {
        "builtin.gfm"
    }

    fn planned_markdown_bytes(
        &self,
        document: &Document,
        assets: &[Asset],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        planned_render_peak(document, assets, options, context)
    }

    fn render<'a>(
        &'a self,
        document: &'a Document,
        assets: &'a [Asset],
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<String, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            render(document, assets, options)
        })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TablePlanOwners {
    occupancy: u64,
    grid_rows: u64,
    grid_slots: u64,
    fallback_header: u64,
    separator_slots: u64,
    separator_strings: u64,
    cell_strings: u64,
    cell_block_joins: u64,
    output: u64,
}

impl TablePlanOwners {
    fn total(&self) -> Result<u64, ConversionError> {
        [
            self.occupancy,
            self.grid_rows,
            self.grid_slots,
            self.fallback_header,
            self.separator_slots,
            self.separator_strings,
            self.cell_strings,
            self.cell_block_joins,
            self.output,
        ]
        .into_iter()
        .try_fold(0_u64, |total, value| total.checked_add(value).ok_or_else(render_plan_overflow))
    }

    fn verify_within(&self, planned: &Self) -> Result<(), ConversionError> {
        for (owner, actual, bound) in [
            ("occupancy", self.occupancy, planned.occupancy),
            ("grid rows", self.grid_rows, planned.grid_rows),
            ("grid slots", self.grid_slots, planned.grid_slots),
            ("fallback header", self.fallback_header, planned.fallback_header),
            ("separator slots", self.separator_slots, planned.separator_slots),
            ("separator strings", self.separator_strings, planned.separator_strings),
            ("cell strings", self.cell_strings, planned.cell_strings),
            ("cell temporaries", self.cell_block_joins, planned.cell_block_joins),
            ("output", self.output, planned.output),
        ] {
            if actual > bound {
                return Err(ConversionError::Internal {
                    detail: format!(
                        "table renderer {owner} allocation {actual} exceeded its {bound}-byte plan"
                    ),
                });
            }
        }
        Ok(())
    }
}

// Planning runs before the request-memory permit exists, so this bounded
// fixed-size scratch area deliberately stays on the stack instead of making
// an uncharged heap allocation. Calls are not nested: the shape is discarded
// before planning any cell's child blocks.
#[allow(clippy::large_stack_arrays)]
fn table_shape(rows: &[TableRow]) -> Result<(usize, bool), ConversionError> {
    let mut occupancy = [0_u32; into_markdown_core::MAX_TABLE_COLUMNS];
    let mut width = 0_usize;
    for row in rows {
        let mut column = 0_usize;
        for cell in &row.cells {
            while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                column = column.saturating_add(1);
            }
            let span = usize::try_from(cell.column_span).map_err(|_| render_plan_overflow())?;
            if span == 0 {
                return Err(render_error("table column span must be positive"));
            }
            let end = column.checked_add(span).ok_or_else(render_plan_overflow)?;
            if end > occupancy.len() {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_table_columns",
                    detail: format!("table renderer width {end} exceeds {}", occupancy.len()),
                });
            }
            occupancy[column..end].fill(cell.row_span);
            column = end;
            width = width.max(end);
        }
        for remaining in &mut occupancy[..width] {
            *remaining = remaining.saturating_sub(1);
        }
    }
    let first_has_header = rows
        .first()
        .is_some_and(|row| !row.cells.is_empty() && row.cells.iter().all(|cell| cell.header));
    Ok((width, first_has_header))
}

fn checked_product(left: usize, right: usize) -> Result<u64, ConversionError> {
    u64::try_from(left.checked_mul(right).ok_or_else(render_plan_overflow)?)
        .map_err(|_| render_plan_overflow())
}

fn fixed_slot_bytes<T>(capacity: usize) -> Result<u64, ConversionError> {
    checked_product(capacity, std::mem::size_of::<T>())
}

fn exact_zero_occupancy(width: usize) -> Result<Vec<u32>, ConversionError> {
    let mut slots = FixedSlots::new(width, "table occupancy allocation failed")?;
    for _ in 0..width {
        slots.push(0)?;
    }
    let values = slots.into_vec()?;
    if values.capacity() != width {
        return Err(render_error("table occupancy allocation lost its exact capacity"));
    }
    Ok(values)
}

fn exact_empty_strings(width: usize, detail: &'static str) -> Result<Vec<String>, ConversionError> {
    let mut slots = FixedSlots::new(width, detail)?;
    for _ in 0..width {
        slots.push(String::new())?;
    }
    let values = slots.into_vec()?;
    if values.capacity() != width {
        return Err(render_error("table string-slot allocation lost its exact capacity"));
    }
    Ok(values)
}

fn exact_string(value: &str, detail: &'static str) -> Result<String, ConversionError> {
    let mut output = ExactString::new(value.len(), detail)?;
    output.push_str(value)?;
    let output = output.finish()?;
    if output.capacity() != value.len() {
        return Err(render_error("table string allocation lost its exact capacity"));
    }
    Ok(output)
}

fn exact_separators(
    width: usize,
    alignments: &[TableAlignment],
) -> Result<Vec<String>, ConversionError> {
    let mut slots = FixedSlots::new(width, "table separator allocation failed")?;
    for column in 0..width {
        let separator = match alignments.get(column).copied().unwrap_or_default() {
            TableAlignment::None => "---",
            TableAlignment::Left => ":---",
            TableAlignment::Center => ":---:",
            TableAlignment::Right => "---:",
        };
        slots.push(exact_string(separator, "table separator string allocation failed")?)?;
    }
    let values = slots.into_vec()?;
    if values.capacity() != width {
        return Err(render_error("table separator allocation lost its exact capacity"));
    }
    Ok(values)
}

fn string_capacity_bytes(values: &[String]) -> Result<u64, ConversionError> {
    values.iter().try_fold(0_u64, |total, value| {
        total
            .checked_add(u64::try_from(value.capacity()).map_err(|_| render_plan_overflow())?)
            .ok_or_else(render_plan_overflow)
    })
}

fn table_row_length(cells: &[String]) -> Result<usize, ConversionError> {
    cells.iter().try_fold(2_usize, |length, cell| {
        length
            .checked_add(3)
            .and_then(|value| value.checked_add(cell.len()))
            .ok_or_else(render_plan_overflow)
    })
}

fn write_exact_table_row(
    output: &mut ExactString,
    cells: &[String],
    newline: bool,
) -> Result<(), ConversionError> {
    output.push_byte(b'|')?;
    for cell in cells {
        output.push_byte(b' ')?;
        output.push_str(cell)?;
        output.push_str(" |")?;
    }
    if newline {
        output.push_byte(b'\n')?;
    }
    Ok(())
}

fn table_cell_owner_bounds<F>(
    rows: &[TableRow],
    mut block_output: F,
) -> Result<(u64, u64), ConversionError>
where
    F: FnMut(&[BlockNode]) -> Result<u64, ConversionError>,
{
    let mut cell_strings = 0_u64;
    let mut cell_block_joins = 0_u64;
    for cell in rows.iter().flat_map(|row| &row.cells) {
        let body = block_output(&cell.blocks)?;
        let flattened = body.checked_mul(4).ok_or_else(render_plan_overflow)?;
        let wrapped = flattened
            .checked_add(if cell.row_span > 1 || cell.column_span > 1 { 112 } else { 0 })
            .and_then(|value| value.checked_add(if cell.header { 17 } else { 0 }))
            .ok_or_else(render_plan_overflow)?;
        cell_strings = cell_strings.checked_add(wrapped).ok_or_else(render_plan_overflow)?;
        // `render_blocks` keeps its Vec<String> and join output alive while
        // two LF normalization results, two `<br>` replacement results, and
        // the optional span/header wrapper are successively constructed.
        cell_block_joins = cell_block_joins
            .checked_add(body)
            .and_then(|value| value.checked_add(body))
            .and_then(|value| value.checked_add(flattened))
            .and_then(|value| value.checked_add(flattened))
            .and_then(|value| value.checked_add(wrapped))
            .ok_or_else(render_plan_overflow)?;
    }
    Ok((cell_strings, cell_block_joins))
}

fn table_plan_owners(
    rows: &[TableRow],
    width: usize,
    first_has_header: bool,
    cell_strings: u64,
    cell_block_joins: u64,
) -> Result<TablePlanOwners, ConversionError> {
    let rows_len = rows.len();
    let rendered_rows = rows_len.checked_add(2).ok_or_else(render_plan_overflow)?;
    let slot_size = std::mem::size_of::<String>();
    let separators = checked_product(width, 5)?;
    let row_overhead = checked_product(rendered_rows, width)?
        .checked_mul(4)
        .and_then(|value| value.checked_add(u64::try_from(rendered_rows).ok()? * 2))
        .ok_or_else(render_plan_overflow)?;
    Ok(TablePlanOwners {
        occupancy: checked_product(width, std::mem::size_of::<u32>())?,
        grid_rows: checked_product(rows_len, std::mem::size_of::<Vec<String>>())?,
        grid_slots: checked_product(
            rows_len.checked_mul(width).ok_or_else(render_plan_overflow)?,
            slot_size,
        )?,
        fallback_header: if first_has_header { 0 } else { checked_product(width, slot_size)? },
        separator_slots: checked_product(width, slot_size)?,
        separator_strings: separators,
        cell_strings,
        cell_block_joins,
        output: cell_strings
            .checked_add(separators)
            .and_then(|value| value.checked_add(row_overhead))
            .ok_or_else(render_plan_overflow)?,
    })
}

#[allow(clippy::too_many_lines)]
fn planned_render_peak(
    document: &Document,
    assets: &[Asset],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    struct Plan<'a> {
        bytes: u64,
        units: u64,
        visited: usize,
        context: &'a ExecutionContext,
    }
    impl Plan<'_> {
        fn add(&mut self, bytes: u64) -> Result<(), ConversionError> {
            self.bytes = self.bytes.checked_add(bytes).ok_or_else(render_plan_overflow)?;
            Ok(())
        }
        fn unit(&mut self) -> Result<(), ConversionError> {
            self.units = self.units.checked_add(1).ok_or_else(render_plan_overflow)?;
            self.visited = self.visited.saturating_add(1);
            if self.visited.is_multiple_of(1_024) {
                self.context.checkpoint()?;
            }
            Ok(())
        }
        fn text(&mut self, value: &str, depth: usize) -> Result<u64, ConversionError> {
            let source = u64::try_from(value.len()).map_err(|_| render_plan_overflow())?;
            let newlines = u64::try_from(value.bytes().filter(|byte| *byte == b'\n').count())
                .map_err(|_| render_plan_overflow())?;
            // `normalize_lf` owns two successive replace results; `single_line`
            // owns one more. `escape_text` expands each input byte by at most
            // five bytes (`&amp;`) and marked text can wrap all six supported
            // marks with at most 13 bytes each.
            self.add(source)?;
            self.add(source)?;
            self.add(source)?;
            let output = source
                .checked_mul(5)
                .and_then(|value| value.checked_add(6 * 13))
                .ok_or_else(render_plan_overflow)?;
            let mut rendered = output;
            self.add(rendered)?;
            // At each typed block ancestor the child string remains alive while
            // indentation replacement, formatting, the Vec<String> slot, and
            // the container join allocate their own result. This recurrence
            // mirrors those four concrete owners instead of applying a global
            // depth multiplier.
            for _ in 0..depth {
                self.add(rendered)?;
                rendered = rendered
                    .checked_add(newlines.checked_mul(4).ok_or_else(render_plan_overflow)?)
                    .and_then(|value| value.checked_add(64))
                    .ok_or_else(render_plan_overflow)?;
                self.add(rendered)?;
                self.add(rendered)?;
                self.add(rendered)?;
            }
            Ok(output)
        }
    }

    fn inlines(
        values: &[Inline],
        block_depth: usize,
        link_depth: usize,
        plan: &mut Plan<'_>,
    ) -> Result<u64, ConversionError> {
        if link_depth > 2 {
            return Err(ConversionError::Internal {
                detail: "renderer preflight rejected nested links".into(),
            });
        }
        let mut output = 0_u64;
        for value in values {
            plan.unit()?;
            let rendered = match value {
                Inline::Text { value, .. }
                | Inline::SourceText { value, .. }
                | Inline::OcrText { value, .. }
                | Inline::Code(value)
                | Inline::Formula(value)
                | Inline::FootnoteReference(value) => plan.text(value, block_depth)?,
                Inline::Link { target, content } => {
                    let target_output = plan.text(target, block_depth)?;
                    inlines(content, block_depth, link_depth + 1, plan)?
                        .checked_add(target_output.checked_mul(3).ok_or_else(render_plan_overflow)?)
                        .and_then(|value| value.checked_add(6))
                        .ok_or_else(render_plan_overflow)?
                }
                Inline::LineBreak => {
                    plan.add(3)?;
                    3
                }
                _ => {
                    return Err(ConversionError::Internal {
                        detail: "renderer preflight encountered an unsupported future inline"
                            .into(),
                    });
                }
            };
            output = output.checked_add(rendered).ok_or_else(render_plan_overflow)?;
        }
        Ok(output)
    }
    fn nodes(
        values: &[BlockNode],
        depth: usize,
        plan: &mut Plan<'_>,
    ) -> Result<u64, ConversionError> {
        if depth > into_markdown_core::MAX_DOCUMENT_DEPTH {
            return Err(ConversionError::Internal {
                detail: "renderer preflight received over-deep document IR".into(),
            });
        }
        let mut output = 0_u64;
        for value in values {
            plan.unit()?;
            let rendered = match &value.block {
                Block::Paragraph(values) | Block::TimedSegment { content: values, .. } => {
                    inlines(values, depth, 1, plan)?
                }
                Block::Heading { content: values, .. } => inlines(values, depth, 1, plan)?
                    .checked_add(16)
                    .ok_or_else(render_plan_overflow)?,
                Block::List { items, .. } => {
                    let mut rendered = 0_u64;
                    for item in items {
                        plan.unit()?;
                        if let Some(label) = &item.marker_label {
                            rendered = rendered
                                .checked_add(plan.text(label, depth)?)
                                .ok_or_else(render_plan_overflow)?;
                        }
                        rendered = rendered
                            .checked_add(
                                nodes(&item.blocks, depth + 1, plan)?
                                    .checked_mul(5)
                                    .and_then(|value| value.checked_add(128))
                                    .ok_or_else(render_plan_overflow)?,
                            )
                            .ok_or_else(render_plan_overflow)?;
                    }
                    rendered
                }
                Block::Table { rows, .. } => {
                    let (width, first_has_header) = table_shape(rows)?;
                    for row in rows {
                        plan.unit()?;
                        for _ in &row.cells {
                            plan.unit()?;
                        }
                    }
                    let (cell_strings, cell_block_joins) =
                        table_cell_owner_bounds(rows, |blocks| nodes(blocks, depth + 1, plan))?;
                    let owners = table_plan_owners(
                        rows,
                        width,
                        first_has_header,
                        cell_strings,
                        cell_block_joins,
                    )?;
                    plan.add(owners.total()?)?;
                    owners.output
                }
                Block::Code { language, text } => {
                    if language.as_deref() == Some("tsv") && longest_run(text, '`') <= 2 {
                        let source =
                            u64::try_from(text.len()).map_err(|_| render_plan_overflow())?;
                        let newlines =
                            u64::try_from(text.bytes().filter(|byte| *byte == b'\n').count())
                                .map_err(|_| render_plan_overflow())?;
                        // Paged workbook TSV escapes backticks, so the three-byte
                        // fence is fixed. Account the two LF-normalization owners,
                        // the fenced output, and each enclosing block join.
                        plan.add(source)?;
                        plan.add(source)?;
                        let mut rendered =
                            source.checked_add(16).ok_or_else(render_plan_overflow)?;
                        plan.add(rendered)?;
                        for _ in 0..depth {
                            plan.add(rendered)?;
                            rendered = rendered
                                .checked_add(
                                    newlines.checked_mul(4).ok_or_else(render_plan_overflow)?,
                                )
                                .and_then(|value| value.checked_add(64))
                                .ok_or_else(render_plan_overflow)?;
                            plan.add(rendered)?;
                            plan.add(rendered)?;
                            plan.add(rendered)?;
                        }
                        rendered
                    } else {
                        let mut rendered = 16_u64;
                        if let Some(language) = language {
                            rendered = rendered
                                .checked_add(plan.text(language, depth)?)
                                .ok_or_else(render_plan_overflow)?;
                        }
                        rendered
                            .checked_add(plan.text(text, depth)?)
                            .ok_or_else(render_plan_overflow)?
                    }
                }
                Block::Formula(value) => plan.text(value, depth)?,
                Block::Footnote { label, blocks } => plan
                    .text(label, depth)?
                    .checked_add(nodes(blocks, depth + 1, plan)?)
                    .and_then(|value| value.checked_add(16))
                    .ok_or_else(render_plan_overflow)?,
                Block::Image { asset, alt } => {
                    let mut rendered = plan.text(&asset.0, depth)?;
                    if let Some(alt) = alt {
                        rendered = rendered
                            .checked_add(plan.text(alt, depth)?)
                            .ok_or_else(render_plan_overflow)?;
                    }
                    rendered.checked_add(8).ok_or_else(render_plan_overflow)?
                }
                Block::Page { blocks, .. } => nodes(blocks, depth + 1, plan)?
                    .checked_add(64)
                    .ok_or_else(render_plan_overflow)?,
                Block::Slide { title, blocks, .. } => {
                    let mut rendered = 32_u64;
                    if let Some(title) = title {
                        rendered = rendered
                            .checked_add(plan.text(title, depth)?)
                            .ok_or_else(render_plan_overflow)?;
                    }
                    rendered
                        .checked_add(nodes(blocks, depth + 1, plan)?)
                        .ok_or_else(render_plan_overflow)?
                }
                Block::Sheet { name, blocks } => plan
                    .text(name, depth)?
                    .checked_add(nodes(blocks, depth + 1, plan)?)
                    .and_then(|value| value.checked_add(16))
                    .ok_or_else(render_plan_overflow)?,
                Block::Rule => {
                    plan.add(3)?;
                    3
                }
                _ => {
                    return Err(ConversionError::Internal {
                        detail: "renderer preflight encountered an unsupported future block".into(),
                    });
                }
            };
            output = output
                .checked_add(rendered)
                .and_then(|value| value.checked_add(2))
                .ok_or_else(render_plan_overflow)?;
        }
        Ok(output)
    }

    context.checkpoint()?;
    let mut plan = Plan { bytes: 0, units: 0, visited: 0, context };
    plan.add(into_markdown_core::estimate_validation_working_set(document, assets, &[])?)?;
    let _ = nodes(&document.blocks, 1, &mut plan)?;
    for asset in assets {
        plan.unit()?;
        for value in [
            Some(&asset.id.0),
            asset.filename.as_ref(),
            Some(&asset.media_type),
            asset.external_uri.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            // Asset planning clones each value into grouping and lookup maps.
            let _ = plan.text(value, 1)?;
        }
        if options.output.asset_mode == AssetMode::Embed && !asset.bytes.is_empty() {
            let source = u64::try_from(asset.bytes.len()).map_err(|_| render_plan_overflow())?;
            let base64 = source
                .checked_add(2)
                .and_then(|value| value.checked_div(3))
                .and_then(|value| value.checked_mul(4))
                .ok_or_else(render_plan_overflow)?;
            let uri = base64
                .checked_add(u64::try_from(asset.media_type.len()).unwrap_or(u64::MAX))
                .and_then(|value| value.checked_add(13))
                .ok_or_else(render_plan_overflow)?;
            // Base64, data URI, percent-encoded destination, and final image.
            plan.add(base64)?;
            plan.add(uri)?;
            plan.add(uri.checked_mul(3).ok_or_else(render_plan_overflow)?)?;
            plan.add(uri.checked_mul(3).ok_or_else(render_plan_overflow)?)?;
        }
    }
    // Exact container/header owners used by render Vecs and asset B-trees. One
    // full 11-slot B-tree node per unit is deliberately conservative for
    // sparse nodes and includes edges and allocator metadata.
    let string_headers = u64::try_from(std::mem::size_of::<String>())
        .unwrap_or(u64::MAX)
        .checked_mul(16)
        .and_then(|value| value.checked_add(12 * u64::try_from(std::mem::size_of::<usize>()).ok()?))
        .ok_or_else(render_plan_overflow)?;
    plan.add(plan.units.checked_mul(string_headers).ok_or_else(render_plan_overflow)?)?;
    Ok(plan.bytes)
}

fn render_plan_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "Markdown renderer preflight plan overflowed".into(),
    }
}

/// Render a document synchronously using the built-in GFM policy.
///
/// This convenience function uses the same validation and deterministic output
/// contract as [`GfmRenderer`]. The result always uses LF line endings.
///
/// # Errors
///
/// Returns a stable conversion error when the document or asset inventory is
/// invalid, or when a non-GFM output flavor is requested.
pub fn render(
    document: &Document,
    assets: &[Asset],
    options: &ConversionOptions,
) -> Result<String, ConversionError> {
    if !options.output.flavor.eq_ignore_ascii_case("gfm") {
        return Err(ConversionError::Unsupported {
            detail: format!(
                "renderer builtin.gfm does not support flavor {}",
                options.output.flavor
            ),
        });
    }
    let plan = plan_assets(document, assets, options)?;
    let context = RenderContext { plan: &plan, assets, options, document };
    let mut output = context.render_blocks(&document.blocks)?;
    trim_blank_lines(&mut output);
    if !output.is_empty() {
        output.push('\n');
    }
    Ok(output)
}

struct RenderContext<'a> {
    plan: &'a AssetPlan,
    assets: &'a [Asset],
    options: &'a ConversionOptions,
    document: &'a Document,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineContext {
    Normal,
    TableCell,
}

impl RenderContext<'_> {
    fn render_blocks(&self, nodes: &[BlockNode]) -> Result<String, ConversionError> {
        self.render_blocks_in(nodes, InlineContext::Normal)
    }

    fn render_blocks_in(
        &self,
        nodes: &[BlockNode],
        inline_context: InlineContext,
    ) -> Result<String, ConversionError> {
        let mut rendered = Vec::with_capacity(nodes.len());
        for node in nodes {
            let block = self.render_block(node, inline_context)?;
            if !block.is_empty() {
                rendered.push(block);
            }
        }
        Ok(rendered.join("\n\n"))
    }

    #[allow(clippy::too_many_lines)]
    fn render_block(
        &self,
        node: &BlockNode,
        inline_context: InlineContext,
    ) -> Result<String, ConversionError> {
        match &node.block {
            Block::Paragraph(content) => render_inlines(content, inline_context),
            Block::Heading { level, content } => Ok(format!(
                "{} {}",
                "#".repeat(usize::from(*level)),
                render_inlines(content, inline_context)?
            )),
            Block::List { kind, start, items } => {
                self.render_list(*kind, *start, items, inline_context)
            }
            Block::Table { rows, alignments } => self.render_table(rows, alignments),
            Block::Code { language, text } => {
                Ok(render_fence(text, language.as_deref().map(sanitize_info_string).as_deref()))
            }
            Block::Formula(value) => Ok(render_fence(value, Some("math"))),
            Block::Footnote { label, blocks } => {
                let body = self.render_blocks_in(blocks, inline_context)?;
                let label = footnote_label(label);
                if body.is_empty() {
                    Ok(format!("[^{label}]:"))
                } else {
                    Ok(format!("[^{label}]: {}", indent_continuation(&body, 4)))
                }
            }
            Block::Image { asset, alt } => self.render_image(&asset.0, alt.as_deref()),
            Block::Page { number, blocks } => {
                let body = self.render_blocks_in(blocks, inline_context)?;
                let (anchor_prefix, heading) = page_render_identity(node);
                Ok(with_body(
                    format!("<a id=\"{anchor_prefix}-{number}\"></a>\n\n## {heading} {number}"),
                    &body,
                ))
            }
            Block::Slide { number, title, blocks } => {
                let mut heading = format!("## Slide {number}");
                if let Some(title) = title {
                    write!(
                        heading,
                        ": {}",
                        escape_text(&single_line(title), InlineContext::Normal)
                    )
                    .map_err(|_| render_error("failed to render slide title"))?;
                }
                let body = self.render_blocks_in(blocks, inline_context)?;
                Ok(with_body(heading, &body))
            }
            Block::Sheet { name, blocks } => {
                let body = self.render_blocks_in(blocks, inline_context)?;
                Ok(with_body(
                    format!("## Sheet: {}", escape_text(&single_line(name), InlineContext::Normal)),
                    &body,
                ))
            }
            Block::TimedSegment { range, speaker, content, .. } => {
                let mut line =
                    format!("`{} – {}`", timestamp(range.start_ms), timestamp(range.end_ms));
                if let Some(speaker) = speaker {
                    let speaker = self.speaker_label(speaker);
                    write!(
                        line,
                        " **{}:**",
                        escape_text(&single_line(&speaker), InlineContext::Normal)
                    )
                    .map_err(|_| render_error("failed to render speaker label"))?;
                }
                let rendered_content = render_inlines(content, inline_context)?;
                if !rendered_content.is_empty() {
                    line.push(' ');
                    line.push_str(&rendered_content);
                }
                Ok(line)
            }
            Block::Rule => Ok("---".into()),
            _ => Err(render_error("document contains an unsupported future block variant")),
        }
    }

    fn speaker_label(&self, speaker: &str) -> String {
        let prefix = "media.speaker.";
        let suffix = ".label";
        self.document
            .metadata
            .properties
            .iter()
            .find_map(|(key, value)| {
                key.strip_prefix(prefix)
                    .and_then(|key| key.strip_suffix(suffix))
                    .filter(|id| *id == speaker)
                    .map(|_| value.clone())
            })
            .unwrap_or_else(|| {
                speaker
                    .strip_prefix("speaker-")
                    .and_then(|value| value.parse::<u8>().ok())
                    .filter(|value| (1..=64).contains(value))
                    .map_or_else(|| speaker.to_owned(), |value| format!("Speaker {value}"))
            })
    }

    fn render_list(
        &self,
        kind: ListKind,
        start: u64,
        items: &[ListItem],
        context: InlineContext,
    ) -> Result<String, ConversionError> {
        let mut lines = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let mut marker = match kind {
                ListKind::Bullet => "-".to_owned(),
                ListKind::Task => {
                    if item.checked == Some(true) {
                        "- [x]".to_owned()
                    } else {
                        "- [ ]".to_owned()
                    }
                }
                ListKind::Ordered => {
                    let number = start
                        .checked_add(index as u64)
                        .ok_or_else(|| render_error("ordered-list marker overflowed u64"))?;
                    format!("{number}.")
                }
            };
            if let Some(label) = &item.marker_label {
                marker.push_str(" <!-- source-marker: ");
                marker.push_str(&encode_bytes(label, |byte| byte.is_ascii_alphanumeric()));
                marker.push_str(" -->");
            }
            let body = self.render_blocks_in(&item.blocks, context)?;
            let paragraph_first =
                item.blocks.first().is_some_and(|node| matches!(node.block, Block::Paragraph(_)));
            if body.is_empty() {
                lines.push(marker);
            } else if paragraph_first {
                lines.push(format!("{marker} {}", indent_continuation(&body, 4)));
            } else {
                lines.push(format!("{marker}\n{}", indent_all(&body, 4)));
            }
        }
        Ok(lines.join("\n"))
    }

    fn render_table(
        &self,
        rows: &[TableRow],
        alignments: &[TableAlignment],
    ) -> Result<String, ConversionError> {
        self.render_table_measured(rows, alignments).map(|(output, _)| output)
    }

    fn render_table_measured(
        &self,
        rows: &[TableRow],
        alignments: &[TableAlignment],
    ) -> Result<(String, TablePlanOwners), ConversionError> {
        let (width, first_has_header) = table_shape(rows)?;
        let (grid, mut actual) = self.table_grid(rows, width)?;
        let fallback = (!first_has_header)
            .then(|| exact_empty_strings(width, "table fallback header allocation failed"))
            .transpose()?;
        if let Some(fallback) = &fallback {
            actual.fallback_header = fixed_slot_bytes::<String>(fallback.capacity())?;
        }
        let separators = exact_separators(width, alignments)?;
        actual.separator_slots = fixed_slot_bytes::<String>(separators.capacity())?;
        actual.separator_strings = string_capacity_bytes(&separators)?;

        let start = usize::from(first_has_header);
        let header = if first_has_header { &grid[0] } else { fallback.as_deref().unwrap_or(&[]) };
        let mut output_length = table_row_length(header)?
            .checked_add(table_row_length(&separators)?)
            .ok_or_else(render_plan_overflow)?;
        for row in &grid[start..] {
            output_length = output_length
                .checked_add(table_row_length(row)?)
                .ok_or_else(render_plan_overflow)?;
        }
        output_length = output_length.checked_sub(1).ok_or_else(render_plan_overflow)?;
        let mut output = ExactString::new(output_length, "table output allocation failed")?;
        write_exact_table_row(&mut output, header, true)?;
        write_exact_table_row(&mut output, &separators, start < grid.len())?;
        for (index, row) in grid[start..].iter().enumerate() {
            write_exact_table_row(&mut output, row, index + start + 1 < grid.len())?;
        }
        let output = output.finish()?;
        actual.output = u64::try_from(output.capacity()).map_err(|_| render_plan_overflow())?;
        let planned = table_plan_owners(
            rows,
            width,
            first_has_header,
            actual.cell_strings,
            actual.cell_block_joins,
        )?;
        actual.verify_within(&planned)?;
        Ok((output, actual))
    }

    fn table_grid(
        &self,
        rows: &[TableRow],
        width: usize,
    ) -> Result<(Vec<Vec<String>>, TablePlanOwners), ConversionError> {
        let mut occupancy = exact_zero_occupancy(width)?;
        let mut grid_slots = FixedSlots::new(rows.len(), "table row allocation failed")?;
        let mut actual = TablePlanOwners {
            occupancy: fixed_slot_bytes::<u32>(occupancy.capacity())?,
            grid_rows: fixed_slot_bytes::<Vec<String>>(rows.len())?,
            ..TablePlanOwners::default()
        };
        for row in rows {
            let mut rendered = exact_empty_strings(width, "table cell-slot allocation failed")?;
            actual.grid_slots = actual
                .grid_slots
                .checked_add(fixed_slot_bytes::<String>(rendered.capacity())?)
                .ok_or_else(render_plan_overflow)?;
            let mut column = 0_usize;
            for cell in &row.cells {
                while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                    column = column.checked_add(1).ok_or_else(render_plan_overflow)?;
                }
                let span = usize::try_from(cell.column_span)
                    .map_err(|_| render_error("table column span cannot be represented"))?;
                let end = column
                    .checked_add(span)
                    .ok_or_else(|| render_error("table width overflowed"))?;
                if end > width {
                    return Err(render_error("table width changed after deterministic preflight"));
                }
                let (value, temporary) = self.render_cell(cell)?;
                actual.cell_strings = actual
                    .cell_strings
                    .checked_add(
                        u64::try_from(value.capacity()).map_err(|_| render_plan_overflow())?,
                    )
                    .ok_or_else(render_plan_overflow)?;
                actual.cell_block_joins = actual
                    .cell_block_joins
                    .checked_add(temporary)
                    .ok_or_else(render_plan_overflow)?;
                rendered[column] = value;
                occupancy[column..end].fill(cell.row_span);
                column = end;
            }
            for remaining in &mut occupancy {
                *remaining = remaining.saturating_sub(1);
            }
            grid_slots.push(rendered)?;
        }
        let grid = grid_slots.into_vec()?;
        if grid.capacity() != rows.len() {
            return Err(render_error("table row allocation lost its exact capacity"));
        }
        Ok((grid, actual))
    }

    fn render_cell(&self, cell: &Cell) -> Result<(String, u64), ConversionError> {
        let rendered = self.render_blocks_in(&cell.blocks, InlineContext::TableCell)?;
        let normalized = normalize_lf(&rendered);
        let paragraphs = normalized.replace("\n\n", "<br><br>");
        let flattened = paragraphs.replace('\n', "<br>");
        let base_temporary = [rendered.capacity(), normalized.capacity(), paragraphs.capacity()]
            .into_iter()
            .try_fold(0_u64, |total, capacity| {
                total
                    .checked_add(u64::try_from(capacity).map_err(|_| render_plan_overflow())?)
                    .ok_or_else(render_plan_overflow)
            })?;
        let mut wrapper_peak =
            u64::try_from(flattened.capacity()).map_err(|_| render_plan_overflow())?;
        let mut rendered = if cell.row_span > 1 || cell.column_span > 1 {
            format!(
                "<span data-rowspan=\"{}\" data-colspan=\"{}\">{flattened}</span>",
                cell.row_span, cell.column_span
            )
        } else {
            flattened
        };
        if cell.header {
            let wrapped = format!("<strong>{rendered}</strong>");
            wrapper_peak = wrapper_peak.max(
                u64::try_from(rendered.capacity())
                    .ok()
                    .and_then(|left| {
                        u64::try_from(wrapped.capacity())
                            .ok()
                            .and_then(|right| left.checked_add(right))
                    })
                    .ok_or_else(render_plan_overflow)?,
            );
            rendered = wrapped;
        }
        let exact = exact_string(&rendered, "table cell string allocation failed")?;
        wrapper_peak = wrapper_peak.max(
            u64::try_from(rendered.capacity())
                .ok()
                .and_then(|left| {
                    u64::try_from(exact.capacity()).ok().and_then(|right| left.checked_add(right))
                })
                .ok_or_else(render_plan_overflow)?,
        );
        Ok((exact, base_temporary.checked_add(wrapper_peak).ok_or_else(render_plan_overflow)?))
    }

    fn render_image(&self, id: &str, alt: Option<&str>) -> Result<String, ConversionError> {
        let alt = escape_image_alt(&single_line(alt.unwrap_or_default()));
        if self.options.output.asset_mode == AssetMode::Omit {
            return Ok(alt);
        }
        let reference = self
            .plan
            .reference(id)
            .ok_or_else(|| render_error(format!("image references missing asset {id}")))?;
        let asset = &self.assets[reference.source_index];
        let target = match self.options.output.asset_mode {
            AssetMode::Omit => return Ok(alt),
            AssetMode::Embed | AssetMode::Extract if asset.bytes.is_empty() => asset
                .external_uri
                .as_deref()
                .ok_or_else(|| render_error(format!("asset {id} has neither bytes nor URI")))?
                .to_owned(),
            AssetMode::Embed if !asset.bytes.is_empty() => format!(
                "data:{};base64,{}",
                asset.media_type,
                base64::engine::general_purpose::STANDARD.encode(&asset.bytes)
            ),
            AssetMode::Embed => {
                return Err(render_error(format!("asset {id} has no bytes or external URI")));
            }
            AssetMode::Extract => self
                .plan
                .uri(id)
                .ok_or_else(|| render_error(format!("asset {id} has no extracted URI")))?
                .to_owned(),
        };
        let destination =
            if self.options.output.asset_mode == AssetMode::Embed && !asset.bytes.is_empty() {
                escape_generated_destination(&target)
            } else {
                validate_link_target(&target)?;
                escape_destination(&target, InlineContext::Normal)
            };
        Ok(format!("![{alt}](<{destination}>)"))
    }
}

fn page_render_identity(node: &BlockNode) -> (&'static str, &'static str) {
    if node.provenance.provider == "builtin.converter.image" || node.id.0.starts_with("image-page-")
    {
        ("image-frame", "Image frame")
    } else {
        // Preserve the established PDF anchor contract for PDF and legacy Page
        // producers. New source families opt in through an audited identity.
        ("pdf-page", "Page")
    }
}

fn render_inlines(inlines: &[Inline], context: InlineContext) -> Result<String, ConversionError> {
    let mut output = String::new();
    for inline in inlines {
        match inline {
            Inline::Text { value, marks }
            | Inline::SourceText { value, marks, .. }
            | Inline::OcrText { value, marks, .. } => {
                output.push_str(&render_marked_text(value, marks, context));
            }
            Inline::Code(value) => output.push_str(&render_code_span(value, context)),
            Inline::Link { target, content } => {
                validate_link_target(target)?;
                output.push('[');
                output.push_str(&render_inlines(content, context)?);
                output.push_str("](<");
                output.push_str(&escape_destination(target, context));
                output.push_str(">)");
            }
            Inline::Formula(value) => {
                let code = render_code_span(value, context);
                output.push('$');
                output.push_str(&code);
                output.push('$');
            }
            Inline::FootnoteReference(label) => {
                output.push_str("[^");
                output.push_str(&footnote_label(label));
                output.push(']');
            }
            Inline::LineBreak => output.push_str("  \n"),
            _ => {
                return Err(render_error("document contains an unsupported future inline variant"));
            }
        }
    }
    Ok(output)
}

fn render_marked_text(value: &str, marks: &[InlineMark], context: InlineContext) -> String {
    let mut rendered = escape_text(&single_line(value), context);
    let marks = marks.iter().copied().collect::<BTreeSet<_>>();
    for mark in [
        InlineMark::Subscript,
        InlineMark::Superscript,
        InlineMark::Underline,
        InlineMark::Strikethrough,
        InlineMark::Italic,
        InlineMark::Bold,
    ] {
        if marks.contains(&mark) {
            rendered = match mark {
                InlineMark::Bold => format!("<strong>{rendered}</strong>"),
                InlineMark::Italic => format!("<em>{rendered}</em>"),
                InlineMark::Strikethrough => format!("<del>{rendered}</del>"),
                InlineMark::Underline => format!("<u>{rendered}</u>"),
                InlineMark::Superscript => format!("<sup>{rendered}</sup>"),
                InlineMark::Subscript => format!("<sub>{rendered}</sub>"),
            };
        }
    }
    rendered
}

fn render_code_span(value: &str, context: InlineContext) -> String {
    let mut value = single_line(value);
    if value.is_empty() || value.chars().all(char::is_whitespace) {
        return format!("<code>{}</code>", escape_html_code(&value, context));
    }
    if context == InlineContext::TableCell {
        value = value.replace('|', "\\|");
    }
    let fence = "`".repeat(longest_run(&value, '`').saturating_add(1).max(1));
    let padding = value.starts_with([' ', '`']) || value.ends_with([' ', '`']);
    if padding { format!("{fence} {value} {fence}") } else { format!("{fence}{value}{fence}") }
}

fn render_fence(value: &str, info: Option<&str>) -> String {
    let value = normalize_lf(value);
    let fence = "`".repeat(longest_run(&value, '`').saturating_add(1).max(3));
    let mut output = fence.clone();
    if let Some(info) = info.filter(|value| !value.is_empty()) {
        output.push_str(info);
    }
    output.push('\n');
    output.push_str(&value);
    if !value.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&fence);
    output
}

fn sanitize_info_string(value: &str) -> String {
    encode_bytes(value.trim(), |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'-' | b'.' | b'#')
    })
}

fn footnote_label(value: &str) -> String {
    let mut output = String::from("fn-");
    for byte in value.as_bytes() {
        output.push(hex_digit(byte >> 4, false));
        output.push(hex_digit(byte & 0x0f, false));
    }
    output
}

fn escape_text(value: &str, _: InlineContext) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if character == '&' {
            output.push_str("&amp;");
            continue;
        }
        if matches!(
            character,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '<'
                | '>'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
                | '~'
        ) {
            output.push('\\');
        }
        output.push(character);
    }
    output
}

fn escape_image_alt(value: &str) -> String {
    escape_text(value, InlineContext::Normal).replace('"', "&quot;")
}

fn escape_destination(value: &str, context: InlineContext) -> String {
    let value = normalize_lf(value).replace('&', "&amp;");
    encode_bytes(&value, |byte| {
        !byte.is_ascii_control()
            && !matches!(byte, b' ' | b'<' | b'>' | b'\\')
            && (context != InlineContext::TableCell || byte != b'|')
    })
}

fn escape_generated_destination(value: &str) -> String {
    encode_bytes(value, |byte| {
        !byte.is_ascii_control() && !matches!(byte, b' ' | b'<' | b'>' | b'\\')
    })
}

fn escape_html_code(value: &str, _: InlineContext) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            _ => output.push(character),
        }
    }
    output
}

fn encode_bytes(value: &str, safe: impl Fn(u8) -> bool) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if safe(byte) {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(hex_digit(byte >> 4, true));
            output.push(hex_digit(byte & 0x0f, true));
        }
    }
    output
}

fn hex_digit(value: u8, uppercase: bool) -> char {
    let alphabet = if uppercase { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    char::from(alphabet[usize::from(value)])
}

fn indent_continuation(value: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);
    value.replace('\n', &format!("\n{indent}"))
}

fn indent_all(value: &str, spaces: usize) -> String {
    format!("{}{}", " ".repeat(spaces), indent_continuation(value, spaces))
}

fn with_body(heading: String, body: &str) -> String {
    if body.is_empty() { heading } else { format!("{heading}\n\n{body}") }
}

fn valid_media_type(value: &str) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else {
        return false;
    };
    !kind.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-' | b'/'
                )
        })
}

fn normalize_media_type(value: &str) -> Result<String, ConversionError> {
    if !valid_media_type(value) {
        return Err(asset_plan_error(
            "invalidAssetMediaType",
            format!("invalid asset media type {value}"),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn media_type_extension(media_type: &str) -> Option<String> {
    let extension = match media_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/avif" => "avif",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/zip" => "zip",
        _ => return None,
    };
    Some(extension.into())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(hex_digit(byte >> 4, false));
        output.push(hex_digit(byte & 0x0f, false));
    }
    output
}

fn validate_asset_uri_prefix(prefix: Option<&str>) -> Result<(), ConversionError> {
    let Some(prefix) = prefix else { return Ok(()) };
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty()
        || prefix.starts_with(['/', '\\'])
        || prefix.starts_with("//")
        || prefix.contains(['\\', '?', '#'])
        || prefix.chars().any(char::is_control)
        || contains_html_entity(prefix)
    {
        return Err(asset_plan_error("unsafeAssetUriPrefix", "unsafe asset URI prefix"));
    }
    if prefix.split('/').any(|segment| segment.is_empty() || segment == ".") {
        return Err(asset_plan_error(
            "unsafeAssetUriPrefix",
            "asset URI prefix is not a portable relative path",
        ));
    }
    if prefix.find(':').is_some() {
        return Err(asset_plan_error(
            "unsafeAssetUriPrefix",
            "absolute and scheme-bearing asset URI prefixes are not permitted",
        ));
    }
    Ok(())
}

fn validate_external_uri(uri: Option<&str>, id: &str) -> Result<(), ConversionError> {
    let Some(uri) = uri else { return Ok(()) };
    validate_link_target(uri).and_then(|()| validate_external_asset_uri(uri)).map_err(|_| {
        asset_plan_error("unsafeExternalAssetUri", format!("asset {id} has an unsafe external URI"))
    })
}

fn validate_external_asset_uri(value: &str) -> Result<(), ConversionError> {
    if canonical_external_asset_uri(value).as_deref() != Some(value) {
        return Err(render_error("external asset URI is not canonical safe HTTP(S)"));
    }
    Ok(())
}

fn validate_link_target(value: &str) -> Result<(), ConversionError> {
    if value.chars().any(char::is_control) {
        return Err(render_error("link target contains a control character"));
    }
    if contains_html_entity(value) {
        return Err(render_error("link target contains an HTML character reference"));
    }
    let Some(colon) = value.find(':') else {
        return Ok(());
    };
    let scheme = &value[..colon];
    if scheme.is_empty()
        || !scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic() || index > 0 && matches!(byte, b'+' | b'-' | b'.')
        })
    {
        return Ok(());
    }
    if matches!(scheme.to_ascii_lowercase().as_str(), "javascript" | "vbscript" | "data" | "file") {
        return Err(render_error(format!("link target uses disallowed scheme {scheme}")));
    }
    if let Some(authority) =
        value[colon + 1..].strip_prefix("//").and_then(|rest| rest.split('/').next())
        && authority.contains('@')
    {
        return Err(render_error("link target authority contains user information"));
    }
    Ok(())
}

fn contains_html_entity(value: &str) -> bool {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'&' {
            continue;
        }
        let tail = &bytes[index + 1..];
        let Some(end) = tail.iter().position(|byte| *byte == b';') else {
            continue;
        };
        if end == 0 || end > 32 {
            continue;
        }
        let body = &tail[..end];
        let named = body.iter().all(u8::is_ascii_alphanumeric);
        let numeric = body
            .strip_prefix(b"#")
            .is_some_and(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit));
        let hexadecimal = body
            .strip_prefix(b"#x")
            .or_else(|| body.strip_prefix(b"#X"))
            .is_some_and(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_hexdigit));
        if named || numeric || hexadecimal {
            return true;
        }
    }
    false
}

fn timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = milliseconds / 60_000 % 60;
    let seconds = milliseconds / 1_000 % 60;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

fn single_line(value: &str) -> String {
    normalize_lf(value).replace('\n', " ")
}

fn normalize_lf(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn longest_run(value: &str, needle: char) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in value.chars() {
        if character == needle {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn safe_extension(value: Option<&str>) -> Option<String> {
    let filename = value?.rsplit(['/', '\\']).next()?;
    let (_, extension) = filename.rsplit_once('.')?;
    (!extension.is_empty()
        && extension.len() <= 16
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    .then(|| extension.to_ascii_lowercase())
}

fn join_uri_prefix(prefix: Option<&str>, filename: &str) -> String {
    prefix.map_or_else(
        || filename.to_owned(),
        |prefix| format!("{}/{}", prefix.trim_end_matches('/'), filename),
    )
}

fn trim_blank_lines(value: &mut String) {
    let length = value.trim_end_matches('\n').len();
    value.truncate(length);
}

fn render_error(detail: impl Into<String>) -> ConversionError {
    ConversionError::Internal { detail: format!("Markdown rendering failed: {}", detail.into()) }
}

fn asset_plan_error(code: &str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Internal {
        detail: format!("asset planning failed ({code}): {}", detail.into()),
    }
}

fn validate_planned_references<'a>(
    nodes: &[BlockNode],
    plan: &'a AssetPlan,
    referenced_assets: &mut BTreeSet<&'a str>,
) -> Result<(), ConversionError> {
    for node in nodes {
        match &node.block {
            Block::Image { asset, .. } => {
                let (id, _) = plan.by_id.get_key_value(&asset.0).ok_or_else(|| {
                    render_error(format!("image references missing asset {}", asset.0))
                })?;
                referenced_assets.insert(id.as_str());
            }
            Block::List { items, .. } => {
                for item in items {
                    validate_planned_references(&item.blocks, plan, referenced_assets)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    for cell in &row.cells {
                        validate_planned_references(&cell.blocks, plan, referenced_assets)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                validate_planned_references(blocks, plan, referenced_assets)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        AssetId, CellRef, DocumentMetadata, NodeId, OcrEvidence, OcrEvidenceStage, OcrEvidenceStep,
        OcrSourceRegion, Provenance, ProvenanceKind, Rect, SourceLocator, SourcePoint, TimeRange,
    };
    use pulldown_cmark::{Event, Options, Parser, Tag};

    fn provenance() -> Provenance {
        Provenance {
            kind: ProvenanceKind::NativeParser,
            provider: "test.parser".into(),
            locator: SourceLocator::default(),
            confidence: Some(1.0),
        }
    }

    fn node(id: impl Into<String>, block: Block) -> BlockNode {
        BlockNode { id: NodeId(id.into()), block, provenance: provenance() }
    }

    #[test]
    fn structured_ocr_text_renders_as_text_without_leaking_evidence() {
        let provenance = Provenance {
            kind: ProvenanceKind::LocalOcr,
            provider: "recognizer".into(),
            locator: SourceLocator {
                page: Some(1),
                bounds: Some(Rect { x: 0.0, y: 0.0, width: 10.0, height: 2.0 }),
                page_width: Some(100.0),
                page_height: Some(100.0),
                ..SourceLocator::default()
            },
            confidence: Some(0.9),
        };
        let inline = Inline::OcrText {
            value: "scanned **text**".into(),
            marks: vec![],
            provenance: Box::new(provenance.clone()),
            evidence: Box::new(OcrEvidence {
                page: 1,
                regions: vec![OcrSourceRegion {
                    source_index: 0,
                    polygon: [
                        SourcePoint { x: 0.0, y: 0.0 },
                        SourcePoint { x: 10.0, y: 0.0 },
                        SourcePoint { x: 10.0, y: 2.0 },
                        SourcePoint { x: 0.0, y: 2.0 },
                    ],
                    detection_confidence: 0.9,
                    recognition_confidence: 0.9,
                }],
                chain: vec![
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Detection,
                        provider: "detector".into(),
                        model: Some("det".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Recognition,
                        provider: "recognizer".into(),
                        model: Some("rec".into()),
                    },
                    OcrEvidenceStep {
                        stage: OcrEvidenceStage::Merge,
                        provider: "merge".into(),
                        model: None,
                    },
                ],
            }),
        };
        let document = Document {
            blocks: vec![BlockNode {
                id: NodeId("ocr".into()),
                block: Block::Paragraph(vec![inline]),
                provenance,
            }],
            ..Document::default()
        };
        let markdown = render(&document, &[], &ConversionOptions::default()).unwrap();
        assert_eq!(markdown, "scanned \\*\\*text\\*\\*\n");
        assert!(!markdown.contains("detector"));
    }

    fn paragraph(id: &str, value: &str) -> BlockNode {
        node(id, Block::Paragraph(vec![Inline::Text { value: value.into(), marks: vec![] }]))
    }

    fn document(blocks: Vec<BlockNode>) -> Document {
        Document { blocks, ..Document::default() }
    }

    fn output(document: &Document) -> String {
        render(document, &[], &ConversionOptions::default()).unwrap()
    }

    #[test]
    fn empty_document_is_empty_and_material_document_is_rendered() {
        assert_eq!(output(&Document::default()), "");
        assert_eq!(output(&document(vec![paragraph("p", "hello")])), "hello\n");
    }

    #[test]
    fn headings_rich_text_links_code_formula_and_breaks_have_stable_golden() {
        let content = vec![
            Inline::Text {
                value: "a*[x]\r\nb".into(),
                marks: vec![InlineMark::Underline, InlineMark::Bold, InlineMark::Italic],
            },
            Inline::Code(" a``b ".into()),
            Inline::Link {
                target: "https://e.invalid/a b>x".into(),
                content: vec![Inline::Text { value: "link]".into(), marks: vec![] }],
            },
            Inline::Formula("x`y".into()),
            Inline::LineBreak,
            Inline::Text { value: "tail".into(), marks: vec![InlineMark::Strikethrough] },
        ];
        let doc = document(vec![node("h", Block::Heading { level: 3, content })]);
        assert_eq!(
            output(&doc),
            "### <strong><em><u>a\\*\\[x\\] b</u></em></strong>```  a``b  ```[link\\]](<https://e.invalid/a%20b%3Ex>)$``x`y``$  \n<del>tail</del>\n"
        );
    }

    #[test]
    fn nested_lists_and_empty_items_are_deterministic() {
        let nested = node(
            "nested",
            Block::List {
                kind: ListKind::Bullet,
                start: 1,
                items: vec![ListItem {
                    checked: None,
                    marker_label: None,
                    blocks: vec![paragraph("np", "nested")],
                }],
            },
        );
        let doc = document(vec![node(
            "list",
            Block::List {
                kind: ListKind::Task,
                start: 1,
                items: vec![
                    ListItem {
                        checked: Some(true),
                        marker_label: Some("ignored safely".into()),
                        blocks: vec![paragraph("p", "done"), nested],
                    },
                    ListItem { checked: Some(false), marker_label: None, blocks: vec![] },
                ],
            },
        )]);
        assert_eq!(
            output(&doc),
            "- [x] <!-- source-marker: ignored%20safely --> done\n    \n    - nested\n- [ ]\n"
        );
    }

    #[test]
    fn ordered_lists_and_all_inline_marks_have_canonical_output() {
        let doc = document(vec![
            node(
                "ordered",
                Block::List {
                    kind: ListKind::Ordered,
                    start: 7,
                    items: vec![
                        ListItem {
                            checked: None,
                            marker_label: None,
                            blocks: vec![paragraph("a", "seven")],
                        },
                        ListItem {
                            checked: None,
                            marker_label: None,
                            blocks: vec![paragraph("b", "eight")],
                        },
                    ],
                },
            ),
            node(
                "marks",
                Block::Paragraph(vec![
                    Inline::Text { value: "sup".into(), marks: vec![InlineMark::Superscript] },
                    Inline::Text { value: "sub".into(), marks: vec![InlineMark::Subscript] },
                ]),
            ),
        ]);
        assert_eq!(output(&doc), "7. seven\n8. eight\n\n<sup>sup</sup><sub>sub</sub>\n");
    }

    #[test]
    fn tables_expand_spans_flatten_blocks_and_escape_content() {
        let rows = vec![
            TableRow {
                cells: vec![
                    Cell {
                        row_span: 2,
                        column_span: 1,
                        header: true,
                        blocks: vec![paragraph("a", "A|x")],
                    },
                    Cell {
                        row_span: 1,
                        column_span: 2,
                        header: true,
                        blocks: vec![paragraph("b", "B\r\nline")],
                    },
                ],
            },
            TableRow {
                cells: vec![
                    Cell {
                        row_span: 1,
                        column_span: 1,
                        header: false,
                        blocks: vec![paragraph("c", "C")],
                    },
                    Cell {
                        row_span: 1,
                        column_span: 1,
                        header: false,
                        blocks: vec![paragraph("d", "D")],
                    },
                ],
            },
        ];
        assert_eq!(
            output(&document(vec![node("t", Block::Table { rows, alignments: vec![] })])),
            "| <strong><span data-rowspan=\"2\" data-colspan=\"1\">A\\|x</span></strong> | <strong><span data-rowspan=\"1\" data-colspan=\"2\">B line</span></strong> |  |\n| --- | --- | --- |\n|  | C | D |\n"
        );
    }

    #[test]
    fn table_span_intersections_preserve_a_rectangular_grid() {
        let cell = |id: &str, value: &str, row_span, column_span| Cell {
            row_span,
            column_span,
            header: false,
            blocks: vec![paragraph(id, value)],
        };
        let rows = vec![
            TableRow { cells: vec![cell("a", "A", 2, 2), cell("b", "B", 1, 2)] },
            TableRow { cells: vec![cell("c", "C", 2, 1), cell("d", "D", 1, 1)] },
            TableRow {
                cells: vec![cell("e", "E", 1, 1), cell("f", "F", 1, 1), cell("g", "G", 1, 1)],
            },
        ];
        assert_eq!(
            output(&document(vec![node("t", Block::Table { rows, alignments: vec![] })])),
            "|  |  |  |  |\n| --- | --- | --- | --- |\n| <span data-rowspan=\"2\" data-colspan=\"2\">A</span> |  | <span data-rowspan=\"1\" data-colspan=\"2\">B</span> |  |\n|  |  | <span data-rowspan=\"2\" data-colspan=\"1\">C</span> | D |\n| E | F |  | G |\n"
        );
    }

    #[test]
    fn code_fences_are_longer_than_malicious_content_and_use_lf() {
        let doc = document(vec![node(
            "code",
            Block::Code {
                language: Some("rust ``` evil\ninfo".into()),
                text: "a\r\n```\rbody".into(),
            },
        )]);
        assert_eq!(output(&doc), "````rust%20%60%60%60%20evil%0Ainfo\na\n```\nbody\n````\n");
    }

    #[test]
    fn empty_and_whitespace_inline_code_have_exact_commonmark_semantics() {
        for value in ["", " ", "   ", "\t"] {
            let doc = document(vec![node("p", Block::Paragraph(vec![Inline::Code(value.into())]))]);
            let markdown = output(&doc);
            let html = {
                let parser = Parser::new(&markdown);
                let mut html = String::new();
                pulldown_cmark::html::push_html(&mut html, parser);
                html
            };
            assert!(html.contains("<code>"));
            assert!(html.contains("</code>"));
            assert_eq!(html.matches("<code>").count(), 1);
        }
        assert_eq!(
            output(&document(vec![
                node("p", Block::Paragraph(vec![Inline::Code(String::new())]),)
            ])),
            "<code></code>\n"
        );
    }

    #[test]
    fn marked_boundary_whitespace_remains_inside_the_mark() {
        let doc = document(vec![node(
            "p",
            Block::Paragraph(vec![Inline::Text {
                value: "\u{2003} x \u{2003}".into(),
                marks: vec![InlineMark::Bold, InlineMark::Italic, InlineMark::Strikethrough],
            }]),
        )]);
        let markdown = output(&doc);
        assert_eq!(markdown, "<strong><em><del>\u{2003} x \u{2003}</del></em></strong>\n");
        let mut html = String::new();
        pulldown_cmark::html::push_html(&mut html, Parser::new(&markdown));
        assert!(html.contains("<strong><em><del>\u{2003} x \u{2003}</del></em></strong>"));
    }

    #[test]
    fn table_context_preserves_code_backslashes_pipes_and_link_destinations() {
        let rows = vec![TableRow {
            cells: vec![Cell {
                row_span: 1,
                column_span: 1,
                header: false,
                blocks: vec![node(
                    "p",
                    Block::Paragraph(vec![
                        Inline::Code(r"a\|b".into()),
                        Inline::Text { value: " / ".into(), marks: vec![] },
                        Inline::Link {
                            target: "https://example.invalid/a|b".into(),
                            content: vec![Inline::Text { value: "link".into(), marks: vec![] }],
                        },
                    ]),
                )],
            }],
        }];
        let markdown =
            output(&document(vec![node("t", Block::Table { rows, alignments: vec![] })]));
        assert!(markdown.contains(r"`a\\|b`"));
        assert!(markdown.contains("https://example.invalid/a%7Cb"));
        let mut html = String::new();
        pulldown_cmark::html::push_html(
            &mut html,
            Parser::new_ext(&markdown, Options::ENABLE_TABLES),
        );
        assert!(html.contains(r"<code>a\|b</code>"));
        assert!(html.contains("href=\"https://example.invalid/a%7Cb\""));
    }

    #[test]
    fn asset_filenames_are_bounded_ascii_and_cross_platform() {
        let long_id = "\0😀".repeat(10_000);
        let cases = [
            ("CON", Some("dir\\evil.PNG")),
            ("con", Some("dir/evil.png")),
            (long_id.as_str(), Some("name.超长扩展名")),
            ("Case", Some("trailing.")),
        ];
        let names =
            cases.iter().map(|(id, suggested)| asset_filename(id, *suggested)).collect::<Vec<_>>();
        for name in &names {
            assert!(name.is_ascii());
            assert!(name.len() <= 87);
            assert!(name.starts_with("asset-"));
            assert!(!name.ends_with('.'));
        }
        assert_ne!(names[0].to_ascii_lowercase(), names[1].to_ascii_lowercase());
        assert_eq!(asset_filename("CON", Some("dir\\evil.PNG")), names[0]);
    }

    #[test]
    fn content_plan_deduplicates_ids_and_uses_mime_authoritatively() {
        let doc = document(vec![
            node("a", Block::Image { asset: AssetId("upper".into()), alt: None }),
            node("b", Block::Image { asset: AssetId("lower".into()), alt: None }),
        ]);
        let assets = [
            Asset {
                id: AssetId("upper".into()),
                filename: Some("CON.HTML".into()),
                media_type: "IMAGE/PNG".into(),
                bytes: vec![1, 2, 3],
                external_uri: None,
            },
            Asset {
                id: AssetId("lower".into()),
                filename: Some("../aux.jpg:stream".into()),
                media_type: "image/png".into(),
                bytes: vec![1, 2, 3],
                external_uri: None,
            },
        ];
        let plan = plan_assets(&doc, &assets, &ConversionOptions::default()).unwrap();
        assert_eq!(plan.entries().len(), 1);
        assert_eq!(plan.entries()[0].asset_ids, ["lower", "upper"]);
        assert!(
            std::path::Path::new(&plan.entries()[0].filename)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        );
        assert_eq!(plan.entries()[0].sha256.len(), 64);
        assert_eq!(plan.uri("lower"), plan.uri("upper"));
    }

    #[test]
    fn content_plan_rejects_conflicting_metadata_and_unsafe_prefixes() {
        let doc = document(vec![node("a", Block::Image { asset: AssetId("a".into()), alt: None })]);
        let asset = |id: &str, media_type: &str| Asset {
            id: AssetId(id.into()),
            filename: None,
            media_type: media_type.into(),
            bytes: vec![7],
            external_uri: None,
        };
        let error = plan_assets(
            &doc,
            &[asset("a", "image/png"), asset("b", "image/jpeg")],
            &ConversionOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("assetMetadataConflict"));

        for prefix in ["/tmp/assets", "//server/share", "C:/assets", "data:x"] {
            let mut options = ConversionOptions::default();
            options.output.asset_uri_prefix = Some(prefix.into());
            assert!(plan_assets(&doc, &[asset("a", "image/png")], &options).is_err(), "{prefix}");
        }
    }

    #[test]
    fn commonmark_decodes_colon_entities_but_renderer_rejects_them() {
        for entity in ["&colon;", "&#58;", "&#x3a;"] {
            let markdown = format!("[x](<javascript{entity}alert(1)>)");
            let decoded = Parser::new(&markdown).find_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
                _ => None,
            });
            assert_eq!(decoded.as_deref(), Some("javascript:alert(1)"));

            let doc = document(vec![node(
                "p",
                Block::Paragraph(vec![Inline::Link {
                    target: format!("javascript{entity}alert(1)"),
                    content: vec![Inline::Text { value: "x".into(), marks: vec![] }],
                }]),
            )]);
            assert!(render(&doc, &[], &ConversionOptions::default()).is_err());
        }
    }

    #[test]
    fn formula_footnote_and_container_nodes_have_stable_golden() {
        let reference = paragraph("ref", "note");
        let mut reference = reference;
        reference.block = Block::Paragraph(vec![Inline::FootnoteReference("n ]\r\n".into())]);
        let doc = document(vec![
            node("formula", Block::Formula("x```y\r\nz".into())),
            reference,
            node(
                "foot",
                Block::Footnote { label: "n ]\r\n".into(), blocks: vec![paragraph("fp", "foot")] },
            ),
            node("page", Block::Page { number: 2, blocks: vec![paragraph("pp", "page")] }),
            node(
                "slide",
                Block::Slide { number: 3, title: Some("A # title".into()), blocks: vec![] },
            ),
            node(
                "sheet",
                Block::Sheet { name: "Data | 2026".into(), blocks: vec![paragraph("sp", "sheet")] },
            ),
            node(
                "time",
                Block::TimedSegment {
                    range: TimeRange { start_ms: 3_661_002, end_ms: 3_662_003 },
                    speaker: Some("A*B".into()),
                    speaker_confidence: None,
                    tokens: Vec::new(),
                    content: vec![Inline::Text { value: "hello".into(), marks: vec![] }],
                },
            ),
            node("rule", Block::Rule),
        ]);
        assert_eq!(
            output(&doc),
            "````math\nx```y\nz\n````\n\n[^fn-6e205d0d0a]\n\n[^fn-6e205d0d0a]: foot\n\n<a id=\"pdf-page-2\"></a>\n\n## Page 2\n\npage\n\n## Slide 3: A \\# title\n\n## Sheet: Data \\| 2026\n\nsheet\n\n`01:01:01.002 – 01:01:02.003` **A\\*B:** hello\n\n---\n"
        );
    }

    #[test]
    fn anonymous_speaker_ids_have_readable_defaults_and_metadata_only_labels() {
        let mut doc = document(vec![node(
            "time",
            Block::TimedSegment {
                range: TimeRange { start_ms: 1_000, end_ms: 2_000 },
                speaker: Some("speaker-1".into()),
                speaker_confidence: Some(0.8),
                tokens: Vec::new(),
                content: vec![Inline::Text { value: "hello".into(), marks: vec![] }],
            },
        )]);
        assert_eq!(output(&doc), "`00:00:01.000 – 00:00:02.000` **Speaker 1:** hello\n");
        doc.metadata.properties.insert("media.speaker.speaker-1.label".into(), "张 *三*".into());
        assert_eq!(output(&doc), "`00:00:01.000 – 00:00:02.000` **张 \\*三\\*:** hello\n");
    }

    #[test]
    fn images_follow_extract_embed_and_omit_policies_without_writing() {
        let image = document(vec![node(
            "image",
            Block::Image { asset: AssetId("img".into()), alt: Some("a]lt\r\n".into()) },
        )]);
        let asset = Asset {
            id: AssetId("img".into()),
            filename: Some("../bad:name.png".into()),
            media_type: "image/png".into(),
            bytes: vec![0, 1, 2],
            external_uri: None,
        };
        let mut options = ConversionOptions::default();
        options.output.asset_uri_prefix = Some("assets/".into());
        assert_eq!(
            render(&image, std::slice::from_ref(&asset), &options).unwrap(),
            format!(
                "![a\\]lt ](<{}>)\n",
                plan_assets(&image, std::slice::from_ref(&asset), &options)
                    .unwrap()
                    .uri("img")
                    .unwrap()
            )
        );
        options.output.asset_mode = AssetMode::Embed;
        assert_eq!(
            render(&image, std::slice::from_ref(&asset), &options).unwrap(),
            "![a\\]lt ](<data:image/png;base64,AAEC>)\n"
        );
        options.output.asset_mode = AssetMode::Omit;
        assert_eq!(render(&image, std::slice::from_ref(&asset), &options).unwrap(), "a\\]lt \n");
    }

    #[test]
    fn malformed_documents_and_asset_inventories_return_stable_errors() {
        let invalid = Document { schema_version: 99, ..Document::default() };
        assert_eq!(
            render(&invalid, &[], &ConversionOptions::default()).unwrap_err().code().as_str(),
            "internal"
        );
        let image = document(vec![node(
            "image",
            Block::Image { asset: AssetId("missing".into()), alt: None },
        )]);
        assert_eq!(
            render(&image, &[], &ConversionOptions::default()).unwrap_err().code().as_str(),
            "internal"
        );
        let duplicate = Asset {
            id: AssetId("x".into()),
            filename: None,
            media_type: "x/test".into(),
            bytes: vec![],
            external_uri: None,
        };
        assert!(
            render(
                &Document::default(),
                &[duplicate.clone(), duplicate],
                &ConversionOptions::default()
            )
            .is_err()
        );
    }

    #[test]
    fn unsafe_links_entities_and_mime_types_fail_stably() {
        for target in [
            "javascript:alert(1)",
            "javascript&colon;alert(1)",
            "javascript&#58;alert(1)",
            "javascript&#x3a;alert(1)",
            "DATA:text/html,x",
            "file:///etc/passwd",
            "https://user@example.invalid/x",
            "https://example.invalid/a\nheader",
        ] {
            let linked = document(vec![node(
                "p",
                Block::Paragraph(vec![Inline::Link {
                    target: target.into(),
                    content: vec![Inline::Text { value: "x".into(), marks: vec![] }],
                }]),
            )]);
            assert_eq!(
                render(&linked, &[], &ConversionOptions::default()).unwrap_err().code().as_str(),
                "internal"
            );
        }

        let asset = |id: &str, media_type: &str| Asset {
            id: AssetId(id.into()),
            filename: Some("same?.png".into()),
            media_type: media_type.into(),
            bytes: vec![1],
            external_uri: None,
        };
        let bad_mime = [asset("a", "image/png;base64,EVIL")];
        assert!(
            render(
                &document(vec![node("a", Block::Image { asset: AssetId("a".into()), alt: None })]),
                &bad_mime,
                &ConversionOptions::default()
            )
            .is_err()
        );
        for uri in [
            "https://example.invalid/x.png?token=secret",
            "https://example.invalid/x.png#fragment",
            "https://user@example.invalid/x.png",
            "file:///tmp/x.png",
        ] {
            let external = Asset {
                id: AssetId("external".into()),
                filename: None,
                media_type: "image/png".into(),
                bytes: vec![],
                external_uri: Some(uri.into()),
            };
            let image = document(vec![node(
                "external",
                Block::Image { asset: AssetId("external".into()), alt: None },
            )]);
            assert!(render(&image, &[external], &ConversionOptions::default()).is_err());
        }
    }

    #[test]
    fn external_only_images_render_original_uri_offline_in_extract_and_embed() {
        let image = document(vec![node(
            "page",
            Block::Page {
                number: 1,
                blocks: vec![node(
                    "image",
                    Block::Image { asset: AssetId("remote".into()), alt: Some("remote".into()) },
                )],
            },
        )]);
        let asset = Asset {
            id: AssetId("remote".into()),
            filename: Some("remote.png".into()),
            media_type: "image/png".into(),
            bytes: vec![],
            external_uri: Some("https://example.invalid/x.png".into()),
        };
        let mut options = ConversionOptions::default();
        options.output.asset_mode = AssetMode::Embed;
        assert_eq!(
            render(&image, std::slice::from_ref(&asset), &options).unwrap(),
            "<a id=\"pdf-page-1\"></a>\n\n## Page 1\n\n![remote](<https://example.invalid/x.png>)\n"
        );
        options.output.asset_mode = AssetMode::Extract;
        assert_eq!(
            render(&image, std::slice::from_ref(&asset), &options).unwrap(),
            "<a id=\"pdf-page-1\"></a>\n\n## Page 1\n\n![remote](<https://example.invalid/x.png>)\n"
        );
        options.output.asset_mode = AssetMode::Omit;
        assert!(render(&image, &[], &options).is_err());
        assert!(render(&image, &[asset], &options).is_ok());
    }

    #[test]
    fn unrelated_attachments_do_not_change_markdown_or_asset_mode_validation() {
        let doc = document(vec![paragraph("p", "body")]);
        let attachments = [
            Asset {
                id: AssetId("one".into()),
                filename: Some("same.png".into()),
                media_type: "image/png".into(),
                bytes: vec![],
                external_uri: Some("https://example.invalid/one.png".into()),
            },
            Asset {
                id: AssetId("two".into()),
                filename: Some("same.png".into()),
                media_type: "image/png".into(),
                bytes: vec![],
                external_uri: Some("https://example.invalid/two.png".into()),
            },
        ];
        assert_eq!(render(&doc, &attachments, &ConversionOptions::default()).unwrap(), "body\n");
    }

    #[test]
    fn metadata_order_does_not_change_body_and_mark_order_is_canonical() {
        let mut left = document(vec![node(
            "p",
            Block::Paragraph(vec![Inline::Text {
                value: "x".into(),
                marks: vec![InlineMark::Italic, InlineMark::Bold],
            }]),
        )]);
        left.metadata = DocumentMetadata {
            title: Some("title".into()),
            authors: vec!["author".into()],
            properties: BTreeMap::from([("z".into(), "1".into()), ("a".into(), "2".into())]),
        };
        let mut right = left.clone();
        if let Block::Paragraph(content) = &mut right.blocks[0].block
            && let Inline::Text { marks, .. } = &mut content[0]
        {
            marks.reverse();
        }
        assert_eq!(output(&left), output(&right));
        assert_eq!(output(&left), "<strong><em>x</em></strong>\n");
    }

    #[test]
    fn deepest_valid_nesting_renders_without_panicking() {
        let mut current = paragraph("leaf", "deep");
        for depth in (1..16).rev() {
            current = node(
                format!("list-{depth}"),
                Block::List {
                    kind: ListKind::Bullet,
                    start: 1,
                    items: vec![ListItem {
                        checked: None,
                        marker_label: None,
                        blocks: vec![current],
                    }],
                },
            );
        }
        let rendered = output(&document(vec![current]));
        assert!(rendered.contains("deep"));
        assert!(!rendered.contains('\r'));
    }

    #[test]
    fn source_locator_types_do_not_affect_markdown_but_remain_valid() {
        let mut block = paragraph("p", "located");
        block.provenance.locator.sheet = Some("Data".into());
        block.provenance.locator.cell = Some(CellRef { row: 4, column: 2 });
        assert_eq!(output(&document(vec![block])), "located\n");
    }

    #[test]
    fn builtin_renderer_plan_is_reserved_before_escape_heavy_construction() {
        let value = format!("{}\n{}", "&<>[]\\`*_{}#!|~".repeat(2_048), "x".repeat(16_384));
        let mut nested = node(
            "leaf",
            Block::Paragraph(vec![Inline::Text { value: value.clone(), marks: vec![] }]),
        );
        for depth in (1..16).rev() {
            nested = node(
                format!("list-{depth}"),
                Block::List {
                    kind: ListKind::Bullet,
                    start: 1,
                    items: vec![ListItem {
                        checked: None,
                        marker_label: None,
                        blocks: vec![nested],
                    }],
                },
            );
        }
        let document = document(vec![nested]);
        let renderer = GfmRenderer;
        let options = ConversionOptions::default();
        let measuring = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        let plan = renderer.planned_markdown_bytes(&document, &[], &options, &measuring).unwrap();
        assert!(plan > u64::try_from(value.len() * 16).unwrap());

        let low = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits { max_memory_bytes: plan - 1, ..Default::default() },
        );
        let calls = std::sync::atomic::AtomicUsize::new(0);
        assert!(low.reserve_memory(plan).is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        let exact = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits { max_memory_bytes: plan, ..Default::default() },
        );
        let mut parent = exact.reserve_memory(plan).unwrap();
        let credit = exact.with_memory_credit(&mut parent).unwrap();
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let markdown = render(&document, &[], &options).unwrap();
        assert!(u64::try_from(markdown.capacity()).unwrap() <= plan);
        drop(credit);
        parent.shrink(plan - u64::try_from(markdown.capacity()).unwrap()).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn table_plan_covers_each_live_owner_and_exact_preflight_boundary() {
        fn cell(text: &str, header: bool, row_span: u32, column_span: u32) -> Cell {
            Cell {
                row_span,
                column_span,
                header,
                blocks: if text.is_empty() {
                    Vec::new()
                } else {
                    text.split('\n')
                        .enumerate()
                        .map(|(line, value)| {
                            node(
                                format!(
                                    "cell-{}-{row_span}-{column_span}-{line}",
                                    text.replace('\n', "-")
                                ),
                                Block::Paragraph(vec![Inline::Text {
                                    value: value.into(),
                                    marks: vec![],
                                }]),
                            )
                        })
                        .collect()
                },
            }
        }

        fn assert_measured_owners(rows: &[TableRow], expected_width: usize) -> String {
            fn fixture_block_output(blocks: &[BlockNode]) -> Result<u64, ConversionError> {
                blocks.iter().try_fold(0_u64, |total, block| {
                    let Block::Paragraph(inlines) = &block.block else {
                        panic!("table capacity fixture only uses paragraph cells")
                    };
                    let rendered = inlines.iter().try_fold(0_u64, |total, inline| {
                        let Inline::Text { value, .. } = inline else {
                            panic!("table capacity fixture only uses text cells")
                        };
                        let source =
                            u64::try_from(value.len()).map_err(|_| render_plan_overflow())?;
                        total
                            .checked_add(
                                source
                                    .checked_mul(5)
                                    .and_then(|value| value.checked_add(6 * 13))
                                    .ok_or_else(render_plan_overflow)?,
                            )
                            .ok_or_else(render_plan_overflow)
                    })?;
                    total
                        .checked_add(rendered)
                        .and_then(|value| value.checked_add(2))
                        .ok_or_else(render_plan_overflow)
                })
            }

            let (width, has_header) = table_shape(rows).unwrap();
            assert_eq!(width, expected_width);
            assert!(!has_header);
            let options = ConversionOptions::default();
            let document = Document::default();
            let plan = plan_assets(&document, &[], &options).unwrap();
            let context =
                RenderContext { plan: &plan, assets: &[], options: &options, document: &document };
            let (markdown, actual) = context
                .render_table_measured(
                    rows,
                    &[TableAlignment::Center, TableAlignment::Right, TableAlignment::Left],
                )
                .unwrap();
            let (planned_cell_strings, planned_cell_block_joins) =
                table_cell_owner_bounds(rows, fixture_block_output).unwrap();
            let planned = table_plan_owners(
                rows,
                width,
                has_header,
                planned_cell_strings,
                planned_cell_block_joins,
            )
            .unwrap();
            assert_eq!(actual.occupancy, planned.occupancy);
            assert_eq!(actual.grid_rows, planned.grid_rows);
            assert_eq!(actual.grid_slots, planned.grid_slots);
            assert_eq!(actual.fallback_header, planned.fallback_header);
            assert_eq!(actual.separator_slots, planned.separator_slots);
            assert!(actual.separator_strings <= planned.separator_strings);
            assert!(actual.cell_strings <= planned.cell_strings);
            assert!(actual.cell_block_joins <= planned.cell_block_joins);
            assert!(actual.output <= planned.output);
            assert_eq!(u64::try_from(markdown.capacity()).unwrap(), actual.output);
            markdown
        }

        let span_rows = vec![
            TableRow { cells: vec![cell("head\nline", true, 2, 2), cell("h3", true, 1, 1)] },
            TableRow { cells: vec![cell("", false, 1, 1)] },
            TableRow {
                cells: vec![cell("a", false, 1, 1), cell("b", false, 1, 1), cell("c", false, 1, 1)],
            },
        ];
        let (width, has_header) = table_shape(&span_rows).unwrap();
        assert_eq!((width, has_header), (3, true));
        let owners = table_plan_owners(&span_rows, width, has_header, 1_000, 2_000).unwrap();
        assert_eq!(owners.occupancy, 3 * u64::try_from(std::mem::size_of::<u32>()).unwrap());
        assert_eq!(
            owners.grid_rows,
            3 * u64::try_from(std::mem::size_of::<Vec<String>>()).unwrap()
        );
        assert_eq!(owners.grid_slots, 9 * u64::try_from(std::mem::size_of::<String>()).unwrap());
        assert_eq!(owners.fallback_header, 0);
        assert_eq!(
            owners.separator_slots,
            3 * u64::try_from(std::mem::size_of::<String>()).unwrap()
        );
        assert_eq!(owners.separator_strings, 15);
        assert_eq!(owners.cell_strings, 1_000);
        assert_eq!(owners.cell_block_joins, 2_000);
        assert!(owners.output >= owners.cell_strings + owners.separator_strings);

        let no_header =
            vec![TableRow { cells: vec![cell("body", false, 1, 1), cell("", false, 1, 2)] }];
        let (width, has_header) = table_shape(&no_header).unwrap();
        assert_eq!((width, has_header), (3, false));
        let owners = table_plan_owners(&no_header, width, has_header, 0, 0).unwrap();
        assert_eq!(
            owners.fallback_header,
            3 * u64::try_from(std::mem::size_of::<String>()).unwrap()
        );

        let growth_boundary_span = 8_193_u32;
        let growth_boundary = vec![
            TableRow {
                cells: vec![
                    cell("growth\nfirst", false, 1, growth_boundary_span),
                    cell("growth\nsecond", false, 1, 1),
                ],
            },
            TableRow { cells: vec![cell("growth\nbody", false, 1, growth_boundary_span + 1)] },
        ];
        let growth_markdown = assert_measured_owners(&growth_boundary, 8_194);
        assert!(growth_markdown.contains("growth<br><br>first"));
        assert!(growth_markdown.contains("growth<br><br>body"));

        let maximum_span = u32::try_from(into_markdown_core::MAX_TABLE_COLUMNS).unwrap();
        let adversarial_first_span = 8_193_u32;
        let adversarial_second_span = maximum_span - adversarial_first_span;
        let maximum = vec![
            TableRow {
                cells: vec![
                    cell("maximum\nfirst", false, 1, adversarial_first_span),
                    cell("maximum\nsecond", false, 1, adversarial_second_span),
                ],
            },
            TableRow { cells: vec![cell("maximum\nbody", false, 1, maximum_span)] },
        ];
        let wide_markdown = assert_measured_owners(&maximum, into_markdown_core::MAX_TABLE_COLUMNS);
        let options = ConversionOptions::default();
        assert!(wide_markdown.contains("maximum<br><br>first"));
        assert!(wide_markdown.contains("maximum<br><br>body"));

        for rows in [span_rows, no_header, growth_boundary, maximum] {
            let document =
                document(vec![node("table-plan", Block::Table { rows, alignments: Vec::new() })]);
            let measuring = ExecutionContext::new(
                into_markdown_core::ExecutionOptions::default(),
                into_markdown_core::ResourceLimits::default(),
            );
            let plan = planned_render_peak(&document, &[], &options, &measuring).unwrap();
            let low = ExecutionContext::new(
                into_markdown_core::ExecutionOptions::default(),
                into_markdown_core::ResourceLimits {
                    max_memory_bytes: plan - 1,
                    ..Default::default()
                },
            );
            let calls = std::sync::atomic::AtomicUsize::new(0);
            // This is the renderer-call boundary used by the engine: the
            // plan-minus-one reservation fails before the render closure is
            // invoked, so no table allocation or formatting has begun.
            assert!(low.reserve_memory(plan).is_err());
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

            let exact = ExecutionContext::new(
                into_markdown_core::ExecutionOptions::default(),
                into_markdown_core::ResourceLimits { max_memory_bytes: plan, ..Default::default() },
            );
            let permit = exact.reserve_memory(plan).unwrap();
            let render_after_preflight = || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                render(&document, &[], &options)
            };
            let markdown = render_after_preflight().unwrap();
            assert!(u64::try_from(markdown.capacity()).unwrap() <= plan);
            drop(permit);
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn renderer_plan_rejects_nested_links_and_honors_cancellation_before_render() {
        let linked = document(vec![node(
            "p",
            Block::Paragraph(vec![Inline::Link {
                target: "https://example.test/outer".into(),
                content: vec![Inline::Link {
                    target: "https://example.test/inner".into(),
                    content: vec![Inline::Text { value: "x".into(), marks: vec![] }],
                }],
            }]),
        )]);
        let context = ExecutionContext::new(
            into_markdown_core::ExecutionOptions::default(),
            into_markdown_core::ResourceLimits::default(),
        );
        assert!(
            planned_render_peak(&linked, &[], &ConversionOptions::default(), &context).is_err()
        );

        let cancellation = into_markdown_core::CancellationToken::new();
        cancellation.cancel();
        let cancelled = ExecutionContext::new(
            into_markdown_core::ExecutionOptions { cancellation, ..Default::default() },
            into_markdown_core::ResourceLimits::default(),
        );
        assert!(matches!(
            planned_render_peak(
                &Document::default(),
                &[],
                &ConversionOptions::default(),
                &cancelled
            ),
            Err(ConversionError::Cancelled)
        ));
    }
}
