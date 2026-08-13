//! Bounded, non-executing Rich Text Format conversion.

use encoding_rs::{BIG5, Encoding, GBK, SHIFT_JIS, WINDOWS_1252};
use image::{
    ImageDecoder as _, Limits as ImageLimits,
    codecs::{jpeg::JpegDecoder, png::PngDecoder},
};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ExecutionContext,
    FormatCandidate, Inline, InlineMark, InputFormat, ListItem, ListKind, MAX_DOCUMENT_INLINES,
    MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS, NodeId, ProbeOutcome, Provenance, ProvenanceKind,
    ResolvedInput, ResourceReservation, Services, SourceLocator, TableRow,
    canonical_external_asset_uri,
};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::mem::size_of;

const FORMATS: &[InputFormat] = &[InputFormat::Rtf];
const PROVIDER_ID: &str = "builtin.converter.rtf";
const MAX_CONTROLS: u64 = 1_000_000;
const MAX_NUMERIC_DIGITS: usize = 10;
const MAX_DIAGNOSTICS: usize = 4096;
const CHECKPOINT_INTERVAL: usize = 4096;
const MAX_METADATA_BYTES: usize = 64 * 1024;

/// Strict, offline RTF converter. Embedded objects and active destinations are never executed.
#[derive(Debug, Default)]
pub struct RtfConverter;

impl Converter for RtfConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        240
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
            if candidate.format != InputFormat::Rtf {
                return Ok(ProbeOutcome::NotApplicable);
            }
            Ok(if strict_header(&input.bytes).is_some() {
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
        Box::pin(async move { Parser::new(&input.bytes, options, context)?.parse() })
    }
}

fn strict_header(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(b"{\\rtf") {
        return None;
    }
    let mut offset = 5;
    let first = *bytes.get(offset)?;
    if !first.is_ascii_digit() {
        return None;
    }
    while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
        offset += 1;
        if offset - 5 > MAX_NUMERIC_DIGITS {
            return None;
        }
    }
    matches!(bytes.get(offset), Some(b' ' | b'\\' | b'{' | b'}' | b'\r' | b'\n')).then_some(offset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Destination {
    Body,
    Skip,
    FontTable,
    Pict,
    ListText,
    MetaTitle,
    MetaAuthor,
    FieldInstruction,
    FieldResult,
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // These independent RTF character flags inherit by group.
struct State {
    destination: Destination,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    superscript: bool,
    subscript: bool,
    ansi_codepage: u16,
    font: i32,
    unicode_skip: u8,
    fallback_remaining: u8,
    ignorable: bool,
    at_group_start: bool,
    in_table: bool,
    list_id: Option<i32>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            destination: Destination::Body,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            superscript: false,
            subscript: false,
            ansi_codepage: 1252,
            font: 0,
            unicode_skip: 1,
            fallback_remaining: 0,
            ignorable: false,
            at_group_start: true,
            in_table: false,
            list_id: None,
        }
    }
}

#[derive(Debug)]
struct Frame {
    state: State,
    start: usize,
    introduced: Option<Destination>,
}

#[derive(Debug, Default)]
struct Paragraph {
    inlines: Vec<Inline>,
    start: Option<usize>,
    end: usize,
}

#[derive(Debug, Default)]
struct TableBuilder {
    rows: Vec<TableRow>,
    cells: Vec<Cell>,
    cell_blocks: Vec<BlockNode>,
    cell_definitions: Vec<CellMerge>,
    cell_definition_index: usize,
    pending_cell_merge: CellMerge,
    active: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum CellMerge {
    #[default]
    Normal,
    Start,
    Continue,
}

#[derive(Debug, Default)]
struct Picture {
    bytes: Vec<u8>,
    media_type: Option<&'static str>,
    start: usize,
    saw_odd_nibble: bool,
    high_nibble: Option<u8>,
}

struct Parser<'a> {
    bytes: &'a [u8],
    options: &'a ConversionOptions,
    context: &'a ExecutionContext,
    offset: usize,
    frames: Vec<Frame>,
    fallback_state: State,
    root_closed: bool,
    paragraph: Paragraph,
    table: TableBuilder,
    blocks: Vec<BlockNode>,
    assets: Vec<Asset>,
    diagnostics: Vec<Diagnostic>,
    metadata: into_markdown_core::DocumentMetadata,
    font_charsets: BTreeMap<i32, u16>,
    font_table_font: Option<i32>,
    capture: String,
    picture: Option<Picture>,
    pending_list_marker: Option<String>,
    pending_link: Option<String>,
    active_link: Option<String>,
    field_inline_start: Option<usize>,
    pending_high_surrogate: Option<(u16, usize, usize)>,
    node_sequence: u64,
    control_count: u64,
    decoded_bytes: u64,
    total_asset_bytes: u64,
    table_cells: u64,
    memory: ResourceReservation,
}

impl<'a> Parser<'a> {
    fn new(
        bytes: &'a [u8],
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
    ) -> Result<Self, ConversionError> {
        context.checkpoint()?;
        let input_len = u64::try_from(bytes.len())
            .map_err(|_| limit("max_input_bytes", "RTF size overflow"))?;
        if input_len > options.limits.max_input_bytes {
            return Err(limit(
                "max_input_bytes",
                format!("{input_len} > {}", options.limits.max_input_bytes),
            ));
        }
        if options.limits.max_nesting_depth == 0 {
            return Err(limit("max_nesting_depth", "RTF root requires depth 1"));
        }
        if strict_header(bytes).is_none() {
            return Err(malformed("RTF header must begin with {\\rtfN and a delimiter"));
        }
        let mut memory = context.reserve_memory(0)?;
        let mut frames = Vec::new();
        reserve_vec(&mut frames, 1, &mut memory)?;
        frames.push(Frame { state: State::default(), start: 0, introduced: None });
        Ok(Self {
            bytes,
            options,
            context,
            offset: 1,
            frames,
            fallback_state: State::default(),
            root_closed: false,
            paragraph: Paragraph::default(),
            table: TableBuilder::default(),
            blocks: Vec::new(),
            assets: Vec::new(),
            diagnostics: Vec::new(),
            metadata: into_markdown_core::DocumentMetadata::default(),
            font_charsets: BTreeMap::new(),
            font_table_font: None,
            capture: String::new(),
            picture: None,
            pending_list_marker: None,
            pending_link: None,
            active_link: None,
            field_inline_start: None,
            pending_high_surrogate: None,
            node_sequence: 0,
            control_count: 0,
            decoded_bytes: 0,
            total_asset_bytes: 0,
            table_cells: 0,
            memory,
        })
    }

    fn parse(mut self) -> Result<ConverterOutput, ConversionError> {
        while self.offset < self.bytes.len() {
            if self.root_closed {
                if self.bytes[self.offset..].iter().all(u8::is_ascii_whitespace) {
                    self.offset = self.bytes.len();
                    break;
                }
                return Err(malformed("non-whitespace data follows the root RTF group"));
            }
            if self.offset.is_multiple_of(CHECKPOINT_INTERVAL) {
                self.context.checkpoint()?;
            }
            match self.bytes[self.offset] {
                b'{' => self.open_group()?,
                b'}' => self.close_group()?,
                b'\\' => self.control()?,
                b'\r' | b'\n' => self.offset += 1,
                _ => self.plain_text()?,
            }
        }
        if !self.root_closed || self.frames.len() != 1 {
            return Err(malformed("unterminated RTF group"));
        }
        self.flush_pending_surrogate()?;
        self.finish_table_or_paragraph(self.bytes.len())?;
        if self.blocks.is_empty() && self.assets.is_empty() {
            self.add_diagnostic(
                "rtf.emptyDocument",
                DiagnosticSeverity::Info,
                "RTF contains no displayable content",
                Some(locator(0, self.bytes.len())),
            )?;
        }
        let document =
            Document { metadata: self.metadata, blocks: self.blocks, ..Document::default() };
        document.validate().map_err(|error| ConversionError::Internal {
            detail: format!("RTF converter produced invalid IR: {error}"),
        })?;
        Ok(ConverterOutput { document, assets: self.assets, diagnostics: self.diagnostics })
    }

    fn state(&self) -> &State {
        self.frames.last().map_or(&self.fallback_state, |frame| &frame.state)
    }

    fn state_mut(&mut self) -> &mut State {
        self.frames.last_mut().map_or(&mut self.fallback_state, |frame| &mut frame.state)
    }

    fn open_group(&mut self) -> Result<(), ConversionError> {
        let depth = self
            .frames
            .len()
            .checked_add(1)
            .ok_or_else(|| limit("max_nesting_depth", "group depth overflow"))?;
        if depth > usize::from(self.options.limits.max_nesting_depth) {
            return Err(limit(
                "max_nesting_depth",
                format!("{depth} > {}", self.options.limits.max_nesting_depth),
            ));
        }
        let state = self.state().clone();
        reserve_vec(&mut self.frames, 1, &mut self.memory)?;
        self.frames.push(Frame {
            state: State { at_group_start: true, ignorable: false, ..state },
            start: self.offset,
            introduced: None,
        });
        self.offset += 1;
        Ok(())
    }

    fn close_group(&mut self) -> Result<(), ConversionError> {
        self.flush_pending_surrogate()?;
        if self.frames.len() <= 1 {
            let (introduced, start) = self
                .frames
                .last()
                .map(|frame| (frame.introduced, frame.start))
                .ok_or_else(|| malformed("unexpected closing brace"))?;
            self.offset += 1;
            self.finish_destination(introduced, start, self.offset)?;
            self.finish_table_or_paragraph(self.offset)?;
            self.root_closed = true;
            return Ok(());
        }
        let frame = self.frames.pop().ok_or_else(|| malformed("group stack underflow"))?;
        self.offset += 1;
        self.finish_destination(frame.introduced, frame.start, self.offset)
    }

    fn finish_destination(
        &mut self,
        introduced: Option<Destination>,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        match introduced {
            Some(Destination::Pict) => self.finish_picture(end),
            Some(Destination::ListText) => {
                let trimmed = self.capture.trim();
                let mut marker = String::new();
                reserve_string(&mut marker, trimmed.len(), &mut self.memory)?;
                marker.push_str(trimmed);
                self.capture.clear();
                if !marker.is_empty() {
                    self.pending_list_marker = Some(marker);
                }
                Ok(())
            }
            Some(Destination::MetaTitle) => {
                let trimmed = self.capture.trim();
                let mut title = String::new();
                reserve_string(&mut title, trimmed.len(), &mut self.memory)?;
                title.push_str(trimmed);
                self.capture.clear();
                if !title.is_empty() {
                    self.metadata.title = Some(title);
                }
                Ok(())
            }
            Some(Destination::MetaAuthor) => {
                let trimmed = self.capture.trim();
                let mut author = String::new();
                reserve_string(&mut author, trimmed.len(), &mut self.memory)?;
                author.push_str(trimmed);
                self.capture.clear();
                if !author.is_empty() {
                    reserve_vec(&mut self.metadata.authors, 1, &mut self.memory)?;
                    self.metadata.authors.push(author);
                }
                Ok(())
            }
            Some(Destination::FieldInstruction) => {
                let instruction = std::mem::take(&mut self.capture);
                // The canonical URL helper returns an owned string; prepay its maximum UTF-8 size.
                self.memory.grow(u64::try_from(instruction.len()).unwrap_or(u64::MAX))?;
                self.pending_link = safe_hyperlink(&instruction);
                if self.pending_link.is_none() && instruction.trim().starts_with("HYPERLINK") {
                    self.add_diagnostic(
                        "rtf.unsafeHyperlinkSkipped",
                        DiagnosticSeverity::Warning,
                        "unsafe or non-canonical hyperlink target was skipped",
                        Some(locator(start, end)),
                    )?;
                }
                Ok(())
            }
            Some(Destination::FieldResult) => self.finish_field_result(),
            _ => Ok(()),
        }
    }

    fn control(&mut self) -> Result<(), ConversionError> {
        let start = self.offset;
        self.offset += 1;
        let Some(&next) = self.bytes.get(self.offset) else {
            return Err(malformed("trailing RTF escape"));
        };
        self.control_count = self
            .control_count
            .checked_add(1)
            .ok_or_else(|| limit("rtf_control_count", "control count overflow"))?;
        if self.control_count.is_multiple_of(1024) {
            self.context.checkpoint()?;
        }
        if self.control_count > MAX_CONTROLS {
            return Err(limit(
                "rtf_control_count",
                format!("{} > {MAX_CONTROLS}", self.control_count),
            ));
        }
        if next.is_ascii_alphabetic() {
            let name_start = self.offset;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_alphabetic) {
                self.offset += 1;
            }
            let name = std::str::from_utf8(&self.bytes[name_start..self.offset])
                .map_err(|_| malformed("control word is not ASCII"))?;
            let mut negative = false;
            if self.bytes.get(self.offset) == Some(&b'-') {
                negative = true;
                self.offset += 1;
            }
            let digit_start = self.offset;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
                if self.offset - digit_start > MAX_NUMERIC_DIGITS {
                    return Err(limit(
                        "rtf_numeric_digits",
                        "control parameter has more than 10 digits",
                    ));
                }
            }
            let parameter = if self.offset > digit_start {
                let digits = std::str::from_utf8(&self.bytes[digit_start..self.offset])
                    .map_err(|_| malformed("control parameter is not ASCII"))?;
                let value = digits
                    .parse::<i64>()
                    .map_err(|_| limit("rtf_numeric_value", "control parameter overflow"))?;
                Some(if negative {
                    value
                        .checked_neg()
                        .ok_or_else(|| limit("rtf_numeric_value", "negative parameter overflow"))?
                } else {
                    value
                })
            } else {
                if negative {
                    return Err(malformed("minus sign is not followed by a control parameter"));
                }
                None
            };
            if self.bytes.get(self.offset) == Some(&b' ') {
                self.offset += 1;
            }
            self.control_word(name, parameter, start, self.offset)
        } else {
            self.offset += 1;
            self.control_symbol(next, start, self.offset)
        }
    }

    #[allow(clippy::too_many_lines)]
    fn control_word(
        &mut self,
        name: &str,
        parameter: Option<i64>,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        let at_start = self.state().at_group_start;
        if at_start && let Some(destination) = destination(name, self.state().ignorable) {
            self.enter_destination(destination, start)?;
        }
        self.state_mut().at_group_start = false;
        let destination = self.state().destination;

        if destination == Destination::Pict {
            match name {
                "pngblip" => self.picture.as_mut().map(|p| p.media_type = Some("image/png")),
                "jpegblip" => self.picture.as_mut().map(|p| p.media_type = Some("image/jpeg")),
                "emfblip" | "wmetafile" => self.picture.as_mut().map(|p| p.media_type = None),
                "bin" => {
                    let count = usize::try_from(
                        parameter.ok_or_else(|| malformed("bin requires a byte count"))?,
                    )
                    .map_err(|_| limit("max_asset_bytes", "bin byte count is invalid"))?;
                    self.picture_binary(count)?;
                    None
                }
                _ => None,
            };
            return Ok(());
        }
        if destination == Destination::FontTable {
            match name {
                "f" => self.font_table_font = Some(parameter_i32(parameter, "font number")?),
                "fcharset" => {
                    let charset = parameter_u16(parameter, "font charset")?;
                    if let Some(font) = self.font_table_font {
                        let codepage = font_charset_codepage(charset).ok_or_else(|| {
                            malformed(format!("unsupported RTF font charset {charset}"))
                        })?;
                        reserve_map_entry(&self.font_charsets, &mut self.memory)?;
                        self.font_charsets.insert(font, codepage);
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if matches!(destination, Destination::Skip) {
            return Ok(());
        }

        match name {
            "ansicpg" => {
                let codepage = parameter_u16(parameter, "ANSI codepage")?;
                encoding_for_codepage(codepage)?;
                self.state_mut().ansi_codepage = codepage;
            }
            "f" => self.state_mut().font = parameter_i32(parameter, "font number")?,
            "uc" => {
                let value = parameter.ok_or_else(|| malformed("uc requires a parameter"))?;
                self.state_mut().unicode_skip = u8::try_from(value)
                    .map_err(|_| limit("rtf_unicode_fallback", "uc must be in 0..=255"))?;
            }
            "u" => {
                let value = parameter.ok_or_else(|| malformed("u requires a parameter"))?;
                let signed = i16::try_from(value)
                    .map_err(|_| limit("rtf_unicode_value", "u must be a signed 16-bit value"))?;
                let unit = u16::from_ne_bytes(signed.to_ne_bytes());
                self.emit_unicode(unit, start, end)?;
                self.state_mut().fallback_remaining = self.state().unicode_skip;
            }
            "par" => {
                if destination == Destination::FieldResult {
                    self.finish_field_result()?;
                }
                self.finish_paragraph(end)?;
            }
            "line" => self.emit_inline(Inline::LineBreak, start, end)?,
            "tab" => self.emit_text("\t", start, end)?,
            "emdash" => self.emit_text("—", start, end)?,
            "endash" => self.emit_text("–", start, end)?,
            "bullet" => self.emit_text("•", start, end)?,
            "lquote" => self.emit_text("‘", start, end)?,
            "rquote" => self.emit_text("’", start, end)?,
            "ldblquote" => self.emit_text("“", start, end)?,
            "rdblquote" => self.emit_text("”", start, end)?,
            "b" => self.state_mut().bold = parameter.unwrap_or(1) != 0,
            "i" => self.state_mut().italic = parameter.unwrap_or(1) != 0,
            "ul" => self.state_mut().underline = parameter.unwrap_or(1) != 0,
            "ulnone" => self.state_mut().underline = false,
            "strike" => self.state_mut().strike = parameter.unwrap_or(1) != 0,
            "super" => {
                self.state_mut().superscript = true;
                self.state_mut().subscript = false;
            }
            "sub" => {
                self.state_mut().subscript = true;
                self.state_mut().superscript = false;
            }
            "nosupersub" => {
                self.state_mut().superscript = false;
                self.state_mut().subscript = false;
            }
            "plain" => {
                let state = self.state_mut();
                state.bold = false;
                state.italic = false;
                state.underline = false;
                state.strike = false;
                state.superscript = false;
                state.subscript = false;
            }
            "pard" => {
                if self.table.active {
                    self.finish_table(end)?;
                }
                let state = self.state_mut();
                state.in_table = false;
                state.list_id = None;
            }
            "intbl" => self.state_mut().in_table = parameter.unwrap_or(1) != 0,
            "ls" => self.state_mut().list_id = Some(parameter_i32(parameter, "list number")?),
            "trowd" => self.start_table_row(end)?,
            "cell" => self.finish_cell(end)?,
            "row" => self.finish_row(end)?,
            "clmgf" => self.table.pending_cell_merge = CellMerge::Start,
            "clmrg" => self.table.pending_cell_merge = CellMerge::Continue,
            "cellx" => {
                reserve_vec(&mut self.table.cell_definitions, 1, &mut self.memory)?;
                self.table.cell_definitions.push(self.table.pending_cell_merge);
                self.table.pending_cell_merge = CellMerge::Normal;
            }
            "rtf" | "ansi" | "deff" | "viewkind" | "field" | "trgaph" | "trleft" | "li" | "ri"
            | "fi" | "sa" | "sb" | "fs" | "cf" | "highlight" | "lang" | "langfe" | "langnp"
            | "rtlch" | "ltrch" | "rtlpar" | "ltrpar" | "keep" | "keepn" | "widctlpar"
            | "nowidctlpar" => {}
            _ if is_known_non_destination_control(name) => {}
            _ => self.add_diagnostic(
                "rtf.unknownControlIgnored",
                DiagnosticSeverity::Info,
                "unknown non-destination control word was ignored",
                Some(locator(start, end)),
            )?,
        }
        Ok(())
    }

    fn control_symbol(
        &mut self,
        symbol: u8,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        if symbol == b'*' && self.state().at_group_start {
            self.state_mut().ignorable = true;
            return Ok(());
        }
        self.state_mut().at_group_start = false;
        if self.state().fallback_remaining > 0
            && matches!(symbol, b'\\' | b'{' | b'}' | b'~' | b'-' | b'_')
        {
            self.state_mut().fallback_remaining -= 1;
            return Ok(());
        }
        match symbol {
            b'\\' | b'{' | b'}' => self.emit_ansi(&[symbol], start, end),
            b'~' => self.emit_text("\u{00a0}", start, end),
            b'-' => self.emit_text("\u{00ad}", start, end),
            b'_' => self.emit_text("\u{2011}", start, end),
            b'\'' => self.hex_escape(start),
            b'\n' | b'\r' => Ok(()),
            _ => {
                self.add_diagnostic(
                    "rtf.unknownControlSymbolIgnored",
                    DiagnosticSeverity::Info,
                    "unknown control symbol was ignored",
                    Some(locator(start, end)),
                )?;
                Ok(())
            }
        }
    }

    fn enter_destination(
        &mut self,
        destination: Destination,
        start: usize,
    ) -> Result<(), ConversionError> {
        if let Some(frame) = self.frames.last_mut() {
            frame.state.destination = destination;
            frame.introduced = Some(destination);
        }
        match destination {
            Destination::Pict => {
                if self.picture.is_some() {
                    return Err(malformed("nested pict destination is invalid"));
                }
                self.picture = Some(Picture { start, ..Picture::default() });
            }
            Destination::ListText
            | Destination::MetaTitle
            | Destination::MetaAuthor
            | Destination::FieldInstruction => {
                self.capture.clear();
            }
            Destination::FieldResult => {
                if self.field_inline_start.is_some() {
                    return Err(malformed("nested field result is invalid"));
                }
                self.active_link = self.pending_link.take();
                self.field_inline_start = Some(self.paragraph.inlines.len());
            }
            _ => {}
        }
        Ok(())
    }

    fn plain_text(&mut self) -> Result<(), ConversionError> {
        let start = self.offset;
        while self.offset < self.bytes.len()
            && !matches!(self.bytes[self.offset], b'{' | b'}' | b'\\' | b'\r' | b'\n')
        {
            self.offset += 1;
            if self.offset.is_multiple_of(CHECKPOINT_INTERVAL) {
                self.context.checkpoint()?;
            }
        }
        let bytes = &self.bytes[start..self.offset];
        if self.state().destination == Destination::Pict {
            return self.picture_hex(bytes, start);
        }
        self.emit_ansi(bytes, start, self.offset)
    }

    fn hex_escape(&mut self, start: usize) -> Result<(), ConversionError> {
        let mut cursor = self.offset;
        let mut count = 0_usize;
        loop {
            let value = self
                .bytes
                .get(cursor..cursor.saturating_add(2))
                .ok_or_else(|| malformed("truncated hexadecimal escape"))?;
            hex(value[0]).ok_or_else(|| malformed("invalid hexadecimal escape"))?;
            hex(value[1]).ok_or_else(|| malformed("invalid hexadecimal escape"))?;
            count = count
                .checked_add(1)
                .ok_or_else(|| limit("rtf_control_count", "hex escape run overflow"))?;
            cursor += 2;
            if self.bytes.get(cursor..cursor.saturating_add(2)) == Some(b"\\'") {
                cursor += 2;
            } else {
                break;
            }
        }
        let additional = u64::try_from(count.saturating_sub(1)).unwrap_or(u64::MAX);
        self.control_count = self
            .control_count
            .checked_add(additional)
            .ok_or_else(|| limit("rtf_control_count", "control count overflow"))?;
        if self.control_count > MAX_CONTROLS {
            return Err(limit(
                "rtf_control_count",
                format!("{} > {MAX_CONTROLS}", self.control_count),
            ));
        }
        let mut decoded = Vec::new();
        reserve_vec(&mut decoded, count, &mut self.memory)?;
        let mut source = self.offset;
        for index in 0..count {
            if index != 0 {
                source += 2;
            }
            let high = hex(self.bytes[source]).ok_or_else(|| malformed("invalid hex digit"))?;
            let low = hex(self.bytes[source + 1]).ok_or_else(|| malformed("invalid hex digit"))?;
            decoded.push((high << 4) | low);
            source += 2;
        }
        self.offset = cursor;
        self.emit_ansi(&decoded, start, self.offset)
    }

    fn emit_ansi(&mut self, bytes: &[u8], start: usize, end: usize) -> Result<(), ConversionError> {
        if !matches!(
            self.state().destination,
            Destination::Body
                | Destination::ListText
                | Destination::MetaTitle
                | Destination::MetaAuthor
                | Destination::FieldInstruction
                | Destination::FieldResult
        ) {
            return Ok(());
        }
        let skip = usize::from(self.state().fallback_remaining).min(bytes.len());
        self.state_mut().fallback_remaining =
            self.state().fallback_remaining.saturating_sub(u8::try_from(skip).unwrap_or(u8::MAX));
        let bytes = &bytes[skip..];
        if bytes.is_empty() {
            return Ok(());
        }
        self.flush_pending_surrogate()?;
        let codepage = self
            .font_charsets
            .get(&self.state().font)
            .copied()
            .unwrap_or(self.state().ansi_codepage);
        let encoding = encoding_for_codepage(codepage)?;
        if !bytes.is_ascii() {
            let decode_bound = u64::try_from(bytes.len())
                .unwrap_or(u64::MAX)
                .checked_mul(3)
                .ok_or_else(|| limit("max_memory_bytes", "RTF decode working set overflow"))?;
            self.memory.grow(decode_bound)?;
        }
        let (decoded, _, malformed_bytes) = encoding.decode(bytes);
        if malformed_bytes {
            self.add_diagnostic(
                "rtf.invalidByteSequenceReplaced",
                DiagnosticSeverity::Warning,
                "invalid codepage byte sequence was replaced",
                Some(locator(start + skip, end)),
            )?;
        }
        self.emit_text(&decoded, start + skip, end)
    }

    fn emit_unicode(&mut self, unit: u16, start: usize, end: usize) -> Result<(), ConversionError> {
        if let Some((high, high_start, _)) = self.pending_high_surrogate.take() {
            if (0xdc00..=0xdfff).contains(&unit) {
                let scalar =
                    0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
                let value = char::from_u32(scalar)
                    .ok_or_else(|| malformed("invalid Unicode surrogate pair"))?;
                let mut buffer = [0_u8; 4];
                return self.emit_text_inner(value.encode_utf8(&mut buffer), high_start, end);
            }
            self.emit_text_inner("\u{fffd}", high_start, start)?;
        }
        if (0xd800..=0xdbff).contains(&unit) {
            self.pending_high_surrogate = Some((unit, start, end));
            return Ok(());
        }
        let value = char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}');
        let mut buffer = [0_u8; 4];
        self.emit_text_inner(value.encode_utf8(&mut buffer), start, end)
    }

    fn emit_text(&mut self, value: &str, start: usize, end: usize) -> Result<(), ConversionError> {
        self.flush_pending_surrogate()?;
        self.emit_text_inner(value, start, end)
    }

    fn flush_pending_surrogate(&mut self) -> Result<(), ConversionError> {
        if let Some((_, start, end)) = self.pending_high_surrogate.take() {
            self.emit_text_inner("\u{fffd}", start, end)?;
        }
        Ok(())
    }

    fn emit_text_inner(
        &mut self,
        value: &str,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        if value.is_empty() {
            return Ok(());
        }
        match self.state().destination {
            Destination::ListText
            | Destination::MetaTitle
            | Destination::MetaAuthor
            | Destination::FieldInstruction => {
                if self.capture.len().saturating_add(value.len()) > MAX_METADATA_BYTES {
                    return Err(limit("rtf_capture_bytes", "destination capture exceeds 64 KiB"));
                }
                reserve_string(&mut self.capture, value.len(), &mut self.memory)?;
                self.capture.push_str(value);
                return Ok(());
            }
            Destination::Body | Destination::FieldResult => {}
            _ => return Ok(()),
        }
        self.decoded_bytes = self
            .decoded_bytes
            .checked_add(u64::try_from(value.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("rtf_decoded_bytes", "decoded text size overflow"))?;
        if self.decoded_bytes > self.options.limits.max_decompressed_bytes {
            return Err(limit(
                "rtf_decoded_bytes",
                format!("{} > {}", self.decoded_bytes, self.options.limits.max_decompressed_bytes),
            ));
        }
        let marks = self.current_marks()?;
        let starts_field = self.field_inline_start == Some(self.paragraph.inlines.len());
        if let Some(Inline::Text { value: current, marks: current_marks }) =
            self.paragraph.inlines.last_mut()
            && *current_marks == marks
            && !starts_field
        {
            reserve_string(current, value.len(), &mut self.memory)?;
            current.push_str(value);
        } else {
            if self.paragraph.inlines.len() >= MAX_DOCUMENT_INLINES {
                return Err(limit("document_inlines", format!("> {MAX_DOCUMENT_INLINES}")));
            }
            let mut text = String::new();
            reserve_string(&mut text, value.len(), &mut self.memory)?;
            text.push_str(value);
            reserve_vec(&mut self.paragraph.inlines, 1, &mut self.memory)?;
            self.paragraph.inlines.push(Inline::Text { value: text, marks });
        }
        self.paragraph.start.get_or_insert(start);
        self.paragraph.end = self.paragraph.end.max(end);
        Ok(())
    }

    fn emit_inline(
        &mut self,
        inline: Inline,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        if !matches!(self.state().destination, Destination::Body | Destination::FieldResult) {
            return Ok(());
        }
        reserve_vec(&mut self.paragraph.inlines, 1, &mut self.memory)?;
        self.paragraph.inlines.push(inline);
        self.paragraph.start.get_or_insert(start);
        self.paragraph.end = self.paragraph.end.max(end);
        Ok(())
    }

    fn current_marks(&mut self) -> Result<Vec<InlineMark>, ConversionError> {
        let state = self.state();
        let selected = [
            (state.bold, InlineMark::Bold),
            (state.italic, InlineMark::Italic),
            (state.strike, InlineMark::Strikethrough),
            (state.underline, InlineMark::Underline),
            (state.superscript, InlineMark::Superscript),
            (state.subscript, InlineMark::Subscript),
        ];
        let count = selected.iter().filter(|(enabled, _)| *enabled).count();
        let mut marks = Vec::new();
        reserve_vec(&mut marks, count, &mut self.memory)?;
        for (enabled, mark) in selected {
            if enabled {
                marks.push(mark);
            }
        }
        Ok(marks)
    }

    fn finish_paragraph(&mut self, end: usize) -> Result<(), ConversionError> {
        if self.paragraph.inlines.is_empty() {
            return Ok(());
        }
        let start = self.paragraph.start.unwrap_or(end);
        let inlines = std::mem::take(&mut self.paragraph.inlines);
        let block = self.node(Block::Paragraph(inlines), start, self.paragraph.end.max(end))?;
        self.paragraph = Paragraph::default();
        if self.table.active || self.state().in_table {
            reserve_vec(&mut self.table.cell_blocks, 1, &mut self.memory)?;
            self.table.cell_blocks.push(block);
        } else if self.state().list_id.is_some() || self.pending_list_marker.is_some() {
            let marker = self.pending_list_marker.take();
            let kind = if marker
                .as_deref()
                .is_some_and(|value| value.contains('•') || value.contains('·'))
            {
                ListKind::Bullet
            } else {
                ListKind::Ordered
            };
            let mut item_blocks = Vec::new();
            reserve_vec(&mut item_blocks, 1, &mut self.memory)?;
            item_blocks.push(block);
            let item = ListItem { checked: None, marker_label: marker, blocks: item_blocks };
            let mut items = Vec::new();
            reserve_vec(&mut items, 1, &mut self.memory)?;
            items.push(item);
            let list = self.node(Block::List { kind, start: 1, items }, start, end)?;
            self.push_block(list)?;
        } else {
            self.push_block(block)?;
        }
        Ok(())
    }

    fn finish_field_result(&mut self) -> Result<(), ConversionError> {
        let Some(start) = self.field_inline_start.take() else {
            return Ok(());
        };
        let Some(target) = self.active_link.take() else {
            return Ok(());
        };
        if start > self.paragraph.inlines.len() {
            return Err(ConversionError::Internal {
                detail: "RTF field inline boundary exceeds paragraph".into(),
            });
        }
        let count = self.paragraph.inlines.len() - start;
        if count == 0 {
            return Ok(());
        }
        let mut content = Vec::new();
        reserve_vec(&mut content, count, &mut self.memory)?;
        content.extend(self.paragraph.inlines.drain(start..));
        reserve_vec(&mut self.paragraph.inlines, 1, &mut self.memory)?;
        self.paragraph.inlines.push(Inline::Link { target, content });
        Ok(())
    }

    fn start_table_row(&mut self, end: usize) -> Result<(), ConversionError> {
        if !self.table.cells.is_empty() {
            self.finish_row(end)?;
        }
        self.table.active = true;
        self.table.cell_definitions.clear();
        self.table.cell_definition_index = 0;
        self.table.pending_cell_merge = CellMerge::Normal;
        self.state_mut().in_table = true;
        Ok(())
    }

    fn finish_cell(&mut self, end: usize) -> Result<(), ConversionError> {
        self.finish_paragraph(end)?;
        if self.table.cell_blocks.is_empty() {
            let empty = self.node(Block::Paragraph(Vec::new()), end, end)?;
            reserve_vec(&mut self.table.cell_blocks, 1, &mut self.memory)?;
            self.table.cell_blocks.push(empty);
        }
        self.table_cells = self
            .table_cells
            .checked_add(1)
            .ok_or_else(|| limit("max_table_cells", "RTF table cell count overflow"))?;
        if self.table_cells > self.options.limits.max_table_cells {
            return Err(limit(
                "max_table_cells",
                format!("{} > {}", self.table_cells, self.options.limits.max_table_cells),
            ));
        }
        let merge = self
            .table
            .cell_definitions
            .get(self.table.cell_definition_index)
            .copied()
            .unwrap_or(CellMerge::Normal);
        self.table.cell_definition_index = self.table.cell_definition_index.saturating_add(1);
        let blocks = std::mem::take(&mut self.table.cell_blocks);
        if merge == CellMerge::Continue {
            let previous = self
                .table
                .cells
                .last_mut()
                .filter(|cell| cell.column_span >= 1)
                .ok_or_else(|| malformed("horizontal merge continuation has no origin cell"))?;
            previous.column_span = previous
                .column_span
                .checked_add(1)
                .ok_or_else(|| limit("max_table_columns", "RTF cell span overflow"))?;
            if blocks
                .iter()
                .any(|node| !matches!(&node.block, Block::Paragraph(value) if value.is_empty()))
            {
                return Err(malformed(
                    "horizontal merge continuation contains displayable content",
                ));
            }
        } else {
            reserve_vec(&mut self.table.cells, 1, &mut self.memory)?;
            self.table.cells.push(Cell { row_span: 1, column_span: 1, header: false, blocks });
        }
        Ok(())
    }

    fn finish_row(&mut self, end: usize) -> Result<(), ConversionError> {
        if !self.paragraph.inlines.is_empty() || !self.table.cell_blocks.is_empty() {
            self.finish_cell(end)?;
        }
        if self.table.cells.is_empty() {
            return Err(malformed("RTF table row contains no cells"));
        }
        if self.table.cells.len() > MAX_TABLE_COLUMNS {
            return Err(limit(
                "max_table_columns",
                format!("{} > {MAX_TABLE_COLUMNS}", self.table.cells.len()),
            ));
        }
        if u64::try_from(self.table.rows.len()).unwrap_or(u64::MAX)
            >= self.options.limits.max_table_rows
        {
            return Err(limit(
                "max_table_rows",
                format!(">= {}", self.options.limits.max_table_rows),
            ));
        }
        reserve_vec(&mut self.table.rows, 1, &mut self.memory)?;
        self.table.rows.push(TableRow { cells: std::mem::take(&mut self.table.cells) });
        self.table.cell_definitions.clear();
        self.table.cell_definition_index = 0;
        Ok(())
    }

    fn finish_table(&mut self, end: usize) -> Result<(), ConversionError> {
        if !self.paragraph.inlines.is_empty()
            || !self.table.cell_blocks.is_empty()
            || !self.table.cells.is_empty()
        {
            self.finish_row(end)?;
        }
        if !self.table.rows.is_empty() {
            let start = self
                .table
                .rows
                .first()
                .and_then(|row| row.cells.first())
                .and_then(|cell| cell.blocks.first())
                .and_then(|node| node.provenance.locator.byte_start)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(end);
            let rows = std::mem::take(&mut self.table.rows);
            let table = self.node(Block::Table { rows, alignments: Vec::new() }, start, end)?;
            self.push_block(table)?;
        }
        self.table.active = false;
        Ok(())
    }

    fn finish_table_or_paragraph(&mut self, end: usize) -> Result<(), ConversionError> {
        if self.table.active { self.finish_table(end) } else { self.finish_paragraph(end) }
    }

    fn picture_hex(&mut self, bytes: &[u8], start: usize) -> Result<(), ConversionError> {
        let Some(mut picture) = self.picture.take() else {
            return Err(ConversionError::Internal {
                detail: "pict destination lacks state".into(),
            });
        };
        for (index, byte) in bytes.iter().copied().enumerate() {
            if index.is_multiple_of(CHECKPOINT_INTERVAL) {
                self.context.checkpoint()?;
            }
            if byte.is_ascii_whitespace() {
                continue;
            }
            let nibble = hex(byte).ok_or_else(|| {
                malformed(format!("invalid pict hexadecimal byte at {}", start + index))
            })?;
            if let Some(high) = picture.high_nibble.take() {
                let next = picture
                    .bytes
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| limit("max_asset_bytes", "picture byte count overflow"))?;
                if u64::try_from(next).unwrap_or(u64::MAX) > self.options.limits.max_asset_bytes {
                    return Err(limit(
                        "max_asset_bytes",
                        format!("{next} > {}", self.options.limits.max_asset_bytes),
                    ));
                }
                reserve_vec(&mut picture.bytes, 1, &mut self.memory)?;
                picture.bytes.push((high << 4) | nibble);
            } else {
                picture.high_nibble = Some(nibble);
            }
        }
        picture.saw_odd_nibble = picture.high_nibble.is_some();
        self.picture = Some(picture);
        Ok(())
    }

    fn picture_binary(&mut self, count: usize) -> Result<(), ConversionError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| limit("max_asset_bytes", "bin range overflow"))?;
        let bytes =
            self.bytes.get(self.offset..end).ok_or_else(|| malformed("truncated pict bin data"))?;
        let Some(mut picture) = self.picture.take() else {
            return Err(ConversionError::Internal { detail: "pict bin lacks state".into() });
        };
        if picture.high_nibble.is_some() {
            return Err(malformed("pict bin data follows an incomplete hexadecimal byte"));
        }
        let next = picture
            .bytes
            .len()
            .checked_add(count)
            .ok_or_else(|| limit("max_asset_bytes", "picture byte count overflow"))?;
        if u64::try_from(next).unwrap_or(u64::MAX) > self.options.limits.max_asset_bytes {
            return Err(limit(
                "max_asset_bytes",
                format!("{next} > {}", self.options.limits.max_asset_bytes),
            ));
        }
        reserve_vec(&mut picture.bytes, count, &mut self.memory)?;
        picture.bytes.extend_from_slice(bytes);
        self.picture = Some(picture);
        self.offset = end;
        Ok(())
    }

    fn finish_picture(&mut self, end: usize) -> Result<(), ConversionError> {
        let Some(picture) = self.picture.take() else {
            return Err(ConversionError::Internal { detail: "pict close lacks state".into() });
        };
        if picture.saw_odd_nibble {
            return Err(malformed("pict hexadecimal data has an odd number of nibbles"));
        }
        let Some(media_type) = picture.media_type else {
            return self.add_diagnostic(
                "rtf.unsupportedVectorImage",
                DiagnosticSeverity::Warning,
                "EMF/WMF or untyped pict content was not retained",
                Some(locator(picture.start, end)),
            );
        };
        audit_image(&picture.bytes, media_type, self.options, self.context, &mut self.memory)?;
        let size = u64::try_from(picture.bytes.len()).unwrap_or(u64::MAX);
        self.total_asset_bytes = self
            .total_asset_bytes
            .checked_add(size)
            .ok_or_else(|| limit("max_total_asset_bytes", "asset total overflow"))?;
        if self.total_asset_bytes > self.options.limits.max_total_asset_bytes {
            return Err(limit(
                "max_total_asset_bytes",
                format!(
                    "{} > {}",
                    self.total_asset_bytes, self.options.limits.max_total_asset_bytes
                ),
            ));
        }
        // Prepay bounded IDs, filename, and MIME before their formatting allocates.
        self.memory.grow(256)?;
        let id = format!("rtf-image-{}", self.assets.len() + 1);
        let filename = format!("{id}.{}", if media_type == "image/png" { "png" } else { "jpg" });
        reserve_vec(&mut self.assets, 1, &mut self.memory)?;
        self.assets.push(Asset {
            id: AssetId(id.clone()),
            filename: Some(filename),
            media_type: media_type.into(),
            bytes: picture.bytes,
            external_uri: None,
        });
        let node = self.node(Block::Image { asset: AssetId(id), alt: None }, picture.start, end)?;
        self.push_block(node)
    }

    fn node(
        &mut self,
        block: Block,
        start: usize,
        end: usize,
    ) -> Result<BlockNode, ConversionError> {
        self.node_sequence = self
            .node_sequence
            .checked_add(1)
            .ok_or_else(|| limit("document_nodes", "node ID overflow"))?;
        // Node IDs are a fixed prefix plus u64 decimal; provider is fixed. Prepay their maximum.
        self.memory.grow(96)?;
        let id = format!("rtf-{}", self.node_sequence);
        Ok(BlockNode {
            id: NodeId(id),
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: locator(start, end),
                confidence: Some(1.0),
            },
        })
    }

    fn push_block(&mut self, block: BlockNode) -> Result<(), ConversionError> {
        if self.blocks.len() >= MAX_DOCUMENT_NODES {
            return Err(limit("document_nodes", format!(">= {MAX_DOCUMENT_NODES}")));
        }
        reserve_vec(&mut self.blocks, 1, &mut self.memory)?;
        self.blocks.push(block);
        Ok(())
    }

    fn add_diagnostic(
        &mut self,
        code: &str,
        severity: DiagnosticSeverity,
        message: &str,
        locator: Option<SourceLocator>,
    ) -> Result<(), ConversionError> {
        if self.diagnostics.len() >= MAX_DIAGNOSTICS {
            return Err(limit("rtf_diagnostics", format!(">= {MAX_DIAGNOSTICS}")));
        }
        self.memory
            .grow(u64::try_from(code.len().saturating_add(message.len())).unwrap_or(u64::MAX))?;
        reserve_vec(&mut self.diagnostics, 1, &mut self.memory)?;
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity,
            message: message.into(),
            locator,
        });
        Ok(())
    }
}

fn destination(name: &str, ignorable: bool) -> Option<Destination> {
    let value = match name {
        "fonttbl" => Destination::FontTable,
        "pict" => Destination::Pict,
        "listtext" | "pntext" => Destination::ListText,
        "title" => Destination::MetaTitle,
        "author" => Destination::MetaAuthor,
        "fldinst" => Destination::FieldInstruction,
        "fldrslt" => Destination::FieldResult,
        "colortbl" | "stylesheet" | "info" | "listtable" | "listoverridetable" | "generator"
        | "object" | "objdata" | "filetbl" | "datastore" | "themedata" | "colorschememapping"
        | "htmltag" | "xmlopen" | "xmlattrname" | "xmlattrvalue" | "xmlclose" | "nonshppict"
        | "shppict" | "header" | "headerl" | "headerr" | "headerf" | "footer" | "footerl"
        | "footerr" | "footerf" | "annotation" | "footnote" | "aftncn" | "aftnsep" | "aftnsepc"
        | "private" | "passwordhash" => Destination::Skip,
        _ if ignorable => Destination::Skip,
        _ => return None,
    };
    Some(value)
}

fn is_known_non_destination_control(name: &str) -> bool {
    matches!(
        name,
        "deflang"
            | "deflangfe"
            | "adeflang"
            | "fet"
            | "paperw"
            | "paperh"
            | "margl"
            | "margr"
            | "margt"
            | "margb"
            | "sectd"
            | "sect"
            | "page"
            | "qc"
            | "ql"
            | "qr"
            | "qj"
            | "sbasedon"
            | "snext"
            | "s"
            | "cs"
            | "additive"
            | "deleted"
            | "revised"
            | "revauth"
            | "revdttm"
            | "charrsid"
            | "pararsid"
            | "sectrsid"
    )
}

fn safe_hyperlink(instruction: &str) -> Option<String> {
    let value = instruction.trim();
    let rest = value.strip_prefix("HYPERLINK")?.trim_start();
    let target = if let Some(rest) = rest.strip_prefix('"') {
        rest.split_once('"')?.0
    } else {
        rest.split_ascii_whitespace().next()?
    };
    canonical_external_asset_uri(target).filter(|canonical| canonical == target)
}

fn audit_image(
    bytes: &[u8],
    media_type: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    const MAX_DIMENSION: u32 = 32_768;
    context.checkpoint()?;
    let compressed = u64::try_from(bytes.len())
        .map_err(|_| limit("max_asset_bytes", "picture size cannot be represented"))?;
    let (dimensions, decoded_bytes) = if media_type == "image/png" {
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(malformed("pict bytes do not match the declared PNG signature"));
        }
        let mut decoder = PngDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed("PNG pict header is invalid"))?;
        let dimensions = decoder.dimensions();
        set_image_limits(
            &mut decoder,
            dimensions,
            image_working_bound(dimensions, compressed)?,
            options,
        )?;
        (dimensions, decoder.total_bytes())
    } else {
        if !bytes.starts_with(&[0xff, 0xd8, 0xff]) || !bytes.ends_with(&[0xff, 0xd9]) {
            return Err(malformed("pict bytes do not match the declared JPEG signature"));
        }
        let mut decoder = JpegDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed("JPEG pict header is invalid"))?;
        let dimensions = decoder.dimensions();
        set_image_limits(
            &mut decoder,
            dimensions,
            image_working_bound(dimensions, compressed)?,
            options,
        )?;
        (dimensions, decoder.total_bytes())
    };
    if dimensions.0 == 0
        || dimensions.1 == 0
        || dimensions.0 > MAX_DIMENSION
        || dimensions.1 > MAX_DIMENSION
    {
        return Err(limit(
            "image_dimensions",
            format!("{}x{} exceeds the audited image bounds", dimensions.0, dimensions.1),
        ));
    }
    if decoded_bytes > options.limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            format!("decoded picture {decoded_bytes} > {}", options.limits.max_decompressed_bytes),
        ));
    }
    // Decoder internals are bounded through ImageLimits. Reserve the decoded output plus a
    // conservative compressed-input copy and 256 KiB codec state before decoding.
    let working = decoded_bytes
        .checked_add(compressed)
        .and_then(|value| value.checked_add(256 * 1024))
        .ok_or_else(|| limit("max_memory_bytes", "picture audit working set overflow"))?;
    memory.grow(working)?;
    let length = usize::try_from(decoded_bytes)
        .map_err(|_| limit("max_decompressed_bytes", "decoded picture is too large"))?;
    let mut pixels = Vec::new();
    reserve_vec(&mut pixels, length, memory)?;
    pixels.resize(length, 0);
    if media_type == "image/png" {
        let mut decoder = PngDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed("PNG pict header is invalid"))?;
        set_image_limits(&mut decoder, dimensions, working, options)?;
        decoder.read_image(&mut pixels).map_err(|_| malformed("PNG pict stream is invalid"))?;
    } else {
        let mut decoder = JpegDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed("JPEG pict header is invalid"))?;
        set_image_limits(&mut decoder, dimensions, working, options)?;
        decoder.read_image(&mut pixels).map_err(|_| malformed("JPEG pict stream is invalid"))?;
    }
    context.checkpoint()
}

fn image_working_bound(dimensions: (u32, u32), compressed: u64) -> Result<u64, ConversionError> {
    u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|pixels| pixels.checked_add(compressed))
        .and_then(|value| value.checked_add(256 * 1024))
        .ok_or_else(|| limit("image_decode_memory", "picture working set overflow"))
}

fn set_image_limits<D: image::ImageDecoder>(
    decoder: &mut D,
    dimensions: (u32, u32),
    max_alloc: u64,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let mut limits = ImageLimits::default();
    limits.max_image_width = Some(dimensions.0.min(32_768));
    limits.max_image_height = Some(dimensions.1.min(32_768));
    limits.max_alloc = Some(max_alloc.min(options.limits.max_memory_bytes));
    decoder
        .set_limits(limits)
        .map_err(|_| limit("image_decode_memory", "image decoder rejected resource limits"))
}

fn encoding_for_codepage(codepage: u16) -> Result<&'static Encoding, ConversionError> {
    match codepage {
        1252 => Ok(WINDOWS_1252),
        936 => Ok(GBK),
        950 => Ok(BIG5),
        932 => Ok(SHIFT_JIS),
        value => Err(malformed(format!("unsupported RTF ANSI codepage {value}"))),
    }
}

fn font_charset_codepage(charset: u16) -> Option<u16> {
    match charset {
        0 | 1 | 2 | 77 | 255 => Some(1252),
        128 => Some(932),
        134 => Some(936),
        136 => Some(950),
        _ => None,
    }
}

fn parameter_i32(parameter: Option<i64>, name: &str) -> Result<i32, ConversionError> {
    i32::try_from(parameter.ok_or_else(|| malformed(format!("{name} requires a parameter")))?)
        .map_err(|_| limit("rtf_numeric_value", format!("{name} is outside signed 32-bit range")))
}

fn parameter_u16(parameter: Option<i64>, name: &str) -> Result<u16, ConversionError> {
    u16::try_from(parameter.ok_or_else(|| malformed(format!("{name} requires a parameter")))?)
        .map_err(|_| limit("rtf_numeric_value", format!("{name} is outside unsigned 16-bit range")))
}

fn reserve_vec<T>(
    value: &mut Vec<T>,
    additional: usize,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    if additional <= value.capacity().saturating_sub(value.len()) {
        return Ok(());
    }
    let old = value.capacity();
    let requested = additional.saturating_sub(value.capacity().saturating_sub(value.len()));
    let bytes = u64::try_from(
        requested
            .checked_mul(size_of::<T>())
            .ok_or_else(|| limit("max_memory_bytes", "vector capacity overflow"))?,
    )
    .map_err(|_| limit("max_memory_bytes", "vector capacity cannot be represented"))?;
    memory.grow(bytes)?;
    value
        .try_reserve_exact(additional)
        .map_err(|error| limit("max_memory_bytes", format!("vector allocation failed: {error}")))?;
    let actual = value.capacity().saturating_sub(old);
    if actual > requested {
        memory.grow(
            u64::try_from((actual - requested).saturating_mul(size_of::<T>())).unwrap_or(u64::MAX),
        )?;
    }
    Ok(())
}

fn reserve_string(
    value: &mut String,
    additional: usize,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    if additional <= value.capacity().saturating_sub(value.len()) {
        return Ok(());
    }
    let old = value.capacity();
    let requested = additional.saturating_sub(value.capacity().saturating_sub(value.len()));
    memory.grow(u64::try_from(requested).unwrap_or(u64::MAX))?;
    value
        .try_reserve_exact(additional)
        .map_err(|error| limit("max_memory_bytes", format!("string allocation failed: {error}")))?;
    let actual = value.capacity().saturating_sub(old);
    if actual > requested {
        memory.grow(u64::try_from(actual - requested).unwrap_or(u64::MAX))?;
    }
    Ok(())
}

fn reserve_map_entry<K, V>(
    _: &BTreeMap<K, V>,
    memory: &mut ResourceReservation,
) -> Result<(), ConversionError> {
    memory.grow(u64::try_from(size_of::<(K, V)>() + 3 * size_of::<usize>()).unwrap_or(u64::MAX))
}

fn locator(start: usize, end: usize) -> SourceLocator {
    SourceLocator {
        byte_start: u64::try_from(start).ok(),
        byte_end: u64::try_from(end).ok(),
        ..SourceLocator::default()
    }
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some("rtf".into()), detail: detail.into() }
}

fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{
        ConversionOptions, ErrorCode, ExecutionOptions, FormatCandidate, SourceMetadata,
    };
    use std::sync::Arc;

    fn convert(bytes: &[u8]) -> Result<ConverterOutput, ConversionError> {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        Parser::new(bytes, &options, &context)?.parse()
    }

    fn paragraph_text(output: &ConverterOutput) -> String {
        output
            .document
            .blocks
            .iter()
            .filter_map(|node| match &node.block {
                Block::Paragraph(inlines) => Some(inlines),
                _ => None,
            })
            .flatten()
            .filter_map(|inline| match inline {
                Inline::Text { value, .. } => Some(value.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn strict_probe_rejects_plain_text_prefixes() {
        assert!(strict_header(b"{\\rtf1\\ansi ok}").is_some());
        assert!(strict_header(b"{\\rtf\\ansi no version}").is_none());
        assert!(strict_header(b"prefix {\\rtf1 no}").is_none());
        assert!(strict_header(b"{\\rtf1x no delimiter}").is_none());
    }

    #[test]
    fn styles_unicode_hex_and_spans_are_preserved() {
        let output = convert(b"{\\rtf1\\ansi A {\\b bold} \\u20013? \\'e9\\par}").unwrap();
        assert_eq!(paragraph_text(&output), "A bold 中 é");
        let node = &output.document.blocks[0];
        assert_eq!(node.provenance.locator.byte_start, Some(12));
        assert!(
            node.provenance.locator.byte_end.unwrap() > node.provenance.locator.byte_start.unwrap()
        );
        let Block::Paragraph(inlines) = &node.block else { panic!("paragraph") };
        assert!(inlines.iter().any(|inline| matches!(inline, Inline::Text { marks, .. } if marks.contains(&InlineMark::Bold))));
    }

    #[test]
    fn codepages_font_charset_surrogates_and_unicode_fallback_are_deterministic() {
        let chinese = convert(
            b"{\\rtf1\\ansi\\ansicpg1252{\\fonttbl{\\f0\\fcharset134 SimSun;}}\\f0 \\'d6\\'d0\\'ce\\'c4 \\uc2\\u-10179a\\~\\u-8704cd\\par}",
        )
        .unwrap();
        assert_eq!(paragraph_text(&chinese), "中文 😀");
    }

    #[test]
    fn active_destinations_are_skipped() {
        let output =
            convert(b"{\\rtf1\\ansi before{\\object{\\*\\objdata 0102}{\\result BAD}}after\\par}")
                .unwrap();
        assert_eq!(paragraph_text(&output), "beforeafter");
        assert!(output.assets.is_empty());
    }

    #[test]
    fn malformed_and_limits_are_stable() {
        assert_eq!(convert(b"{\\rtf1\\ansi no close").unwrap_err().code(), ErrorCode::Malformed);
        assert_eq!(
            convert(b"{\\rtf1\\u99999999999?}").unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
        let mut options = ConversionOptions::default();
        options.limits.max_nesting_depth = 2;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        assert_eq!(
            Parser::new(b"{\\rtf1{{x}}}", &options, &context).unwrap().parse().unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn table_list_and_safe_field_map_to_structured_ir() {
        let table =
            convert(b"{\\rtf1\\ansi\\trowd\\intbl A\\cell B\\cell\\row\\pard after\\par}").unwrap();
        assert!(matches!(table.document.blocks[0].block, Block::Table { .. }));
        assert!(matches!(table.document.blocks[1].block, Block::Paragraph(_)));

        let span = convert(
            b"{\\rtf1\\ansi\\trowd\\clmgf\\cellx1000\\clmrg\\cellx2000\\intbl merged\\cell\\cell\\row}",
        )
        .unwrap();
        let Block::Table { rows, .. } = &span.document.blocks[0].block else { panic!("table") };
        assert_eq!(rows[0].cells.len(), 1);
        assert_eq!(rows[0].cells[0].column_span, 2);

        let list = convert(b"{\\rtf1\\ansi{\\listtext\\bullet\\tab}Item\\par}").unwrap();
        assert!(matches!(
            list.document.blocks[0].block,
            Block::List { kind: ListKind::Bullet, .. }
        ));

        let link = convert(b"{\\rtf1\\ansi before{\\field{\\*\\fldinst HYPERLINK \\\"https://example.invalid/path\\\"}{\\fldrslt safe}}after\\par}").unwrap();
        let Block::Paragraph(inlines) = &link.document.blocks[0].block else {
            panic!("field result paragraph")
        };
        assert!(matches!(
            &inlines[1],
            Inline::Link { target, .. } if target == "https://example.invalid/path"
        ));

        let secret = convert(b"{\\rtf1\\ansi{\\field{\\*\\fldinst HYPERLINK \\\"https://user:secret@example.invalid/path?token=x\\\"}{\\fldrslt label}}}").unwrap();
        assert!(
            secret
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "rtf.unsafeHyperlinkSkipped")
        );
        let Block::Paragraph(inlines) = &secret.document.blocks[0].block else {
            panic!("unsafe link fallback paragraph")
        };
        assert!(matches!(&inlines[0], Inline::Text { value, .. } if value == "label"));
    }

    #[test]
    fn png_picture_is_decoded_and_retained_but_vector_is_not() {
        let png = "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360606060000000050001a5f645400000000049454e44ae426082";
        let source = format!("{{\\rtf1\\ansi{{\\pict\\pngblip {png}}}}}");
        let output = convert(source.as_bytes()).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].media_type, "image/png");
        assert!(matches!(output.document.blocks[0].block, Block::Image { .. }));

        let vector = convert(b"{\\rtf1\\ansi{\\pict\\emfblip 0102}}").unwrap();
        assert!(vector.assets.is_empty());
        assert!(
            vector
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "rtf.unsupportedVectorImage")
        );
    }

    #[test]
    fn cancellation_and_trailing_payload_are_controlled_errors() {
        let mut options = ConversionOptions::default();
        let cancellation = into_markdown_core::CancellationToken::new();
        cancellation.cancel();
        let context = ExecutionContext::new(
            ExecutionOptions { cancellation, ..ExecutionOptions::default() },
            options.limits.clone(),
        );
        let error = Parser::new(b"{\\rtf1 text}", &options, &context).err().unwrap();
        assert_eq!(error.code(), ErrorCode::Cancelled);
        options.limits.max_input_bytes = 1024;
        assert_eq!(convert(b"{\\rtf1 text}payload").unwrap_err().code(), ErrorCode::Malformed);
    }

    #[test]
    fn probe_is_not_extension_only() {
        let converter = RtfConverter;
        let input = ResolvedInput {
            metadata: SourceMetadata::default(),
            bytes: Arc::from(&b"not rtf"[..]),
        };
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits);
        let result = futures_lite_for_test(converter.probe(
            &input,
            &FormatCandidate::new(InputFormat::Rtf, 0.55, "extension"),
            &context,
        ));
        assert_eq!(result.unwrap(), ProbeOutcome::NotApplicable);
    }

    fn futures_lite_for_test<T>(mut future: BoxFuture<'_, T>) -> T {
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test future unexpectedly pending"),
        }
    }
}
