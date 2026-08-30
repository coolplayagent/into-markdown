use super::reader::{Result, Tokens};
use crate::workbook::cell::cell_name;

#[derive(Default)]
pub(in crate::legacy_office::xls) struct References {
    names: Vec<String>,
    maximum_name_bytes: usize,
    books: Vec<Option<u16>>,
    xtis: Vec<(u16, u16, u16)>,
    invalid: bool,
}

impl References {
    pub(in crate::legacy_office::xls) fn add_sheet(&mut self, name: &str) {
        let quoted_bytes = name.len() + name.bytes().filter(|byte| *byte == b'\'').count();
        self.maximum_name_bytes = self.maximum_name_bytes.max(quoted_bytes);
        self.names.push(name.to_owned());
    }

    pub(super) fn expansion_bytes(&self, token_bytes: usize) -> u64 {
        // The shortest 3D reference occupies seven token bytes. Account for two
        // long sheet names per range, quoting scratch, retained atoms and output.
        u64::try_from(token_bytes / 7)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::try_from(self.maximum_name_bytes).unwrap_or(u64::MAX))
            .saturating_mul(6)
    }

    pub(in crate::legacy_office::xls) fn record(&mut self, kind: u16, body: &[u8]) {
        let mut input = Tokens::new(body);
        if self.read_record(kind, &mut input).is_err() {
            self.invalid = true;
        }
    }

    fn read_record(&mut self, kind: u16, input: &mut Tokens<'_>) -> Result<()> {
        match kind {
            0x01ae => {
                let count = input.word()?;
                let kind = input.word()?;
                // Only the exact self-referencing SupBook form authenticates local sheets.
                self.books.push((kind == 0x0401 && input.remaining() == 0).then_some(count));
            }
            0x0017 => {
                let count = usize::from(input.word()?);
                if input.remaining() != count * 6 {
                    return Err("invalid-externsheet");
                }
                for _ in 0..count {
                    self.xtis.push((input.word()?, input.word()?, input.word()?));
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn sheet_prefix(&self, ixti: u16) -> Result<String> {
        if self.invalid {
            return Err("invalid-reference-metadata");
        }
        let (book, first, last) = *self.xtis.get(usize::from(ixti)).ok_or("unknown-xti")?;
        let book = self.books.get(usize::from(book)).ok_or("unknown-supbook")?;
        let count = book.ok_or("external-reference")?;
        if usize::from(count) != self.names.len() || first > last {
            return Err("invalid-local-reference");
        }
        let first_name = self.names.get(usize::from(first)).ok_or("invalid-local-reference")?;
        let last_name = self.names.get(usize::from(last)).ok_or("invalid-local-reference")?;
        let name =
            if first == last { first_name.clone() } else { format!("{first_name}:{last_name}") };
        Ok(format!("'{}'!", name.replace('\'', "''")))
    }
}

pub(super) fn reference(input: &mut Tokens<'_>, biff8: bool) -> Result<String> {
    let row = input.word()?;
    let column = if biff8 { input.word()? } else { u16::from(input.byte()?) };
    coordinate(row, column, biff8)
}

pub(super) fn area(input: &mut Tokens<'_>, biff8: bool) -> Result<String> {
    let first_row = input.word()?;
    let last_row = input.word()?;
    let first_column = if biff8 { input.word()? } else { u16::from(input.byte()?) };
    let last_column = if biff8 { input.word()? } else { u16::from(input.byte()?) };
    let first = coordinate(first_row, first_column, biff8)?;
    let last = coordinate(last_row, last_column, biff8)?;
    Ok(format!("{first}:{last}"))
}

fn coordinate(row: u16, column: u16, biff8: bool) -> Result<String> {
    // MS-XLS RgceLoc: flags describe absolute/relative display, not offsets.
    // RgceLocRel (PtgRefN / shared formulas) is deliberately not handled here.
    let (row, column, row_relative, column_relative) = if biff8 {
        (row, column & 0x3fff, column & 0x8000 != 0, column & 0x4000 != 0)
    } else {
        (row & 0x3fff, column, row & 0x8000 != 0, row & 0x4000 != 0)
    };
    if column > 255 {
        return Err("invalid-reference-column");
    }
    let name = cell_name(u32::from(row), u32::from(column));
    let split =
        name.find(|character: char| character.is_ascii_digit()).ok_or("invalid-reference")?;
    Ok(format!(
        "{}{}{}{}",
        if column_relative { "" } else { "$" },
        &name[..split],
        if row_relative { "" } else { "$" },
        &name[split..]
    ))
}
