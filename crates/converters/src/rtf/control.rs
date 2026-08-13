//! Control-word and control-symbol dispatch.

use super::budget::{limit, locator, malformed, parameter_i32, parameter_u16, reserve_vec};
use super::destinations::{destination, is_known_non_destination_control};
use super::parser::{
    CellMerge, Destination, FontCharset, MAX_CONTROLS, MAX_NUMERIC_DIGITS, MAX_RTF_FONTS, Parser,
};
use super::text::{encoding_for_codepage, font_charset_codepage};
use into_markdown_core::{ConversionError, DiagnosticSeverity, Inline};

impl Parser<'_> {
    pub(super) fn control(&mut self) -> Result<(), ConversionError> {
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
    pub(super) fn control_word(
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
                        if self.font_charsets.len() >= MAX_RTF_FONTS {
                            return Err(limit("rtf_font_count", format!(">= {MAX_RTF_FONTS}")));
                        }
                        let order = u32::try_from(self.font_charsets.len()).map_err(|_| {
                            limit("rtf_font_count", "font definition order overflow")
                        })?;
                        reserve_vec(&mut self.font_charsets, 1, &mut self.memory)?;
                        self.font_charsets.push(FontCharset { font, codepage, order });
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

    pub(super) fn control_symbol(
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
}
