use crate::workbook::error::{limit, malformed};
use crate::workbook::model::{BinaryFormulaContext, WorkbookKind, WorkbookParts};
use crate::workbook::opc::content_types::{ContentTypeMap, require_content_type};
use crate::workbook::opc::package::{PackageEntry, read_entry};
use crate::workbook::opc::relationships::{
    Relationship, decode_attr, is_relationship_kind, parse_relationships, relationship_part,
};
use crate::workbook::schema::{
    ROOT_OFFICE_DOCUMENT, ROOT_OFFICE_DOCUMENT_STRICT, SPREADSHEET_BINARY_MAIN,
    SPREADSHEET_MACRO_MAIN, SPREADSHEET_MAIN, XLSB_CHARTSHEET_CT, XLSB_DIALOGSHEET_CT,
    XLSB_MACROSHEET_CT, XLSB_SHARED_STRINGS_CT, XLSB_STYLES_CT, XLSB_WORKSHEET_CT,
    XML_CHARTSHEET_CT, XML_DIALOGSHEET_CT, XML_MACROSHEET_CT, XML_SHARED_STRINGS_CT, XML_STYLES_CT,
    XML_WORKSHEET_CT,
};
use crate::workbook::xlsb::workbook::{parse_binary_workbook_sheets, scan_binary_workbook_surface};
use crate::workbook::xlsx::workbook::{parse_xml_workbook_sheets, scan_xml_workbook_surface};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::reader::Reader as XmlReader;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

pub(in crate::workbook) fn root_workbook_authority(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    entries: &BTreeMap<String, PackageEntry>,
    content_types: &ContentTypeMap,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(WorkbookKind, bool), ConversionError> {
    let root = entries
        .get("_rels/.rels")
        .ok_or_else(|| malformed(Some("_rels/.rels"), "package relationships are missing"))?;
    let xml = read_entry(zip, root.index, "_rels/.rels")?;
    let relationships = parse_relationships(&xml, "", "_rels/.rels", options, context)?;
    let mut office_documents = relationships.values().filter(|relationship| {
        relationship.kind == ROOT_OFFICE_DOCUMENT
            || relationship.kind == ROOT_OFFICE_DOCUMENT_STRICT
    });
    let relationship = office_documents
        .next()
        .ok_or_else(|| malformed(Some("_rels/.rels"), "officeDocument relationship is missing"))?;
    if office_documents.next().is_some() || relationship.external {
        return Err(malformed(
            Some("_rels/.rels"),
            "officeDocument relationship must be unique and internal",
        ));
    }
    if relationships.values().any(|candidate| {
        candidate.kind.ends_with("/officeDocument")
            && candidate.kind != ROOT_OFFICE_DOCUMENT
            && candidate.kind != ROOT_OFFICE_DOCUMENT_STRICT
    }) {
        return Err(malformed(Some("_rels/.rels"), "spoofed officeDocument relationship type"));
    }
    let actual_type = content_types
        .for_part(&relationship.target)
        .ok_or_else(|| malformed(Some(&relationship.target), "workbook has no content type"))?;
    let (kind, macro_present, expected_target) = match actual_type {
        SPREADSHEET_MAIN => (WorkbookKind::Xml, false, "xl/workbook.xml"),
        SPREADSHEET_MACRO_MAIN => (WorkbookKind::Xml, true, "xl/workbook.xml"),
        SPREADSHEET_BINARY_MAIN => (WorkbookKind::Binary, true, "xl/workbook.bin"),
        other => {
            return Err(ConversionError::Unsupported {
                detail: format!("unsupported workbook main content type {other}"),
            });
        }
    };
    if relationship.target != expected_target || !entries.contains_key(expected_target) {
        return Err(malformed(
            Some(&relationship.target),
            "officeDocument target does not match the canonical workbook consumed by the parser",
        ));
    }
    Ok((kind, macro_present))
}

pub(in crate::workbook) fn validate_xml_part(
    xml: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut reader = XmlReader::from_reader(xml);
    reader.config_mut().check_end_names = true;
    let mut depth = 0_u16;
    let mut events = 0_u64;
    loop {
        context.checkpoint()?;
        events = events.saturating_add(1);
        if events > u64::try_from(xml.len()).unwrap_or(u64::MAX).saturating_mul(4).max(1_024) {
            return Err(limit("max_decompressed_bytes", format!("excessive XML events in {part}")));
        }
        match reader.read_event() {
            Ok(Event::Start(_)) => {
                depth = depth.saturating_add(1);
                if depth > options.limits.max_nesting_depth {
                    return Err(limit("max_nesting_depth", format!("XML too deep in {part}")));
                }
            }
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(Event::DocType(_)) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok(Event::Eof) => break,
            Err(error) => return Err(malformed(Some(part), format!("invalid XML: {error}"))),
            _ => {}
        }
    }
    if depth != 0 {
        return Err(malformed(Some(part), "unbalanced XML elements"));
    }
    Ok(())
}

pub(in crate::workbook) fn reject_external_workbook_relationships(
    xml: &[u8],
    part: &str,
    _options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let mut reader = XmlReader::from_reader(xml);
    loop {
        context.checkpoint()?;
        match reader.read_event() {
            Ok(Event::Start(event) | Event::Empty(event))
                if event.local_name().as_ref() == b"Relationship" =>
            {
                let mut kind = String::new();
                for attr in event.attributes().with_checks(false) {
                    let attr = attr
                        .map_err(|error| malformed(Some(part), format!("relationship: {error}")))?;
                    if attr.key.local_name().as_ref() == b"Type" {
                        kind = decode_attr(&attr, part)?;
                    }
                }
                let lower = kind.to_ascii_lowercase();
                if lower.ends_with("/externallink") || lower.ends_with("/externallinkpath") {
                    return Err(ConversionError::Unsupported {
                        detail: format!("external workbook relationship is forbidden ({part})"),
                    });
                }
            }
            Ok(Event::DocType(_)) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(malformed(Some(part), format!("invalid relationships: {error}")));
            }
            _ => {}
        }
    }
    Ok(())
}

pub(in crate::workbook) fn workbook_sheet_parts(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    kind: WorkbookKind,
    package_parts: &BTreeSet<String>,
    content_types: &ContentTypeMap,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<WorkbookParts, ConversionError> {
    let workbook_part = match kind {
        WorkbookKind::Xml => "xl/workbook.xml",
        WorkbookKind::Binary => "xl/workbook.bin",
    };
    if !package_parts.contains(workbook_part) {
        return Err(malformed(Some(workbook_part), "declared workbook part is missing"));
    }
    let relationship_part = relationship_part(workbook_part);
    let relationships = if let Some(index) = zip.index_for_name(&relationship_part) {
        let bytes = read_entry(zip, index, &relationship_part)?;
        parse_relationships(&bytes, workbook_part, &relationship_part, options, context)?
    } else {
        return Err(malformed(Some(&relationship_part), "workbook relationships are missing"));
    };
    validate_fixed_parser_parts(
        &relationships,
        kind,
        package_parts,
        content_types,
        &relationship_part,
    )?;
    let workbook_index = zip
        .index_for_name(workbook_part)
        .ok_or_else(|| malformed(Some(workbook_part), "workbook part is missing"))?;
    let workbook = read_entry(zip, workbook_index, workbook_part)?;
    let (names_to_ids, inventory, binary_formula_context) = match kind {
        WorkbookKind::Xml => (
            parse_xml_workbook_sheets(&workbook, options, context)?,
            scan_xml_workbook_surface(&workbook, options, context)?,
            BinaryFormulaContext::default(),
        ),
        WorkbookKind::Binary => {
            let (inventory, formula_context) =
                scan_binary_workbook_surface(&workbook, options, context)?;
            (parse_binary_workbook_sheets(&workbook, options, context)?, inventory, formula_context)
        }
    };
    let mut output = BTreeMap::new();
    let mut targets = BTreeSet::new();
    for (name, relationship_id) in names_to_ids {
        let relationship = relationships.get(&relationship_id).ok_or_else(|| {
            malformed(
                Some(workbook_part),
                format!("sheet {name} references missing relationship {relationship_id}"),
            )
        })?;
        if relationship.external {
            return Err(malformed(
                Some(&relationship_part),
                format!("sheet {name} has an invalid relationship"),
            ));
        }
        if !is_relationship_kind(&relationship.kind, "worksheet") {
            if let Some(content_type) = unsupported_sheet_content_type(kind, &relationship.kind) {
                require_content_type(content_types, &relationship.target, &[content_type])?;
                return Err(ConversionError::Unsupported {
                    detail: format!(
                        "non-worksheet sheet relationship is unsupported and was not opened ({name})"
                    ),
                });
            }
            return Err(malformed(
                Some(&relationship_part),
                format!("sheet {name} has a spoofed relationship type"),
            ));
        }
        if !package_parts.contains(&relationship.target) {
            return Err(malformed(
                Some(&relationship.target),
                format!("sheet part for {name} is missing"),
            ));
        }
        require_content_type(
            content_types,
            &relationship.target,
            &[match kind {
                WorkbookKind::Xml => XML_WORKSHEET_CT,
                WorkbookKind::Binary => XLSB_WORKSHEET_CT,
            }],
        )?;
        if !targets.insert(relationship.target.clone()) {
            return Err(malformed(
                Some(&relationship_part),
                "multiple logical sheets reference the same physical worksheet",
            ));
        }
        if output.insert(name.clone(), relationship.target.clone()).is_some() {
            return Err(malformed(Some(workbook_part), format!("duplicate sheet name {name}")));
        }
    }
    Ok(WorkbookParts { sheets: output, inventory, binary_formula_context })
}

fn unsupported_sheet_content_type(
    kind: WorkbookKind,
    relationship_kind: &str,
) -> Option<&'static str> {
    [
        ("chartsheet", XML_CHARTSHEET_CT, XLSB_CHARTSHEET_CT),
        ("dialogsheet", XML_DIALOGSHEET_CT, XLSB_DIALOGSHEET_CT),
        ("macrosheet", XML_MACROSHEET_CT, XLSB_MACROSHEET_CT),
    ]
    .into_iter()
    .find(|(suffix, _, _)| is_relationship_kind(relationship_kind, suffix))
    .map(|(_, xml, binary)| match kind {
        WorkbookKind::Xml => xml,
        WorkbookKind::Binary => binary,
    })
}

fn validate_fixed_parser_parts(
    relationships: &BTreeMap<String, Relationship>,
    kind: WorkbookKind,
    package_parts: &BTreeSet<String>,
    content_types: &ContentTypeMap,
    relationship_part: &str,
) -> Result<(), ConversionError> {
    let authorities = match kind {
        WorkbookKind::Xml => [
            ("styles", "xl/styles.xml", XML_STYLES_CT),
            ("sharedStrings", "xl/sharedStrings.xml", XML_SHARED_STRINGS_CT),
        ],
        WorkbookKind::Binary => [
            ("styles", "xl/styles.bin", XLSB_STYLES_CT),
            ("sharedStrings", "xl/sharedStrings.bin", XLSB_SHARED_STRINGS_CT),
        ],
    };
    for (suffix, fixed_target, content_type) in authorities {
        let matching = relationships
            .values()
            .filter(|relationship| is_relationship_kind(&relationship.kind, suffix))
            .collect::<Vec<_>>();
        if relationships.values().any(|relationship| {
            relationship.kind.ends_with(&format!("/{suffix}"))
                && !is_relationship_kind(&relationship.kind, suffix)
        }) {
            return Err(malformed(
                Some(relationship_part),
                format!("spoofed {suffix} relationship type"),
            ));
        }
        if package_parts.contains(fixed_target) {
            if matching.len() != 1 || matching[0].external || matching[0].target != fixed_target {
                return Err(malformed(
                    Some(relationship_part),
                    format!(
                        "the fixed {suffix} part consumed by the parser lacks unique relationship authority"
                    ),
                ));
            }
            require_content_type(content_types, fixed_target, &[content_type])?;
        } else if !matching.is_empty() {
            return Err(malformed(
                Some(relationship_part),
                format!("{suffix} relationship targets an unavailable parser part"),
            ));
        }
    }
    Ok(())
}
