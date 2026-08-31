use super::budget::{LegacyBudget, malformed};
use super::builder::{OutputBuilder, locator, slide_locator};
use super::doc::{find_image, list_marker};
use crate::msg::ole::Storage;
use into_markdown_core::{Block, BlockNode, Cell, ConversionError, ListItem, TableRow};
use std::collections::{BTreeMap, BTreeSet};

const CURRENT_USER: &str = "Current User";
const POWERPOINT_DOCUMENT: &str = "PowerPoint Document";
const USER_EDIT_ATOM: u16 = 0x0ff5;
const CURRENT_USER_ATOM: u16 = 0x0ff6;
const PERSIST_PTR_FULL_BLOCK: u16 = 0x1772;
const DOCUMENT_CONTAINER: u16 = 0x03e8;
const SLIDE_CONTAINER: u16 = 0x03ee;
const SLIDE_LIST_WITH_TEXT: u16 = 0x0ff0;
const SLIDE_PERSIST_ATOM: u16 = 0x03f3;
const TEXT_HEADER_ATOM: u16 = 0x0f9f;
const TEXT_CHARS_ATOM: u16 = 0x0fa0;
const TEXT_BYTES_ATOM: u16 = 0x0fa8;
const CRYPT_SESSION_CONTAINER: u16 = 0x2f14;

pub(super) fn convert(
    root: Storage<'_>,
    budget: &mut LegacyBudget<'_>,
) -> Result<into_markdown_core::ConverterOutput, ConversionError> {
    let current_user = root
        .stream(CURRENT_USER)
        .ok_or_else(|| malformed(CURRENT_USER, "required stream is missing"))?;
    let document = root
        .stream(POWERPOINT_DOCUMENT)
        .ok_or_else(|| malformed(POWERPOINT_DOCUMENT, "required stream is missing"))?;
    let current_edit = current_user_edit(current_user)?;
    let persist = persist_directory(document, current_edit, budget)?;
    let document_offset = find_document_offset(document, &persist, budget)?;
    let records = child_records(document, document_offset, 0, budget)?;
    if records.iter().any(|record| record.kind == CRYPT_SESSION_CONTAINER) {
        return Err(ConversionError::Encrypted);
    }

    let mut slides = Vec::new();
    let mut notes = BTreeMap::<u32, Vec<String>>::new();
    for record in records {
        if record.kind != SLIDE_LIST_WITH_TEXT {
            continue;
        }
        let instance = record.instance();
        if instance == 0 {
            slides.extend(slide_references(document, record, budget)?);
        } else if instance == 2 {
            for reference in slide_references(document, record, budget)? {
                if let Some(offset) = persist.get(&reference.persist_id) {
                    notes.insert(reference.slide_id, text_at(document, *offset, budget)?);
                }
            }
        }
    }
    if slides.is_empty() {
        return Err(malformed(
            POWERPOINT_DOCUMENT,
            "document persist object contains no authoritative slide list",
        ));
    }
    budget.pages(slides.len(), POWERPOINT_DOCUMENT)?;

    let mut builder = OutputBuilder::new("ppt");
    for (index, reference) in slides.into_iter().enumerate() {
        budget.work(1, POWERPOINT_DOCUMENT)?;
        let offset = persist.get(&reference.persist_id).copied().ok_or_else(|| {
            malformed(
                POWERPOINT_DOCUMENT,
                format!("slide persist id {} is unresolved", reference.persist_id),
            )
        })?;
        let record = record_at(document, offset)?;
        if record.kind != SLIDE_CONTAINER {
            return Err(malformed(
                POWERPOINT_DOCUMENT,
                format!("persist id {} does not reference a slide container", reference.persist_id),
            ));
        }
        let text = text_at(document, offset, budget)?;
        let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let mut blocks =
            text_blocks(&mut builder, &text, number, "PowerPoint Document/slide", budget)?;
        if let Some(note_text) = notes.get(&reference.slide_id) {
            append_notes(&mut builder, &mut blocks, note_text, number, budget)?;
        }
        let title = text.iter().find(|value| !value.trim().is_empty()).cloned();
        builder.push(
            Block::Slide { number, title, blocks },
            slide_locator("PowerPoint Document", number),
        );
    }

    if let Some(pictures) = root.stream("Pictures") {
        emit_pictures(pictures, &mut builder, budget)?;
    }
    builder.warning(
        "legacyOffice.ppt.layoutPartiallyRecovered",
        "legacy drawing geometry and theme semantics were not executed; text, notes, and safe picture payloads were retained in authoritative slide order",
        Some(locator(POWERPOINT_DOCUMENT)),
    );
    Ok(builder.finish())
}

#[derive(Clone, Copy)]
struct Record {
    ver_instance: u16,
    kind: u16,
    body_start: usize,
    end: usize,
}

impl Record {
    fn instance(self) -> u16 {
        self.ver_instance >> 4
    }

    fn is_container(self) -> bool {
        self.ver_instance & 0x000f == 0x000f
    }
}

#[derive(Clone, Copy)]
struct SlideReference {
    persist_id: u32,
    slide_id: u32,
}

fn current_user_edit(bytes: &[u8]) -> Result<usize, ConversionError> {
    let atom = record_at(bytes, 0)?;
    if atom.kind != CURRENT_USER_ATOM || atom.end - atom.body_start < 16 {
        return Err(malformed(CURRENT_USER, "CurrentUserAtom is invalid or truncated"));
    }
    let atom_size = usize::try_from(le32(bytes, atom.body_start, CURRENT_USER)?)
        .map_err(|_| malformed(CURRENT_USER, "CurrentUserAtom size overflows"))?;
    if atom_size < 0x14 || atom.body_start.saturating_add(atom_size) > atom.end {
        return Err(malformed(CURRENT_USER, "CurrentUserAtom has an invalid body size"));
    }
    let token = le32(bytes, atom.body_start + 4, CURRENT_USER)?;
    if token == 0xf3d1_c4df {
        return Err(ConversionError::Encrypted);
    }
    if token != 0xe391_c05f {
        return Err(malformed(
            CURRENT_USER,
            format!("CurrentUserAtom has invalid token 0x{token:08x}"),
        ));
    }
    usize::try_from(le32(bytes, atom.body_start + 8, CURRENT_USER)?)
        .map_err(|_| malformed(CURRENT_USER, "current user-edit offset overflows"))
}

fn persist_directory(
    bytes: &[u8],
    mut edit_offset: usize,
    budget: &mut LegacyBudget<'_>,
) -> Result<BTreeMap<u32, usize>, ConversionError> {
    let mut edits = Vec::new();
    let mut seen = BTreeSet::new();
    while edit_offset != 0 {
        budget.work(1, "PowerPoint Document/UserEditAtom")?;
        if !seen.insert(edit_offset) {
            return Err(malformed(POWERPOINT_DOCUMENT, "user-edit chain contains a cycle"));
        }
        let edit = record_at(bytes, edit_offset)?;
        if edit.kind != USER_EDIT_ATOM || edit.end - edit.body_start < 28 {
            return Err(malformed(POWERPOINT_DOCUMENT, "user-edit chain contains an invalid atom"));
        }
        let previous = usize::try_from(le32(bytes, edit.body_start + 8, POWERPOINT_DOCUMENT)?)
            .map_err(|_| malformed(POWERPOINT_DOCUMENT, "previous user-edit offset overflows"))?;
        let directory = usize::try_from(le32(bytes, edit.body_start + 12, POWERPOINT_DOCUMENT)?)
            .map_err(|_| malformed(POWERPOINT_DOCUMENT, "persist directory offset overflows"))?;
        edits.push(directory);
        edit_offset = previous;
    }
    let mut persist = BTreeMap::new();
    for directory_offset in edits.into_iter().rev() {
        let record = record_at(bytes, directory_offset)?;
        if record.kind != PERSIST_PTR_FULL_BLOCK {
            return Err(malformed(POWERPOINT_DOCUMENT, "persist directory record has wrong type"));
        }
        let mut cursor = record.body_start;
        while cursor < record.end {
            budget.work(1, "PowerPoint Document/PersistDirectoryAtom")?;
            let header = le32(bytes, cursor, POWERPOINT_DOCUMENT)?;
            cursor += 4;
            let start_id = header & 0x000f_ffff;
            let count = header >> 20;
            if count == 0 {
                return Err(malformed(POWERPOINT_DOCUMENT, "persist directory has an empty run"));
            }
            for delta in 0..count {
                let value = usize::try_from(le32(bytes, cursor, POWERPOINT_DOCUMENT)?)
                    .map_err(|_| malformed(POWERPOINT_DOCUMENT, "persist offset overflows"))?;
                cursor += 4;
                persist.insert(start_id + delta, value);
            }
        }
    }
    Ok(persist)
}

fn find_document_offset(
    bytes: &[u8],
    persist: &BTreeMap<u32, usize>,
    budget: &mut LegacyBudget<'_>,
) -> Result<usize, ConversionError> {
    for offset in persist.values() {
        budget.work(1, POWERPOINT_DOCUMENT)?;
        if record_at(bytes, *offset)?.kind == DOCUMENT_CONTAINER {
            return Ok(*offset);
        }
    }
    Err(malformed(POWERPOINT_DOCUMENT, "document persist object is missing"))
}

fn slide_references(
    bytes: &[u8],
    list: Record,
    budget: &mut LegacyBudget<'_>,
) -> Result<Vec<SlideReference>, ConversionError> {
    let mut references = Vec::new();
    for record in records_in_range(bytes, list.body_start, list.end, 1, budget)? {
        if record.kind == SLIDE_PERSIST_ATOM {
            if record.end - record.body_start < 16 {
                return Err(malformed(POWERPOINT_DOCUMENT, "SlidePersistAtom is truncated"));
            }
            references.push(SlideReference {
                persist_id: le32(bytes, record.body_start, POWERPOINT_DOCUMENT)?,
                slide_id: le32(bytes, record.body_start + 12, POWERPOINT_DOCUMENT)?,
            });
        }
    }
    Ok(references)
}

fn text_at(
    bytes: &[u8],
    offset: usize,
    budget: &mut LegacyBudget<'_>,
) -> Result<Vec<String>, ConversionError> {
    let record = record_at(bytes, offset)?;
    let records = if record.is_container() {
        records_in_range(bytes, record.body_start, record.end, 1, budget)?
    } else {
        vec![record]
    };
    let mut text = Vec::new();
    let mut has_header = false;
    for record in records {
        match record.kind {
            TEXT_HEADER_ATOM => has_header = true,
            TEXT_CHARS_ATOM => {
                let body = &bytes[record.body_start..record.end];
                if !body.len().is_multiple_of(2) {
                    return Err(malformed(POWERPOINT_DOCUMENT, "TextCharsAtom has odd length"));
                }
                let value = String::from_utf16_lossy(
                    &body
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>(),
                );
                push_text(&mut text, &value);
                has_header = false;
            }
            TEXT_BYTES_ATOM => {
                let value = bytes[record.body_start..record.end]
                    .iter()
                    .map(|byte| char::from(*byte))
                    .collect::<String>();
                push_text(&mut text, &value);
                has_header = false;
            }
            _ => {}
        }
    }
    let _ = has_header;
    Ok(text)
}

fn push_text(output: &mut Vec<String>, value: &str) {
    output.extend(
        value
            .split(['\r', '\u{000b}'])
            .map(|line| {
                line.trim_matches(|character: char| character.is_whitespace() && character != '\t')
            })
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    );
}

fn append_notes(
    builder: &mut OutputBuilder,
    blocks: &mut Vec<BlockNode>,
    note_text: &[String],
    number: u32,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let mut note_blocks =
        text_blocks(builder, note_text, number, "PowerPoint Document/notes", budget)?;
    if into_markdown_core::speaker_notes::has_visible_content(
        &note_blocks,
        into_markdown_core::AssetMode::Extract,
    ) {
        let mut heading = builder.node(
            Block::Heading { level: 3, content: vec![OutputBuilder::text("Speaker notes")] },
            slide_locator("PowerPoint Document/notes", number),
        );
        into_markdown_core::speaker_notes::mark_heading(&mut heading)?;
        for block in &mut note_blocks {
            into_markdown_core::speaker_notes::mark_body(block)?;
        }
        blocks.push(heading);
        blocks.append(&mut note_blocks);
    }
    Ok(())
}

fn text_blocks(
    builder: &mut OutputBuilder,
    text: &[String],
    slide: u32,
    part: &str,
    budget: &mut LegacyBudget<'_>,
) -> Result<Vec<BlockNode>, ConversionError> {
    let mut blocks = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        budget.work(1, part)?;
        if text[cursor].contains('\t') {
            let start = cursor;
            while cursor < text.len() && text[cursor].contains('\t') {
                cursor += 1;
            }
            let columns =
                text[start..cursor].iter().map(|row| row.split('\t').count()).max().unwrap_or(0);
            budget.table_shape(cursor - start, columns)?;
            let mut rows = text[start..cursor]
                .iter()
                .map(|row| TableRow {
                    cells: row
                        .split('\t')
                        .map(|value| Cell {
                            row_span: 1,
                            column_span: 1,
                            header: false,
                            blocks: vec![builder.node(
                                Block::Paragraph(vec![OutputBuilder::text(value)]),
                                slide_locator(part, slide),
                            )],
                        })
                        .collect(),
                })
                .collect::<Vec<_>>();
            super::tables::rectangularize(&mut rows, builder, budget, part)?;
            blocks.push(
                builder.node(
                    Block::Table { rows, alignments: Vec::new() },
                    slide_locator(part, slide),
                ),
            );
            continue;
        }
        if let Some((kind, start, _, _)) = list_marker(&text[cursor]) {
            let mut items = Vec::new();
            while cursor < text.len() {
                let Some((next_kind, _, marker, contents)) = list_marker(&text[cursor]) else {
                    break;
                };
                if next_kind != kind {
                    break;
                }
                items.push(ListItem {
                    checked: None,
                    marker_label: Some(marker.into()),
                    blocks: vec![builder.node(
                        Block::Paragraph(vec![OutputBuilder::text(contents)]),
                        slide_locator(part, slide),
                    )],
                });
                cursor += 1;
            }
            blocks
                .push(builder.node(Block::List { kind, start, items }, slide_locator(part, slide)));
            continue;
        }
        blocks.push(builder.node(
            Block::Paragraph(vec![OutputBuilder::text(&text[cursor])]),
            slide_locator(part, slide),
        ));
        cursor += 1;
    }
    Ok(blocks)
}

fn child_records(
    bytes: &[u8],
    offset: usize,
    depth: u16,
    budget: &mut LegacyBudget<'_>,
) -> Result<Vec<Record>, ConversionError> {
    let record = record_at(bytes, offset)?;
    if !record.is_container() {
        return Err(malformed(POWERPOINT_DOCUMENT, "expected a container record"));
    }
    records_in_range(bytes, record.body_start, record.end, depth + 1, budget)
}

fn records_in_range(
    bytes: &[u8],
    start: usize,
    end: usize,
    depth: u16,
    budget: &mut LegacyBudget<'_>,
) -> Result<Vec<Record>, ConversionError> {
    budget.depth(depth, POWERPOINT_DOCUMENT)?;
    let mut cursor = start;
    let mut output = Vec::new();
    while cursor < end {
        budget.work(1, POWERPOINT_DOCUMENT)?;
        let record = record_at(bytes, cursor)?;
        if record.end > end {
            return Err(malformed(POWERPOINT_DOCUMENT, "child record escapes its container"));
        }
        output.push(record);
        if record.is_container() {
            output.extend(records_in_range(
                bytes,
                record.body_start,
                record.end,
                depth + 1,
                budget,
            )?);
        }
        cursor = record.end;
    }
    Ok(output)
}

fn record_at(bytes: &[u8], offset: usize) -> Result<Record, ConversionError> {
    let ver_instance = le16(bytes, offset, POWERPOINT_DOCUMENT)?;
    let kind = le16(bytes, offset + 2, POWERPOINT_DOCUMENT)?;
    let length = usize::try_from(le32(bytes, offset + 4, POWERPOINT_DOCUMENT)?)
        .map_err(|_| malformed(POWERPOINT_DOCUMENT, "record length overflows"))?;
    let body_start = offset
        .checked_add(8)
        .ok_or_else(|| malformed(POWERPOINT_DOCUMENT, "record offset overflows"))?;
    let end = body_start
        .checked_add(length)
        .ok_or_else(|| malformed(POWERPOINT_DOCUMENT, "record range overflows"))?;
    if end > bytes.len() {
        return Err(malformed(POWERPOINT_DOCUMENT, "record is truncated"));
    }
    Ok(Record { ver_instance, kind, body_start, end })
}

fn emit_pictures(
    pictures: &[u8],
    builder: &mut OutputBuilder,
    budget: &mut LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    let mut cursor = 0usize;
    let mut count = 0usize;
    while let Some((start, end, media_type)) = find_image(&pictures[cursor..]) {
        let start = cursor + start;
        let end = cursor + end;
        budget.raster(&pictures[start..end], media_type, "Pictures/image")?;
        budget.asset(end - start, "Pictures/image")?;
        count += 1;
        let extension = if media_type == "image/png" { "png" } else { "jpg" };
        let asset = builder.asset(
            &format!("powerpoint-image-{count}.{extension}"),
            media_type,
            pictures[start..end].to_vec(),
        );
        builder.push(Block::Image { asset, alt: None }, locator("Pictures"));
        cursor = end;
    }
    if count > 0 {
        builder.warning(
            "legacyOffice.ppt.imagePlacementRecovered",
            "safe picture payloads were retained in source order because drawing anchors were incomplete",
            Some(locator("Pictures")),
        );
    }
    Ok(())
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
    use into_markdown_core::{
        ConversionOptions, ExecutionContext, ExecutionOptions, ResourceLimits,
    };

    fn container(child: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(child.len() + 8);
        bytes.extend_from_slice(&0x000fu16.to_le_bytes());
        bytes.extend_from_slice(&DOCUMENT_CONTAINER.to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(child.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(child);
        bytes
    }

    #[test]
    fn record_ranges_are_authenticated() {
        let bytes = [0x0f, 0, 0xe8, 3, 4, 0, 0, 0, 1, 2, 3, 4];
        let record = record_at(&bytes, 0).unwrap();
        assert_eq!(record.kind, DOCUMENT_CONTAINER);
        assert_eq!(record.end, bytes.len());
        assert!(record.is_container());
    }

    #[test]
    fn truncated_record_is_malformed() {
        let bytes = [0, 0, 0, 0, 8, 0, 0, 0];
        assert!(matches!(record_at(&bytes, 0), Err(ConversionError::Malformed { .. })));
    }

    #[test]
    fn deeply_nested_records_use_the_request_depth_limit() {
        let level_one = container(&[]);
        let level_two = container(&level_one);
        let level_three = container(&level_two);
        let bytes = container(&level_three);
        let limits = ResourceLimits { max_nesting_depth: 2, ..ResourceLimits::default() };
        let options = ConversionOptions { limits, ..ConversionOptions::default() };
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut budget = LegacyBudget::new(bytes.len(), &options, &context).unwrap();
        assert!(matches!(
            child_records(&bytes, 0, 0, &mut budget),
            Err(ConversionError::ResourceLimit { limit: "max_nesting_depth", .. })
        ));
    }

    #[test]
    fn recovered_text_boxes_keep_lists_and_tables_structured() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut budget = LegacyBudget::new(128, &options, &context).unwrap();
        let mut builder = OutputBuilder::new("ppt");
        let text =
            vec!["• first".to_owned(), "• second".to_owned(), "A\tB".to_owned(), "1\t2".to_owned()];
        let blocks = text_blocks(&mut builder, &text, 1, "slide", &mut budget).unwrap();
        assert!(matches!(blocks[0].block, Block::List { .. }));
        assert!(matches!(blocks[1].block, Block::Table { .. }));
    }

    #[test]
    fn leading_trailing_and_interior_empty_cells_keep_their_columns() {
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let mut budget = LegacyBudget::new(128, &options, &context).unwrap();
        let mut builder = OutputBuilder::new("ppt");
        let mut text = Vec::new();
        push_text(&mut text, "\tA\t\t\rB\tC");
        assert_eq!(text, ["\tA\t\t", "B\tC"]);
        let blocks = text_blocks(&mut builder, &text, 1, "slide", &mut budget).unwrap();
        let Block::Table { rows, .. } = &blocks[0].block else { panic!("table") };
        assert_eq!(rows.iter().map(|row| row.cells.len()).collect::<Vec<_>>(), [4, 4]);
        assert_eq!(
            rows[0].cells[1].blocks[0].block,
            Block::Paragraph(vec![OutputBuilder::text("A")])
        );
        let document = into_markdown_core::Document { blocks, ..Default::default() };
        document.validate().unwrap();
    }
}
