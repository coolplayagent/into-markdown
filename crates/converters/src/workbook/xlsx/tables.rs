use crate::workbook::error::{limit, malformed};
use crate::workbook::model::WorkbookInventory;
use crate::workbook::opc::relationships::{
    decode_attr, decode_xml_reference, is_spreadsheet_namespace, require_spreadsheet_namespace,
};
use crate::workbook::xlsx::formulas::{DisplayProfile, builtin_number_kind, detect_number_kind};
use into_markdown_core::{ConversionError, ConversionOptions, ErrorPolicy, ExecutionContext};
use quick_xml::events::Event;
use std::collections::{BTreeMap, BTreeSet};

#[allow(clippy::too_many_lines)] // Root, declaration, and actual-entry state is intentionally one pass.
#[cfg(test)]
pub(in crate::workbook) fn scan_xml_shared_strings(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<WorkbookInventory, ConversionError> {
    scan_xml_shared_strings_selected(xml, &BTreeSet::new(), options, context)
        .map(|(inventory, _)| inventory)
}

pub(in crate::workbook) fn scan_xml_shared_strings_selected(
    xml: &[u8],
    required: &BTreeSet<u64>,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(WorkbookInventory, BTreeMap<u64, String>), ConversionError> {
    let part = "xl/sharedStrings.xml";
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut state = SharedStringScan::default();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                require_spreadsheet_namespace(&namespace, part)?;
                state.start(&event, is_empty, required, options, part)?;
            }
            Ok((_, Event::Text(text))) if state.string_depth.is_some() => {
                let decoded = state
                    .selected_value
                    .is_some()
                    .then(|| text.xml_content())
                    .transpose()
                    .map_err(|error| {
                        malformed(Some(part), format!("invalid shared-string text: {error}"))
                    })?;
                state.text(text.iter().len(), decoded.as_deref(), options)?;
            }
            Ok((_, Event::CData(text))) if state.string_depth.is_some() => {
                let decoded =
                    state.selected_value.is_some().then(|| text.decode()).transpose().map_err(
                        |error| {
                            malformed(Some(part), format!("invalid shared-string CDATA: {error}"))
                        },
                    )?;
                state.text(text.iter().len(), decoded.as_deref(), options)?;
            }
            Ok((_, Event::GeneralRef(reference))) => {
                let decoded = decode_xml_reference(reference.as_ref(), part)?;
                if state.string_depth.is_some() {
                    let mut utf8 = [0_u8; 4];
                    let decoded = state
                        .selected_value
                        .is_some()
                        .then(|| decoded.encode_utf8(&mut utf8) as &str);
                    state.text(reference.iter().len().saturating_add(2), decoded, options)?;
                }
            }
            Ok((namespace, Event::End(event))) => {
                require_spreadsheet_namespace(&namespace, part)?;
                state.end(event.local_name().as_ref(), part)?;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => return Err(malformed(Some(part), format!("invalid SST XML: {error}"))),
            _ => {}
        }
        if state.inventory.shared_strings > options.limits.max_table_cells {
            return Err(limit("max_table_cells", "too many shared strings"));
        }
        if state.inventory.shared_string_bytes > options.limits.max_decompressed_bytes {
            return Err(limit("max_decompressed_bytes", "shared string text is too large"));
        }
    }
    state.validate(part)?;
    if let Some(missing) = required.iter().find(|index| !state.selected.contains_key(index)) {
        if options.error_policy == ErrorPolicy::BestEffort {
            for index in required {
                state.selected.entry(*index).or_default();
            }
        } else {
            return Err(malformed(Some(part), format!("shared-string index {missing} is missing")));
        }
    }
    Ok((state.inventory, state.selected))
}

#[derive(Default)]
struct SharedStringScan {
    inventory: WorkbookInventory,
    declared_unique: Option<u64>,
    declared_total: Option<u64>,
    saw_root: bool,
    ended_root: bool,
    depth: u16,
    string_depth: Option<u16>,
    string_index: Option<u64>,
    current_string_bytes: u64,
    selected_value: Option<String>,
    selected: BTreeMap<u64, String>,
}

impl SharedStringScan {
    fn start(
        &mut self,
        event: &quick_xml::events::BytesStart<'_>,
        is_empty: bool,
        required: &BTreeSet<u64>,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        match event.local_name().as_ref() {
            b"sst" => self.start_root(event, is_empty, options, part)?,
            b"si" => self.start_item(is_empty, required, part)?,
            _ if !self.saw_root || self.ended_root || self.depth == 0 => {
                return Err(malformed(Some(part), "invalid shared-string hierarchy"));
            }
            _ => {}
        }
        if !is_empty {
            self.depth = self
                .depth
                .checked_add(1)
                .ok_or_else(|| limit("max_nesting_depth", "shared-string depth overflow"))?;
            if self.depth > options.limits.max_nesting_depth {
                return Err(limit("max_nesting_depth", "shared strings are too deep"));
            }
        }
        Ok(())
    }

    fn start_root(
        &mut self,
        event: &quick_xml::events::BytesStart<'_>,
        is_empty: bool,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.saw_root || self.depth != 0 {
            return Err(malformed(Some(part), "invalid shared-string root"));
        }
        self.saw_root = true;
        let mut attributes = BTreeSet::new();
        for attr in event.attributes().with_checks(false) {
            let attr =
                attr.map_err(|error| malformed(Some(part), format!("sst attribute: {error}")))?;
            if !attributes.insert(attr.key.as_ref().to_vec()) {
                return Err(malformed(Some(part), "duplicate sst attribute"));
            }
            let target = match attr.key.local_name().as_ref() {
                b"uniqueCount" => Some((&mut self.declared_unique, true)),
                b"count" => Some((&mut self.declared_total, false)),
                _ => None,
            };
            if let Some((target, unique)) = target {
                let value = decode_attr(&attr, part)?
                    .parse::<u64>()
                    .map_err(|_| malformed(Some(part), "invalid sst count"))?;
                if unique && value > options.limits.max_table_cells {
                    return Err(limit("max_table_cells", "shared string declaration is too large"));
                }
                *target = Some(value);
            }
        }
        if is_empty {
            return Err(malformed(Some(part), "empty shared-string root"));
        }
        Ok(())
    }

    fn start_item(
        &mut self,
        is_empty: bool,
        required: &BTreeSet<u64>,
        part: &str,
    ) -> Result<(), ConversionError> {
        if !self.saw_root || self.ended_root || self.depth != 1 || self.string_depth.is_some() {
            return Err(malformed(Some(part), "invalid shared-string item state"));
        }
        let index = self.inventory.shared_strings;
        self.inventory.shared_strings = self.inventory.shared_strings.saturating_add(1);
        self.current_string_bytes = 0;
        if is_empty {
            if required.contains(&index) {
                self.selected.insert(index, String::new());
            }
        } else {
            self.string_depth = Some(self.depth);
            self.string_index = Some(index);
            self.selected_value = required.contains(&index).then(String::new);
        }
        Ok(())
    }

    fn text(
        &mut self,
        encoded_len: usize,
        decoded: Option<&str>,
        options: &ConversionOptions,
    ) -> Result<(), ConversionError> {
        let encoded_len = u64::try_from(encoded_len).unwrap_or(u64::MAX);
        self.inventory.shared_string_bytes =
            self.inventory.shared_string_bytes.saturating_add(encoded_len);
        self.current_string_bytes = self.current_string_bytes.saturating_add(encoded_len);
        if let Some((value, decoded)) = self.selected_value.as_mut().zip(decoded) {
            if u64::try_from(value.len().saturating_add(decoded.len())).unwrap_or(u64::MAX)
                > options.limits.max_field_bytes
            {
                return Err(limit("max_field_bytes", "shared string is too large"));
            }
            value.push_str(decoded);
        }
        Ok(())
    }

    fn end(&mut self, local: &[u8], part: &str) -> Result<(), ConversionError> {
        if self.depth == 0 {
            return Err(malformed(Some(part), "unbalanced shared-string element"));
        }
        match local {
            b"si" => {
                if self.string_depth != Some(self.depth - 1) {
                    return Err(malformed(Some(part), "invalid shared-string item end"));
                }
                self.string_depth = None;
                if let Some(value) = self.selected_value.take() {
                    let index = self
                        .string_index
                        .ok_or_else(|| malformed(Some(part), "shared-string index is missing"))?;
                    self.selected.insert(index, value);
                }
                self.string_index = None;
                self.inventory.max_shared_string_bytes =
                    self.inventory.max_shared_string_bytes.max(self.current_string_bytes);
            }
            b"sst" => {
                if self.depth != 1 || self.string_depth.is_some() || self.ended_root {
                    return Err(malformed(Some(part), "invalid shared-string root end"));
                }
                self.ended_root = true;
            }
            _ => {}
        }
        self.depth -= 1;
        Ok(())
    }

    fn validate(&self, part: &str) -> Result<(), ConversionError> {
        if !self.saw_root || !self.ended_root || self.depth != 0 || self.string_depth.is_some() {
            return Err(malformed(Some(part), "incomplete shared-string document"));
        }
        if self.declared_unique.is_some_and(|value| value != self.inventory.shared_strings) {
            return Err(malformed(Some(part), "uniqueCount disagrees with shared string entries"));
        }
        if self.declared_total.is_some_and(|total| {
            total < self.declared_unique.unwrap_or(self.inventory.shared_strings)
        }) {
            return Err(malformed(Some(part), "sst count is smaller than uniqueCount"));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_lines)] // Count and actual-entry checks share one parser state.
#[cfg(test)]
pub(in crate::workbook) fn scan_xml_style_counts(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<WorkbookInventory, ConversionError> {
    scan_xml_styles(xml, options, context).map(|(inventory, _)| inventory)
}

pub(in crate::workbook) fn scan_xml_styles(
    xml: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(WorkbookInventory, DisplayProfile), ConversionError> {
    let part = "xl/styles.xml";
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut state = StyleScan::default();
    loop {
        context.checkpoint()?;
        match reader.read_resolved_event() {
            Ok((namespace, raw_event @ (Event::Start(_) | Event::Empty(_)))) => {
                let is_empty = matches!(raw_event, Event::Empty(_));
                let (Event::Start(event) | Event::Empty(event)) = raw_event else { unreachable!() };
                state.start(&namespace, &event, is_empty, options, part)?;
            }
            Ok((namespace, Event::End(event))) => {
                state.end(&namespace, event.local_name().as_ref(), part)?;
            }
            Ok((_, Event::DocType(_))) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok((_, Event::Eof)) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid styles XML: {error}")));
            }
            _ => {}
        }
        if state.inventory.styles > options.limits.max_table_cells
            || state.inventory.fonts > options.limits.max_table_cells
            || state.inventory.number_formats > options.limits.max_table_cells
        {
            return Err(limit("max_table_cells", "too many workbook style records"));
        }
        if state.inventory.style_format_bytes > options.limits.max_decompressed_bytes {
            return Err(limit("max_decompressed_bytes", "number formats are too large"));
        }
    }
    state.validate(part)?;
    Ok((state.inventory, state.display))
}

#[derive(Default)]
struct StyleScan {
    depth: u16,
    foreign_depth: u16,
    document_state: StyleDocumentState,
    seen_collections: BTreeSet<&'static str>,
    cell_xfs_depth: Option<u16>,
    num_formats_depth: Option<u16>,
    fonts_depth: Option<u16>,
    declared_xfs: Option<u64>,
    declared_num_formats: Option<u64>,
    declared_fonts: Option<u64>,
    inventory: WorkbookInventory,
    custom_formats: BTreeMap<u64, crate::workbook::xlsx::formulas::NumberKind>,
    display: DisplayProfile,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum StyleDocumentState {
    #[default]
    BeforeRoot,
    InRoot,
    Ended,
}

impl StyleScan {
    fn start(
        &mut self,
        namespace: &quick_xml::name::ResolveResult<'_>,
        event: &quick_xml::events::BytesStart<'_>,
        is_empty: bool,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.foreign_depth > 0 || !is_spreadsheet_namespace(namespace) {
            return self.start_extension(is_empty, options, part);
        }
        require_spreadsheet_namespace(namespace, part)?;
        match event.local_name().as_ref() {
            b"styleSheet" => self.start_root(is_empty, part)?,
            _ if self.document_state != StyleDocumentState::InRoot || self.depth == 0 => {
                return Err(malformed(Some(part), "invalid styleSheet hierarchy"));
            }
            b"cellXfs" => self.start_cell_xfs(event, is_empty, options, part)?,
            b"xf" if self.cell_xfs_depth.is_some() => self.record_xf(event, part)?,
            b"fonts" => self.start_fonts(event, is_empty, options, part)?,
            b"font" if self.fonts_depth.is_some() => self.record_font(part)?,
            b"numFmts" => self.start_num_formats(event, is_empty, options, part)?,
            b"numFmt" if self.num_formats_depth.is_some() => {
                self.record_number_format(event, options, part)?;
            }
            _ => {}
        }
        if !is_empty {
            self.increment_depth(options, part)?;
        }
        Ok(())
    }

    fn start_extension(
        &mut self,
        is_empty: bool,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.document_state != StyleDocumentState::InRoot || self.depth == 0 {
            return Err(malformed(Some(part), "extension is outside styleSheet"));
        }
        if !is_empty {
            self.increment_depth(options, part)?;
            self.foreign_depth = self
                .foreign_depth
                .checked_add(1)
                .ok_or_else(|| limit("max_nesting_depth", "styleSheet extension depth overflow"))?;
        }
        Ok(())
    }

    fn increment_depth(
        &mut self,
        options: &ConversionOptions,
        _part: &str,
    ) -> Result<(), ConversionError> {
        self.depth = self
            .depth
            .checked_add(1)
            .ok_or_else(|| limit("max_nesting_depth", "styleSheet depth overflow"))?;
        if self.depth > options.limits.max_nesting_depth {
            return Err(limit("max_nesting_depth", "styleSheet is too deep"));
        }
        Ok(())
    }

    fn start_root(&mut self, is_empty: bool, part: &str) -> Result<(), ConversionError> {
        if self.document_state != StyleDocumentState::BeforeRoot || self.depth != 0 || is_empty {
            return Err(malformed(Some(part), "invalid styleSheet root"));
        }
        self.document_state = StyleDocumentState::InRoot;
        Ok(())
    }

    fn start_cell_xfs(
        &mut self,
        event: &quick_xml::events::BytesStart<'_>,
        is_empty: bool,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.depth != 1
            || !self.seen_collections.insert("cellXfs")
            || self.cell_xfs_depth.is_some()
        {
            return Err(malformed(Some(part), "duplicate or nested cellXfs"));
        }
        self.declared_xfs = style_collection_count(event, part, options)?;
        if !is_empty {
            self.cell_xfs_depth = Some(self.depth);
        }
        Ok(())
    }

    fn record_xf(
        &mut self,
        event: &quick_xml::events::BytesStart<'_>,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.depth != 2 {
            return Err(malformed(Some(part), "nested cellXfs entry"));
        }
        let mut number_format = None;
        for attr in event.attributes().with_checks(false) {
            let attr =
                attr.map_err(|error| malformed(Some(part), format!("xf attribute: {error}")))?;
            if attr.key.local_name().as_ref() == b"numFmtId" {
                number_format = decode_attr(&attr, part)?.parse::<u64>().ok();
            }
        }
        if let Some(kind) = number_format.and_then(|id| {
            builtin_number_kind(id).or_else(|| self.custom_formats.get(&id).copied())
        }) {
            self.display.styles.insert(self.inventory.styles, kind);
        }
        self.inventory.styles = self.inventory.styles.saturating_add(1);
        Ok(())
    }

    fn start_fonts(
        &mut self,
        event: &quick_xml::events::BytesStart<'_>,
        is_empty: bool,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.depth != 1 || !self.seen_collections.insert("fonts") || self.fonts_depth.is_some() {
            return Err(malformed(Some(part), "duplicate or nested fonts"));
        }
        self.declared_fonts = style_collection_count(event, part, options)?;
        if !is_empty {
            self.fonts_depth = Some(self.depth);
        }
        Ok(())
    }

    fn record_font(&mut self, part: &str) -> Result<(), ConversionError> {
        if self.depth != 2 {
            return Err(malformed(Some(part), "nested font entry"));
        }
        self.inventory.fonts = self.inventory.fonts.saturating_add(1);
        Ok(())
    }

    fn start_num_formats(
        &mut self,
        event: &quick_xml::events::BytesStart<'_>,
        is_empty: bool,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.depth != 1
            || !self.seen_collections.insert("numFmts")
            || self.num_formats_depth.is_some()
        {
            return Err(malformed(Some(part), "duplicate or nested numFmts"));
        }
        self.declared_num_formats = style_collection_count(event, part, options)?;
        if !is_empty {
            self.num_formats_depth = Some(self.depth);
        }
        Ok(())
    }

    fn record_number_format(
        &mut self,
        event: &quick_xml::events::BytesStart<'_>,
        options: &ConversionOptions,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.depth != 2 {
            return Err(malformed(Some(part), "nested number-format entry"));
        }
        self.inventory.number_formats = self.inventory.number_formats.saturating_add(1);
        let mut id = None;
        let mut code = None;
        for attr in event.attributes().with_checks(false) {
            let attr =
                attr.map_err(|error| malformed(Some(part), format!("numFmt attribute: {error}")))?;
            if attr.key.local_name().as_ref() == b"numFmtId" {
                id = decode_attr(&attr, part)?.parse::<u64>().ok();
            } else if attr.key.local_name().as_ref() == b"formatCode" {
                let value = decode_attr(&attr, part)?;
                let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
                if length > options.limits.max_field_bytes {
                    return Err(limit("max_field_bytes", "number format is too large"));
                }
                self.inventory.style_format_bytes =
                    self.inventory.style_format_bytes.saturating_add(length);
                code = Some(value);
            }
        }
        if let Some((id, kind)) = id.zip(code.as_deref().and_then(detect_number_kind)) {
            self.custom_formats.insert(id, kind);
        }
        Ok(())
    }

    fn end(
        &mut self,
        namespace: &quick_xml::name::ResolveResult<'_>,
        local: &[u8],
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.foreign_depth > 0 {
            if self.depth == 0 {
                return Err(malformed(Some(part), "unbalanced styleSheet extension"));
            }
            self.depth -= 1;
            self.foreign_depth -= 1;
            return Ok(());
        }
        require_spreadsheet_namespace(namespace, part)?;
        if self.depth == 0 {
            return Err(malformed(Some(part), "unbalanced styleSheet element"));
        }
        match local {
            b"cellXfs" => close_collection(&mut self.cell_xfs_depth, self.depth, part, "cellXfs")?,
            b"numFmts" => {
                close_collection(&mut self.num_formats_depth, self.depth, part, "numFmts")?;
            }
            b"fonts" => close_collection(&mut self.fonts_depth, self.depth, part, "fonts")?,
            b"styleSheet" => self.end_root(part)?,
            _ => {}
        }
        self.depth -= 1;
        Ok(())
    }

    fn end_root(&mut self, part: &str) -> Result<(), ConversionError> {
        if self.depth != 1
            || self.cell_xfs_depth.is_some()
            || self.num_formats_depth.is_some()
            || self.fonts_depth.is_some()
            || self.document_state != StyleDocumentState::InRoot
        {
            return Err(malformed(Some(part), "invalid styleSheet root end"));
        }
        self.document_state = StyleDocumentState::Ended;
        Ok(())
    }

    fn validate(&self, part: &str) -> Result<(), ConversionError> {
        if self.document_state != StyleDocumentState::Ended
            || self.depth != 0
            || self.cell_xfs_depth.is_some()
            || self.num_formats_depth.is_some()
            || self.fonts_depth.is_some()
        {
            return Err(malformed(Some(part), "incomplete styleSheet document"));
        }
        if self.declared_xfs.is_some_and(|value| value != self.inventory.styles)
            || self.declared_num_formats.is_some_and(|value| value != self.inventory.number_formats)
            || self.declared_fonts.is_some_and(|value| value != self.inventory.fonts)
        {
            return Err(malformed(Some(part), "cellXfs count disagrees with style entries"));
        }
        Ok(())
    }
}

fn close_collection(
    open_depth: &mut Option<u16>,
    depth: u16,
    part: &str,
    name: &str,
) -> Result<(), ConversionError> {
    if *open_depth != Some(depth - 1) {
        return Err(malformed(Some(part), format!("invalid {name} end")));
    }
    *open_depth = None;
    Ok(())
}

fn style_collection_count(
    event: &quick_xml::events::BytesStart<'_>,
    part: &str,
    options: &ConversionOptions,
) -> Result<Option<u64>, ConversionError> {
    let mut declared = None;
    let mut attributes = BTreeSet::new();
    for attr in event.attributes().with_checks(false) {
        let attr =
            attr.map_err(|error| malformed(Some(part), format!("style attribute: {error}")))?;
        if !attributes.insert(attr.key.as_ref().to_vec()) {
            return Err(malformed(Some(part), "duplicate style collection attribute"));
        }
        if attr.key.local_name().as_ref() == b"count" {
            let count = decode_attr(&attr, part)?
                .parse::<u64>()
                .map_err(|_| malformed(Some(part), "invalid style collection count"))?;
            if count > options.limits.max_table_cells {
                return Err(limit("max_table_cells", "style declaration is too large"));
            }
            declared = Some(count);
        }
    }
    Ok(declared)
}

#[cfg(test)]
mod tests {
    use super::{scan_xml_shared_strings, scan_xml_shared_strings_selected};
    use into_markdown_core::{
        ConversionOptions, ExecutionContext, ExecutionOptions, ResourceLimits,
    };
    use std::collections::BTreeSet;

    #[test]
    fn declared_reference_count_does_not_allocate_shared_strings() {
        let xml = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="18446744073709551615" uniqueCount="1"><si><t>used</t></si></sst>"#;
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let inventory =
            scan_xml_shared_strings(xml, &ConversionOptions::default(), &context).unwrap();
        assert_eq!(inventory.shared_strings, 1);
    }

    #[test]
    fn selected_production_shared_strings_restore_xml_references() {
        let xml = br#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>R&amp;D &quot;Q&quot; &#x4E2D;</t></si></sst>"#;
        let context = ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default());
        let (_, selected) = scan_xml_shared_strings_selected(
            xml,
            &BTreeSet::from([0]),
            &ConversionOptions::default(),
            &context,
        )
        .unwrap();
        assert_eq!(selected.get(&0).map(String::as_str), Some("R&D \"Q\" 中"));
    }
}
