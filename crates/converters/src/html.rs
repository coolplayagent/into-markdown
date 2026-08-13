//! Offline HTML5 parsing and deterministic semantic extraction.

use html5ever::interface::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, ParseOpts, QualName, parse_document};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, DocumentMetadata,
    ExecutionContext, FormatCandidate, Inline, InlineMark, InputFormat, IrErrorCode, ListItem,
    ListKind, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS, NodeId, ProbeOutcome,
    Provenance, ProvenanceKind, ResolvedInput, Services, SourceLocator, TableAlignment, TableRow,
    canonical_external_asset_uri,
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
            let evidence =
                match super::bounded_utf8_prefix(&input.bytes, super::TEXT_INSPECTION_BYTE_LIMIT) {
                    Some((text, _)) => super::html_document_evidence(text, context)?,
                    None => false,
                };
            Ok(
                if candidate.explicit
                    || candidate.detector_id == "builtin.detector.hints"
                    || evidence
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
        if self.poisoned() {
            return false;
        }
        let next = self.events.get().saturating_add(1);
        self.events.set(next);
        if next > MAX_HTML_EVENTS {
            self.set_error_once(ConversionError::ResourceLimit {
                limit: "html_events",
                detail: format!("HTML parser exceeded {MAX_HTML_EVENTS} tree events"),
            });
            return false;
        }
        if next.is_multiple_of(CHECKPOINT_EVENTS)
            && let Err(error) = self.context.checkpoint()
        {
            self.set_error_once(error);
            return false;
        }
        true
    }

    fn poisoned(&self) -> bool {
        self.error.borrow().is_some()
    }

    fn set_error_once(&self, error: ConversionError) {
        let mut first = self.error.borrow_mut();
        if first.is_none() {
            *first = Some(error);
        }
    }

    fn add(&self, data: NodeData) -> usize {
        if !self.event() {
            return 1;
        }
        let mut nodes = self.nodes.borrow_mut();
        if nodes.len() >= MAX_DOCUMENT_NODES {
            self.set_error_once(ConversionError::ResourceLimit {
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
            self.set_error_once(error);
            return 1;
        }
        let id = nodes.len();
        nodes.push(DomNode { parent: None, children: Vec::new(), depth: 0, data });
        id
    }

    fn detach(&self, child: usize) {
        if self.poisoned() {
            return;
        }
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
        if self.poisoned() {
            return;
        }
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
            self.set_error_once(ConversionError::ResourceLimit {
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
        if self.poisoned() {
            return;
        }
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
                            self.set_error_once(error);
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
        if self.poisoned() {
            return;
        }
        self.parse_errors.set(self.parse_errors.get().saturating_add(1));
    }
    fn get_document(&self) -> usize {
        if self.poisoned() {
            return 0;
        }
        0
    }
    fn elem_name<'a>(&'a self, target: &'a usize) -> Self::ElemName<'a> {
        if self.poisoned() {
            return Ref::map(self.nodes.borrow(), |nodes| match &nodes[1].data {
                NodeData::Element { name, .. } => name,
                _ => unreachable!("sentinel is always an element"),
            });
        }
        Ref::map(self.nodes.borrow(), |nodes| match &nodes[*target].data {
            NodeData::Element { name, .. } => name,
            _ => unreachable!("html5ever requested a name for a non-element handle"),
        })
    }
    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> usize {
        if self.poisoned() {
            return 1;
        }
        let template = flags.template.then(|| self.add(NodeData::Document));
        if self.poisoned() {
            return 1;
        }
        let element = self.add(NodeData::Element { name, attrs, template });
        if self.poisoned() {
            return 1;
        }
        if let Some(template) = template {
            self.nodes.borrow_mut()[template].parent = Some(element);
        }
        element
    }
    fn create_comment(&self, _: StrTendril) -> usize {
        if self.poisoned() {
            return 1;
        }
        self.add(NodeData::Other)
    }
    fn create_pi(&self, _: StrTendril, _: StrTendril) -> usize {
        if self.poisoned() {
            return 1;
        }
        self.add(NodeData::Other)
    }
    fn append(&self, parent: &usize, child: NodeOrText<usize>) {
        if self.poisoned() {
            return;
        }
        self.append_item(*parent, child, None);
    }
    fn append_before_sibling(&self, sibling: &usize, child: NodeOrText<usize>) {
        if self.poisoned() {
            return;
        }
        let parent = self.nodes.borrow().get(*sibling).and_then(|node| node.parent).unwrap_or(0);
        self.append_item(parent, child, Some(*sibling));
    }
    fn append_based_on_parent_node(
        &self,
        element: &usize,
        previous: &usize,
        child: NodeOrText<usize>,
    ) {
        if self.poisoned() {
            return;
        }
        if self.nodes.borrow().get(*element).and_then(|node| node.parent).is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(previous, child);
        }
    }
    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {
        if self.poisoned() {
            return;
        }
        let _ = self.event();
    }
    fn mark_script_already_started(&self, _: &usize) {
        let _ = self.poisoned();
    }
    fn pop(&self, _: &usize) {
        let _ = self.poisoned();
    }
    fn get_template_contents(&self, target: &usize) -> usize {
        if self.poisoned() {
            return 1;
        }
        match &self.nodes.borrow()[*target].data {
            NodeData::Element { template: Some(id), .. } => *id,
            _ => 1,
        }
    }
    fn same_node(&self, x: &usize, y: &usize) -> bool {
        if self.poisoned() {
            return false;
        }
        x == y
    }
    fn set_quirks_mode(&self, _: QuirksMode) {
        let _ = self.poisoned();
    }
    fn add_attrs_if_missing(&self, target: &usize, attrs: Vec<Attribute>) {
        if self.poisoned() {
            return;
        }
        let logical =
            attrs.iter().map(|attr| attr.name.local.len().saturating_add(attr.value.len())).sum();
        if let Err(error) = self.memory.borrow_mut().charge(logical) {
            self.set_error_once(error);
            return;
        }
        let mut nodes = self.nodes.borrow_mut();
        if let NodeData::Element { attrs: existing, .. } = &mut nodes[*target].data {
            let names = existing.iter().map(|attr| attr.name.clone()).collect::<BTreeSet<_>>();
            existing.extend(attrs.into_iter().filter(|attr| !names.contains(&attr.name)));
        }
    }
    fn associate_with_form(&self, _: &usize, _: &usize, _: (&usize, Option<&usize>)) {
        let _ = self.poisoned();
    }
    fn remove_from_parent(&self, target: &usize) {
        if self.poisoned() {
            return;
        }
        self.detach(*target);
    }
    fn reparent_children(&self, node: &usize, new_parent: &usize) {
        if self.poisoned() {
            return;
        }
        let children = self.nodes.borrow()[*node].children.clone();
        for child in children {
            self.insert(*new_parent, child, None);
        }
    }
    fn is_mathml_annotation_xml_integration_point(&self, _: &usize) -> bool {
        if self.poisoned() {
            return false;
        }
        false
    }
    fn set_current_line(&self, _: u64) {
        let _ = self.poisoned();
    }
    fn allow_declarative_shadow_roots(&self, _: &usize) -> bool {
        if self.poisoned() {
            return false;
        }
        false
    }
    fn attach_declarative_shadow(&self, _: &usize, _: &usize, _: &[Attribute]) -> bool {
        if self.poisoned() {
            return false;
        }
        false
    }
    fn maybe_clone_an_option_into_selectedcontent(&self, _: &usize) {
        let _ = self.poisoned();
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
    let (charset, charset_diagnostics) = html_charset(input, options, context)?;
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
    context: &ExecutionContext,
) -> Result<(Option<String>, Vec<Diagnostic>), ConversionError> {
    let explicit = options
        .text
        .charset
        .clone()
        .or_else(|| input.metadata.media_type.as_deref().and_then(media_type_charset));
    if let Some(explicit) = explicit {
        let mut diagnostics = Vec::new();
        if let Some(meta) = prescan_meta_charset(&input.bytes, context)?
            && !meta.eq_ignore_ascii_case(&explicit)
        {
            diagnostics.push(warning(
                "html.metaCharsetIgnored",
                format!("meta charset {meta} conflicts with explicit charset {explicit}"),
            ));
        }
        return Ok((Some(explicit), diagnostics));
    }
    Ok((prescan_meta_charset(&input.bytes, context)?, Vec::new()))
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

fn prescan_meta_charset(
    bytes: &[u8],
    context: &ExecutionContext,
) -> Result<Option<String>, ConversionError> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf])
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xfe, 0xff])
    {
        return Ok(None);
    }
    let Some(sample) = bytes.get(..bytes.len().min(META_PRESCAN_BYTES)) else {
        return Ok(None);
    };
    if !sample
        .iter()
        .all(|byte| *byte == b'\t' || *byte == b'\n' || *byte == b'\r' || *byte >= 0x20)
    {
        return Ok(None);
    }
    let mut offset = 0;
    let mut steps = 0_usize;
    while offset < sample.len() {
        steps = steps.saturating_add(1);
        if steps.is_multiple_of(128) {
            context.checkpoint()?;
        }
        if ascii_prefix_at(sample, offset, b"<!--") {
            offset = find_ascii(sample, offset.saturating_add(4), b"-->", context)?
                .map_or(sample.len(), |end| end.saturating_add(3));
            continue;
        }
        if sample.get(offset) != Some(&b'<') {
            offset += 1;
            continue;
        }
        let Some((name, end)) = scan_start_tag(sample, offset, context)? else {
            offset += 1;
            continue;
        };
        if name.eq_ignore_ascii_case(b"script") || name.eq_ignore_ascii_case(b"style") {
            offset = find_raw_text_end(sample, end, name, context)?;
            continue;
        }
        if name.eq_ignore_ascii_case(b"meta") {
            let Some(tag) = sample.get(offset..end) else {
                return Ok(None);
            };
            let direct = meta_attribute(tag, b"charset", context)?;
            let legacy = if meta_attribute(tag, b"http-equiv", context)?
                .is_some_and(|value| value.eq_ignore_ascii_case(b"content-type"))
            {
                meta_attribute(tag, b"content", context)?.and_then(extract_charset_from_content)
            } else {
                None
            };
            if let Some(value) = direct
                .or(legacy)
                .filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii))
            {
                return Ok(String::from_utf8(value.to_vec()).ok());
            }
        }
        offset = end.max(offset.saturating_add(1));
    }
    Ok(None)
}

fn meta_attribute<'a>(
    tag: &'a [u8],
    wanted: &[u8],
    context: &ExecutionContext,
) -> Result<Option<&'a [u8]>, ConversionError> {
    let mut offset = 5;
    while offset < tag.len() {
        if offset.is_multiple_of(128) {
            context.checkpoint()?;
        }
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
        if offset == name_start {
            offset += 1;
            continue;
        }
        let Some(name) = tag.get(name_start..offset) else { return Ok(None) };
        while tag.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if tag.get(offset) != Some(&b'=') {
            offset = offset.max(name_start.saturating_add(1));
            continue;
        }
        offset += 1;
        while tag.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        let value = if matches!(tag.get(offset), Some(b'\'' | b'\"')) {
            let Some(quote) = tag.get(offset).copied() else { return Ok(None) };
            offset += 1;
            let start = offset;
            let Some(rest) = tag.get(offset..) else { return Ok(None) };
            let Some(length) = rest.iter().position(|byte| *byte == quote) else {
                return Ok(None);
            };
            offset += length;
            let Some(value) = tag.get(start..offset) else { return Ok(None) };
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
            let Some(value) = tag.get(start..offset) else { return Ok(None) };
            value
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn scan_start_tag<'a>(
    bytes: &'a [u8],
    start: usize,
    context: &ExecutionContext,
) -> Result<Option<(&'a [u8], usize)>, ConversionError> {
    let mut offset = start.saturating_add(1);
    let name_start = offset;
    while bytes.get(offset).is_some_and(u8::is_ascii_alphabetic) {
        offset += 1;
    }
    let Some(name) = bytes.get(name_start..offset).filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    if !bytes.get(offset).is_some_and(|byte| is_html_space(*byte) || matches!(byte, b'/' | b'>')) {
        return Ok(None);
    }
    let Some(end) = find_tag_end_checked(bytes, offset, context)? else { return Ok(None) };
    Ok(Some((name, end)))
}

fn find_tag_end_checked(
    bytes: &[u8],
    mut offset: usize,
    context: &ExecutionContext,
) -> Result<Option<usize>, ConversionError> {
    let mut quote = None;
    while let Some(byte) = bytes.get(offset).copied() {
        if offset.is_multiple_of(128) {
            context.checkpoint()?;
        }
        match (quote, byte) {
            (Some(active), value) if active == value => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Ok(Some(offset.saturating_add(1))),
            _ => {}
        }
        offset += 1;
    }
    Ok(None)
}

fn find_raw_text_end(
    bytes: &[u8],
    mut offset: usize,
    name: &[u8],
    context: &ExecutionContext,
) -> Result<usize, ConversionError> {
    while offset < bytes.len() {
        if offset.is_multiple_of(128) {
            context.checkpoint()?;
        }
        if bytes.get(offset..offset.saturating_add(2)) == Some(b"</") {
            let name_start = offset.saturating_add(2);
            let name_end = name_start.saturating_add(name.len());
            if bytes
                .get(name_start..name_end)
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
                && bytes
                    .get(name_end)
                    .is_some_and(|byte| is_html_space(*byte) || matches!(byte, b'/' | b'>'))
                && let Some(end) = find_tag_end_checked(bytes, name_end, context)?
            {
                return Ok(end);
            }
        }
        offset += 1;
    }
    Ok(bytes.len())
}

fn find_ascii(
    bytes: &[u8],
    mut offset: usize,
    needle: &[u8],
    context: &ExecutionContext,
) -> Result<Option<usize>, ConversionError> {
    while offset.saturating_add(needle.len()) <= bytes.len() {
        if offset.is_multiple_of(128) {
            context.checkpoint()?;
        }
        if ascii_prefix_at(bytes, offset, needle) {
            return Ok(Some(offset));
        }
        offset += 1;
    }
    Ok(None)
}

fn ascii_prefix_at(bytes: &[u8], offset: usize, needle: &[u8]) -> bool {
    bytes
        .get(offset..offset.saturating_add(needle.len()))
        .is_some_and(|value| value.eq_ignore_ascii_case(needle))
}

fn extract_charset_from_content(content: &[u8]) -> Option<&[u8]> {
    let mut offset = 0_usize;
    while offset.saturating_add(7) <= content.len() {
        if ascii_prefix_at(content, offset, b"charset") {
            let mut value = offset.saturating_add(7);
            while content.get(value).is_some_and(|byte| is_html_space(*byte)) {
                value += 1;
            }
            if content.get(value) != Some(&b'=') {
                offset += 1;
                continue;
            }
            value += 1;
            while content.get(value).is_some_and(|byte| is_html_space(*byte)) {
                value += 1;
            }
            let end = content
                .get(value..)?
                .iter()
                .position(|byte| is_html_space(*byte) || *byte == b';')
                .map_or(content.len(), |length| value.saturating_add(length));
            return content.get(value..end);
        }
        offset += 1;
    }
    None
}

fn is_html_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0c)
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

#[derive(Clone, Copy, Default)]
struct NodeContext(u8);

impl NodeContext {
    const HIDDEN: u8 = 1;
    const BOILERPLATE: u8 = 1 << 1;
    const FOREIGN: u8 = 1 << 2;
    const TEMPLATE: u8 = 1 << 3;
    const HEAD: u8 = 1 << 4;

    fn mark(&mut self, flag: u8, value: bool) {
        if value {
            self.0 |= flag;
        }
    }

    const fn excluded(self) -> bool {
        self.0 & (Self::HIDDEN | Self::BOILERPLATE | Self::FOREIGN | Self::TEMPLATE) != 0
    }

    const fn in_head(self) -> bool {
        self.0 & Self::HEAD != 0
    }
}

struct PlannedTableCell {
    node: usize,
    column: usize,
    row_span: u32,
    column_span: u32,
}

struct PlannedTable {
    rows: Vec<Vec<PlannedTableCell>>,
    width: usize,
}

#[derive(Clone, Copy)]
struct SourceTableRow {
    node: usize,
    group: usize,
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
        self.blocks = self.collect_child_blocks(root, 0)?;
        if self.blocks.is_empty() {
            return Err(ConversionError::Malformed {
                part: Some("html".into()),
                detail: "HTML contains no visible document content".into(),
            });
        }
        let document =
            Document { metadata: self.metadata, blocks: self.blocks, ..Document::default() };
        document.validate().map_err(|error| {
            let detail = format!("parsed IR invalid at {}: {}", error.path, error.detail);
            if error.code == IrErrorCode::ResourceLimit {
                ConversionError::ResourceLimit { limit: "html_ir", detail }
            } else {
                ConversionError::Malformed { part: Some("html".into()), detail }
            }
        })?;
        Ok(ConverterOutput { document, assets: self.assets, diagnostics: self.diagnostics })
    }

    fn read_metadata(&mut self) {
        for id in 0..self.nodes.len() {
            let context = self.node_context(id);
            if context.excluded() || !context.in_head() || !self.is_html_element(id) {
                continue;
            }
            match self.name(id) {
                Some("title") if self.metadata.title.is_none() => {
                    self.metadata.title = nonempty(self.visible_text(id));
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
        if let Some(html) = (0..self.nodes.len())
            .find(|id| self.is_html_element(*id) && self.name(*id) == Some("html"))
            && !self.node_context(html).excluded()
            && let Some(lang) = self.attr(html, "lang")
        {
            self.metadata.properties.insert("html.lang".into(), lang.into());
        }
    }

    fn valid_base(&mut self) -> Option<Url> {
        let source = self.input.metadata.uri.as_deref().and_then(canonical_base_url);
        for id in 0..self.nodes.len() {
            let context = self.node_context(id);
            if self.is_html_element(id)
                && context.in_head()
                && !context.excluded()
                && self.name(id) == Some("base")
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
            }
        }
        source
    }

    fn choose_main(&mut self) -> usize {
        let body = (0..self.nodes.len()).find(|id| self.name(*id) == Some("body")).unwrap_or(0);
        let explicit = (0..self.nodes.len())
            .filter(|id| {
                self.is_html_element(*id)
                    && (matches!(self.name(*id), Some("main" | "article"))
                        || self.attr(*id, "role").is_some_and(|v| v.eq_ignore_ascii_case("main")))
            })
            .filter(|id| !self.node_context(*id).excluded())
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

    fn collect_child_blocks(
        &mut self,
        id: usize,
        depth: usize,
    ) -> Result<Vec<BlockNode>, ConversionError> {
        self.context.checkpoint()?;
        if depth > usize::from(u16::MAX) {
            return Self::limit("html_nesting_depth", "semantic extraction depth overflowed");
        }
        let children = self.nodes.get(id).map(|node| node.children.clone()).unwrap_or_default();
        let mut blocks = Vec::new();
        let mut inline = Vec::new();
        for child in children {
            if self.node_context(child).excluded() && !self.is_foreign_root(child) {
                continue;
            }
            if self.is_block_node(child) {
                self.flush_paragraph(&mut blocks, &mut inline);
                blocks.extend(self.build_block(child, depth.saturating_add(1))?);
            } else {
                inline.extend(self.inline_node(child, Vec::new())?);
            }
        }
        self.flush_paragraph(&mut blocks, &mut inline);
        Ok(blocks)
    }

    fn flush_paragraph(&mut self, blocks: &mut Vec<BlockNode>, inline: &mut Vec<Inline>) {
        if inline.is_empty() {
            return;
        }
        blocks.push(self.make_node(Block::Paragraph(std::mem::take(inline))));
    }

    fn build_block(&mut self, id: usize, depth: usize) -> Result<Vec<BlockNode>, ConversionError> {
        if self.node_context(id).excluded() && !self.is_foreign_root(id) {
            return Ok(Vec::new());
        }
        let Some(name) = self.name(id).map(str::to_owned) else {
            return Ok(Vec::new());
        };
        let block = match name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let content = self.inline_children(id)?;
                (!content.is_empty()).then(|| {
                    self.make_node(Block::Heading { level: name.as_bytes()[1] - b'0', content })
                })
            }
            "p" | "address" | "figcaption" => {
                let content = self.inline_children(id)?;
                (!content.is_empty()).then(|| self.make_node(Block::Paragraph(content)))
            }
            "div" | "section" | "article" | "main" | "body" | "html" => {
                return self.collect_child_blocks(id, depth);
            }
            "ul" | "ol" => {
                self.build_list(id, name == "ol", depth)?.map(|value| self.make_node(value))
            }
            "table" => self.build_table(id, depth)?.map(|value| self.make_node(value)),
            "pre" => {
                let language = self
                    .first_visible_descendant(id, "code")
                    .and_then(|code| self.code_language(code));
                let text = self.raw_visible_text(id);
                (!text.is_empty()).then(|| self.make_node(Block::Code { language, text }))
            }
            "img" => self.build_image(id).map(|value| self.make_node(value)),
            "hr" => Some(self.make_node(Block::Rule)),
            "svg" | "math" => {
                let text = normalize(&self.raw_text_unfiltered(id));
                self.diagnostics.push(warning(
                    "html.activeForeignContentOmitted",
                    format!("{name} content was not traversed as HTML resources"),
                ));
                (!text.is_empty())
                    .then(|| self.make_node(Block::Code { language: Some(name), text }))
            }
            "li" | "tr" | "td" | "th" | "code" | "head" => None,
            _ => return self.collect_child_blocks(id, depth),
        };
        Ok(block.into_iter().collect())
    }

    fn build_list(
        &mut self,
        id: usize,
        ordered: bool,
        depth: usize,
    ) -> Result<Option<Block>, ConversionError> {
        let mut items = Vec::new();
        let children = self.nodes[id].children.clone();
        for child in children {
            if !self.is_html_element(child)
                || self.name(child) != Some("li")
                || self.node_context(child).excluded()
            {
                continue;
            }
            let blocks = self.collect_child_blocks(child, depth.saturating_add(1))?;
            if !blocks.is_empty() {
                items.push(ListItem { checked: None, marker_label: None, blocks });
            }
        }
        if items.is_empty() {
            return Ok(None);
        }
        let start = self.attr(id, "start").and_then(|value| value.parse().ok()).unwrap_or(1);
        Ok(Some(Block::List {
            kind: if ordered { ListKind::Ordered } else { ListKind::Bullet },
            start,
            items,
        }))
    }

    fn build_table(&mut self, id: usize, depth: usize) -> Result<Option<Block>, ConversionError> {
        let source_rows = self.direct_table_rows(id);
        let row_count = source_rows.len();
        if u64::try_from(row_count).unwrap_or(u64::MAX) > self.max_table_rows {
            return Self::limit("max_table_rows", "HTML table has too many rows");
        }
        if source_rows.is_empty() {
            return Ok(None);
        }
        let planned = self.plan_table(&source_rows)?;
        if planned.width == 0 {
            return Ok(None);
        }
        let logical_cells = u64::try_from(planned.width)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::try_from(row_count).unwrap_or(u64::MAX));
        if logical_cells > self.max_table_cells {
            return Self::limit("max_table_cells", "HTML table has too many logical cells");
        }
        let rows = self.render_table(planned, depth)?;
        Ok(Some(Block::Table { rows, alignments: Vec::<TableAlignment>::new() }))
    }

    fn plan_table(
        &mut self,
        source_rows: &[SourceTableRow],
    ) -> Result<PlannedTable, ConversionError> {
        let mut occupancy = Vec::<u32>::new();
        let mut planned = Vec::<Vec<PlannedTableCell>>::new();
        let mut width = 0_usize;
        for (row_index, source_row) in source_rows.iter().copied().enumerate() {
            let mut row_cells = Vec::new();
            let group_rows = source_rows[row_index..]
                .iter()
                .take_while(|row| row.group == source_row.group)
                .count();
            let remaining_rows = u32::try_from(group_rows).unwrap_or(u32::MAX).max(1);
            for cell in self.direct_table_cells(source_row.node) {
                let requested_row_span = table_span(self.attr(cell, "rowspan"));
                let requested_row_span =
                    if requested_row_span == 0 { remaining_rows } else { requested_row_span };
                let row_span = requested_row_span.min(remaining_rows);
                if row_span != requested_row_span {
                    self.diagnostics.push(warning(
                        "html.tableRowspanClamped",
                        "rowspan extending beyond its row group was clamped".into(),
                    ));
                }
                let requested_column_span = table_span(self.attr(cell, "colspan"));
                let column_span = if requested_column_span == 0 {
                    self.diagnostics.push(warning(
                        "html.tableColspanNormalized",
                        "zero colspan was normalized to one column".into(),
                    ));
                    1
                } else {
                    requested_column_span
                };
                let span =
                    usize::try_from(column_span).map_err(|_| ConversionError::ResourceLimit {
                        limit: "max_table_columns",
                        detail: "HTML column span cannot be represented".into(),
                    })?;
                let mut column = 0_usize;
                loop {
                    while occupancy.get(column).is_some_and(|remaining| *remaining > 0) {
                        column += 1;
                    }
                    let end =
                        column.checked_add(span).ok_or_else(|| ConversionError::ResourceLimit {
                            limit: "max_table_columns",
                            detail: "HTML table width overflowed".into(),
                        })?;
                    if u64::try_from(end).unwrap_or(u64::MAX)
                        > self.max_table_columns.min(MAX_TABLE_COLUMNS as u64)
                    {
                        return Self::limit("max_table_columns", "HTML table is too wide");
                    }
                    if occupancy
                        .get(column..end)
                        .is_some_and(|slots| slots.iter().any(|remaining| *remaining > 0))
                    {
                        column += 1;
                        continue;
                    }
                    if occupancy.len() < end {
                        occupancy.resize(end, 0);
                    }
                    occupancy[column..end].fill(row_span);
                    width = width.max(end);
                    row_cells.push(PlannedTableCell { node: cell, column, row_span, column_span });
                    break;
                }
            }
            planned.push(row_cells);
            for remaining in &mut occupancy {
                *remaining = remaining.saturating_sub(1);
            }
        }
        Ok(PlannedTable { rows: planned, width })
    }

    fn render_table(
        &mut self,
        planned: PlannedTable,
        depth: usize,
    ) -> Result<Vec<TableRow>, ConversionError> {
        let mut rows = Vec::with_capacity(planned.rows.len());
        let mut active = vec![0_u32; planned.width];
        for row_cells in planned.rows {
            let mut cells = Vec::new();
            let mut planned_index = 0_usize;
            let mut column = 0_usize;
            while column < planned.width {
                if active[column] > 0 {
                    column += 1;
                    continue;
                }
                if row_cells.get(planned_index).is_some_and(|cell| cell.column == column) {
                    let cell = &row_cells[planned_index];
                    let span = usize::try_from(cell.column_span).unwrap_or(planned.width);
                    let end = column.saturating_add(span).min(planned.width);
                    active[column..end].fill(cell.row_span);
                    let blocks = self.collect_child_blocks(cell.node, depth.saturating_add(1))?;
                    cells.push(Cell {
                        row_span: cell.row_span,
                        column_span: cell.column_span,
                        header: self.name(cell.node) == Some("th"),
                        blocks,
                    });
                    planned_index += 1;
                    column = end;
                } else {
                    active[column] = 1;
                    cells.push(Cell {
                        row_span: 1,
                        column_span: 1,
                        header: false,
                        blocks: Vec::new(),
                    });
                    column += 1;
                }
            }
            rows.push(TableRow { cells });
            for remaining in &mut active {
                *remaining = remaining.saturating_sub(1);
            }
        }
        Ok(rows)
    }

    fn build_image(&mut self, id: usize) -> Option<Block> {
        let alt = self.attr(id, "alt").map(normalize).filter(|v| !v.is_empty());
        let src = self.attr(id, "src")?;
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
            return alt
                .map(|value| Block::Paragraph(vec![Inline::Text { value, marks: Vec::new() }]));
        };
        let asset_id = AssetId(format!("html-external-image-{:06}", self.assets.len() + 1));
        self.assets.push(Asset {
            id: asset_id.clone(),
            filename: None,
            media_type: image_media_type(&uri).into(),
            bytes: Vec::new(),
            external_uri: Some(uri),
        });
        Some(Block::Image { asset: asset_id, alt })
    }

    fn inline_children(&mut self, id: usize) -> Result<Vec<Inline>, ConversionError> {
        let mut output = Vec::new();
        for child in self.nodes[id].children.clone() {
            if !self.is_block_node(child) {
                output.extend(self.inline_node(child, Vec::new())?);
            }
        }
        Ok(output)
    }

    fn inline_node(
        &mut self,
        id: usize,
        mut marks: Vec<InlineMark>,
    ) -> Result<Vec<Inline>, ConversionError> {
        let mut output = Vec::new();
        if self.node_context(id).excluded() {
            return Ok(output);
        }
        if let NodeData::Text(value) = &self.nodes[id].data {
            let value = normalize(value);
            if !value.is_empty() {
                output.push(Inline::Text { value, marks });
            }
            return Ok(output);
        }
        if let Some(name) = self.name(id) {
            if name == "a" {
                let content = self.inline_children_with_marks(id, &marks)?;
                let Some(href) = self.attr(id, "href") else { return Ok(content) };
                let target = Url::parse(href)
                    .ok()
                    .or_else(|| self.base.as_ref()?.join(href).ok())
                    .map_or_else(|| href.to_string(), |url| url.to_string());
                if safe_link_target(&target) && !content.is_empty() {
                    return Ok(vec![Inline::Link { target, content }]);
                }
                if !content.is_empty() {
                    self.diagnostics.push(warning(
                        "html.linkUriRejected",
                        "unsafe link destination was omitted".into(),
                    ));
                }
                return Ok(content);
            }
            match name {
                "strong" | "b" => marks.push(InlineMark::Bold),
                "em" | "i" => marks.push(InlineMark::Italic),
                "del" | "s" | "strike" => marks.push(InlineMark::Strikethrough),
                "u" => marks.push(InlineMark::Underline),
                "sup" => marks.push(InlineMark::Superscript),
                "sub" => marks.push(InlineMark::Subscript),
                "br" => return Ok(vec![Inline::LineBreak]),
                "code" => {
                    return Ok(nonempty(self.raw_visible_text(id))
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
                NodeData::Element { .. } if self.name(child) == Some("a") => {
                    let content = self.inline_children_with_marks(child, &marks)?;
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
                _ => output.extend(self.inline_node(child, marks.clone())?),
            }
        }
        self.inline_count = self.inline_count.saturating_add(output.len());
        if self.inline_count > MAX_DOCUMENT_INLINES {
            return Self::limit("html_inlines", "HTML produced too many inline nodes");
        }
        Ok(output)
    }

    fn inline_children_with_marks(
        &mut self,
        id: usize,
        marks: &[InlineMark],
    ) -> Result<Vec<Inline>, ConversionError> {
        let mut output = Vec::new();
        for child in self.nodes[id].children.clone() {
            output.extend(self.inline_node(child, marks.to_owned())?);
        }
        Ok(output)
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
    fn is_html_element(&self, id: usize) -> bool {
        match &self.nodes.get(id).map(|node| &node.data) {
            Some(NodeData::Element { name, .. }) => name.ns == html5ever::ns!(html),
            _ => false,
        }
    }
    fn node_context(&self, id: usize) -> NodeContext {
        let mut context = NodeContext::default();
        let mut current = Some(id);
        while let Some(node) = current.and_then(|node| self.nodes.get(node)) {
            if let NodeData::Element { name, .. } = &node.data {
                if name.ns == html5ever::ns!(html) {
                    let local = name.local.as_ref();
                    context.mark(NodeContext::HEAD, local == "head");
                    context.mark(NodeContext::TEMPLATE, local == "template");
                    context.mark(NodeContext::HIDDEN, Self::node_hidden(node));
                    context.mark(NodeContext::BOILERPLATE, Self::node_boilerplate(node));
                } else {
                    context.mark(NodeContext::FOREIGN, true);
                }
            }
            current = node.parent;
        }
        context
    }
    fn node_hidden(node: &DomNode) -> bool {
        let NodeData::Element { name, attrs, .. } = &node.data else { return false };
        matches!(
            name.local.as_ref(),
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
        ) || attrs.iter().any(|attr| matches!(attr.name.local.as_ref(), "hidden" | "inert"))
            || attrs.iter().any(|attr| {
                attr.name.local.as_ref() == "aria-hidden" && attr.value.eq_ignore_ascii_case("true")
            })
    }
    fn node_boilerplate(node: &DomNode) -> bool {
        let NodeData::Element { name, attrs, .. } = &node.data else { return false };
        if matches!(name.local.as_ref(), "nav" | "aside" | "footer") {
            return true;
        }
        if attrs.iter().any(|attr| {
            attr.name.local.as_ref() == "role"
                && ["navigation", "banner", "contentinfo", "complementary", "dialog"]
                    .iter()
                    .any(|role| attr.value.eq_ignore_ascii_case(role))
        }) {
            return true;
        }
        attrs.iter().filter(|attr| matches!(attr.name.local.as_ref(), "id" | "class")).any(|attr| {
            attr.value
                .split(|character: char| {
                    character.is_ascii_whitespace() || matches!(character, '-' | '_' | ':' | '.')
                })
                .filter(|token| !token.is_empty())
                .any(is_boilerplate_token)
        })
    }
    fn visible_text(&self, id: usize) -> String {
        normalize(&self.raw_visible_text(id))
    }
    fn raw_visible_text(&self, id: usize) -> String {
        if self.node_context(id).excluded() {
            return String::new();
        }
        let mut out = String::new();
        match &self.nodes[id].data {
            NodeData::Text(value) => out.push_str(value),
            NodeData::Element { .. } | NodeData::Document => {
                for child in &self.nodes[id].children {
                    out.push_str(&self.raw_visible_text(*child));
                }
            }
            NodeData::Other => {}
        }
        out
    }
    fn raw_text_unfiltered(&self, id: usize) -> String {
        let mut out = String::new();
        for child in &self.nodes[id].children {
            match &self.nodes[*child].data {
                NodeData::Text(value) => out.push_str(value),
                NodeData::Element { .. } => {
                    out.push_str(&self.raw_text_unfiltered(*child));
                }
                _ => {}
            }
        }
        out
    }
    fn visible_text_len(&self, id: usize) -> usize {
        self.visible_text(id).len()
    }
    fn link_text_len(&self, id: usize) -> usize {
        self.descendants(id)
            .into_iter()
            .filter(|id| {
                self.is_html_element(*id)
                    && self.name(*id) == Some("a")
                    && !self.node_context(*id).excluded()
            })
            .map(|id| self.visible_text(id).len())
            .sum()
    }
    fn descendants_named(&self, id: usize, names: &[&str]) -> usize {
        self.descendants(id)
            .into_iter()
            .filter(|id| {
                self.is_html_element(*id)
                    && !self.node_context(*id).excluded()
                    && self.name(*id).is_some_and(|name| names.contains(&name))
            })
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
    fn first_visible_descendant(&self, id: usize, name: &str) -> Option<usize> {
        self.descendants(id).into_iter().find(|id| {
            self.is_html_element(*id)
                && self.name(*id) == Some(name)
                && !self.node_context(*id).excluded()
        })
    }
    fn is_block_node(&self, id: usize) -> bool {
        if !self.is_html_element(id) {
            return matches!(self.name(id), Some("svg" | "math"));
        }
        matches!(
            self.name(id),
            Some(
                "address"
                    | "article"
                    | "body"
                    | "div"
                    | "figcaption"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "head"
                    | "hr"
                    | "html"
                    | "img"
                    | "main"
                    | "ol"
                    | "p"
                    | "pre"
                    | "section"
                    | "table"
                    | "ul"
            )
        )
    }
    fn is_foreign_root(&self, id: usize) -> bool {
        if self.is_html_element(id) || !matches!(self.name(id), Some("svg" | "math")) {
            return false;
        }
        self.nodes[id].parent.is_some_and(|parent| !self.node_context(parent).excluded())
    }
    fn direct_table_rows(&self, table: usize) -> Vec<SourceTableRow> {
        let mut rows = Vec::new();
        for child in self.nodes[table].children.iter().copied() {
            if !self.is_html_element(child) || self.node_context(child).excluded() {
                continue;
            }
            if self.name(child) == Some("tr") {
                rows.push(SourceTableRow { node: child, group: table });
            } else if matches!(self.name(child), Some("thead" | "tbody" | "tfoot")) {
                rows.extend(self.nodes[child].children.iter().copied().filter_map(|row| {
                    (self.is_html_element(row)
                        && self.name(row) == Some("tr")
                        && !self.node_context(row).excluded())
                    .then_some(SourceTableRow { node: row, group: child })
                }));
            }
        }
        rows
    }
    fn direct_table_cells(&self, row: usize) -> Vec<usize> {
        self.nodes[row]
            .children
            .iter()
            .copied()
            .filter(|cell| {
                self.is_html_element(*cell)
                    && matches!(self.name(*cell), Some("td" | "th"))
                    && !self.node_context(*cell).excluded()
            })
            .collect()
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
fn table_span(value: Option<&str>) -> u32 {
    value.and_then(|v| v.parse::<u32>().ok()).unwrap_or(1)
}
fn is_boilerplate_token(token: &str) -> bool {
    [
        "ad",
        "ads",
        "advert",
        "advertisement",
        "advertising",
        "cookie",
        "modal",
        "navigation",
        "popup",
        "recommend",
        "recommended",
        "related",
        "sidebar",
    ]
    .iter()
    .any(|candidate| token.eq_ignore_ascii_case(candidate))
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
        assert!(
            crate::html_document_evidence("<!doctype html><html><body>x</body></html>", &context())
                .unwrap()
        );
        assert!(
            crate::html_document_evidence("<article><h1>x</h1><p>y</p></article>", &context())
                .unwrap()
        );
        assert!(
            !crate::html_document_evidence(
                "<?xml version='1.0'?><rss><item>x</item></rss>",
                &context()
            )
            .unwrap()
        );
        assert!(!crate::html_document_evidence("ordinary <x> text", &context()).unwrap());
        assert!(
            !crate::html_document_evidence("# Markdown\n\n<div>raw</div>", &context()).unwrap()
        );
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

    #[test]
    fn review_p1_1_meta_invalid_attribute_bytes_always_advance() {
        for source in [
            b"<meta @ charset=utf-8><main><p>x</p></main>".as_slice(),
            b"<meta _ : . charset=utf-8><main><p>x</p></main>".as_slice(),
            b"<meta \xc3\xa9 charset=utf-8><main><p>x</p></main>".as_slice(),
        ] {
            assert_eq!(prescan_meta_charset(source, &context()).unwrap().as_deref(), Some("utf-8"));
        }
        assert_eq!(
            prescan_meta_charset(
                b"<meta data-name charset = 'windows-1252'><main>x</main>",
                &context()
            )
            .unwrap()
            .as_deref(),
            Some("windows-1252")
        );
    }

    #[test]
    fn review_p1_2_poisoned_sink_is_constant_noop_and_preserves_first_error() {
        let dom = Dom::new(&ConversionOptions::default(), &context()).unwrap();
        dom.set_error_once(ConversionError::ResourceLimit {
            limit: "first",
            detail: "first error".into(),
        });
        let node_count = dom.nodes.borrow().len();
        let child_count = dom.nodes.borrow()[0].children.len();
        let memory = dom.memory.borrow().mark();
        dom.append(&0, NodeOrText::AppendText(StrTendril::from_slice("ignored")));
        dom.create_element(
            QualName::new(None, html5ever::ns!(html), html5ever::local_name!("div")),
            Vec::new(),
            ElementFlags::default(),
        );
        dom.set_error_once(ConversionError::Internal { detail: "replacement".into() });
        assert_eq!(dom.nodes.borrow().len(), node_count);
        assert_eq!(dom.nodes.borrow()[0].children.len(), child_count);
        assert_eq!(dom.memory.borrow().mark(), memory);
        assert!(matches!(
            dom.error.borrow().as_ref(),
            Some(ConversionError::ResourceLimit { limit: "first", .. })
        ));
    }

    #[test]
    fn review_p1_3_ancestor_exclusion_covers_candidates_and_assets() {
        let output = convert(
            "<div hidden><main><p>secret</p><img src='https://e.invalid/s.png'></main></div><p>real</p>",
        );
        let rendered = format!("{:?}", output.document.blocks);
        assert!(rendered.contains("real"));
        assert!(!rendered.contains("secret"));
        assert!(output.assets.is_empty());

        let output = convert(
            "<nav><main><p>nav secret</p></main></nav><aside><main>aside secret</main></aside><p>real</p>",
        );
        let rendered = format!("{:?}", output.document.blocks);
        assert!(rendered.contains("real"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn review_p1_4_scoring_excludes_boilerplate_and_tokenizes_labels() {
        for label in ["id=advert", "class=ad", "class='hero ad-slot'"] {
            let source = format!("<main><div {label}>noise</div></main><p>real</p>");
            let rendered = format!("{:?}", convert(&source).document.blocks);
            assert!(rendered.contains("real"));
            assert!(!rendered.contains("noise"));
        }
        let rendered =
            format!("{:?}", convert("<main><nav>noise</nav></main><p>real</p>").document.blocks);
        assert!(rendered.contains("real") && !rendered.contains("noise"));
        assert!(
            format!("{:?}", convert("<main><p class=shadow>kept</p></main>").document.blocks)
                .contains("kept")
        );
    }

    #[test]
    fn review_p1_5_direct_and_mixed_container_text_is_preserved() {
        assert!(
            format!("{:?}", convert("<main>Hello world</main>").document.blocks)
                .contains("Hello world")
        );
        assert!(
            format!("{:?}", convert("<body>Body text</body>").document.blocks)
                .contains("Body text")
        );
        let blocks = convert("<main>before<p>middle</p>after</main>").document.blocks;
        assert_eq!(blocks.len(), 3);
        let rendered = format!("{blocks:?}");
        assert!(
            rendered.contains("before")
                && rendered.contains("middle")
                && rendered.contains("after")
        );
    }

    #[test]
    fn review_p1_6_table_occupancy_clamps_and_ignores_nested_rows() {
        let output = convert(
            "<main><table><tr><td rowspan=2>A</td><td>B</td></tr><tr><td colspan=2>C</td></tr></table></main>",
        );
        output.document.validate().unwrap();
        let output = convert("<main><table><tr><td rowspan=2>A</td></tr></table></main>");
        output.document.validate().unwrap();
        assert!(output.diagnostics.iter().any(|d| d.code == "html.tableRowspanClamped"));
        let output = convert(
            "<main><table><tr><td>outer<table><tr><td>inner</td></tr></table></td></tr></table></main>",
        );
        output.document.validate().unwrap();
        assert_eq!(count_tables(&output.document.blocks), 2);
        let output = convert(
            "<main><table><thead><tr><td rowspan=2>A</td></tr></thead><tbody><tr><td rowspan=0>B</td><td colspan=0>C</td></tr><tr><td>D</td></tr></tbody></table></main>",
        );
        output.document.validate().unwrap();
        assert!(output.diagnostics.iter().any(|d| d.code == "html.tableRowspanClamped"));
        assert!(output.diagnostics.iter().any(|d| d.code == "html.tableColspanNormalized"));
    }

    #[test]
    fn review_p1_7_and_8_detector_prefers_markdown_and_stays_bounded() {
        for source in [
            "# Title\n\n<article><p>x</p></article>",
            "```html\n<article><p>x</p></article>\n```",
            "~~~html\n<article><p>x</p></article>\n~~~",
            "    <article><p>x</p></article>",
            "\t<!-- <article><p>x</p></article> -->",
        ] {
            assert_eq!(detected_format(source.as_bytes()), InputFormat::Markdown);
        }
        assert_ne!(
            crate::structured_text_candidate(
                b"<!-- <article><p>x</p></article> --> ordinary text",
                &context()
            )
            .unwrap()
            .map(|candidate| candidate.format),
            Some(InputFormat::Html)
        );
        let mut large = b"<article><p>x</p></article>".to_vec();
        large.resize(super::super::TEXT_INSPECTION_BYTE_LIMIT + 2 * 1024 * 1024, b' ');
        assert_eq!(detected_format(&large), InputFormat::Html);
    }

    #[test]
    fn review_p1_9_meta_prescan_skips_comments_scripts_and_metadata() {
        let mut source = b"<!-- <meta charset=utf-8> --><script><meta charset=utf-8></script><metadata charset=utf-8></metadata><meta charset=windows-1252><main><p>caf".to_vec();
        source.push(0xe9);
        source.extend_from_slice(b"</p></main>");
        let output = convert_html(
            &ResolvedInput { bytes: Arc::from(source), metadata: SourceMetadata::default() },
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert!(format!("{:?}", output.document.blocks).contains("caf\u{e9}"));
        assert_eq!(
            prescan_meta_charset(b"<metadata charset=utf-8><meta charset=big5>", &context())
                .unwrap()
                .as_deref(),
            Some("big5")
        );
        assert_eq!(
            prescan_meta_charset(
                b"<script><meta charset=utf-8></scripture><meta charset=big5></script><meta charset=windows-1252>",
                &context(),
            )
            .unwrap()
            .as_deref(),
            Some("windows-1252")
        );
    }

    #[test]
    fn review_p1_10_metadata_requires_visible_html_head_context() {
        let output = convert(
            "<!doctype html><html><head><template><meta name=author content=evil></template><title>real</title><meta name=author content=good></head><body><svg><title>svg-title</title></svg><p>x</p></body></html>",
        );
        assert_eq!(output.document.metadata.title.as_deref(), Some("real"));
        assert_eq!(output.document.metadata.authors, ["good"]);
    }

    #[test]
    fn review_p2_11_nested_lists_remain_nested_blocks() {
        let output = convert("<main><ul><li>one<ul><li>two</li></ul></li></ul></main>");
        let Block::List { items, .. } = &output.document.blocks[0].block else {
            panic!("expected list")
        };
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].blocks[0].block, Block::Paragraph(_)));
        assert!(matches!(items[0].blocks[1].block, Block::List { .. }));
        output.document.validate().unwrap();
    }

    #[test]
    fn review_p2_12_invalid_base_does_not_hide_first_valid_base() {
        let input = ResolvedInput {
            bytes: Arc::from(
                b"<head><base href='http://127.0.0.1/private/'><base href='https://example.invalid/docs/'></head><body><main><img src='a.png'></main></body>".as_slice(),
            ),
            metadata: SourceMetadata {
                uri: Some("https://source.invalid/root.html".into()),
                ..SourceMetadata::default()
            },
        };
        let output = convert_html(&input, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(
            output.assets[0].external_uri.as_deref(),
            Some("https://example.invalid/docs/a.png")
        );
        assert!(output.diagnostics.iter().any(|d| d.code == "html.baseRejected"));
    }

    fn detected_format(source: &[u8]) -> InputFormat {
        crate::structured_text_candidate(source, &context()).unwrap().unwrap().format
    }

    fn count_tables(blocks: &[BlockNode]) -> usize {
        blocks
            .iter()
            .map(|node| match &node.block {
                Block::Table { rows, .. } => {
                    1 + rows
                        .iter()
                        .flat_map(|row| &row.cells)
                        .map(|cell| count_tables(&cell.blocks))
                        .sum::<usize>()
                }
                Block::List { items, .. } => {
                    items.iter().map(|item| count_tables(&item.blocks)).sum()
                }
                _ => 0,
            })
            .sum()
    }
}
