use super::{
    BIFF4, BIFF5, BIFF8, BOF, BOF4, Block, CONTINUE, ConversionError, ConversionOptions,
    ConverterOutput, DIMENSIONS, Diagnostic, DiagnosticSeverity, EOF, EXTERN_SHEET, ErrorPolicy,
    FILE_PASS, FORMULA, Inline, IrErrorCode, LegacyBudget, MSO_DRAWING, MSO_DRAWING_GROUP, OBJ,
    PANE, SCL, SELECTION, SHARED_FORMULA, STRING, SUP_BOOK, ValidationLimits, WINDOW1, WINDOW2,
    limit, locator, malformed, read_u16, read_u32,
};

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
}

pub(super) fn recover_continued_formula_string_caches(
    workbook: &[u8],
    output: &mut ConverterOutput,
    part: &str,
    budget: &mut LegacyBudget<'_>,
    options: &ConversionOptions,
) -> Result<usize, ConversionError> {
    let mut cursor = 0_usize;
    let mut sheet = None;
    let mut next_sheet = 0_usize;
    let mut recovered = 0_usize;
    while cursor < workbook.len() {
        budget.work(1, part)?;
        let (kind, body, end) = biff_record(workbook, cursor, part)?;
        if matches!(kind, BOF | BOF4) {
            let substream = read_u16(body, 2, part)?;
            if substream == 0x0010 {
                sheet = Some(next_sheet);
                next_sheet = next_sheet
                    .checked_add(1)
                    .ok_or_else(|| malformed(part, "BIFF worksheet count overflowed"))?;
            }
        } else if kind == EOF {
            sheet = None;
        } else if kind == FORMULA && has_noncanonical_formula_string_cache(body) && sheet.is_some()
        {
            let row = u32::from(read_u16(body, 0, part)?);
            let column = u32::from(read_u16(body, 2, part)?);
            if let Some(value) = decode_continued_formula_string(
                workbook,
                end,
                part,
                budget,
                options.limits.max_field_bytes,
            )? {
                let replaced =
                    replace_formula_cache(output, sheet.unwrap_or(0), row, column, &value);
                if !replaced {
                    cursor = end;
                    continue;
                }
                recovered = recovered
                    .checked_add(1)
                    .ok_or_else(|| malformed(part, "formula-cache recovery count overflowed"))?;
            }
        }
        cursor = end;
    }
    Ok(recovered)
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
    if kind == SHARED_FORMULA {
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

pub(super) fn replace_formula_cache(
    output: &mut ConverterOutput,
    sheet_index: usize,
    row: u32,
    column: u32,
    cache: &str,
) -> bool {
    let Some(blocks) = output
        .document
        .blocks
        .iter_mut()
        .filter_map(|node| match &mut node.block {
            Block::Sheet { blocks, .. } => Some(blocks),
            _ => None,
        })
        .nth(sheet_index)
    else {
        return false;
    };
    for node in blocks {
        let Block::Table { rows, .. } = &mut node.block else { continue };
        for table_row in rows {
            for cell in &mut table_row.cells {
                if !cell.blocks.iter().any(|node| {
                    node.provenance
                        .locator
                        .cell
                        .as_ref()
                        .is_some_and(|reference| reference.row == row && reference.column == column)
                }) {
                    continue;
                }
                for node in &mut cell.blocks {
                    let Block::Paragraph(inlines) = &mut node.block else { continue };
                    for inline in inlines {
                        match inline {
                            Inline::Text { value, .. } => {
                                cache.clone_into(value);
                                return true;
                            }
                            Inline::Code(value) if value.starts_with('=') => {
                                if let Some(marker) = value.rfind(" [cached: ") {
                                    value.truncate(marker + " [cached: ".len());
                                } else {
                                    value.push_str(" [cached: ");
                                }
                                value.push_str(cache);
                                value.push(']');
                                return true;
                            }
                            _ => {}
                        }
                    }
                }
                return false;
            }
        }
    }
    false
}

pub(super) fn enforce_document_node_limit(output: &ConverterOutput) -> Result<(), ConversionError> {
    let Err(error) = output.document.validate() else { return Ok(()) };
    if error.code == IrErrorCode::ResourceLimit
        && output
            .document
            .validate_with_limits(&ValidationLimits {
                max_nodes: usize::MAX,
                ..ValidationLimits::default()
            })
            .is_ok()
    {
        return Err(limit(
            "documentNodes",
            format!(
                "legacy XLS output exceeds the {} node limit",
                into_markdown_core::MAX_DOCUMENT_NODES
            ),
        ));
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
}

impl Preflight {
    pub(super) fn has(&self, flag: PreflightFlag) -> bool {
        self.flags.contains(flag)
    }
}

pub(super) fn preflight(
    bytes: &[u8],
    part: &str,
    budget: &mut LegacyBudget<'_>,
    error_policy: ErrorPolicy,
) -> Result<Preflight, ConversionError> {
    let mut cursor = 0usize;
    let mut biff_version = None;
    let mut at_substream_boundary = false;
    let mut zero_padding_start = None;
    let mut result = Preflight { logical_end: bytes.len(), ..Preflight::default() };
    while cursor < bytes.len() {
        budget.work(1, part)?;
        let Some(header) = bytes.get(cursor..cursor.saturating_add(4)) else {
            if error_policy == ErrorPolicy::BestEffort
                && (at_substream_boundary || zero_padding_start.is_some())
                && bytes[cursor..].iter().all(|byte| *byte == 0)
            {
                result.flags.insert(PreflightFlag::TailPadding);
                result.logical_end = zero_padding_start.unwrap_or(cursor);
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
                && at_substream_boundary
                && is_ignorable_tail_record(kind)
            {
                result.flags.insert(PreflightFlag::OptionalTailRecord);
                result.logical_end = cursor;
                break;
            }
            return Err(malformed(part, "truncated BIFF record body"));
        };
        if at_substream_boundary && kind == 0 && length == 0 {
            zero_padding_start.get_or_insert(cursor);
            cursor = end;
            continue;
        }
        at_substream_boundary = false;
        zero_padding_start = None;
        match kind {
            BOF | BOF4 => {
                let version = if kind == BOF4 { BIFF4 } else { read_u16(body, 0, part)? };
                let supported = version == BIFF8
                    || (matches!(version, BIFF4 | BIFF5)
                        && error_policy == ErrorPolicy::BestEffort);
                if !supported {
                    return Err(ConversionError::Unsupported {
                        detail: format!(
                            "XLS BIFF version 0x{version:04x} predates Office 97-2003 BIFF8"
                        ),
                    });
                }
                if biff_version.is_some_and(|current| current != version) {
                    return Err(malformed(part, "BIFF substreams disagree on workbook version"));
                }
                biff_version = Some(version);
            }
            FILE_PASS => return Err(ConversionError::Encrypted),
            EOF => at_substream_boundary = true,
            DIMENSIONS => {
                if preflight_dimensions(body, biff_version, part, budget, error_policy)? {
                    result.flags.insert(PreflightFlag::DimensionMetadata);
                }
            }
            FORMULA if has_noncanonical_formula_string_cache(body) => {
                if error_policy == ErrorPolicy::Strict {
                    return Err(malformed(part, "non-canonical cached formula-string metadata"));
                }
                result.flags.insert(PreflightFlag::FormulaCacheMetadata);
            }
            SUP_BOOK | EXTERN_SHEET => result.flags.insert(PreflightFlag::ExternalBindings),
            OBJ | MSO_DRAWING_GROUP | MSO_DRAWING => {
                result.flags.insert(PreflightFlag::EmbeddedObjects);
            }
            _ => {}
        }
        cursor = end;
    }
    if zero_padding_start.is_some() {
        if error_policy == ErrorPolicy::Strict {
            return Err(malformed(part, "zero padding follows the final BIFF substream"));
        }
        result.flags.insert(PreflightFlag::TailPadding);
        result.logical_end = zero_padding_start.unwrap_or(bytes.len());
    }
    let Some(biff_version) = biff_version else {
        return Err(malformed(part, "Workbook stream has no BIFF8 BOF record"));
    };
    result.biff_version = biff_version;
    Ok(result)
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
