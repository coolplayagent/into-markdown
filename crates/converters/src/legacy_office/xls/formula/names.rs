//! Preserve authenticated local name identifiers, never evaluate their definitions.

use super::reader::{Result, Tokens};
use crate::workbook::cell::parse_cell_ref;
use std::collections::BTreeMap;

struct Name {
    text: String,
    key: String,
    scope: u16,
    unsupported: Option<&'static str>,
}

#[derive(Default)]
pub(super) struct Names {
    entries: Vec<Option<Name>>,
    counts: BTreeMap<(u16, String), usize>,
    incomplete: bool,
    pub(super) maximum_bytes: usize,
}

impl Names {
    pub(super) fn record(&mut self, body: &[u8]) {
        let entry = parse(body).ok();
        if let Some(name) = &entry {
            self.maximum_bytes = self.maximum_bytes.max(name.text.len());
            *self.counts.entry((name.scope, name.key.clone())).or_default() += 1;
        } else {
            self.incomplete = true;
        }
        // Retain every ordinal, including unsupported built-ins and malformed records.
        self.entries.push(entry);
    }

    pub(super) fn resolve(&self, index: u32, sheet: usize, sheets: &[String]) -> Result<String> {
        if self.incomplete {
            return Err("invalid-defined-name-metadata");
        }
        let index = index.checked_sub(1).ok_or("unknown-defined-name")?;
        let index = usize::try_from(index).map_err(|_| "unknown-defined-name")?;
        let name =
            self.entries.get(index).and_then(Option::as_ref).ok_or("unknown-defined-name")?;
        if let Some(reason) = name.unsupported {
            return Err(reason);
        }
        if self.counts.get(&(name.scope, name.key.clone())) != Some(&1) {
            return Err("ambiguous-defined-name");
        }
        let current = u16::try_from(sheet + 1).map_err(|_| "invalid-defined-name-scope")?;
        if name.scope == 0 {
            if self.counts.contains_key(&(current, name.key.clone())) {
                return Err("shadowed-global-name");
            }
        } else {
            let owner =
                sheets.get(usize::from(name.scope - 1)).ok_or("invalid-defined-name-scope")?;
            if name.scope != current {
                return Ok(format!("'{}'!{}", owner.replace('\'', "''"), name.text));
            }
        }
        Ok(name.text.clone())
    }
}

fn parse(body: &[u8]) -> Result<Name> {
    let mut input = Tokens::new(body);
    let flags = input.word()?;
    input.byte()?; // Macro keyboard shortcut, never used.
    let characters = usize::from(input.byte()?);
    let formula_bytes = usize::from(input.word()?);
    input.take(2)?; // Reserved, ignored by MS-XLS.
    let scope = input.word()?;
    input.take(4)?;
    let text = input.text(characters, true)?;
    input.take(formula_bytes)?; // Authenticate only the span; do not parse or execute rgce.
    let unsupported = if flags & 0x000e != 0 {
        Some("macro-defined-name")
    } else if flags & 0x0020 != 0 {
        Some("builtin-defined-name")
    } else if !identifier(&text) {
        Some("unsupported-defined-name-identifier")
    } else {
        None
    };
    Ok(Name { key: text.to_lowercase(), text, scope, unsupported })
}

fn identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else { return false };
    (first.is_alphabetic() || matches!(first, '_' | '\\'))
        && characters
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.' | '\\'))
        && !matches!(name.to_ascii_uppercase().as_str(), "R" | "C" | "TRUE" | "FALSE")
        // BIFF8 names may look like addresses outside its 256-column/65536-row grid
        // (for example On2); applying the larger XLSX grid would lose legal names.
        && !parse_cell_ref(name).is_ok_and(|(row, column)| {
            u16::try_from(row).is_ok() && u8::try_from(column).is_ok()
        })
        && !r1c1_reference(name)
}

fn r1c1_reference(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.strip_prefix('R').and_then(|value| value.split_once('C')).is_some_and(|(row, column)| {
        row.bytes().all(|value| value.is_ascii_digit())
            && column.bytes().all(|value| value.is_ascii_digit())
    })
}
