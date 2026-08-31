//! Deterministic JSON and XML conversion into the common document IR.

use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput,
    Document, ExecutionContext, FormatCandidate, Inline, InputFormat, MAX_DOCUMENT_INLINES,
    MAX_DOCUMENT_NODES, NodeId, ProbeOutcome, Provenance, ProvenanceKind, ResolvedInput, Services,
    SourceLocator, TextDecodingMode,
};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;

use super::text::{DecodedText, LogicalMemory};

const FORMATS: &[InputFormat] = &[InputFormat::Json, InputFormat::Xml];
const PROVIDER_ID: &str = "builtin.converter.structured-data";
const CHECKPOINT_BYTES: usize = 4096;

#[cfg(test)]
thread_local! {
    static JSON_STRING_DECODE_INVOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static XML_READ_EVENT_INVOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static XML_SCAN_CHECKPOINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Strict, offline JSON/XML converter.
#[derive(Debug, Default)]
pub struct StructuredDataConverter;

impl Converter for StructuredDataConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn priority(&self) -> i32 {
        200
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
            if !FORMATS.contains(&candidate.format) {
                return Ok(ProbeOutcome::NotApplicable);
            }
            let applicable = match candidate.format {
                InputFormat::Json => json_shape(&input.bytes),
                InputFormat::Xml => xml_shape(&input.bytes),
                _ => false,
            };
            let format_hint = candidate.detector_id == "builtin.detector.hints";
            Ok(if candidate.explicit || format_hint || applicable {
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
        candidate: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move {
            let document = match candidate.format {
                InputFormat::Json => convert_json(&input.bytes, options, context)?,
                InputFormat::Xml => convert_xml(&input.bytes, options, context)?,
                _ => {
                    return Err(ConversionError::Internal {
                        detail: "structured converter received an unsupported format".into(),
                    });
                }
            };
            Ok(ConverterOutput::new(document, Vec::new(), Vec::new()))
        })
    }
}

fn malformed(format: InputFormat, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(format.as_str().into()), detail: detail.into() }
}

fn json_shape(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    bytes
        .iter()
        .copied()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| matches!(b, b'{' | b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n'))
}

fn xml_shape(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'<')
}

struct JsonLexer<'a> {
    bytes: &'a [u8],
    offset: usize,
    base: usize,
    max_string: usize,
    context: &'a ExecutionContext,
    next_checkpoint: usize,
    memory: LogicalMemory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IterState {
    First,
    Key,
    Colon,
    Value,
    CommaOrEnd,
}

enum IterFrame {
    Object {
        state: IterState,
        keys: BTreeSet<String>,
        pending: Option<(String, usize)>,
        heading: usize,
        start: usize,
        has_values: bool,
    },
    Array {
        state: IterState,
        index: usize,
        heading: usize,
        start: usize,
        has_values: bool,
    },
}

enum IterSlot {
    Root,
    Object(String, usize),
    Array(usize),
}

#[allow(clippy::too_many_lines)]
fn convert_json(
    source: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Document, ConversionError> {
    context.checkpoint()?;
    let (bytes, base) =
        source.strip_prefix(&[0xef, 0xbb, 0xbf]).map_or((source, 0), |rest| (rest, 3));
    std::str::from_utf8(bytes)
        .map_err(|error| malformed(InputFormat::Json, format!("JSON must be UTF-8: {error}")))?;
    if super::scan_json(bytes, context)?.status != super::JsonScanStatus::Complete {
        return Err(malformed(InputFormat::Json, "JSON syntax is incomplete or invalid"));
    }
    let mut lexer = JsonLexer {
        bytes,
        offset: 0,
        base,
        max_string: usize::try_from(options.limits.max_field_bytes).unwrap_or(usize::MAX),
        context,
        next_checkpoint: 0,
        memory: LogicalMemory::new(context)?,
    };
    let mut frames = Vec::<IterFrame>::new();
    let mut builder = IrBuilder::new(InputFormat::Json, context)?;
    let mut root_seen = false;
    let mut node_count = 0_usize;
    loop {
        lexer.space()?;
        if frames.is_empty() && root_seen {
            if lexer.offset != bytes.len() {
                return Err(malformed(
                    InputFormat::Json,
                    format!("trailing content at byte {}", lexer.offset + base),
                ));
            }
            break;
        }
        if iter_punctuation(&mut lexer, &mut frames, &mut builder)? {
            continue;
        }
        let slot = iter_slot(&mut frames, root_seen)?;
        if matches!(slot, IterSlot::Root) {
            root_seen = true;
        }
        node_count = node_count.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "json_nodes",
            detail: "JSON node count overflowed".into(),
        })?;
        if node_count > MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "json_nodes",
                detail: format!("JSON exceeds {MAX_DOCUMENT_NODES} nodes"),
            });
        }
        let start = lexer.offset + base;
        match lexer.bytes.get(lexer.offset).copied() {
            Some(b'{' | b'[') => {
                if frames.len() >= usize::from(options.limits.max_nesting_depth) {
                    return Err(ConversionError::ResourceLimit {
                        limit: "json_nesting_depth",
                        detail: format!("JSON exceeds {} levels", options.limits.max_nesting_depth),
                    });
                }
                let object = lexer.bytes[lexer.offset] == b'{';
                lexer.offset += 1;
                let label = iter_label(&slot, &mut lexer.memory)?;
                let source_start = iter_slot_start(&slot, start);
                let heading = if matches!(slot, IterSlot::Root) {
                    builder.heading_index(1, "JSON".into(), start, start + 1)?
                } else {
                    builder.heading_index(
                        u8::try_from((frames.len() + 2).min(6)).unwrap_or(6),
                        label,
                        source_start,
                        start + 1,
                    )?
                };
                lexer.memory.reserve_vec(&mut frames, 1)?;
                frames.push(if object {
                    IterFrame::Object {
                        state: IterState::First,
                        keys: BTreeSet::new(),
                        pending: None,
                        heading,
                        start,
                        has_values: false,
                    }
                } else {
                    IterFrame::Array {
                        state: IterState::First,
                        index: 0,
                        heading,
                        start,
                        has_values: false,
                    }
                });
            }
            Some(b'"') => {
                let decoded = lexer.string()?.0;
                let display_budget = decoded
                    .len()
                    .checked_mul(6)
                    .and_then(|value| value.checked_add(2))
                    .ok_or_else(|| ConversionError::ResourceLimit {
                        limit: "max_memory_bytes",
                        detail: "JSON string display capacity overflowed".into(),
                    })?;
                lexer.memory.charge(display_budget)?;
                let display =
                    serde_json::to_string(&decoded).map_err(|error| ConversionError::Internal {
                        detail: format!("JSON string display encoding failed: {error}"),
                    })?;
                iter_scalar(
                    &slot,
                    display,
                    start,
                    lexer.offset + base,
                    &mut lexer.memory,
                    &mut builder,
                )?;
            }
            Some(b'-' | b'0'..=b'9') => {
                let display = lexer.number()?;
                iter_scalar(
                    &slot,
                    display,
                    start,
                    lexer.offset + base,
                    &mut lexer.memory,
                    &mut builder,
                )?;
            }
            Some(b't') => {
                lexer.literal(b"true")?;
                lexer.memory.charge(4)?;
                iter_scalar(
                    &slot,
                    "true".into(),
                    start,
                    lexer.offset + base,
                    &mut lexer.memory,
                    &mut builder,
                )?;
            }
            Some(b'f') => {
                lexer.literal(b"false")?;
                lexer.memory.charge(5)?;
                iter_scalar(
                    &slot,
                    "false".into(),
                    start,
                    lexer.offset + base,
                    &mut lexer.memory,
                    &mut builder,
                )?;
            }
            Some(b'n') => {
                lexer.literal(b"null")?;
                lexer.memory.charge(4)?;
                iter_scalar(
                    &slot,
                    "null".into(),
                    start,
                    lexer.offset + base,
                    &mut lexer.memory,
                    &mut builder,
                )?;
            }
            _ => {
                return Err(malformed(
                    InputFormat::Json,
                    format!("expected a value at byte {}", lexer.offset + base),
                ));
            }
        }
    }
    builder.finish()
}

fn iter_punctuation(
    lexer: &mut JsonLexer<'_>,
    frames: &mut Vec<IterFrame>,
    builder: &mut IrBuilder,
) -> Result<bool, ConversionError> {
    let Some(frame) = frames.last_mut() else { return Ok(false) };
    match frame {
        IterFrame::Object { state, keys, pending, .. }
            if matches!(*state, IterState::First | IterState::Key) =>
        {
            if *state == IterState::First && lexer.bytes.get(lexer.offset) == Some(&b'}') {
                return iter_close(lexer, frames, builder, true);
            }
            let start = lexer.offset + lexer.base;
            let key = lexer.string()?.0;
            if keys.contains(&key) {
                return Err(malformed(
                    InputFormat::Json,
                    format!("duplicate object key {key:?} at byte {start}"),
                ));
            }
            lexer.memory.charge(key.len().checked_add(size_of::<String>() * 2).ok_or_else(
                || ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "JSON object key memory overflowed".into(),
                },
            )?)?;
            lexer.memory.charge(128)?;
            keys.insert(key.clone());
            *pending = Some((key, start));
            *state = IterState::Colon;
            Ok(true)
        }
        IterFrame::Array { state, .. } if *state == IterState::First => {
            if lexer.bytes.get(lexer.offset) == Some(&b']') {
                return iter_close(lexer, frames, builder, true);
            }
            Ok(false)
        }
        IterFrame::Object { state, .. } if *state == IterState::Colon => {
            lexer.expect(b':')?;
            *state = IterState::Value;
            Ok(true)
        }
        IterFrame::Object { state, .. } if *state == IterState::CommaOrEnd => {
            let close = b'}';
            if lexer.bytes.get(lexer.offset) == Some(&close) {
                return iter_close(lexer, frames, builder, false);
            }
            lexer.expect(b',')?;
            *state = IterState::Key;
            lexer.space()?;
            if lexer.bytes.get(lexer.offset) == Some(&close) {
                return Err(malformed(
                    InputFormat::Json,
                    format!("trailing comma at byte {}", lexer.offset + lexer.base),
                ));
            }
            Ok(true)
        }
        IterFrame::Array { state, .. } if *state == IterState::CommaOrEnd => {
            let close = b']';
            if lexer.bytes.get(lexer.offset) == Some(&close) {
                return iter_close(lexer, frames, builder, false);
            }
            lexer.expect(b',')?;
            *state = IterState::Value;
            lexer.space()?;
            if lexer.bytes.get(lexer.offset) == Some(&close) {
                return Err(malformed(
                    InputFormat::Json,
                    format!("trailing comma at byte {}", lexer.offset + lexer.base),
                ));
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn iter_close(
    lexer: &mut JsonLexer<'_>,
    frames: &mut Vec<IterFrame>,
    builder: &mut IrBuilder,
    empty: bool,
) -> Result<bool, ConversionError> {
    lexer.offset += 1;
    let end = lexer.offset + lexer.base;
    let frame = frames.pop().ok_or_else(|| ConversionError::Internal {
        detail: "JSON container stack underflowed".into(),
    })?;
    let (heading, start, object, has_values) = match frame {
        IterFrame::Object { heading, start, has_values, .. } => (heading, start, true, has_values),
        IterFrame::Array { heading, start, has_values, .. } => (heading, start, false, has_values),
    };
    builder.set_end(heading, end)?;
    if empty && !has_values {
        builder.code(Some("json"), if object { "{}" } else { "[]" }.into(), start, end)?;
    }
    Ok(true)
}

fn iter_slot(frames: &mut [IterFrame], root_seen: bool) -> Result<IterSlot, ConversionError> {
    let Some(frame) = frames.last_mut() else {
        return if root_seen {
            Err(malformed(InputFormat::Json, "multiple root values"))
        } else {
            Ok(IterSlot::Root)
        };
    };
    match frame {
        IterFrame::Object { state, pending, has_values, .. } if *state == IterState::Value => {
            *state = IterState::CommaOrEnd;
            *has_values = true;
            let (key, start) = pending.take().ok_or_else(|| ConversionError::Internal {
                detail: "JSON object value has no pending key".into(),
            })?;
            Ok(IterSlot::Object(key, start))
        }
        IterFrame::Array { state, index, has_values, .. }
            if matches!(*state, IterState::First | IterState::Value) =>
        {
            let current = *index;
            *index = index.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "json_nodes",
                detail: "JSON array index overflowed".into(),
            })?;
            *state = IterState::CommaOrEnd;
            *has_values = true;
            Ok(IterSlot::Array(current))
        }
        _ => Err(malformed(InputFormat::Json, "JSON value appears in an invalid position")),
    }
}

fn iter_scalar(
    slot: &IterSlot,
    display: String,
    start: usize,
    end: usize,
    memory: &mut LogicalMemory,
    builder: &mut IrBuilder,
) -> Result<(), ConversionError> {
    if matches!(slot, IterSlot::Root) {
        builder.heading(1, "JSON".into(), start, end)?;
        builder.code(None, display, start, end)
    } else {
        builder.json_scalar(iter_label(slot, memory)?, display, iter_slot_start(slot, start), end)
    }
}

fn iter_label(slot: &IterSlot, memory: &mut LogicalMemory) -> Result<String, ConversionError> {
    let label = match slot {
        IterSlot::Root => {
            memory.charge(1)?;
            "$".into()
        }
        IterSlot::Object(key, _) => {
            memory.charge(key.len())?;
            key.clone()
        }
        IterSlot::Array(index) => {
            memory.charge(24)?;
            format!("[{index}]")
        }
    };
    Ok(label)
}

fn iter_slot_start(slot: &IterSlot, fallback: usize) -> usize {
    if let IterSlot::Object(_, start) = slot { *start } else { fallback }
}

impl JsonLexer<'_> {
    fn checkpoint(&mut self) -> Result<(), ConversionError> {
        if self.offset >= self.next_checkpoint {
            self.context.checkpoint()?;
            self.next_checkpoint = self.offset.saturating_add(CHECKPOINT_BYTES);
        }
        Ok(())
    }
    fn space(&mut self) -> Result<(), ConversionError> {
        while self.bytes.get(self.offset).is_some_and(u8::is_ascii_whitespace) {
            self.offset += 1;
            self.checkpoint()?;
        }
        Ok(())
    }
    fn string(&mut self) -> Result<(String, usize), ConversionError> {
        let start = self.offset;
        let mut budget = super::JsonScanBudget { context: self.context, next_checkpoint: 0 };
        let end = match super::scan_json_string(self.bytes, start, &mut budget)? {
            super::JsonLexeme::Complete(end) => end,
            super::JsonLexeme::Open => {
                return Err(malformed(
                    InputFormat::Json,
                    format!("unterminated string at byte {}", start + self.base),
                ));
            }
            super::JsonLexeme::Invalid => {
                return Err(malformed(
                    InputFormat::Json,
                    format!("invalid string at byte {}", start + self.base),
                ));
            }
        };
        self.memory.charge(end.saturating_sub(start))?;
        #[cfg(test)]
        JSON_STRING_DECODE_INVOCATIONS.with(|count| count.set(count.get() + 1));
        let output =
            serde_json::from_slice::<String>(&self.bytes[start..end]).map_err(|error| {
                malformed(
                    InputFormat::Json,
                    format!("invalid string at byte {}: {error}", start + self.base),
                )
            })?;
        if output.len() > self.max_string {
            return Err(ConversionError::ResourceLimit {
                limit: "json_string_bytes",
                detail: format!("decoded JSON string exceeds {} bytes", self.max_string),
            });
        }
        self.offset = end;
        Ok((output, start))
    }
    fn number(&mut self) -> Result<String, ConversionError> {
        let start = self.offset;
        let mut budget = super::JsonScanBudget { context: self.context, next_checkpoint: 0 };
        self.offset = match super::scan_json_number(self.bytes, start, &mut budget)? {
            super::JsonLexeme::Complete(end) => end,
            super::JsonLexeme::Open | super::JsonLexeme::Invalid => {
                return Err(malformed(
                    InputFormat::Json,
                    format!("invalid number at byte {}", start + self.base),
                ));
            }
        };
        let lexeme =
            std::str::from_utf8(&self.bytes[start..self.offset]).expect("number lexeme is ASCII");
        self.memory.charge(lexeme.len())?;
        Ok(lexeme.to_owned())
    }
    fn literal(&mut self, literal: &[u8]) -> Result<(), ConversionError> {
        match super::scan_json_literal(self.bytes, self.offset, literal) {
            super::JsonLexeme::Complete(end) => {
                self.offset = end;
                Ok(())
            }
            super::JsonLexeme::Open | super::JsonLexeme::Invalid => Err(malformed(
                InputFormat::Json,
                format!("invalid literal at byte {}", self.offset + self.base),
            )),
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.offset) == Some(&byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, byte: u8) -> Result<(), ConversionError> {
        if self.take(byte) {
            Ok(())
        } else {
            Err(malformed(
                InputFormat::Json,
                format!("expected {:?} at byte {}", char::from(byte), self.offset + self.base),
            ))
        }
    }
}

#[derive(Debug)]
struct XmlFrame {
    qname: String,
    expanded: String,
}

const XML_NAMESPACE_URI: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE_URI: &str = "http://www.w3.org/2000/xmlns/";

fn xml_scan_checkpoint(
    context: &ExecutionContext,
    offset: usize,
    next: &mut usize,
) -> Result<(), ConversionError> {
    while offset >= *next {
        #[cfg(test)]
        XML_SCAN_CHECKPOINTS.with(|count| count.set(count.get() + 1));
        context.checkpoint()?;
        *next = next.saturating_add(CHECKPOINT_BYTES);
    }
    Ok(())
}

/// Allocation-free lexical pass used to bound the largest slice quick-xml can
/// materialize as one event. It deliberately understands quoted tag content
/// and the distinct comment, CDATA, and PI terminators.
pub(crate) fn preflight_xml(
    source: &str,
    context: &ExecutionContext,
) -> Result<usize, ConversionError> {
    let bytes = source.as_bytes();
    let mut offset = 0;
    let mut next_checkpoint = 0;
    let mut largest = 0;
    while offset < bytes.len() {
        xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
        let start = offset;
        if bytes[offset] != b'<' {
            while offset < bytes.len() && bytes[offset] != b'<' {
                offset += 1;
                xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
            }
        } else if bytes[offset..].starts_with(b"<!--") {
            offset = scan_xml_terminator(bytes, offset + 4, b"-->", context, &mut next_checkpoint)?;
        } else if bytes[offset..].starts_with(b"<![CDATA[") {
            offset = scan_xml_terminator(bytes, offset + 9, b"]]>", context, &mut next_checkpoint)?;
        } else if bytes[offset..].starts_with(b"<?") {
            offset = scan_xml_terminator(bytes, offset + 2, b"?>", context, &mut next_checkpoint)?;
        } else {
            offset += 1;
            let mut quote = None;
            loop {
                xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
                let Some(byte) = bytes.get(offset).copied() else {
                    return Err(malformed(InputFormat::Xml, "unterminated XML markup"));
                };
                offset += 1;
                match (quote, byte) {
                    (Some(expected), actual) if expected == actual => quote = None,
                    (None, b'\'' | b'\"') => quote = Some(byte),
                    (None, b'>') => break,
                    _ => {}
                }
            }
        }
        largest = largest.max(offset.saturating_sub(start));
    }
    context.checkpoint()?;
    Ok(largest)
}

fn scan_xml_terminator(
    bytes: &[u8],
    mut offset: usize,
    terminator: &[u8],
    context: &ExecutionContext,
    next_checkpoint: &mut usize,
) -> Result<usize, ConversionError> {
    loop {
        xml_scan_checkpoint(context, offset, next_checkpoint)?;
        if bytes.get(offset..).is_some_and(|tail| tail.starts_with(terminator)) {
            return Ok(offset + terminator.len());
        }
        if offset == bytes.len() {
            return Err(malformed(InputFormat::Xml, "unterminated XML lexical event"));
        }
        offset += 1;
    }
}

fn xml_name_start(character: char) -> bool {
    matches!(character as u32,
        0x41..=0x5a | 0x5f | 0x61..=0x7a | 0xc0..=0xd6 | 0xd8..=0xf6 |
        0xf8..=0x2ff | 0x370..=0x37d | 0x37f..=0x1fff | 0x200c..=0x200d |
        0x2070..=0x218f | 0x2c00..=0x2fef | 0x3001..=0xd7ff |
        0xf900..=0xfdcf | 0xfdf0..=0xfffd | 0x1_0000..=0xe_ffff)
}

fn xml_name_char(character: char) -> bool {
    xml_name_start(character)
        || matches!(character as u32, 0x2d | 0x2e | 0x30..=0x39 | 0xb7 | 0x300..=0x36f | 0x203f..=0x2040)
}

fn validate_ncname(name: &str, part: &str) -> Result<(), ConversionError> {
    let mut characters = name.chars();
    if !characters.next().is_some_and(xml_name_start)
        || characters.any(|character| !xml_name_char(character))
    {
        return Err(malformed(InputFormat::Xml, format!("invalid XML NCName in {part}")));
    }
    Ok(())
}

fn validate_xml_name(name: &str, part: &str) -> Result<(), ConversionError> {
    let mut characters = name.chars();
    if !characters.next().is_some_and(|character| character == ':' || xml_name_start(character))
        || characters.any(|character| character != ':' && !xml_name_char(character))
    {
        return Err(malformed(InputFormat::Xml, format!("invalid XML Name in {part}")));
    }
    Ok(())
}

fn validate_qname(name: &str, part: &str) -> Result<(), ConversionError> {
    let mut pieces = name.split(':');
    let first = pieces.next().unwrap_or_default();
    validate_ncname(first, part)?;
    if let Some(local) = pieces.next() {
        validate_ncname(local, part)?;
        if pieces.next().is_some() {
            return Err(malformed(
                InputFormat::Xml,
                format!("XML QName in {part} has multiple colons"),
            ));
        }
    }
    Ok(())
}

pub(crate) fn xml_charset(source: &[u8]) -> (&'static str, &'static str) {
    if source.starts_with(&[0xff, 0xfe]) || source.starts_with(&[b'<', 0, b'?', 0]) {
        ("utf-16le", "utf-16le")
    } else if source.starts_with(&[0xfe, 0xff]) || source.starts_with(&[0, b'<', 0, b'?']) {
        ("utf-16be", "utf-16be")
    } else {
        ("utf-8", "utf-8")
    }
}

pub(super) enum XmlDetectionText {
    Decoded(String),
    InvalidUtf16,
}

pub(super) fn decode_xml_for_detection(
    source: &[u8],
    context: &ExecutionContext,
) -> Result<Option<XmlDetectionText>, ConversionError> {
    let (charset, _) = xml_charset(source);
    if charset == "utf-8" {
        return Ok(None);
    }
    let xml_prefix = match charset {
        "utf-16le" => source.strip_prefix(&[0xff, 0xfe]).unwrap_or(source).starts_with(&[b'<', 0]),
        "utf-16be" => source.strip_prefix(&[0xfe, 0xff]).unwrap_or(source).starts_with(&[0, b'<']),
        _ => false,
    };
    match super::text::decode_source(source, Some(charset), TextDecodingMode::Strict, context) {
        Ok((decoded, _)) => Ok(Some(XmlDetectionText::Decoded(decoded.text))),
        Err(ConversionError::Malformed { .. }) if xml_prefix => {
            Ok(Some(XmlDetectionText::InvalidUtf16))
        }
        Err(ConversionError::Malformed { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Validate that XML evidence contains exactly one complete root without
/// allocating document IR. Syntax failures remain detection evidence only;
/// resource, cancellation, and timeout failures stay authoritative.
pub(super) fn xml_complete_for_detection(
    source: &[u8],
    context: &ExecutionContext,
) -> Result<bool, ConversionError> {
    let (charset, _) = xml_charset(source);
    let decoded = match super::text::decode_source(
        source,
        Some(charset),
        TextDecodingMode::Strict,
        context,
    ) {
        Ok((decoded, _)) => decoded,
        Err(ConversionError::Malformed { .. }) => return Ok(false),
        Err(error) => return Err(error),
    };
    if let Err(error) = preflight_xml(&decoded.text, context) {
        return match error {
            ConversionError::Malformed { .. } => Ok(false),
            other => Err(other),
        };
    }
    let mut reader = NsReader::from_str(&decoded.text);
    {
        let config = reader.config_mut();
        config.allow_dangling_amp = false;
        config.allow_unmatched_ends = false;
        config.check_end_names = true;
        config.check_comments = true;
    }
    let mut depth = 0_u64;
    let mut roots = 0_u8;
    loop {
        context.checkpoint()?;
        let Ok(event) = reader.read_event() else {
            return Ok(false);
        };
        match event {
            Event::Start(_) => {
                if depth == 0 {
                    roots = roots.saturating_add(1);
                }
                depth = depth.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                    limit: "max_nesting_depth",
                    detail: "XML detection depth overflowed".into(),
                })?;
                if depth > u64::from(context.resource_limits().max_nesting_depth) {
                    return Err(ConversionError::ResourceLimit {
                        limit: "max_nesting_depth",
                        detail: format!(
                            "{depth} > {}",
                            context.resource_limits().max_nesting_depth
                        ),
                    });
                }
            }
            Event::Empty(_) => {
                if depth == 0 {
                    roots = roots.saturating_add(1);
                }
            }
            Event::End(_) => {
                let Some(next) = depth.checked_sub(1) else { return Ok(false) };
                depth = next;
            }
            Event::Text(text) if depth == 0 => {
                let Ok(value) = text.decode() else { return Ok(false) };
                if !value.chars().all(char::is_whitespace) {
                    return Ok(false);
                }
            }
            Event::CData(_) | Event::GeneralRef(_) if depth == 0 => return Ok(false),
            Event::Eof => return Ok(depth == 0 && roots == 1),
            _ => {}
        }
        if roots > 1 {
            return Ok(false);
        }
    }
}

#[allow(clippy::too_many_lines)]
fn convert_xml(
    source: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Document, ConversionError> {
    let (charset, actual_encoding) = xml_charset(source);
    let (decoded_source, diagnostics) =
        super::text::decode_source(source, Some(charset), TextDecodingMode::Strict, context)?;
    if !diagnostics.is_empty() {
        return Err(ConversionError::Internal {
            detail: "strict XML decoding unexpectedly produced recovery diagnostics".into(),
        });
    }
    let largest_event = preflight_xml(&decoded_source.text, context)?;
    let mut parse_memory = LogicalMemory::new(context)?;
    let mut scratch_memory = LogicalMemory::new(context)?;
    let mut reader = NsReader::from_str(&decoded_source.text);
    {
        let config = reader.config_mut();
        config.allow_dangling_amp = false;
        config.allow_unmatched_ends = false;
        config.check_end_names = true;
        config.check_comments = true;
    }
    let mut builder = IrBuilder::new(InputFormat::Xml, context)?;
    let mut stack: Vec<XmlFrame> = Vec::new();
    let mut root_seen = false;
    let mut events = 0_usize;
    let mut text_bytes = 0_usize;
    let mut previous = 0_usize;
    loop {
        context.checkpoint()?;
        let scratch_mark = scratch_memory.mark();
        // quick-xml and all event-local decoding/formatting may materialize an
        // owned representation proportional to one complete lexical event.
        // Reserve the preflight upper bound before every parser call.
        scratch_memory.charge(largest_event)?;
        #[cfg(test)]
        XML_READ_EVENT_INVOCATIONS.with(|count| count.set(count.get() + 1));
        let event = reader.read_event().map_err(|error| {
            malformed(
                InputFormat::Xml,
                format!(
                    "invalid XML at byte {}: {error}",
                    decoded_source.source_range(previous, previous).0
                ),
            )
        })?;
        let end = usize::try_from(reader.buffer_position()).map_err(|_| {
            ConversionError::ResourceLimit {
                limit: "max_input_bytes",
                detail: "XML parser position exceeds platform address space".into(),
            }
        })?;
        let start = previous;
        previous = end;
        parse_memory.charge(end.saturating_sub(start))?;
        events = events.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "xml_nodes",
            detail: "XML event count overflowed".into(),
        })?;
        if events > MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "xml_nodes",
                detail: format!("XML exceeds {MAX_DOCUMENT_NODES} events"),
            });
        }
        let event_empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let empty = event_empty;
                if stack.is_empty() && root_seen {
                    return Err(malformed(InputFormat::Xml, "XML contains multiple root elements"));
                }
                let element_name = element.name();
                let raw_name = std::str::from_utf8(element_name.as_ref())
                    .map_err(|_| malformed(InputFormat::Xml, "XML QName is not UTF-8"))?;
                validate_qname(raw_name, "element name")?;
                if raw_name.split_once(':').is_some_and(|(prefix, _)| prefix == "xmlns") {
                    return Err(malformed(InputFormat::Xml, "xmlns cannot be an element prefix"));
                }
                let (qname, expanded, element_identity) =
                    resolved_element(&reader, &element, &mut parse_memory)?;
                if stack.len() >= usize::from(options.limits.max_nesting_depth) {
                    return Err(ConversionError::ResourceLimit {
                        limit: "xml_nesting_depth",
                        detail: format!("XML exceeds {} levels", options.limits.max_nesting_depth),
                    });
                }
                if stack.is_empty() {
                    root_seen = true;
                }
                builder.heading(
                    u8::try_from((stack.len() + 1).min(6)).unwrap_or(6),
                    element_identity,
                    decoded_source.source_range(start, end).0,
                    decoded_source.source_range(start, end).1,
                )?;
                emit_xml_attributes(
                    &reader,
                    &element,
                    &decoded_source,
                    start,
                    end,
                    options,
                    context,
                    &mut scratch_memory,
                    &mut builder,
                )?;
                if !empty {
                    parse_memory.charge(
                        qname
                            .capacity()
                            .checked_add(expanded.capacity())
                            .and_then(|bytes| bytes.checked_add(size_of::<XmlFrame>()))
                            .ok_or_else(|| ConversionError::ResourceLimit {
                                limit: "max_memory_bytes",
                                detail: "XML element stack memory overflowed".into(),
                            })?,
                    )?;
                    parse_memory.reserve_vec(&mut stack, 1)?;
                    stack.push(XmlFrame { qname, expanded });
                }
            }
            Event::End(element) => {
                let expected = stack.pop().ok_or_else(|| {
                    malformed(InputFormat::Xml, "XML end tag has no open element")
                })?;
                let element_name = element.name();
                let raw = std::str::from_utf8(element_name.as_ref())
                    .map_err(|_| malformed(InputFormat::Xml, "XML name is not UTF-8"))?;
                validate_qname(raw, "end tag")?;
                scratch_memory.charge(raw.len())?;
                let raw = raw.to_owned();
                let (_, expanded) = resolved_end(&reader, element.name(), &mut scratch_memory)?;
                if raw != expected.qname || expanded != expected.expanded {
                    return Err(malformed(
                        InputFormat::Xml,
                        format!("mismatched closing tag </{raw}>"),
                    ));
                }
            }
            Event::Text(value) => {
                let raw_range = decoded_source.source_range(start, end);
                let decoded = value.xml_content().map_err(|error| {
                    malformed(InputFormat::Xml, format!("invalid XML text: {error}"))
                })?;
                validate_xml_chars(&decoded, "text")?;
                if stack.is_empty() && !decoded.chars().all(char::is_whitespace) {
                    return Err(malformed(
                        InputFormat::Xml,
                        "character data appears outside the root",
                    ));
                }
                if !decoded.is_empty() && !stack.is_empty() {
                    add_xml_text(
                        decoded.into_owned(),
                        raw_range.0,
                        raw_range.1,
                        &mut text_bytes,
                        options,
                        &mut builder,
                    )?;
                }
            }
            Event::CData(value) => {
                let raw_range = decoded_source.source_range(start, end);
                let decoded = value
                    .decode()
                    .map_err(|error| {
                        malformed(InputFormat::Xml, format!("invalid CDATA: {error}"))
                    })?
                    .into_owned();
                validate_xml_chars(&decoded, "CDATA")?;
                if stack.is_empty() && !decoded.is_empty() {
                    return Err(malformed(InputFormat::Xml, "CDATA appears outside the root"));
                }
                if !decoded.is_empty() && !stack.is_empty() {
                    add_xml_text(
                        decoded,
                        raw_range.0,
                        raw_range.1,
                        &mut text_bytes,
                        options,
                        &mut builder,
                    )?;
                }
            }
            Event::Comment(value) => {
                let decoded = value.decode().map_err(|error| {
                    malformed(InputFormat::Xml, format!("invalid comment: {error}"))
                })?;
                validate_xml_chars(&decoded, "comment")?;
                scratch_memory.charge(32)?;
                builder.insert_metadata(
                    format!("xml.comment.{:06}", builder.metadata.len() + 1),
                    decoded.into_owned(),
                )?;
            }
            Event::PI(value) => {
                let raw_range = decoded_source.source_range(start, end);
                let target = std::str::from_utf8(value.target()).map_err(|_| {
                    malformed(InputFormat::Xml, "processing instruction target is not UTF-8")
                })?;
                validate_xml_name(target, "processing instruction target")?;
                if target.eq_ignore_ascii_case("xml") {
                    return Err(malformed(
                        InputFormat::Xml,
                        "processing instruction target xml is reserved",
                    ));
                }
                let decoded = reader.decoder().decode(value.as_ref()).map_err(|error| {
                    malformed(InputFormat::Xml, format!("invalid processing instruction: {error}"))
                })?;
                validate_xml_chars(&decoded, "processing instruction")?;
                builder.code(
                    Some("xml-processing-instruction"),
                    format!("<?{decoded}?>"),
                    raw_range.0,
                    raw_range.1,
                )?;
            }
            Event::Decl(decl) => {
                if start != 0 {
                    return Err(malformed(InputFormat::Xml, "XML declaration must be first"));
                }
                let _ = decl;
                let declaration = decoded_source.text.get(start..end).ok_or_else(|| {
                    ConversionError::Internal { detail: "XML declaration span is invalid".into() }
                })?;
                validate_xml_declaration(declaration, actual_encoding)?;
            }
            Event::DocType(_) => {
                return Err(malformed(InputFormat::Xml, "DOCTYPE and DTD are not allowed"));
            }
            Event::GeneralRef(reference) => {
                let raw_range = decoded_source.source_range(start, end);
                if stack.is_empty() {
                    return Err(malformed(
                        InputFormat::Xml,
                        "entity reference appears outside the root",
                    ));
                }
                let raw = reference.decode().map_err(|error| {
                    malformed(InputFormat::Xml, format!("invalid entity reference: {error}"))
                })?;
                let decoded = predefined_or_numeric_entity(&raw)?;
                add_xml_text(
                    decoded,
                    raw_range.0,
                    raw_range.1,
                    &mut text_bytes,
                    options,
                    &mut builder,
                )?;
            }
            Event::Eof => {
                scratch_memory.rewind(scratch_mark)?;
                break;
            }
        }
        scratch_memory.rewind(scratch_mark)?;
    }
    if !root_seen || !stack.is_empty() {
        return Err(malformed(InputFormat::Xml, "XML root is missing or incomplete"));
    }
    builder.finish()
}

pub(crate) fn validate_xml_declaration(
    declaration: &str,
    actual: &str,
) -> Result<(), ConversionError> {
    let body = declaration
        .strip_prefix("<?xml")
        .and_then(|value| value.strip_suffix("?>"))
        .ok_or_else(|| malformed(InputFormat::Xml, "invalid XML declaration delimiters"))?;
    let bytes = body.as_bytes();
    let mut offset = 0;
    let mut field = 0;
    let mut encoding = None;
    while offset < bytes.len() {
        let before_space = offset;
        while bytes.get(offset).is_some_and(|byte| xml_space(*byte)) {
            offset += 1;
        }
        if offset == bytes.len() {
            break;
        }
        if offset == before_space {
            return Err(malformed(InputFormat::Xml, "XML declaration fields require whitespace"));
        }
        let name_start = offset;
        while bytes.get(offset).is_some_and(u8::is_ascii_alphabetic) {
            offset += 1;
        }
        let name = &body[name_start..offset];
        while bytes.get(offset).is_some_and(|byte| xml_space(*byte)) {
            offset += 1;
        }
        if bytes.get(offset) != Some(&b'=') {
            return Err(malformed(InputFormat::Xml, "XML declaration field is missing '='"));
        }
        offset += 1;
        while bytes.get(offset).is_some_and(|byte| xml_space(*byte)) {
            offset += 1;
        }
        let Some(quote @ (b'\'' | b'\"')) = bytes.get(offset).copied() else {
            return Err(malformed(InputFormat::Xml, "XML declaration value is not quoted"));
        };
        offset += 1;
        let value_start = offset;
        while bytes.get(offset).is_some_and(|byte| *byte != quote) {
            offset += 1;
        }
        if bytes.get(offset) != Some(&quote) {
            return Err(malformed(InputFormat::Xml, "unterminated XML declaration value"));
        }
        let value = &body[value_start..offset];
        offset += 1;
        match (field, name) {
            (0, "version") if value == "1.0" => {}
            (0, _) => {
                return Err(malformed(
                    InputFormat::Xml,
                    "XML declaration must begin with version=\"1.0\"",
                ));
            }
            (1, "encoding") => {
                let mut chars = value.chars();
                if !chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                    || chars.any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
                {
                    return Err(malformed(InputFormat::Xml, "invalid XML encoding name"));
                }
                encoding = Some(value);
            }
            (1 | 2, "standalone") if matches!(value, "yes" | "no") => {}
            _ => {
                return Err(malformed(
                    InputFormat::Xml,
                    "invalid, duplicate, or out-of-order XML declaration field",
                ));
            }
        }
        field += 1;
    }
    if field == 0 {
        return Err(malformed(InputFormat::Xml, "XML declaration is missing version"));
    }
    if let Some(label) = encoding {
        let matches = match actual {
            "utf-8" => label.eq_ignore_ascii_case("utf-8"),
            "utf-16le" => {
                label.eq_ignore_ascii_case("utf-16") || label.eq_ignore_ascii_case("utf-16le")
            }
            "utf-16be" => {
                label.eq_ignore_ascii_case("utf-16") || label.eq_ignore_ascii_case("utf-16be")
            }
            _ => false,
        };
        if !matches {
            return Err(malformed(
                InputFormat::Xml,
                format!("XML encoding declaration {label:?} conflicts with {actual} input"),
            ));
        }
    }
    Ok(())
}

fn resolved_element(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    memory: &mut LogicalMemory,
) -> Result<(String, String, String), ConversionError> {
    let element_name = element.name();
    let qname = std::str::from_utf8(element_name.as_ref())
        .map_err(|_| malformed(InputFormat::Xml, "XML QName is not UTF-8"))?;
    let (namespace, local) = reader.resolve_element(element.name());
    let namespace = namespace_uri(namespace, memory)?;
    let local = std::str::from_utf8(local.as_ref())
        .map_err(|_| malformed(InputFormat::Xml, "XML local name is not UTF-8"))?;
    let prefix = qname.split_once(':').map_or("", |(prefix, _)| prefix);
    let uri = namespace.unwrap_or_default();
    memory.charge(
        qname
            .len()
            .saturating_mul(2)
            .saturating_add(local.len())
            .saturating_mul(2)
            .saturating_add(uri.len())
            .saturating_mul(2)
            .saturating_add(80),
    )?;
    Ok((
        qname.to_owned(),
        format!("{{{uri}}}{local}"),
        format!("{qname} (local={local}, prefix={prefix:?}, namespace={uri:?})"),
    ))
}

fn resolved_end(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
    memory: &mut LogicalMemory,
) -> Result<(String, String), ConversionError> {
    let qname = std::str::from_utf8(name.as_ref())
        .map_err(|_| malformed(InputFormat::Xml, "XML QName is not UTF-8"))?;
    let (namespace, local) = reader.resolve_element(name);
    let namespace = namespace_uri(namespace, memory)?;
    let local = std::str::from_utf8(local.as_ref())
        .map_err(|_| malformed(InputFormat::Xml, "XML local name is not UTF-8"))?;
    memory.charge(
        qname
            .len()
            .saturating_add(local.len())
            .saturating_add(namespace.as_deref().map_or(0, str::len))
            .saturating_add(2),
    )?;
    Ok((qname.to_owned(), format!("{{{}}}{local}", namespace.unwrap_or_default())))
}

fn namespace_uri(
    value: ResolveResult<'_>,
    memory: &mut LogicalMemory,
) -> Result<Option<String>, ConversionError> {
    match value {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(uri) => {
            let uri = std::str::from_utf8(uri.as_ref())
                .map_err(|_| malformed(InputFormat::Xml, "namespace URI is not UTF-8"))?;
            memory.charge(uri.len())?;
            Ok(Some(uri.to_owned()))
        }
        ResolveResult::Unknown(prefix) => Err(malformed(
            InputFormat::Xml,
            format!("unbound namespace prefix {:?}", String::from_utf8_lossy(&prefix)),
        )),
    }
}

#[derive(Clone, Copy)]
struct XmlAttributeSpan {
    name_start: usize,
    name_end: usize,
    value_start: usize,
    value_end: usize,
}

fn scan_xml_attribute_spans(
    source: &str,
    event_start: usize,
    event_end: usize,
    context: &ExecutionContext,
    memory: &mut LogicalMemory,
) -> Result<Vec<XmlAttributeSpan>, ConversionError> {
    let bytes =
        source.as_bytes().get(event_start..event_end).ok_or_else(|| ConversionError::Internal {
            detail: "XML start-tag event range is outside decoded text".into(),
        })?;
    let tag = bytes
        .iter()
        .position(|byte| *byte == b'<')
        .ok_or_else(|| malformed(InputFormat::Xml, "start-tag event has no opening delimiter"))?;
    let mut offset = tag + 1;
    let mut next_checkpoint = 0;
    while bytes.get(offset).is_some_and(|byte| !xml_space(*byte) && !matches!(*byte, b'/' | b'>')) {
        offset += 1;
        xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
    }
    if offset == tag + 1 {
        return Err(malformed(InputFormat::Xml, "start tag has no QName"));
    }
    let mut spans = Vec::new();
    loop {
        while bytes.get(offset).is_some_and(|byte| xml_space(*byte)) {
            offset += 1;
            xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
        }
        match bytes.get(offset).copied() {
            Some(b'>') => break,
            Some(b'/') if bytes.get(offset + 1) == Some(&b'>') => break,
            None => return Err(malformed(InputFormat::Xml, "unterminated start tag")),
            _ => {}
        }
        let name_start = offset;
        while bytes.get(offset).is_some_and(|byte| {
            !xml_space(*byte) && !matches!(*byte, b'=' | b'/' | b'>' | b'<' | b'\'' | b'"')
        }) {
            offset += 1;
            xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
        }
        let name_end = offset;
        if name_end == name_start {
            return Err(malformed(InputFormat::Xml, "attribute has no QName"));
        }
        while bytes.get(offset).is_some_and(|byte| xml_space(*byte)) {
            offset += 1;
            xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
        }
        if bytes.get(offset) != Some(&b'=') {
            return Err(malformed(InputFormat::Xml, "attribute QName is not followed by '='"));
        }
        offset += 1;
        while bytes.get(offset).is_some_and(|byte| xml_space(*byte)) {
            offset += 1;
            xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
        }
        let Some(quote @ (b'\'' | b'"')) = bytes.get(offset).copied() else {
            return Err(malformed(InputFormat::Xml, "attribute value is not quoted"));
        };
        offset += 1;
        let value_start = offset;
        while bytes.get(offset).is_some_and(|byte| *byte != quote) {
            if bytes[offset] == b'<' {
                return Err(malformed(InputFormat::Xml, "attribute value contains '<'"));
            }
            offset += 1;
            xml_scan_checkpoint(context, offset, &mut next_checkpoint)?;
        }
        let value_end = offset;
        if bytes.get(offset) != Some(&quote) {
            return Err(malformed(InputFormat::Xml, "unterminated attribute value"));
        }
        offset += 1;
        memory.reserve_vec(&mut spans, 1)?;
        spans.push(XmlAttributeSpan {
            name_start: event_start + name_start,
            name_end: event_start + name_end,
            value_start: event_start + value_start,
            value_end: event_start + value_end,
        });
    }
    Ok(spans)
}

fn xml_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

#[allow(clippy::too_many_arguments)]
fn emit_xml_attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    decoded: &DecodedText,
    event_start: usize,
    event_end: usize,
    options: &ConversionOptions,
    context: &ExecutionContext,
    memory: &mut LogicalMemory,
    builder: &mut IrBuilder,
) -> Result<(), ConversionError> {
    let spans = scan_xml_attribute_spans(&decoded.text, event_start, event_end, context, memory)?;
    let mut expanded = BTreeSet::new();
    for (index, attribute) in element.attributes().enumerate() {
        if index >= MAX_DOCUMENT_INLINES {
            return Err(ConversionError::ResourceLimit {
                limit: "xml_attributes",
                detail: format!("element exceeds {MAX_DOCUMENT_INLINES} attributes"),
            });
        }
        let attribute = attribute.map_err(|error| {
            malformed(InputFormat::Xml, format!("invalid XML attribute: {error}"))
        })?;
        let raw = std::str::from_utf8(attribute.key.as_ref())
            .map_err(|_| malformed(InputFormat::Xml, "attribute QName is not UTF-8"))?;
        validate_qname(raw, "attribute name")?;
        let span = spans.get(index).copied().ok_or_else(|| ConversionError::Internal {
            detail: "quick-xml returned more attributes than the bounded start-tag scanner".into(),
        })?;
        if decoded.text.get(span.name_start..span.name_end) != Some(raw) {
            return Err(ConversionError::Internal {
                detail: "quick-xml attribute order disagrees with the start-tag scanner".into(),
            });
        }
        let value = attribute.decode_and_unescape_value(reader.decoder()).map_err(|error| {
            malformed(InputFormat::Xml, format!("invalid attribute {raw:?}: {error}"))
        })?;
        validate_xml_chars(&value, raw)?;
        if value.len() > usize::try_from(options.limits.max_field_bytes).unwrap_or(usize::MAX) {
            return Err(ConversionError::ResourceLimit {
                limit: "xml_attribute_bytes",
                detail: format!(
                    "attribute exceeds {} decoded bytes",
                    options.limits.max_field_bytes
                ),
            });
        }
        let name_range = decoded.source_range(span.name_start, span.name_end);
        let value_range = decoded.source_range(span.value_start, span.value_end);
        if raw == "xmlns" || raw.starts_with("xmlns:") {
            validate_namespace_declaration(raw, &value)?;
            memory.charge(raw.len())?;
            builder.code(Some("xml-attribute-name"), raw.to_owned(), name_range.0, name_range.1)?;
            builder.code(
                Some("xml-attribute-value"),
                value.into_owned(),
                value_range.0,
                value_range.1,
            )?;
            continue;
        }
        let (namespace, local) = reader.resolve_attribute(attribute.key);
        let uri = namespace_uri(namespace, memory)?.unwrap_or_default();
        if raw.split_once(':').is_some_and(|(prefix, _)| prefix == "xml")
            && uri != XML_NAMESPACE_URI
        {
            return Err(malformed(InputFormat::Xml, "xml prefix is not bound to its reserved URI"));
        }
        memory.charge(
            uri.len()
                .checked_add(local.as_ref().len())
                .and_then(|bytes| bytes.checked_add(128))
                .ok_or_else(|| ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: "XML expanded attribute memory overflowed".into(),
                })?,
        )?;
        let identity = format!("{{{uri}}}{}", String::from_utf8_lossy(local.as_ref()));
        if !expanded.insert(identity.clone()) {
            return Err(malformed(
                InputFormat::Xml,
                format!("duplicate expanded attribute name {identity}"),
            ));
        }
        let prefix = raw.split_once(':').map_or("", |(prefix, _)| prefix);
        builder.code(
            Some("xml-attribute-name"),
            format!(
                "{raw} (local={}, prefix={prefix:?}, namespace={uri:?})",
                String::from_utf8_lossy(local.as_ref())
            ),
            name_range.0,
            name_range.1,
        )?;
        builder.code(
            Some("xml-attribute-value"),
            value.into_owned(),
            value_range.0,
            value_range.1,
        )?;
    }
    if spans.len() != element.attributes().count() {
        return Err(ConversionError::Internal {
            detail: "start-tag scanner returned more attributes than quick-xml".into(),
        });
    }
    Ok(())
}

fn validate_namespace_declaration(raw: &str, value: &str) -> Result<(), ConversionError> {
    let prefix = raw.strip_prefix("xmlns:");
    if prefix == Some("xmlns") {
        return Err(malformed(InputFormat::Xml, "xmlns prefix cannot be declared"));
    }
    if value == XMLNS_NAMESPACE_URI {
        return Err(malformed(InputFormat::Xml, "the xmlns namespace URI is reserved"));
    }
    if prefix == Some("xml") {
        if value != XML_NAMESPACE_URI {
            return Err(malformed(
                InputFormat::Xml,
                "xml prefix must bind only to its reserved URI",
            ));
        }
    } else if value == XML_NAMESPACE_URI {
        return Err(malformed(InputFormat::Xml, "the XML namespace URI may bind only to xml"));
    }
    if prefix.is_some() && value.is_empty() {
        return Err(malformed(
            InputFormat::Xml,
            "XML 1.0 does not allow undeclaring a namespace prefix",
        ));
    }
    Ok(())
}

pub(crate) fn predefined_or_numeric_entity_scalar(raw: &str) -> Result<char, ConversionError> {
    let value = match raw {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "apos" => '\'',
        "quot" => '"',
        _ if raw.starts_with("#x") => {
            char::from_u32(u32::from_str_radix(&raw[2..], 16).map_err(|_| {
                malformed(InputFormat::Xml, format!("invalid numeric entity &{raw};"))
            })?)
            .ok_or_else(|| malformed(InputFormat::Xml, format!("invalid numeric entity &{raw};")))?
        }
        _ if raw.starts_with('#') => {
            char::from_u32(raw[1..].parse::<u32>().map_err(|_| {
                malformed(InputFormat::Xml, format!("invalid numeric entity &{raw};"))
            })?)
            .ok_or_else(|| malformed(InputFormat::Xml, format!("invalid numeric entity &{raw};")))?
        }
        _ => {
            return Err(malformed(
                InputFormat::Xml,
                format!("entity &{raw}; is not one of the five predefined or a numeric reference"),
            ));
        }
    };
    if !xml_char(value) {
        return Err(malformed(
            InputFormat::Xml,
            format!("numeric entity &{raw}; is not an XML character"),
        ));
    }
    Ok(value)
}

pub(crate) fn predefined_or_numeric_entity(raw: &str) -> Result<String, ConversionError> {
    Ok(predefined_or_numeric_entity_scalar(raw)?.to_string())
}

fn xml_char(value: char) -> bool {
    matches!(value as u32, 0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x1_0000..=0x10_ffff)
}

pub(crate) fn validate_xml_chars(value: &str, part: &str) -> Result<(), ConversionError> {
    if let Some(character) = value.chars().find(|character| !xml_char(*character)) {
        return Err(malformed(
            InputFormat::Xml,
            format!("{part} contains disallowed XML character U+{:04X}", character as u32),
        ));
    }
    Ok(())
}

fn add_xml_text(
    value: String,
    start: usize,
    end: usize,
    total: &mut usize,
    options: &ConversionOptions,
    builder: &mut IrBuilder,
) -> Result<(), ConversionError> {
    *total = total.checked_add(value.len()).ok_or_else(|| ConversionError::ResourceLimit {
        limit: "xml_text_bytes",
        detail: "decoded XML text length overflowed".into(),
    })?;
    let limit = usize::try_from(options.limits.max_input_bytes).unwrap_or(usize::MAX);
    if *total > limit {
        return Err(ConversionError::ResourceLimit {
            limit: "xml_text_bytes",
            detail: format!("decoded XML text exceeds {limit} bytes"),
        });
    }
    builder.text_paragraph(value, start, end)
}

struct IrBuilder {
    format: InputFormat,
    blocks: Vec<BlockNode>,
    next_id: usize,
    metadata: BTreeMap<String, String>,
    memory: LogicalMemory,
}
impl IrBuilder {
    fn new(format: InputFormat, context: &ExecutionContext) -> Result<Self, ConversionError> {
        Ok(Self {
            format,
            blocks: Vec::new(),
            next_id: 1,
            metadata: BTreeMap::new(),
            memory: LogicalMemory::new(context)?,
        })
    }
    fn push(&mut self, block: Block, start: usize, end: usize) -> Result<(), ConversionError> {
        if self.blocks.len() >= MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "document_nodes",
                detail: format!("output exceeds {MAX_DOCUMENT_NODES} blocks"),
            });
        }
        self.memory.reserve_vec(&mut self.blocks, 1)?;
        self.memory.charge(self.format.as_str().len().checked_add(21).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "structured node ID memory overflowed".into(),
            }
        })?)?;
        let id_text = format!("{}-{}", self.format.as_str(), self.next_id);
        self.memory.charge(PROVIDER_ID.len())?;
        let id = NodeId(id_text);
        self.next_id += 1;
        self.blocks.push(BlockNode {
            id,
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: SourceLocator {
                    byte_start: Some(u64::try_from(start).unwrap_or(u64::MAX)),
                    byte_end: Some(u64::try_from(end).unwrap_or(u64::MAX)),
                    ..SourceLocator::default()
                },
                confidence: Some(1.0),
            },
        });
        Ok(())
    }
    fn heading(
        &mut self,
        level: u8,
        value: String,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        self.memory.charge(value.capacity())?;
        let mut content = Vec::new();
        self.memory.reserve_vec(&mut content, 1)?;
        content.push(Inline::Text { value, marks: Vec::new() });
        self.push(Block::Heading { level, content }, start, end)
    }
    fn heading_index(
        &mut self,
        level: u8,
        value: String,
        start: usize,
        end: usize,
    ) -> Result<usize, ConversionError> {
        let index = self.blocks.len();
        self.heading(level, value, start, end)?;
        Ok(index)
    }
    fn set_end(&mut self, index: usize, end: usize) -> Result<(), ConversionError> {
        let block = self.blocks.get_mut(index).ok_or_else(|| ConversionError::Internal {
            detail: "structured IR block index is out of bounds".into(),
        })?;
        block.provenance.locator.byte_end = Some(u64::try_from(end).unwrap_or(u64::MAX));
        Ok(())
    }
    fn json_scalar(
        &mut self,
        label: String,
        display: String,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        self.memory.charge(label.capacity())?;
        self.memory.charge(display.capacity())?;
        self.memory.charge(2)?;
        let mut content = Vec::new();
        self.memory.reserve_vec(&mut content, 3)?;
        content.push(Inline::Code(label));
        content.push(Inline::Text { value: ": ".into(), marks: Vec::new() });
        content.push(Inline::Code(display));
        self.push(Block::Paragraph(content), start, end)
    }
    fn text_paragraph(
        &mut self,
        value: String,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        self.memory.charge(value.capacity())?;
        let mut content = Vec::new();
        self.memory.reserve_vec(&mut content, 1)?;
        content.push(Inline::Text { value, marks: Vec::new() });
        self.push(Block::Paragraph(content), start, end)
    }
    fn code(
        &mut self,
        language: Option<&str>,
        text: String,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        self.memory.charge(text.capacity())?;
        let language = if let Some(language) = language {
            self.memory.charge(language.len())?;
            Some(language.to_owned())
        } else {
            None
        };
        self.push(Block::Code { language, text }, start, end)
    }
    fn insert_metadata(&mut self, key: String, value: String) -> Result<(), ConversionError> {
        self.memory.charge(key.capacity())?;
        self.memory.charge(value.capacity())?;
        self.memory.charge(size_of::<(String, String)>() * 2)?;
        self.metadata.insert(key, value);
        Ok(())
    }
    fn finish(self) -> Result<Document, ConversionError> {
        let mut document = Document { blocks: self.blocks, ..Document::default() };
        document.metadata.properties = self.metadata;
        document.validate().map_err(|error| ConversionError::Internal {
            detail: format!("structured converter produced invalid IR: {error}"),
        })?;
        Ok(document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits};
    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }
    #[test]
    fn json_rejects_duplicate_and_unpaired_surrogate() {
        let options = ConversionOptions::default();
        for input in [
            include_bytes!("../tests/fixtures/json/duplicate-key.json").as_slice(),
            br#""\uD800""#.as_slice(),
        ] {
            assert!(matches!(
                convert_json(input, &options, &context()),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }
    #[test]
    fn json_preserves_order_number_and_bom_offsets() {
        let document = convert_json(
            include_bytes!("../tests/fixtures/json/large-number.json"),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert!(
            matches!(&document.blocks[1].block, Block::Paragraph(value) if matches!(&value[0], Inline::Code(key) if key == "integer") && matches!(&value[2], Inline::Code(number) if number == "900719925474099312345678901234567890"))
        );
        assert_eq!(document.blocks[1].provenance.locator.byte_start, Some(1));
        let bom = convert_json(b"\xef\xbb\xbf{\"a\":1}", &ConversionOptions::default(), &context())
            .unwrap();
        assert_eq!(bom.blocks[1].provenance.locator.byte_start, Some(4));

        let empty = convert_json(b"[]", &ConversionOptions::default(), &context()).unwrap();
        assert!(matches!(&empty.blocks[1].block, Block::Code { text, .. } if text == "[]"));
    }

    #[test]
    fn top_level_json_scalars_are_deterministic_candidates() {
        for input in [b"true".as_slice(), b"123".as_slice(), br#""x""#.as_slice()] {
            let candidate = super::super::structured_for_test(input, &context()).unwrap().unwrap();
            assert_eq!(candidate.format, InputFormat::Json);
            assert!(convert_json(input, &ConversionOptions::default(), &context()).is_ok());
        }
    }
    #[test]
    fn xml_preserves_namespace_mixed_content_and_rejects_dtd() {
        let document = convert_xml(
            include_bytes!("../tests/fixtures/xml/mixed-namespace.xml"),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert!(document.blocks.iter().any(|node| matches!(&node.block, Block::Heading { content, .. } if matches!(&content[0], Inline::Text { value, .. } if value.starts_with("a:item ") && value.contains("namespace=\"urn:a\"")))));
        assert!(matches!(
            convert_xml(
                include_bytes!("../tests/fixtures/xml/doctype-entity.xml"),
                &ConversionOptions::default(),
                &context()
            ),
            Err(ConversionError::Malformed { .. })
        ));
        assert!(matches!(
            convert_xml(
                include_bytes!("../tests/fixtures/xml/billion-laughs.xml"),
                &ConversionOptions::default(),
                &context()
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }
    #[test]
    fn xml_rejects_unbound_prefix_and_non_utf8_declaration() {
        for input in [
            br"<p:r/>".as_slice(),
            br#"<?xml version="1.0" encoding="ISO-8859-1"?><r/>"#.as_slice(),
        ] {
            assert!(matches!(
                convert_xml(input, &ConversionOptions::default(), &context()),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn xml_encoding_is_read_only_from_the_declaration_event() {
        for input in [
            br#"<r encoding="UTF-16">encoding="ISO-8859-1"</r>"#.as_slice(),
            br#"<r><!-- encoding="UTF-16" --></r>"#.as_slice(),
            br#"<r><?target encoding="UTF-16"?></r>"#.as_slice(),
        ] {
            assert!(convert_xml(input, &ConversionOptions::default(), &context()).is_ok());
        }
    }

    #[test]
    fn xml_declaration_obeys_xml_1_0_grammar() {
        for valid in [
            br#"<?xml version="1.0"?><r/>"#.as_slice(),
            br#"<?xml version='1.0' encoding="UTF-8"?><r/>"#.as_slice(),
            br#"<?xml version="1.0" standalone='yes'?><r/>"#.as_slice(),
            br#"<?xml version="1.0" encoding="UTF-8" standalone="no"?><r/>"#.as_slice(),
        ] {
            assert!(convert_xml(valid, &ConversionOptions::default(), &context()).is_ok());
        }
        for invalid in [
            br"<?xml?><r/>".as_slice(),
            br#"<?xml encoding="UTF-8" version="1.0"?><r/>"#.as_slice(),
            br#"<?xml version="1.1"?><r/>"#.as_slice(),
            br#"<?xml version "1.0"?><r/>"#.as_slice(),
            br#"<?xml version="1.0" version="1.0"?><r/>"#.as_slice(),
            br#"<?xml version="1.0" standalone="maybe"?><r/>"#.as_slice(),
            br#"<?xml version="1.0" standalone="yes" encoding="UTF-8"?><r/>"#.as_slice(),
            br#"<?xml version="1.0" encoding="8UTF"?><r/>"#.as_slice(),
            br#"<?xml version="1.0" extra="x"?><r/>"#.as_slice(),
        ] {
            assert!(matches!(
                convert_xml(invalid, &ConversionOptions::default(), &context()),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn xml_validates_qnames_and_reserved_namespace_bindings() {
        for valid in [
            "<根 属性='值'/>",
            "<r xmlns='urn:a'><x xmlns=''/></r>",
            "<xml:r xmlns:xml='http://www.w3.org/XML/1998/namespace'/>",
        ] {
            assert!(
                convert_xml(valid.as_bytes(), &ConversionOptions::default(), &context()).is_ok()
            );
        }
        for invalid in [
            "<1r/>",
            "<a:b:c xmlns:a='x'/>",
            "<r bad:name:again='x'/>",
            "<xmlns:r/>",
            "<r xmlns:xml='urn:not-xml'/>",
            "<r xmlns:p='http://www.w3.org/XML/1998/namespace'/>",
            "<r xmlns='http://www.w3.org/XML/1998/namespace'/>",
            "<r xmlns:p='http://www.w3.org/2000/xmlns/'/>",
            "<r xmlns:xmlns='urn:x'/>",
            "<r xmlns:p=''/>",
            "<r xmlns:p='urn:x' xmlns:q='urn:x' p:a='1' q:a='2'/>",
        ] {
            assert!(
                matches!(
                    convert_xml(invalid.as_bytes(), &ConversionOptions::default(), &context()),
                    Err(ConversionError::Malformed { .. })
                ),
                "accepted invalid XML: {invalid}"
            );
        }
    }

    #[test]
    fn xml_single_event_preflight_is_memory_bounded_and_quote_aware() {
        let quoted = format!("<r a='{}>{}'/>", "x".repeat(CHECKPOINT_BYTES + 7), "y");
        assert!(convert_xml(quoted.as_bytes(), &ConversionOptions::default(), &context()).is_ok());

        let huge = format!("<r>{}</r>", "x".repeat(256 * 1024));
        let bounded = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 192 * 1024, ..ResourceLimits::default() },
        );
        XML_READ_EVENT_INVOCATIONS.with(|count| count.set(0));
        assert!(matches!(
            convert_xml(huge.as_bytes(), &ConversionOptions::default(), &bounded),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        XML_READ_EVENT_INVOCATIONS.with(|count| assert_eq!(count.get(), 0));

        let timed_out = ExecutionContext::new(
            ExecutionOptions {
                timeout: Some(std::time::Duration::ZERO),
                ..ExecutionOptions::default()
            },
            ResourceLimits::default(),
        );
        assert!(matches!(
            preflight_xml(&format!("<r>{}</r>", "x".repeat(CHECKPOINT_BYTES * 2)), &timed_out),
            Err(ConversionError::Timeout)
        ));
    }

    #[test]
    fn xml_preflight_checkpoints_every_large_event_channel() {
        let payload = "x".repeat(CHECKPOINT_BYTES * 2 + 17);
        let samples = [
            format!("<r>{payload}</r>"),
            format!("<r><![CDATA[{payload}]]></r>"),
            format!("<{payload}/>"),
            format!("<r a='{payload}'/>"),
            format!("<r><!--{payload}--></r>"),
            format!("<r><?target {payload}?></r>"),
        ];
        for sample in samples {
            XML_SCAN_CHECKPOINTS.with(|count| count.set(0));
            preflight_xml(&sample, &context()).unwrap();
            XML_SCAN_CHECKPOINTS.with(|count| assert!(count.get() >= 3, "{sample}"));
        }
    }

    #[test]
    fn xml_utf16_maps_original_offsets_and_allows_bounded_character_refs() {
        let source = r#"<?xml version="1.0" encoding="UTF-16"?><r>&amp;&#x1F642;</r>"#;
        let mut bytes = vec![0xff, 0xfe];
        for unit in source.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let document = convert_xml(&bytes, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(document.blocks[0].provenance.locator.byte_start, Some(80));
        assert!(document.blocks.iter().any(|node| {
            matches!(&node.block, Block::Paragraph(content) if matches!(&content[0], Inline::Text { value, .. } if value == "&" || value == "🙂"))
        }));

        let attribute_source = r#"<r a="🙂"/>"#;
        let mut attribute_bytes = vec![0xff, 0xfe];
        for unit in attribute_source.encode_utf16() {
            attribute_bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let document =
            convert_xml(&attribute_bytes, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(document.blocks[1].provenance.locator.byte_start, Some(8));
        assert_eq!(document.blocks[1].provenance.locator.byte_end, Some(10));
        assert_eq!(document.blocks[2].provenance.locator.byte_start, Some(14));
        assert_eq!(document.blocks[2].provenance.locator.byte_end, Some(18));
    }

    #[test]
    fn xml_attributes_have_independent_qname_and_value_spans() {
        let source = br#"<r xmlns:p="urn:p" a = "x&amp;y" p:b='z'/>"#;
        let document = convert_xml(source, &ConversionOptions::default(), &context()).unwrap();
        let expected = [(3, 10), (12, 17), (19, 20), (24, 31), (33, 36), (38, 39)];
        for (block, (start, end)) in document.blocks[1..].iter().zip(expected) {
            assert_eq!(block.provenance.locator.byte_start, Some(start));
            assert_eq!(block.provenance.locator.byte_end, Some(end));
        }
        assert!(
            matches!(&document.blocks[5].block, Block::Code { language: Some(language), text } if language == "xml-attribute-name" && text.contains("namespace=\"urn:p\""))
        );
    }

    #[test]
    fn xml_validates_every_decoded_character_channel() {
        let invalid = [
            b"<r>\x01</r>".as_slice(),
            b"<r><![CDATA[\x01]]></r>".as_slice(),
            b"<r a=\"\x01\"/>".as_slice(),
            b"<r>&#1;</r>".as_slice(),
            b"<r a=\"&#1;\"/>".as_slice(),
            b"<r>&#xD800;</r>".as_slice(),
            b"<r a=\"&#xFFFE;\"/>".as_slice(),
        ];
        for input in invalid {
            assert!(matches!(
                convert_xml(input, &ConversionOptions::default(), &context()),
                Err(ConversionError::Malformed { .. })
            ));
        }
        let noncharacter = "<r>\u{fffe}</r>";
        assert!(matches!(
            convert_xml(noncharacter.as_bytes(), &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn xml_comments_are_metadata_and_pi_is_explicit_ir() {
        let document = convert_xml(
            br"<r><!--note--><?target value?></r>",
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(
            document.metadata.properties.get("xml.comment.000001").map(String::as_str),
            Some("note")
        );
        assert!(document.blocks.iter().any(|node| matches!(&node.block, Block::Code { language: Some(language), .. } if language == "xml-processing-instruction")));
        for invalid in [br"<r><?1bad?></r>".as_slice(), br"<r><?XML?></r>".as_slice()] {
            assert!(matches!(
                convert_xml(invalid, &ConversionOptions::default(), &context()),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn damaged_structures_are_protected_without_swallowing_prose() {
        let context = context();
        assert_eq!(
            super::super::structured_for_test(br#"{ "a":}"#, &context).unwrap().unwrap().format,
            InputFormat::Json
        );
        assert!(super::super::structured_for_test(b"{hello}", &context).unwrap().is_none());
        assert_eq!(
            super::super::structured_for_test(b"<r></x>", &context).unwrap().unwrap().format,
            InputFormat::Xml
        );
        assert!(super::super::structured_for_test(b"<3 is less", &context).unwrap().is_none());
    }

    #[test]
    fn structured_depth_width_and_memory_budgets_are_controlled() {
        let mut options = ConversionOptions::default();
        options.limits.max_nesting_depth = 3;
        assert!(matches!(
            convert_json(include_bytes!("../tests/fixtures/json/deep.json"), &options, &context()),
            Err(ConversionError::ResourceLimit { limit: "json_nesting_depth", .. })
        ));
        let wide = convert_json(
            include_bytes!("../tests/fixtures/json/wide.json"),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(wide.blocks.len(), 17);

        let tiny = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 2, ..ResourceLimits::default() },
        );
        assert!(matches!(
            convert_json(b"{\"a\":1}", &ConversionOptions::default(), &tiny),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));

        let large = format!("{{\"value\":\"{}\"}}", "x".repeat(500_000));
        let bounded = ExecutionContext::new(
            ExecutionOptions::default(),
            ResourceLimits { max_memory_bytes: 128 * 1024, ..ResourceLimits::default() },
        );
        JSON_STRING_DECODE_INVOCATIONS.with(|count| count.set(0));
        assert!(matches!(
            convert_json(large.as_bytes(), &ConversionOptions::default(), &bounded),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        JSON_STRING_DECODE_INVOCATIONS.with(|count| assert_eq!(count.get(), 1));
    }
}
