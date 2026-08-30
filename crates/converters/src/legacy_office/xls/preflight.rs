use super::{
    ARRAY, BIFF4, BIFF5, BIFF8, BOF, BOF4, BOUND_SHEET, Block, BlockNode, CONTINUE,
    ConversionError, ConverterOutput, DIMENSIONS, Diagnostic, DiagnosticSeverity, EOF,
    EXTERN_SHEET, ErrorPolicy, FILE_PASS, FORMULA, LegacyBudget, MSO_DRAWING, MSO_DRAWING_GROUP,
    OBJ, PANE, SCL, SELECTION, SHARED_FORMULA, STRING, SUP_BOOK, TABLE, WINDOW1, WINDOW2, limit,
    locator, malformed, read_u16, read_u32,
};
use std::collections::BTreeMap;

pub(super) fn append_preflight_diagnostics(
    output: &mut ConverterOutput,
    preflight: &Preflight,
    part: &str,
) {
    if preflight.has(PreflightFlag::ExternalBindings) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.externalBindingsSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "external workbook bindings were retained as inert formula text and were not resolved"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if preflight.has(PreflightFlag::TailPadding) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.trailingPaddingIgnored".into(),
            severity: DiagnosticSeverity::Info,
            message: "incomplete zero padding after a complete BIFF substream was ignored".into(),
            locator: Some(locator(part)),
        });
    }
    if matches!(preflight.biff_version, BIFF4 | BIFF5) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.legacyBiffRecovered".into(),
            severity: DiagnosticSeverity::Info,
            message: format!(
                "BIFF{} workbook data was converted through the bounded legacy reader",
                if preflight.biff_version == BIFF4 { 4 } else { 5 }
            ),
            locator: Some(locator(part)),
        });
    }
    if preflight.has(PreflightFlag::DimensionMetadata) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.dimensionMetadataRecovered".into(),
            severity: DiagnosticSeverity::Info,
            message: "non-canonical Dimensions metadata was authenticated and omitted from the compatibility reader"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if preflight.has(PreflightFlag::FormulaCacheMetadata) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.formulaStringCacheRecovered".into(),
            severity: DiagnosticSeverity::Info,
            message: "non-canonical reserved bytes in cached formula-string metadata were normalized without evaluating the formula"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if preflight.has(PreflightFlag::OptionalTailRecord) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.optionalTailRecordIgnored".into(),
            severity: DiagnosticSeverity::Info,
            message: "a truncated trailing worksheet-view record after a complete BIFF substream was ignored"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if preflight.has(PreflightFlag::EmbeddedObjects) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.embeddedObjectsSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "embedded OLE, drawing, and ActiveX objects were not executed or exported"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if preflight.has(PreflightFlag::SheetNameMetadata) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.longSheetNameRecovered".into(),
            severity: DiagnosticSeverity::Info,
            message: "a complete, bounded sheet name exceeding the canonical 31-character limit was retained"
                .into(),
            locator: Some(locator(part)),
        });
    }
    if preflight.has(PreflightFlag::NestedCharts) {
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.chartCachesSkipped".into(),
            severity: DiagnosticSeverity::Warning,
            message: "nested chart caches were excluded from worksheet cells and were not rendered"
                .into(),
            locator: Some(locator(part)),
        });
    }
}

pub(super) fn biff_record<'a>(
    bytes: &'a [u8],
    cursor: usize,
    part: &str,
) -> Result<(u16, &'a [u8], usize), ConversionError> {
    let header = bytes
        .get(cursor..cursor.saturating_add(4))
        .ok_or_else(|| malformed(part, "truncated BIFF record header"))?;
    let kind = u16::from_le_bytes([header[0], header[1]]);
    let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
    let body_start =
        cursor.checked_add(4).ok_or_else(|| malformed(part, "BIFF record offset overflowed"))?;
    let end = body_start
        .checked_add(length)
        .ok_or_else(|| malformed(part, "BIFF record length overflowed"))?;
    let body =
        bytes.get(body_start..end).ok_or_else(|| malformed(part, "truncated BIFF record body"))?;
    Ok((kind, body, end))
}

pub(super) fn decode_continued_formula_string(
    workbook: &[u8],
    cursor: usize,
    part: &str,
    budget: &mut LegacyBudget<'_>,
    max_field_bytes: u64,
) -> Result<Option<String>, ConversionError> {
    let (mut kind, mut body, mut next) = biff_record(workbook, cursor, part)?;
    if matches!(kind, SHARED_FORMULA | ARRAY | TABLE) {
        validate_formula_attachment(kind, body, part)?;
        (kind, body, next) = biff_record(workbook, next, part)?;
    }
    if kind != STRING {
        return Err(malformed(part, "string-valued Formula is not followed by a String record"));
    }
    let characters = usize::from(read_u16(body, 0, part)?);
    let flags = *body.get(2).ok_or_else(|| malformed(part, "truncated BIFF String flags"))?;
    let mut offset = 3_usize;
    if flags & 0x08 != 0 {
        offset = offset
            .checked_add(2)
            .ok_or_else(|| malformed(part, "BIFF rich-string offset overflowed"))?;
    }
    if flags & 0x04 != 0 {
        offset = offset
            .checked_add(4)
            .ok_or_else(|| malformed(part, "BIFF phonetic-string offset overflowed"))?;
    }
    if offset > body.len() {
        return Err(malformed(part, "truncated BIFF String metadata"));
    }
    let initial_width = if flags & 1 == 0 { 1 } else { 2 };
    let initial_capacity = body.len().saturating_sub(offset) / initial_width;
    if characters <= initial_capacity {
        return Ok(None);
    }
    if u64::try_from(characters).unwrap_or(u64::MAX) > max_field_bytes {
        return Err(limit(
            "max_field_bytes",
            "continued cached formula string exceeds the field limit",
        ));
    }
    let capacity = characters
        .checked_mul(3)
        .ok_or_else(|| limit("max_memory_bytes", "continued formula string size overflowed"))?;
    let mut output = String::new();
    output.try_reserve_exact(capacity).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve continued formula string: {error}"))
    })?;
    let mut remaining = characters;
    let mut segment = &body[offset..];
    let mut width = initial_width;
    let mut pending_high_surrogate = None;
    loop {
        let available = segment.len() / width;
        let take = remaining.min(available);
        budget.work(u64::try_from(take).unwrap_or(u64::MAX), part)?;
        if width == 1 {
            if pending_high_surrogate.is_some() {
                return Err(malformed(part, "BIFF String changes encoding within a surrogate"));
            }
            output.extend(segment[..take].iter().map(|byte| char::from(*byte)));
        } else {
            append_utf16_string_units(
                &mut output,
                &segment[..take * 2],
                &mut pending_high_surrogate,
                part,
            )?;
        }
        remaining -= take;
        if remaining == 0 {
            break;
        }
        let (kind, continuation, end) = biff_record(workbook, next, part)?;
        if kind != CONTINUE {
            return Err(malformed(part, "BIFF String continuation is missing"));
        }
        let continuation_flags = *continuation
            .first()
            .ok_or_else(|| malformed(part, "empty BIFF String continuation"))?;
        if continuation_flags & !1 != 0 {
            return Err(malformed(part, "invalid BIFF String continuation flags"));
        }
        width = if continuation_flags == 0 { 1 } else { 2 };
        segment = &continuation[1..];
        next = end;
    }
    if pending_high_surrogate.is_some() {
        return Err(malformed(part, "BIFF String ends with an incomplete surrogate"));
    }
    if u64::try_from(output.len()).unwrap_or(u64::MAX) > max_field_bytes {
        return Err(limit(
            "max_field_bytes",
            "continued cached formula string exceeds the field limit",
        ));
    }
    Ok(Some(output))
}

fn validate_formula_attachment(kind: u16, body: &[u8], part: &str) -> Result<(), ConversionError> {
    let minimum = match kind {
        SHARED_FORMULA => 10,
        ARRAY => 14, // Range, flags, calculation chain, token count.
        TABLE => 16, // Range, flags, and two input-cell addresses.
        _ => unreachable!(),
    };
    if body.len() < minimum {
        return Err(malformed(part, "truncated Formula attachment"));
    }
    if kind == TABLE && body.len() != minimum {
        return Err(malformed(part, "invalid Table attachment length"));
    }
    if kind != TABLE {
        let token_bytes = usize::from(read_u16(body, minimum - 2, part)?);
        if token_bytes > body.len() - minimum {
            return Err(malformed(part, "truncated Formula attachment tokens"));
        }
    }
    Ok(())
}

pub(super) fn bound_sheet_substream(body: &[u8], part: &str) -> Result<u16, ConversionError> {
    match body.get(5).ok_or_else(|| malformed(part, "truncated BoundSheet type"))? {
        0 => Ok(0x0010),
        1 => Ok(0x0040),
        2 => Ok(0x0020),
        kind => Err(ConversionError::Unsupported {
            detail: format!("unsupported BoundSheet type 0x{kind:02x}"),
        }),
    }
}

pub(super) fn bound_sheet_name_length(
    body: &[u8],
    part: &str,
    error_policy: ErrorPolicy,
) -> Result<usize, ConversionError> {
    let characters = usize::from(
        *body.get(6).ok_or_else(|| malformed(part, "truncated BoundSheet name length"))?,
    );
    if characters == 0 || (characters > 31 && error_policy == ErrorPolicy::Strict) {
        return Err(malformed(part, "invalid BoundSheet name length"));
    }
    Ok(characters)
}

pub(super) fn append_utf16_string_units(
    output: &mut String,
    bytes: &[u8],
    pending_high_surrogate: &mut Option<u16>,
    part: &str,
) -> Result<(), ConversionError> {
    for chunk in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if let Some(high) = pending_high_surrogate.take() {
            if !(0xdc00..=0xdfff).contains(&unit) {
                return Err(malformed(part, "invalid UTF-16 surrogate in BIFF String"));
            }
            let scalar = 0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
            output.push(
                char::from_u32(scalar)
                    .ok_or_else(|| malformed(part, "invalid Unicode scalar in BIFF String"))?,
            );
        } else if (0xd800..=0xdbff).contains(&unit) {
            *pending_high_surrogate = Some(unit);
        } else if (0xdc00..=0xdfff).contains(&unit) {
            return Err(malformed(part, "orphan UTF-16 surrogate in BIFF String"));
        } else {
            output.push(
                char::from_u32(u32::from(unit))
                    .ok_or_else(|| malformed(part, "invalid Unicode scalar in BIFF String"))?,
            );
        }
    }
    Ok(())
}

pub(super) fn enforce_document_node_limit(output: &ConverterOutput) -> Result<(), ConversionError> {
    let mut count = 0_usize;
    count_document_nodes(&output.document.blocks, &mut count)?;
    Ok(())
}

fn count_document_nodes(blocks: &[BlockNode], count: &mut usize) -> Result<(), ConversionError> {
    for node in blocks {
        *count = count
            .checked_add(1)
            .ok_or_else(|| limit("documentNodes", "legacy XLS node count overflowed"))?;
        match &node.block {
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => count_document_nodes(blocks, count)?,
            Block::List { items, .. } => {
                for item in items {
                    *count = count.checked_add(1).ok_or_else(|| {
                        limit("documentNodes", "legacy XLS list-node count overflowed")
                    })?;
                    count_document_nodes(&item.blocks, count)?;
                }
            }
            Block::Table { rows, .. } => {
                for row in rows {
                    *count = count
                        .checked_add(1)
                        .and_then(|value| value.checked_add(row.cells.len()))
                        .ok_or_else(|| {
                            limit("documentNodes", "legacy XLS table-node count overflowed")
                        })?;
                    for cell in &row.cells {
                        count_document_nodes(&cell.blocks, count)?;
                    }
                }
            }
            _ => {}
        }
        if *count > into_markdown_core::MAX_DOCUMENT_NODES {
            return Err(limit(
                "documentNodes",
                format!(
                    "legacy XLS output exceeds the {} node limit",
                    into_markdown_core::MAX_DOCUMENT_NODES
                ),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum PreflightFlag {
    ExternalBindings,
    EmbeddedObjects,
    TailPadding,
    OptionalTailRecord,
    DimensionMetadata,
    FormulaCacheMetadata,
    SheetNameMetadata,
    NestedCharts,
}

#[derive(Default)]
pub(super) struct PreflightFlags(u8);

impl PreflightFlags {
    fn insert(&mut self, flag: PreflightFlag) {
        self.0 |= 1_u8 << flag as u8;
    }

    fn contains(&self, flag: PreflightFlag) -> bool {
        self.0 & (1_u8 << flag as u8) != 0
    }
}

#[derive(Default)]
pub(super) struct Preflight {
    flags: PreflightFlags,
    pub(super) biff_version: u16,
    pub(super) logical_end: usize,
    pub(super) hints: crate::workbook::LegacyXlsHints,
}

impl Preflight {
    pub(super) fn has(&self, flag: PreflightFlag) -> bool {
        self.flags.contains(flag)
    }
}

#[derive(Default)]
struct PreflightState {
    biff_version: Option<u16>,
    at_substream_boundary: bool,
    zero_padding_start: Option<usize>,
    bound_sheet_offsets: BTreeMap<u32, u16>,
    completed_sheet_offsets: BTreeMap<u32, u16>,
    open_sheet_offset: Option<u32>,
    substreams: [u16; 16],
    substream_depth: usize,
    completed_globals: usize,
    completed_worksheets: usize,
    result: Preflight,
}

impl PreflightState {
    fn final_substream_proven(&self) -> bool {
        final_substream_proven(
            &self.bound_sheet_offsets,
            &self.completed_sheet_offsets,
            (self.substream_depth != 0).then_some(self.substreams[0]),
            self.completed_globals,
            self.completed_worksheets,
        )
    }

    fn handle_record(
        &mut self,
        kind: u16,
        body: &[u8],
        cursor: usize,
        part: &str,
        budget: &mut LegacyBudget<'_>,
        error_policy: ErrorPolicy,
    ) -> Result<(), ConversionError> {
        self.at_substream_boundary = false;
        self.zero_padding_start = None;
        match kind {
            BOF | BOF4 => self.open_biff_substream(kind, body, cursor, part, error_policy)?,
            FILE_PASS => return Err(ConversionError::Encrypted),
            EOF => self.close_biff_substream(part)?,
            BOUND_SHEET => {
                let offset = read_u32(body, 0, part)?;
                let substream = bound_sheet_substream(body, part)?;
                if bound_sheet_name_length(body, part, error_policy)? > 31 {
                    self.result.flags.insert(PreflightFlag::SheetNameMetadata);
                }
                if self.bound_sheet_offsets.insert(offset, substream).is_some() {
                    return Err(malformed(part, "duplicate BoundSheet offset"));
                }
            }
            DIMENSIONS => {
                if preflight_dimensions(body, self.biff_version, part, budget, error_policy)? {
                    self.result.flags.insert(PreflightFlag::DimensionMetadata);
                }
            }
            FORMULA if has_noncanonical_formula_string_cache(body) => {
                if error_policy == ErrorPolicy::Strict {
                    return Err(malformed(part, "non-canonical cached formula-string metadata"));
                }
                self.result.flags.insert(PreflightFlag::FormulaCacheMetadata);
            }
            SUP_BOOK | EXTERN_SHEET => {
                self.result.flags.insert(PreflightFlag::ExternalBindings);
            }
            OBJ | MSO_DRAWING_GROUP | MSO_DRAWING => {
                self.result.flags.insert(PreflightFlag::EmbeddedObjects);
            }
            _ => {}
        }
        Ok(())
    }

    fn open_biff_substream(
        &mut self,
        kind: u16,
        body: &[u8],
        cursor: usize,
        part: &str,
        error_policy: ErrorPolicy,
    ) -> Result<(), ConversionError> {
        let version = if kind == BOF4 { BIFF4 } else { read_u16(body, 0, part)? };
        let supported = version == BIFF8
            || (matches!(version, BIFF4 | BIFF5) && error_policy == ErrorPolicy::BestEffort);
        if !supported {
            return Err(ConversionError::Unsupported {
                detail: format!("XLS BIFF version 0x{version:04x} predates Office 97-2003 BIFF8"),
            });
        }
        if self.biff_version.is_some_and(|current| current != version) {
            return Err(malformed(part, "BIFF substreams disagree on workbook version"));
        }
        self.biff_version = Some(version);
        let substream = read_u16(body, 2, part)?;
        if self.substream_depth == self.substreams.len() {
            return Err(malformed(part, "BIFF substream nesting is too deep"));
        }
        if self.substream_depth != 0 {
            let parent = self.substreams[self.substream_depth - 1];
            if parent != 0x0010 || substream != 0x0020 {
                return Err(malformed(part, "unsupported nested BIFF substream"));
            }
            self.result.flags.insert(PreflightFlag::NestedCharts);
        }
        self.substreams[self.substream_depth] = substream;
        self.substream_depth += 1;
        if self.substream_depth == 1 && matches!(substream, 0x0010 | 0x0020 | 0x0040) {
            let offset = u32::try_from(cursor)
                .map_err(|_| malformed(part, "worksheet BOF offset overflowed"))?;
            if (!self.bound_sheet_offsets.is_empty() || substream != 0x0010)
                && self.bound_sheet_offsets.get(&offset) != Some(&substream)
            {
                return Err(malformed(
                    part,
                    "sheet BOF offset/type is not authenticated by BoundSheet",
                ));
            }
            self.open_sheet_offset = Some(offset);
        }
        Ok(())
    }

    fn close_biff_substream(&mut self, part: &str) -> Result<(), ConversionError> {
        if self.substream_depth == 0 {
            return Err(malformed(part, "BIFF EOF has no open substream"));
        }
        self.substream_depth -= 1;
        let substream = self.substreams[self.substream_depth];
        if self.substream_depth == 0 && matches!(substream, 0x0010 | 0x0020 | 0x0040) {
            let offset = self
                .open_sheet_offset
                .take()
                .ok_or_else(|| malformed(part, "worksheet EOF has no BOF offset"))?;
            if self.completed_sheet_offsets.insert(offset, substream).is_some() {
                return Err(malformed(part, "duplicate worksheet substream"));
            }
            if substream == 0x0010 {
                self.completed_worksheets = self
                    .completed_worksheets
                    .checked_add(1)
                    .ok_or_else(|| malformed(part, "worksheet count overflowed"))?;
            }
        } else if substream == 0x0005 {
            if self.substream_depth != 0 {
                return Err(malformed(part, "workbook globals close inside another substream"));
            }
            self.completed_globals = self
                .completed_globals
                .checked_add(1)
                .ok_or_else(|| malformed(part, "workbook globals count overflowed"))?;
            if self.completed_globals > 1 {
                return Err(malformed(part, "duplicate workbook globals substream"));
            }
        }
        self.at_substream_boundary = self.substream_depth == 0;
        Ok(())
    }

    fn finish(
        mut self,
        bytes_len: usize,
        part: &str,
        error_policy: ErrorPolicy,
    ) -> Result<Preflight, ConversionError> {
        if self.zero_padding_start.is_some() {
            if error_policy == ErrorPolicy::Strict {
                return Err(malformed(part, "zero padding follows the final BIFF substream"));
            }
            self.result.flags.insert(PreflightFlag::TailPadding);
            self.result.logical_end = self.zero_padding_start.unwrap_or(bytes_len);
        }
        self.result.biff_version = self
            .biff_version
            .ok_or_else(|| malformed(part, "Workbook stream has no BIFF8 BOF record"))?;
        if !self.final_substream_proven() {
            return Err(malformed(part, "BIFF workbook substreams are incomplete or ambiguous"));
        }
        Ok(self.result)
    }
}

pub(super) fn preflight(
    bytes: &[u8],
    part: &str,
    budget: &mut LegacyBudget<'_>,
    error_policy: ErrorPolicy,
) -> Result<Preflight, ConversionError> {
    let mut cursor = 0usize;
    let mut state = PreflightState {
        result: Preflight { logical_end: bytes.len(), ..Preflight::default() },
        ..PreflightState::default()
    };
    while cursor < bytes.len() {
        budget.work(1, part)?;
        let Some(header) = bytes.get(cursor..cursor.saturating_add(4)) else {
            if error_policy == ErrorPolicy::BestEffort
                && (state.at_substream_boundary || state.zero_padding_start.is_some())
                && bytes[cursor..].iter().all(|byte| *byte == 0)
                && state.final_substream_proven()
            {
                state.result.flags.insert(PreflightFlag::TailPadding);
                state.result.logical_end = state.zero_padding_start.unwrap_or(cursor);
                break;
            }
            return Err(malformed(part, "truncated BIFF record header"));
        };
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let body_start = cursor
            .checked_add(4)
            .ok_or_else(|| malformed(part, "BIFF record offset overflowed"))?;
        let end = body_start
            .checked_add(length)
            .ok_or_else(|| malformed(part, "BIFF record length overflowed"))?;
        let Some(body) = bytes.get(body_start..end) else {
            if error_policy == ErrorPolicy::BestEffort
                && state.at_substream_boundary
                && is_ignorable_tail_record(kind)
                && state.final_substream_proven()
            {
                state.result.flags.insert(PreflightFlag::OptionalTailRecord);
                state.result.logical_end = cursor;
                break;
            }
            return Err(malformed(part, "truncated BIFF record body"));
        };
        if state.at_substream_boundary && kind == 0 && length == 0 {
            state.zero_padding_start.get_or_insert(cursor);
            cursor = end;
            continue;
        }
        state.handle_record(kind, body, cursor, part, budget, error_policy)?;
        cursor = end;
    }
    state.finish(bytes.len(), part, error_policy)
}

fn final_substream_proven(
    bound_sheet_offsets: &BTreeMap<u32, u16>,
    completed_sheet_offsets: &BTreeMap<u32, u16>,
    open_substream: Option<u16>,
    completed_globals: usize,
    completed_worksheets: usize,
) -> bool {
    open_substream.is_none()
        && if bound_sheet_offsets.is_empty() {
            (completed_globals == 1 && completed_worksheets == 0)
                || (completed_globals == 0 && completed_worksheets == 1)
        } else {
            completed_globals == 1 && bound_sheet_offsets == completed_sheet_offsets
        }
}

pub(super) fn is_ignorable_tail_record(kind: u16) -> bool {
    matches!(kind, WINDOW1 | WINDOW2 | PANE | SELECTION | SCL)
}

pub(super) fn has_noncanonical_formula_string_cache(body: &[u8]) -> bool {
    body.get(6) == Some(&0)
        && body.get(12..14) == Some(&[0xff, 0xff])
        && body.get(7..12).is_some_and(|reserved| reserved.iter().any(|byte| *byte != 0))
}

pub(super) fn preflight_dimensions(
    body: &[u8],
    biff_version: Option<u16>,
    part: &str,
    budget: &mut LegacyBudget<'_>,
    error_policy: ErrorPolicy,
) -> Result<bool, ConversionError> {
    let legacy = matches!(biff_version, Some(BIFF4 | BIFF5));
    let expected_length = if legacy { 10 } else { 14 };
    let recovered_metadata = body.len() != expected_length;
    if recovered_metadata && error_policy == ErrorPolicy::Strict {
        return Err(malformed(part, "non-canonical BIFF Dimensions record length"));
    }
    let (first_row, last_row, first_column, last_column) = if legacy {
        if body.len() < 8 {
            return Err(malformed(part, "truncated BIFF5 Dimensions record"));
        }
        (
            u64::from(read_u16(body, 0, part)?),
            u64::from(read_u16(body, 2, part)?),
            u64::from(read_u16(body, 4, part)?),
            u64::from(read_u16(body, 6, part)?),
        )
    } else {
        if body.len() < 12 {
            return Err(malformed(part, "truncated BIFF8 Dimensions record"));
        }
        (
            u64::from(read_u32(body, 0, part)?),
            u64::from(read_u32(body, 4, part)?),
            u64::from(read_u16(body, 8, part)?),
            u64::from(read_u16(body, 10, part)?),
        )
    };
    if last_row < first_row || last_column < first_column {
        return Err(malformed(part, "BIFF8 Dimensions range is reversed"));
    }
    budget.table_shape(
        usize::try_from(last_row - first_row).unwrap_or(usize::MAX),
        usize::try_from(last_column - first_column).unwrap_or(usize::MAX),
    )?;
    Ok(recovered_metadata)
}
