use crate::text;
use into_markdown_core::{
    Block, BlockNode, BoxFuture, Cell, CellRef, ConversionError, ConversionOptions, Converter,
    ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ExecutionContext, FormatCandidate,
    Inline, InputFormat, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS, NodeId,
    ProbeOutcome, Provenance, ProvenanceKind, RaggedRowsMode, ResolvedInput, Services,
    SourceLocator, TableHeaderMode, TableRow,
};

const FORMATS: &[InputFormat] = &[InputFormat::Csv, InputFormat::Tsv];
const PROVIDER_ID: &str = "builtin.converter.delimited-text";
const RAGGED_CODE: &str = "delimited.raggedRecordPadded";

/// RFC 4180 CSV and equivalent tab-separated text converter.
#[derive(Debug, Default)]
pub struct DelimitedTextConverter;

impl Converter for DelimitedTextConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        110
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
            if candidate.detector_id == "builtin.detector.content" {
                let options = ConversionOptions::default();
                let probe_memory = u64::try_from(input.bytes.len())
                    .ok()
                    .and_then(|size| size.checked_mul(4))
                    .ok_or_else(|| ConversionError::ResourceLimit {
                        limit: "max_memory_bytes",
                        detail: "delimited probe memory estimate overflowed".into(),
                    })?;
                let _probe_memory = context.reserve_memory(probe_memory)?;
                let (decoded, _) =
                    text::decode_source(&input.bytes, None, options.text.decoding_mode, context)?;
                let delimiter = if candidate.format == InputFormat::Tsv { '\t' } else { ',' };
                let records =
                    parse_records(&decoded.text, delimiter, candidate.format, &options, context)?;
                let width = records.first().map_or(0, |record| record.fields.len());
                if width < 2 || records.iter().any(|record| record.fields.len() != width) {
                    return Ok(ProbeOutcome::NotApplicable);
                }
            }
            Ok(ProbeOutcome::Match { confidence: 1.0 })
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
        Box::pin(async move { convert_delimited(input, candidate.format, options, context) })
    }
}

#[derive(Debug)]
struct RawField {
    value: String,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct RawRecord {
    fields: Vec<RawField>,
    start: usize,
    end: usize,
}

fn convert_delimited(
    input: &ResolvedInput,
    format: InputFormat,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let size = u64::try_from(input.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_input_bytes",
        detail: "delimited input size cannot be represented as u64".into(),
    })?;
    if size > options.limits.max_input_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_input_bytes",
            detail: format!("{size} > {}", options.limits.max_input_bytes),
        });
    }
    let working = size.checked_mul(5).ok_or_else(|| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "delimited decoding memory estimate overflowed".into(),
    })?;
    let _memory = context.reserve_memory(working)?;
    let (decoded, mut diagnostics) = text::decode_source(
        &input.bytes,
        options.text.charset.as_deref(),
        options.text.decoding_mode,
        context,
    )?;
    let delimiter = if format == InputFormat::Tsv { '\t' } else { ',' };
    let mut records = parse_records(&decoded.text, delimiter, format, options, context)?;
    enforce_shape(&mut records, options, &decoded, &mut diagnostics)?;
    let cell_count = records
        .first()
        .map_or(0_usize, |record| record.fields.len())
        .checked_mul(records.len())
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "table memory estimate overflowed".into(),
        })?;
    let table_memory = u64::try_from(cell_count)
        .ok()
        .and_then(|cells| cells.checked_mul(256))
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "table IR memory estimate overflowed".into(),
        })?;
    let _table_memory = context.reserve_memory(table_memory)?;
    let header = has_header(&records, options.delimited_text.header);
    let document = build_table(records, header, &decoded, format, context)?;
    Ok(ConverterOutput { document, diagnostics, assets: Vec::new() })
}

fn parse_records(
    text: &str,
    delimiter: char,
    format: InputFormat,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<RawRecord>, ConversionError> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    let mut value = String::new();
    let mut field_start = 0;
    let mut record_start = 0;
    let mut quoted = false;
    let mut after_quote = false;
    let mut at_field_start = true;
    let mut iter = text.char_indices().peekable();

    while let Some((offset, character)) = iter.next() {
        if offset.is_multiple_of(4096) {
            context.checkpoint()?;
        }
        if quoted {
            if character == '"' {
                if iter.peek().is_some_and(|(_, next)| *next == '"') {
                    push_field_character(&mut value, '"', options)?;
                    iter.next();
                } else {
                    quoted = false;
                    after_quote = true;
                }
            } else {
                push_field_character(&mut value, character, options)?;
            }
            continue;
        }
        if at_field_start && character == '"' {
            quoted = true;
            at_field_start = false;
            continue;
        }
        if after_quote && character != delimiter && !matches!(character, '\r' | '\n') {
            return malformed(format, offset, "unexpected character after closing quote");
        }
        if character == delimiter {
            fields.push(RawField {
                value: std::mem::take(&mut value),
                start: field_start,
                end: offset,
            });
            field_start = offset + character.len_utf8();
            at_field_start = true;
            after_quote = false;
            continue;
        }
        if matches!(character, '\r' | '\n') {
            fields.push(RawField {
                value: std::mem::take(&mut value),
                start: field_start,
                end: offset,
            });
            let terminator_end =
                if character == '\r' && iter.peek().is_some_and(|(_, next)| *next == '\n') {
                    iter.next().map_or(offset + 1, |(next, value)| next + value.len_utf8())
                } else {
                    offset + character.len_utf8()
                };
            push_record(&mut records, &mut fields, record_start, offset, options)?;
            record_start = terminator_end;
            field_start = terminator_end;
            at_field_start = true;
            after_quote = false;
            continue;
        }
        if character == '"' {
            return malformed(format, offset, "quote in an unquoted field");
        }
        push_field_character(&mut value, character, options)?;
        at_field_start = false;
    }
    if quoted {
        return malformed(format, text.len(), "unterminated quoted field");
    }
    if record_start < text.len() || records.is_empty() || field_start < text.len() {
        fields.push(RawField { value, start: field_start, end: text.len() });
        push_record(&mut records, &mut fields, record_start, text.len(), options)?;
    }
    Ok(records)
}

fn push_field_character(
    value: &mut String,
    character: char,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let next = value.len().checked_add(character.len_utf8()).ok_or_else(|| {
        ConversionError::ResourceLimit {
            limit: "max_field_bytes",
            detail: "decoded field length overflowed".into(),
        }
    })?;
    if u64::try_from(next).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_field_bytes",
            detail: format!("decoded field exceeds {} bytes", options.limits.max_field_bytes),
        });
    }
    value.push(character);
    Ok(())
}

fn push_record(
    records: &mut Vec<RawRecord>,
    fields: &mut Vec<RawField>,
    start: usize,
    end: usize,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let next = records.len().checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
        limit: "max_table_rows",
        detail: "record count overflowed".into(),
    })?;
    if u64::try_from(next).unwrap_or(u64::MAX) > options.limits.max_table_rows {
        return Err(ConversionError::ResourceLimit {
            limit: "max_table_rows",
            detail: format!("{next} > {}", options.limits.max_table_rows),
        });
    }
    records.push(RawRecord { fields: std::mem::take(fields), start, end });
    Ok(())
}

fn malformed<T>(format: InputFormat, offset: usize, detail: &str) -> Result<T, ConversionError> {
    Err(ConversionError::Malformed {
        part: Some(format.as_str().into()),
        detail: format!("{detail} at decoded byte {offset}"),
    })
}

fn enforce_shape(
    records: &mut [RawRecord],
    options: &ConversionOptions,
    decoded: &text::DecodedText,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), ConversionError> {
    let width = records.first().map_or(0, |record| record.fields.len());
    let width_u64 = u64::try_from(width).unwrap_or(u64::MAX);
    if width_u64 > options.limits.max_table_columns || width > MAX_TABLE_COLUMNS {
        return Err(ConversionError::ResourceLimit {
            limit: "max_table_columns",
            detail: format!(
                "{width} > {}",
                options.limits.max_table_columns.min(MAX_TABLE_COLUMNS as u64)
            ),
        });
    }
    let mut cells = 0_u64;
    for (row, record) in records.iter_mut().enumerate() {
        if record.fields.len() != width {
            if options.delimited_text.ragged_rows == RaggedRowsMode::Strict
                || record.fields.len() > width
            {
                return Err(ConversionError::Malformed {
                    part: Some("table".into()),
                    detail: format!(
                        "ragged record {} has {} fields; expected {width}",
                        row + 1,
                        record.fields.len()
                    ),
                });
            }
            let missing = width - record.fields.len();
            record.fields.extend((0..missing).map(|_| RawField {
                value: String::new(),
                start: record.end,
                end: record.end,
            }));
            let (start, end) = decoded.source_range(record.start, record.end);
            diagnostics.push(Diagnostic {
                code: RAGGED_CODE.into(),
                severity: DiagnosticSeverity::Warning,
                message: format!("padded record {} with {missing} empty cell(s)", row + 1),
                locator: Some(locator(start, end, None, None)),
            });
        }
        for field in &record.fields {
            let length = u64::try_from(field.value.len()).unwrap_or(u64::MAX);
            if length > options.limits.max_field_bytes {
                return Err(ConversionError::ResourceLimit {
                    limit: "max_field_bytes",
                    detail: format!(
                        "field in record {} has {length} bytes; limit is {}",
                        row + 1,
                        options.limits.max_field_bytes
                    ),
                });
            }
        }
        cells = cells.checked_add(width_u64).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_table_cells",
            detail: "cell count overflowed".into(),
        })?;
        if cells > options.limits.max_table_cells {
            return Err(ConversionError::ResourceLimit {
                limit: "max_table_cells",
                detail: format!("{cells} > {}", options.limits.max_table_cells),
            });
        }
    }
    if usize::try_from(cells).unwrap_or(usize::MAX) > MAX_DOCUMENT_INLINES
        || usize::try_from(cells.saturating_mul(2).saturating_add(records.len() as u64 + 1))
            .unwrap_or(usize::MAX)
            > MAX_DOCUMENT_NODES
    {
        return Err(ConversionError::ResourceLimit {
            limit: "max_document_nodes",
            detail: "table exceeds document IR structural budgets".into(),
        });
    }
    Ok(())
}

fn has_header(records: &[RawRecord], mode: TableHeaderMode) -> bool {
    match mode {
        TableHeaderMode::Always => true,
        TableHeaderMode::Never => false,
        TableHeaderMode::Auto => {
            let Some(first) = records.first() else { return false };
            if records.len() < 2 || first.fields.iter().any(|field| field.value.trim().is_empty()) {
                return false;
            }
            let mut labels = std::collections::BTreeSet::new();
            if !first.fields.iter().all(|field| labels.insert(field.value.trim().to_lowercase())) {
                return false;
            }
            first.fields.iter().enumerate().any(|(column, field)| {
                !is_number(&field.value)
                    && records[1..].iter().all(|record| {
                        record.fields.get(column).is_some_and(|value| is_number(&value.value))
                    })
            })
        }
    }
}

fn is_number(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.parse::<f64>().is_ok()
}

fn build_table(
    records: Vec<RawRecord>,
    header: bool,
    decoded: &text::DecodedText,
    format: InputFormat,
    context: &ExecutionContext,
) -> Result<Document, ConversionError> {
    let mut rows = Vec::with_capacity(records.len());
    for (row_index, record) in records.into_iter().enumerate() {
        context.checkpoint()?;
        let mut cells = Vec::with_capacity(record.fields.len());
        for (column_index, field) in record.fields.into_iter().enumerate() {
            if column_index.is_multiple_of(1024) {
                context.checkpoint()?;
            }
            let (start, end) = decoded.source_range(field.start, field.end);
            let cell_ref = Some(CellRef {
                row: u32::try_from(row_index).map_err(|_| ConversionError::ResourceLimit {
                    limit: "max_table_rows",
                    detail: "row coordinate overflowed".into(),
                })?,
                column: u32::try_from(column_index).map_err(|_| {
                    ConversionError::ResourceLimit {
                        limit: "max_table_columns",
                        detail: "column coordinate overflowed".into(),
                    }
                })?,
            });
            let provenance = provenance(start, end, cell_ref, Some(format));
            cells.push(Cell {
                row_span: 1,
                column_span: 1,
                header: header && row_index == 0,
                blocks: vec![BlockNode {
                    id: NodeId(format!("delimited-r{}-c{}", row_index + 1, column_index + 1)),
                    block: Block::Paragraph(vec![Inline::Text {
                        value: field.value,
                        marks: Vec::new(),
                    }]),
                    provenance,
                }],
            });
        }
        rows.push(TableRow { cells });
    }
    let (start, end) = decoded.source_range(0, decoded.text.len());
    Ok(Document {
        blocks: vec![BlockNode {
            id: NodeId("delimited-table-1".into()),
            block: Block::Table { rows },
            provenance: provenance(start, end, None, None),
        }],
        ..Document::default()
    })
}

fn provenance(
    start: usize,
    end: usize,
    cell: Option<CellRef>,
    format: Option<InputFormat>,
) -> Provenance {
    Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: PROVIDER_ID.into(),
        locator: locator(start, end, cell, format),
        confidence: Some(1.0),
    }
}

fn locator(
    start: usize,
    end: usize,
    cell: Option<CellRef>,
    format: Option<InputFormat>,
) -> SourceLocator {
    SourceLocator {
        byte_start: u64::try_from(start).ok(),
        byte_end: u64::try_from(end).ok(),
        sheet: format.map(|value| value.as_str().to_ascii_uppercase()),
        cell,
        ..SourceLocator::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ExecutionOptions, ResourceLimits, SourceMetadata};
    use std::sync::Arc;

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn convert(bytes: &[u8], format: InputFormat, options: &ConversionOptions) -> ConverterOutput {
        let input = ResolvedInput { bytes: Arc::from(bytes), metadata: SourceMetadata::default() };
        convert_delimited(&input, format, options, &context()).unwrap()
    }

    #[test]
    fn rfc_quotes_crlf_empty_and_formula_are_literal() {
        let output = convert(
            b"name,note,formula,empty\r\nAlice,\"one\r\ntwo and \"\"quote\"\"\",=1+1,\r\n",
            InputFormat::Csv,
            &ConversionOptions::default(),
        );
        let Block::Table { rows } = &output.document.blocks[0].block else { panic!() };
        assert_eq!(rows.len(), 2);
        let Block::Paragraph(value) = &rows[1].cells[1].blocks[0].block else { panic!() };
        assert!(
            matches!(&value[0], Inline::Text { value, .. } if value == "one\r\ntwo and \"quote\"")
        );
        let Block::Paragraph(value) = &rows[1].cells[2].blocks[0].block else { panic!() };
        assert!(matches!(&value[0], Inline::Text { value, .. } if value == "=1+1"));
        assert_eq!(rows[1].cells[1].blocks[0].provenance.locator.byte_start, Some(31));
    }

    #[test]
    fn utf16_bom_tsv_maps_original_bytes() {
        let mut bytes = vec![0xff, 0xfe];
        for unit in "标签\t值\r一\t2".encode_utf16() {
            bytes.extend(unit.to_le_bytes());
        }
        let output = convert(&bytes, InputFormat::Tsv, &ConversionOptions::default());
        let Block::Table { rows } = &output.document.blocks[0].block else { panic!() };
        assert_eq!(rows[0].cells[0].blocks[0].provenance.locator.byte_start, Some(2));
        assert_eq!(rows[0].cells[0].blocks[0].provenance.locator.byte_end, Some(6));
    }

    #[test]
    fn header_override_and_ragged_policy_are_deterministic() {
        let mut options = ConversionOptions::default();
        options.delimited_text.header = TableHeaderMode::Always;
        options.delimited_text.ragged_rows = RaggedRowsMode::Pad;
        let output = convert(b"a,b\n1", InputFormat::Csv, &options);
        let Block::Table { rows } = &output.document.blocks[0].block else { panic!() };
        assert!(rows[0].cells.iter().all(|cell| cell.header));
        assert_eq!(rows[1].cells.len(), 2);
        assert_eq!(output.diagnostics[0].code, RAGGED_CODE);
    }

    #[test]
    fn malformed_quotes_and_limits_are_typed() {
        let input =
            ResolvedInput { bytes: Arc::from(&b"a,\"b"[..]), metadata: SourceMetadata::default() };
        let error =
            convert_delimited(&input, InputFormat::Csv, &ConversionOptions::default(), &context())
                .unwrap_err();
        assert!(matches!(error, ConversionError::Malformed { .. }));
        let mut options = ConversionOptions::default();
        options.limits.max_table_columns = 1;
        let error = convert_delimited(
            &ResolvedInput { bytes: Arc::from(&b"a,b"[..]), metadata: SourceMetadata::default() },
            InputFormat::Csv,
            &options,
            &context(),
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_table_columns", .. }));

        let mut options = ConversionOptions::default();
        options.limits.max_field_bytes = 1;
        let error = convert_delimited(
            &ResolvedInput { bytes: Arc::from(&b"ab"[..]), metadata: SourceMetadata::default() },
            InputFormat::Csv,
            &options,
            &context(),
        )
        .unwrap_err();
        assert!(matches!(error, ConversionError::ResourceLimit { limit: "max_field_bytes", .. }));
    }

    #[test]
    fn lone_cr_trailing_cells_and_utf8_bom_are_preserved() {
        let output = convert(
            b"\xef\xbb\xbfhead,value,\rone,1,\rthree,3,",
            InputFormat::Csv,
            &ConversionOptions::default(),
        );
        let Block::Table { rows } = &output.document.blocks[0].block else { panic!() };
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].cells[0].blocks[0].provenance.locator.byte_start, Some(3));
        let Block::Paragraph(value) = &rows[2].cells[2].blocks[0].block else { panic!() };
        assert!(matches!(&value[0], Inline::Text { value, .. } if value.is_empty()));
    }

    #[test]
    fn blank_records_and_trailing_cells_pad_without_losing_offsets() {
        let mut options = ConversionOptions::default();
        options.delimited_text.ragged_rows = RaggedRowsMode::Pad;
        let output = convert(b"head,value\rone,1\r\rthree,3", InputFormat::Csv, &options);
        let Block::Table { rows } = &output.document.blocks[0].block else { panic!() };
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[2].cells.len(), 2);
        assert_eq!(rows[2].cells[0].blocks[0].provenance.locator.byte_start, Some(17));
        assert_eq!(output.diagnostics[0].code, RAGGED_CODE);
    }
}
