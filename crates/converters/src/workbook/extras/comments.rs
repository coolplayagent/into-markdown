use crate::workbook::budget::checked_field_bytes;
use crate::workbook::cell::{cell_name, parse_cell_ref};
use crate::workbook::error::{limit, malformed};
use crate::workbook::model::{Annotation, CellCoordinate};
use crate::workbook::opc::relationships::{decode_attr, require_spreadsheet_namespace};
use crate::workbook::schema::{MAX_EXCEL_COLUMNS, MAX_EXCEL_ROWS};
use crate::workbook::xlsb::records::{
    le_u32, read_xlsb_varint, validate_xlsb_rich_string, xlsb_wide_string,
};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use std::collections::BTreeSet;

#[allow(clippy::too_many_lines)] // One linear record-state machine keeps fail-closed ordering visible.
pub(super) fn parse_binary_comments(
    data: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<Annotation>, ConversionError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Start,
        Begun,
        Authors,
        AuthorsDone,
        Comments,
        CommentsDone,
        Done,
    }
    let mut offset = 0_usize;
    let mut authors = Vec::new();
    let mut current: Option<(CellCoordinate, usize, Option<String>)> = None;
    let mut output = Vec::new();
    let mut state = State::Start;
    let mut frt_depth = 0_u16;
    let mut uids = BTreeSet::<[u8; 16]>::new();
    while offset < data.len() {
        context.checkpoint()?;
        let typ = u16::try_from(read_xlsb_varint(data, &mut offset, 2, part)?)
            .map_err(|_| malformed(Some(part), "XLSB comment record type overflow"))?;
        let len = usize::try_from(read_xlsb_varint(data, &mut offset, 4, part)?)
            .map_err(|_| malformed(Some(part), "XLSB comment record length overflow"))?;
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated XLSB comment record"))?;
        let payload = &data[offset..end];
        match typ {
            0x0274 if state == State::Start && payload.is_empty() => state = State::Begun,
            0x0276 if state == State::Begun && payload.is_empty() => state = State::Authors,
            0x0278 => {
                if state != State::Authors || frt_depth != 0 {
                    return Err(malformed(Some(part), "BrtCommentAuthor is outside author list"));
                }
                let (author, consumed) = xlsb_wide_string(payload, 0, false, part)?;
                if consumed != payload.len() || author.chars().count() > 54 {
                    return Err(malformed(Some(part), "invalid BrtCommentAuthor"));
                }
                checked_field_bytes(
                    options,
                    "XLSB comment author",
                    &[u64::try_from(author.len()).unwrap_or(u64::MAX)],
                )?;
                authors.push(author);
            }
            0x0277 if state == State::Authors && payload.is_empty() => {
                state = State::AuthorsDone;
            }
            0x0279 if state == State::AuthorsDone && payload.is_empty() => {
                state = State::Comments;
            }
            0x027b => {
                if state != State::Comments
                    || current.is_some()
                    || frt_depth != 0
                    || payload.len() != 36
                {
                    return Err(malformed(Some(part), "invalid BrtBeginComment"));
                }
                let author_id = usize::try_from(le_u32(&payload[0..4]))
                    .map_err(|_| malformed(Some(part), "comment author id overflow"))?;
                let start = (le_u32(&payload[4..8]), le_u32(&payload[12..16]));
                let end = (le_u32(&payload[8..12]), le_u32(&payload[16..20]));
                if start != end || start.0 >= MAX_EXCEL_ROWS || start.1 >= MAX_EXCEL_COLUMNS {
                    return Err(malformed(Some(part), "invalid comment cell range"));
                }
                current = Some((start, author_id, None));
            }
            0x027d => {
                let Some((_, _, text_slot)) = &mut current else {
                    return Err(malformed(Some(part), "comment text outside comment"));
                };
                // MS-XLSB BrtCommentText requires fRichStr=1 and fExtStr=0.
                if text_slot.is_some()
                    || payload.first().is_none_or(|flags| flags & 0x03 != 0x01)
                    || frt_depth != 0
                {
                    return Err(malformed(Some(part), "invalid BrtCommentText"));
                }
                *text_slot = Some(validate_xlsb_rich_string(payload, part, options)?);
            }
            0x027c => {
                if state != State::Comments || !payload.is_empty() || frt_depth != 0 {
                    return Err(malformed(Some(part), "invalid BrtEndComment"));
                }
                let (cell, author_id, text) = current
                    .take()
                    .ok_or_else(|| malformed(Some(part), "comment end without start"))?;
                let text =
                    text.ok_or_else(|| malformed(Some(part), "XLSB comment has no text record"))?;
                if u64::try_from(text.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
                    return Err(limit("max_field_bytes", "XLSB comment is too large"));
                }
                let author = authors
                    .get(author_id)
                    .ok_or_else(|| malformed(Some(part), "invalid XLSB comment author id"))?;
                let cell_text = cell_name(cell.0, cell.1);
                checked_field_bytes(
                    options,
                    "rendered XLSB comment",
                    &[
                        8,
                        u64::try_from(cell_text.len()).unwrap_or(u64::MAX),
                        2,
                        u64::try_from(author.len()).unwrap_or(u64::MAX),
                        1,
                        2,
                        u64::try_from(text.len()).unwrap_or(u64::MAX),
                    ],
                )?;
                output.push(Annotation { cell, text, author: Some(author.clone()) });
            }
            0x027a
                if state == State::Comments
                    && current.is_none()
                    && frt_depth == 0
                    && payload.is_empty() =>
            {
                state = State::CommentsDone;
            }
            0x0275 if state == State::CommentsDone && payload.is_empty() => state = State::Done,
            0x0023 if !matches!(state, State::Start | State::Done) && payload.is_empty() => {
                frt_depth = frt_depth
                    .checked_add(1)
                    .ok_or_else(|| limit("max_nesting_depth", "XLSB comment FRT depth overflow"))?;
                if frt_depth > options.limits.max_nesting_depth {
                    return Err(limit("max_nesting_depth", "XLSB comment FRT is too deep"));
                }
            }
            0x0024 if frt_depth > 0 && payload.is_empty() => frt_depth -= 1,
            0x0c00
                if matches!(state, State::Begun | State::AuthorsDone | State::Comments)
                    && payload.len() == 16 =>
            {
                let uid: [u8; 16] = payload.try_into().expect("length checked");
                if !uids.insert(uid) {
                    return Err(malformed(Some(part), "duplicate XLSB comment UID"));
                }
            }
            _ => {
                return Err(malformed(
                    Some(part),
                    format!("invalid XLSB comment record/state 0x{typ:04x}"),
                ));
            }
        }
        offset = end;
        if authors.len() as u64 > options.limits.max_table_cells
            || output.len() as u64 > options.limits.max_table_cells
        {
            return Err(limit("max_table_cells", "too many XLSB comments"));
        }
    }
    if current.is_some() || state != State::Done || frt_depth != 0 {
        return Err(malformed(Some(part), "incomplete XLSB comments container"));
    }
    Ok(output)
}

#[allow(clippy::too_many_lines)]
pub(super) fn parse_comments(
    xml: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<Annotation>, ConversionError> {
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut authors = Vec::new();
    let mut output = Vec::new();
    let mut in_author = false;
    let mut author_text = String::new();
    let mut current: Option<((u32, u32), usize, String)> = None;
    let mut in_comment_text = false;
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, Event::Start(event))) => {
                require_spreadsheet_namespace(&namespace, part)?;
                match event.local_name().as_ref() {
                    b"author" => {
                        in_author = true;
                        author_text.clear();
                    }
                    b"comment" => {
                        let mut reference = None;
                        let mut author_id = None;
                        for attr in event.attributes().with_checks(false) {
                            let attr = attr.map_err(|error| {
                                malformed(Some(part), format!("invalid comment attribute: {error}"))
                            })?;
                            match attr.key.local_name().as_ref() {
                                b"ref" => reference = Some(decode_attr(&attr, part)?),
                                b"authorId" => author_id = Some(decode_attr(&attr, part)?),
                                _ => {}
                            }
                        }
                        let cell = parse_cell_ref(&reference.ok_or_else(|| {
                            malformed(Some(part), "comment reference is missing")
                        })?)?;
                        let author_id = author_id
                            .ok_or_else(|| malformed(Some(part), "comment author is missing"))?
                            .parse::<usize>()
                            .map_err(|_| malformed(Some(part), "invalid comment author id"))?;
                        current = Some((cell, author_id, String::new()));
                    }
                    b"text" if current.is_some() => in_comment_text = true,
                    _ => {}
                }
            }
            Ok((namespace, Event::Empty(event))) => {
                require_spreadsheet_namespace(&namespace, part)?;
                if event.local_name().as_ref() == b"comment" {
                    return Err(malformed(Some(part), "empty comment is missing fields"));
                }
            }
            Ok((_, Event::Text(text))) => {
                let value = text
                    .xml_content()
                    .map_err(|error| malformed(Some(part), format!("comment text: {error}")))?;
                if in_author {
                    checked_field_bytes(
                        options,
                        "comment author",
                        &[
                            u64::try_from(author_text.len()).unwrap_or(u64::MAX),
                            u64::try_from(value.len()).unwrap_or(u64::MAX),
                        ],
                    )?;
                    author_text.push_str(&value);
                } else if in_comment_text && let Some((_, _, text)) = &mut current {
                    checked_field_bytes(
                        options,
                        "comment text",
                        &[
                            u64::try_from(text.len()).unwrap_or(u64::MAX),
                            u64::try_from(value.len()).unwrap_or(u64::MAX),
                        ],
                    )?;
                    text.push_str(&value);
                }
            }
            Ok((_, Event::End(event))) => match event.local_name().as_ref() {
                b"author" => {
                    in_author = false;
                    checked_field_bytes(
                        options,
                        "comment author",
                        &[u64::try_from(author_text.len()).unwrap_or(u64::MAX)],
                    )?;
                    authors.push(std::mem::take(&mut author_text));
                }
                b"text" => in_comment_text = false,
                b"comment" => {
                    let (cell, author_id, text) = current
                        .take()
                        .ok_or_else(|| malformed(Some(part), "comment end without start"))?;
                    if u64::try_from(text.len()).unwrap_or(u64::MAX)
                        > options.limits.max_field_bytes
                    {
                        return Err(limit("max_field_bytes", "comment is too large"));
                    }
                    let author = authors
                        .get(author_id)
                        .ok_or_else(|| malformed(Some(part), "invalid comment author id"))?;
                    let cell_text = cell_name(cell.0, cell.1);
                    checked_field_bytes(
                        options,
                        "rendered comment",
                        &[
                            8,
                            u64::try_from(cell_text.len()).unwrap_or(u64::MAX),
                            2,
                            u64::try_from(author.len()).unwrap_or(u64::MAX),
                            1,
                            2,
                            u64::try_from(text.len()).unwrap_or(u64::MAX),
                        ],
                    )?;
                    output.push(Annotation { cell, text, author: Some(author.clone()) });
                }
                _ => {}
            },
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid comments XML: {error}")));
            }
            _ => {}
        }
    }
    if current.is_some() || in_author {
        return Err(malformed(Some(part), "truncated comments XML"));
    }
    Ok(output)
}
