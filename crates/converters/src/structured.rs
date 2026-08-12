//! Deterministic JSON and XML conversion into the common document IR.

use into_markdown_core::{
    Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter, ConverterOutput,
    Document, ExecutionContext, FormatCandidate, Inline, InputFormat, MAX_DOCUMENT_INLINES,
    MAX_DOCUMENT_NODES, NodeId, ProbeOutcome, Provenance, ProvenanceKind, ResolvedInput, Services,
    SourceLocator,
};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use std::collections::{BTreeMap, BTreeSet};

const FORMATS: &[InputFormat] = &[InputFormat::Json, InputFormat::Xml];
const PROVIDER_ID: &str = "builtin.converter.structured-data";
const CHECKPOINT_BYTES: usize = 4096;

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
            Ok(ConverterOutput { document, assets: Vec::new(), diagnostics: Vec::new() })
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
        .is_some_and(|b| matches!(b, b'{' | b'['))
}

fn xml_shape(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'<')
}

#[derive(Debug)]
enum JsonValue {
    Object(Vec<JsonMember>),
    Array(Vec<JsonNode>),
    Scalar { display: String },
}

#[derive(Debug)]
struct JsonNode {
    value: JsonValue,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct JsonMember {
    key: String,
    key_start: usize,
    value: JsonNode,
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    offset: usize,
    base: usize,
    nodes: usize,
    max_depth: usize,
    max_string: usize,
    context: &'a ExecutionContext,
    next_checkpoint: usize,
}

fn convert_json(
    source: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Document, ConversionError> {
    context.checkpoint()?;
    let _memory = context.reserve_memory(u64::try_from(source.len()).unwrap_or(u64::MAX))?;
    let (bytes, base) = if let Some(rest) = source.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        (rest, 3)
    } else {
        (source, 0)
    };
    let _ = std::str::from_utf8(bytes)
        .map_err(|error| malformed(InputFormat::Json, format!("JSON must be UTF-8: {error}")))?;
    if super::scan_json(bytes, context)?.status != super::JsonScanStatus::Complete {
        return Err(malformed(InputFormat::Json, "JSON syntax is incomplete or invalid"));
    }
    let mut parser = JsonParser {
        bytes,
        offset: 0,
        base,
        nodes: 0,
        max_depth: usize::from(options.limits.max_nesting_depth),
        max_string: usize::try_from(options.limits.max_field_bytes).unwrap_or(usize::MAX),
        context,
        next_checkpoint: 0,
    };
    let root = parser.value(0)?;
    parser.space()?;
    if parser.offset != bytes.len() {
        return Err(malformed(
            InputFormat::Json,
            format!("trailing content at byte {}", parser.offset + base),
        ));
    }
    let mut builder = IrBuilder::new(InputFormat::Json);
    builder.heading(1, "JSON".into(), root.start, root.end)?;
    emit_json(&root, "$", 1, &mut builder)?;
    builder.finish()
}

impl JsonParser<'_> {
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
    fn value(&mut self, depth: usize) -> Result<JsonNode, ConversionError> {
        self.space()?;
        if depth > self.max_depth {
            return Err(ConversionError::ResourceLimit {
                limit: "json_nesting_depth",
                detail: format!("JSON exceeds {} levels", self.max_depth),
            });
        }
        self.nodes = self.nodes.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "json_nodes",
            detail: "JSON node count overflowed".into(),
        })?;
        if self.nodes > MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "json_nodes",
                detail: format!("JSON exceeds {MAX_DOCUMENT_NODES} nodes"),
            });
        }
        let start = self.offset + self.base;
        let value = match self.bytes.get(self.offset).copied() {
            Some(b'{') => self.object(depth)?,
            Some(b'[') => self.array(depth)?,
            Some(b'"') => {
                let string = self.string()?.0;
                JsonValue::Scalar {
                    display: serde_json::to_string(&string).map_err(|error| {
                        ConversionError::Internal {
                            detail: format!("JSON string display encoding failed: {error}"),
                        }
                    })?,
                }
            }
            Some(b'-' | b'0'..=b'9') => JsonValue::Scalar { display: self.number()? },
            Some(b't') => {
                self.literal(b"true")?;
                JsonValue::Scalar { display: "true".into() }
            }
            Some(b'f') => {
                self.literal(b"false")?;
                JsonValue::Scalar { display: "false".into() }
            }
            Some(b'n') => {
                self.literal(b"null")?;
                JsonValue::Scalar { display: "null".into() }
            }
            _ => {
                return Err(malformed(
                    InputFormat::Json,
                    format!("expected a value at byte {}", self.offset + self.base),
                ));
            }
        };
        Ok(JsonNode { value, start, end: self.offset + self.base })
    }
    fn object(&mut self, depth: usize) -> Result<JsonValue, ConversionError> {
        self.offset += 1;
        self.space()?;
        let mut members = Vec::new();
        let mut keys = BTreeSet::new();
        if self.take(b'}') {
            return Ok(JsonValue::Object(members));
        }
        loop {
            self.space()?;
            let key_start = self.offset + self.base;
            let (key, _) = self.string()?;
            if !keys.insert(key.clone()) {
                return Err(malformed(
                    InputFormat::Json,
                    format!("duplicate object key {key:?} at byte {key_start}"),
                ));
            }
            self.space()?;
            self.expect(b':')?;
            let value = self.value(depth + 1)?;
            members.push(JsonMember { key, key_start, value });
            self.space()?;
            if self.take(b'}') {
                break;
            }
            self.expect(b',')?;
            self.space()?;
            if self.bytes.get(self.offset) == Some(&b'}') {
                return Err(malformed(
                    InputFormat::Json,
                    format!("trailing comma at byte {}", self.offset + self.base),
                ));
            }
        }
        Ok(JsonValue::Object(members))
    }
    fn array(&mut self, depth: usize) -> Result<JsonValue, ConversionError> {
        self.offset += 1;
        self.space()?;
        let mut values = Vec::new();
        if self.take(b']') {
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.value(depth + 1)?);
            self.space()?;
            if self.take(b']') {
                break;
            }
            self.expect(b',')?;
            self.space()?;
            if self.bytes.get(self.offset) == Some(&b']') {
                return Err(malformed(
                    InputFormat::Json,
                    format!("trailing comma at byte {}", self.offset + self.base),
                ));
            }
        }
        Ok(JsonValue::Array(values))
    }
    fn string(&mut self) -> Result<(String, usize), ConversionError> {
        let start = self.offset;
        self.expect(b'"')?;
        let mut output = String::new();
        while let Some(byte) = self.bytes.get(self.offset).copied() {
            self.checkpoint()?;
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok((output, start));
                }
                0x00..=0x1f => {
                    return Err(malformed(
                        InputFormat::Json,
                        format!("unescaped control character at byte {}", self.offset + self.base),
                    ));
                }
                b'\\' => {
                    self.offset += 1;
                    self.escape(&mut output)?;
                }
                _ => {
                    let text =
                        std::str::from_utf8(&self.bytes[self.offset..]).map_err(|error| {
                            malformed(
                                InputFormat::Json,
                                format!(
                                    "invalid UTF-8 at byte {}: {error}",
                                    self.offset + self.base
                                ),
                            )
                        })?;
                    let ch = text
                        .chars()
                        .next()
                        .ok_or_else(|| malformed(InputFormat::Json, "unterminated string"))?;
                    output.push(ch);
                    self.offset += ch.len_utf8();
                }
            }
            if output.len() > self.max_string {
                return Err(ConversionError::ResourceLimit {
                    limit: "json_string_bytes",
                    detail: format!("decoded JSON string exceeds {} bytes", self.max_string),
                });
            }
        }
        Err(malformed(
            InputFormat::Json,
            format!("unterminated string at byte {}", start + self.base),
        ))
    }
    fn escape(&mut self, output: &mut String) -> Result<(), ConversionError> {
        let at = self.offset + self.base;
        let byte = *self.bytes.get(self.offset).ok_or_else(|| {
            malformed(InputFormat::Json, format!("unterminated escape at byte {at}"))
        })?;
        self.offset += 1;
        match byte {
            b'"' => output.push('"'),
            b'\\' => output.push('\\'),
            b'/' => output.push('/'),
            b'b' => output.push('\u{8}'),
            b'f' => output.push('\u{c}'),
            b'n' => output.push('\n'),
            b'r' => output.push('\r'),
            b't' => output.push('\t'),
            b'u' => {
                let first = self.hex4()?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    if self.bytes.get(self.offset..self.offset + 2) != Some(b"\\u") {
                        return Err(malformed(
                            InputFormat::Json,
                            format!("high surrogate without low surrogate at byte {at}"),
                        ));
                    }
                    self.offset += 2;
                    let second = self.hex4()?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err(malformed(
                            InputFormat::Json,
                            format!("invalid low surrogate at byte {at}"),
                        ));
                    }
                    0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&first) {
                    return Err(malformed(
                        InputFormat::Json,
                        format!("unpaired low surrogate at byte {at}"),
                    ));
                } else {
                    u32::from(first)
                };
                output.push(char::from_u32(scalar).ok_or_else(|| {
                    malformed(InputFormat::Json, format!("invalid Unicode scalar at byte {at}"))
                })?);
            }
            _ => return Err(malformed(InputFormat::Json, format!("invalid escape at byte {at}"))),
        }
        Ok(())
    }
    fn hex4(&mut self) -> Result<u16, ConversionError> {
        let start = self.offset;
        let end = start
            .checked_add(4)
            .ok_or_else(|| malformed(InputFormat::Json, "escape offset overflow"))?;
        let digits = self.bytes.get(start..end).ok_or_else(|| {
            malformed(
                InputFormat::Json,
                format!("short Unicode escape at byte {}", start + self.base),
            )
        })?;
        let mut value = 0_u16;
        for digit in digits {
            value = value * 16
                + u16::from(match digit {
                    b'0'..=b'9' => digit - b'0',
                    b'a'..=b'f' => digit - b'a' + 10,
                    b'A'..=b'F' => digit - b'A' + 10,
                    _ => {
                        return Err(malformed(
                            InputFormat::Json,
                            format!("invalid Unicode escape at byte {}", start + self.base),
                        ));
                    }
                });
        }
        self.offset = end;
        Ok(value)
    }
    fn number(&mut self) -> Result<String, ConversionError> {
        let start = self.offset;
        self.take(b'-');
        match self.bytes.get(self.offset) {
            Some(b'0') => {
                self.offset += 1;
                if self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                    return Err(malformed(
                        InputFormat::Json,
                        format!("leading zero at byte {}", self.offset + self.base),
                    ));
                }
            }
            Some(b'1'..=b'9') => {
                while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                    self.offset += 1;
                }
            }
            _ => {
                return Err(malformed(
                    InputFormat::Json,
                    format!("invalid number at byte {}", start + self.base),
                ));
            }
        }
        if self.take(b'.') {
            let digits = self.offset;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if digits == self.offset {
                return Err(malformed(
                    InputFormat::Json,
                    format!("missing fractional digits at byte {}", self.offset + self.base),
                ));
            }
        }
        if self.bytes.get(self.offset).is_some_and(|b| matches!(b, b'e' | b'E')) {
            self.offset += 1;
            if self.bytes.get(self.offset).is_some_and(|b| matches!(b, b'+' | b'-')) {
                self.offset += 1;
            }
            let digits = self.offset;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if digits == self.offset {
                return Err(malformed(
                    InputFormat::Json,
                    format!("missing exponent digits at byte {}", self.offset + self.base),
                ));
            }
        }
        Ok(std::str::from_utf8(&self.bytes[start..self.offset])
            .expect("number lexeme is ASCII")
            .into())
    }
    fn literal(&mut self, literal: &[u8]) -> Result<(), ConversionError> {
        if self.bytes.get(self.offset..self.offset + literal.len()) == Some(literal) {
            self.offset += literal.len();
            Ok(())
        } else {
            Err(malformed(
                InputFormat::Json,
                format!("invalid literal at byte {}", self.offset + self.base),
            ))
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

fn emit_json(
    node: &JsonNode,
    path: &str,
    depth: usize,
    builder: &mut IrBuilder,
) -> Result<(), ConversionError> {
    match &node.value {
        JsonValue::Scalar { display } => builder.code(None, display.clone(), node.start, node.end),
        JsonValue::Object(members) => {
            if members.is_empty() {
                builder.code(Some("json"), "{}".into(), node.start, node.end)?;
            }
            for member in members {
                let member_path = format!("{path}.{}", member.key);
                if let JsonValue::Scalar { display } = &member.value.value {
                    builder.paragraph(
                        vec![
                            Inline::Code(member.key.clone()),
                            Inline::Text { value: ": ".into(), marks: Vec::new() },
                            Inline::Code(display.clone()),
                        ],
                        member.key_start,
                        member.value.end,
                    )?;
                } else {
                    builder.heading(
                        u8::try_from((depth + 1).min(6)).unwrap_or(6),
                        member_path.clone(),
                        member.key_start,
                        member.value.end,
                    )?;
                    emit_json(&member.value, &member_path, depth + 1, builder)?;
                }
            }
            Ok(())
        }
        JsonValue::Array(values) => {
            if values.is_empty() {
                builder.code(Some("json"), "[]".into(), node.start, node.end)?;
            }
            for (index, value) in values.iter().enumerate() {
                let item_path = format!("{path}[{index}]");
                if let JsonValue::Scalar { display } = &value.value {
                    builder.paragraph(
                        vec![
                            Inline::Code(format!("[{index}]")),
                            Inline::Text { value: ": ".into(), marks: Vec::new() },
                            Inline::Code(display.clone()),
                        ],
                        value.start,
                        value.end,
                    )?;
                } else {
                    builder.heading(
                        u8::try_from((depth + 1).min(6)).unwrap_or(6),
                        item_path.clone(),
                        value.start,
                        value.end,
                    )?;
                    emit_json(value, &item_path, depth + 1, builder)?;
                }
            }
            Ok(())
        }
    }
}

#[derive(Debug)]
struct XmlFrame {
    qname: String,
    expanded: String,
}

enum XmlOffsetMap {
    Utf8 { bom: usize },
    Utf16(Vec<usize>),
}

impl XmlOffsetMap {
    fn raw(&self, decoded: usize) -> Result<usize, ConversionError> {
        match self {
            Self::Utf8 { bom } => {
                decoded.checked_add(*bom).ok_or_else(|| ConversionError::ResourceLimit {
                    limit: "max_input_bytes",
                    detail: "XML source offset overflowed".into(),
                })
            }
            Self::Utf16(map) => {
                map.get(decoded).copied().ok_or_else(|| ConversionError::Internal {
                    detail: "decoded XML offset has no original-byte mapping".into(),
                })
            }
        }
    }
}

fn decode_xml_source(
    source: &[u8],
) -> Result<(String, XmlOffsetMap, &'static str), ConversionError> {
    let (endian, start) = if source.starts_with(&[0xff, 0xfe]) {
        (Some(true), 2)
    } else if source.starts_with(&[0xfe, 0xff]) {
        (Some(false), 2)
    } else if source.starts_with(&[b'<', 0, b'?', 0]) {
        (Some(true), 0)
    } else if source.starts_with(&[0, b'<', 0, b'?']) {
        (Some(false), 0)
    } else {
        (None, if source.starts_with(&[0xef, 0xbb, 0xbf]) { 3 } else { 0 })
    };
    let Some(little) = endian else {
        let text = std::str::from_utf8(&source[start..]).map_err(|error| {
            malformed(InputFormat::Xml, format!("XML must be UTF-8 or UTF-16: {error}"))
        })?;
        return Ok((text.to_owned(), XmlOffsetMap::Utf8 { bom: start }, "utf-8"));
    };
    if !(source.len() - start).is_multiple_of(2) {
        return Err(malformed(InputFormat::Xml, "UTF-16 XML has a trailing byte"));
    }
    let mut text = String::new();
    let mut map = vec![start];
    let mut offset = start;
    while offset < source.len() {
        let read = |at| {
            if little {
                u16::from_le_bytes([source[at], source[at + 1]])
            } else {
                u16::from_be_bytes([source[at], source[at + 1]])
            }
        };
        let unit = read(offset);
        let raw_start = offset;
        offset += 2;
        let scalar = if (0xd800..=0xdbff).contains(&unit) {
            if offset + 1 >= source.len() {
                return Err(malformed(InputFormat::Xml, "UTF-16 XML ends after a high surrogate"));
            }
            let low = read(offset);
            if !(0xdc00..=0xdfff).contains(&low) {
                return Err(malformed(
                    InputFormat::Xml,
                    "UTF-16 XML contains an invalid surrogate pair",
                ));
            }
            offset += 2;
            0x1_0000 + ((u32::from(unit) - 0xd800) << 10) + u32::from(low) - 0xdc00
        } else if (0xdc00..=0xdfff).contains(&unit) {
            return Err(malformed(
                InputFormat::Xml,
                "UTF-16 XML contains an unpaired low surrogate",
            ));
        } else {
            u32::from(unit)
        };
        let character = char::from_u32(scalar)
            .ok_or_else(|| malformed(InputFormat::Xml, "UTF-16 XML contains an invalid scalar"))?;
        text.push(character);
        for _ in 1..character.len_utf8() {
            map.push(raw_start);
        }
        map.push(offset);
    }
    Ok((text, XmlOffsetMap::Utf16(map), if little { "utf-16le" } else { "utf-16be" }))
}

pub(super) fn decode_xml_for_detection(source: &[u8]) -> Option<String> {
    let utf16 = source.starts_with(&[0xff, 0xfe])
        || source.starts_with(&[0xfe, 0xff])
        || source.starts_with(&[b'<', 0, b'?', 0])
        || source.starts_with(&[0, b'<', 0, b'?']);
    utf16.then(|| decode_xml_source(source).ok().map(|value| value.0)).flatten()
}

#[allow(clippy::too_many_lines)]
fn convert_xml(
    source: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Document, ConversionError> {
    let _memory = context.reserve_memory(u64::try_from(source.len()).unwrap_or(u64::MAX))?;
    let (text, offsets, actual_encoding) = decode_xml_source(source)?;
    reject_xml_encoding_conflict(&text, actual_encoding)?;
    let mut reader = NsReader::from_str(&text);
    {
        let config = reader.config_mut();
        config.allow_dangling_amp = false;
        config.allow_unmatched_ends = false;
        config.check_end_names = true;
        config.check_comments = true;
    }
    let mut builder = IrBuilder::new(InputFormat::Xml);
    let mut stack: Vec<XmlFrame> = Vec::new();
    let mut root_seen = false;
    let mut events = 0_usize;
    let mut text_bytes = 0_usize;
    let mut previous = 0_usize;
    loop {
        context.checkpoint()?;
        let event = reader.read_event().map_err(|error| {
            malformed(
                InputFormat::Xml,
                format!(
                    "invalid XML at byte {}: {error}",
                    offsets.raw(previous).unwrap_or(previous)
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
                let (qname, expanded) = resolved_element(&reader, &element)?;
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
                    qname.clone(),
                    offsets.raw(start)?,
                    offsets.raw(end)?,
                )?;
                emit_xml_attributes(
                    &reader,
                    &element,
                    offsets.raw(start)?,
                    offsets.raw(end)?,
                    options,
                    &mut builder,
                )?;
                if !empty {
                    stack.push(XmlFrame { qname, expanded });
                }
            }
            Event::End(element) => {
                let expected = stack.pop().ok_or_else(|| {
                    malformed(InputFormat::Xml, "XML end tag has no open element")
                })?;
                let raw = std::str::from_utf8(element.name().as_ref())
                    .map_err(|_| malformed(InputFormat::Xml, "XML name is not UTF-8"))?
                    .to_owned();
                let (_, expanded) = resolved_end(&reader, element.name())?;
                if raw != expected.qname || expanded != expected.expanded {
                    return Err(malformed(
                        InputFormat::Xml,
                        format!("mismatched closing tag </{raw}>"),
                    ));
                }
            }
            Event::Text(value) => {
                let decoded = value.xml_content().map_err(|error| {
                    malformed(InputFormat::Xml, format!("invalid XML text: {error}"))
                })?;
                if stack.is_empty() && !decoded.chars().all(char::is_whitespace) {
                    return Err(malformed(
                        InputFormat::Xml,
                        "character data appears outside the root",
                    ));
                }
                if !decoded.is_empty() && !stack.is_empty() {
                    add_xml_text(
                        decoded.into_owned(),
                        offsets.raw(start)?,
                        offsets.raw(end)?,
                        &mut text_bytes,
                        options,
                        &mut builder,
                    )?;
                }
            }
            Event::CData(value) => {
                let decoded = value
                    .decode()
                    .map_err(|error| {
                        malformed(InputFormat::Xml, format!("invalid CDATA: {error}"))
                    })?
                    .into_owned();
                if stack.is_empty() && !decoded.is_empty() {
                    return Err(malformed(InputFormat::Xml, "CDATA appears outside the root"));
                }
                if !decoded.is_empty() && !stack.is_empty() {
                    add_xml_text(
                        decoded,
                        offsets.raw(start)?,
                        offsets.raw(end)?,
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
                builder.metadata.insert(
                    format!("xml.comment.{:06}", builder.metadata.len() + 1),
                    decoded.into_owned(),
                );
            }
            Event::PI(value) => {
                let decoded = reader.decoder().decode(value.as_ref()).map_err(|error| {
                    malformed(InputFormat::Xml, format!("invalid processing instruction: {error}"))
                })?;
                builder.code(
                    Some("xml-processing-instruction"),
                    format!("<?{decoded}?>"),
                    offsets.raw(start)?,
                    offsets.raw(end)?,
                )?;
            }
            Event::Decl(decl) => {
                if start != 0 {
                    return Err(malformed(InputFormat::Xml, "XML declaration must be first"));
                }
                let _ = decl;
            }
            Event::DocType(_) => {
                return Err(malformed(InputFormat::Xml, "DOCTYPE and DTD are not allowed"));
            }
            Event::GeneralRef(reference) => {
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
                    offsets.raw(start)?,
                    offsets.raw(end)?,
                    &mut text_bytes,
                    options,
                    &mut builder,
                )?;
            }
            Event::Eof => break,
        }
    }
    if !root_seen || !stack.is_empty() {
        return Err(malformed(InputFormat::Xml, "XML root is missing or incomplete"));
    }
    builder.finish()
}

fn reject_xml_encoding_conflict(text: &str, actual: &str) -> Result<(), ConversionError> {
    let prefix = &text[..text.len().min(256)];
    if let Some(decl) = prefix.strip_prefix("<?xml")
        && let Some(pos) = decl.find("encoding")
    {
        let tail = &decl[pos + "encoding".len()..];
        let quote =
            tail.find(['\'', '"']).and_then(|i| tail.as_bytes().get(i).copied().map(|q| (i, q)));
        if let Some((at, quote)) = quote
            && let Some(end) = tail[at + 1..].bytes().position(|b| b == quote)
        {
            let label = &tail[at + 1..at + 1 + end];
            let matches = match actual {
                "utf-8" => {
                    label.eq_ignore_ascii_case("utf-8") || label.eq_ignore_ascii_case("utf8")
                }
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
    }
    Ok(())
}

fn resolved_element(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
) -> Result<(String, String), ConversionError> {
    let qname = std::str::from_utf8(element.name().as_ref())
        .map_err(|_| malformed(InputFormat::Xml, "XML QName is not UTF-8"))?
        .to_owned();
    let (namespace, local) = reader.resolve_element(element.name());
    let namespace = namespace_uri(namespace)?;
    let local = std::str::from_utf8(local.as_ref())
        .map_err(|_| malformed(InputFormat::Xml, "XML local name is not UTF-8"))?;
    Ok((qname, format!("{{{}}}{local}", namespace.unwrap_or_default())))
}

fn resolved_end(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
) -> Result<(String, String), ConversionError> {
    let qname = std::str::from_utf8(name.as_ref())
        .map_err(|_| malformed(InputFormat::Xml, "XML QName is not UTF-8"))?
        .to_owned();
    let (namespace, local) = reader.resolve_element(name);
    let namespace = namespace_uri(namespace)?;
    let local = std::str::from_utf8(local.as_ref())
        .map_err(|_| malformed(InputFormat::Xml, "XML local name is not UTF-8"))?;
    Ok((qname, format!("{{{}}}{local}", namespace.unwrap_or_default())))
}

fn namespace_uri(value: ResolveResult<'_>) -> Result<Option<String>, ConversionError> {
    match value {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(uri) => Ok(Some(
            std::str::from_utf8(uri.as_ref())
                .map_err(|_| malformed(InputFormat::Xml, "namespace URI is not UTF-8"))?
                .to_owned(),
        )),
        ResolveResult::Unknown(prefix) => Err(malformed(
            InputFormat::Xml,
            format!("unbound namespace prefix {:?}", String::from_utf8_lossy(&prefix)),
        )),
    }
}

fn emit_xml_attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    start: usize,
    end: usize,
    options: &ConversionOptions,
    builder: &mut IrBuilder,
) -> Result<(), ConversionError> {
    let mut lines = Vec::new();
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
        let value = attribute.decode_and_unescape_value(reader.decoder()).map_err(|error| {
            malformed(InputFormat::Xml, format!("invalid attribute {raw:?}: {error}"))
        })?;
        if value.len() > usize::try_from(options.limits.max_field_bytes).unwrap_or(usize::MAX) {
            return Err(ConversionError::ResourceLimit {
                limit: "xml_attribute_bytes",
                detail: format!(
                    "attribute exceeds {} decoded bytes",
                    options.limits.max_field_bytes
                ),
            });
        }
        if raw == "xmlns" || raw.starts_with("xmlns:") {
            lines.push(format!("{raw} = {value:?}"));
            continue;
        }
        let (namespace, local) = reader.resolve_attribute(attribute.key);
        let uri = namespace_uri(namespace)?.unwrap_or_default();
        let identity = format!("{{{uri}}}{}", String::from_utf8_lossy(local.as_ref()));
        if !expanded.insert(identity.clone()) {
            return Err(malformed(
                InputFormat::Xml,
                format!("duplicate expanded attribute name {identity}"),
            ));
        }
        let prefix = raw.split_once(':').map_or("", |(prefix, _)| prefix);
        lines.push(format!(
            "{raw} (local={}, prefix={prefix:?}, namespace={uri:?}) = {value:?}",
            String::from_utf8_lossy(local.as_ref())
        ));
    }
    if !lines.is_empty() {
        builder.code(Some("xml-attributes"), lines.join("\n"), start, end)?;
    }
    Ok(())
}

fn predefined_or_numeric_entity(raw: &str) -> Result<String, ConversionError> {
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
    Ok(value.to_string())
}

fn xml_char(value: char) -> bool {
    matches!(value as u32, 0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x1_0000..=0x10_ffff)
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
    builder.paragraph(vec![Inline::Text { value, marks: Vec::new() }], start, end)
}

struct IrBuilder {
    format: InputFormat,
    blocks: Vec<BlockNode>,
    next_id: usize,
    metadata: BTreeMap<String, String>,
}
impl IrBuilder {
    fn new(format: InputFormat) -> Self {
        Self { format, blocks: Vec::new(), next_id: 1, metadata: BTreeMap::new() }
    }
    fn push(&mut self, block: Block, start: usize, end: usize) -> Result<(), ConversionError> {
        if self.blocks.len() >= MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "document_nodes",
                detail: format!("output exceeds {MAX_DOCUMENT_NODES} blocks"),
            });
        }
        let id = NodeId(format!("{}-{}", self.format.as_str(), self.next_id));
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
        self.push(
            Block::Heading { level, content: vec![Inline::Text { value, marks: Vec::new() }] },
            start,
            end,
        )
    }
    fn paragraph(
        &mut self,
        value: Vec<Inline>,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        self.push(Block::Paragraph(value), start, end)
    }
    fn code(
        &mut self,
        language: Option<&str>,
        text: String,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        self.push(Block::Code { language: language.map(str::to_owned), text }, start, end)
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
    }
    #[test]
    fn xml_preserves_namespace_mixed_content_and_rejects_dtd() {
        let document = convert_xml(
            include_bytes!("../tests/fixtures/xml/mixed-namespace.xml"),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert!(document.blocks.iter().any(|node| matches!(&node.block, Block::Heading { content, .. } if matches!(&content[0], Inline::Text { value, .. } if value == "a:item"))));
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
    }

    #[test]
    fn damaged_structures_are_protected_without_swallowing_prose() {
        let context = context();
        assert_eq!(
            super::super::structured_text_candidate(br#"{ "a":}"#, &context)
                .unwrap()
                .unwrap()
                .format,
            InputFormat::Json
        );
        assert!(super::super::structured_text_candidate(b"{hello}", &context).unwrap().is_none());
        assert_eq!(
            super::super::structured_text_candidate(b"<r></x>", &context).unwrap().unwrap().format,
            InputFormat::Xml
        );
        assert!(
            super::super::structured_text_candidate(b"<3 is less", &context).unwrap().is_none()
        );
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
    }
}
