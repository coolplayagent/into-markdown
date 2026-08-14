//! XLSB merge extraction at the Calamine adapter boundary.

use crate::workbook::error::malformed;
use crate::workbook::opc::package::read_entry;
use crate::workbook::xlsb::sheet::scan_xlsb_sheet;
use calamine::{Dimensions, SheetType};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use std::collections::BTreeMap;
use std::io::Cursor;

pub(in crate::workbook) fn extract_xlsb_merges(
    bytes: &[u8],
    sheets: &[calamine::Sheet],
    sheet_parts: &BTreeMap<String, String>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<BTreeMap<String, Vec<Dimensions>>, ConversionError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| malformed(None, format!("invalid XLSB ZIP: {error}")))?;
    let worksheet_names = sheets
        .iter()
        .filter(|sheet| sheet.typ == SheetType::WorkSheet)
        .map(|sheet| sheet.name.clone())
        .collect::<Vec<_>>();
    if sheet_parts
        .iter()
        .filter(|(_, part)| part.to_ascii_lowercase().starts_with("xl/worksheets/"))
        .count()
        != worksheet_names.len()
    {
        return Err(malformed(None, "XLSB worksheet metadata and parts disagree"));
    }
    let mut output = BTreeMap::new();
    for name in worksheet_names {
        let part = sheet_parts
            .get(&name)
            .ok_or_else(|| malformed(None, format!("missing XLSB part mapping for {name}")))?;
        let index = zip
            .index_for_name(part)
            .ok_or_else(|| malformed(Some(part), "mapped worksheet part is missing"))?;
        let data = read_entry(&mut zip, index, part)?;
        let scan = scan_xlsb_sheet(&data, part, None, options, context)?;
        output.insert(name, scan.merges);
    }
    Ok(output)
}
