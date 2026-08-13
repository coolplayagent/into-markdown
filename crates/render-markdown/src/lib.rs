//! Deterministic GitHub-Flavored Markdown rendering for the unified IR.
//!
//! This crate deliberately renders asset references only. Writing extracted
//! assets remains the caller's responsibility.

use base64::Engine as _;
use into_markdown_core::{
    Asset, AssetMode, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Document, ExecutionContext, Inline, InlineMark, ListItem, ListKind, MarkdownRenderer,
    TableAlignment, TableRow, canonical_external_asset_uri,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

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
        .map(|(sha256, (source_index, media_type, extension, mut asset_ids))| {
            asset_ids.sort();
            let mut filename = format!("asset-{sha256}");
            if let Some(extension) = extension {
                filename.push('.');
                filename.push_str(&extension);
            }
            let uri = join_uri_prefix(options.output.asset_uri_prefix.as_deref(), &filename);
            PlannedAsset {
                asset_ids,
                source_index,
                filename,
                uri,
                media_type,
                size: u64::try_from(assets[source_index].bytes.len()).unwrap_or(u64::MAX),
                sha256,
            }
        })
        .collect::<Vec<_>>();
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

impl MarkdownRenderer for GfmRenderer {
    fn id(&self) -> &'static str {
        "builtin.gfm"
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
    let context = RenderContext { plan: &plan, assets, options };
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
            let block = self.render_block(&node.block, inline_context)?;
            if !block.is_empty() {
                rendered.push(block);
            }
        }
        Ok(rendered.join("\n\n"))
    }

    #[allow(clippy::too_many_lines)]
    fn render_block(
        &self,
        block: &Block,
        inline_context: InlineContext,
    ) -> Result<String, ConversionError> {
        match block {
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
                Ok(with_body(format!("## Page {number}"), &body))
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
            Block::TimedSegment { range, speaker, content } => {
                let mut line =
                    format!("`{} – {}`", timestamp(range.start_ms), timestamp(range.end_ms));
                if let Some(speaker) = speaker {
                    write!(
                        line,
                        " **{}:**",
                        escape_text(&single_line(speaker), InlineContext::Normal)
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
        let grid = self.table_grid(rows)?;
        let width = grid.first().map_or(0, Vec::len);
        let first_has_header = rows
            .first()
            .is_some_and(|row| !row.cells.is_empty() && row.cells.iter().all(|cell| cell.header));
        let mut output = String::new();
        if first_has_header {
            write_table_row(&mut output, &grid[0]);
        } else {
            write_table_row(&mut output, &vec![String::new(); width]);
        }
        let separators = (0..width)
            .map(|column| {
                match alignments.get(column).copied().unwrap_or_default() {
                    TableAlignment::None => "---",
                    TableAlignment::Left => ":---",
                    TableAlignment::Center => ":---:",
                    TableAlignment::Right => "---:",
                }
                .into()
            })
            .collect::<Vec<String>>();
        write_table_row(&mut output, &separators);
        let start = usize::from(first_has_header);
        for row in &grid[start..] {
            write_table_row(&mut output, row);
        }
        output.pop();
        Ok(output)
    }

    fn table_grid(&self, rows: &[TableRow]) -> Result<Vec<Vec<String>>, ConversionError> {
        let mut occupancy: Vec<u32> = Vec::new();
        let mut grid = Vec::with_capacity(rows.len());
        for row in rows {
            let mut rendered = vec![String::new(); occupancy.len()];
            let mut column = 0_usize;
            for cell in &row.cells {
                while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                    column += 1;
                }
                let span = usize::try_from(cell.column_span)
                    .map_err(|_| render_error("table column span cannot be represented"))?;
                let end = column
                    .checked_add(span)
                    .ok_or_else(|| render_error("table width overflowed"))?;
                if occupancy.len() < end {
                    occupancy.resize(end, 0);
                    rendered.resize(end, String::new());
                }
                rendered[column] = self.render_cell(cell)?;
                occupancy[column..end].fill(cell.row_span);
                column = end;
            }
            for remaining in &mut occupancy {
                *remaining = remaining.saturating_sub(1);
            }
            grid.push(rendered);
        }
        Ok(grid)
    }

    fn render_cell(&self, cell: &Cell) -> Result<String, ConversionError> {
        let rendered = self.render_blocks_in(&cell.blocks, InlineContext::TableCell)?;
        let flattened = normalize_lf(&rendered).replace("\n\n", "<br><br>").replace('\n', "<br>");
        let mut rendered = if cell.row_span > 1 || cell.column_span > 1 {
            format!(
                "<span data-rowspan=\"{}\" data-colspan=\"{}\">{flattened}</span>",
                cell.row_span, cell.column_span
            )
        } else {
            flattened
        };
        if cell.header {
            rendered = format!("<strong>{rendered}</strong>");
        }
        Ok(rendered)
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

fn render_inlines(inlines: &[Inline], context: InlineContext) -> Result<String, ConversionError> {
    let mut output = String::new();
    for inline in inlines {
        match inline {
            Inline::Text { value, marks } => {
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

fn write_table_row(output: &mut String, cells: &[String]) {
    output.push('|');
    for cell in cells {
        output.push(' ');
        output.push_str(cell);
        output.push_str(" |");
    }
    output.push('\n');
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
        AssetId, CellRef, DocumentMetadata, NodeId, Provenance, ProvenanceKind, SourceLocator,
        TimeRange,
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
                    content: vec![Inline::Text { value: "hello".into(), marks: vec![] }],
                },
            ),
            node("rule", Block::Rule),
        ]);
        assert_eq!(
            output(&doc),
            "````math\nx```y\nz\n````\n\n[^fn-6e205d0d0a]\n\n[^fn-6e205d0d0a]: foot\n\n## Page 2\n\npage\n\n## Slide 3: A \\# title\n\n## Sheet: Data \\| 2026\n\nsheet\n\n`01:01:01.002 – 01:01:02.003` **A\\*B:** hello\n\n---\n"
        );
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
            "## Page 1\n\n![remote](<https://example.invalid/x.png>)\n"
        );
        options.output.asset_mode = AssetMode::Extract;
        assert_eq!(
            render(&image, std::slice::from_ref(&asset), &options).unwrap(),
            "## Page 1\n\n![remote](<https://example.invalid/x.png>)\n"
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
}
