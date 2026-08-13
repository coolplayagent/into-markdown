//! Offline HTML5 parsing and deterministic semantic extraction.

use html5ever::interface::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, ParseOpts, QualName, parse_document};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, DocumentMetadata,
    ExecutionContext, FormatCandidate, Inline, InlineMark, InputFormat, ListItem, ListKind,
    MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES, NodeId, ProbeOutcome, Provenance, ProvenanceKind,
    ResolvedInput, Services, SourceLocator, TableAlignment, TableRow, canonical_external_asset_uri,
};
use std::borrow::Cow;
use std::cell::{Cell as MutCell, Ref, RefCell};
use std::collections::BTreeSet;
use std::mem::size_of;
use url::Url;

use super::text::{DecodedText, LogicalMemory, decode_source};

const FORMATS: &[InputFormat] = &[InputFormat::Html];
const PROVIDER_ID: &str = "builtin.converter.html";
const MAX_HTML_EVENTS: usize = 1_000_000;
const META_PRESCAN_BYTES: usize = 1024;
const CHECKPOINT_EVENTS: usize = 1024;

/// Browser-compatible HTML5 parser with an offline semantic extractor.
#[derive(Debug, Default)]
pub struct HtmlConverter;

impl Converter for HtmlConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn priority(&self) -> i32 {
        210
    }
    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
    }

    fn probe<'a>(
        &'a self,
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if candidate.format != InputFormat::Html {
                return Ok(ProbeOutcome::NotApplicable);
            }
            Ok(
                if candidate.explicit
                    || candidate.detector_id == "builtin.detector.hints"
                    || super::html_document_evidence(&input.bytes)
                {
                    ProbeOutcome::Match { confidence: 1.0 }
                } else {
                    ProbeOutcome::NotApplicable
                },
            )
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
        Box::pin(async move { convert_html(input, options, context) })
    }
}

#[derive(Clone)]
enum NodeData {
    Document,
    Element { name: QualName, attrs: Vec<Attribute>, template: Option<usize> },
    Text(String),
    Other,
}

#[derive(Clone)]
struct DomNode {
    parent: Option<usize>,
    children: Vec<usize>,
    depth: usize,
    data: NodeData,
}

struct Dom {
    nodes: RefCell<Vec<DomNode>>,
    error: RefCell<Option<ConversionError>>,
    parse_errors: MutCell<usize>,
    events: MutCell<usize>,
    max_depth: usize,
    context: ExecutionContext,
    memory: RefCell<LogicalMemory>,
}

impl Dom {
    fn new(
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        let mut memory = LogicalMemory::new(context)?;
        memory.charge(size_of::<DomNode>())?;
        memory.charge(size_of::<DomNode>())?;
        Ok(Self {
            nodes: RefCell::new(vec![
                DomNode { parent: None, children: Vec::new(), depth: 0, data: NodeData::Document },
                DomNode {
                    parent: None,
                    children: Vec::new(),
                    depth: 0,
                    data: NodeData::Element {
                        name: QualName::new(
                            None,
                            html5ever::ns!(html),
                            html5ever::local_name!("span"),
                        ),
                        attrs: Vec::new(),
                        template: None,
                    },
                },
            ]),
            error: RefCell::new(None),
            parse_errors: MutCell::new(0),
            events: MutCell::new(0),
            max_depth: usize::from(options.limits.max_nesting_depth),
            context: context.clone(),
            memory: RefCell::new(memory),
        })
    }

    fn event(&self) -> bool {
        if self.error.borrow().is_some() {
            return false;
        }
        let next = self.events.get().saturating_add(1);
        self.events.set(next);
        if next > MAX_HTML_EVENTS {
            *self.error.borrow_mut() = Some(ConversionError::ResourceLimit {
                limit: "html_events",
                detail: format!("HTML parser exceeded {MAX_HTML_EVENTS} tree events"),
            });
            return false;
        }
        if next.is_multiple_of(CHECKPOINT_EVENTS)
            && let Err(error) = self.context.checkpoint()
        {
            *self.error.borrow_mut() = Some(error);
            return false;
        }
        true
    }

    fn add(&self, data: NodeData) -> usize {
        if !self.event() {
            return 1;
        }
        let mut nodes = self.nodes.borrow_mut();
        if nodes.len() >= MAX_DOCUMENT_NODES {
            *self.error.borrow_mut() = Some(ConversionError::ResourceLimit {
                limit: "html_nodes",
                detail: format!("HTML DOM exceeded {MAX_DOCUMENT_NODES} nodes"),
            });
            return 1;
        }
        let logical = size_of::<DomNode>()
            + match &data {
                NodeData::Element { attrs, .. } => {
                    attrs.iter().map(|a| a.name.local.len() + a.value.len()).sum()
                }
                NodeData::Text(value) => value.len(),
                _ => 0,
            };
        if let Err(error) = self.memory.borrow_mut().charge(logical) {
            *self.error.borrow_mut() = Some(error);
            return 1;
        }
        let id = nodes.len();
        nodes.push(DomNode { parent: None, children: Vec::new(), depth: 0, data });
        id
    }

    fn detach(&self, child: usize) {
        let parent = self.nodes.borrow().get(child).and_then(|node| node.parent);
        if let Some(parent) = parent {
            let mut nodes = self.nodes.borrow_mut();
            if let Some(position) = nodes[parent].children.iter().position(|id| *id == child) {
                nodes[parent].children.remove(position);
            }
            nodes[child].parent = None;
        }
    }

    fn insert(&self, parent: usize, child: usize, before: Option<usize>) {
        if !self.event()
            || parent >= self.nodes.borrow().len()
            || child >= self.nodes.borrow().len()
        {
            return;
        }
        self.detach(child);
        let depth = self.nodes.borrow()[parent].depth.saturating_add(1);
        let relative_height = {
            let nodes = self.nodes.borrow();
            let base = nodes[child].depth;
            let mut height = 0;
            let mut stack = vec![child];
            while let Some(id) = stack.pop() {
                height = height.max(nodes[id].depth.saturating_sub(base));
                stack.extend(nodes[id].children.iter().copied());
            }
            height
        };
        if depth.saturating_add(relative_height) > self.max_depth {
            *self.error.borrow_mut() = Some(ConversionError::ResourceLimit {
                limit: "html_nesting_depth",
                detail: format!("HTML DOM exceeded {} levels", self.max_depth),
            });
            return;
        }
        let mut nodes = self.nodes.borrow_mut();
        let old_depth = nodes[child].depth;
        nodes[child].parent = Some(parent);
        let position = before
            .and_then(|id| nodes[parent].children.iter().position(|child| *child == id))
            .unwrap_or(nodes[parent].children.len());
        nodes[parent].children.insert(position, child);
        let mut stack = vec![child];
        while let Some(id) = stack.pop() {
            let relative = nodes[id].depth.saturating_sub(old_depth);
            nodes[id].depth = depth.saturating_add(relative);
            stack.extend(nodes[id].children.iter().copied());
        }
    }

    fn append_item(&self, parent: usize, item: NodeOrText<usize>, before: Option<usize>) {
        match item {
            NodeOrText::AppendNode(child) => self.insert(parent, child, before),
            NodeOrText::AppendText(text) => {
                if text.is_empty() {
                    return;
                }
                let previous = {
                    let nodes = self.nodes.borrow();
                    let position = before
                        .and_then(|id| nodes[parent].children.iter().position(|child| *child == id))
                        .unwrap_or(nodes[parent].children.len());
                    position
                        .checked_sub(1)
                        .and_then(|index| nodes[parent].children.get(index))
                        .copied()
                };
                if let Some(previous) = previous {
                    let mut nodes = self.nodes.borrow_mut();
                    if let NodeData::Text(value) = &mut nodes[previous].data {
                        if let Err(error) = self.memory.borrow_mut().charge(text.len()) {
                            *self.error.borrow_mut() = Some(error);
                        } else {
                            value.push_str(&text);
                        }
                        return;
                    }
                }
                let child = self.add(NodeData::Text(text.to_string()));
                self.insert(parent, child, before);
            }
        }
    }
}

impl TreeSink for Dom {
    type Handle = usize;
    type Output = Self;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Self {
        self
    }
    fn parse_error(&self, _: Cow<'static, str>) {
        if self.error.borrow().is_some() {
            return;
        }
        self.parse_errors.set(self.parse_errors.get().saturating_add(1));
    }
    fn get_document(&self) -> usize {
        0
    }
    fn elem_name<'a>(&'a self, target: &'a usize) -> Self::ElemName<'a> {
        Ref::map(self.nodes.borrow(), |nodes| match &nodes[*target].data {
            NodeData::Element { name, .. } => name,
            _ => unreachable!("html5ever requested a name for a non-element handle"),
        })
    }
    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> usize {
        let template = flags.template.then(|| self.add(NodeData::Document));
        self.add(NodeData::Element { name, attrs, template })
    }
    fn create_comment(&self, _: StrTendril) -> usize {
        self.add(NodeData::Other)
    }
    fn create_pi(&self, _: StrTendril, _: StrTendril) -> usize {
        self.add(NodeData::Other)
    }
    fn append(&self, parent: &usize, child: NodeOrText<usize>) {
        self.append_item(*parent, child, None);
    }
    fn append_before_sibling(&self, sibling: &usize, child: NodeOrText<usize>) {
        let parent = self.nodes.borrow().get(*sibling).and_then(|node| node.parent).unwrap_or(0);
        self.append_item(parent, child, Some(*sibling));
    }
    fn append_based_on_parent_node(
        &self,
        element: &usize,
        previous: &usize,
        child: NodeOrText<usize>,
    ) {
        if self.nodes.borrow().get(*element).and_then(|node| node.parent).is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(previous, child);
        }
    }
    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {
        let _ = self.event();
    }
    fn get_template_contents(&self, target: &usize) -> usize {
        match &self.nodes.borrow()[*target].data {
            NodeData::Element { template: Some(id), .. } => *id,
            _ => 1,
        }
    }
    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }
    fn set_quirks_mode(&self, _: QuirksMode) {}
    fn add_attrs_if_missing(&self, target: &usize, attrs: Vec<Attribute>) {
        if self.error.borrow().is_some() {
            return;
        }
        let logical =
            attrs.iter().map(|attr| attr.name.local.len().saturating_add(attr.value.len())).sum();
        if let Err(error) = self.memory.borrow_mut().charge(logical) {
            *self.error.borrow_mut() = Some(error);
            return;
        }
        let mut nodes = self.nodes.borrow_mut();
        if let NodeData::Element { attrs: existing, .. } = &mut nodes[*target].data {
            let names = existing.iter().map(|attr| attr.name.clone()).collect::<BTreeSet<_>>();
            existing.extend(attrs.into_iter().filter(|attr| !names.contains(&attr.name)));
        }
    }
    fn remove_from_parent(&self, target: &usize) {
        if self.error.borrow().is_some() {
            return;
        }
        self.detach(*target);
    }
    fn reparent_children(&self, node: &usize, new_parent: &usize) {
        if self.error.borrow().is_some() {
            return;
        }
        let children = self.nodes.borrow()[*node].children.clone();
        for child in children {
            self.insert(*new_parent, child, None);
        }
    }
    fn is_mathml_annotation_xml_integration_point(&self, _: &usize) -> bool {
        false
    }
}

fn convert_html(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let input_size = u64::try_from(input.bytes.len()).unwrap_or(u64::MAX);
    if input_size > options.limits.max_input_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: format!("{input_size} > {}", options.limits.max_input_bytes),
        });
    }
    let (charset, charset_diagnostics) = html_charset(input, options);
    let (mut decoded, mut diagnostics) =
        decode_source(&input.bytes, charset.as_deref(), options.text.decoding_mode, context)?;
    diagnostics.extend(charset_diagnostics);

    // This reservation represents cooperative parser work, not html5ever's allocator or RSS.
    let event_units = decoded.text.len().saturating_mul(4).min(MAX_HTML_EVENTS);
    let parser_work = decoded
        .text
        .len()
        .saturating_mul(2)
        .saturating_add(event_units.saturating_mul(size_of::<usize>()));
    decoded.memory.charge(parser_work)?;

    let sink = Dom::new(options, context)?;
    let parse_options = ParseOpts {
        tree_builder: html5ever::tree_builder::TreeBuilderOpts {
            scripting_enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let dom = parse_document(sink, parse_options).one(decoded.text.as_str());
    if let Some(error) = dom.error.into_inner() {
        return Err(error);
    }
    if dom.parse_errors.get() > 0 {
        diagnostics.push(warning(
            "html.parseRecovered",
            format!("HTML5 parser recovered from {} syntax error(s)", dom.parse_errors.get()),
        ));
    }
    diagnostics.push(warning(
        "html.sourceLocationUnavailable",
        "HTML5 tree construction can synthesize or reparent nodes; ambiguous DOM nodes intentionally have no fabricated byte span".into(),
    ));

    let nodes = dom.nodes.into_inner();
    let builder = Builder::new(&nodes, input, decoded, options, context, diagnostics);
    builder.extract()
}

fn html_charset(
    input: &ResolvedInput,
    options: &ConversionOptions,
) -> (Option<String>, Vec<Diagnostic>) {
    let explicit = options
        .text
        .charset
        .clone()
        .or_else(|| input.metadata.media_type.as_deref().and_then(media_type_charset));
    if let Some(explicit) = explicit {
        let mut diagnostics = Vec::new();
        if let Some(meta) = prescan_meta_charset(&input.bytes)
            && !meta.eq_ignore_ascii_case(&explicit)
        {
            diagnostics.push(warning(
                "html.metaCharsetIgnored",
                format!("meta charset {meta} conflicts with explicit charset {explicit}"),
            ));
        }
        return (Some(explicit), diagnostics);
    }
    (prescan_meta_charset(&input.bytes), Vec::new())
}

fn media_type_charset(value: &str) -> Option<String> {
    value
        .split(';')
        .skip(1)
        .find_map(|parameter| {
            let (name, value) = parameter.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches(['\'', '"']).to_string())
        })
        .filter(|value| !value.is_empty())
}

fn prescan_meta_charset(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf])
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xfe, 0xff])
    {
        return None;
    }
    let sample = bytes.get(..bytes.len().min(META_PRESCAN_BYTES))?;
    if !sample
        .iter()
        .all(|byte| *byte == b'\t' || *byte == b'\n' || *byte == b'\r' || *byte >= 0x20)
    {
        return None;
    }
    let lower = sample.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    let mut offset = 0;
    while let Some(relative) = lower.get(offset..)?.windows(5).position(|window| window == b"<meta")
    {
        let start = offset + relative;
        let end = lower.get(start..)?.iter().position(|byte| *byte == b'>').map(|n| start + n)?;
        let tag = lower.get(start..=end)?;
        if let Some(value) = meta_attribute(tag, b"charset")
            .or_else(|| {
                meta_attribute(tag, b"http-equiv")
                    .filter(|value| value.eq_ignore_ascii_case(b"content-type"))?;
                let content = meta_attribute(tag, b"content")?;
                let position = content.windows(8).position(|window| window == b"charset=")?;
                let value = content.get(position + 8..)?.trim_ascii_start();
                Some(
                    &value[..value
                        .iter()
                        .position(|byte| byte.is_ascii_whitespace() || *byte == b';')
                        .unwrap_or(value.len())],
                )
            })
            .filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii))
        {
            return String::from_utf8(value.to_vec()).ok();
        }
        offset = end.saturating_add(1);
    }
    None
}

fn meta_attribute<'a>(tag: &'a [u8], wanted: &[u8]) -> Option<&'a [u8]> {
    let mut offset = 5;
    while offset < tag.len() {
        while tag.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if matches!(tag.get(offset), None | Some(b'>' | b'/')) {
            break;
        }
        let name_start = offset;
        while tag.get(offset).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-') {
            offset += 1;
        }
        let name = tag.get(name_start..offset)?;
        while tag.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if tag.get(offset) != Some(&b'=') {
            continue;
        }
        offset += 1;
        while tag.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        let value = if matches!(tag.get(offset), Some(b'\'' | b'\"')) {
            let quote = *tag.get(offset)?;
            offset += 1;
            let start = offset;
            offset += tag.get(offset..)?.iter().position(|byte| *byte == quote)?;
            let value = tag.get(start..offset)?;
            offset += 1;
            value
        } else {
            let start = offset;
            while tag
                .get(offset)
                .is_some_and(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'>' | b'/'))
            {
                offset += 1;
            }
            tag.get(start..offset)?
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Some(value);
        }
    }
    None
}

struct Builder<'a> {
    nodes: &'a [DomNode],
    input: &'a ResolvedInput,
    _decoded: DecodedText,
    context: &'a ExecutionContext,
    diagnostics: Vec<Diagnostic>,
    blocks: Vec<BlockNode>,
    assets: Vec<Asset>,
    metadata: DocumentMetadata,
    next_node: usize,
    base: Option<Url>,
    inline_count: usize,
    max_table_rows: u64,
    max_table_columns: u64,
    max_table_cells: u64,
}

impl<'a> Builder<'a> {
    fn new(
        nodes: &'a [DomNode],
        input: &'a ResolvedInput,
        decoded: DecodedText,
        options: &ConversionOptions,
        context: &'a ExecutionContext,
        diagnostics: Vec<Diagnostic>,
    ) -> Self {
        Self {
            nodes,
            input,
            _decoded: decoded,
            context,
            diagnostics,
            blocks: Vec::new(),
            assets: Vec::new(),
            metadata: DocumentMetadata::default(),
            next_node: 0,
            base: None,
            inline_count: 0,
            max_table_rows: options.limits.max_table_rows,
            max_table_columns: options.limits.max_table_columns,
            max_table_cells: options.limits.max_table_cells,
        }
    }

    fn extract(mut self) -> Result<ConverterOutput, ConversionError> {
        self.read_metadata();
        self.base = self.valid_base();
        let root = self.choose_main();
        self.emit_children(root, 0)?;
        if self.blocks.is_empty() {
            return Err(ConversionError::Malformed {
                part: Some("html".into()),
                detail: "HTML contains no visible document content".into(),
            });
        }
        let document =
            Document { metadata: self.metadata, blocks: self.blocks, ..Document::default() };
        Ok(ConverterOutput { document, assets: self.assets, diagnostics: self.diagnostics })
    }

    fn read_metadata(&mut self) {
        for id in 0..self.nodes.len() {
            match self.name(id) {
                Some("title") if self.metadata.title.is_none() => {
                    self.metadata.title = nonempty(self.text(id));
                }
                Some("html") => {
                    if let Some(lang) = self.attr(id, "lang") {
                        self.metadata.properties.insert("html.lang".into(), lang.into());
                    }
                }
                Some("meta") => {
                    let key = self.attr(id, "name").or_else(|| self.attr(id, "property"));
                    let value = self.attr(id, "content");
                    if let (Some(key), Some(value)) = (key, value) {
                        let key = key.to_ascii_lowercase();
                        if key == "author" {
                            self.metadata.authors.push(value.into());
                        } else if matches!(
                            key.as_str(),
                            "description" | "keywords" | "og:title" | "og:description"
                        ) {
                            self.metadata
                                .properties
                                .insert(format!("html.meta.{key}"), value.into());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn valid_base(&mut self) -> Option<Url> {
        let source = self.input.metadata.uri.as_deref().and_then(canonical_base_url);
        for id in 0..self.nodes.len() {
            if self.name(id) == Some("base")
                && let Some(href) = self.attr(id, "href")
            {
                let parsed = Url::parse(href).ok().or_else(|| source.as_ref()?.join(href).ok());
                if let Some(url) = parsed.and_then(valid_http_base) {
                    return Some(url);
                }
                self.diagnostics.push(warning(
                    "html.baseRejected",
                    "base URL is not a canonical public HTTP(S) reference".into(),
                ));
                return source;
            }
        }
        source
    }

    fn choose_main(&mut self) -> usize {
        let body = (0..self.nodes.len()).find(|id| self.name(*id) == Some("body")).unwrap_or(0);
        let explicit = (0..self.nodes.len())
            .filter(|id| {
                matches!(self.name(*id), Some("main" | "article"))
                    || self.attr(*id, "role").is_some_and(|v| v.eq_ignore_ascii_case("main"))
            })
            .filter(|id| !self.hidden(*id))
            .collect::<Vec<_>>();
        let selected = explicit.into_iter().max_by_key(|id| (self.score(*id), usize::MAX - *id));
        if let Some(id) = selected.filter(|id| self.visible_text_len(*id) > 0) {
            return id;
        }
        self.diagnostics.push(warning(
            "html.mainContentFallback",
            "no non-empty explicit main-content region; used visible body content".into(),
        ));
        body
    }

    // Fixed scoring constants are intentionally simple and covered by golden tests.
    fn score(&self, id: usize) -> i64 {
        let text = i64::try_from(self.visible_text_len(id)).unwrap_or(i64::MAX);
        let links = i64::try_from(self.link_text_len(id)).unwrap_or(i64::MAX);
        let paragraphs = i64::try_from(self.descendants_named(id, &["p"]) * 80).unwrap_or(i64::MAX);
        let headings = i64::try_from(self.descendants_named(id, &["h1", "h2", "h3"]) * 120)
            .unwrap_or(i64::MAX);
        text.saturating_add(paragraphs)
            .saturating_add(headings)
            .saturating_sub(links.saturating_mul(2))
    }

    fn emit_children(&mut self, id: usize, depth: usize) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        if depth > usize::from(u16::MAX) {
            return Self::limit("html_nesting_depth", "semantic extraction depth overflowed");
        }
        let children = self.nodes.get(id).map(|node| node.children.clone()).unwrap_or_default();
        for child in children {
            self.emit(child, depth + 1)?;
        }
        Ok(())
    }

    fn emit(&mut self, id: usize, depth: usize) -> Result<(), ConversionError> {
        if self.hidden(id) || self.boilerplate(id) {
            return Ok(());
        }
        let Some(name) = self.name(id).map(str::to_owned) else {
            return Ok(());
        };
        match name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name.as_bytes()[1] - b'0';
                let inline = self.inlines(id, Vec::new())?;
                if !inline.is_empty() {
                    self.push(Block::Heading { level, content: inline })?;
                }
            }
            "p" | "div" | "section" | "article" | "main" | "address" | "figcaption" => {
                if self.has_block_children(id) {
                    self.emit_children(id, depth)?;
                } else {
                    let inline = self.inlines(id, Vec::new())?;
                    if !inline.is_empty() {
                        self.push(Block::Paragraph(inline))?;
                    }
                }
            }
            "ul" | "ol" => self.emit_list(id, name == "ol")?,
            "table" => self.emit_table(id)?,
            "pre" => {
                let language =
                    self.first_descendant(id, "code").and_then(|code| self.code_language(code));
                let text = self.raw_text(id);
                if !text.is_empty() {
                    self.push(Block::Code { language, text })?;
                }
            }
            "img" => self.emit_image(id)?,
            "hr" => self.push(Block::Rule)?,
            "svg" | "math" => {
                let text = normalize(&self.text(id));
                self.diagnostics.push(warning(
                    "html.activeForeignContentOmitted",
                    format!("{name} content was not traversed as HTML resources"),
                ));
                if !text.is_empty() {
                    self.push(Block::Code { language: Some(name), text })?;
                }
            }
            "li" | "tr" | "td" | "th" | "code" => {}
            _ => self.emit_children(id, depth)?,
        }
        Ok(())
    }

    fn emit_list(&mut self, id: usize, ordered: bool) -> Result<(), ConversionError> {
        let mut items = Vec::new();
        for child in self.element_descendants_direct(id, "li") {
            let content = self.inlines(child, Vec::new())?;
            if !content.is_empty() {
                let node = self.make_node(Block::Paragraph(content));
                items.push(ListItem { checked: None, marker_label: None, blocks: vec![node] });
            }
        }
        if !items.is_empty() {
            let start = self.attr(id, "start").and_then(|v| v.parse().ok()).unwrap_or(1);
            self.push(Block::List {
                kind: if ordered { ListKind::Ordered } else { ListKind::Bullet },
                start,
                items,
            })?;
        }
        Ok(())
    }

    fn emit_table(&mut self, id: usize) -> Result<(), ConversionError> {
        let row_ids = self
            .descendants(id)
            .into_iter()
            .filter(|id| self.name(*id) == Some("tr"))
            .collect::<Vec<_>>();
        if u64::try_from(row_ids.len()).unwrap_or(u64::MAX) > self.max_table_rows {
            return Self::limit("max_table_rows", "HTML table has too many rows");
        }
        let mut rows = Vec::new();
        let mut width = 0_u64;
        let mut total_cells = 0_u64;
        for row in row_ids {
            let mut cells = Vec::new();
            let mut row_width = 0_u64;
            let cell_ids = self.nodes[row]
                .children
                .iter()
                .copied()
                .filter(|id| matches!(self.name(*id), Some("td" | "th")))
                .collect::<Vec<_>>();
            for cell in cell_ids {
                let row_span = positive_span(self.attr(cell, "rowspan"));
                let column_span = positive_span(self.attr(cell, "colspan"));
                row_width = row_width.saturating_add(u64::from(column_span));
                total_cells = total_cells
                    .saturating_add(u64::from(row_span).saturating_mul(u64::from(column_span)));
                let content = self.inlines(cell, Vec::new())?;
                let blocks = if content.is_empty() {
                    Vec::new()
                } else {
                    vec![self.make_node(Block::Paragraph(content))]
                };
                cells.push(Cell {
                    row_span,
                    column_span,
                    header: self.name(cell) == Some("th"),
                    blocks,
                });
            }
            width = width.max(row_width);
            if !cells.is_empty() {
                rows.push(TableRow { cells });
            }
        }
        if width > self.max_table_columns {
            return Self::limit("max_table_columns", "HTML table is too wide");
        }
        if total_cells > self.max_table_cells {
            return Self::limit("max_table_cells", "HTML table has too many logical cells");
        }
        if !rows.is_empty() {
            self.push(Block::Table { rows, alignments: Vec::<TableAlignment>::new() })?;
        }
        Ok(())
    }

    fn emit_image(&mut self, id: usize) -> Result<(), ConversionError> {
        let alt = self.attr(id, "alt").map(normalize).filter(|v| !v.is_empty());
        let Some(src) = self.attr(id, "src") else {
            return Ok(());
        };
        let resolved = Url::parse(src).ok().or_else(|| self.base.as_ref()?.join(src).ok());
        let Some(uri) = resolved
            .map(|url| url.to_string())
            .filter(|uri| canonical_external_asset_uri(uri).as_deref() == Some(uri.as_ref()))
        else {
            self.diagnostics.push(warning(
                "html.imageUriRejected",
                "image URI was retained only as alternative text; no network access occurred"
                    .into(),
            ));
            if let Some(alt) = alt {
                self.push(Block::Paragraph(vec![Inline::Text { value: alt, marks: Vec::new() }]))?;
            }
            return Ok(());
        };
        let asset_id = AssetId(format!("html-external-image-{:06}", self.assets.len() + 1));
        self.assets.push(Asset {
            id: asset_id.clone(),
            filename: None,
            media_type: image_media_type(&uri).into(),
            bytes: Vec::new(),
            external_uri: Some(uri),
        });
        self.push(Block::Image { asset: asset_id, alt })
    }

    fn inlines(
        &mut self,
        id: usize,
        mut marks: Vec<InlineMark>,
    ) -> Result<Vec<Inline>, ConversionError> {
        let mut output = Vec::new();
        if self.hidden(id) {
            return Ok(output);
        }
        if let Some(name) = self.name(id) {
            match name {
                "strong" | "b" => marks.push(InlineMark::Bold),
                "em" | "i" => marks.push(InlineMark::Italic),
                "del" | "s" | "strike" => marks.push(InlineMark::Strikethrough),
                "u" => marks.push(InlineMark::Underline),
                "sup" => marks.push(InlineMark::Superscript),
                "sub" => marks.push(InlineMark::Subscript),
                "br" => return Ok(vec![Inline::LineBreak]),
                "code" => {
                    return Ok(nonempty(self.raw_text(id))
                        .map(|value| vec![Inline::Code(value)])
                        .unwrap_or_default());
                }
                "svg" | "math" | "script" | "style" | "template" | "noscript" | "img" => {
                    return Ok(output);
                }
                _ => {}
            }
        }
        for child in self.nodes[id].children.clone() {
            match &self.nodes[child].data {
                NodeData::Text(value) => {
                    let value = normalize(value);
                    if !value.is_empty() {
                        output.push(Inline::Text { value, marks: marks.clone() });
                    }
                }
                NodeData::Element { .. } if self.name(child) == Some("a") => {
                    let content = self.inlines(child, marks.clone())?;
                    if let Some(href) = self.attr(child, "href") {
                        let target = Url::parse(href)
                            .ok()
                            .or_else(|| self.base.as_ref()?.join(href).ok())
                            .map_or_else(|| href.to_string(), |url| url.to_string());
                        if safe_link_target(&target) && !content.is_empty() {
                            output.push(Inline::Link { target, content });
                        } else if !content.is_empty() {
                            self.diagnostics.push(warning(
                                "html.linkUriRejected",
                                "unsafe link destination was omitted".into(),
                            ));
                            output.extend(content);
                        }
                    } else {
                        output.extend(content);
                    }
                }
                NodeData::Element { .. } => output.extend(self.inlines(child, marks.clone())?),
                _ => {}
            }
        }
        self.inline_count = self.inline_count.saturating_add(output.len());
        if self.inline_count > MAX_DOCUMENT_INLINES {
            return Self::limit("html_inlines", "HTML produced too many inline nodes");
        }
        Ok(output)
    }

    fn push(&mut self, block: Block) -> Result<(), ConversionError> {
        if self.blocks.len() >= MAX_DOCUMENT_NODES {
            return Self::limit("html_ir_nodes", "HTML produced too many IR nodes");
        }
        let node = self.make_node(block);
        self.blocks.push(node);
        Ok(())
    }
    fn make_node(&mut self, block: Block) -> BlockNode {
        self.next_node += 1;
        BlockNode {
            id: NodeId(format!("html-{:06}", self.next_node)),
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: SourceLocator {
                    byte_start: Some(0),
                    byte_end: u64::try_from(self.input.bytes.len()).ok(),
                    ..SourceLocator::default()
                },
                confidence: None,
            },
        }
    }
    fn name(&self, id: usize) -> Option<&str> {
        match &self.nodes.get(id)?.data {
            NodeData::Element { name, .. } => Some(name.local.as_ref()),
            _ => None,
        }
    }
    fn attr(&self, id: usize, name: &str) -> Option<&str> {
        match &self.nodes.get(id)?.data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .find(|a| a.name.local.as_ref().eq_ignore_ascii_case(name))
                .map(|a| a.value.as_ref()),
            _ => None,
        }
    }
    fn hidden(&self, id: usize) -> bool {
        matches!(
            self.name(id),
            Some(
                "script"
                    | "style"
                    | "template"
                    | "noscript"
                    | "iframe"
                    | "object"
                    | "embed"
                    | "canvas"
                    | "input"
                    | "select"
                    | "textarea"
                    | "button"
            )
        ) || self.attr(id, "hidden").is_some()
            || self.attr(id, "inert").is_some()
            || self.attr(id, "aria-hidden").is_some_and(|v| v.eq_ignore_ascii_case("true"))
    }
    fn boilerplate(&self, id: usize) -> bool {
        if matches!(self.name(id), Some("nav" | "aside" | "footer")) {
            return true;
        }
        if self.attr(id, "role").is_some_and(|v| {
            matches!(
                v.to_ascii_lowercase().as_str(),
                "navigation" | "banner" | "contentinfo" | "complementary" | "dialog"
            )
        }) {
            return true;
        }
        let labels = format!(
            "{} {}",
            self.attr(id, "id").unwrap_or(""),
            self.attr(id, "class").unwrap_or("")
        )
        .to_ascii_lowercase();
        [
            " advert",
            " ad-",
            "-ad ",
            "cookie",
            "popup",
            "modal",
            "recommend",
            "related",
            "sidebar",
            "navigation",
        ]
        .iter()
        .any(|needle| labels.contains(needle))
    }
    fn text(&self, id: usize) -> String {
        normalize(&self.raw_text(id))
    }
    fn raw_text(&self, id: usize) -> String {
        let mut out = String::new();
        for child in &self.nodes[id].children {
            match &self.nodes[*child].data {
                NodeData::Text(value) => out.push_str(value),
                NodeData::Element { .. } if !self.hidden(*child) => {
                    out.push_str(&self.raw_text(*child));
                }
                _ => {}
            }
        }
        out
    }
    fn visible_text_len(&self, id: usize) -> usize {
        if self.hidden(id) || self.boilerplate(id) { 0 } else { self.text(id).len() }
    }
    fn link_text_len(&self, id: usize) -> usize {
        self.descendants(id)
            .into_iter()
            .filter(|id| self.name(*id) == Some("a"))
            .map(|id| self.text(id).len())
            .sum()
    }
    fn descendants_named(&self, id: usize, names: &[&str]) -> usize {
        self.descendants(id)
            .into_iter()
            .filter(|id| self.name(*id).is_some_and(|name| names.contains(&name)))
            .count()
    }
    fn descendants(&self, id: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = self.nodes[id].children.clone();
        while let Some(next) = stack.pop() {
            out.push(next);
            stack.extend(self.nodes[next].children.iter().rev());
        }
        out
    }
    fn first_descendant(&self, id: usize, name: &str) -> Option<usize> {
        self.descendants(id).into_iter().find(|id| self.name(*id) == Some(name))
    }
    fn element_descendants_direct(&self, id: usize, name: &str) -> Vec<usize> {
        self.nodes[id].children.iter().copied().filter(|id| self.name(*id) == Some(name)).collect()
    }
    fn has_block_children(&self, id: usize) -> bool {
        self.nodes[id].children.iter().any(|id| {
            matches!(
                self.name(*id),
                Some(
                    "p" | "div"
                        | "section"
                        | "article"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "ul"
                        | "ol"
                        | "table"
                        | "pre"
                        | "img"
                        | "hr"
                        | "svg"
                        | "math"
                )
            )
        })
    }
    fn code_language(&self, id: usize) -> Option<String> {
        self.attr(id, "class").and_then(|v| {
            v.split_ascii_whitespace()
                .find_map(|part| part.strip_prefix("language-").map(str::to_string))
        })
    }
    fn limit<T>(limit: &'static str, detail: &str) -> Result<T, ConversionError> {
        Err(ConversionError::ResourceLimit { limit, detail: detail.into() })
    }
}

fn warning(code: &str, message: String) -> Diagnostic {
    Diagnostic { code: code.into(), severity: DiagnosticSeverity::Warning, message, locator: None }
}
fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
fn positive_span(value: Option<&str>) -> u32 {
    value.and_then(|v| v.parse::<u32>().ok()).filter(|v| *v > 0).unwrap_or(1)
}
fn valid_http_base(mut url: Url) -> Option<Url> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || denied_base_host(url.host_str()?)
    {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url)
}
fn denied_base_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    let Ok(address) = host.parse::<std::net::IpAddr>() else { return false };
    match address {
        std::net::IpAddr::V4(value) => {
            value.is_private()
                || value.is_loopback()
                || value.is_link_local()
                || value.is_unspecified()
                || value.is_multicast()
        }
        std::net::IpAddr::V6(value) => {
            value.is_loopback()
                || value.is_unspecified()
                || value.is_multicast()
                || value.is_unique_local()
                || value.is_unicast_link_local()
        }
    }
}
fn canonical_base_url(value: &str) -> Option<Url> {
    valid_http_base(Url::parse(value).ok()?)
}
fn image_media_type(uri: &str) -> &'static str {
    let extension = Url::parse(uri)
        .ok()
        .and_then(|url| std::path::Path::new(url.path()).extension()?.to_str().map(str::to_owned));
    if extension.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("png")) {
        "image/png"
    } else if extension.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("gif")) {
        "image/gif"
    } else if extension.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("webp")) {
        "image/webp"
    } else if extension.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("svg")) {
        "image/svg+xml"
    } else {
        "image/jpeg"
    }
}
fn safe_link_target(value: &str) -> bool {
    if value.chars().any(char::is_control) || value.contains('&') {
        return false;
    }
    let Some(colon) = value.find(':') else { return true };
    let scheme = &value[..colon];
    scheme.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_alphabetic() || index > 0 && matches!(byte, b'+' | b'-' | b'.')
    }) && !matches!(
        scheme.to_ascii_lowercase().as_str(),
        "javascript" | "vbscript" | "data" | "file" | "blob"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits, SourceMetadata};
    use std::sync::Arc;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }
    fn convert(source: &str) -> ConverterOutput {
        convert_html(
            &ResolvedInput {
                bytes: Arc::from(source.as_bytes()),
                metadata: SourceMetadata::default(),
            },
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap()
    }

    #[test]
    fn extracts_semantics_and_omits_active_or_boilerplate_content() {
        let output = convert(
            "<!doctype html><title>T</title><nav>menu</nav><main><h1>Hello</h1><p>A <strong>safe</strong> body.</p><script>bad()</script></main>",
        );
        assert_eq!(output.document.metadata.title.as_deref(), Some("T"));
        let rendered = format!("{:?}", output.document.blocks);
        assert!(rendered.contains("Hello") && rendered.contains("safe"));
        assert!(!rendered.contains("menu") && !rendered.contains("bad"));
    }

    #[test]
    fn relative_external_image_is_audited_but_never_fetched() {
        let input = ResolvedInput {
            bytes: Arc::from(b"<main><img src='a.png' alt='A'></main>".as_slice()),
            metadata: SourceMetadata {
                uri: Some("https://example.invalid/docs/page.html".into()),
                ..SourceMetadata::default()
            },
        };
        let output = convert_html(&input, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(
            output.assets[0].external_uri.as_deref(),
            Some("https://example.invalid/docs/a.png")
        );
        assert!(output.assets[0].bytes.is_empty());
    }

    #[test]
    fn empty_or_multiple_main_uses_deterministic_nonempty_choice() {
        let output = convert("<main></main><main><p>chosen</p></main><nav><p>noise</p></nav>");
        assert!(format!("{:?}", output.document.blocks).contains("chosen"));
        assert!(!format!("{:?}", output.document.blocks).contains("noise"));
    }

    #[test]
    fn svg_descendants_do_not_become_assets() {
        let output = convert(
            "<main><svg><a href='https://e.invalid'><image href='https://e.invalid/a.png'/></a><text>x</text></svg><p>ok</p></main>",
        );
        assert!(output.assets.is_empty());
        assert!(output.diagnostics.iter().any(|d| d.code == "html.activeForeignContentOmitted"));
    }

    #[test]
    fn hidden_repeated_entity_and_implicit_nodes_are_safe() {
        let source = "<main><p>A &amp; B<p hidden>hidden<p aria-hidden=true>aria<p inert>inert<p>C";
        let output = convert(source);
        let rendered = format!("{:?}", output.document.blocks);
        assert!(rendered.contains("A & B") && rendered.contains('C'));
        assert!(
            !rendered.contains("hidden")
                && !rendered.contains("aria")
                && !rendered.contains("inert")
        );
        assert!(output.document.blocks.iter().all(|node| {
            node.provenance.locator.byte_start == Some(0)
                && node.provenance.locator.byte_end == u64::try_from(source.len()).ok()
        }));
    }

    #[test]
    fn unsafe_links_and_bases_are_data_not_authority() {
        let output = convert(
            "<base href='http://127.0.0.1/private/'><main><p><a href='javascript:bad()'>safe label</a></p><img src='x.png' alt='x'></main>",
        );
        assert!(output.assets.is_empty());
        assert!(output.diagnostics.iter().any(|d| d.code == "html.baseRejected"));
        assert!(output.diagnostics.iter().any(|d| d.code == "html.linkUriRejected"));
    }

    #[test]
    fn tables_with_spans_pass_core_grid_validation() {
        let output = convert(
            "<main><table><tr><th rowspan=2>A</th><th colspan=2>B</th></tr><tr><td>C</td><td>D</td></tr></table></main>",
        );
        output.document.validate().unwrap();
    }

    #[test]
    fn only_navigation_has_stable_empty_body_failure() {
        let error = convert_html(
            &ResolvedInput {
                bytes: Arc::from(b"<nav><p>menu only</p></nav>".as_slice()),
                metadata: SourceMetadata::default(),
            },
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap_err();
        assert!(
            matches!(error, ConversionError::Malformed { part: Some(part), .. } if part == "html")
        );
    }

    #[test]
    fn detector_evidence_does_not_capture_xml_text_or_markdown() {
        assert!(crate::html_document_evidence(b"<!doctype html><html><body>x</body></html>"));
        assert!(crate::html_document_evidence(b"<article><h1>x</h1><p>y</p></article>"));
        assert!(!crate::html_document_evidence(b"<?xml version='1.0'?><rss><item>x</item></rss>"));
        assert!(!crate::html_document_evidence(b"ordinary <x> text"));
        assert!(!crate::html_document_evidence(b"# Markdown\n\n<div>raw</div>"));
        let candidate =
            crate::structured_text_candidate(b"<article><h1>x</h1><p>y</p></article>", &context())
                .unwrap()
                .unwrap();
        assert_eq!(candidate.format, InputFormat::Html);
    }

    #[test]
    fn explicit_charset_wins_over_meta_with_diagnostic() {
        let input = ResolvedInput {
            bytes: Arc::from(
                b"<meta http-equiv='content-type' content='text/html; charset=windows-1252'><main><p>safe</p></main>".as_slice(),
            ),
            metadata: SourceMetadata::default(),
        };
        let mut options = ConversionOptions::default();
        options.text.charset = Some("utf-8".into());
        let output = convert_html(&input, &options, &context()).unwrap();
        assert!(output.diagnostics.iter().any(|d| d.code == "html.metaCharsetIgnored"));
    }
}
