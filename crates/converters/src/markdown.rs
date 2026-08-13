//! Offline Markdown/GFM parsing into the unified document IR.

use crate::text;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ExecutionContext,
    FormatCandidate, Inline, InlineMark, InputFormat, ListItem, ListKind, MAX_DOCUMENT_INLINES,
    MAX_DOCUMENT_NODES, NodeId, ProbeOutcome, Provenance, ProvenanceKind, ResolvedInput,
    ResourceReservation, Services, SourceLocator, TableRow, canonical_external_asset_uri,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::mem::size_of;
use std::ops::Range;

const FORMATS: &[InputFormat] = &[InputFormat::Markdown];
const PROVIDER_ID: &str = "builtin.converter.markdown-gfm";
const RAW_HTML_CODE: &str = "markdown.rawHtmlPreservedAsCode";
const BLOCKQUOTE_CODE: &str = "markdown.blockquotePreservedAsCode";
const EXTERNAL_IMAGE_CODE: &str = "markdown.externalImagePreservedAsLink";
const DUPLICATE_DEFINITION_CODE: &str = "markdown.duplicateDefinitionIgnored";

/// `CommonMark` and GitHub-Flavored Markdown converter.
#[derive(Debug, Default)]
pub struct MarkdownConverter;

impl Converter for MarkdownConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        120
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
    }

    fn probe<'a>(
        &'a self,
        _: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Ok(if candidate.format == InputFormat::Markdown {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
            })
        })
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { convert_markdown(input, options, context) })
    }
}

/// Conservative content evidence: plain prose and a lone incidental marker stay TXT.
pub(crate) fn strong_markdown_evidence(
    text: &str,
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let mut structural = 0_u8;
    let mut open_fence: Option<u8> = None;
    let mut table_separator = false;
    let mut previous_nonblank = false;
    for (index, line) in text.lines().take(4096).enumerate() {
        if index.is_multiple_of(128) {
            context.checkpoint()?;
        }
        let trimmed = line.trim_start();
        let heading = trimmed.starts_with("# ") || trimmed.starts_with("## ");
        let list = trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
            || ordered_marker(trimmed);
        let quote = trimmed.starts_with("> ");
        let rule = matches!(trimmed, "---" | "***" | "___");
        let fence = if trimmed.starts_with("```") {
            Some(b'`')
        } else if trimmed.starts_with("~~~") {
            Some(b'~')
        } else {
            None
        };
        let closed_fence = fence.is_some_and(|marker| open_fence == Some(marker));
        if let Some(marker) = fence {
            open_fence = if closed_fence { None } else { Some(marker) };
        }
        let setext = previous_nonblank
            && !trimmed.is_empty()
            && (trimmed.bytes().all(|byte| byte == b'=')
                || trimmed.len() >= 3 && trimmed.bytes().all(|byte| byte == b'-'));
        let task = trimmed.starts_with("- [ ] ")
            || trimmed.starts_with("- [x] ")
            || trimmed.starts_with("- [X] ");
        table_separator |=
            trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.contains("---");
        structural = structural
            .saturating_add(u8::from(heading || list || quote || rule || fence.is_some()));
        if structural >= 2 || table_separator || closed_fence || setext || task {
            return Ok(true);
        }
        previous_nonblank = !trimmed.trim().is_empty();
    }
    Ok(false)
}

fn ordered_marker(value: &str) -> bool {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0
        && value.as_bytes().get(digits).is_some_and(|byte| matches!(byte, b'.' | b')'))
        && value.as_bytes().get(digits + 1) == Some(&b' ')
}

fn convert_markdown(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let input_size =
        u64::try_from(input.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: "Markdown input size cannot be represented as u64".into(),
        })?;
    if input_size > options.limits.max_input_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: format!("{input_size} > {}", options.limits.max_input_bytes),
        });
    }
    if options.text.charset.as_deref().is_some_and(|label| {
        !label.trim().eq_ignore_ascii_case("utf-8") && !label.trim().eq_ignore_ascii_case("utf8")
    }) {
        return Err(ConversionError::Malformed {
            part: Some("charset".into()),
            detail: "Markdown source encoding must be UTF-8".into(),
        });
    }
    let (mut decoded, mut diagnostics) =
        text::decode_source(&input.bytes, Some("utf-8"), options.text.decoding_mode, context)?;
    scan_duplicate_definitions(&mut decoded, &mut diagnostics, context)?;
    let mut builder = Builder::new(&decoded, options, context, diagnostics)?;
    builder.parse()?;
    let (document, diagnostics, assets) = builder.finish()?;
    Ok(ConverterOutput { document, diagnostics, assets })
}

#[derive(Debug)]
enum FrameKind {
    Root,
    Paragraph,
    Heading(u8),
    List { start: u64, ordered: bool },
    Item,
    Footnote(String),
    Table,
    TableHead,
    TableRow,
    TableCell { header: bool },
    Code(Option<String>),
    BlockQuote,
    HtmlBlock,
    Emphasis,
    Strong,
    Strikethrough,
    Superscript,
    Subscript,
    Link(String),
    Image(String),
}

#[derive(Debug)]
struct Frame {
    kind: FrameKind,
    span: Range<usize>,
    blocks: Vec<BlockNode>,
    inlines: Vec<Inline>,
    items: Vec<ListItem>,
    rows: Vec<TableRow>,
    cells: Vec<Cell>,
    checked: Option<bool>,
    literal: String,
    images: Vec<ExternalImage>,
}

impl Frame {
    fn new(kind: FrameKind, span: Range<usize>) -> Self {
        Self {
            kind,
            span,
            blocks: Vec::new(),
            inlines: Vec::new(),
            items: Vec::new(),
            rows: Vec::new(),
            cells: Vec::new(),
            checked: None,
            literal: String::new(),
            images: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct ExternalImage {
    target: String,
    alt: String,
    span: Range<usize>,
}

struct Builder<'a> {
    source: &'a text::DecodedText,
    options: &'a ConversionOptions,
    context: &'a ExecutionContext,
    frames: Vec<Frame>,
    diagnostics: Vec<Diagnostic>,
    node_count: usize,
    inline_count: usize,
    sequence: u64,
    parser_memory: ResourceReservation,
    assets: Vec<Asset>,
    footnotes: std::collections::BTreeSet<String>,
}

impl<'a> Builder<'a> {
    fn new(
        source: &'a text::DecodedText,
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
        diagnostics: Vec<Diagnostic>,
    ) -> Result<Self, ConversionError> {
        let mut frames = Vec::new();
        frames.try_reserve_exact(8).map_err(allocation_error)?;
        frames.push(Frame::new(FrameKind::Root, 0..source.text.len()));
        let parser_bytes = u64::try_from(source.text.len())
            .map_err(|_| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "Markdown parser reservation cannot be represented as u64".into(),
            })?
            .checked_add(u64_size(
                frames.capacity().saturating_mul(size_of::<Frame>()),
                "Markdown initial frame stack",
            )?)
            .ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "Markdown parser reservation overflowed".into(),
            })?;
        Ok(Self {
            source,
            options,
            context,
            frames,
            diagnostics,
            node_count: 0,
            inline_count: 0,
            sequence: 0,
            parser_memory: context.reserve_memory(parser_bytes)?,
            assets: Vec::new(),
            footnotes: std::collections::BTreeSet::new(),
        })
    }

    fn parse(&mut self) -> Result<(), ConversionError> {
        let parser = Parser::new_ext(&self.source.text, parser_options()).into_offset_iter();
        for (index, (event, span)) in parser.enumerate() {
            if index.is_multiple_of(128) {
                self.context.checkpoint()?;
            }
            match event {
                Event::Start(tag) => self.start(tag, span)?,
                Event::End(end) => self.end(end, span)?,
                Event::Text(value) => self.text(&value)?,
                Event::Code(value) => self.push_inline(Inline::Code(value.into_string()))?,
                Event::InlineMath(value) => {
                    self.push_inline(Inline::Formula(value.into_string()))?;
                }
                Event::DisplayMath(value) => {
                    let node = self.node(Block::Formula(value.into_string()), span)?;
                    self.push_block(node)?;
                }
                Event::Html(value) => {
                    self.diagnostic(
                        RAW_HTML_CODE,
                        "raw HTML was preserved as non-executable code",
                        &span,
                    )?;
                    self.push_inline(Inline::Code(value.into_string()))?;
                }
                Event::InlineHtml(value) => self.inline_html(&value, span)?,
                Event::FootnoteReference(label) => {
                    self.push_inline(Inline::FootnoteReference(normalize_footnote_label(&label)))?;
                }
                Event::SoftBreak => self.text("\n")?,
                Event::HardBreak => self.push_inline(Inline::LineBreak)?,
                Event::Rule => {
                    let node = self.node(Block::Rule, span)?;
                    self.push_block(node)?;
                }
                Event::TaskListMarker(checked) => {
                    let item = self
                        .frames
                        .iter_mut()
                        .rev()
                        .find(|frame| matches!(frame.kind, FrameKind::Item));
                    if let Some(item) = item {
                        item.checked = Some(checked);
                    }
                }
            }
        }
        if self.frames.len() != 1 {
            return Err(ConversionError::Malformed {
                part: Some("markdown".into()),
                detail: "Markdown parser ended with unclosed event containers".into(),
            });
        }
        Ok(())
    }

    fn start(&mut self, tag: Tag<'_>, span: Range<usize>) -> Result<(), ConversionError> {
        let kind = match tag {
            Tag::Paragraph => FrameKind::Paragraph,
            Tag::Heading { level, .. } => FrameKind::Heading(heading_level(level)),
            Tag::BlockQuote(_) => FrameKind::BlockQuote,
            Tag::CodeBlock(kind) => FrameKind::Code(match kind {
                CodeBlockKind::Indented => None,
                CodeBlockKind::Fenced(info) => {
                    let language = info.split_whitespace().next().unwrap_or_default().trim();
                    (!language.is_empty()).then(|| language.to_owned())
                }
            }),
            Tag::HtmlBlock | Tag::MetadataBlock(_) => FrameKind::HtmlBlock,
            Tag::List(start) => {
                FrameKind::List { start: start.unwrap_or(1), ordered: start.is_some() }
            }
            Tag::Item => FrameKind::Item,
            Tag::FootnoteDefinition(label) => FrameKind::Footnote(normalize_footnote_label(&label)),
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                FrameKind::BlockQuote
            }
            Tag::Table(_) => FrameKind::Table,
            Tag::TableHead => FrameKind::TableHead,
            Tag::TableRow => FrameKind::TableRow,
            Tag::TableCell => {
                let header = self
                    .frames
                    .iter()
                    .rev()
                    .any(|frame| matches!(frame.kind, FrameKind::TableHead));
                FrameKind::TableCell { header }
            }
            Tag::Emphasis => FrameKind::Emphasis,
            Tag::Strong => FrameKind::Strong,
            Tag::Strikethrough => FrameKind::Strikethrough,
            Tag::Superscript => FrameKind::Superscript,
            Tag::Subscript => FrameKind::Subscript,
            Tag::Link { dest_url, .. } => FrameKind::Link(dest_url.into_string()),
            Tag::Image { dest_url, .. } => FrameKind::Image(dest_url.into_string()),
        };
        let depth = self.frames.len();
        if depth > usize::from(self.options.limits.max_nesting_depth) {
            return Err(ConversionError::ResourceLimit {
                limit: "max_nesting_depth",
                detail: format!(
                    "Markdown event depth {depth} > {}",
                    self.options.limits.max_nesting_depth
                ),
            });
        }
        let adds_structural = matches!(
            kind,
            FrameKind::List { .. }
                | FrameKind::Item
                | FrameKind::Footnote(_)
                | FrameKind::Table
                | FrameKind::TableRow
                | FrameKind::TableCell { .. }
        );
        let structural_depth = self
            .frames
            .iter()
            .filter(|frame| {
                matches!(
                    frame.kind,
                    FrameKind::List { .. }
                        | FrameKind::Item
                        | FrameKind::Footnote(_)
                        | FrameKind::Table
                        | FrameKind::TableRow
                        | FrameKind::TableCell { .. }
                )
            })
            .count();
        if adds_structural && structural_depth >= into_markdown_core::MAX_DOCUMENT_DEPTH {
            return Err(ConversionError::ResourceLimit {
                limit: "documentDepth",
                detail: format!(
                    "Markdown structural depth exceeds {}",
                    into_markdown_core::MAX_DOCUMENT_DEPTH
                ),
            });
        }
        self.parser_memory.grow(u64_size(size_of::<Frame>(), "Markdown frame")?)?;
        self.frames.try_reserve_exact(1).map_err(allocation_error)?;
        self.frames.push(Frame::new(kind, span));
        Ok(())
    }

    fn end(&mut self, _: TagEnd, span: Range<usize>) -> Result<(), ConversionError> {
        let mut frame = self.frames.pop().ok_or_else(|| ConversionError::Malformed {
            part: Some("markdown".into()),
            detail: "Markdown parser emitted an unmatched end event".into(),
        })?;
        if matches!(frame.kind, FrameKind::Root) {
            return Err(ConversionError::Malformed {
                part: Some("markdown".into()),
                detail: "Markdown parser attempted to close the document root".into(),
            });
        }
        frame.span.end = frame.span.end.max(span.end);
        self.close(frame)
    }

    #[allow(clippy::too_many_lines)]
    fn close(&mut self, mut frame: Frame) -> Result<(), ConversionError> {
        match frame.kind {
            FrameKind::Paragraph => {
                if frame.images.len() == 1 && frame.inlines.len() == 1 {
                    let image = frame.images.pop().ok_or_else(|| ConversionError::Internal {
                        detail: "standalone Markdown image disappeared".into(),
                    })?;
                    self.diagnostic(
                        "markdown.externalImageReferencedOffline",
                        "absolute HTTP(S) image was referenced without fetching its bytes",
                        &image.span,
                    )?;
                    if image.target.to_ascii_lowercase().ends_with(".svg") {
                        self.diagnostic(
                            "markdown.externalSvgMayContainActiveContent",
                            "external SVG was preserved only as a URI reference and may contain active content when opened by a consumer",
                            &image.span,
                        )?;
                    }
                    let asset_id = format!("markdown-external-image-{}", self.assets.len() + 1);
                    self.parser_memory.grow(u64_size(size_of::<Asset>(), "Markdown asset")?)?;
                    self.charge_text(&asset_id, "Markdown asset ID")?;
                    self.charge_text(&image.target, "Markdown external URI")?;
                    self.charge_text(&image.alt, "Markdown image alt")?;
                    self.assets.try_reserve_exact(1).map_err(allocation_error)?;
                    self.assets.push(Asset {
                        id: AssetId(asset_id.clone()),
                        filename: None,
                        media_type: image_media_type(&image.target).into(),
                        bytes: Vec::new(),
                        external_uri: Some(image.target),
                    });
                    let node = self.node(
                        Block::Image {
                            asset: AssetId(asset_id),
                            alt: (!image.alt.is_empty()).then_some(image.alt),
                        },
                        frame.span,
                    )?;
                    return self.push_block(node);
                }
                for image in &frame.images {
                    self.diagnostic(
                        "markdown.inlineExternalImagePreservedAsLink",
                        "inline image cannot be represented by the block-only image IR and was preserved as a link",
                        &image.span,
                    )?;
                }
                let node = self.node(Block::Paragraph(frame.inlines), frame.span)?;
                self.push_block(node)
            }
            FrameKind::Heading(level) => {
                let node =
                    self.node(Block::Heading { level, content: frame.inlines }, frame.span)?;
                self.push_block(node)
            }
            FrameKind::List { start, ordered } => {
                let task = frame.items.iter().any(|item| item.checked.is_some());
                let kind = if task {
                    ListKind::Task
                } else if ordered {
                    ListKind::Ordered
                } else {
                    ListKind::Bullet
                };
                let node =
                    self.node(Block::List { kind, start, items: frame.items }, frame.span)?;
                self.push_block(node)
            }
            FrameKind::Item => {
                self.consume_structural_container()?;
                if !frame.inlines.is_empty() {
                    let paragraph =
                        self.node(Block::Paragraph(frame.inlines), frame.span.clone())?;
                    frame.blocks.insert(0, paragraph);
                }
                let item =
                    ListItem { checked: frame.checked, marker_label: None, blocks: frame.blocks };
                let parent = self.parent_mut()?;
                parent.items.try_reserve_exact(1).map_err(allocation_error)?;
                parent.items.push(item);
                Ok(())
            }
            FrameKind::Footnote(label) => {
                if !self.footnotes.insert(label.clone()) {
                    self.diagnostic(
                        DUPLICATE_DEFINITION_CODE,
                        "duplicate footnote definition was ignored; the first definition wins",
                        &frame.span,
                    )?;
                    return Ok(());
                }
                if !frame.inlines.is_empty() {
                    let paragraph =
                        self.node(Block::Paragraph(frame.inlines), frame.span.clone())?;
                    frame.blocks.push(paragraph);
                }
                let node =
                    self.node(Block::Footnote { label, blocks: frame.blocks }, frame.span)?;
                self.push_block(node)
            }
            FrameKind::Table => {
                let node = self.node(Block::Table { rows: frame.rows }, frame.span)?;
                self.push_block(node)
            }
            FrameKind::TableHead => {
                if !frame.cells.is_empty() {
                    frame.rows.push(TableRow { cells: std::mem::take(&mut frame.cells) });
                }
                let parent = self.parent_mut()?;
                parent.rows.try_reserve_exact(frame.rows.len()).map_err(allocation_error)?;
                parent.rows.append(&mut frame.rows);
                Ok(())
            }
            FrameKind::TableRow => {
                self.consume_structural_container()?;
                let row = TableRow { cells: frame.cells };
                let parent = self.parent_mut()?;
                parent.rows.try_reserve_exact(1).map_err(allocation_error)?;
                parent.rows.push(row);
                Ok(())
            }
            FrameKind::TableCell { header } => {
                self.consume_structural_container()?;
                if header {
                    for inline in &mut frame.inlines {
                        remove_mark(inline, InlineMark::Bold);
                    }
                }
                if !frame.inlines.is_empty() {
                    let paragraph =
                        self.node(Block::Paragraph(frame.inlines), frame.span.clone())?;
                    frame.blocks.push(paragraph);
                }
                let cell = Cell { row_span: 1, column_span: 1, header, blocks: frame.blocks };
                let parent = self.parent_mut()?;
                parent.cells.try_reserve_exact(1).map_err(allocation_error)?;
                parent.cells.push(cell);
                Ok(())
            }
            FrameKind::Code(language) => {
                let literal = if frame.literal.is_empty() {
                    plain_text(&frame.inlines, &mut self.parser_memory)?
                } else {
                    frame.literal
                };
                let node = self.node(Block::Code { language, text: literal }, frame.span)?;
                self.push_block(node)
            }
            FrameKind::BlockQuote => {
                self.diagnostic(
                    BLOCKQUOTE_CODE,
                    "blockquote was preserved as a non-executable Markdown code container",
                    &frame.span,
                )?;
                let raw = self.source.text.get(frame.span.clone()).unwrap_or_default().to_owned();
                self.charge_text(&raw, "Markdown blockquote fallback")?;
                let node = self.node(
                    Block::Code { language: Some("markdown-blockquote".into()), text: raw },
                    frame.span,
                )?;
                self.push_block(node)
            }
            FrameKind::HtmlBlock => {
                self.diagnostic(
                    RAW_HTML_CODE,
                    "raw HTML was preserved as non-executable code",
                    &frame.span,
                )?;
                let raw = self.source.text.get(frame.span.clone()).unwrap_or_default().to_owned();
                self.charge_text(&raw, "Markdown HTML fallback")?;
                let node = self
                    .node(Block::Code { language: Some("html".into()), text: raw }, frame.span)?;
                self.push_block(node)
            }
            FrameKind::Emphasis => self.finish_mark(frame.inlines, InlineMark::Italic),
            FrameKind::Strong => self.finish_mark(frame.inlines, InlineMark::Bold),
            FrameKind::Strikethrough => self.finish_mark(frame.inlines, InlineMark::Strikethrough),
            FrameKind::Superscript => self.finish_mark(frame.inlines, InlineMark::Superscript),
            FrameKind::Subscript => self.finish_mark(frame.inlines, InlineMark::Subscript),
            FrameKind::Link(target) => {
                if safe_link_target(&target) {
                    self.push_inline(Inline::Link { target, content: frame.inlines })
                } else {
                    self.diagnostic(
                        "markdown.unsafeLinkDropped",
                        "unsafe link destination was removed",
                        &frame.span,
                    )?;
                    self.extend_inlines(frame.inlines)
                }
            }
            FrameKind::Image(target) => {
                let alt = plain_text(&frame.inlines, &mut self.parser_memory)?;
                if safe_external_image_target(&target) {
                    let parent = self.parent_mut()?;
                    parent.images.try_reserve_exact(1).map_err(allocation_error)?;
                    parent.images.push(ExternalImage {
                        target: target.clone(),
                        alt: alt.clone(),
                        span: frame.span.clone(),
                    });
                    self.push_inline(Inline::Link {
                        target,
                        content: vec![Inline::Text { value: alt, marks: Vec::new() }],
                    })
                } else if safe_link_target(&target) {
                    self.diagnostic(
                        EXTERNAL_IMAGE_CODE,
                        "image target could not become an offline asset and was preserved as a link",
                        &frame.span,
                    )?;
                    self.push_inline(Inline::Link {
                        target,
                        content: vec![Inline::Text { value: alt, marks: Vec::new() }],
                    })
                } else {
                    self.diagnostic(
                        EXTERNAL_IMAGE_CODE,
                        "unsafe image target was preserved as literal text",
                        &frame.span,
                    )?;
                    self.push_inline(Inline::Text {
                        value: format!("{alt} ({target})"),
                        marks: Vec::new(),
                    })
                }
            }
            FrameKind::Root => {
                Err(ConversionError::Internal { detail: "closed Markdown root frame".into() })
            }
        }
    }

    fn text(&mut self, value: &str) -> Result<(), ConversionError> {
        if matches!(self.frames.last().map(|frame| &frame.kind), Some(FrameKind::Code(_))) {
            let frame = self.parent_mut()?;
            frame.literal.try_reserve_exact(value.len()).map_err(allocation_error)?;
            frame.literal.push_str(value);
            Ok(())
        } else {
            self.parser_memory.grow(u64_size(value.len(), "Markdown text")?)?;
            self.push_inline(Inline::Text { value: value.to_owned(), marks: Vec::new() })
        }
    }

    fn inline_html(&mut self, value: &str, span: Range<usize>) -> Result<(), ConversionError> {
        let normalized = value.trim().to_ascii_lowercase();
        let start = match normalized.as_str() {
            "<strong>" => Some(FrameKind::Strong),
            "<em>" => Some(FrameKind::Emphasis),
            "<del>" => Some(FrameKind::Strikethrough),
            "<sup>" => Some(FrameKind::Superscript),
            "<sub>" => Some(FrameKind::Subscript),
            _ => None,
        };
        if let Some(kind) = start {
            self.push_inline_frame(kind, span)?;
            return Ok(());
        }
        let closes_current = self.frames.last().is_some_and(|frame| {
            matches!(
                (normalized.as_str(), &frame.kind),
                ("</strong>", FrameKind::Strong)
                    | ("</em>", FrameKind::Emphasis)
                    | ("</del>", FrameKind::Strikethrough)
                    | ("</sup>", FrameKind::Superscript)
                    | ("</sub>", FrameKind::Subscript)
            )
        });
        if closes_current {
            let mut frame = self.frames.pop().ok_or_else(|| ConversionError::Internal {
                detail: "inline HTML formatting frame disappeared".into(),
            })?;
            frame.span.end = frame.span.end.max(span.end);
            return self.close(frame);
        }
        self.diagnostic(
            RAW_HTML_CODE,
            "raw inline HTML was preserved as non-executable code",
            &span,
        )?;
        self.push_inline(Inline::Code(value.to_owned()))
    }

    fn finish_mark(
        &mut self,
        mut inlines: Vec<Inline>,
        mark: InlineMark,
    ) -> Result<(), ConversionError> {
        for inline in &mut inlines {
            apply_mark(inline, mark);
        }
        self.extend_inlines(inlines)
    }

    fn push_inline(&mut self, inline: Inline) -> Result<(), ConversionError> {
        self.inline_count = self.inline_count.saturating_add(1);
        if self.inline_count > MAX_DOCUMENT_INLINES {
            return Err(ConversionError::ResourceLimit {
                limit: "documentInlines",
                detail: format!("{} > {MAX_DOCUMENT_INLINES}", self.inline_count),
            });
        }
        self.parser_memory.grow(u64_size(size_of::<Inline>(), "Markdown inline")?)?;
        let parent = self.parent_mut()?;
        parent.inlines.try_reserve_exact(1).map_err(allocation_error)?;
        parent.inlines.push(inline);
        Ok(())
    }

    fn extend_inlines(&mut self, inlines: Vec<Inline>) -> Result<(), ConversionError> {
        let parent = self.parent_mut()?;
        parent.inlines.try_reserve_exact(inlines.len()).map_err(allocation_error)?;
        parent.inlines.extend(inlines);
        Ok(())
    }

    fn push_block(&mut self, block: BlockNode) -> Result<(), ConversionError> {
        let parent = self.parent_mut()?;
        parent.blocks.try_reserve_exact(1).map_err(allocation_error)?;
        parent.blocks.push(block);
        Ok(())
    }

    fn parent_mut(&mut self) -> Result<&mut Frame, ConversionError> {
        self.frames.last_mut().ok_or_else(|| ConversionError::Internal {
            detail: "Markdown event stack is empty".into(),
        })
    }

    fn node(&mut self, block: Block, span: Range<usize>) -> Result<BlockNode, ConversionError> {
        self.consume_structural_container()?;
        self.sequence =
            self.sequence.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "documentNodes",
                detail: "Markdown node sequence overflowed".into(),
            })?;
        let (start, end) = self.source.source_range(span.start, span.end);
        Ok(BlockNode {
            id: NodeId(format!("markdown-{}", self.sequence)),
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: byte_locator(start, end),
                confidence: Some(1.0),
            },
        })
    }

    fn consume_structural_container(&mut self) -> Result<(), ConversionError> {
        self.node_count = self.node_count.saturating_add(1);
        if self.node_count > MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "documentNodes",
                detail: format!("{} > {MAX_DOCUMENT_NODES}", self.node_count),
            });
        }
        self.parser_memory.grow(u64_size(size_of::<BlockNode>(), "Markdown block")?)
    }

    fn push_inline_frame(
        &mut self,
        kind: FrameKind,
        span: Range<usize>,
    ) -> Result<(), ConversionError> {
        if self.frames.len() > usize::from(self.options.limits.max_nesting_depth) {
            return Err(ConversionError::ResourceLimit {
                limit: "max_nesting_depth",
                detail: "inline HTML formatting exceeds Markdown nesting budget".into(),
            });
        }
        self.parser_memory.grow(u64_size(size_of::<Frame>(), "Markdown frame")?)?;
        self.frames.try_reserve_exact(1).map_err(allocation_error)?;
        self.frames.push(Frame::new(kind, span));
        Ok(())
    }

    fn diagnostic(
        &mut self,
        code: &str,
        message: &str,
        span: &Range<usize>,
    ) -> Result<(), ConversionError> {
        self.parser_memory.grow(u64_size(size_of::<Diagnostic>(), "Markdown diagnostic")?)?;
        self.parser_memory.grow(u64_size(
            code.len().saturating_add(message.len()),
            "Markdown diagnostic strings",
        )?)?;
        self.diagnostics.try_reserve_exact(1).map_err(allocation_error)?;
        let (start, end) = self.source.source_range(span.start, span.end);
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            locator: Some(byte_locator(start, end)),
        });
        Ok(())
    }

    fn charge_text(&mut self, value: &str, label: &str) -> Result<(), ConversionError> {
        self.parser_memory.grow(u64_size(value.len(), label)?)
    }

    fn finish(mut self) -> Result<(Document, Vec<Diagnostic>, Vec<Asset>), ConversionError> {
        let root = self.frames.pop().ok_or_else(|| ConversionError::Internal {
            detail: "Markdown root frame disappeared".into(),
        })?;
        let document = Document { blocks: root.blocks, ..Document::default() };
        document.validate().map_err(|error| ConversionError::Malformed {
            part: Some("markdown".into()),
            detail: format!("parsed IR invalid at {}: {}", error.path, error.detail),
        })?;
        Ok((document, self.diagnostics, self.assets))
    }
}

fn parser_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn normalize_footnote_label(label: &str) -> String {
    let Some(hex) = label.strip_prefix("fn-") else { return label.to_owned() };
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return label.to_owned();
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let (Some(high), Some(low)) = (hex_value(pair[0]), hex_value(pair[1])) else {
            return label.to_owned();
        };
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).unwrap_or_else(|_| label.to_owned())
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn apply_mark(inline: &mut Inline, mark: InlineMark) {
    match inline {
        Inline::Text { marks, .. } => {
            if !marks.contains(&mark) {
                marks.push(mark);
            }
        }
        Inline::Link { content, .. } => {
            for nested in content {
                apply_mark(nested, mark);
            }
        }
        _ => {}
    }
}

fn remove_mark(inline: &mut Inline, mark: InlineMark) {
    match inline {
        Inline::Text { marks, .. } => marks.retain(|candidate| *candidate != mark),
        Inline::Link { content, .. } => {
            for nested in content {
                remove_mark(nested, mark);
            }
        }
        _ => {}
    }
}

fn plain_text(
    inlines: &[Inline],
    memory: &mut ResourceReservation,
) -> Result<String, ConversionError> {
    let mut output = String::new();
    let mut stack = inlines.iter().rev().collect::<Vec<_>>();
    memory.grow(u64_size(
        stack.capacity().saturating_mul(size_of::<&Inline>()),
        "Markdown inline traversal",
    )?)?;
    while let Some(inline) = stack.pop() {
        match inline {
            Inline::Text { value, .. } | Inline::Code(value) | Inline::Formula(value) => {
                memory.grow(u64_size(value.len(), "Markdown plain text")?)?;
                output.try_reserve_exact(value.len()).map_err(allocation_error)?;
                output.push_str(value);
            }
            Inline::Link { content, .. } => {
                memory.grow(u64_size(
                    content.len().saturating_mul(size_of::<&Inline>()),
                    "Markdown inline traversal",
                )?)?;
                stack.try_reserve_exact(content.len()).map_err(allocation_error)?;
                stack.extend(content.iter().rev());
            }
            Inline::FootnoteReference(label) => {
                memory.grow(u64_size(label.len().saturating_add(3), "Markdown footnote text")?)?;
                output
                    .try_reserve_exact(label.len().saturating_add(3))
                    .map_err(allocation_error)?;
                output.push_str("[^");
                output.push_str(label);
                output.push(']');
            }
            Inline::LineBreak => output.push('\n'),
            _ => {}
        }
    }
    Ok(output)
}

fn safe_link_target(value: &str) -> bool {
    if value.chars().any(char::is_control) || value.contains("&#") {
        return false;
    }
    let Some(colon) = value.find(':') else { return true };
    let scheme = &value[..colon];
    if scheme.is_empty()
        || !scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic() || index > 0 && matches!(byte, b'+' | b'-' | b'.')
        })
    {
        return true;
    }
    !matches!(scheme.to_ascii_lowercase().as_str(), "javascript" | "vbscript" | "data" | "file")
        && !value[colon + 1..]
            .strip_prefix("//")
            .and_then(|rest| rest.split('/').next())
            .is_some_and(|authority| authority.contains('@'))
}

fn safe_external_image_target(value: &str) -> bool {
    canonical_external_asset_uri(value).as_deref() == Some(value)
}

fn image_media_type(target: &str) -> &'static str {
    let extension = target
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension));
    match extension.map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("avif") => "image/avif",
        _ => "application/octet-stream",
    }
}

fn scan_duplicate_definitions(
    decoded: &mut text::DecodedText,
    diagnostics: &mut Vec<Diagnostic>,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    decoded.memory.charge(decoded.text.len())?;
    let markdown = decoded.text.clone();
    let mut seen = std::collections::BTreeSet::new();
    let mut offset = 0_usize;
    let mut fence: Option<u8> = None;
    for (index, line) in markdown.split_inclusive('\n').enumerate() {
        if index.is_multiple_of(128) {
            context.checkpoint()?;
        }
        let trimmed = line.trim_start();
        let marker = if trimmed.starts_with("```") {
            Some(b'`')
        } else if trimmed.starts_with("~~~") {
            Some(b'~')
        } else {
            None
        };
        if let Some(marker) = marker {
            fence = if fence == Some(marker) {
                None
            } else if fence.is_none() {
                Some(marker)
            } else {
                fence
            };
        } else if fence.is_none()
            && line.len() - trimmed.len() < 4
            && let Some(close) = trimmed.strip_prefix('[').and_then(|tail| tail.find("]:"))
        {
            let label = trimmed[1..=close].trim().to_ascii_lowercase();
            if !label.is_empty() && !label.starts_with('^') && !seen.insert(label.clone()) {
                let leading = line.len() - trimmed.len();
                let start = offset + leading;
                let end = offset + line.trim_end_matches(['\r', '\n']).len();
                let (source_start, source_end) = decoded.source_range(start, end);
                decoded.memory.charge(size_of::<Diagnostic>())?;
                diagnostics.try_reserve_exact(1).map_err(allocation_error)?;
                diagnostics.push(Diagnostic {
                    code: DUPLICATE_DEFINITION_CODE.into(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!(
                        "duplicate definition {label:?} was ignored; the first definition wins"
                    ),
                    locator: Some(byte_locator(source_start, source_end)),
                });
            }
        }
        offset = offset.saturating_add(line.len());
    }
    Ok(())
}

fn byte_locator(start: usize, end: usize) -> SourceLocator {
    SourceLocator {
        byte_start: u64::try_from(start).ok(),
        byte_end: u64::try_from(end).ok(),
        ..SourceLocator::default()
    }
}

#[allow(clippy::needless_pass_by_value)] // `map_err` supplies the owned standard error.
fn allocation_error(error: std::collections::TryReserveError) -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("Markdown parser allocation failed: {error}"),
    }
}

fn u64_size(value: usize, label: &str) -> Result<u64, ConversionError> {
    u64::try_from(value).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("{label} capacity cannot be represented as u64"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits, SourceMetadata};

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn input(bytes: &[u8]) -> ResolvedInput {
        ResolvedInput { bytes: bytes.to_vec().into(), metadata: SourceMetadata::default() }
    }

    fn convert(bytes: &[u8]) -> Result<ConverterOutput, ConversionError> {
        convert_markdown(&input(bytes), &ConversionOptions::default(), &context())
    }

    #[test]
    fn parses_gfm_structures_into_ir() {
        let source = b"# Heading\n\nA **bold** [link](https://example.com).\n\n- [x] done\n- [ ] todo\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n\nnote[^n]\n\n[^n]: foot\n";
        let output = convert(source).unwrap();
        assert!(matches!(output.document.blocks[0].block, Block::Heading { level: 1, .. }));
        assert!(
            output
                .document
                .blocks
                .iter()
                .any(|node| matches!(node.block, Block::List { kind: ListKind::Task, .. }))
        );
        assert!(
            output.document.blocks.iter().any(|node| matches!(node.block, Block::Table { .. }))
        );
        assert!(output.document.blocks.iter().any(|node| matches!(&node.block, Block::Code { language: Some(language), .. } if language == "rust")));
        assert!(
            output.document.blocks.iter().any(|node| matches!(node.block, Block::Footnote { .. }))
        );
    }

    #[test]
    fn parse_render_parse_is_semantically_stable() {
        let source = b"Title\n=====\n\nText *em* ~~gone~~ and <https://example.com>.  \nnext\n\n1. one\n   - nested\n\n```rust\nlet x = 1;\n```\n\n| A | B |\n|---|---|\n| x | y |\n\nref[^a]\n\n[^a]: note\n";
        let first = convert(source).unwrap();
        let options = ConversionOptions::default();
        let rendered =
            into_markdown_render_markdown::render(&first.document, &first.assets, &options)
                .unwrap();
        assert_ne!(rendered.as_bytes(), source, "conversion must not bypass the IR");
        let second = convert(rendered.as_bytes()).unwrap();
        let rendered_again =
            into_markdown_render_markdown::render(&second.document, &second.assets, &options)
                .unwrap();
        assert_eq!(rendered_again, rendered);
    }

    #[test]
    fn bom_offsets_and_invalid_utf8_are_explicit() {
        let output = convert(b"\xef\xbb\xbf# hi\n").unwrap();
        assert_eq!(output.document.blocks[0].provenance.locator.byte_start, Some(3));
        assert!(matches!(convert(b"# \xff\n"), Err(ConversionError::Malformed { .. })));
    }

    #[test]
    fn html_blockquote_and_images_are_safe_fallbacks() {
        let output =
            convert(b"<script>alert(1)</script>\n\n> quoted\n\n![x](https://example.com/x.png)\n")
                .unwrap();
        assert!(output.diagnostics.iter().any(|item| item.code == RAW_HTML_CODE));
        assert!(output.diagnostics.iter().any(|item| item.code == BLOCKQUOTE_CODE));
        assert!(
            output
                .diagnostics
                .iter()
                .any(|item| item.code == "markdown.externalImageReferencedOffline")
        );
        assert_eq!(output.assets.len(), 1);
        assert!(output.assets[0].bytes.is_empty());
        assert_eq!(output.assets[0].external_uri.as_deref(), Some("https://example.com/x.png"));
        assert!(
            output.document.blocks.iter().any(|node| matches!(node.block, Block::Image { .. }))
        );
    }

    #[test]
    fn inline_relative_and_secret_bearing_images_degrade_with_target_preserved() {
        let output = convert(
            b"before ![inline](https://example.com/i.png) after\n\n![relative](img.png)\n\n![secret](https://example.com/i.png?token=x)\n",
        )
        .unwrap();
        assert!(output.assets.is_empty());
        assert!(
            output
                .diagnostics
                .iter()
                .any(|item| { item.code == "markdown.inlineExternalImagePreservedAsLink" })
        );
        assert!(
            output.diagnostics.iter().filter(|item| item.code == EXTERNAL_IMAGE_CODE).count() >= 2
        );
        let rendered = into_markdown_render_markdown::render(
            &output.document,
            &output.assets,
            &ConversionOptions::default(),
        )
        .unwrap();
        assert!(rendered.contains("https://example.com/i.png"));
        assert!(rendered.contains("img.png"));
        assert!(rendered.contains("token=x"));
    }

    #[test]
    fn content_detection_is_conservative() {
        assert!(!strong_markdown_evidence("ordinary prose\nsecond line", &context()).unwrap());
        assert!(strong_markdown_evidence("# heading\n\n- item", &context()).unwrap());
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let mut options = ConversionOptions::default();
        options.limits.max_nesting_depth = 4;
        let error =
            convert_markdown(&input(b"> > > > > nested\n"), &options, &context()).unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_nesting_depth", .. }));
    }

    #[test]
    fn duplicate_definition_diagnostics_follow_parser_fences_and_first_wins() {
        let output = convert(
            b"[ref][x]\n\n[x]: https://first.example/\n[x]: https://second.example/\n\n```\n[x]: https://code.example/\n```\n",
        )
        .unwrap();
        assert_eq!(
            output.diagnostics.iter().filter(|item| item.code == DUPLICATE_DEFINITION_CODE).count(),
            1
        );
        let rendered = into_markdown_render_markdown::render(
            &output.document,
            &output.assets,
            &ConversionOptions::default(),
        )
        .unwrap();
        assert!(rendered.contains("https://first.example/"));
        assert!(!rendered.contains("https://second.example/"));
    }

    #[test]
    fn inline_html_depth_and_deep_links_are_bounded_without_recursion() {
        let mut options = ConversionOptions::default();
        options.limits.max_nesting_depth = 8;
        let html = format!("{}x{}\n", "<em>".repeat(9), "</em>".repeat(9));
        let error = convert_markdown(&input(html.as_bytes()), &options, &context()).unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_nesting_depth", .. }));

        let mut links = String::new();
        for _ in 0..64 {
            links.push('[');
        }
        links.push('x');
        for _ in 0..64 {
            links.push_str("](https://example.com)");
        }
        links.push('\n');
        let output = convert(links.as_bytes()).unwrap();
        assert!(!output.document.blocks.is_empty());

        let joined = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || convert(links.as_bytes()).map(|result| result.document.blocks.len()))
            .unwrap()
            .join();
        assert!(joined.is_ok(), "bounded Markdown parsing must not abort a small-stack thread");
        assert!(joined.unwrap().is_ok());
    }

    #[test]
    fn duplicate_scanner_ignores_indented_code_and_footnote_definitions() {
        let output =
            convert(b"    [x]: https://code.example/\n\n[^note]: one\n\n[^note]: two\n").unwrap();
        assert_eq!(
            output.diagnostics.iter().filter(|item| item.code == DUPLICATE_DEFINITION_CODE).count(),
            1
        );
    }
}
