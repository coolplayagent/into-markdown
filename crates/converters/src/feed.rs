//! Bounded, offline RSS 2.0 and Atom 1.0 conversion.

use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter,
    ConverterOutput, Diagnostic, DiagnosticSeverity, Document, DocumentMetadata, ExecutionContext,
    FormatCandidate, Inline, InputFormat, IrErrorCode, NodeId, ProbeOutcome, Provenance,
    ProvenanceKind, ResolvedInput, Services, SourceLocator,
};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use url::Url;

#[cfg(test)]
use std::cell::Cell;

use super::text::{DecodedText, LogicalMemory};

const FORMATS: &[InputFormat] = &[InputFormat::Feed];
const PROVIDER_ID: &str = "builtin.converter.feed";
const ATOM_NS: &str = "http://www.w3.org/2005/Atom";
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const CONTENT_NS: &str = "http://purl.org/rss/1.0/modules/content/";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const MAX_FEED_EVENTS: usize = 1_000_000;
const MAX_FEED_DIAGNOSTICS: usize = 100_000;
const MAX_FEED_ATTRIBUTES_PER_ELEMENT: usize = 4096;
const FEED_DETECTION_EVENT_LIMIT: usize = 4096;
const FEED_DETECTION_BYTE_LIMIT: usize = 1024 * 1024;

#[cfg(test)]
thread_local! {
    static FEED_XML_OBJECTS: Cell<usize> = const { Cell::new(0) };
    static FEED_XML_STRINGS: Cell<usize> = const { Cell::new(0) };
    static FEED_URL_PARSES: Cell<usize> = const { Cell::new(0) };
    static FEED_XHTML_WRITES: Cell<usize> = const { Cell::new(0) };
    static FEED_XHTML_ESCAPES: Cell<usize> = const { Cell::new(0) };
    static FEED_XHTML_ESCAPE_PEAK: Cell<usize> = const { Cell::new(0) };
    static FEED_XHTML_MEMORY_PEAK: Cell<usize> = const { Cell::new(0) };
    static FEED_TARGET_ATTRIBUTE: Cell<usize> = const { Cell::new(usize::MAX) };
    static FEED_TARGET_ATTRIBUTE_CONSTRUCTIONS: Cell<usize> = const { Cell::new(0) };
    static FEED_DIAGNOSTIC_PUBLICATION_SWAPS: Cell<usize> = const { Cell::new(0) };
}

fn record_feed_xml_object() {
    #[cfg(test)]
    FEED_XML_OBJECTS.with(|count| count.set(count.get() + 1));
}

fn record_feed_xml_string() {
    #[cfg(test)]
    FEED_XML_STRINGS.with(|count| count.set(count.get() + 1));
}

fn record_feed_attribute_construction(index: usize) {
    #[cfg(test)]
    FEED_TARGET_ATTRIBUTE.with(|target| {
        if target.get() == index {
            FEED_TARGET_ATTRIBUTE_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
        }
    });
    #[cfg(not(test))]
    let _ = index;
}

fn record_diagnostic_publication_swap() {
    #[cfg(test)]
    FEED_DIAGNOSTIC_PUBLICATION_SWAPS.with(|count| count.set(count.get() + 1));
}

fn record_feed_url_parse() {
    #[cfg(test)]
    FEED_URL_PARSES.with(|count| count.set(count.get() + 1));
}

fn record_feed_xhtml_write() {
    #[cfg(test)]
    FEED_XHTML_WRITES.with(|count| count.set(count.get() + 1));
}

fn record_feed_xhtml_memory(memory: usize) {
    #[cfg(test)]
    FEED_XHTML_MEMORY_PEAK.with(|peak| peak.set(peak.get().max(memory)));
    #[cfg(not(test))]
    let _ = memory;
}

fn record_feed_xhtml_escape(memory: usize) {
    #[cfg(test)]
    {
        FEED_XHTML_ESCAPES.with(|count| count.set(count.get() + 1));
        FEED_XHTML_ESCAPE_PEAK.with(|peak| peak.set(memory));
    }
    #[cfg(not(test))]
    let _ = memory;
}

#[cfg(test)]
fn reset_feed_xml_hooks() {
    FEED_XML_OBJECTS.with(|count| count.set(0));
    FEED_XML_STRINGS.with(|count| count.set(0));
    FEED_URL_PARSES.with(|count| count.set(0));
    FEED_XHTML_WRITES.with(|count| count.set(0));
    FEED_XHTML_ESCAPES.with(|count| count.set(0));
    FEED_XHTML_ESCAPE_PEAK.with(|peak| peak.set(0));
    FEED_XHTML_MEMORY_PEAK.with(|peak| peak.set(0));
    FEED_TARGET_ATTRIBUTE.with(|target| target.set(usize::MAX));
    FEED_TARGET_ATTRIBUTE_CONSTRUCTIONS.with(|count| count.set(0));
    FEED_DIAGNOSTIC_PUBLICATION_SWAPS.with(|count| count.set(0));
}

#[cfg(test)]
fn watch_feed_attribute(index: usize) {
    FEED_TARGET_ATTRIBUTE.with(|target| target.set(index));
    FEED_TARGET_ATTRIBUTE_CONSTRUCTIONS.with(|count| count.set(0));
}

#[cfg(test)]
fn target_attribute_constructions() -> usize {
    FEED_TARGET_ATTRIBUTE_CONSTRUCTIONS.with(Cell::get)
}

#[cfg(test)]
fn diagnostic_publication_swaps() -> usize {
    FEED_DIAGNOSTIC_PUBLICATION_SWAPS.with(Cell::get)
}

#[cfg(test)]
fn feed_xhtml_escape_peak() -> usize {
    FEED_XHTML_ESCAPE_PEAK.with(Cell::get)
}

#[cfg(test)]
fn feed_xhtml_memory_peak() -> usize {
    FEED_XHTML_MEMORY_PEAK.with(Cell::get)
}

#[cfg(test)]
fn feed_xml_hook_counts() -> (usize, usize, usize, usize, usize) {
    (
        FEED_XML_OBJECTS.with(Cell::get),
        FEED_XML_STRINGS.with(Cell::get),
        FEED_URL_PARSES.with(Cell::get),
        FEED_XHTML_WRITES.with(Cell::get),
        FEED_XHTML_ESCAPES.with(Cell::get),
    )
}

/// Strict local RSS 2.0 and Atom 1.0 converter.
#[derive(Debug, Default)]
pub struct FeedConverter;

impl Converter for FeedConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn priority(&self) -> i32 {
        220
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
            if candidate.format != InputFormat::Feed {
                return Ok(ProbeOutcome::NotApplicable);
            }
            if candidate.explicit || candidate.detector_id == "builtin.detector.hints" {
                return Ok(ProbeOutcome::Match { confidence: 1.0 });
            }
            Ok(if strong_feed_evidence(&input.bytes, context)? {
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
        Box::pin(async move { convert_feed(input, options, context) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FeedKind {
    Rss,
    Atom,
}

#[derive(Default)]
struct Entry {
    title: Option<Content>,
    author: Option<String>,
    published: Option<String>,
    updated: Option<String>,
    link: Option<LinkCandidate>,
    id: Option<String>,
    summary: Option<Content>,
    content: Option<Content>,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ContentType {
    #[default]
    Text,
    Html,
    Xhtml,
}

#[derive(Default)]
struct Content {
    value: String,
    kind: ContentType,
    base: Option<String>,
}

struct LinkCandidate {
    rank: u8,
    target: String,
}

struct Frame {
    local: String,
    namespace: String,
    base: Option<String>,
    text: String,
    attrs: FeedAttributes,
    xhtml_div_start: Option<usize>,
    xhtml_div_end: Option<usize>,
}

struct FeedAttribute {
    raw: String,
    namespace: String,
    local: String,
    value: String,
}

impl FeedAttribute {
    fn expanded_eq(&self, namespace: &str, local: &str) -> bool {
        self.namespace == namespace && self.local == local
    }
}

#[derive(Default)]
struct FeedAttributes(Vec<FeedAttribute>);

impl FeedAttributes {
    fn get(&self, raw: &str) -> Option<&String> {
        self.0.iter().find(|attribute| attribute.raw == raw).map(|attribute| &attribute.value)
    }
}

struct ParsedFeed {
    kind: FeedKind,
    title: Option<Content>,
    subtitle: Option<Content>,
    author: Option<String>,
    link: Option<LinkCandidate>,
    updated: Option<String>,
    entries: Vec<Entry>,
    diagnostics: Vec<Diagnostic>,
    decoded: DecodedText,
    budget: FeedBudget,
}

struct FeedBudget {
    aggregate: super::html::FeedHtmlBudget,
    events: usize,
    source_text_bytes: u64,
    html_bytes: u64,
    max_source_text_bytes: u64,
    max_html_bytes: u64,
}

impl FeedBudget {
    fn new(
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        Ok(Self {
            aggregate: super::html::FeedHtmlBudget::new(
                options.limits.max_feed_text_bytes,
                MAX_FEED_DIAGNOSTICS,
                options.limits.max_memory_bytes,
                context,
            )?,
            events: 0,
            source_text_bytes: 0,
            html_bytes: 0,
            max_source_text_bytes: options.limits.max_feed_text_bytes,
            max_html_bytes: options.limits.max_feed_html_bytes,
        })
    }

    fn event(&mut self) -> Result<(), ConversionError> {
        self.events = self
            .events
            .checked_add(1)
            .ok_or_else(|| limit("feed_events", "feed event count overflowed"))?;
        if self.events > MAX_FEED_EVENTS {
            return Err(limit(
                "feed_events",
                &format!("feed exceeds {MAX_FEED_EVENTS} XML events"),
            ));
        }
        Ok(())
    }

    fn source_text(&mut self, bytes: usize) -> Result<(), ConversionError> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.source_text_bytes = self
            .source_text_bytes
            .checked_add(bytes)
            .ok_or_else(|| limit("max_feed_text_bytes", "feed text byte count overflowed"))?;
        if self.source_text_bytes > self.max_source_text_bytes {
            return Err(limit(
                "max_feed_text_bytes",
                &format!("feed text exceeds {} bytes", self.max_source_text_bytes),
            ));
        }
        Ok(())
    }

    fn html(&mut self, bytes: usize) -> Result<(), ConversionError> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.html_bytes = self.html_bytes.checked_add(bytes).ok_or_else(|| {
            limit("max_feed_html_bytes", "nested feed HTML byte count overflowed")
        })?;
        if self.html_bytes > self.max_html_bytes {
            return Err(limit(
                "max_feed_html_bytes",
                &format!("nested feed HTML exceeds {} bytes", self.max_html_bytes),
            ));
        }
        Ok(())
    }

    fn ir(&mut self, nodes: usize, inlines: usize) -> Result<(), ConversionError> {
        self.aggregate.nodes(nodes)?;
        self.aggregate.inlines(inlines)?;
        Ok(())
    }
}

fn limit(name: &'static str, detail: &str) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("feed".into()), detail: detail.into() }
}

pub(crate) fn strong_feed_evidence(
    source: &[u8],
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let (charset, _) = super::structured::xml_charset(source);
    let source = detection_prefix(source, charset);
    let Ok((decoded, _)) = super::text::decode_source(
        source,
        Some(charset),
        into_markdown_core::TextDecodingMode::Strict,
        context,
    ) else {
        return Ok(false);
    };
    let mut reader = NsReader::from_str(&decoded.text);
    reader.config_mut().check_end_names = true;
    let mut root: Option<FeedKind> = None;
    let mut depth = 0_usize;
    for _ in 0..FEED_DETECTION_EVENT_LIMIT {
        context.checkpoint()?;
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                let (namespace, local) = resolved_name(&reader, &element)?;
                if root.is_none() {
                    if local == "feed" && namespace == ATOM_NS {
                        return Ok(true);
                    }
                    if local == "rss"
                        && namespace.is_empty()
                        && attribute_equals(&reader, &element, "version", "2.0")?
                    {
                        root = Some(FeedKind::Rss);
                        depth = 1;
                        continue;
                    }
                    return Ok(false);
                }
                if root == Some(FeedKind::Rss) && depth == 1 {
                    return Ok(local == "channel" && namespace.is_empty());
                }
                return Ok(false);
            }
            Ok(Event::Empty(element)) => {
                let (namespace, local) = resolved_name(&reader, &element)?;
                if root.is_none() {
                    return Ok(local == "feed" && namespace == ATOM_NS);
                }
                if root == Some(FeedKind::Rss) && depth == 1 {
                    return Ok(local == "channel" && namespace.is_empty());
                }
                return Ok(false);
            }
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(Event::Decl(_) | Event::Comment(_) | Event::PI(_)) => {}
            Ok(Event::Text(value)) if value.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            _ => return Ok(false),
        }
    }
    Ok(false)
}

fn detection_prefix<'a>(source: &'a [u8], charset: &str) -> &'a [u8] {
    let mut end = source.len().min(FEED_DETECTION_BYTE_LIMIT);
    match charset {
        "utf-8" => {
            if let Err(error) = std::str::from_utf8(&source[..end])
                && error.error_len().is_none()
            {
                end = error.valid_up_to();
            }
        }
        "utf-16le" | "utf-16be" => {
            end -= end % 2;
            if end >= 2 {
                let pair = &source[end - 2..end];
                let unit = if charset == "utf-16le" {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                };
                if (0xd800..=0xdbff).contains(&unit) {
                    end -= 2;
                }
            }
        }
        _ => {}
    }
    &source[..end]
}

fn convert_feed(
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
    let parsed = parse_feed(input, options, context)?;
    build_output(parsed, options, context)
}

#[allow(clippy::too_many_lines)]
fn parse_feed(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ParsedFeed, ConversionError> {
    let (charset, actual) = super::structured::xml_charset(&input.bytes);
    let (decoded, recoveries) = super::text::decode_source(
        &input.bytes,
        Some(charset),
        into_markdown_core::TextDecodingMode::Strict,
        context,
    )?;
    if !recoveries.is_empty() {
        return Err(ConversionError::Internal {
            detail: "strict feed XML decoding produced recovery diagnostics".into(),
        });
    }
    let largest = super::structured::preflight_xml(&decoded.text, context)?;
    let mut budget = FeedBudget::new(options, context)?;
    let mut scratch = LogicalMemory::new(context)?;
    let mut reader = NsReader::from_str(&decoded.text);
    {
        let config = reader.config_mut();
        config.allow_dangling_amp = false;
        config.allow_unmatched_ends = false;
        config.check_end_names = true;
        config.check_comments = true;
    }
    let source_base = input
        .metadata
        .uri
        .as_deref()
        .map(|uri| canonical_source_base(uri, &mut budget, context))
        .transpose()?
        .flatten();
    let mut stack: Vec<Frame> = Vec::new();
    let mut root = None;
    let mut title = None;
    let mut subtitle = None;
    let mut author = None;
    let mut link = None;
    let mut updated = None;
    let mut entries = Vec::new();
    let mut current_entry: Option<Entry> = None;
    let mut rss_channel_seen = false;
    let mut diagnostics = Vec::new();
    let mut previous = 0_usize;
    loop {
        context.checkpoint()?;
        let mark = scratch.mark();
        scratch.charge(largest)?;
        let event = reader.read_event().map_err(|error| {
            malformed(format!("invalid feed XML near byte {previous}: {error}"))
        })?;
        let end = usize::try_from(reader.buffer_position()).map_err(|_| {
            ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "feed parser position exceeds platform address space".into(),
            }
        })?;
        let start = previous;
        previous = end;
        budget.event()?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                if stack.len() >= usize::from(options.limits.max_nesting_depth) {
                    return Err(ConversionError::ResourceLimit {
                        limit: "feed_nesting_depth",
                        detail: format!("feed exceeds {} levels", options.limits.max_nesting_depth),
                    });
                }
                let (namespace, local) = resolved_name_bounded(&reader, &element, &mut budget)?;
                let attrs = attributes(&reader, &element, &mut budget, context)?;
                if stack.is_empty() {
                    if root.is_some() {
                        return Err(malformed("feed XML contains multiple root elements"));
                    }
                    let kind = if local == "feed" && namespace == ATOM_NS {
                        FeedKind::Atom
                    } else if local == "rss"
                        && namespace.is_empty()
                        && attrs.get("version").map(String::as_str) == Some("2.0")
                    {
                        FeedKind::Rss
                    } else {
                        return Err(malformed(
                            "root must be RSS 2.0 rss or Atom 1.0 namespace-qualified feed",
                        ));
                    };
                    root = Some(kind);
                } else if matches!(root, Some(FeedKind::Rss))
                    && stack.len() == 1
                    && !(local == "channel" && namespace.is_empty())
                {
                    return Err(malformed("RSS 2.0 root must contain a channel element"));
                }
                if matches!(root, Some(FeedKind::Rss))
                    && stack.len() == 1
                    && local == "channel"
                    && namespace.is_empty()
                {
                    if rss_channel_seen {
                        return Err(malformed("RSS 2.0 must contain exactly one channel"));
                    }
                    rss_channel_seen = true;
                }
                let inherited =
                    stack.last().and_then(|frame| frame.base.as_deref()).or(source_base.as_deref());
                let base = xml_base(&attrs, inherited, &mut diagnostics, &mut budget, context)?;
                if is_entry_start(root, &stack, &local, &namespace) {
                    if current_entry.is_some() {
                        return Err(malformed("feed entries cannot be nested"));
                    }
                    let count = entries.len().saturating_add(1);
                    if count
                        > usize::try_from(options.limits.max_feed_entries).unwrap_or(usize::MAX)
                    {
                        return Err(ConversionError::ResourceLimit {
                            limit: "max_feed_entries",
                            detail: format!(
                                "feed exceeds {} entries",
                                options.limits.max_feed_entries
                            ),
                        });
                    }
                    budget.aggregate.reserve_vec(&mut entries, 1)?;
                    current_entry = Some(Entry {
                        start: decoded.source_range(start, start).0,
                        ..Entry::default()
                    });
                }
                let atom_text_construct = atom_text_construct_path(root, &stack);
                if atom_text_construct
                    && stack
                        .last()
                        .and_then(|parent| parent.attrs.get("type"))
                        .map_or("text", String::as_str)
                        != "xhtml"
                {
                    return Err(malformed(
                        "Atom text/html constructs must contain escaped text, not child elements",
                    ));
                }
                if atom_text_construct
                    && let Some(parent) = stack.last_mut()
                    && parent.attrs.get("type").is_some_and(|value| value == "xhtml")
                {
                    if parent.xhtml_div_start.is_some() {
                        return Err(malformed(
                            "Atom type=xhtml content must contain exactly one XHTML div",
                        ));
                    }
                    if local != "div" || namespace != XHTML_NS {
                        return Err(malformed(
                            "Atom type=xhtml content must contain one XHTML div",
                        ));
                    }
                    parent.xhtml_div_start = Some(start);
                    if empty {
                        parent.xhtml_div_end = Some(end);
                    }
                }
                budget.aggregate.reserve_vec(&mut stack, 1)?;
                stack.push(Frame {
                    local,
                    namespace,
                    base,
                    text: String::new(),
                    attrs,
                    xhtml_div_start: None,
                    xhtml_div_end: None,
                });
                if empty {
                    close_frame(
                        &mut stack,
                        root,
                        &mut current_entry,
                        &mut title,
                        &mut subtitle,
                        &mut author,
                        &mut link,
                        &mut updated,
                        &mut entries,
                        &decoded,
                        start,
                        end,
                        options,
                        &mut diagnostics,
                        &mut budget,
                        context,
                    )?;
                }
            }
            Event::End(_) => {
                if stack.len() >= 2 {
                    let child = stack.last().expect("length checked");
                    if child.local == "div" && child.namespace == XHTML_NS {
                        let parent_index = stack.len() - 2;
                        let parent = stack.get_mut(parent_index).expect("length checked");
                        if is_atom_text_construct_name(&parent.local) && parent.namespace == ATOM_NS
                        {
                            parent.xhtml_div_end = Some(end);
                        }
                    }
                }
                close_frame(
                    &mut stack,
                    root,
                    &mut current_entry,
                    &mut title,
                    &mut subtitle,
                    &mut author,
                    &mut link,
                    &mut updated,
                    &mut entries,
                    &decoded,
                    start,
                    end,
                    options,
                    &mut diagnostics,
                    &mut budget,
                    context,
                )?;
            }
            Event::Text(value) => {
                let decoded_text = value
                    .xml_content()
                    .map_err(|error| malformed(format!("invalid feed text: {error}")))?;
                if stack.is_empty() && !decoded_text.chars().all(char::is_whitespace) {
                    return Err(malformed("character data appears outside the feed root"));
                }
                append_text(stack.last_mut(), &decoded_text, &mut budget)?;
            }
            Event::CData(value) => {
                let decoded_text = value
                    .decode()
                    .map_err(|error| malformed(format!("invalid feed CDATA: {error}")))?;
                if stack.is_empty() && !decoded_text.is_empty() {
                    return Err(malformed("CDATA appears outside the feed root"));
                }
                append_text(stack.last_mut(), &decoded_text, &mut budget)?;
            }
            Event::GeneralRef(reference) => {
                let raw = std::str::from_utf8(reference.as_ref())
                    .map_err(|_| malformed("feed entity is not UTF-8"))?;
                let scalar = super::structured::predefined_or_numeric_entity_scalar(raw)?;
                let mut encoded = [0_u8; 4];
                append_text(stack.last_mut(), scalar.encode_utf8(&mut encoded), &mut budget)?;
            }
            Event::Decl(_) => {
                if start != 0 {
                    return Err(malformed("XML declaration must be first"));
                }
                let declaration = decoded.text.get(start..end).ok_or_else(|| {
                    ConversionError::Internal { detail: "feed declaration span is invalid".into() }
                })?;
                super::structured::validate_xml_declaration(declaration, actual)?;
            }
            Event::DocType(_) => return Err(malformed("DOCTYPE and DTD are not allowed in feeds")),
            Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => {
                scratch.rewind(mark)?;
                break;
            }
        }
        scratch.rewind(mark)?;
    }
    let kind = root.ok_or_else(|| malformed("feed root is missing"))?;
    if !stack.is_empty() || current_entry.is_some() {
        return Err(malformed("feed XML is incomplete"));
    }
    if kind == FeedKind::Rss && !rss_channel_seen {
        return Err(malformed("RSS 2.0 root is missing its channel"));
    }
    if kind == FeedKind::Rss && entries.is_empty() {
        push_diagnostic_parts(
            &mut diagnostics,
            DiagnosticSeverity::Warning,
            "feed.empty",
            "RSS channel contains no items",
            None,
            &mut budget,
        )?;
    }
    Ok(ParsedFeed {
        kind,
        title,
        subtitle,
        author,
        link,
        updated,
        entries,
        diagnostics,
        decoded,
        budget,
    })
}

fn resolved_name_bounded(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    budget: &mut FeedBudget,
) -> Result<(String, String), ConversionError> {
    let (namespace, local) = reader.resolve_element(element.name());
    let namespace = match namespace {
        ResolveResult::Unbound => String::new(),
        ResolveResult::Bound(value) => budgeted_copy(
            std::str::from_utf8(value.as_ref())
                .map_err(|_| malformed("namespace URI is not UTF-8"))?,
            budget,
        )?,
        ResolveResult::Unknown(_) => return Err(malformed("unbound element namespace prefix")),
    };
    let local = budgeted_copy(
        std::str::from_utf8(local.as_ref())
            .map_err(|_| malformed("element local name is not UTF-8"))?,
        budget,
    )?;
    Ok((namespace, local))
}

fn resolved_name(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<(String, String), ConversionError> {
    let (namespace, local) = reader.resolve_element(element.name());
    let namespace = match namespace {
        ResolveResult::Unbound => String::new(),
        ResolveResult::Bound(value) => std::str::from_utf8(value.as_ref())
            .map_err(|_| malformed("namespace URI is not UTF-8"))?
            .to_owned(),
        ResolveResult::Unknown(_) => return Err(malformed("unbound element namespace prefix")),
    };
    let local = std::str::from_utf8(local.as_ref())
        .map_err(|_| malformed("element local name is not UTF-8"))?
        .to_owned();
    Ok((namespace, local))
}

fn attribute_equals(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    wanted: &str,
    expected: &str,
) -> Result<bool, ConversionError> {
    for attribute in element.attributes() {
        let attribute =
            attribute.map_err(|error| malformed(format!("invalid attribute: {error}")))?;
        if attribute.key.as_ref() == wanted.as_bytes() {
            return Ok(attribute
                .decode_and_unescape_value(reader.decoder())
                .map_err(|error| malformed(format!("invalid attribute {wanted:?}: {error}")))?
                == expected);
        }
    }
    Ok(false)
}

fn budgeted_copy(value: &str, budget: &mut FeedBudget) -> Result<String, ConversionError> {
    let mut output = String::new();
    budget.aggregate.reserve_string_capacity(&mut output, value.len())?;
    record_feed_xml_string();
    output.push_str(value);
    Ok(output)
}

fn budgeted_concat(
    prefix: &str,
    value: &str,
    budget: &mut FeedBudget,
) -> Result<String, ConversionError> {
    let length = prefix
        .len()
        .checked_add(value.len())
        .ok_or_else(|| limit("max_memory_bytes", "feed string length overflowed"))?;
    let mut output = String::new();
    budget.aggregate.reserve_string_capacity(&mut output, length)?;
    record_feed_xml_string();
    output.push_str(prefix);
    output.push_str(value);
    Ok(output)
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn budgeted_numbered(
    prefix: &str,
    number: usize,
    minimum_digits: usize,
    budget: &mut FeedBudget,
) -> Result<String, ConversionError> {
    let digits = decimal_digits(number).max(minimum_digits);
    let length = prefix
        .len()
        .checked_add(digits)
        .ok_or_else(|| limit("max_memory_bytes", "numbered feed string length overflowed"))?;
    let mut output = String::new();
    budget.aggregate.reserve_string_capacity(&mut output, length)?;
    record_feed_xml_string();
    write!(output, "{prefix}{number:0minimum_digits$}").map_err(|_| ConversionError::Internal {
        detail: "numbered feed string formatting failed".into(),
    })?;
    debug_assert_eq!(output.len(), length);
    Ok(output)
}

fn replace_counted_string(
    destination: &mut String,
    replacement: String,
    old_bytes: usize,
    new_bytes: usize,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    if let Err(error) = budget.aggregate.replacement_output_growth(old_bytes, new_bytes) {
        let replacement_capacity = replacement.capacity();
        drop(replacement);
        budget.aggregate.release_temporary_capacity(replacement_capacity)?;
        return Err(error);
    }
    let old_capacity = destination.capacity();
    if let Err(error) = budget.aggregate.validate_temporary_capacity_release(old_capacity) {
        let replacement_capacity = replacement.capacity();
        drop(replacement);
        budget.aggregate.release_temporary_capacity(replacement_capacity)?;
        return Err(error);
    }
    let old = std::mem::replace(destination, replacement);
    drop(old);
    budget.aggregate.release_temporary_capacity(old_capacity)
}

/// Cooperative workspace bound for the pinned `url` parser. The persistent
/// normalized URL is accounted separately at its actual `String` capacity.
/// `url` 2.5.4 stores one serialization plus component/range tables; the 16x
/// byte factor covers UTF-8 percent/IDNA expansion and all range slots, while
/// the fixed term covers the parser and IDNA working vectors. This reservation
/// is made before the first `Url::parse` call and is released only after the
/// `Url` value is dropped.
fn url_workspace_bound(value: &str, base: Option<&str>) -> Result<usize, ConversionError> {
    value
        .len()
        .checked_add(base.map_or(0, str::len))
        .and_then(|bytes| bytes.checked_mul(16))
        .and_then(|bytes| bytes.checked_add(4096))
        .ok_or_else(|| limit("max_memory_bytes", "URL workspace bound overflowed"))
}

fn canonical_source_base(
    value: &str,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<Option<String>, ConversionError> {
    let transaction = budget.aggregate.transaction_snapshot();
    let result = (|| {
        let workspace = url_workspace_bound(value, None)?;
        budget.aggregate.prepay_parser_memory(workspace)?;
        context.checkpoint()?;
        record_feed_url_parse();
        let parsed = Url::parse(value).ok().and_then(super::html::valid_http_base);
        let output = parsed.as_ref().map(|url| budgeted_copy(url.as_str(), budget)).transpose();
        drop(parsed);
        let output = output?;
        budget.aggregate.release_parser_memory(workspace)?;
        Ok(output)
    })();
    match result {
        Ok(output) => Ok(output),
        Err(error) => {
            // Every temporary/result value from the closure has been dropped;
            // an atomic prepay failure is also safe to rewind to the same mark.
            budget.aggregate.rewind(transaction)?;
            Err(error)
        }
    }
}

fn attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<FeedAttributes, ConversionError> {
    let transaction = budget.aggregate.transaction_snapshot();
    match attributes_inner(reader, element, budget, context) {
        Ok(output) => Ok(output),
        Err(error) => {
            budget.aggregate.rewind(transaction)?;
            Err(error)
        }
    }
}

fn attributes_inner(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<FeedAttributes, ConversionError> {
    let mut output = FeedAttributes::default();
    for (attribute_index, attribute) in element.attributes().enumerate() {
        if output.0.len() >= MAX_FEED_ATTRIBUTES_PER_ELEMENT {
            return Err(limit(
                "feed_attributes",
                &format!("feed element exceeds {MAX_FEED_ATTRIBUTES_PER_ELEMENT} attributes"),
            ));
        }
        if output.0.len().is_multiple_of(128) {
            context.checkpoint()?;
        }
        let attribute =
            attribute.map_err(|error| malformed(format!("invalid attribute: {error}")))?;
        let raw = std::str::from_utf8(attribute.key.as_ref())
            .map_err(|_| malformed("attribute name is not UTF-8"))?;
        let raw_value = std::str::from_utf8(attribute.value.as_ref())
            .map_err(|_| malformed("attribute value is not UTF-8"))?;
        let value = unescape_attribute(raw_value, budget, context)?;
        super::structured::validate_xml_chars(&value, raw)?;
        let (namespace, local) = reader.resolve_attribute(attribute.key);
        let namespace = match namespace {
            ResolveResult::Unbound => String::new(),
            ResolveResult::Bound(uri) => budgeted_copy(
                std::str::from_utf8(uri.as_ref())
                    .map_err(|_| malformed("attribute namespace URI is not UTF-8"))?,
                budget,
            )?,
            ResolveResult::Unknown(_) => {
                return Err(malformed("unbound attribute namespace prefix"));
            }
        };
        let local = budgeted_copy(
            std::str::from_utf8(local.as_ref())
                .map_err(|_| malformed("attribute local name is not UTF-8"))?,
            budget,
        )?;
        if output.0.iter().any(|existing| existing.expanded_eq(&namespace, &local)) {
            return Err(malformed("duplicate expanded attribute name"));
        }
        budget.aggregate.reserve_vec(&mut output.0, 1)?;
        let raw = budgeted_copy(raw, budget)?;
        record_feed_attribute_construction(attribute_index + 1);
        record_feed_xml_object();
        output.0.push(FeedAttribute { raw, namespace, local, value });
    }
    Ok(output)
}

fn unescape_attribute(
    raw: &str,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<String, ConversionError> {
    let mut output = String::new();
    budget.aggregate.reserve_string_capacity(&mut output, raw.len())?;
    record_feed_xml_string();
    let mut offset = 0;
    while offset < raw.len() {
        if offset.is_multiple_of(1024) {
            context.checkpoint()?;
        }
        let tail = &raw[offset..];
        if let Some(entity) = tail.strip_prefix('&') {
            let end = entity.find(';').ok_or_else(|| malformed("unterminated attribute entity"))?;
            let scalar = super::structured::predefined_or_numeric_entity_scalar(&entity[..end])?;
            output.push(scalar);
            offset = offset
                .checked_add(end + 2)
                .ok_or_else(|| limit("max_memory_bytes", "attribute offset overflowed"))?;
        } else {
            let character = tail.chars().next().expect("non-empty tail");
            output.push(character);
            offset += character.len_utf8();
        }
    }
    Ok(output)
}

fn xml_base(
    attributes: &FeedAttributes,
    inherited: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<Option<String>, ConversionError> {
    let mut local_diagnostics = Vec::new();
    let transaction = budget.aggregate.transaction_snapshot();
    let result = xml_base_inner(attributes, inherited, &mut local_diagnostics, budget, context)
        .and_then(|base| {
            publish_diagnostics(diagnostics, &mut local_diagnostics, budget)?;
            Ok(base)
        });
    match result {
        Ok(base) => Ok(base),
        Err(error) => {
            drop(local_diagnostics);
            budget.aggregate.rewind(transaction)?;
            Err(error)
        }
    }
}

fn xml_base_inner(
    attributes: &FeedAttributes,
    inherited: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<Option<String>, ConversionError> {
    let explicit = attributes
        .0
        .iter()
        .find(|attribute| attribute.expanded_eq(XML_NS, "base"))
        .map(|attribute| attribute.value.as_str());
    let Some(value) = explicit else {
        return inherited.map(|value| budgeted_copy(value, budget)).transpose();
    };
    let workspace = url_workspace_bound(value, inherited)?;
    budget.aggregate.prepay_parser_memory(workspace)?;
    context.checkpoint()?;
    record_feed_url_parse();
    let parsed = Url::parse(value).ok().or_else(|| Url::parse(inherited?).ok()?.join(value).ok());
    let valid = parsed.and_then(super::html::valid_http_base);
    let result = if let Some(base) = valid.as_ref() {
        Some(budgeted_copy(base.as_str(), budget)?)
    } else {
        push_diagnostic_parts(
            diagnostics,
            DiagnosticSeverity::Warning,
            "feed.baseRejected",
            "xml:base was rejected; no network access occurred",
            None,
            budget,
        )?;
        inherited.map(|value| budgeted_copy(value, budget)).transpose()?
    };
    drop(valid);
    budget.aggregate.release_parser_memory(workspace)?;
    Ok(result)
}

fn append_text(
    frame: Option<&mut Frame>,
    value: &str,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    super::structured::validate_xml_chars(value, "feed text")?;
    budget.source_text(value.len())?;
    if let Some(frame) = frame {
        budget.aggregate.reserve_string_capacity(&mut frame.text, value.len())?;
        frame.text.push_str(value);
    }
    Ok(())
}

fn push_diagnostic_parts(
    diagnostics: &mut Vec<Diagnostic>,
    severity: DiagnosticSeverity,
    code: &str,
    message: &str,
    locator: Option<SourceLocator>,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    let transaction = budget.aggregate.transaction_snapshot();
    let mut local = Vec::new();
    let result = (|| {
        budget.aggregate.begin_feed_diagnostic(code.len(), message.len())?;
        budget.aggregate.reserve_vec(&mut local, 1)?;
        let code = budgeted_copy(code, budget)?;
        let message = budgeted_copy(message, budget)?;
        record_feed_xml_object();
        local.push(Diagnostic { code, severity, message, locator });
        publish_diagnostics(diagnostics, &mut local, budget)?;
        Ok(())
    })();
    if let Err(error) = result {
        drop(local);
        budget.aggregate.rewind(transaction)?;
        return Err(error);
    }
    Ok(())
}

/// Publish a fully-built local transaction while leaving the destination
/// byte-for-byte unchanged until every fallible step has succeeded.
///
/// Both the destination and local capacities were reserved through the shared
/// feed budget. A replacement vector is reserved while they remain live, so
/// the lease covers the allocator peak. After the release amount is validated,
/// `swap` and `append` cannot allocate: the replacement already has room for
/// every element. Only then are the two superseded allocations dropped and
/// their exact capacities released. An allocation failure therefore preserves
/// the destination's pointer, length, contents, and capacity for a retry.
fn publish_diagnostics(
    destination: &mut Vec<Diagnostic>,
    local: &mut Vec<Diagnostic>,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    if local.is_empty() {
        return Ok(());
    }
    let total = destination
        .len()
        .checked_add(local.len())
        .ok_or_else(|| limit("max_memory_bytes", "diagnostic publication length overflowed"))?;
    let old_capacity = destination
        .capacity()
        .checked_mul(std::mem::size_of::<Diagnostic>())
        .ok_or_else(|| limit("max_memory_bytes", "diagnostic capacity overflowed"))?;
    let local_capacity = local
        .capacity()
        .checked_mul(std::mem::size_of::<Diagnostic>())
        .ok_or_else(|| limit("max_memory_bytes", "local diagnostic capacity overflowed"))?;
    let released = old_capacity
        .checked_add(local_capacity)
        .ok_or_else(|| limit("max_memory_bytes", "diagnostic release overflowed"))?;

    let allocation = budget.aggregate.transaction_snapshot();
    let mut replacement = Vec::new();
    if let Err(error) = budget.aggregate.reserve_vec(&mut replacement, total) {
        drop(replacement);
        budget.aggregate.rewind(allocation)?;
        return Err(error);
    }
    if let Err(error) = budget.aggregate.validate_temporary_capacity_release(released) {
        drop(replacement);
        budget.aggregate.rewind(allocation)?;
        return Err(error);
    }

    // Publication starts here. The replacement capacity is at least `total`,
    // so these moves cannot allocate or fail.
    record_diagnostic_publication_swap();
    std::mem::swap(destination, &mut replacement);
    destination.append(&mut replacement);
    destination.append(local);
    drop(replacement);
    let empty_local = std::mem::take(local);
    drop(empty_local);
    budget.aggregate.release_temporary_capacity(released)?;
    Ok(())
}

fn is_atom_text_construct_name(local: &str) -> bool {
    matches!(local, "title" | "subtitle" | "summary" | "content")
}

fn is_entry_start(root: Option<FeedKind>, stack: &[Frame], local: &str, namespace: &str) -> bool {
    match root {
        Some(FeedKind::Atom) => atom_feed_path(stack) && local == "entry" && namespace == ATOM_NS,
        Some(FeedKind::Rss) => rss_channel_path(stack) && local == "item" && namespace.is_empty(),
        None => false,
    }
}

fn path_is(stack: &[Frame], path: &[(&str, &str)]) -> bool {
    stack.len() == path.len()
        && stack.iter().zip(path).all(|(frame, (namespace, local))| {
            frame.namespace == *namespace && frame.local == *local
        })
}

fn atom_feed_path(stack: &[Frame]) -> bool {
    path_is(stack, &[(ATOM_NS, "feed")])
}

fn atom_entry_path(stack: &[Frame]) -> bool {
    path_is(stack, &[(ATOM_NS, "feed"), (ATOM_NS, "entry")])
}

fn atom_feed_author_path(stack: &[Frame]) -> bool {
    path_is(stack, &[(ATOM_NS, "feed"), (ATOM_NS, "author")])
}

fn atom_entry_author_path(stack: &[Frame]) -> bool {
    path_is(stack, &[(ATOM_NS, "feed"), (ATOM_NS, "entry"), (ATOM_NS, "author")])
}

fn rss_channel_path(stack: &[Frame]) -> bool {
    path_is(stack, &[("", "rss"), ("", "channel")])
}

fn rss_item_path(stack: &[Frame]) -> bool {
    path_is(stack, &[("", "rss"), ("", "channel"), ("", "item")])
}

fn atom_text_construct_path(root: Option<FeedKind>, stack: &[Frame]) -> bool {
    if root != Some(FeedKind::Atom) {
        return false;
    }
    match stack {
        [feed, construct] => {
            feed.namespace == ATOM_NS
                && feed.local == "feed"
                && construct.namespace == ATOM_NS
                && matches!(construct.local.as_str(), "title" | "subtitle")
        }
        [feed, entry, construct] => {
            feed.namespace == ATOM_NS
                && feed.local == "feed"
                && entry.namespace == ATOM_NS
                && entry.local == "entry"
                && construct.namespace == ATOM_NS
                && matches!(construct.local.as_str(), "title" | "summary" | "content")
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn close_frame(
    stack: &mut Vec<Frame>,
    root: Option<FeedKind>,
    current: &mut Option<Entry>,
    feed_title: &mut Option<Content>,
    feed_subtitle: &mut Option<Content>,
    feed_author: &mut Option<String>,
    feed_link: &mut Option<LinkCandidate>,
    feed_updated: &mut Option<String>,
    entries: &mut Vec<Entry>,
    decoded: &DecodedText,
    _event_start: usize,
    event_end: usize,
    _options: &ConversionOptions,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut frame = stack.pop().ok_or_else(|| malformed("closing tag has no open element"))?;
    let in_entry = current.is_some();
    if let Some(entry) = current.as_mut() {
        match root {
            Some(FeedKind::Rss) if rss_item_path(stack) => {
                match (frame.namespace.as_str(), frame.local.as_str()) {
                    ("", "title") => set_first_content(
                        &mut entry.title,
                        std::mem::take(&mut frame.text),
                        ContentType::Text,
                        frame.base.clone(),
                    ),
                    ("", "link") => {
                        let text = normalize_bounded(&frame.text, budget, context)?;
                        select_link(
                            &mut entry.link,
                            text.as_str(),
                            frame.base.as_deref(),
                            0,
                            diagnostics,
                            budget,
                        )?;
                    }
                    ("", "guid") => {
                        set_first_normalized(&mut entry.id, &frame.text, budget, context)?;
                    }
                    ("", "author") | (DC_NS, "creator") => {
                        set_first_normalized(&mut entry.author, &frame.text, budget, context)?;
                    }
                    ("", "pubDate") => {
                        set_first_normalized(&mut entry.published, &frame.text, budget, context)?;
                    }
                    ("", "description") => {
                        set_first_content(
                            &mut entry.summary,
                            std::mem::take(&mut frame.text),
                            ContentType::Html,
                            frame.base.clone(),
                        );
                    }
                    (CONTENT_NS, "encoded") => {
                        set_first_content(
                            &mut entry.content,
                            std::mem::take(&mut frame.text),
                            ContentType::Html,
                            frame.base.clone(),
                        );
                    }
                    _ => {}
                }
            }
            Some(FeedKind::Atom) if atom_entry_path(stack) => {
                match (frame.namespace.as_str(), frame.local.as_str()) {
                    (ATOM_NS, "title") => {
                        set_first_atom_content(
                            &mut entry.title,
                            frame,
                            decoded,
                            budget,
                            context,
                            diagnostics,
                        )?;
                        return Ok(());
                    }
                    (ATOM_NS, "id") => {
                        set_first_normalized(&mut entry.id, &frame.text, budget, context)?;
                    }
                    (ATOM_NS, "published") => {
                        set_first_normalized(&mut entry.published, &frame.text, budget, context)?;
                    }
                    (ATOM_NS, "updated") => {
                        set_first_normalized(&mut entry.updated, &frame.text, budget, context)?;
                    }
                    (ATOM_NS, "summary") => {
                        set_first_atom_content(
                            &mut entry.summary,
                            frame,
                            decoded,
                            budget,
                            context,
                            diagnostics,
                        )?;
                        return Ok(());
                    }
                    (ATOM_NS, "content") => {
                        set_first_atom_content(
                            &mut entry.content,
                            frame,
                            decoded,
                            budget,
                            context,
                            diagnostics,
                        )?;
                        return Ok(());
                    }
                    (ATOM_NS, "link") => {
                        select_atom_link(&mut entry.link, &frame, diagnostics, budget)?;
                    }
                    _ => {}
                }
            }
            Some(FeedKind::Atom)
                if frame.namespace == ATOM_NS
                    && frame.local == "name"
                    && atom_entry_author_path(stack) =>
            {
                set_first_normalized(&mut entry.author, &frame.text, budget, context)?;
            }
            _ => {}
        }
    }
    if !in_entry {
        match root {
            Some(FeedKind::Rss) if rss_channel_path(stack) => {
                match (frame.namespace.as_str(), frame.local.as_str()) {
                    ("", "title") => set_first_content(
                        feed_title,
                        std::mem::take(&mut frame.text),
                        ContentType::Text,
                        frame.base.clone(),
                    ),
                    ("", "link") => {
                        let text = normalize_bounded(&frame.text, budget, context)?;
                        select_link(
                            feed_link,
                            text.as_str(),
                            frame.base.as_deref(),
                            0,
                            diagnostics,
                            budget,
                        )?;
                    }
                    ("", "pubDate" | "lastBuildDate") => {
                        set_first_normalized(feed_updated, &frame.text, budget, context)?;
                    }
                    (DC_NS, "creator") => {
                        set_first_normalized(feed_author, &frame.text, budget, context)?;
                    }
                    _ => {}
                }
            }
            Some(FeedKind::Atom) if atom_feed_path(stack) => {
                match (frame.namespace.as_str(), frame.local.as_str()) {
                    (ATOM_NS, "title") => {
                        set_first_atom_content(
                            feed_title,
                            frame,
                            decoded,
                            budget,
                            context,
                            diagnostics,
                        )?;
                        return Ok(());
                    }
                    (ATOM_NS, "subtitle") => {
                        set_first_atom_content(
                            feed_subtitle,
                            frame,
                            decoded,
                            budget,
                            context,
                            diagnostics,
                        )?;
                        return Ok(());
                    }
                    (ATOM_NS, "updated") => {
                        set_first_normalized(feed_updated, &frame.text, budget, context)?;
                    }
                    (ATOM_NS, "link") => {
                        select_atom_link(feed_link, &frame, diagnostics, budget)?;
                    }
                    _ => {}
                }
            }
            Some(FeedKind::Atom)
                if frame.namespace == ATOM_NS
                    && frame.local == "name"
                    && atom_feed_author_path(stack) =>
            {
                set_first_normalized(feed_author, &frame.text, budget, context)?;
            }
            _ => {}
        }
    }
    let entry_closed = match root {
        Some(FeedKind::Rss) => {
            frame.namespace.is_empty() && frame.local == "item" && rss_channel_path(stack)
        }
        Some(FeedKind::Atom) => {
            frame.namespace == ATOM_NS && frame.local == "entry" && atom_feed_path(stack)
        }
        None => false,
    };
    if entry_closed {
        let mut entry = current.take().ok_or_else(|| malformed("entry state is missing"))?;
        entry.end = decoded.source_range(event_end, event_end).1;
        budget.aggregate.reserve_vec(entries, 1)?;
        entries.push(entry);
    }
    Ok(())
}

fn atom_content(
    mut frame: Frame,
    decoded: &DecodedText,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<Content>, ConversionError> {
    let kind = match frame.attrs.get("type").map_or("text", String::as_str) {
        "text" => ContentType::Text,
        "html" => ContentType::Html,
        "xhtml" => ContentType::Xhtml,
        other => {
            return Err(malformed(format!("unsupported Atom text construct type {other:?}")));
        }
    };
    let value = if kind == ContentType::Xhtml {
        let start = frame
            .xhtml_div_start
            .ok_or_else(|| malformed("Atom type=xhtml content is missing its XHTML div"))?;
        let end = frame
            .xhtml_div_end
            .ok_or_else(|| malformed("Atom type=xhtml content has an incomplete XHTML div"))?;
        if !frame.text.chars().all(char::is_whitespace) {
            return Err(malformed("Atom type=xhtml text construct may only contain its XHTML div"));
        }
        let fragment = decoded.text.get(start..end).ok_or_else(|| ConversionError::Internal {
            detail: "Atom XHTML source range is invalid".into(),
        })?;
        serialize_xhtml(fragment, frame.base.as_deref(), budget, context, diagnostics)?
    } else {
        std::mem::take(&mut frame.text)
    };
    Ok(nonempty_content(value, kind, frame.base.clone()))
}

fn nonempty_content(value: String, kind: ContentType, base: Option<String>) -> Option<Content> {
    (kind == ContentType::Xhtml || !value.trim().is_empty()).then_some(Content {
        value,
        kind,
        base,
    })
}

fn set_first_content(
    slot: &mut Option<Content>,
    value: String,
    kind: ContentType,
    base: Option<String>,
) {
    if slot.is_none() {
        *slot = nonempty_content(value, kind, base);
    }
}

fn set_first_atom_content(
    slot: &mut Option<Content>,
    frame: Frame,
    decoded: &DecodedText,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), ConversionError> {
    if slot.is_none() {
        *slot = atom_content(frame, decoded, budget, context, diagnostics)?;
    }
    Ok(())
}

fn set_first_normalized(
    slot: &mut Option<String>,
    value: &str,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if slot.is_none() {
        let value = normalize_bounded(value, budget, context)?;
        if !value.is_empty() {
            *slot = Some(value);
        }
    }
    Ok(())
}

fn normalize_bounded(
    value: &str,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<String, ConversionError> {
    let mut output = String::new();
    budget.aggregate.reserve_string_capacity(&mut output, value.len())?;
    let mut pending_space = false;
    for (index, character) in value.char_indices() {
        if index % 4096 == 0 {
            context.checkpoint()?;
        }
        if character.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(character);
        }
    }
    Ok(output)
}

fn select_atom_link(
    slot: &mut Option<LinkCandidate>,
    frame: &Frame,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    if frame.attrs.get("rel").map_or("alternate", String::as_str) != "alternate" {
        return Ok(());
    }
    let rank = match frame.attrs.get("type").map(String::as_str) {
        Some(value)
            if value.eq_ignore_ascii_case("text/html")
                || value.eq_ignore_ascii_case("application/xhtml+xml") =>
        {
            0
        }
        None | Some("") => 1,
        Some(value) if value.starts_with("text/") => 2,
        Some(_) => 3,
    };
    if slot.as_ref().is_some_and(|candidate| candidate.rank <= rank) {
        return Ok(());
    }
    let Some(href) = frame.attrs.get("href") else { return Ok(()) };
    select_link(slot, href, frame.base.as_deref(), rank, diagnostics, budget)
}

fn select_link(
    slot: &mut Option<LinkCandidate>,
    value: &str,
    base: Option<&str>,
    rank: u8,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    let Some(target) = canonical_link(value, base, diagnostics, budget)? else {
        return Ok(());
    };
    if slot.as_ref().is_none_or(|candidate| rank < candidate.rank) {
        *slot = Some(LinkCandidate { rank, target });
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn serialize_xhtml(
    fragment: &str,
    inherited_base: Option<&str>,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<String, ConversionError> {
    let transaction = budget.aggregate.transaction_snapshot();
    let mut local_diagnostics = Vec::new();
    match serialize_xhtml_inner(fragment, inherited_base, budget, context, &mut local_diagnostics) {
        Ok(output) => {
            if let Err(error) = publish_diagnostics(diagnostics, &mut local_diagnostics, budget) {
                drop(output);
                drop(local_diagnostics);
                budget.aggregate.rewind(transaction)?;
                return Err(error);
            }
            Ok(output)
        }
        Err(error) => {
            drop(local_diagnostics);
            budget.aggregate.rewind(transaction)?;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn serialize_xhtml_inner(
    fragment: &str,
    inherited_base: Option<&str>,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<String, ConversionError> {
    let mut reader = NsReader::from_str(fragment);
    reader.config_mut().check_end_names = true;
    let mut output = String::new();
    let mut bases = Vec::new();
    budget.aggregate.reserve_vec(&mut bases, 1)?;
    bases.push(inherited_base.map(|base| budgeted_copy(base, budget)).transpose()?);
    let mut events = 0_usize;
    loop {
        if events.is_multiple_of(256) {
            context.checkpoint()?;
        }
        events = events
            .checked_add(1)
            .ok_or_else(|| limit("feed_events", "XHTML event count overflowed"))?;
        budget.event()?;
        let event = reader
            .read_event()
            .map_err(|error| malformed(format!("invalid Atom XHTML fragment: {error}")))?;
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let (namespace, local) = resolved_name_bounded(&reader, &element, budget)?;
                if namespace != XHTML_NS {
                    return Err(malformed(format!(
                        "Atom XHTML contains foreign element {{{namespace}}}{local}"
                    )));
                }
                let inherited = bases.last().and_then(|value| value.as_deref());
                let mut attributes = attributes(&reader, &element, budget, context)?;
                let base = xml_base(&attributes, inherited, diagnostics, budget, context)?;
                append_xhtml_raw(&mut output, "<", budget)?;
                append_xhtml_raw(&mut output, &local, budget)?;
                for attribute in &mut attributes.0 {
                    if attribute.raw == "xmlns" || attribute.raw.starts_with("xmlns:") {
                        continue;
                    }
                    if attribute.expanded_eq(XML_NS, "base") {
                        continue;
                    }
                    if !attribute.namespace.is_empty() {
                        continue;
                    }
                    if matches!(attribute.local.as_str(), "href" | "src") {
                        let Some(resolved) =
                            canonical_link(&attribute.value, base.as_deref(), diagnostics, budget)?
                        else {
                            continue;
                        };
                        let old = std::mem::replace(&mut attribute.value, resolved);
                        let old_capacity = old.capacity();
                        drop(old);
                        budget.aggregate.release_temporary_capacity(old_capacity)?;
                    }
                    append_xhtml_raw(&mut output, " ", budget)?;
                    append_xhtml_raw(&mut output, &attribute.local, budget)?;
                    append_xhtml_raw(&mut output, "=\"", budget)?;
                    append_xhtml_escaped(&mut output, &attribute.value, true, budget, context)?;
                    append_xhtml_raw(&mut output, "\"", budget)?;
                }
                append_xhtml_raw(&mut output, if empty { "/>" } else { ">" }, budget)?;
                if !empty {
                    budget.aggregate.reserve_vec(&mut bases, 1)?;
                    bases.push(base);
                } else if let Some(base) = base {
                    let capacity = base.capacity();
                    drop(base);
                    budget.aggregate.release_temporary_capacity(capacity)?;
                }
                let temporary = namespace
                    .capacity()
                    .checked_add(local.capacity())
                    .and_then(|bytes| bytes.checked_add(attribute_capacity(&attributes)))
                    .ok_or_else(|| {
                        limit("max_memory_bytes", "XHTML temporary memory overflowed")
                    })?;
                drop(attributes);
                drop(namespace);
                drop(local);
                budget.aggregate.release_temporary_capacity(temporary)?;
            }
            Event::End(element) => {
                if let Some(base) =
                    bases.pop().ok_or_else(|| malformed("Atom XHTML base stack underflowed"))?
                {
                    let capacity = base.capacity();
                    drop(base);
                    budget.aggregate.release_temporary_capacity(capacity)?;
                }
                let (_, local) = reader.resolve_element(element.name());
                let local = std::str::from_utf8(local.as_ref())
                    .map_err(|_| malformed("XHTML end name is not UTF-8"))?;
                append_xhtml_raw(&mut output, "</", budget)?;
                append_xhtml_raw(&mut output, local, budget)?;
                append_xhtml_raw(&mut output, ">", budget)?;
            }
            Event::Text(value) => {
                let value = std::str::from_utf8(value.as_ref())
                    .map_err(|_| malformed("Atom XHTML text is not UTF-8"))?;
                super::structured::validate_xml_chars(value, "Atom XHTML text")?;
                append_xhtml_raw(&mut output, value, budget)?;
            }
            Event::CData(value) => {
                let value = std::str::from_utf8(value.as_ref())
                    .map_err(|_| malformed("Atom XHTML CDATA is not UTF-8"))?;
                super::structured::validate_xml_chars(value, "Atom XHTML CDATA")?;
                append_xhtml_escaped(&mut output, value, false, budget, context)?;
            }
            Event::GeneralRef(reference) => {
                let raw = std::str::from_utf8(reference.as_ref())
                    .map_err(|_| malformed("Atom XHTML entity is not UTF-8"))?;
                super::structured::predefined_or_numeric_entity_scalar(raw)?;
                append_xhtml_raw(&mut output, "&", budget)?;
                append_xhtml_raw(&mut output, raw, budget)?;
                append_xhtml_raw(&mut output, ";", budget)?;
            }
            Event::Comment(value) => {
                let value = std::str::from_utf8(value.as_ref())
                    .map_err(|_| malformed("Atom XHTML comment is not UTF-8"))?;
                append_xhtml_raw(&mut output, "<!--", budget)?;
                append_xhtml_raw(&mut output, value, budget)?;
                append_xhtml_raw(&mut output, "-->", budget)?;
            }
            Event::DocType(_) | Event::Decl(_) | Event::PI(_) => {
                return Err(malformed("declarations are not allowed inside Atom XHTML"));
            }
            Event::Eof => break,
        }
    }
    if bases.len() != 1 {
        return Err(malformed("Atom XHTML base stack is incomplete"));
    }
    if let Some(base) = bases.pop().flatten() {
        let capacity = base.capacity();
        drop(base);
        budget.aggregate.release_temporary_capacity(capacity)?;
    }
    let bases_capacity = bases
        .capacity()
        .checked_mul(std::mem::size_of::<Option<String>>())
        .ok_or_else(|| limit("max_memory_bytes", "XHTML base vector memory overflowed"))?;
    drop(bases);
    budget.aggregate.release_temporary_capacity(bases_capacity)?;
    Ok(output)
}

fn attribute_capacity(attributes: &FeedAttributes) -> usize {
    attributes.0.iter().fold(
        attributes.0.capacity().saturating_mul(std::mem::size_of::<FeedAttribute>()),
        |total, attribute| {
            total
                .saturating_add(attribute.raw.capacity())
                .saturating_add(attribute.namespace.capacity())
                .saturating_add(attribute.local.capacity())
                .saturating_add(attribute.value.capacity())
        },
    )
}

fn append_xhtml_raw(
    output: &mut String,
    value: &str,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    budget.aggregate.reserve_string_capacity(output, value.len())?;
    record_feed_xhtml_memory(budget.aggregate.snapshot().persistent_memory_bytes);
    record_feed_xhtml_write();
    output.push_str(value);
    Ok(())
}

fn append_xhtml_escaped(
    output: &mut String,
    value: &str,
    attribute: bool,
    budget: &mut FeedBudget,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut escaped_len = 0_usize;
    for (index, character) in value.char_indices() {
        if index.is_multiple_of(1024) {
            context.checkpoint()?;
        }
        let bytes = match character {
            '&' => 5,
            '<' | '>' => 4,
            '"' if attribute => 6,
            _ => character.len_utf8(),
        };
        escaped_len = escaped_len
            .checked_add(bytes)
            .ok_or_else(|| limit("max_memory_bytes", "XHTML escape size overflowed"))?;
    }
    budget.aggregate.reserve_string_capacity(output, escaped_len)?;
    record_feed_xhtml_memory(budget.aggregate.snapshot().persistent_memory_bytes);
    record_feed_xhtml_escape(budget.aggregate.snapshot().persistent_memory_bytes);
    record_feed_xhtml_write();
    for character in value.chars() {
        output.push_str(match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' if attribute => "&quot;",
            _ => {
                output.push(character);
                continue;
            }
        });
    }
    Ok(())
}

fn canonical_link(
    value: &str,
    base: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut FeedBudget,
) -> Result<Option<String>, ConversionError> {
    let transaction = budget.aggregate.transaction_snapshot();
    let mut local_diagnostics = Vec::new();
    let result = (|| {
        let value = value.trim();
        let workspace = url_workspace_bound(value, base)?;
        budget.aggregate.prepay_parser_memory(workspace)?;
        record_feed_url_parse();
        let parsed = Url::parse(value).ok().or_else(|| Url::parse(base?).ok()?.join(value).ok());
        let valid = parsed.filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && !url.as_str().chars().any(char::is_control)
        });
        let output = if let Some(url) = valid.as_ref() {
            Some(budgeted_copy(url.as_str(), budget)?)
        } else {
            push_diagnostic_parts(
                &mut local_diagnostics,
                DiagnosticSeverity::Warning,
                "feed.linkRejected",
                "unsafe or unresolved feed link was omitted; no network access occurred",
                None,
                budget,
            )?;
            None
        };
        drop(valid);
        budget.aggregate.release_parser_memory(workspace)?;
        publish_diagnostics(diagnostics, &mut local_diagnostics, budget)?;
        Ok(output)
    })();
    match result {
        Ok(output) => Ok(output),
        Err(error) => {
            drop(local_diagnostics);
            budget.aggregate.rewind(transaction)?;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn build_output(
    mut parsed: ParsedFeed,
    options: &ConversionOptions,
    exec_context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    let title_content = parsed.title.take();
    let metadata_title = match title_content.as_ref() {
        Some(content) if content.kind == ContentType::Text => {
            Some(normalize_bounded(&content.value, &mut parsed.budget, exec_context)?)
        }
        _ => None,
    };
    let mut authors = Vec::new();
    if let Some(author) = parsed.author.take() {
        parsed.budget.aggregate.reserve_vec(&mut authors, 1)?;
        authors.push(author);
    }
    let mut document = Document {
        metadata: DocumentMetadata {
            title: metadata_title,
            authors,
            ..DocumentMetadata::default()
        },
        ..Document::default()
    };
    push_diagnostic_parts(
        &mut parsed.diagnostics,
        DiagnosticSeverity::Info,
        "feed.sourceOrder",
        "entries preserve source order; timestamps do not reorder the feed",
        None,
        &mut parsed.budget,
    )?;
    let mut seen = Vec::new();
    let mut assets = Vec::new();
    let mut node_index = 0_usize;
    let feed_locator = Entry {
        start: 0,
        end: parsed.decoded.source_range(parsed.decoded.text.len(), parsed.decoded.text.len()).1,
        ..Entry::default()
    };
    if let Some(link) = parsed.link.take() {
        push_labeled(
            &mut document.blocks,
            &mut node_index,
            "Feed link",
            &link.target,
            &feed_locator,
            &mut parsed.budget,
        )?;
    }
    if let Some(value) = parsed.updated.take() {
        match parse_time(parsed.kind, &value) {
            Ok(time) => push_labeled(
                &mut document.blocks,
                &mut node_index,
                "Feed updated",
                &time,
                &feed_locator,
                &mut parsed.budget,
            )?,
            Err(detail) => {
                push_diagnostic_parts(
                    &mut parsed.diagnostics,
                    DiagnosticSeverity::Warning,
                    "feed.timeInvalid",
                    &detail,
                    None,
                    &mut parsed.budget,
                )?;
                push_labeled(
                    &mut document.blocks,
                    &mut node_index,
                    "Feed updated",
                    &value,
                    &feed_locator,
                    &mut parsed.budget,
                )?;
            }
        }
    }
    if let Some(content) = title_content.filter(|content| content.kind != ContentType::Text) {
        append_content(
            "Title",
            content,
            &feed_locator,
            options,
            exec_context,
            &mut document.blocks,
            &mut assets,
            &mut parsed.diagnostics,
            &mut node_index,
            &mut parsed.budget,
        )?;
    }
    if let Some(subtitle) = parsed.subtitle.take() {
        append_content(
            "Subtitle",
            subtitle,
            &feed_locator,
            options,
            exec_context,
            &mut document.blocks,
            &mut assets,
            &mut parsed.diagnostics,
            &mut node_index,
            &mut parsed.budget,
        )?;
    }
    for (source_index, mut entry) in parsed.entries.into_iter().enumerate() {
        exec_context.checkpoint()?;
        let key = dedup_key(&entry, &mut parsed.budget)?;
        if seen.iter().any(|existing| existing == &key) {
            let duplicate_kind = key.split_once(':').map_or(key.as_str(), |(kind, _)| kind);
            let mut message = String::new();
            let message_capacity = 48_usize
                .checked_add(duplicate_kind.len())
                .ok_or_else(|| limit("max_memory_bytes", "duplicate diagnostic overflowed"))?;
            parsed.budget.aggregate.reserve_string_capacity(&mut message, message_capacity)?;
            write!(
                message,
                "entry {} was omitted by deterministic key {}",
                source_index + 1,
                duplicate_kind
            )
            .map_err(|_| ConversionError::Internal {
                detail: "duplicate diagnostic formatting failed".into(),
            })?;
            push_diagnostic_parts(
                &mut parsed.diagnostics,
                DiagnosticSeverity::Warning,
                "feed.duplicateEntry",
                &message,
                Some(entry_locator(&entry)),
                &mut parsed.budget,
            )?;
            let message_capacity = message.capacity();
            drop(message);
            parsed.budget.aggregate.release_temporary_capacity(message_capacity)?;
            let key_capacity = key.capacity();
            drop(key);
            parsed.budget.aggregate.release_temporary_capacity(key_capacity)?;
            continue;
        }
        parsed.budget.aggregate.reserve_vec(&mut seen, 1)?;
        seen.push(key);
        let title_content = entry.title.take();
        let title = match title_content.as_ref() {
            Some(content) if content.kind == ContentType::Text => {
                normalize_bounded(&content.value, &mut parsed.budget, exec_context)?
            }
            _ => budgeted_numbered("Entry ", source_index + 1, 1, &mut parsed.budget)?,
        };
        push_node(
            &mut document.blocks,
            &mut node_index,
            Block::Heading { level: 2, content: vec![text_inline(title)] },
            &entry,
            PROVIDER_ID,
            &mut parsed.budget,
        )?;
        if let Some(content) = title_content.filter(|content| content.kind != ContentType::Text) {
            append_content(
                "Title",
                content,
                &entry,
                options,
                exec_context,
                &mut document.blocks,
                &mut assets,
                &mut parsed.diagnostics,
                &mut node_index,
                &mut parsed.budget,
            )?;
        }
        if let Some(author) = entry.author.take() {
            push_labeled(
                &mut document.blocks,
                &mut node_index,
                "Author",
                &author,
                &entry,
                &mut parsed.budget,
            )?;
        }
        for (label, raw) in
            [("Published", entry.published.take()), ("Updated", entry.updated.take())]
        {
            if let Some(raw) = raw {
                match parse_time(parsed.kind, &raw) {
                    Ok(value) => {
                        push_labeled(
                            &mut document.blocks,
                            &mut node_index,
                            label,
                            &value,
                            &entry,
                            &mut parsed.budget,
                        )?;
                    }
                    Err(detail) => {
                        push_diagnostic_parts(
                            &mut parsed.diagnostics,
                            DiagnosticSeverity::Warning,
                            "feed.timeInvalid",
                            &detail,
                            Some(entry_locator(&entry)),
                            &mut parsed.budget,
                        )?;
                        push_labeled(
                            &mut document.blocks,
                            &mut node_index,
                            label,
                            &raw,
                            &entry,
                            &mut parsed.budget,
                        )?;
                    }
                }
            }
        }
        if let Some(link) = entry.link.take() {
            push_node(
                &mut document.blocks,
                &mut node_index,
                Block::Paragraph(vec![
                    Inline::Text { value: "Link: ".into(), marks: Vec::new() },
                    Inline::Link {
                        target: link.target,
                        content: vec![text_inline("Open entry".into())],
                    },
                ]),
                &entry,
                PROVIDER_ID,
                &mut parsed.budget,
            )?;
        }
        if let Some(summary) = entry.summary.take() {
            append_content(
                "Summary",
                summary,
                &entry,
                options,
                exec_context,
                &mut document.blocks,
                &mut assets,
                &mut parsed.diagnostics,
                &mut node_index,
                &mut parsed.budget,
            )?;
        }
        if let Some(content) = entry.content.take() {
            append_content(
                "Content",
                content,
                &entry,
                options,
                exec_context,
                &mut document.blocks,
                &mut assets,
                &mut parsed.diagnostics,
                &mut node_index,
                &mut parsed.budget,
            )?;
        }
    }
    document.validate().map_err(|error| {
        let detail = format!("feed IR invalid at {}: {}", error.path, error.detail);
        if error.code == IrErrorCode::ResourceLimit {
            ConversionError::ResourceLimit { limit: "feed_ir", detail }
        } else {
            malformed(detail)
        }
    })?;
    drop(parsed.budget);
    drop(parsed.decoded);
    Ok(ConverterOutput { document, assets, diagnostics: parsed.diagnostics })
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_lines)]
fn append_content(
    label: &str,
    content: Content,
    entry: &Entry,
    options: &ConversionOptions,
    exec_context: &ExecutionContext,
    blocks: &mut Vec<BlockNode>,
    assets: &mut Vec<Asset>,
    diagnostics: &mut Vec<Diagnostic>,
    node_index: &mut usize,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    push_node(
        blocks,
        node_index,
        Block::Heading { level: 3, content: vec![text_inline(label.into())] },
        entry,
        PROVIDER_ID,
        budget,
    )?;
    if content.kind == ContentType::Text {
        let value = normalize_bounded(&content.value, budget, exec_context)?;
        push_node(
            blocks,
            node_index,
            Block::Paragraph(vec![text_inline(value)]),
            entry,
            PROVIDER_ID,
            budget,
        )?;
        return Ok(());
    }
    budget.html(content.value.len())?;
    let before = budget.aggregate.snapshot();
    let fragment_transaction = budget.aggregate.transaction_snapshot();
    let output = super::html::convert_feed_html_fragment(
        &content.value,
        content.base.as_deref(),
        options,
        exec_context,
        &mut budget.aggregate,
    );
    let mut output = match output {
        Ok(output) => output,
        Err(ConversionError::Malformed { .. }) => {
            push_diagnostic_parts(
                diagnostics,
                DiagnosticSeverity::Warning,
                "feed.htmlOmitted",
                "nested HTML had no safe semantic content and was omitted",
                Some(entry_locator(entry)),
                budget,
            )?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    verify_html_fragment_accounting(&output, before, budget.aggregate.snapshot())?;
    if output.document.blocks.is_empty() && output.assets.is_empty() {
        drop(output);
        budget.aggregate.rewind(fragment_transaction)?;
        push_diagnostic_parts(
            diagnostics,
            DiagnosticSeverity::Warning,
            "feed.htmlOmitted",
            "nested HTML had no safe semantic content and was omitted",
            Some(entry_locator(entry)),
            budget,
        )?;
        return Ok(());
    }
    for diagnostic in &mut output.diagnostics {
        let old_code_len = diagnostic.code.len();
        let new_code_len = old_code_len.saturating_add("feed.".len());
        let new_code = budgeted_concat("feed.", &diagnostic.code, budget)?;
        replace_counted_string(&mut diagnostic.code, new_code, old_code_len, new_code_len, budget)?;
        diagnostic.locator = Some(entry_locator(entry));
    }
    let asset_offset = assets.len();
    let mut asset_map: Vec<(AssetId, usize)> = Vec::new();
    for (index, asset) in output.assets.iter_mut().enumerate() {
        let number = asset_offset
            .checked_add(index)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| limit("max_memory_bytes", "feed asset index overflowed"))?;
        let new_id = budgeted_numbered("feed-external-", number, 6, budget)?;
        let new_id_len = new_id.len();
        let old_id_len = asset.id.0.len();
        budget.aggregate.replacement_output_growth(old_id_len, new_id_len)?;
        budget.aggregate.validate_temporary_capacity_release(asset.id.0.capacity())?;
        let old = std::mem::replace(&mut asset.id, AssetId(new_id));
        let old_capacity = old.0.capacity();
        budget.aggregate.reserve_vec(&mut asset_map, 1)?;
        asset_map.push((old, index));
        // The old ID remains live in the lookup table until every node has
        // been rewritten, so its capacity is released with that table below.
        debug_assert_eq!(old_capacity, asset_map.last().map_or(0, |item| item.0.0.capacity()));
    }
    for node in &mut output.document.blocks {
        rewrite_node(node, &asset_map, &output.assets, node_index, entry, budget)?;
    }
    let asset_map_capacity = asset_map
        .capacity()
        .checked_mul(std::mem::size_of::<(AssetId, usize)>())
        .and_then(|bytes| {
            asset_map.iter().try_fold(bytes, |total, item| total.checked_add(item.0.0.capacity()))
        })
        .ok_or_else(|| limit("max_memory_bytes", "asset lookup capacity overflowed"))?;
    drop(asset_map);
    budget.aggregate.release_temporary_capacity(asset_map_capacity)?;

    budget.aggregate.reserve_vec(assets, output.assets.len())?;
    let output_asset_capacity = output
        .assets
        .capacity()
        .checked_mul(std::mem::size_of::<Asset>())
        .ok_or_else(|| limit("max_memory_bytes", "nested asset capacity overflowed"))?;
    assets.append(&mut output.assets);
    let old_assets = std::mem::take(&mut output.assets);
    drop(old_assets);
    budget.aggregate.release_temporary_capacity(output_asset_capacity)?;

    budget.aggregate.reserve_vec(blocks, output.document.blocks.len())?;
    let output_block_capacity = output
        .document
        .blocks
        .capacity()
        .checked_mul(std::mem::size_of::<BlockNode>())
        .ok_or_else(|| limit("max_memory_bytes", "nested block capacity overflowed"))?;
    blocks.append(&mut output.document.blocks);
    let old_blocks = std::mem::take(&mut output.document.blocks);
    drop(old_blocks);
    budget.aggregate.release_temporary_capacity(output_block_capacity)?;

    publish_diagnostics(diagnostics, &mut output.diagnostics, budget)?;
    Ok(())
}

fn verify_html_fragment_accounting(
    output: &ConverterOutput,
    before: super::html::FeedHtmlBudgetSnapshot,
    after: super::html::FeedHtmlBudgetSnapshot,
) -> Result<(), ConversionError> {
    let (nodes, inlines, strings, bytes) = inspect_fragment_nodes(&output.document.blocks);
    let mut output_strings = strings;
    let mut output_bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    for asset in &output.assets {
        let (asset_strings, asset_bytes) = inspect_asset_strings(asset);
        output_strings = output_strings.saturating_add(asset_strings);
        output_bytes = output_bytes.saturating_add(u64::try_from(asset_bytes).unwrap_or(u64::MAX));
    }
    for diagnostic in &output.diagnostics {
        output_strings = output_strings.saturating_add(2);
        output_bytes = output_bytes
            .saturating_add(u64::try_from(diagnostic.code.len()).unwrap_or(u64::MAX))
            .saturating_add(u64::try_from(diagnostic.message.len()).unwrap_or(u64::MAX));
    }
    let charged_nodes = after.nodes.checked_sub(before.nodes);
    let charged_inlines = after.inlines.checked_sub(before.inlines);
    let charged_assets = after.assets.checked_sub(before.assets);
    let charged_diagnostics = after.diagnostics.checked_sub(before.diagnostics);
    let charged_strings = after.strings.checked_sub(before.strings);
    let charged_bytes = after.output_bytes.checked_sub(before.output_bytes);
    let charged_memory = after.persistent_memory_bytes.checked_sub(before.persistent_memory_bytes);
    if charged_nodes.is_none_or(|count| count < nodes)
        || charged_inlines.is_none_or(|count| count < inlines)
        || charged_assets.is_none_or(|count| count < output.assets.len())
        || charged_diagnostics.is_none_or(|count| count < output.diagnostics.len())
        || charged_strings.is_none_or(|count| count < output_strings)
        || charged_bytes.is_none_or(|count| count < output_bytes)
        || charged_memory.is_none()
    {
        return Err(ConversionError::Internal {
            detail: "nested HTML returned output that was not precharged to the feed budget".into(),
        });
    }
    Ok(())
}

fn inspect_asset_strings(asset: &Asset) -> (usize, usize) {
    let mut strings = 2_usize;
    let mut bytes = asset.id.0.len().saturating_add(asset.media_type.len());
    if let Some(filename) = &asset.filename {
        strings = strings.saturating_add(1);
        bytes = bytes.saturating_add(filename.len());
    }
    if let Some(uri) = &asset.external_uri {
        strings = strings.saturating_add(1);
        bytes = bytes.saturating_add(uri.len());
    }
    (strings, bytes)
}

fn rewrite_node(
    node: &mut BlockNode,
    map: &[(AssetId, usize)],
    rewritten_assets: &[Asset],
    index: &mut usize,
    entry: &Entry,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    *index += 1;
    let new_id = budgeted_numbered("feed-", *index, 6, budget)?;
    let old_id_len = node.id.0.len();
    let new_id_len = new_id.len();
    replace_counted_string(&mut node.id.0, new_id, old_id_len, new_id_len, budget)?;
    let provider_prefix = budgeted_concat(PROVIDER_ID, "/", budget)?;
    let provider = budgeted_concat(&provider_prefix, &node.provenance.provider, budget)?;
    let provider_prefix_capacity = provider_prefix.capacity();
    drop(provider_prefix);
    budget.aggregate.release_temporary_capacity(provider_prefix_capacity)?;
    let old_provider_len = node.provenance.provider.len();
    let provider_len = provider.len();
    replace_counted_string(
        &mut node.provenance.provider,
        provider,
        old_provider_len,
        provider_len,
        budget,
    )?;
    node.provenance = Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: std::mem::take(&mut node.provenance.provider),
        locator: entry_locator(entry),
        confidence: node.provenance.confidence,
    };
    match &mut node.block {
        Block::Image { asset, .. } => {
            if let Some((_, asset_index)) = map.iter().find(|(old, _)| old == asset) {
                let new = rewritten_assets.get(*asset_index).ok_or_else(|| {
                    ConversionError::Internal {
                        detail: "rewritten feed asset index is missing".into(),
                    }
                })?;
                let replacement = budgeted_copy(&new.id.0, budget)?;
                let old_len = asset.0.len();
                let new_len = replacement.len();
                replace_counted_string(&mut asset.0, replacement, old_len, new_len, budget)?;
            }
        }
        Block::Footnote { blocks: children, .. }
        | Block::Page { blocks: children, .. }
        | Block::Slide { blocks: children, .. }
        | Block::Sheet { blocks: children, .. } => {
            for child in children {
                rewrite_node(child, map, rewritten_assets, index, entry, budget)?;
            }
        }
        Block::List { items, .. } => {
            for item in items {
                for child in &mut item.blocks {
                    rewrite_node(child, map, rewritten_assets, index, entry, budget)?;
                }
            }
        }
        Block::Table { rows, .. } => {
            for row in rows {
                for cell in &mut row.cells {
                    for child in &mut cell.blocks {
                        rewrite_node(child, map, rewritten_assets, index, entry, budget)?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn inspect_fragment_nodes(nodes: &[BlockNode]) -> (usize, usize, usize, usize) {
    let mut counts = (0_usize, 0_usize, 0_usize, 0_usize);
    for node in nodes {
        inspect_node(node, &mut counts);
    }
    counts
}

fn inspect_node(node: &BlockNode, counts: &mut (usize, usize, usize, usize)) {
    counts.0 = counts.0.saturating_add(1);
    counts.2 = counts.2.saturating_add(2);
    counts.3 =
        counts.3.saturating_add(node.id.0.len()).saturating_add(node.provenance.provider.len());
    inspect_block(&node.block, counts);
}

fn inspect_block(block: &Block, counts: &mut (usize, usize, usize, usize)) {
    match block {
        Block::Paragraph(content) | Block::Heading { content, .. } => {
            inspect_inlines(content, counts);
        }
        Block::List { items, .. } => {
            counts.0 = counts.0.saturating_add(items.len());
            for item in items {
                if let Some(marker) = &item.marker_label {
                    counts.2 = counts.2.saturating_add(1);
                    counts.3 = counts.3.saturating_add(marker.len());
                }
                for child in &item.blocks {
                    inspect_node(child, counts);
                }
            }
        }
        Block::Table { rows, .. } => {
            counts.0 = counts.0.saturating_add(rows.len());
            for row in rows {
                counts.0 = counts.0.saturating_add(row.cells.len());
                for cell in &row.cells {
                    for child in &cell.blocks {
                        inspect_node(child, counts);
                    }
                }
            }
        }
        Block::Footnote { label, blocks } => {
            counts.2 = counts.2.saturating_add(1);
            counts.3 = counts.3.saturating_add(label.len());
            for child in blocks {
                inspect_node(child, counts);
            }
        }
        Block::Page { blocks, .. } => {
            for child in blocks {
                inspect_node(child, counts);
            }
        }
        Block::Slide { title, blocks, .. } => {
            if let Some(title) = title {
                counts.2 = counts.2.saturating_add(1);
                counts.3 = counts.3.saturating_add(title.len());
            }
            for child in blocks {
                inspect_node(child, counts);
            }
        }
        Block::Sheet { name, blocks } => {
            counts.2 = counts.2.saturating_add(1);
            counts.3 = counts.3.saturating_add(name.len());
            for child in blocks {
                inspect_node(child, counts);
            }
        }
        Block::Code { language, text } => {
            counts.2 = counts.2.saturating_add(1);
            counts.3 = counts.3.saturating_add(text.len());
            if let Some(language) = language {
                counts.2 = counts.2.saturating_add(1);
                counts.3 = counts.3.saturating_add(language.len());
            }
        }
        Block::Formula(text) => {
            counts.2 = counts.2.saturating_add(1);
            counts.3 = counts.3.saturating_add(text.len());
        }
        Block::Image { asset, alt } => {
            counts.2 = counts.2.saturating_add(1);
            counts.3 = counts.3.saturating_add(asset.0.len());
            if let Some(alt) = alt {
                counts.2 = counts.2.saturating_add(1);
                counts.3 = counts.3.saturating_add(alt.len());
            }
        }
        Block::TimedSegment { speaker, content, .. } => {
            if let Some(speaker) = speaker {
                counts.2 = counts.2.saturating_add(1);
                counts.3 = counts.3.saturating_add(speaker.len());
            }
            inspect_inlines(content, counts);
        }
        _ => {}
    }
}

fn inspect_inlines(inlines: &[Inline], counts: &mut (usize, usize, usize, usize)) {
    counts.1 = counts.1.saturating_add(inlines.len());
    for inline in inlines {
        match inline {
            Inline::Text { value, .. }
            | Inline::Code(value)
            | Inline::Formula(value)
            | Inline::FootnoteReference(value) => {
                counts.2 = counts.2.saturating_add(1);
                counts.3 = counts.3.saturating_add(value.len());
            }
            Inline::Link { target, content } => {
                counts.2 = counts.2.saturating_add(1);
                counts.3 = counts.3.saturating_add(target.len());
                inspect_inlines(content, counts);
            }
            _ => {}
        }
    }
}

fn dedup_key(entry: &Entry, budget: &mut FeedBudget) -> Result<String, ConversionError> {
    if let Some(id) = entry.id.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        return budgeted_concat("id:", id, budget);
    }
    if let Some(link) = &entry.link {
        return budgeted_concat("link:", &link.target, budget);
    }
    let mut digest = Sha256::new();
    for value in [
        entry.title.as_ref().map(|v| v.value.as_str()),
        entry.author.as_deref(),
        entry.published.as_deref(),
        entry.updated.as_deref(),
        entry.summary.as_ref().map(|v| v.value.as_str()),
        entry.content.as_ref().map(|v| v.value.as_str()),
    ] {
        let bytes = value.unwrap_or_default().as_bytes();
        digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(bytes);
    }
    let mut output = String::new();
    budget.aggregate.reserve_string_capacity(&mut output, 71)?;
    record_feed_xml_string();
    output.push_str("digest:");
    for byte in digest.finalize() {
        write!(output, "{byte:02x}").map_err(|_| ConversionError::Internal {
            detail: "feed digest formatting failed".into(),
        })?;
    }
    Ok(output)
}

fn push_labeled(
    blocks: &mut Vec<BlockNode>,
    index: &mut usize,
    label: &str,
    value: &str,
    entry: &Entry,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    let mut text = String::new();
    let length = label
        .len()
        .checked_add(2)
        .and_then(|length| length.checked_add(value.len()))
        .ok_or_else(|| limit("max_memory_bytes", "labeled feed text overflowed"))?;
    budget.aggregate.reserve_string_capacity(&mut text, length)?;
    text.push_str(label);
    text.push_str(": ");
    text.push_str(value);
    push_node(blocks, index, Block::Paragraph(vec![text_inline(text)]), entry, PROVIDER_ID, budget)
}

fn push_node(
    blocks: &mut Vec<BlockNode>,
    index: &mut usize,
    block: Block,
    entry: &Entry,
    provider: &str,
    budget: &mut FeedBudget,
) -> Result<(), ConversionError> {
    let (nodes, inlines, strings, bytes) = inspect_block_value(&block);
    budget.ir(nodes, inlines)?;
    budget.aggregate.strings(strings, bytes)?;
    budget.aggregate.reserve_vec(blocks, 1)?;
    let next =
        index.checked_add(1).ok_or_else(|| limit("feed_nodes", "feed node index overflowed"))?;
    let id = budgeted_numbered("feed-", next, 6, budget)?;
    let provider = budgeted_copy(provider, budget)?;
    budget.aggregate.record_output_strings(2, id.len().saturating_add(provider.len()))?;
    *index = next;
    blocks.push(BlockNode {
        id: NodeId(id),
        block,
        provenance: Provenance {
            kind: ProvenanceKind::NativeParser,
            provider,
            locator: entry_locator(entry),
            confidence: None,
        },
    });
    Ok(())
}

fn inspect_block_value(block: &Block) -> (usize, usize, usize, usize) {
    let mut counts = (1, 0, 0, 0);
    inspect_block(block, &mut counts);
    counts
}

fn entry_locator(entry: &Entry) -> SourceLocator {
    SourceLocator {
        byte_start: u64::try_from(entry.start).ok(),
        byte_end: u64::try_from(entry.end).ok(),
        ..SourceLocator::default()
    }
}

fn text_inline(value: String) -> Inline {
    Inline::Text { value, marks: Vec::new() }
}

fn parse_time(kind: FeedKind, value: &str) -> Result<String, String> {
    match kind {
        FeedKind::Atom => parse_rfc3339(value),
        FeedKind::Rss => parse_rfc822(value),
    }
}

fn parse_rfc3339(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return Err(format!("invalid RFC3339 timestamp {value:?}"));
    }
    let year = i64::from(digits(bytes, 0, 4)?);
    let month = digits(bytes, 5, 2)?;
    let day = digits(bytes, 8, 2)?;
    let hour = digits(bytes, 11, 2)?;
    let minute = digits(bytes, 14, 2)?;
    let second = digits(bytes, 17, 2)?;
    if second == 60 {
        return Err("RFC3339 leap seconds are rejected for deterministic normalization".into());
    }
    validate_datetime(year, month, day, hour, minute, second)?;
    let mut offset = 19;
    let mut fraction = "";
    if bytes.get(offset) == Some(&b'.') {
        let start = offset;
        offset += 1;
        let digits_start = offset;
        while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
            offset += 1;
        }
        if offset == digits_start {
            return Err(format!("invalid RFC3339 fraction {value:?}"));
        }
        fraction = &value[start..offset];
    }
    let zone = bytes.get(offset..).ok_or_else(|| format!("missing RFC3339 zone {value:?}"))?;
    let zone_seconds = if matches!(zone, [b'Z' | b'z']) {
        0
    } else if zone.len() == 6 && matches!(zone[0], b'+' | b'-') && zone[3] == b':' {
        let hours = digits(zone, 1, 2)?;
        let minutes = digits(zone, 4, 2)?;
        if hours > 23 || minutes > 59 {
            return Err(format!("invalid RFC3339 zone {value:?}"));
        }
        let sign = if zone[0] == b'+' { 1 } else { -1 };
        sign * i64::from(hours * 3600 + minutes * 60)
    } else {
        return Err(format!("invalid RFC3339 zone {value:?}"));
    };
    let utc = to_unix(year, month, day, hour, minute, second)? - zone_seconds;
    let (y, m, d, hh, mm, ss) = from_unix(utc)?;
    Ok(format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}{fraction}Z"))
}

fn parse_rfc822(value: &str) -> Result<String, String> {
    if value.contains(['\r', '\n', '\t']) || value.split(' ').any(str::is_empty) {
        return Err(format!("invalid RFC822 folding or whitespace in {value:?}"));
    }
    let mut fields = value.split(' ');
    let first = fields.next().ok_or_else(|| format!("missing RFC822 date in {value:?}"))?;
    let mut declared_weekday = None;
    let day_field = if first.ends_with(',') {
        let weekday = first.trim_end_matches(',');
        if !matches!(weekday, "Mon" | "Tue" | "Wed" | "Thu" | "Fri" | "Sat" | "Sun") {
            return Err(format!("invalid RFC822 weekday in {value:?}"));
        }
        declared_weekday = Some(weekday);
        fields.next().ok_or_else(|| format!("missing RFC822 day in {value:?}"))?
    } else {
        first
    };
    let month_field = fields.next().ok_or_else(|| format!("missing RFC822 month in {value:?}"))?;
    let year_field = fields.next().ok_or_else(|| format!("missing RFC822 year in {value:?}"))?;
    let time_field = fields.next().ok_or_else(|| format!("missing RFC822 time in {value:?}"))?;
    let zone = fields.next().ok_or_else(|| format!("missing RFC822 zone in {value:?}"))?;
    if fields.next().is_some()
        || year_field.len() != 4
        || !(1..=2).contains(&day_field.len())
        || !day_field.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("RFC822 date requires four-digit year and seconds: {value:?}"));
    }
    let day = day_field.parse::<u32>().map_err(|_| format!("invalid RFC822 day {value:?}"))?;
    let month = match month_field {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return Err(format!("invalid RFC822 month {value:?}")),
    };
    let year = year_field.parse::<i64>().map_err(|_| format!("invalid RFC822 year {value:?}"))?;
    let mut time = time_field.split(':');
    let hour_field = time.next().unwrap_or_default();
    let minute_field = time.next().unwrap_or_default();
    let second_field = time.next().unwrap_or_default();
    if time.next().is_some()
        || hour_field.len() != 2
        || minute_field.len() != 2
        || second_field.len() != 2
    {
        return Err(format!("RFC822 time requires seconds: {value:?}"));
    }
    let hour = hour_field.parse::<u32>().map_err(|_| format!("invalid RFC822 hour {value:?}"))?;
    let minute =
        minute_field.parse::<u32>().map_err(|_| format!("invalid RFC822 minute {value:?}"))?;
    let second =
        second_field.parse::<u32>().map_err(|_| format!("invalid RFC822 second {value:?}"))?;
    if second == 60 {
        return Err("RFC822 leap seconds are rejected".into());
    }
    validate_datetime(year, month, day, hour, minute, second)?;
    if let Some(declared) = declared_weekday {
        let names = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let days = to_unix(year, month, day, 0, 0, 0)?.div_euclid(86_400);
        let actual = names
            [usize::try_from((days + 4).rem_euclid(7)).map_err(|_| "weekday conversion failed")?];
        if declared != actual {
            return Err(format!(
                "RFC822 weekday {declared} disagrees with calendar date ({actual})"
            ));
        }
    }
    let zone_seconds = if matches!(zone, "GMT" | "UT") {
        0
    } else {
        let bytes = zone.as_bytes();
        if bytes.len() != 5 || !matches!(bytes[0], b'+' | b'-') {
            return Err(format!("obsolete or invalid RFC822 zone {zone:?}"));
        }
        let hours = digits(bytes, 1, 2)?;
        let minutes = digits(bytes, 3, 2)?;
        if hours > 23 || minutes > 59 {
            return Err(format!("invalid RFC822 zone {zone:?}"));
        }
        (if bytes[0] == b'+' { 1 } else { -1 }) * i64::from(hours * 3600 + minutes * 60)
    };
    let utc = to_unix(year, month, day, hour, minute, second)? - zone_seconds;
    let (y, m, d, hh, mm, ss) = from_unix(utc)?;
    Ok(format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z"))
}

fn digits(bytes: &[u8], start: usize, len: usize) -> Result<u32, String> {
    let value = bytes.get(start..start + len).ok_or_else(|| "truncated date field".to_string())?;
    if !value.iter().all(u8::is_ascii_digit) {
        return Err("non-digit in date field".into());
    }
    value.iter().try_fold(0_u32, |number, digit| {
        number
            .checked_mul(10)
            .and_then(|n| n.checked_add(u32::from(*digit - b'0')))
            .ok_or_else(|| "date field overflow".into())
    })
}

fn validate_datetime(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Result<(), String> {
    if !(0..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err("date-time component is out of range".into());
    }
    Ok(())
}
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}
fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}
fn days_before_year(year: i64) -> i64 {
    let y = year - 1;
    365 * y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)
}
fn to_unix(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Result<i64, String> {
    let before_month: i64 = (1..month).map(|m| i64::from(days_in_month(year, m))).sum();
    let days = days_before_year(year) + before_month + i64::from(day - 1) - days_before_year(1970);
    days.checked_mul(86_400)
        .and_then(|v| v.checked_add(i64::from(hour * 3600 + minute * 60 + second)))
        .ok_or_else(|| "date-time overflow".into())
}
fn from_unix(seconds: i64) -> Result<(i64, u32, u32, u32, u32, u32), String> {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let mut year = 1970 + days.div_euclid(365);
    while days_before_year(year) - days_before_year(1970) > days {
        year -= 1;
    }
    while days_before_year(year + 1) - days_before_year(1970) <= days {
        year += 1;
    }
    if !(0..=9999).contains(&year) {
        return Err("normalized date is out of range".into());
    }
    let mut day = days - (days_before_year(year) - days_before_year(1970));
    let mut month = 1_u32;
    while day >= i64::from(days_in_month(year, month)) {
        day -= i64::from(days_in_month(year, month));
        month += 1;
    }
    Ok((
        year,
        month,
        u32::try_from(day + 1).map_err(|_| "date conversion failed")?,
        u32::try_from(rest / 3600).map_err(|_| "time conversion failed")?,
        u32::try_from(rest % 3600 / 60).map_err(|_| "time conversion failed")?,
        u32::try_from(rest % 60).map_err(|_| "time conversion failed")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        CancellationToken, ErrorCode, ExecutionOptions, ResourceLimits, SourceMetadata,
    };
    use std::sync::Arc;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }
    fn input(value: &str, uri: Option<&str>) -> ResolvedInput {
        ResolvedInput {
            bytes: Arc::from(value.as_bytes()),
            metadata: SourceMetadata {
                uri: uri.map(str::to_owned),
                size: value.len() as u64,
                ..Default::default()
            },
        }
    }
    fn convert(value: &str) -> Result<ConverterOutput, ConversionError> {
        convert_feed(
            &input(value, Some("https://example.com/feed.xml")),
            &ConversionOptions::default(),
            &context(),
        )
    }

    #[test]
    fn allocation_container_audit_stays_fail_closed() {
        let source = include_str!("feed.rs");
        let production = source.split_once("#[cfg(test)]\nmod tests").unwrap().0;
        for banned in [
            concat!("BTree", "Map"),
            concat!("BTree", "Set"),
            concat!("Hash", "Map"),
            concat!("Hash", "Set"),
            concat!(".coll", "ect"),
        ] {
            assert!(!production.contains(banned), "unbudgeted container pattern {banned}");
        }
        for persistent_format in [
            concat!("NodeId(for", "mat!"),
            concat!("AssetId(for", "mat!"),
            concat!("diagnostic.code = for", "mat!"),
            concat!("text_inline(for", "mat!"),
            concat!("Ok(for", "mat!(\"id:"),
            concat!("Ok(for", "mat!(\"link:"),
        ] {
            assert!(
                !production.contains(persistent_format),
                "persistent formatting bypass {persistent_format}"
            );
        }
        assert!(!production.contains(concat!("size_of", "+")));
        assert!(!production.contains(concat!("size_of", ").saturating_add")));
    }

    fn collect_ids(nodes: &[BlockNode], ids: &mut Vec<String>) {
        for node in nodes {
            assert!(!ids.iter().any(|id| id == &node.id.0), "duplicate node id {}", node.id.0);
            ids.push(node.id.0.clone());
            match &node.block {
                Block::List { items, .. } => {
                    for item in items {
                        collect_ids(&item.blocks, ids);
                    }
                }
                Block::Table { rows, .. } => {
                    for row in rows {
                        for cell in &row.cells {
                            collect_ids(&cell.blocks, ids);
                        }
                    }
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => collect_ids(blocks, ids),
                _ => {}
            }
        }
    }

    fn collect_text(nodes: &[BlockNode], output: &mut String) {
        fn inlines(values: &[Inline], output: &mut String) {
            for inline in values {
                match inline {
                    Inline::Text { value, .. } | Inline::Code(value) | Inline::Formula(value) => {
                        output.push_str(value);
                    }
                    Inline::Link { content, .. } => inlines(content, output),
                    _ => {}
                }
            }
        }
        for node in nodes {
            match &node.block {
                Block::Paragraph(values)
                | Block::Heading { content: values, .. }
                | Block::TimedSegment { content: values, .. } => inlines(values, output),
                Block::List { items, .. } => {
                    for item in items {
                        collect_text(&item.blocks, output);
                    }
                }
                Block::Table { rows, .. } => {
                    for row in rows {
                        for cell in &row.cells {
                            collect_text(&cell.blocks, output);
                        }
                    }
                }
                Block::Footnote { blocks, .. }
                | Block::Page { blocks, .. }
                | Block::Slide { blocks, .. }
                | Block::Sheet { blocks, .. } => collect_text(blocks, output),
                Block::Code { text, .. } | Block::Formula(text) => output.push_str(text),
                _ => {}
            }
        }
    }

    #[test]
    fn rss_fixture_converts_html_and_deduplicates_in_source_order() {
        let source = include_str!("../tests/fixtures/feed/rss2.xml");
        let output = convert(source).unwrap();
        assert_eq!(output.document.metadata.title.as_deref(), Some("Example RSS"));
        assert!(output.diagnostics.iter().any(|diagnostic| diagnostic.code == "feed.sourceOrder"));
        assert!(output.document.blocks.iter().any(|node| matches!(&node.block, Block::Heading { content, .. } if matches!(&content[0], Inline::Text { value, .. } if value == "First"))));
        assert!(output.diagnostics.iter().any(|d| d.code == "feed.duplicateEntry"));
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].external_uri.as_deref(), Some("https://example.com/image.png"));
    }

    #[test]
    fn atom_fixture_handles_html_xhtml_and_relative_links() {
        let output = convert(include_str!("../tests/fixtures/feed/atom.xml")).unwrap();
        assert!(output.document.metadata.properties.is_empty());
        assert!(output.document.blocks.iter().any(|node| matches!(&node.block, Block::Paragraph(content) if content.iter().any(|inline| matches!(inline, Inline::Link { target, .. } if target == "https://example.com/base/post?q=1#part")))));
        assert!(output.document.blocks.iter().any(|node| matches!(&node.block, Block::Paragraph(content) if content.iter().any(|inline| matches!(inline, Inline::Text { value, .. } if value.contains("XHTML body"))))));
    }

    #[test]
    fn rejects_namespace_confusion_dtd_and_ordinary_xml() {
        for source in [
            "<feed><entry/></feed>",
            "<rss version='2.0' xmlns='urn:not-rss'><channel/></rss>",
            "<!DOCTYPE rss [<!ENTITY x 'boom'>]><rss version='2.0'><channel><title>&x;</title></channel></rss>",
            "<root><feed xmlns='http://www.w3.org/2005/Atom'/></root>",
        ] {
            assert!(convert(source).is_err());
            assert!(!strong_feed_evidence(source.as_bytes(), &context()).unwrap());
        }
    }

    #[test]
    fn exact_namespaces_and_parent_paths_ignore_spoofed_fields() {
        let atom = r"<feed xmlns='http://www.w3.org/2005/Atom' xmlns:e='urn:evil'>
          <title>Good feed</title><e:entry><title>bad outer</title></e:entry>
          <entry><id>good</id><e:entry><title>bad inner</title></e:entry><title>Good entry</title></entry>
        </feed>";
        let output = convert(atom).unwrap();
        let mut text = String::new();
        collect_text(&output.document.blocks, &mut text);
        assert!(text.contains("Good entry"));
        assert!(!text.contains("bad outer") && !text.contains("bad inner"));

        let rss = r"<rss version='2.0' xmlns:e='urn:evil'><channel><title>Feed</title>
          <e:item><title>bad outer</title></e:item>
          <item><guid>x</guid><e:item><title>bad inner</title></e:item><title>Good item</title></item>
        </channel></rss>";
        let output = convert(rss).unwrap();
        let mut text = String::new();
        collect_text(&output.document.blocks, &mut text);
        assert!(text.contains("Good item"));
        assert!(!text.contains("bad outer") && !text.contains("bad inner"));
    }

    #[test]
    fn html_is_fail_closed_and_nested_ids_are_document_global() {
        let source = r"<rss version='2.0'><channel><title>F</title>
          <item><guid>1</guid><title>A</title><description><![CDATA[
            <script>TOP_SECRET</script><ul><li>one<ul><li>nested</li></ul></li></ul>
          ]]></description></item>
          <item><guid>2</guid><title>B</title><description><![CDATA[
            <style>STYLE_SECRET</style><ul><li>two<ul><li>nested</li></ul></li></ul>
          ]]></description></item>
        </channel></rss>";
        let output = convert(source).unwrap();
        let mut text = String::new();
        collect_text(&output.document.blocks, &mut text);
        assert!(!text.contains("TOP_SECRET") && !text.contains("STYLE_SECRET"));
        let mut ids = Vec::new();
        collect_ids(&output.document.blocks, &mut ids);
        assert!(
            output
                .document
                .blocks
                .iter()
                .all(|node| node.provenance.provider.starts_with(PROVIDER_ID))
        );

        let active_only = convert("<rss version='2.0'><channel><title>F</title><item><guid>x</guid><description><![CDATA[<script>ONLY_SECRET</script>]]></description></item></channel></rss>").unwrap();
        let mut active_text = String::new();
        collect_text(&active_only.document.blocks, &mut active_text);
        assert!(!active_text.contains("ONLY_SECRET"));
        assert!(
            active_only.diagnostics.iter().any(|diagnostic| diagnostic.code == "feed.htmlOmitted")
        );
    }

    #[test]
    fn atom_text_constructs_xml_base_and_alternate_ranking_are_complete() {
        let source = r"<feed xmlns='http://www.w3.org/2005/Atom' xml:base='https://example.com/feed/'>
          <title type='html'>&lt;b&gt;HTML title&lt;/b&gt;&lt;script&gt;TITLE_SECRET&lt;/script&gt;</title>
          <subtitle type='xhtml'><div xmlns='http://www.w3.org/1999/xhtml'/></subtitle>
          <entry><id>1</id><title type='xhtml'><div xmlns='http://www.w3.org/1999/xhtml'><b>X title</b></div></title>
            <link rel='alternate' type='application/pdf' href='wrong.pdf'/>
            <link rel='alternate' type='text/html' href='right.html?q=1#part'/>
            <summary type='html'>&lt;p&gt;HTML summary&lt;/p&gt;&lt;template&gt;SUMMARY_SECRET&lt;/template&gt;</summary>
            <content type='xhtml' xml:base='content/'><div xmlns='http://www.w3.org/1999/xhtml' xml:base='root/'><p xml:base='child/'><a href='post'>Body</a></p></div></content>
          </entry>
        </feed>";
        let output = convert(source).unwrap();
        let mut text = String::new();
        collect_text(&output.document.blocks, &mut text);
        assert!(
            text.contains("HTML title")
                && text.contains("X title")
                && text.contains("HTML summary")
        );
        assert!(!text.contains("TITLE_SECRET") && !text.contains("SUMMARY_SECRET"));
        assert!(output.document.blocks.iter().any(|node| matches!(&node.block,
            Block::Paragraph(values) if values.iter().any(|inline| matches!(inline,
                Inline::Link { target, .. } if target == "https://example.com/feed/content/root/child/post")))));
        assert!(output.document.blocks.iter().any(|node| matches!(&node.block,
            Block::Paragraph(values) if values.iter().any(|inline| matches!(inline,
                Inline::Link { target, .. } if target == "https://example.com/feed/right.html?q=1#part")))));
    }

    #[test]
    fn detector_skips_bounded_comments_and_requires_direct_rss_channel() {
        let comments = "<!--x-->".repeat(33);
        let rss =
            format!("<?xml version='1.0'?>{comments}<rss version='2.0'>{comments}<channel/></rss>");
        assert!(strong_feed_evidence(rss.as_bytes(), &context()).unwrap());
        let large = format!(
            "<rss version='2.0'><channel><description>{}</description></channel></rss>",
            "x".repeat(FEED_DETECTION_BYTE_LIMIT + 1)
        );
        assert!(strong_feed_evidence(large.as_bytes(), &context()).unwrap());
        for source in [
            "<rss version='2.0'><wrapper><channel/></wrapper></rss>",
            "<root><channel/><item/></root>",
            "<rss version='1.0'><channel/></rss>",
        ] {
            assert!(!strong_feed_evidence(source.as_bytes(), &context()).unwrap());
        }
    }

    #[test]
    fn nested_html_limit_is_aggregate_across_fragments() {
        let source = "<rss version='2.0'><channel><title>F</title><item><guid>1</guid><description><![CDATA[<p>123456</p>]]></description></item><item><guid>2</guid><description><![CDATA[<p>abcdef</p>]]></description></item></channel></rss>";
        let mut options = ConversionOptions::default();
        options.limits.max_feed_html_bytes = 20;
        let error = convert_feed(
            &input(source, Some("https://example.com/feed.xml")),
            &options,
            &context(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConversionError::ResourceLimit { limit: "max_feed_html_bytes", .. }
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Keep all aggregate-budget dimensions in one regression.
    fn nested_html_persistent_budget_stops_before_second_fragment_allocation() {
        #[derive(Clone, Copy, Debug)]
        enum Dimension {
            Nodes,
            Inlines,
            Assets,
            Diagnostics,
            Strings,
            OutputBytes,
            Memory,
        }

        let fragment = "<p>one</p><img src='https://example.com/a.png' alt='a'>";
        for dimension in [
            Dimension::Nodes,
            Dimension::Inlines,
            Dimension::Assets,
            Dimension::Diagnostics,
            Dimension::Strings,
            Dimension::OutputBytes,
            Dimension::Memory,
        ] {
            let context = context();
            let options = ConversionOptions::default();
            let mut budget = super::super::html::FeedHtmlBudget::new(
                options.limits.max_feed_text_bytes,
                MAX_FEED_DIAGNOSTICS,
                options.limits.max_memory_bytes,
                &context,
            )
            .unwrap();
            let first = super::super::html::convert_feed_html_fragment(
                fragment,
                Some("https://example.com/feed.xml"),
                &options,
                &context,
                &mut budget,
            )
            .unwrap();
            assert!(!first.document.blocks.is_empty() && !first.assets.is_empty());
            let used = budget.snapshot();
            budget.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
                nodes: if matches!(dimension, Dimension::Nodes) { used.nodes } else { usize::MAX },
                inlines: if matches!(dimension, Dimension::Inlines) {
                    used.inlines
                } else {
                    usize::MAX
                },
                assets: if matches!(dimension, Dimension::Assets) {
                    used.assets
                } else {
                    usize::MAX
                },
                diagnostics: if matches!(dimension, Dimension::Diagnostics) {
                    used.diagnostics
                } else {
                    usize::MAX
                },
                strings: if matches!(dimension, Dimension::Strings) {
                    used.strings
                } else {
                    usize::MAX
                },
                output_bytes: if matches!(dimension, Dimension::OutputBytes) {
                    used.output_bytes
                } else {
                    u64::MAX
                },
                persistent_memory_bytes: if matches!(dimension, Dimension::Memory) {
                    used.persistent_memory_bytes
                } else {
                    usize::MAX
                },
            });
            super::super::html::reset_feed_html_object_count();
            let error = super::super::html::convert_feed_html_fragment(
                fragment,
                Some("https://example.com/feed.xml"),
                &options,
                &context,
                &mut budget,
            )
            .unwrap_err();
            let expected = match dimension {
                Dimension::Nodes => "feed_nodes",
                Dimension::Inlines => "feed_inlines",
                Dimension::Assets => "feed_assets",
                Dimension::Diagnostics => "feed_diagnostics",
                Dimension::Strings => "feed_output_strings",
                Dimension::OutputBytes => "max_feed_text_bytes",
                Dimension::Memory => "max_memory_bytes",
            };
            assert!(
                matches!(&error, ConversionError::ResourceLimit { limit, .. } if *limit == expected),
                "{dimension:?} returned {error:?}"
            );
            let allocated = super::super::html::feed_html_object_count();
            match dimension {
                Dimension::Nodes => assert_eq!(allocated.nodes, 0),
                Dimension::Inlines => assert_eq!(allocated.inlines, 0),
                Dimension::Assets => assert_eq!(allocated.assets, 0),
                Dimension::Diagnostics => assert_eq!(allocated.diagnostics, 0),
                Dimension::Strings | Dimension::OutputBytes | Dimension::Memory => {
                    assert_eq!(allocated.strings, 0);
                    assert_eq!(allocated.diagnostics, 0);
                }
            }
            if matches!(dimension, Dimension::Memory) {
                assert_eq!(allocated, super::super::html::FeedHtmlObjectCounts::default());
            }
        }

        // The measured successful capacity lease is sufficient on a fresh
        // budget: container slots and object quotas are not double charged.
        let context = context();
        let options = ConversionOptions::default();
        let mut measured = super::super::html::FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            MAX_FEED_DIAGNOSTICS,
            options.limits.max_memory_bytes,
            &context,
        )
        .unwrap();
        super::super::html::convert_feed_html_fragment(
            fragment,
            Some("https://example.com/feed.xml"),
            &options,
            &context,
            &mut measured,
        )
        .unwrap();
        let mut exact = measured.snapshot();
        exact.persistent_memory_bytes = exact
            .persistent_memory_bytes
            .checked_add(
                super::super::html::feed_html_parser_memory_bound(fragment, &context).unwrap(),
            )
            .unwrap();
        let mut replay = super::super::html::FeedHtmlBudget::new(
            options.limits.max_feed_text_bytes,
            MAX_FEED_DIAGNOSTICS,
            options.limits.max_memory_bytes,
            &context,
        )
        .unwrap();
        replay.set_test_limits(exact);
        super::super::html::convert_feed_html_fragment(
            fragment,
            Some("https://example.com/feed.xml"),
            &options,
            &context,
            &mut replay,
        )
        .unwrap();
    }

    #[test]
    fn append_content_fails_before_aggregate_second_fragment_node_allocation() {
        let context = context();
        let options = ConversionOptions::default();
        let entry = Entry { start: 10, end: 20, ..Entry::default() };
        let mut blocks = Vec::new();
        let mut assets = Vec::new();
        let mut diagnostics = Vec::new();
        let mut node_index = 0;
        let mut budget = FeedBudget::new(&options, &context).unwrap();
        append_content(
            "Summary",
            Content { value: "<p>first</p>".into(), kind: ContentType::Html, base: None },
            &entry,
            &options,
            &context,
            &mut blocks,
            &mut assets,
            &mut diagnostics,
            &mut node_index,
            &mut budget,
        )
        .unwrap();
        let used = budget.aggregate.snapshot();
        budget.aggregate.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
            nodes: used.nodes + 1,
            inlines: usize::MAX,
            assets: usize::MAX,
            diagnostics: usize::MAX,
            strings: usize::MAX,
            output_bytes: u64::MAX,
            persistent_memory_bytes: usize::MAX,
        });
        super::super::html::reset_feed_html_object_count();
        let error = append_content(
            "Content",
            Content { value: "<p>second</p>".into(), kind: ContentType::Html, base: None },
            &entry,
            &options,
            &context,
            &mut blocks,
            &mut assets,
            &mut diagnostics,
            &mut node_index,
            &mut budget,
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "feed_nodes", .. }));
        assert_eq!(super::super::html::feed_html_object_count().nodes, 0);
    }

    #[test]
    fn time_contract_is_strict_and_normalizes_offsets() {
        assert_eq!(parse_rfc3339("2024-02-29T23:30:00+01:30").unwrap(), "2024-02-29T22:00:00Z");
        assert_eq!(
            parse_rfc822("Thu, 29 Feb 2024 23:30:00 +0130").unwrap(),
            "2024-02-29T22:00:00Z"
        );
        for value in ["2024-01-01T00:00:60Z", "2024-01-01 00:00:00Z"] {
            assert!(parse_rfc3339(value).is_err());
        }
        for value in [
            "01 Jan 24 00:00:00 GMT",
            "01 Jan 2024 00:00 GMT",
            "01 Jan 2024 00:00:00 EST",
            "01  Jan 2024 00:00:00 GMT",
            "001 Jan 2024 00:00:00 GMT",
            "1 Jan 2024 0:00:00 GMT",
            "1 Jan 2024 00:0:00 GMT",
            "1 Jan 2024 00:00:0 GMT",
        ] {
            assert!(parse_rfc822(value).is_err());
        }
    }

    fn thousand_attribute_source() -> String {
        let mut source = String::from("<rss version='2.0'><channel");
        for index in 0..1000 {
            write!(source, " a{index}=''").unwrap();
        }
        source.push_str("/></rss>");
        source
    }

    fn first_channel<'a>(reader: &mut NsReader<&'a [u8]>) -> BytesStart<'a> {
        loop {
            match reader.read_event().unwrap() {
                Event::Start(element) | Event::Empty(element)
                    if element.name().as_ref() == b"channel" =>
                {
                    return element;
                }
                Event::Eof => panic!("channel not found"),
                _ => {}
            }
        }
    }

    #[test]
    fn thousand_attributes_use_exact_shared_capacity_before_construction() {
        let source = thousand_attribute_source();
        let options = ConversionOptions::default();
        let context = context();
        let mut measured = FeedBudget::new(&options, &context).unwrap();
        let mut reader = NsReader::from_str(&source);
        let element = first_channel(&mut reader);
        reset_feed_xml_hooks();
        let measured_attributes = attributes(&reader, &element, &mut measured, &context).unwrap();
        assert_eq!(measured_attributes.0.len(), 1000);
        assert_eq!(feed_xml_hook_counts().0, 1000);
        let exact = measured.aggregate.snapshot();
        assert!(exact.persistent_memory_bytes > 75_000);
        drop(measured_attributes);

        let mut limited = FeedBudget::new(&options, &context).unwrap();
        limited.aggregate.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
            persistent_memory_bytes: exact.persistent_memory_bytes - 1,
            ..exact
        });
        let mut reader = NsReader::from_str(&source);
        let element = first_channel(&mut reader);
        reset_feed_xml_hooks();
        watch_feed_attribute(1000);
        let error = attributes(&reader, &element, &mut limited, &context).err().unwrap();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert!(feed_xml_hook_counts().0 < 1000);
        assert_eq!(target_attribute_constructions(), 0);
        assert_eq!(limited.aggregate.snapshot().persistent_memory_bytes, 0);

        let mut exact_budget = FeedBudget::new(&options, &context).unwrap();
        exact_budget.aggregate.set_test_limits(exact);
        let mut reader = NsReader::from_str(&source);
        let element = first_channel(&mut reader);
        reset_feed_xml_hooks();
        watch_feed_attribute(1000);
        let retried_attributes =
            attributes(&reader, &element, &mut exact_budget, &context).unwrap();
        assert_eq!(retried_attributes.0.len(), 1000);
        assert_eq!(feed_xml_hook_counts().0, 1000);
        assert_eq!(target_attribute_constructions(), 1);
        assert_eq!(
            exact_budget.aggregate.snapshot().persistent_memory_bytes,
            exact.persistent_memory_bytes
        );

        let constrained = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 75_000, ..ResourceLimits::default() },
        );
        let error = parse_feed(&input(&source, None), &options, &constrained).err().unwrap();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
    }

    #[test]
    fn xhtml_cdata_escape_growth_reserves_before_write_and_retries_exactly() {
        let payload = "&<>\"".repeat(4096);
        let fragment = format!("<div xmlns='{XHTML_NS}'><![CDATA[{payload}]]></div>");
        let options = ConversionOptions::default();
        let context = context();
        let mut measured = FeedBudget::new(&options, &context).unwrap();
        reset_feed_xml_hooks();
        let output =
            serialize_xhtml(&fragment, None, &mut measured, &context, &mut Vec::new()).unwrap();
        assert!(output.contains("&amp;&lt;&gt;\""));
        assert_eq!(feed_xml_hook_counts().4, 1);
        let escape_peak = feed_xhtml_escape_peak();
        let exact = measured.aggregate.snapshot();
        let memory_peak = feed_xhtml_memory_peak();
        assert!(escape_peak < memory_peak);

        let mut limited = FeedBudget::new(&options, &context).unwrap();
        limited.aggregate.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
            persistent_memory_bytes: escape_peak - 1,
            ..exact
        });
        reset_feed_xml_hooks();
        let error =
            serialize_xhtml(&fragment, None, &mut limited, &context, &mut Vec::new()).unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert_eq!(feed_xml_hook_counts().4, 0, "{error:?}");
        assert_eq!(limited.aggregate.snapshot().persistent_memory_bytes, 0);

        let mut exact_budget = FeedBudget::new(&options, &context).unwrap();
        exact_budget.aggregate.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
            persistent_memory_bytes: memory_peak,
            ..exact
        });
        reset_feed_xml_hooks();
        serialize_xhtml(&fragment, None, &mut exact_budget, &context, &mut Vec::new()).unwrap();
        assert_eq!(feed_xml_hook_counts().4, 1);
        assert_eq!(
            exact_budget.aggregate.snapshot().persistent_memory_bytes,
            exact.persistent_memory_bytes
        );

        let boundary = format!("<div xmlns='{XHTML_NS}'><![CDATA[]]]]><![CDATA[>]]></div>");
        let mut boundary_budget = FeedBudget::new(&options, &context).unwrap();
        let escaped =
            serialize_xhtml(&boundary, None, &mut boundary_budget, &context, &mut Vec::new())
                .unwrap();
        assert!(escaped.contains("]]&gt;"));
        assert!(!escaped.contains("]]>"));
    }

    #[test]
    fn diagnostic_publication_failure_preserves_external_vector_for_exact_retry() {
        let options = ConversionOptions::default();
        let context = context();
        let mut budget = FeedBudget::new(&options, &context).unwrap();
        let mut external = Vec::new();
        push_diagnostic_parts(
            &mut external,
            DiagnosticSeverity::Info,
            "feed.first",
            "first",
            None,
            &mut budget,
        )
        .unwrap();

        let mut local = Vec::new();
        budget.aggregate.begin_feed_diagnostic("feed.second".len(), "second".len()).unwrap();
        budget.aggregate.reserve_vec(&mut local, 1).unwrap();
        let code = budgeted_copy("feed.second", &mut budget).unwrap();
        let message = budgeted_copy("second", &mut budget).unwrap();
        local.push(Diagnostic {
            code,
            severity: DiagnosticSeverity::Warning,
            message,
            locator: None,
        });

        let pointer = external.as_ptr();
        let len = external.len();
        let capacity = external.capacity();
        let code = external[0].code.clone();
        let message = external[0].message.clone();
        let before = budget.aggregate.snapshot();
        let target_slots = len.checked_add(local.len()).unwrap().max(4);
        let mut allocator_probe: Vec<Diagnostic> = Vec::new();
        allocator_probe.try_reserve_exact(target_slots).unwrap();
        let replacement_bytes =
            allocator_probe.capacity().checked_mul(std::mem::size_of::<Diagnostic>()).unwrap();
        drop(allocator_probe);

        budget.aggregate.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
            nodes: usize::MAX,
            inlines: usize::MAX,
            assets: usize::MAX,
            diagnostics: usize::MAX,
            strings: usize::MAX,
            output_bytes: u64::MAX,
            persistent_memory_bytes: before
                .persistent_memory_bytes
                .checked_add(replacement_bytes)
                .unwrap()
                - 1,
        });
        reset_feed_xml_hooks();
        let error = publish_diagnostics(&mut external, &mut local, &mut budget).unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert_eq!(diagnostic_publication_swaps(), 0);
        assert_eq!(external.as_ptr(), pointer);
        assert_eq!(external.len(), len);
        assert_eq!(external.capacity(), capacity);
        assert_eq!(external[0].code, code);
        assert_eq!(external[0].message, message);
        assert_eq!(local.len(), 1);
        assert_eq!(
            budget.aggregate.snapshot().persistent_memory_bytes,
            before.persistent_memory_bytes
        );

        budget.aggregate.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
            nodes: usize::MAX,
            inlines: usize::MAX,
            assets: usize::MAX,
            diagnostics: usize::MAX,
            strings: usize::MAX,
            output_bytes: u64::MAX,
            persistent_memory_bytes: before
                .persistent_memory_bytes
                .checked_add(replacement_bytes)
                .unwrap(),
        });
        publish_diagnostics(&mut external, &mut local, &mut budget).unwrap();
        assert_eq!(diagnostic_publication_swaps(), 1);
        assert_eq!(external.len(), 2);
        assert!(local.is_empty());
    }

    #[test]
    fn xml_base_and_diagnostic_allocations_cross_real_budget_boundaries() {
        let options = ConversionOptions::default();
        let context = context();
        let xml_base_attributes = FeedAttributes(vec![FeedAttribute {
            raw: "xml:base".into(),
            namespace: XML_NS.into(),
            local: "base".into(),
            value: "https://example.com/a/".into(),
        }]);
        let workspace = url_workspace_bound("https://example.com/a/", None).unwrap();
        let mut limited = FeedBudget::new(&options, &context).unwrap();
        limited.aggregate.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
            nodes: usize::MAX,
            inlines: usize::MAX,
            assets: usize::MAX,
            diagnostics: usize::MAX,
            strings: usize::MAX,
            output_bytes: u64::MAX,
            persistent_memory_bytes: workspace - 1,
        });
        reset_feed_xml_hooks();
        let error = xml_base(&xml_base_attributes, None, &mut Vec::new(), &mut limited, &context)
            .err()
            .unwrap();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert_eq!(feed_xml_hook_counts().2, 0);
        assert_eq!(limited.aggregate.snapshot().persistent_memory_bytes, 0);

        let mut measured = FeedBudget::new(&options, &context).unwrap();
        reset_feed_xml_hooks();
        let base =
            xml_base(&xml_base_attributes, None, &mut Vec::new(), &mut measured, &context).unwrap();
        assert_eq!(base.as_deref(), Some("https://example.com/a/"));
        assert_eq!(feed_xml_hook_counts().2, 1);

        let mut measured = FeedBudget::new(&options, &context).unwrap();
        reset_feed_xml_hooks();
        push_diagnostic_parts(
            &mut Vec::new(),
            DiagnosticSeverity::Warning,
            "feed.test",
            "budgeted diagnostic",
            None,
            &mut measured,
        )
        .unwrap();
        let exact = measured.aggregate.snapshot();
        assert_eq!(feed_xml_hook_counts().0, 1);

        let mut limited = FeedBudget::new(&options, &context).unwrap();
        limited.aggregate.set_test_limits(super::super::html::FeedHtmlBudgetSnapshot {
            persistent_memory_bytes: exact.persistent_memory_bytes - 1,
            ..exact
        });
        reset_feed_xml_hooks();
        let error = push_diagnostic_parts(
            &mut Vec::new(),
            DiagnosticSeverity::Warning,
            "feed.test",
            "budgeted diagnostic",
            None,
            &mut limited,
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
        assert_eq!(feed_xml_hook_counts().0, 0);
        assert_eq!(limited.aggregate.snapshot().persistent_memory_bytes, 0);
    }

    #[test]
    fn limits_are_stable() {
        let mut options = ConversionOptions::default();
        options.limits.max_feed_entries = 0;
        let error = parse_feed(
            &input("<rss version='2.0'><channel><item/></channel></rss>", None),
            &options,
            &context(),
        )
        .err()
        .unwrap();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_feed_entries", .. }));

        let token = CancellationToken::new();
        token.cancel();
        let cancelled = ExecutionContext::new(
            ExecutionOptions { cancellation: token, ..ExecutionOptions::default() },
            ResourceLimits::default(),
        );
        let error = parse_feed(
            &input("<rss version='2.0'><channel/></rss>", None),
            &ConversionOptions::default(),
            &cancelled,
        )
        .err()
        .unwrap();
        assert_eq!(error.code(), ErrorCode::Cancelled);

        let constrained = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 1024, ..ResourceLimits::default() },
        );
        let large = format!(
            "<rss version='2.0'><channel><description>{}</description></channel></rss>",
            "x".repeat(4096)
        );
        let error = parse_feed(&input(&large, None), &ConversionOptions::default(), &constrained)
            .err()
            .unwrap();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_memory_bytes", .. }));
    }
}
