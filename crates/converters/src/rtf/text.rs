//! Codepage, Unicode, fallback, and inline text decoding.

use super::budget::{hex, limit, locator, malformed, reserve_string, reserve_vec};
use super::parser::{CHECKPOINT_INTERVAL, Destination, MAX_METADATA_BYTES, Parser};
use encoding_rs::{BIG5, Encoding, GBK, SHIFT_JIS, WINDOWS_1252};
use into_markdown_core::{
    ConversionError, DiagnosticSeverity, Inline, InlineMark, MAX_DOCUMENT_INLINES,
};

impl Parser<'_> {
    pub(super) fn plain_text(&mut self) -> Result<(), ConversionError> {
        let start = self.offset;
        let codepage = self.current_codepage();
        while self.offset < self.bytes.len() {
            let byte = self.bytes[self.offset];
            if self.state().destination != Destination::Pict
                && is_dbcs_lead(codepage, byte)
                && self
                    .bytes
                    .get(self.offset + 1)
                    .is_some_and(|trail| is_dbcs_trail(codepage, *trail))
            {
                self.offset += 2;
            } else if matches!(byte, b'{' | b'}' | b'\\' | b'\r' | b'\n') {
                break;
            } else {
                self.offset += 1;
            }
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

    pub(super) fn hex_escape(&mut self, start: usize) -> Result<(), ConversionError> {
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
                self.charge_control()?;
                cursor += 2;
            } else {
                break;
            }
        }
        let mut decoded = Vec::new();
        reserve_vec(&mut decoded, count, &mut self.memory)?;
        let mut source = self.offset;
        for index in 0..count {
            if index != 0 && index.is_multiple_of(1024) {
                self.context.checkpoint()?;
            }
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

    pub(super) fn emit_ansi(
        &mut self,
        bytes: &[u8],
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
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
        let codepage = self.current_codepage();
        let (skip, skipped_units) =
            skip_ansi_units(bytes, codepage, self.state().fallback_remaining);
        self.state_mut().fallback_remaining =
            self.state().fallback_remaining.saturating_sub(skipped_units);
        let bytes = &bytes[skip..];
        if bytes.is_empty() {
            return Ok(());
        }
        self.flush_pending_surrogate()?;
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

    pub(super) fn emit_unicode(
        &mut self,
        unit: u16,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
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

    pub(super) fn emit_text(
        &mut self,
        value: &str,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        self.flush_pending_surrogate()?;
        self.emit_text_inner(value, start, end)
    }

    pub(super) fn flush_pending_surrogate(&mut self) -> Result<(), ConversionError> {
        if let Some((_, start, end)) = self.pending_high_surrogate.take() {
            self.emit_text_inner("\u{fffd}", start, end)?;
        }
        Ok(())
    }

    pub(super) fn emit_text_inner(
        &mut self,
        value: &str,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        if value.is_empty() {
            return Ok(());
        }
        if value.chars().any(is_forbidden_text_control) {
            return Err(malformed(
                "RTF text contains a forbidden C0, C1, or DEL control character",
            ));
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
        let starts_field = self
            .field
            .as_ref()
            .is_some_and(|field| field.inline_start == Some(self.paragraph.inlines.len()));
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

    pub(super) fn emit_inline(
        &mut self,
        inline: Inline,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        if !matches!(self.state().destination, Destination::Body | Destination::FieldResult) {
            return Ok(());
        }
        if self.paragraph.inlines.len() >= MAX_DOCUMENT_INLINES {
            return Err(limit("document_inlines", format!(">= {MAX_DOCUMENT_INLINES}")));
        }
        reserve_vec(&mut self.paragraph.inlines, 1, &mut self.memory)?;
        self.paragraph.inlines.push(inline);
        self.paragraph.start.get_or_insert(start);
        self.paragraph.end = self.paragraph.end.max(end);
        Ok(())
    }

    pub(super) fn current_marks(&mut self) -> Result<Vec<InlineMark>, ConversionError> {
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

    pub(super) fn current_codepage(&self) -> u16 {
        self.font_charsets
            .binary_search_by_key(&self.state().font, |entry| entry.font)
            .ok()
            .and_then(|index| self.font_charsets.get(index))
            .map_or(self.state().ansi_codepage, |entry| entry.codepage)
    }
}

fn is_forbidden_text_control(value: char) -> bool {
    let value = u32::from(value);
    value <= 0x1f && value != u32::from(b'\t') || (0x7f..=0x9f).contains(&value)
}

fn is_dbcs_lead(codepage: u16, byte: u8) -> bool {
    match codepage {
        932 => matches!(byte, 0x81..=0x9f | 0xe0..=0xfc),
        936 | 950 => matches!(byte, 0x81..=0xfe),
        _ => false,
    }
}

fn is_dbcs_trail(codepage: u16, byte: u8) -> bool {
    match codepage {
        932 => matches!(byte, 0x40..=0x7e | 0x80..=0xfc),
        936 => matches!(byte, 0x40..=0xfe) && byte != 0x7f,
        950 => matches!(byte, 0x40..=0x7e | 0xa1..=0xfe),
        _ => false,
    }
}

fn skip_ansi_units(bytes: &[u8], codepage: u16, maximum: u8) -> (usize, u8) {
    let mut offset = 0;
    let mut units = 0;
    while units < maximum && offset < bytes.len() {
        let width = if is_dbcs_lead(codepage, bytes[offset])
            && bytes.get(offset + 1).is_some_and(|trail| is_dbcs_trail(codepage, *trail))
        {
            2
        } else {
            1
        };
        offset += width;
        units += 1;
    }
    (offset, units)
}

pub(super) fn encoding_for_codepage(codepage: u16) -> Result<&'static Encoding, ConversionError> {
    match codepage {
        1252 => Ok(WINDOWS_1252),
        936 => Ok(GBK),
        950 => Ok(BIG5),
        932 => Ok(SHIFT_JIS),
        value => Err(malformed(format!("unsupported RTF ANSI codepage {value}"))),
    }
}

pub(super) fn font_charset_codepage(charset: u16) -> Option<u16> {
    match charset {
        0 | 1 | 2 | 77 | 255 => Some(1252),
        128 => Some(932),
        134 => Some(936),
        136 => Some(950),
        _ => None,
    }
}
