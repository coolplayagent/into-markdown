//! Deterministic GitHub-Flavored Markdown rendering for the unified IR.
//!
//! This crate deliberately renders asset references only. Writing extracted
//! assets remains the caller's responsibility.

use base64::Engine as _;
use into_markdown_core::{
    Asset, AssetMode, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Document, Inline, InlineMark, ListItem, ListKind, MarkdownRenderer, TableRow,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Deterministic renderer occupying the single built-in GFM renderer slot.
#[derive(Debug, Default)]
pub struct GfmRenderer;

impl MarkdownRenderer for GfmRenderer {
    fn id(&self) -> &'static str {
        "builtin.gfm"
    }

    fn render<'a>(
        &'a self,
        document: &'a Document,
        assets: &'a [Asset],
        options: &'a ConversionOptions,
    ) -> BoxFuture<'a, Result<String, ConversionError>> {
        Box::pin(async move { render(document, assets, options) })
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
    document.validate().map_err(|error| ConversionError::Internal {
        detail: format!(
            "renderer received invalid document IR ({} at {}): {}",
            error.code.as_str(),
            error.path,
            error.detail
        ),
    })?;
    let inventory = AssetInventory::new(assets)?;
    let mut referenced_assets = BTreeSet::new();
    validate_image_references(&document.blocks, &inventory, &mut referenced_assets)?;
    if options.output.asset_mode == AssetMode::Extract {
        inventory.validate_extract_targets(&referenced_assets)?;
    }
    let context = RenderContext { inventory, options };
    let mut output = context.render_blocks(&document.blocks)?;
    trim_blank_lines(&mut output);
    if !output.is_empty() {
        output.push('\n');
    }
    Ok(output)
}

struct AssetInventory<'a> {
    by_id: BTreeMap<&'a str, (usize, &'a Asset)>,
}

impl<'a> AssetInventory<'a> {
    fn new(assets: &'a [Asset]) -> Result<Self, ConversionError> {
        let mut by_id = BTreeMap::new();
        for (index, asset) in assets.iter().enumerate() {
            if asset.id.0.trim().is_empty() {
                return Err(render_error("asset inventory contains an empty asset ID"));
            }
            if !valid_media_type(&asset.media_type) {
                return Err(render_error(format!(
                    "asset {} has an invalid media type",
                    asset.id.0
                )));
            }
            if by_id.insert(asset.id.0.as_str(), (index, asset)).is_some() {
                return Err(render_error(format!("duplicate asset ID {}", asset.id.0)));
            }
        }
        Ok(Self { by_id })
    }

    fn get(&self, id: &str) -> Result<(usize, &'a Asset), ConversionError> {
        self.by_id
            .get(id)
            .copied()
            .ok_or_else(|| render_error(format!("image references missing asset {id}")))
    }

    fn validate_extract_targets(
        &self,
        referenced_assets: &BTreeSet<&str>,
    ) -> Result<(), ConversionError> {
        let mut targets = BTreeSet::new();
        for id in referenced_assets {
            let (index, asset) = self.get(id)?;
            if asset.bytes.is_empty() {
                return Err(render_error(format!(
                    "asset {} has no bytes for extract mode",
                    asset.id.0
                )));
            }
            let fallback = format!("asset-{}", index + 1);
            let target = sanitize_filename(asset.filename.as_deref().unwrap_or(&fallback));
            if !targets.insert(target.clone()) {
                return Err(render_error(format!(
                    "extract target {target} is shared by multiple assets"
                )));
            }
        }
        Ok(())
    }
}

struct RenderContext<'a> {
    inventory: AssetInventory<'a>,
    options: &'a ConversionOptions,
}

impl RenderContext<'_> {
    fn render_blocks(&self, nodes: &[BlockNode]) -> Result<String, ConversionError> {
        let mut rendered = Vec::with_capacity(nodes.len());
        for node in nodes {
            let block = self.render_block(&node.block)?;
            if !block.is_empty() {
                rendered.push(block);
            }
        }
        Ok(rendered.join("\n\n"))
    }

    #[allow(clippy::too_many_lines)]
    fn render_block(&self, block: &Block) -> Result<String, ConversionError> {
        match block {
            Block::Paragraph(content) => render_inlines(content),
            Block::Heading { level, content } => {
                Ok(format!("{} {}", "#".repeat(usize::from(*level)), render_inlines(content)?))
            }
            Block::List { kind, start, items } => self.render_list(*kind, *start, items),
            Block::Table { rows } => self.render_table(rows),
            Block::Code { language, text } => {
                Ok(render_fence(text, language.as_deref().map(sanitize_info_string).as_deref()))
            }
            Block::Formula(value) => Ok(render_fence(value, Some("math"))),
            Block::Footnote { label, blocks } => {
                let body = self.render_blocks(blocks)?;
                let label = footnote_label(label);
                if body.is_empty() {
                    Ok(format!("[^{label}]:"))
                } else {
                    Ok(format!("[^{label}]: {}", indent_continuation(&body, 4)))
                }
            }
            Block::Image { asset, alt } => self.render_image(&asset.0, alt.as_deref()),
            Block::Page { number, blocks } => {
                let body = self.render_blocks(blocks)?;
                Ok(with_body(format!("## Page {number}"), &body))
            }
            Block::Slide { number, title, blocks } => {
                let mut heading = format!("## Slide {number}");
                if let Some(title) = title {
                    write!(heading, ": {}", escape_text(&single_line(title)))
                        .map_err(|_| render_error("failed to render slide title"))?;
                }
                let body = self.render_blocks(blocks)?;
                Ok(with_body(heading, &body))
            }
            Block::Sheet { name, blocks } => {
                let body = self.render_blocks(blocks)?;
                Ok(with_body(format!("## Sheet: {}", escape_text(&single_line(name))), &body))
            }
            Block::TimedSegment { range, speaker, content } => {
                let mut line =
                    format!("`{} – {}`", timestamp(range.start_ms), timestamp(range.end_ms));
                if let Some(speaker) = speaker {
                    write!(line, " **{}:**", escape_text(&single_line(speaker)))
                        .map_err(|_| render_error("failed to render speaker label"))?;
                }
                let content = render_inlines(content)?;
                if !content.is_empty() {
                    line.push(' ');
                    line.push_str(&content);
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
            let body = self.render_blocks(&item.blocks)?;
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

    fn render_table(&self, rows: &[TableRow]) -> Result<String, ConversionError> {
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
        write_table_row(&mut output, &vec!["---".into(); width]);
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
        let rendered = self.render_blocks(&cell.blocks)?;
        let flattened = normalize_lf(&rendered).replace("\n\n", "<br><br>").replace('\n', "<br>");
        let flattened = escape_unescaped_pipes(&flattened);
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
        let (index, asset) = self.inventory.get(id)?;
        let target = match self.options.output.asset_mode {
            AssetMode::Omit => return Ok(alt),
            AssetMode::Embed if !asset.bytes.is_empty() => format!(
                "data:{};base64,{}",
                asset.media_type,
                base64::engine::general_purpose::STANDARD.encode(&asset.bytes)
            ),
            AssetMode::Embed => {
                return Err(render_error(format!("asset {id} has no bytes for embed mode")));
            }
            AssetMode::Extract => {
                let fallback = format!("asset-{}", index + 1);
                let filename = sanitize_filename(asset.filename.as_deref().unwrap_or(&fallback));
                join_uri_prefix(self.options.output.asset_uri_prefix.as_deref(), &filename)
            }
        };
        Ok(format!("![{alt}](<{}>)", escape_generated_destination(&target)))
    }
}

fn render_inlines(inlines: &[Inline]) -> Result<String, ConversionError> {
    let mut output = String::new();
    for inline in inlines {
        match inline {
            Inline::Text { value, marks } => output.push_str(&render_marked_text(value, marks)),
            Inline::Code(value) => output.push_str(&render_code_span(value)),
            Inline::Link { target, content } => {
                validate_link_target(target)?;
                output.push('[');
                output.push_str(&render_inlines(content)?);
                output.push_str("](<");
                output.push_str(&escape_destination(target));
                output.push_str(">)");
            }
            Inline::Formula(value) => {
                let code = render_code_span(value);
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

fn render_marked_text(value: &str, marks: &[InlineMark]) -> String {
    let mut rendered = escape_text(&single_line(value));
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
                InlineMark::Bold => format!("**{rendered}**"),
                InlineMark::Italic => format!("*{rendered}*"),
                InlineMark::Strikethrough => format!("~~{rendered}~~"),
                InlineMark::Underline => format!("<u>{rendered}</u>"),
                InlineMark::Superscript => format!("<sup>{rendered}</sup>"),
                InlineMark::Subscript => format!("<sub>{rendered}</sub>"),
            };
        }
    }
    rendered
}

fn render_code_span(value: &str) -> String {
    let value = single_line(value);
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

fn escape_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
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
    escape_text(value).replace('"', "&quot;")
}

fn escape_destination(value: &str) -> String {
    escape_generated_destination(&normalize_lf(value))
}

fn escape_generated_destination(value: &str) -> String {
    encode_bytes(value, |byte| {
        !byte.is_ascii_control() && !matches!(byte, b' ' | b'<' | b'>' | b'\\')
    })
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

fn escape_unescaped_pipes(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut slashes = 0_usize;
    for character in value.chars() {
        if character == '|' && slashes.is_multiple_of(2) {
            output.push('\\');
        }
        output.push(character);
        slashes = if character == '\\' { slashes + 1 } else { 0 };
    }
    output
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

fn validate_link_target(value: &str) -> Result<(), ConversionError> {
    if value.chars().any(char::is_control) {
        return Err(render_error("link target contains a control character"));
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

fn sanitize_filename(value: &str) -> String {
    let filename = value.rsplit(['/', '\\']).next().unwrap_or("asset");
    let sanitized = filename
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() || matches!(sanitized.as_str(), "." | "..") {
        "asset".into()
    } else {
        sanitized
    }
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

fn validate_image_references<'a>(
    nodes: &[BlockNode],
    inventory: &AssetInventory<'a>,
    referenced_assets: &mut BTreeSet<&'a str>,
) -> Result<(), ConversionError> {
    for node in nodes {
        match &node.block {
            Block::Image { asset, .. } => {
                let (_, asset) = inventory.get(&asset.0)?;
                referenced_assets.insert(asset.id.0.as_str());
            }
            Block::List { items, .. } => {
                for item in items {
                    validate_image_references(&item.blocks, inventory, referenced_assets)?;
                }
            }
            Block::Table { rows } => {
                for row in rows {
                    for cell in &row.cells {
                        validate_image_references(&cell.blocks, inventory, referenced_assets)?;
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                validate_image_references(blocks, inventory, referenced_assets)?;
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
            "### ***<u>a\\*\\[x\\] b</u>***```  a``b  ```[link\\]](<https://e.invalid/a%20b%3Ex>)$``x`y``$  \n~~tail~~\n"
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
            output(&document(vec![node("t", Block::Table { rows })])),
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
            output(&document(vec![node("t", Block::Table { rows })])),
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
            "![a\\]lt ](<assets/bad_name.png>)\n"
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
    fn unsafe_links_mime_types_and_extract_collisions_fail_stably() {
        for target in [
            "javascript:alert(1)",
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

        let image = document(vec![
            node("a", Block::Image { asset: AssetId("a".into()), alt: None }),
            node("b", Block::Image { asset: AssetId("b".into()), alt: None }),
        ]);
        let asset = |id: &str, media_type: &str| Asset {
            id: AssetId(id.into()),
            filename: Some("same?.png".into()),
            media_type: media_type.into(),
            bytes: vec![1],
            external_uri: None,
        };
        let assets = [asset("a", "image/png"), asset("b", "image/png")];
        assert!(render(&image, &assets, &ConversionOptions::default()).is_err());
        let bad_mime = [asset("a", "image/png;base64,EVIL")];
        assert!(
            render(
                &document(vec![node("a", Block::Image { asset: AssetId("a".into()), alt: None })]),
                &bad_mime,
                &ConversionOptions::default()
            )
            .is_err()
        );
    }

    #[test]
    fn embed_requires_bytes_and_all_modes_validate_nested_image_references() {
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
        assert!(render(&image, std::slice::from_ref(&asset), &options).is_err());
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
        assert_eq!(output(&left), "***x***\n");
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
