use crate::workbook::error::{limit, malformed};
use crate::workbook::opc::relationships::is_spreadsheet_namespace;
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

pub(in crate::workbook) fn read_selected<R: BufRead>(
    input: R,
    required: &BTreeSet<u64>,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<BTreeMap<u64, String>, ConversionError> {
    if required.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut reader = quick_xml::reader::NsReader::from_reader(input);
    reader.config_mut().check_end_names = true;
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut output = BTreeMap::new();
    let mut index = 0_u64;
    let mut in_item = false;
    let mut item = String::new();
    loop {
        context.checkpoint()?;
        let (namespace, event) = reader.read_resolved_event_into(&mut buffer).map_err(|error| {
            malformed(Some(part), format!("invalid shared strings XML: {error}"))
        })?;
        let core = is_spreadsheet_namespace(&namespace);
        match event {
            Event::Start(element) if core && element.local_name().as_ref() == b"si" => {
                if in_item {
                    return Err(malformed(Some(part), "nested shared-string item"));
                }
                in_item = true;
                item.clear();
            }
            Event::Empty(element) if core && element.local_name().as_ref() == b"si" => {
                if required.contains(&index) {
                    output.insert(index, String::new());
                }
                index = index.saturating_add(1);
            }
            Event::Text(text) if in_item => {
                let decoded = text.xml_content().map_err(|error| {
                    malformed(Some(part), format!("invalid shared-string text: {error}"))
                })?;
                let next = item
                    .len()
                    .checked_add(decoded.len())
                    .ok_or_else(|| limit("max_field_bytes", "shared string length overflow"))?;
                if u64::try_from(next).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
                    return Err(limit("max_field_bytes", "shared string is too large"));
                }
                item.push_str(&decoded);
            }
            Event::CData(text) if in_item => {
                let decoded = text.decode().map_err(|error| {
                    malformed(Some(part), format!("invalid shared-string CDATA: {error}"))
                })?;
                if u64::try_from(item.len().saturating_add(decoded.len())).unwrap_or(u64::MAX)
                    > options.limits.max_field_bytes
                {
                    return Err(limit("max_field_bytes", "shared string is too large"));
                }
                item.push_str(&decoded);
            }
            Event::End(element) if core && element.local_name().as_ref() == b"si" => {
                if !in_item {
                    return Err(malformed(Some(part), "shared-string end without start"));
                }
                if required.contains(&index) {
                    output.insert(index, std::mem::take(&mut item));
                }
                in_item = false;
                index = index.saturating_add(1);
                if output.len() == required.len() {
                    break;
                }
            }
            Event::DocType(_) => return Err(malformed(Some(part), "DTD is forbidden")),
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if let Some(missing) = required.iter().find(|index| !output.contains_key(index)) {
        if options.error_policy == into_markdown_core::ErrorPolicy::BestEffort {
            for index in required {
                output.entry(*index).or_default();
            }
        } else {
            return Err(malformed(Some(part), format!("shared-string index {missing} is missing")));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::read_selected;
    use into_markdown_core::{
        ConversionOptions, ExecutionContext, ExecutionOptions, ResourceLimits,
    };
    use std::collections::BTreeSet;
    use std::io::Cursor;

    #[test]
    fn selects_rich_hot_and_empty_strings_in_one_pass() {
        let xml = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>cold</t></si><si><r><t>hot</t></r><r><t> value</t></r></si><si/></sst>"#;
        let required = BTreeSet::from([1, 2]);
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let values = read_selected(
            Cursor::new(xml),
            &required,
            "xl/sharedStrings.xml",
            &ConversionOptions::default(),
            &context,
        )
        .unwrap();
        assert_eq!(values.get(&1).map(String::as_str), Some("hot value"));
        assert_eq!(values.get(&2).map(String::as_str), Some(""));
    }
}
