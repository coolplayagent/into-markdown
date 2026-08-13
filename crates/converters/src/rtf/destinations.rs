//! Group destinations and inherited state transitions.

use super::budget::{limit, locator, malformed, reserve_string, reserve_vec};
use super::parser::{Destination, Frame, Parser, Picture, State};
use into_markdown_core::{ConversionError, DiagnosticSeverity, canonical_external_asset_uri};
use std::cmp::Reverse;

impl Parser<'_> {
    pub(super) fn open_group(&mut self) -> Result<(), ConversionError> {
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

    pub(super) fn close_group(&mut self) -> Result<(), ConversionError> {
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

    pub(super) fn finish_destination(
        &mut self,
        introduced: Option<Destination>,
        start: usize,
        end: usize,
    ) -> Result<(), ConversionError> {
        match introduced {
            Some(Destination::Pict) => self.finish_picture(end),
            Some(Destination::FontTable) => {
                self.font_charsets.sort_unstable_by_key(|entry| (entry.font, Reverse(entry.order)));
                self.font_charsets.dedup_by_key(|entry| entry.font);
                self.font_table_font = None;
                Ok(())
            }
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

    pub(super) fn enter_destination(
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
}

pub(super) fn destination(name: &str, ignorable: bool) -> Option<Destination> {
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

pub(super) fn is_known_non_destination_control(name: &str) -> bool {
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

pub(super) fn safe_hyperlink(instruction: &str) -> Option<String> {
    let value = instruction.trim();
    let rest = value.strip_prefix("HYPERLINK")?.trim_start();
    let target = if let Some(rest) = rest.strip_prefix('"') {
        rest.split_once('"')?.0
    } else {
        rest.split_ascii_whitespace().next()?
    };
    canonical_external_asset_uri(target).filter(|canonical| canonical == target)
}
