//! Direct PAPX row definitions (MS-DOC 2.4.3, 2.9.174-175 and 2.9.321).

use super::{Piece, le16, le32};
use crate::legacy_office::budget::{LegacyBudget, malformed};
use into_markdown_core::ConversionError;

#[derive(Clone, Debug)]
pub(super) struct Row {
    pub start: usize,
    pub end: usize,
    pub edges: Vec<i16>,
    pub flags: Vec<u16>,
}

pub(super) fn read_rows(
    word: &[u8],
    table: &[u8],
    pieces: &[Piece],
    budget: &mut LegacyBudget<'_>,
) -> Result<Vec<Row>, ConversionError> {
    let offset = le32(word, 0x102, "WordDocument/FIB")? as usize;
    let length = le32(word, 0x106, "WordDocument/FIB")? as usize;
    if length == 0 {
        return Ok(Vec::new());
    }
    let plc = table
        .get(offset..offset.saturating_add(length))
        .ok_or_else(|| malformed("WordDocument/PAPX", "paragraph index is truncated"))?;
    if plc.len() < 4 || (plc.len() - 4) % 8 != 0 {
        return Err(malformed("WordDocument/PAPX", "invalid paragraph index shape"));
    }
    let count = (plc.len() - 4) / 8;
    let mut runs = Vec::new();
    for index in 0..count {
        budget.work(512, "WordDocument/PAPX")?;
        let page = le32(plc, (count + 1) * 4 + index * 4, "WordDocument/PAPX")? as usize;
        let offset = page.saturating_mul(512);
        let fkp = word
            .get(offset..offset.saturating_add(512))
            .ok_or_else(|| malformed("WordDocument/PAPX", "paragraph page is truncated"))?;
        budget.work(
            (pieces.len() as u64).saturating_mul(u64::from(fkp[511])),
            "WordDocument/PAPX pieces",
        )?;
        read_page(fkp, pieces, &mut runs)?;
    }
    runs.sort_by_key(|run| run.0);
    let mut start = None;
    let mut rows = Vec::new();
    for (first, end, in_table, definition) in runs {
        if !in_table {
            start = None;
            continue;
        }
        let first = *start.get_or_insert(first);
        if let Some((edges, flags)) = definition {
            rows.push(Row { start: first, end, edges, flags });
            start = Some(end);
        }
    }
    Ok(rows)
}

type Definition = (Vec<i16>, Vec<u16>);
type Run = (usize, usize, bool, Option<Definition>);

fn read_page(fkp: &[u8], pieces: &[Piece], runs: &mut Vec<Run>) -> Result<(), ConversionError> {
    let count = usize::from(fkp[511]);
    if count > 29 {
        return Err(malformed("WordDocument/PAPX", "paragraph page has too many runs"));
    }
    for index in 0..count {
        let start_fc = le32(fkp, index * 4, "WordDocument/PAPX")?;
        let end_fc = le32(fkp, index * 4 + 4, "WordDocument/PAPX")?;
        let offset = usize::from(fkp[(count + 1) * 4 + index * 13]) * 2;
        let (in_table, definition) = if offset == 0 {
            (false, None)
        } else {
            if offset < (count + 1) * 4 + count * 13 {
                return Err(malformed(
                    "WordDocument/PAPX",
                    "properties overlap the paragraph index",
                ));
            }
            let cb = usize::from(fkp[offset]);
            let (start, length) = if cb == 0 {
                (offset + 2, usize::from(fkp[offset + 1]) * 2)
            } else {
                (offset + 1, cb * 2 - 1)
            };
            let properties = fkp[..511]
                .get(start..start.saturating_add(length))
                .and_then(|bytes| bytes.get(2..))
                .ok_or_else(|| {
                    malformed("WordDocument/PAPX", "paragraph properties are truncated")
                })?;
            properties_row(properties)
        };
        for piece in pieces {
            let step = if piece.compressed { 1 } else { 2 };
            let piece_end = piece
                .file_offset
                .saturating_add((piece.end_cp - piece.start_cp).saturating_mul(step));
            let first = start_fc.max(piece.file_offset);
            let end = end_fc.min(piece_end);
            if first < end {
                let start_cp = piece.start_cp + (first - piece.file_offset) / step;
                let end_cp = piece.start_cp + (end - piece.file_offset) / step;
                runs.push((start_cp as usize, end_cp as usize, in_table, definition.clone()));
            }
        }
    }
    Ok(())
}

fn properties_row(mut bytes: &[u8]) -> (bool, Option<Definition>) {
    let mut in_table = false;
    let mut row_mark = false;
    let mut definition = None;
    while bytes.len() >= 2 {
        let opcode = u16::from_le_bytes([bytes[0], bytes[1]]);
        bytes = &bytes[2..];
        let length = match opcode >> 13 {
            0 | 1 => 1,
            2 | 4 | 5 => 2,
            3 => 4,
            7 => 3,
            _ if opcode == 0xd608 => {
                let Some(raw) = bytes.get(..2) else { break };
                usize::from(u16::from_le_bytes([raw[0], raw[1]])) + 1
            }
            _ => {
                let Some(length) = bytes.first() else { break };
                // Extended tab changes have a distinct variable layout; do not scan
                // their opaque payload for apparent table opcodes.
                if opcode == 0xc615 && *length == 255 {
                    break;
                }
                usize::from(*length) + 1
            }
        };
        let Some(operand) = bytes.get(..length) else { break };
        match opcode {
            0x2416 => in_table = operand[0] != 0,
            0x2417 => row_mark = operand[0] != 0,
            0xd608 => definition = table_definition(operand),
            _ => {}
        }
        bytes = &bytes[length..];
    }
    (in_table, if row_mark { definition } else { None })
}

fn table_definition(bytes: &[u8]) -> Option<Definition> {
    let count = usize::from(*bytes.get(2)?);
    if count == 0 || count > 63 {
        return None;
    }
    let edge_bytes = bytes.get(3..3 + (count + 1) * 2)?;
    let edges = edge_bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    if edges.windows(2).any(|pair| pair[0] >= pair[1]) {
        return None;
    }
    let offset = 3 + (count + 1) * 2;
    let flags = (0..count)
        .map(|index| le16(bytes, offset + index * 20, "WordDocument/TC80").unwrap_or(0))
        .collect();
    Some((edges, flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_properties() -> Vec<u8> {
        let mut bytes = vec![0x16, 0x24, 1, 0x17, 0x24, 1, 8, 0xd6, 48, 0, 2];
        for edge in [0i16, 100, 200] {
            bytes.extend_from_slice(&edge.to_le_bytes());
        }
        for flags in [0x60u16, 0] {
            bytes.extend_from_slice(&flags.to_le_bytes());
            bytes.extend_from_slice(&[0; 18]);
        }
        bytes
    }

    #[test]
    fn reads_direct_papx_row_and_cell_merge_flags() {
        let mut page = [0u8; 512];
        page[..4].copy_from_slice(&100u32.to_le_bytes());
        page[4..8].copy_from_slice(&105u32.to_le_bytes());
        page[8] = 100;
        page[200] = 30;
        let properties = row_properties();
        page[203..203 + properties.len()].copy_from_slice(&properties);
        page[511] = 1;
        let pieces = [Piece { start_cp: 0, end_cp: 5, file_offset: 100, compressed: true }];
        let mut runs = Vec::new();
        read_page(&page, &pieces, &mut runs).unwrap();
        assert_eq!(runs, [(0, 5, true, Some((vec![0, 100, 200], vec![0x60, 0])))]);
        page[8] = 1;
        assert!(read_page(&page, &pieces, &mut Vec::new()).is_err());
    }

    #[test]
    fn opaque_or_truncated_operands_are_not_scanned_for_table_signatures() {
        let properties = row_properties();
        let mut opaque = vec![1, 0xc6, u8::try_from(properties.len()).unwrap()];
        opaque.extend_from_slice(&properties);
        assert_eq!(properties_row(&opaque), (false, None));
        assert!(properties_row(&properties[..properties.len() - 1]).1.is_none());
    }
}
