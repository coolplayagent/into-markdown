use super::budget::{LegacyBudget, malformed};
use super::builder::{OutputBuilder, locator};
use crate::msg::ole::Storage;
use encoding_rs::{BIG5, Encoding, GBK, SHIFT_JIS, WINDOWS_1251, WINDOWS_1252};
use into_markdown_core::{Block, Cell, ConversionError, Inline, ListItem, ListKind, TableRow};

mod links;
mod table_output;
mod tables;

const WORD97: u16 = 0x00c1;
const WORD2003_MAX: u16 = 0x0112;
const FIB_IDENT: u16 = 0xa5ec;
const FC_CLX: usize = 0x01a2;
const LCB_CLX: usize = 0x01a6;
const CCP_TEXT: usize = 0x004c;
const CCP_FTN: usize = 0x0050;
const CCP_HDD: usize = 0x0054;
const CCP_MCR: usize = 0x0058;
const CCP_ATN: usize = 0x005c;
const CCP_EDN: usize = 0x0060;
const CCP_TXBX: usize = 0x0064;
const CCP_HDR_TXBX: usize = 0x0068;

pub(super) fn convert(
    root: Storage<'_>,
    budget: &mut LegacyBudget<'_>,
) -> Result<into_markdown_core::ConverterOutput, ConversionError> {
    let word = root
        .stream("WordDocument")
        .ok_or_else(|| malformed("WordDocument", "required stream is missing"))?;
    if le16(word, 0, "WordDocument")? != FIB_IDENT {
        return Err(malformed("WordDocument", "invalid Word FIB signature"));
    }
    let version = le16(word, 2, "WordDocument")?;
    if version < WORD97 {
        return Err(ConversionError::Unsupported {
            detail: format!("Word binary version 0x{version:04x} predates Office 97"),
        });
    }
    if version > WORD2003_MAX {
        return Err(ConversionError::Unsupported {
            detail: format!("Word binary version 0x{version:04x} is outside Office 97-2003"),
        });
    }
    let flags = le16(word, 10, "WordDocument")?;
    if flags & 0x0100 != 0 || flags & 0x8000 != 0 {
        return Err(ConversionError::Encrypted);
    }
    let table_name = if flags & 0x0200 == 0 { "0Table" } else { "1Table" };
    let table = root
        .stream(table_name)
        .ok_or_else(|| malformed(table_name, "selected Word table stream is missing"))?;
    let story_counts = story_counts(word)?;
    let main_characters = story_counts[0];
    let lid = le16(word, 6, "WordDocument")?;
    let encoding = encoding_for_lid(lid);
    let pieces = piece_table(word, table, budget)?;
    let (text, total_characters, repaired_unicode) =
        decode_pieces(word, &pieces, encoding, budget)?;
    let story_characters = story_counts.iter().sum::<usize>();
    if story_characters > total_characters {
        return Err(malformed(
            "WordDocument/CLX",
            "FIB story ranges exceed the piece-table character range",
        ));
    }
    let main = prefix_chars(&text, main_characters);
    let mut builder = OutputBuilder::new("doc");
    if repaired_unicode {
        builder.warning(
            "legacyOffice.doc.invalidUnicodeReplaced",
            "unpaired UTF-16 code units were replaced with the Unicode replacement character",
            Some(locator("WordDocument/text")),
        );
    }
    let rows = tables::read_rows(word, table, &pieces, budget)?;
    table_output::emit(main, &rows, &mut builder, budget)?;
    let footnote_start = main_characters;
    emit_notes(
        slice_chars(&text, footnote_start, story_counts[1])?,
        "footnote",
        &mut builder,
        budget,
    )?;
    let endnote_start = story_counts[..5].iter().sum::<usize>();
    emit_notes(
        slice_chars(&text, endnote_start, story_counts[5])?,
        "endnote",
        &mut builder,
        budget,
    )?;
    if story_counts[2..5].iter().chain(&story_counts[6..]).any(|count| *count > 0)
        || total_characters > story_characters
    {
        builder.warning(
            "legacyOffice.doc.additionalStoriesSkipped",
            "headers, comments, macros, or text-frame stories were present but could not be bound to a stable source location",
            Some(locator("WordDocument")),
        );
    }
    if contains_fields(main) {
        builder.warning(
            "legacyOffice.doc.fieldInstructionSkipped",
            "Word field instructions were not executed; only stored display text was retained",
            Some(locator("WordDocument")),
        );
    }
    if let Some(data) = root.stream("Data") {
        emit_images(data, &mut builder, budget)?;
    }
    builder.warning(
        "legacyOffice.doc.formattingPartiallyRecovered",
        "paragraph text, tables, lists, fields, notes, and safe images were retained; unsupported binary property runs were not guessed",
        Some(locator("WordDocument")),
    );
    let mut output = builder.finish();
    links::normalize(&mut output, budget)?;
    Ok(output)
}

fn story_counts(word: &[u8]) -> Result<[usize; 8], ConversionError> {
    let mut counts = [0usize; 8];
    for (slot, offset) in counts.iter_mut().zip([
        CCP_TEXT,
        CCP_FTN,
        CCP_HDD,
        CCP_MCR,
        CCP_ATN,
        CCP_EDN,
        CCP_TXBX,
        CCP_HDR_TXBX,
    ]) {
        *slot = usize::try_from(le32(word, offset, "WordDocument/FIB")?)
            .map_err(|_| malformed("WordDocument/FIB", "story character count overflows"))?;
    }
    counts.iter().try_fold(0usize, |total, count| {
        total
            .checked_add(*count)
            .ok_or_else(|| malformed("WordDocument/FIB", "story character total overflows"))
    })?;
    Ok(counts)
}

#[derive(Debug, Clone, Copy)]
struct Piece {
    start_cp: u32,
    end_cp: u32,
    file_offset: u32,
    compressed: bool,
}

fn piece_table(
    word: &[u8],
    table: &[u8],
    budget: &mut LegacyBudget<'_>,
) -> Result<Vec<Piece>, ConversionError> {
    let offset = usize::try_from(le32(word, FC_CLX, "WordDocument/FIB")?)
        .map_err(|_| malformed("WordDocument/FIB", "CLX offset overflows"))?;
    let length = usize::try_from(le32(word, LCB_CLX, "WordDocument/FIB")?)
        .map_err(|_| malformed("WordDocument/FIB", "CLX length overflows"))?;
    let clx = table
        .get(
            offset
                ..offset
                    .checked_add(length)
                    .ok_or_else(|| malformed("WordDocument/CLX", "CLX range overflows"))?,
        )
        .ok_or_else(|| malformed("WordDocument/CLX", "CLX falls outside the table stream"))?;
    let mut cursor = 0usize;
    let plc = loop {
        budget.work(1, "WordDocument/CLX")?;
        let kind = *clx
            .get(cursor)
            .ok_or_else(|| malformed("WordDocument/CLX", "piece table record is truncated"))?;
        cursor += 1;
        match kind {
            0x01 => {
                let bytes = usize::from(le16(clx, cursor, "WordDocument/CLX")?);
                cursor = cursor
                    .checked_add(2 + bytes)
                    .ok_or_else(|| malformed("WordDocument/CLX", "property record overflows"))?;
                if cursor > clx.len() {
                    return Err(malformed("WordDocument/CLX", "property record is truncated"));
                }
            }
            0x02 => {
                let bytes = usize::try_from(le32(clx, cursor, "WordDocument/CLX")?)
                    .map_err(|_| malformed("WordDocument/CLX", "piece table size overflows"))?;
                cursor += 4;
                break clx
                    .get(
                        cursor..cursor.checked_add(bytes).ok_or_else(|| {
                            malformed("WordDocument/CLX", "piece table range overflows")
                        })?,
                    )
                    .ok_or_else(|| malformed("WordDocument/CLX", "piece table is truncated"))?;
            }
            _ => return Err(malformed("WordDocument/CLX", "unknown CLX record type")),
        }
    };
    if plc.len() < 4 || (plc.len() - 4) % 12 != 0 {
        return Err(malformed("WordDocument/CLX", "piece table has an invalid shape"));
    }
    let count = (plc.len() - 4) / 12;
    let cp_bytes = (count + 1) * 4;
    let mut pieces = Vec::new();
    pieces.try_reserve_exact(count).map_err(|_| {
        super::budget::limit("max_memory_bytes", "piece inventory allocation failed")
    })?;
    for index in 0..count {
        budget.work(1, "WordDocument/CLX")?;
        let start_cp = le32(plc, index * 4, "WordDocument/CLX")?;
        let end_cp = le32(plc, (index + 1) * 4, "WordDocument/CLX")?;
        if end_cp < start_cp {
            return Err(malformed("WordDocument/CLX", "piece character ranges are not monotonic"));
        }
        let raw = le32(plc, cp_bytes + index * 8 + 2, "WordDocument/CLX")?;
        let compressed = raw & 0x4000_0000 != 0;
        let file_offset = if compressed { (raw & 0x3fff_ffff) / 2 } else { raw & 0x3fff_ffff };
        pieces.push(Piece { start_cp, end_cp, file_offset, compressed });
    }
    Ok(pieces)
}

fn decode_pieces(
    word: &[u8],
    pieces: &[Piece],
    encoding: &'static Encoding,
    budget: &mut LegacyBudget<'_>,
) -> Result<(String, usize, bool), ConversionError> {
    let mut output = String::new();
    let mut expected_cp = 0u32;
    let mut repaired_unicode = false;
    for piece in pieces {
        budget.work(u64::from(piece.end_cp - piece.start_cp), "WordDocument/text")?;
        if piece.start_cp != expected_cp {
            return Err(malformed("WordDocument/CLX", "piece table has a character gap"));
        }
        let chars = usize::try_from(piece.end_cp - piece.start_cp)
            .map_err(|_| malformed("WordDocument/CLX", "piece length overflows"))?;
        let start = usize::try_from(piece.file_offset)
            .map_err(|_| malformed("WordDocument/CLX", "piece offset overflows"))?;
        if piece.compressed {
            let raw = word
                .get(
                    start..start.checked_add(chars).ok_or_else(|| {
                        malformed("WordDocument/CLX", "compressed piece range overflows")
                    })?,
                )
                .ok_or_else(|| malformed("WordDocument/CLX", "compressed piece is truncated"))?;
            let (decoded, _, had_errors) = encoding.decode(raw);
            if had_errors {
                return Err(malformed("WordDocument/text", "compressed text has invalid encoding"));
            }
            output.push_str(&decoded);
        } else {
            let bytes = chars
                .checked_mul(2)
                .ok_or_else(|| malformed("WordDocument/CLX", "Unicode piece size overflows"))?;
            let raw = word
                .get(
                    start..start.checked_add(bytes).ok_or_else(|| {
                        malformed("WordDocument/CLX", "Unicode piece range overflows")
                    })?,
                )
                .ok_or_else(|| malformed("WordDocument/CLX", "Unicode piece is truncated"))?;
            for decoded in char::decode_utf16(
                raw.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
            ) {
                if let Ok(value) = decoded {
                    output.push(value);
                } else {
                    repaired_unicode = true;
                    output.push('\u{fffd}');
                }
            }
        }
        expected_cp = piece.end_cp;
    }
    Ok((output, usize::try_from(expected_cp).unwrap_or(usize::MAX), repaired_unicode))
}

fn emit_story(
    text: &str,
    builder: &mut OutputBuilder,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let mut table_rows = Vec::new();
    let mut list: Option<(ListKind, u64, Vec<ListItem>)> = None;
    for paragraph in text.split('\r') {
        budget.work(u64::try_from(paragraph.len()).unwrap_or(u64::MAX), "WordDocument/story")?;
        let raw = paragraph.replace('\u{b}', "\n").trim_matches(['\0', '\u{c}']).to_owned();
        let cleaned = visible_field_text(&raw);
        if cleaned.contains('\u{7}') {
            flush_list(builder, &mut list);
            let cells = cleaned
                .trim_end_matches('\r')
                .strip_suffix('\u{7}')
                .unwrap_or(&cleaned)
                .split_terminator('\u{7}')
                .map(|value| Cell {
                    row_span: 1,
                    column_span: 1,
                    header: false,
                    blocks: vec![
                        builder
                            .node(Block::Paragraph(field_inlines(value)), locator("WordDocument")),
                    ],
                })
                .collect::<Vec<_>>();
            if !cells.is_empty() {
                table_rows.push(TableRow { cells });
            }
            continue;
        }
        flush_table(builder, &mut table_rows, budget)?;
        if !cleaned.trim().is_empty() {
            if let Some((kind, start, marker, contents)) = list_marker(&cleaned) {
                if list.as_ref().is_some_and(|(current, _, _)| *current != kind) {
                    flush_list(builder, &mut list);
                }
                let paragraph = builder
                    .node(Block::Paragraph(field_inlines(contents)), locator("WordDocument"));
                let entry = list.get_or_insert_with(|| (kind, start, Vec::new()));
                entry.2.push(ListItem {
                    checked: None,
                    marker_label: Some(marker.to_owned()),
                    blocks: vec![paragraph],
                });
            } else {
                flush_list(builder, &mut list);
                builder.push(Block::Paragraph(field_inlines(&raw)), locator("WordDocument"));
            }
        }
    }
    flush_list(builder, &mut list);
    flush_table(builder, &mut table_rows, budget)
}

fn flush_list(builder: &mut OutputBuilder, list: &mut Option<(ListKind, u64, Vec<ListItem>)>) {
    if let Some((kind, start, items)) = list.take() {
        builder.push(Block::List { kind, start, items }, locator("WordDocument"));
    }
}

pub(super) fn list_marker(value: &str) -> Option<(ListKind, u64, &str, &str)> {
    let trimmed = value.trim_start_matches(['\t', ' ']);
    for marker in ["•", "·", "‣", "◦"] {
        if let Some(contents) = trimmed
            .strip_prefix(marker)
            .and_then(|rest| rest.strip_prefix(['\t', ' ']).map(str::trim_start))
        {
            return Some((ListKind::Bullet, 1, marker, contents));
        }
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        let (number, rest) = trimmed.split_at(digits);
        if let Some(rest) = rest
            .strip_prefix(['.', ')'])
            .and_then(|rest| rest.strip_prefix(['\t', ' ']).map(str::trim_start))
        {
            return number
                .parse::<u64>()
                .ok()
                .map(|start| (ListKind::Ordered, start, &trimmed[..=digits], rest));
        }
    }
    None
}

fn field_inlines(value: &str) -> Vec<Inline> {
    let mut output = Vec::new();
    let mut rest = value;
    while let Some(begin) = rest.find('\u{13}') {
        if begin > 0 {
            output.push(OutputBuilder::text(&rest[..begin]));
        }
        let field = &rest[begin + '\u{13}'.len_utf8()..];
        let Some(separator) = field.find('\u{14}') else {
            output.push(OutputBuilder::text(visible_field_text(&rest[begin..])));
            rest = "";
            break;
        };
        let result = &field[separator + '\u{14}'.len_utf8()..];
        let Some(end) = result.find('\u{15}') else {
            output.push(OutputBuilder::text(visible_field_text(&rest[begin..])));
            rest = "";
            break;
        };
        let display = &result[..end];
        if let Some(target) = hyperlink_target(&field[..separator]) {
            output.push(Inline::Link {
                target: target.to_owned(),
                content: vec![OutputBuilder::text(display)],
            });
        } else if !display.is_empty() {
            output.push(OutputBuilder::text(display));
        }
        rest = &result[end + '\u{15}'.len_utf8()..];
    }
    if !rest.is_empty() {
        output.push(OutputBuilder::text(rest));
    }
    output
}

fn hyperlink_target(instruction: &str) -> Option<&str> {
    let value = instruction.trim();
    let rest = value.strip_prefix("HYPERLINK")?.trim_start();
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.split('"').next().filter(|target| !target.is_empty())
    } else {
        rest.split_whitespace().next().filter(|target| !target.is_empty())
    }
}

fn emit_notes(
    text: &str,
    family: &str,
    builder: &mut OutputBuilder,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    for (index, paragraph) in text.split('\r').filter(|value| !value.trim().is_empty()).enumerate()
    {
        budget.work(u64::try_from(paragraph.len()).unwrap_or(u64::MAX), "WordDocument/notes")?;
        let note =
            builder.node(Block::Paragraph(field_inlines(paragraph)), locator("WordDocument/notes"));
        builder.push(
            Block::Footnote { label: format!("{family}-{}", index + 1), blocks: vec![note] },
            locator("WordDocument/notes"),
        );
    }
    Ok(())
}

fn slice_chars(value: &str, start: usize, length: usize) -> Result<&str, ConversionError> {
    let start_byte = value
        .char_indices()
        .nth(start)
        .map_or_else(
            || (start == value.chars().count()).then_some(value.len()),
            |(index, _)| Some(index),
        )
        .ok_or_else(|| malformed("WordDocument/FIB", "story start exceeds decoded text"))?;
    let end_char = start
        .checked_add(length)
        .ok_or_else(|| malformed("WordDocument/FIB", "story range overflows"))?;
    let end_byte = value
        .char_indices()
        .nth(end_char)
        .map_or_else(
            || (end_char == value.chars().count()).then_some(value.len()),
            |(index, _)| Some(index),
        )
        .ok_or_else(|| malformed("WordDocument/FIB", "story end exceeds decoded text"))?;
    Ok(&value[start_byte..end_byte])
}

fn flush_table(
    builder: &mut OutputBuilder,
    rows: &mut Vec<TableRow>,
    budget: &LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    if rows.is_empty() {
        return Ok(());
    }
    crate::legacy_office::tables::rectangularize(rows, builder, budget, "WordDocument")?;
    builder.push(
        Block::Table { rows: std::mem::take(rows), alignments: Vec::new() },
        locator("WordDocument"),
    );
    Ok(())
}

fn visible_field_text(value: &str) -> String {
    let mut output = String::new();
    let mut field_depth = 0u16;
    let mut showing_result = true;
    for character in value.chars() {
        match character {
            '\u{13}' => {
                field_depth = field_depth.saturating_add(1);
                showing_result = false;
            }
            '\u{14}' if field_depth > 0 => showing_result = true,
            '\u{15}' if field_depth > 0 => {
                field_depth -= 1;
                showing_result = field_depth == 0;
            }
            _ if showing_result => output.push(character),
            _ => {}
        }
    }
    output
}

fn contains_fields(value: &str) -> bool {
    value.contains('\u{13}') || value.contains('\u{14}') || value.contains('\u{15}')
}

fn emit_images(
    data: &[u8],
    builder: &mut OutputBuilder,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let mut cursor = 0usize;
    let mut count = 0usize;
    while cursor < data.len() {
        let found = find_image(&data[cursor..])
            .map(|(start, end, media)| (cursor + start, cursor + end, media));
        let Some((start, end, media_type)) = found else { break };
        let bytes = data[start..end].to_vec();
        budget.raster(&bytes, media_type, "Data/image")?;
        budget.asset(bytes.len(), "Data/image")?;
        count += 1;
        let extension = if media_type == "image/png" { "png" } else { "jpg" };
        let id = builder.asset(&format!("word-image-{count}.{extension}"), media_type, bytes);
        builder.push(Block::Image { asset: id, alt: None }, locator("Data"));
        cursor = end;
    }
    if count > 0 {
        builder.warning(
            "legacyOffice.doc.imagePlacementRecovered",
            "embedded image bytes were retained in source order because legacy drawing anchors were incomplete",
            Some(locator("Data")),
        );
    }
    Ok(())
}

pub(super) fn find_image(bytes: &[u8]) -> Option<(usize, usize, &'static str)> {
    let png = bytes.windows(8).position(|window| window == b"\x89PNG\r\n\x1a\n");
    let jpeg = bytes.windows(2).position(|window| window == b"\xff\xd8");
    match (png, jpeg) {
        (Some(start), None | Some(_)) if jpeg.is_none_or(|other| start < other) => {
            png_end(&bytes[start..]).map(|end| (start, start + end, "image/png"))
        }
        (_, Some(start)) => bytes[start + 2..]
            .windows(2)
            .position(|window| window == b"\xff\xd9")
            .map(|end| (start, start + 2 + end + 2, "image/jpeg")),
        _ => None,
    }
}

fn png_end(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut cursor = 8usize;
    loop {
        let length =
            usize::try_from(u32::from_be_bytes(bytes.get(cursor..cursor + 4)?.try_into().ok()?))
                .ok()?;
        let kind = bytes.get(cursor + 4..cursor + 8)?;
        cursor = cursor.checked_add(12 + length)?;
        if cursor > bytes.len() {
            return None;
        }
        if kind == b"IEND" {
            return Some(cursor);
        }
    }
}

fn prefix_chars(value: &str, count: usize) -> &str {
    match value.char_indices().nth(count) {
        Some((offset, _)) => &value[..offset],
        None => value,
    }
}

fn encoding_for_lid(lid: u16) -> &'static Encoding {
    match lid {
        0x0411 => SHIFT_JIS,
        0x0404 | 0x0c04 | 0x1404 => BIG5,
        0x0804 | 0x1004 => GBK,
        0x0419 | 0x0422 | 0x0423 => WINDOWS_1251,
        _ => WINDOWS_1252,
    }
}

fn le16(bytes: &[u8], offset: usize, part: &str) -> Result<u16, ConversionError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| malformed(part, "truncated little-endian u16"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn le32(bytes: &[u8], offset: usize, part: &str) -> Result<u32, ConversionError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| malformed(part, "truncated little-endian u32"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_instructions_do_not_enter_output() {
        assert_eq!(
            visible_field_text("before \u{13} HYPERLINK x \u{14}shown\u{15} after"),
            "before shown after"
        );
        assert_eq!(
            field_inlines(
                "before \u{13} HYPERLINK \"https://example.test\" \u{14}shown\u{15} after"
            ),
            vec![
                OutputBuilder::text("before "),
                Inline::Link {
                    target: "https://example.test".into(),
                    content: vec![OutputBuilder::text("shown")],
                },
                OutputBuilder::text(" after"),
            ]
        );
    }

    #[test]
    fn list_markers_require_a_delimiter() {
        assert!(matches!(list_marker("• item"), Some((ListKind::Bullet, 1, _, "item"))));
        assert!(matches!(list_marker("12) item"), Some((ListKind::Ordered, 12, _, "item"))));
        assert!(list_marker("12monkeys").is_none());
    }

    #[test]
    fn png_boundary_requires_iend() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(png_end(&png), Some(png.len()));
        assert_eq!(png_end(&png[..png.len() - 1]), None);
    }
}
