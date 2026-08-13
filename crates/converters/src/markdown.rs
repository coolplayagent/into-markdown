//! Offline Markdown/GFM parsing into the unified document IR.

use crate::text;
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ExecutionContext,
    FormatCandidate, Inline, InlineMark, InputFormat, ListItem, ListKind, MAX_DOCUMENT_INLINES,
    MAX_DOCUMENT_NODES, NodeId, ProbeOutcome, Provenance, ProvenanceKind, ResolvedInput, Services,
    SourceLocator, TableAlignment, TableRow, canonical_external_asset_uri,
};
use pulldown_cmark::{
    Alignment, CodeBlockKind, CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use std::fmt::Write as _;
use std::ops::Range;

const FORMATS: &[InputFormat] = &[InputFormat::Markdown];
const PROVIDER_ID: &str = "builtin.converter.markdown-gfm";
const RAW_HTML_CODE: &str = "markdown.rawHtmlPreservedAsCode";
const BLOCKQUOTE_CODE: &str = "markdown.blockquotePreservedAsCode";
const EXTERNAL_IMAGE_CODE: &str = "markdown.externalImagePreservedAsLink";
const DUPLICATE_DEFINITION_CODE: &str = "markdown.duplicateDefinitionIgnored";
// Cooperative logical work weights for the pinned Markdown parser. These are deliberately not
// estimates of pulldown-cmark's allocator capacity or process RSS. Event work is bounded by an
// explicit converter limit derived from input bytes and the IR node/inline ceilings.
const PARSER_FIXED_LOGICAL_WORK_BYTES: usize = 32 * 1024;
const PARSER_EVENT_LOGICAL_WORK_BYTES: usize = 1;
const PARSER_DEPTH_LOGICAL_WORK_BYTES: usize = std::mem::size_of::<usize>();
const LOGICAL_SET_ENTRY_BYTES: usize =
    std::mem::size_of::<String>() + 3 * std::mem::size_of::<usize>();

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

    fn planned_output_bytes(
        &self,
        _: &ResolvedInput,
        _: &FormatCandidate,
        _: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<u64, ConversionError> {
        Ok(context.available_memory_bytes())
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

pub(crate) fn convert_markdown(
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
    Ok(ConverterOutput::new(document, assets, diagnostics))
}

#[derive(Debug)]
enum FrameKind {
    Root,
    Paragraph,
    Heading(u8),
    List { start: u64, ordered: bool },
    Item,
    Footnote(String),
    Table(Vec<TableAlignment>),
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
    html_marks: Vec<PendingHtmlMark>,
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
            html_marks: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct PendingHtmlMark {
    tag: &'static str,
    mark: InlineMark,
    inline_index: usize,
    span: Range<usize>,
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
    parser_event_limit: usize,
    sequence: u64,
    parser_memory: text::LogicalMemory,
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
        let mut parser_memory = text::LogicalMemory::new(context)?;
        let parser_event_limit = max_parser_events(source.text.len())?;
        parser_memory.charge(parser_logical_work_bytes(
            source.text.len(),
            options.limits.max_nesting_depth,
        )?)?;
        let mut frames = Vec::new();
        parser_memory.reserve_vec(&mut frames, 8)?;
        frames.push(Frame::new(FrameKind::Root, 0..source.text.len()));
        Ok(Self {
            source,
            options,
            context,
            frames,
            diagnostics,
            node_count: 0,
            inline_count: 0,
            parser_event_limit,
            sequence: 0,
            parser_memory,
            assets: Vec::new(),
            footnotes: std::collections::BTreeSet::new(),
        })
    }

    fn parse(&mut self) -> Result<(), ConversionError> {
        let parser = Parser::new_ext(&self.source.text, parser_options()).into_offset_iter();
        for (index, (event, span)) in parser.enumerate() {
            if index >= self.parser_event_limit {
                return Err(ConversionError::ResourceLimit {
                    limit: "markdownEvents",
                    detail: format!(
                        "Markdown parser event count exceeded {}",
                        self.parser_event_limit
                    ),
                });
            }
            if index.is_multiple_of(128) {
                self.context.checkpoint()?;
            }
            match event {
                Event::Start(tag) => self.start(tag, span)?,
                Event::End(end) => self.end(end, span)?,
                Event::Text(value) => self.text(&value)?,
                Event::Code(value) => {
                    let value = self.own_parser_text(value)?;
                    self.push_inline(Inline::Code(value))?;
                }
                Event::InlineMath(value) => {
                    let value = self.own_parser_text(value)?;
                    self.push_inline(Inline::Formula(value))?;
                }
                Event::DisplayMath(value) => {
                    let value = self.own_parser_text(value)?;
                    let node = self.node(Block::Formula(value), span)?;
                    self.push_block(node)?;
                }
                Event::Html(value) => {
                    self.diagnostic(
                        RAW_HTML_CODE,
                        "raw HTML was preserved as non-executable code",
                        &span,
                    )?;
                    let value = self.own_parser_text(value)?;
                    self.push_inline(Inline::Code(value))?;
                }
                Event::InlineHtml(value) => self.inline_html(&value, span)?,
                Event::FootnoteReference(label) => {
                    let label = normalize_footnote_label(&label, &mut self.parser_memory)?;
                    self.push_inline(Inline::FootnoteReference(label))?;
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

    #[allow(clippy::too_many_lines)]
    fn start(&mut self, tag: Tag<'_>, span: Range<usize>) -> Result<(), ConversionError> {
        self.flush_pending_html_marks()?;
        let kind = match tag {
            Tag::Paragraph => FrameKind::Paragraph,
            Tag::Heading { level, .. } => FrameKind::Heading(heading_level(level)),
            Tag::BlockQuote(_) => FrameKind::BlockQuote,
            Tag::CodeBlock(kind) => FrameKind::Code(match kind {
                CodeBlockKind::Indented => None,
                CodeBlockKind::Fenced(info) => {
                    let language = info.split_whitespace().next().unwrap_or_default().trim();
                    if language.is_empty() { None } else { Some(self.owned_text(language)?) }
                }
            }),
            Tag::HtmlBlock | Tag::MetadataBlock(_) => FrameKind::HtmlBlock,
            Tag::List(start) => {
                FrameKind::List { start: start.unwrap_or(1), ordered: start.is_some() }
            }
            Tag::Item => FrameKind::Item,
            Tag::FootnoteDefinition(label) => {
                let label = normalize_footnote_label(&label, &mut self.parser_memory)?;
                FrameKind::Footnote(label)
            }
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                FrameKind::BlockQuote
            }
            Tag::Table(alignments) => {
                let mut converted = Vec::new();
                self.parser_memory.reserve_vec(&mut converted, alignments.len())?;
                converted.extend(alignments.into_iter().map(|alignment| match alignment {
                    Alignment::None => TableAlignment::None,
                    Alignment::Left => TableAlignment::Left,
                    Alignment::Center => TableAlignment::Center,
                    Alignment::Right => TableAlignment::Right,
                }));
                FrameKind::Table(converted)
            }
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
            Tag::Link { dest_url, .. } => FrameKind::Link(self.own_parser_text(dest_url)?),
            Tag::Image { dest_url, .. } => FrameKind::Image(self.own_parser_text(dest_url)?),
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
                | FrameKind::Table(_)
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
                        | FrameKind::Table(_)
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
        self.parser_memory.reserve_vec(&mut self.frames, 1)?;
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
        for pending in &frame.html_marks {
            self.diagnostic(
                RAW_HTML_CODE,
                "unclosed raw inline HTML was preserved as non-executable code",
                &pending.span,
            )?;
        }
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
                    if image_target_has_extension(&image.target, "svg") {
                        self.diagnostic(
                            "markdown.externalSvgMayContainActiveContent",
                            "external SVG was preserved only as a URI reference and may contain active content when opened by a consumer",
                            &image.span,
                        )?;
                    }
                    let asset_id = self.asset_id()?;
                    self.charge_text(&image.target, "Markdown external URI")?;
                    self.charge_text(&image.alt, "Markdown image alt")?;
                    self.parser_memory.charge(asset_id.len())?;
                    self.parser_memory.charge(image_media_type(&image.target).len())?;
                    self.parser_memory.reserve_vec(&mut self.assets, 1)?;
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
                let mut group = Vec::new();
                let mut group_task = frame.items.first().is_some_and(|item| item.checked.is_some());
                let mut group_offset = 0_usize;
                for (offset, item) in frame.items.into_iter().enumerate() {
                    let item_task = item.checked.is_some();
                    if !group.is_empty() && item_task != group_task {
                        self.push_list_group(
                            std::mem::take(&mut group),
                            group_task,
                            ordered,
                            start,
                            group_offset,
                            frame.span.clone(),
                        )?;
                        group_offset = offset;
                        group_task = item_task;
                    }
                    self.parser_memory.reserve_vec(&mut group, 1)?;
                    group.push(item);
                }
                if !group.is_empty() {
                    self.push_list_group(
                        group,
                        group_task,
                        ordered,
                        start,
                        group_offset,
                        frame.span,
                    )?;
                }
                Ok(())
            }
            FrameKind::Item => {
                self.consume_structural_container()?;
                if !frame.inlines.is_empty() {
                    let paragraph =
                        self.node(Block::Paragraph(frame.inlines), frame.span.clone())?;
                    self.parser_memory.reserve_vec(&mut frame.blocks, 1)?;
                    frame.blocks.insert(0, paragraph);
                }
                let item =
                    ListItem { checked: frame.checked, marker_label: None, blocks: frame.blocks };
                let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
                let parent = frames.last_mut().ok_or_else(|| ConversionError::Internal {
                    detail: "Markdown event stack is empty".into(),
                })?;
                memory.reserve_vec(&mut parent.items, 1)?;
                parent.items.push(item);
                Ok(())
            }
            FrameKind::Footnote(label) => {
                self.parser_memory.charge(
                    label.len().checked_add(LOGICAL_SET_ENTRY_BYTES).ok_or_else(memory_overflow)?,
                )?;
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
                    self.parser_memory.reserve_vec(&mut frame.blocks, 1)?;
                    frame.blocks.push(paragraph);
                }
                let node =
                    self.node(Block::Footnote { label, blocks: frame.blocks }, frame.span)?;
                self.push_block(node)
            }
            FrameKind::Table(alignments) => {
                let node = self.node(Block::Table { rows: frame.rows, alignments }, frame.span)?;
                self.push_block(node)
            }
            FrameKind::TableHead => {
                if !frame.cells.is_empty() {
                    self.parser_memory.reserve_vec(&mut frame.rows, 1)?;
                    frame.rows.push(TableRow { cells: std::mem::take(&mut frame.cells) });
                }
                let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
                let parent = frames.last_mut().ok_or_else(|| ConversionError::Internal {
                    detail: "Markdown event stack is empty".into(),
                })?;
                memory.reserve_vec(&mut parent.rows, frame.rows.len())?;
                parent.rows.append(&mut frame.rows);
                Ok(())
            }
            FrameKind::TableRow => {
                self.consume_structural_container()?;
                let row = TableRow { cells: frame.cells };
                let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
                let parent = frames.last_mut().ok_or_else(|| ConversionError::Internal {
                    detail: "Markdown event stack is empty".into(),
                })?;
                memory.reserve_vec(&mut parent.rows, 1)?;
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
                    self.parser_memory.reserve_vec(&mut frame.blocks, 1)?;
                    frame.blocks.push(paragraph);
                }
                let cell = Cell { row_span: 1, column_span: 1, header, blocks: frame.blocks };
                let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
                let parent = frames.last_mut().ok_or_else(|| ConversionError::Internal {
                    detail: "Markdown event stack is empty".into(),
                })?;
                memory.reserve_vec(&mut parent.cells, 1)?;
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
                let raw =
                    self.owned_text(self.source.text.get(frame.span.clone()).unwrap_or_default())?;
                self.parser_memory.charge("markdown-blockquote".len())?;
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
                let raw =
                    self.owned_text(self.source.text.get(frame.span.clone()).unwrap_or_default())?;
                self.parser_memory.charge("html".len())?;
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
                    self.parser_memory.charge(target.len().saturating_add(alt.len()))?;
                    let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
                    let parent = frames.last_mut().ok_or_else(|| ConversionError::Internal {
                        detail: "Markdown event stack is empty".into(),
                    })?;
                    memory.reserve_vec(&mut parent.images, 1)?;
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
                    let required = alt
                        .len()
                        .checked_add(target.len())
                        .and_then(|value| value.checked_add(3))
                        .ok_or_else(memory_overflow)?;
                    let mut value = String::new();
                    self.parser_memory.reserve_string(&mut value, required)?;
                    write!(&mut value, "{alt} ({target})").map_err(|_| {
                        ConversionError::Internal {
                            detail: "write to Markdown fallback String failed".into(),
                        }
                    })?;
                    self.push_inline(Inline::Text { value, marks: Vec::new() })
                }
            }
            FrameKind::Root => {
                Err(ConversionError::Internal { detail: "closed Markdown root frame".into() })
            }
        }
    }

    fn text(&mut self, value: &str) -> Result<(), ConversionError> {
        if matches!(self.frames.last().map(|frame| &frame.kind), Some(FrameKind::Code(_))) {
            let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
            let frame = frames.last_mut().ok_or_else(|| ConversionError::Internal {
                detail: "Markdown event stack is empty".into(),
            })?;
            memory.reserve_string(&mut frame.literal, value.len())?;
            frame.literal.push_str(value);
            Ok(())
        } else {
            let value = self.owned_text(value)?;
            self.push_inline(Inline::Text { value, marks: Vec::new() })
        }
    }

    fn push_list_group(
        &mut self,
        items: Vec<ListItem>,
        task: bool,
        ordered: bool,
        source_start: u64,
        offset: usize,
        span: Range<usize>,
    ) -> Result<(), ConversionError> {
        let kind = if task {
            ListKind::Task
        } else if ordered {
            ListKind::Ordered
        } else {
            ListKind::Bullet
        };
        let offset = u64::try_from(offset).map_err(|_| ConversionError::ResourceLimit {
            limit: "documentNodes",
            detail: "Markdown ordered-list offset cannot be represented as u64".into(),
        })?;
        let start = if ordered && !task {
            source_start.checked_add(offset).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "documentNodes",
                detail: "Markdown ordered-list start overflowed u64".into(),
            })?
        } else {
            1
        };
        let node = self.node(Block::List { kind, start, items }, span)?;
        self.push_block(node)
    }

    fn inline_html(&mut self, value: &str, span: Range<usize>) -> Result<(), ConversionError> {
        if let Some((inner, mark)) = safe_self_contained_html_mark(value) {
            let inner = self.owned_text(inner)?;
            let mut marks = Vec::new();
            self.parser_memory.reserve_vec(&mut marks, 1)?;
            marks.push(mark);
            self.push_inline(Inline::Text { value: inner, marks })?;
            return Ok(());
        }
        let normalized = value.trim();
        if let Some((tag, mark)) = html_mark_open(normalized) {
            self.inline_count = self.inline_count.saturating_add(1);
            if self.inline_count > MAX_DOCUMENT_INLINES {
                return Err(ConversionError::ResourceLimit {
                    limit: "documentInlines",
                    detail: format!("{} > {MAX_DOCUMENT_INLINES}", self.inline_count),
                });
            }
            let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
            let frame = frames.last_mut().ok_or_else(|| ConversionError::Internal {
                detail: "Markdown event stack is empty".into(),
            })?;
            memory.reserve_vec(&mut frame.inlines, 1)?;
            let mut owned = String::new();
            memory.reserve_string(&mut owned, value.len())?;
            owned.push_str(value);
            let inline_index = frame.inlines.len();
            frame.inlines.push(Inline::Code(owned));
            memory.reserve_vec(&mut frame.html_marks, 1)?;
            frame.html_marks.push(PendingHtmlMark { tag, mark, inline_index, span });
            return Ok(());
        }
        if let Some(tag) = html_mark_close(normalized) {
            let frame = self.frames.last_mut().ok_or_else(|| ConversionError::Internal {
                detail: "Markdown event stack is empty".into(),
            })?;
            if frame.html_marks.last().is_some_and(|pending| pending.tag == tag) {
                let pending = frame.html_marks.pop().ok_or_else(|| ConversionError::Internal {
                    detail: "Markdown inline HTML matcher disappeared".into(),
                })?;
                if !matches!(frame.inlines.get(pending.inline_index), Some(Inline::Code(_))) {
                    return Err(ConversionError::Internal {
                        detail: "Markdown inline HTML opener moved unexpectedly".into(),
                    });
                }
                let removed_index = pending.inline_index;
                frame.inlines.remove(removed_index);
                self.inline_count = self.inline_count.saturating_sub(1);
                for open in &mut frame.html_marks {
                    if open.inline_index > removed_index {
                        open.inline_index -= 1;
                    }
                }
                for inline in &mut frame.inlines[removed_index..] {
                    reserve_mark(inline, pending.mark, &mut self.parser_memory)?;
                    apply_mark(inline, pending.mark);
                }
                return Ok(());
            }
        }
        self.diagnostic(
            RAW_HTML_CODE,
            "raw inline HTML was preserved as non-executable code",
            &span,
        )?;
        let value = self.owned_text(value)?;
        self.push_inline(Inline::Code(value))
    }

    fn finish_mark(
        &mut self,
        mut inlines: Vec<Inline>,
        mark: InlineMark,
    ) -> Result<(), ConversionError> {
        for inline in &mut inlines {
            reserve_mark(inline, mark, &mut self.parser_memory)?;
            apply_mark(inline, mark);
        }
        self.extend_inlines(inlines)
    }

    fn flush_pending_html_marks(&mut self) -> Result<(), ConversionError> {
        let pending = self
            .frames
            .last_mut()
            .map(|frame| std::mem::take(&mut frame.html_marks))
            .unwrap_or_default();
        for open in pending {
            self.diagnostic(
                RAW_HTML_CODE,
                "raw inline HTML crossing a CommonMark container was preserved as code",
                &open.span,
            )?;
        }
        Ok(())
    }

    fn push_inline(&mut self, inline: Inline) -> Result<(), ConversionError> {
        self.inline_count = self.inline_count.saturating_add(1);
        if self.inline_count > MAX_DOCUMENT_INLINES {
            return Err(ConversionError::ResourceLimit {
                limit: "documentInlines",
                detail: format!("{} > {MAX_DOCUMENT_INLINES}", self.inline_count),
            });
        }
        let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
        let parent = frames.last_mut().ok_or_else(|| ConversionError::Internal {
            detail: "Markdown event stack is empty".into(),
        })?;
        memory.reserve_vec(&mut parent.inlines, 1)?;
        parent.inlines.push(inline);
        Ok(())
    }

    fn extend_inlines(&mut self, inlines: Vec<Inline>) -> Result<(), ConversionError> {
        let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
        let parent = frames.last_mut().ok_or_else(|| ConversionError::Internal {
            detail: "Markdown event stack is empty".into(),
        })?;
        memory.reserve_vec(&mut parent.inlines, inlines.len())?;
        parent.inlines.extend(inlines);
        Ok(())
    }

    fn push_block(&mut self, block: BlockNode) -> Result<(), ConversionError> {
        let (memory, frames) = (&mut self.parser_memory, &mut self.frames);
        let parent = frames.last_mut().ok_or_else(|| ConversionError::Internal {
            detail: "Markdown event stack is empty".into(),
        })?;
        memory.reserve_vec(&mut parent.blocks, 1)?;
        parent.blocks.push(block);
        Ok(())
    }

    fn node(&mut self, block: Block, span: Range<usize>) -> Result<BlockNode, ConversionError> {
        self.consume_structural_container()?;
        self.sequence =
            self.sequence.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "documentNodes",
                detail: "Markdown node sequence overflowed".into(),
            })?;
        let (start, end) = self.source.source_range(span.start, span.end);
        let id = self.node_id()?;
        self.parser_memory.charge(PROVIDER_ID.len())?;
        Ok(BlockNode {
            id: NodeId(id),
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
        Ok(())
    }

    fn diagnostic(
        &mut self,
        code: &str,
        message: &str,
        span: &Range<usize>,
    ) -> Result<(), ConversionError> {
        self.parser_memory.reserve_vec(&mut self.diagnostics, 1)?;
        self.parser_memory.charge(code.len().saturating_add(message.len()))?;
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
        let _ = label;
        self.parser_memory.charge(value.len())
    }

    fn owned_text(&mut self, value: &str) -> Result<String, ConversionError> {
        let mut owned = String::new();
        self.parser_memory.reserve_string(&mut owned, value.len())?;
        owned.push_str(value);
        Ok(owned)
    }

    fn own_parser_text(&mut self, value: CowStr<'_>) -> Result<String, ConversionError> {
        // Cow ownership inside pulldown-cmark belongs to the parser. Account the logical content
        // once when it crosses into the project-owned IR, regardless of whether this is a move or
        // a copy at the allocator level.
        self.parser_memory.charge(value.len())?;
        Ok(value.into_string())
    }

    fn node_id(&mut self) -> Result<String, ConversionError> {
        let digits = usize::try_from(self.sequence.ilog10()).unwrap_or(19).saturating_add(1);
        let capacity = "markdown-".len().checked_add(digits).ok_or_else(memory_overflow)?;
        let mut id = String::new();
        self.parser_memory.reserve_string(&mut id, capacity)?;
        write!(&mut id, "markdown-{}", self.sequence).map_err(|_| ConversionError::Internal {
            detail: "write to Markdown node ID failed".into(),
        })?;
        Ok(id)
    }

    fn asset_id(&mut self) -> Result<String, ConversionError> {
        let sequence = self.assets.len().checked_add(1).ok_or_else(memory_overflow)?;
        let digits = sequence.checked_ilog10().unwrap_or(0) as usize + 1;
        let capacity =
            "markdown-external-image-".len().checked_add(digits).ok_or_else(memory_overflow)?;
        let mut id = String::new();
        self.parser_memory.reserve_string(&mut id, capacity)?;
        write!(&mut id, "markdown-external-image-{sequence}").map_err(|_| {
            ConversionError::Internal { detail: "write to Markdown asset ID failed".into() }
        })?;
        Ok(id)
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

fn html_mark_open(value: &str) -> Option<(&'static str, InlineMark)> {
    if value.eq_ignore_ascii_case("<strong>") {
        Some(("strong", InlineMark::Bold))
    } else if value.eq_ignore_ascii_case("<em>") {
        Some(("em", InlineMark::Italic))
    } else if value.eq_ignore_ascii_case("<del>") {
        Some(("del", InlineMark::Strikethrough))
    } else if value.eq_ignore_ascii_case("<sup>") {
        Some(("sup", InlineMark::Superscript))
    } else if value.eq_ignore_ascii_case("<sub>") {
        Some(("sub", InlineMark::Subscript))
    } else {
        None
    }
}

fn html_mark_close(value: &str) -> Option<&'static str> {
    if value.eq_ignore_ascii_case("</strong>") {
        Some("strong")
    } else if value.eq_ignore_ascii_case("</em>") {
        Some("em")
    } else if value.eq_ignore_ascii_case("</del>") {
        Some("del")
    } else if value.eq_ignore_ascii_case("</sup>") {
        Some("sup")
    } else if value.eq_ignore_ascii_case("</sub>") {
        Some("sub")
    } else {
        None
    }
}

fn parser_logical_work_bytes(
    input_bytes: usize,
    max_nesting_depth: u16,
) -> Result<usize, ConversionError> {
    let event_units = max_parser_events(input_bytes)?;
    let event_work =
        event_units.checked_mul(PARSER_EVENT_LOGICAL_WORK_BYTES).ok_or_else(memory_overflow)?;
    let depth_units = usize::from(max_nesting_depth).checked_add(1).ok_or_else(memory_overflow)?;
    let depth_work =
        depth_units.checked_mul(PARSER_DEPTH_LOGICAL_WORK_BYTES).ok_or_else(memory_overflow)?;
    PARSER_FIXED_LOGICAL_WORK_BYTES
        .checked_add(input_bytes)
        .and_then(|value| value.checked_add(event_work))
        .and_then(|value| value.checked_add(depth_work))
        .ok_or_else(memory_overflow)
}

fn max_parser_events(input_bytes: usize) -> Result<usize, ConversionError> {
    let structural_ceiling = MAX_DOCUMENT_NODES
        .checked_add(MAX_DOCUMENT_INLINES)
        .and_then(|value| value.checked_mul(2))
        .and_then(|value| value.checked_add(1))
        .ok_or_else(memory_overflow)?;
    let source_ceiling = input_bytes
        .checked_mul(2)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(memory_overflow)?;
    Ok(source_ceiling.min(structural_ceiling))
}

fn memory_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "Markdown logical work budget overflowed".into(),
    }
}

fn parser_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
}

fn safe_self_contained_html_mark(value: &str) -> Option<(&str, InlineMark)> {
    let candidates = [
        ("<strong>", "</strong>", InlineMark::Bold),
        ("<em>", "</em>", InlineMark::Italic),
        ("<del>", "</del>", InlineMark::Strikethrough),
        ("<sup>", "</sup>", InlineMark::Superscript),
        ("<sub>", "</sub>", InlineMark::Subscript),
    ];
    for (open, close, mark) in candidates {
        if let Some(inner) = value.strip_prefix(open).and_then(|rest| rest.strip_suffix(close))
            && !inner.is_empty()
            && !inner.contains(['<', '>'])
        {
            return Some((inner, mark));
        }
    }
    None
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

fn normalize_footnote_label(
    label: &str,
    memory: &mut text::LogicalMemory,
) -> Result<String, ConversionError> {
    let Some(hex) = label.strip_prefix("fn-") else {
        let mut owned = String::new();
        memory.reserve_string(&mut owned, label.len())?;
        owned.push_str(label);
        return Ok(owned);
    };
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        let mut owned = String::new();
        memory.reserve_string(&mut owned, label.len())?;
        owned.push_str(label);
        return Ok(owned);
    }
    let mut bytes = Vec::new();
    memory.reserve_vec(&mut bytes, hex.len() / 2)?;
    for pair in hex.as_bytes().chunks_exact(2) {
        let (Some(high), Some(low)) = (hex_value(pair[0]), hex_value(pair[1])) else {
            let mut owned = String::new();
            memory.reserve_string(&mut owned, label.len())?;
            owned.push_str(label);
            return Ok(owned);
        };
        bytes.push((high << 4) | low);
    }
    if let Ok(value) = String::from_utf8(bytes) {
        Ok(value)
    } else {
        let mut owned = String::new();
        memory.reserve_string(&mut owned, label.len())?;
        owned.push_str(label);
        Ok(owned)
    }
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

fn reserve_mark(
    inline: &mut Inline,
    mark: InlineMark,
    memory: &mut text::LogicalMemory,
) -> Result<(), ConversionError> {
    match inline {
        Inline::Text { marks, .. } => {
            if !marks.contains(&mark) {
                memory.reserve_vec(marks, 1)?;
            }
        }
        Inline::Link { content, .. } => {
            for nested in content {
                reserve_mark(nested, mark, memory)?;
            }
        }
        _ => {}
    }
    Ok(())
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
    memory: &mut text::LogicalMemory,
) -> Result<String, ConversionError> {
    let mut output = String::new();
    let mut stack = Vec::new();
    memory.reserve_vec(&mut stack, inlines.len())?;
    stack.extend(inlines.iter().rev());
    while let Some(inline) = stack.pop() {
        match inline {
            Inline::Text { value, .. } | Inline::Code(value) | Inline::Formula(value) => {
                memory.reserve_string(&mut output, value.len())?;
                output.push_str(value);
            }
            Inline::Link { content, .. } => {
                memory.reserve_vec(&mut stack, content.len())?;
                stack.extend(content.iter().rev());
            }
            Inline::FootnoteReference(label) => {
                memory.reserve_string(&mut output, label.len().saturating_add(3))?;
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
    !["javascript", "vbscript", "data", "file"]
        .iter()
        .any(|blocked| scheme.eq_ignore_ascii_case(blocked))
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
    if extension.is_some_and(|value| value.eq_ignore_ascii_case("png")) {
        "image/png"
    } else if extension.is_some_and(|value| {
        value.eq_ignore_ascii_case("jpg") || value.eq_ignore_ascii_case("jpeg")
    }) {
        "image/jpeg"
    } else if extension.is_some_and(|value| value.eq_ignore_ascii_case("gif")) {
        "image/gif"
    } else if extension.is_some_and(|value| value.eq_ignore_ascii_case("webp")) {
        "image/webp"
    } else if extension.is_some_and(|value| value.eq_ignore_ascii_case("svg")) {
        "image/svg+xml"
    } else if extension.is_some_and(|value| value.eq_ignore_ascii_case("avif")) {
        "image/avif"
    } else {
        "application/octet-stream"
    }
}

fn image_target_has_extension(target: &str, expected: &str) -> bool {
    target
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.').map(|(_, extension)| extension))
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
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
            let source_label = trimmed[1..=close].trim();
            decoded.memory.charge(
                source_label
                    .len()
                    .checked_add(LOGICAL_SET_ENTRY_BYTES)
                    .ok_or_else(memory_overflow)?,
            )?;
            let label = logical_ascii_lowercase(source_label, &mut decoded.memory)?;
            if !label.is_empty() && !label.starts_with('^') && seen.contains(&label) {
                let leading = line.len() - trimmed.len();
                let start = offset + leading;
                let end = offset + line.trim_end_matches(['\r', '\n']).len();
                let (source_start, source_end) = decoded.source_range(start, end);
                decoded.memory.reserve_vec(diagnostics, 1)?;
                decoded.memory.charge(
                    DUPLICATE_DEFINITION_CODE.len().saturating_add(label.len()).saturating_add(72),
                )?;
                diagnostics.push(Diagnostic {
                    code: DUPLICATE_DEFINITION_CODE.into(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!(
                        "duplicate definition {label:?} was ignored; the first definition wins"
                    ),
                    locator: Some(byte_locator(source_start, source_end)),
                });
            } else if !label.is_empty() && !label.starts_with('^') {
                seen.insert(label);
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

fn logical_ascii_lowercase(
    value: &str,
    memory: &mut text::LogicalMemory,
) -> Result<String, ConversionError> {
    let mut normalized = String::new();
    memory.reserve_string(&mut normalized, value.len())?;
    normalized.extend(value.chars().map(|character| character.to_ascii_lowercase()));
    Ok(normalized)
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
    fn inline_html_is_local_and_deep_links_are_bounded_without_recursion() {
        let mut options = ConversionOptions::default();
        options.limits.max_nesting_depth = 8;
        let html = format!("{}x{}\n", "<em>".repeat(9), "</em>".repeat(9));
        let output = convert_markdown(&input(html.as_bytes()), &options, &context()).unwrap();
        assert!(!output.document.blocks.is_empty());

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
    fn mixed_lists_split_into_valid_reversible_groups() {
        let source =
            b"3. plain\n4. [x] task\n5. plain again\n   - nested bullet\n   - [ ] nested task\n";
        let first = convert(source).unwrap();
        first.document.validate().unwrap();
        let kinds = first
            .document
            .blocks
            .iter()
            .filter_map(|node| match node.block {
                Block::List { kind, start, .. } => Some((kind, start)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![(ListKind::Ordered, 3), (ListKind::Task, 1), (ListKind::Ordered, 5)]
        );
        let rendered = into_markdown_render_markdown::render(
            &first.document,
            &first.assets,
            &ConversionOptions::default(),
        )
        .unwrap();
        let second = convert(rendered.as_bytes()).unwrap();
        second.document.validate().unwrap();
        let rendered_again = into_markdown_render_markdown::render(
            &second.document,
            &second.assets,
            &ConversionOptions::default(),
        )
        .unwrap();
        assert_eq!(rendered_again, rendered);
    }

    #[test]
    fn table_alignments_survive_parse_render_parse() {
        let source = b"| L | C | R | N |\n|:---|:---:|---:|---|\n| a | b | c | d |\n";
        let first = convert(source).unwrap();
        let Block::Table { alignments, .. } = &first.document.blocks[0].block else { panic!() };
        assert_eq!(
            alignments,
            &[
                TableAlignment::Left,
                TableAlignment::Center,
                TableAlignment::Right,
                TableAlignment::None,
            ]
        );
        let rendered = into_markdown_render_markdown::render(
            &first.document,
            &first.assets,
            &ConversionOptions::default(),
        )
        .unwrap();
        assert!(rendered.contains("| :--- | :---: | ---: | --- |"));
        let second = convert(rendered.as_bytes()).unwrap();
        let Block::Table { alignments, .. } = &second.document.blocks[0].block else { panic!() };
        assert_eq!(
            alignments,
            &[
                TableAlignment::Left,
                TableAlignment::Center,
                TableAlignment::Right,
                TableAlignment::None,
            ]
        );
        let rendered_again = into_markdown_render_markdown::render(
            &second.document,
            &second.assets,
            &ConversionOptions::default(),
        )
        .unwrap();
        assert_eq!(rendered_again, rendered);
    }

    #[test]
    fn interleaved_and_unclosed_inline_html_never_corrupts_parser_frames() {
        for source in ["a <em>x</strong> y\n", "a <em>x\n", "a </em>x<strong> y\n"] {
            let output = convert(source.as_bytes()).unwrap();
            output.document.validate().unwrap();
            assert!(output.diagnostics.iter().any(|item| item.code == RAW_HTML_CODE));
        }

        let nested = convert(b"<em><em>x</em></em>\n").unwrap();
        nested.document.validate().unwrap();
        let rendered = into_markdown_render_markdown::render(
            &nested.document,
            &nested.assets,
            &ConversionOptions::default(),
        )
        .unwrap();
        assert_eq!(rendered, "<em>x</em>\n");

        let crossing = convert(b"<em>[x](https://example.com)</em>\n").unwrap();
        crossing.document.validate().unwrap();
        assert!(crossing.diagnostics.iter().any(|item| item.code == RAW_HTML_CODE));
        let Block::Paragraph(inlines) = &crossing.document.blocks[0].block else { panic!() };
        assert!(matches!(inlines.first(), Some(Inline::Code(value)) if value == "<em>"));
        assert!(matches!(inlines.last(), Some(Inline::Code(value)) if value == "</em>"));
    }

    #[test]
    fn markdown_logical_work_obeys_the_execution_memory_budget() {
        let limits = ResourceLimits { max_memory_bytes: 32, ..ResourceLimits::default() };
        let limited = ExecutionContext::new(ExecutionOptions::default(), limits);
        let error = convert_markdown(
            &input(b"# heading\n\n- item one\n- item two\n"),
            &ConversionOptions::default(),
            &limited,
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
    }

    #[test]
    fn parser_logical_work_budget_is_checked_before_parser_construction() {
        let options = ConversionOptions::default();
        let work = parser_logical_work_bytes(0, options.limits.max_nesting_depth).unwrap();
        let limits = ResourceLimits {
            max_memory_bytes: u64::try_from(work - 1).unwrap(),
            ..ResourceLimits::default()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits);
        let decoded = text::decode_source(
            b"",
            Some("utf-8"),
            options.text.decoding_mode,
            &ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default()),
        )
        .unwrap()
        .0;
        let Err(error) = Builder::new(&decoded, &options, &context, Vec::new()) else {
            panic!("parser logical work unexpectedly fit below its boundary");
        };
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));

        assert!(matches!(
            parser_logical_work_bytes(usize::MAX, options.limits.max_nesting_depth),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
    }

    #[test]
    fn markdown_node_and_inline_limits_are_controlled_errors() {
        let options = ConversionOptions::default();
        let context = context();
        let decoded = text::decode_source(b"", Some("utf-8"), options.text.decoding_mode, &context)
            .unwrap()
            .0;
        let mut builder = Builder::new(&decoded, &options, &context, Vec::new()).unwrap();
        builder.node_count = MAX_DOCUMENT_NODES;
        assert!(matches!(
            builder.consume_structural_container(),
            Err(ConversionError::ResourceLimit { limit: "documentNodes", .. })
        ));

        builder.inline_count = MAX_DOCUMENT_INLINES;
        assert!(matches!(
            builder.push_inline(Inline::LineBreak),
            Err(ConversionError::ResourceLimit { limit: "documentInlines", .. })
        ));

        let event_decoded =
            text::decode_source(b"text\n", Some("utf-8"), options.text.decoding_mode, &context)
                .unwrap()
                .0;
        let mut event_builder =
            Builder::new(&event_decoded, &options, &context, Vec::new()).unwrap();
        event_builder.parser_event_limit = 0;
        assert!(matches!(
            event_builder.parse(),
            Err(ConversionError::ResourceLimit { limit: "markdownEvents", .. })
        ));
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
