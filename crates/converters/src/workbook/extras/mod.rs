//! Worksheet extras orchestration over authenticated OPC relationships.

mod comments;
mod drawing;
mod hyperlinks;
pub(super) mod metadata;
mod objects;

use crate::workbook::error::{malformed, warning};
use crate::workbook::extras::comments::{parse_binary_comments, parse_comments};
use crate::workbook::extras::hyperlinks::{
    parse_sheet_drawing_ids, parse_sheet_hyperlinks, resolve_binary_hyperlinks,
};
use crate::workbook::extras::metadata::{parse_cell_styles, parse_sheet_cell_metadata};
use crate::workbook::extras::objects::{extract_drawing_objects, require_package_part};
use crate::workbook::images::ExtractedAssets;
use crate::workbook::model::{SheetExtras, WorkbookKind};
use crate::workbook::opc::content_types::{ContentTypeMap, require_content_type};
use crate::workbook::opc::package::{has_extension, read_entry};
use crate::workbook::opc::relationships::{
    is_relationship_kind, parse_relationships, relationship_part,
};
use crate::workbook::schema::{
    DRAWING_CT, XLSB_COMMENTS_CT, XML_COMMENTS_CT, XML_STYLES_CT, XML_TABLE_CT,
};
use crate::workbook::xlsb::sheet::scan_xlsb_sheet;
use crate::workbook::xlsx::regions::{parse_table_part_ids, parse_table_range};
use into_markdown_core::{
    ConversionError, ConversionOptions, Diagnostic, ErrorPolicy, ExecutionContext, SourceLocator,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

pub(in crate::workbook) struct ExtractedSheetExtras {
    pub(in crate::workbook) sheets: BTreeMap<String, SheetExtras>,
    pub(in crate::workbook) table_ranges: BTreeMap<
        String,
        Vec<(crate::workbook::model::CellCoordinate, crate::workbook::model::CellCoordinate)>,
    >,
    pub(in crate::workbook) assets: ExtractedAssets,
    pub(in crate::workbook) diagnostics: Vec<Diagnostic>,
}

#[allow(clippy::too_many_lines)] // Relationship isolation is clearer as one bounded traversal.
pub(in crate::workbook) fn extract_sheet_extras(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    kind: WorkbookKind,
    sheet_parts: &BTreeMap<String, String>,
    package_parts: &BTreeSet<String>,
    content_types: &ContentTypeMap,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ExtractedSheetExtras, ConversionError> {
    let mut output = BTreeMap::new();
    let mut table_ranges = BTreeMap::new();
    let mut assets = ExtractedAssets::default();
    let mut diagnostics = Vec::new();
    let styles = if kind == WorkbookKind::Xml && package_parts.contains("xl/styles.xml") {
        require_content_type(content_types, "xl/styles.xml", &[XML_STYLES_CT])?;
        let index = zip
            .index_for_name("xl/styles.xml")
            .ok_or_else(|| malformed(Some("xl/styles.xml"), "styles part is missing"))?;
        let xml = read_entry(zip, index, "xl/styles.xml")?;
        parse_cell_styles(&xml, "xl/styles.xml", options, context)?
    } else {
        Vec::new()
    };
    for (sheet_name, sheet_part) in sheet_parts {
        context.checkpoint()?;
        let relationship_path = relationship_part(sheet_part);
        let relationships = if let Some(index) = zip.index_for_name(&relationship_path) {
            let bytes = read_entry(zip, index, &relationship_path)?;
            parse_relationships(&bytes, sheet_part, &relationship_path, options, context)?
        } else {
            BTreeMap::new()
        };
        let mut extras = SheetExtras::default();
        let mut sheet_table_ranges = Vec::new();
        let drawing_relationship_ids;
        if kind == WorkbookKind::Xml && has_extension(sheet_part, "xml") {
            let index = zip
                .index_for_name(sheet_part)
                .ok_or_else(|| malformed(Some(sheet_part), "worksheet part is missing"))?;
            let xml = read_entry(zip, index, sheet_part)?;
            for relationship_id in parse_table_part_ids(&xml, sheet_part, context)? {
                let relationship = relationships.get(&relationship_id).ok_or_else(|| {
                    malformed(
                        Some(sheet_part),
                        format!("missing table relationship {relationship_id}"),
                    )
                })?;
                if relationship.external || !is_relationship_kind(&relationship.kind, "table") {
                    return Err(malformed(Some(sheet_part), "invalid table relationship"));
                }
                require_package_part(package_parts, &relationship.target)?;
                require_content_type(content_types, &relationship.target, &[XML_TABLE_CT])?;
                let table_index = zip.index_for_name(&relationship.target).ok_or_else(|| {
                    malformed(Some(&relationship.target), "table part is missing")
                })?;
                let table_xml = read_entry(zip, table_index, &relationship.target)?;
                sheet_table_ranges.push(parse_table_range(
                    &table_xml,
                    &relationship.target,
                    context,
                )?);
            }
            let (hyperlinks, omitted_hyperlinks) =
                parse_sheet_hyperlinks(&xml, sheet_part, &relationships, options, context)?;
            extras.hyperlinks = hyperlinks;
            if omitted_hyperlinks > 0 {
                diagnostics.push(warning(
                    "spreadsheet.hyperlink.omitted",
                    format!("{omitted_hyperlinks} unsafe or incomplete hyperlink(s) were omitted"),
                    Some(SourceLocator {
                        sheet: Some(sheet_name.clone()),
                        part: Some(sheet_part.clone()),
                        ..SourceLocator::default()
                    }),
                ));
            }
            drawing_relationship_ids = parse_sheet_drawing_ids(&xml, sheet_part, context)?;
            let (cell_marks, hidden_rows, hidden_columns) =
                parse_sheet_cell_metadata(&xml, sheet_part, &styles, options, context)?;
            extras.cell_marks = cell_marks;
            extras.hidden_rows = hidden_rows;
            extras.hidden_columns = hidden_columns;
        } else if kind == WorkbookKind::Binary && has_extension(sheet_part, "bin") {
            let index = zip
                .index_for_name(sheet_part)
                .ok_or_else(|| malformed(Some(sheet_part), "binary worksheet part is missing"))?;
            let data = read_entry(zip, index, sheet_part)?;
            let scan = scan_xlsb_sheet(&data, sheet_part, None, options, context)?;
            extras.hidden_rows = scan.hidden_rows;
            extras.hidden_columns = scan.hidden_columns;
            extras.hyperlinks =
                resolve_binary_hyperlinks(scan.hyperlinks, sheet_part, &relationships, options)?;
            drawing_relationship_ids = scan.drawing_relationship_ids;
        } else {
            drawing_relationship_ids = Vec::new();
        }
        let drawing_relationship_ids =
            drawing_relationship_ids.into_iter().collect::<BTreeSet<_>>();
        let mut found_drawing_relationships = BTreeSet::new();
        let mut found_comments_relationship = false;
        for (relationship_id, relationship) in &relationships {
            if relationship.external {
                if !is_relationship_kind(&relationship.kind, "hyperlink") {
                    return Err(ConversionError::Unsupported {
                        detail: format!(
                            "external non-hyperlink relationship is forbidden ({sheet_part})"
                        ),
                    });
                }
                continue;
            }
            if is_relationship_kind(&relationship.kind, "comments") {
                if found_comments_relationship {
                    return Err(malformed(
                        Some(sheet_part),
                        "worksheet contains multiple comments relationships",
                    ));
                }
                found_comments_relationship = true;
                require_package_part(package_parts, &relationship.target)?;
                if has_extension(&relationship.target, "bin") {
                    require_content_type(content_types, &relationship.target, &[XLSB_COMMENTS_CT])?;
                    let index = zip.index_for_name(&relationship.target).ok_or_else(|| {
                        malformed(Some(&relationship.target), "comments part is missing")
                    })?;
                    let data = read_entry(zip, index, &relationship.target)?;
                    extras.annotations.extend(parse_binary_comments(
                        &data,
                        &relationship.target,
                        options,
                        context,
                    )?);
                    continue;
                }
                if !has_extension(&relationship.target, "xml") {
                    return Err(malformed(
                        Some(&relationship.target),
                        "comments encoding does not match its part extension",
                    ));
                }
                require_content_type(content_types, &relationship.target, &[XML_COMMENTS_CT])?;
                let index = zip.index_for_name(&relationship.target).ok_or_else(|| {
                    malformed(Some(&relationship.target), "comments part is missing")
                })?;
                let xml = read_entry(zip, index, &relationship.target)?;
                extras.annotations.extend(parse_comments(
                    &xml,
                    &relationship.target,
                    options,
                    context,
                )?);
            } else if is_relationship_kind(&relationship.kind, "drawing") {
                if !drawing_relationship_ids.contains(relationship_id) {
                    continue;
                }
                found_drawing_relationships.insert(relationship_id.clone());
                require_package_part(package_parts, &relationship.target)?;
                require_content_type(content_types, &relationship.target, &[DRAWING_CT])?;
                let omitted_images = extract_drawing_objects(
                    zip,
                    package_parts,
                    content_types,
                    relationship,
                    &mut extras,
                    &mut assets,
                    options,
                    context,
                )?;
                if omitted_images > 0 {
                    diagnostics.push(warning(
                        "spreadsheet.image.omitted",
                        format!("{omitted_images} unsupported or invalid image(s) were omitted"),
                        Some(SourceLocator {
                            sheet: Some(sheet_name.clone()),
                            part: Some(relationship.target.clone()),
                            ..SourceLocator::default()
                        }),
                    ));
                }
            } else if drawing_relationship_ids.contains(relationship_id) {
                return Err(malformed(
                    Some(sheet_part),
                    format!("drawing id {relationship_id} has the wrong relationship type"),
                ));
            }
        }
        if found_drawing_relationships != drawing_relationship_ids {
            if options.error_policy == ErrorPolicy::BestEffort {
                diagnostics.push(warning(
                    "spreadsheet.extension.omitted",
                    "worksheet drawing reference was omitted because its relationship is missing"
                        .into(),
                    Some(SourceLocator {
                        sheet: Some(sheet_name.clone()),
                        part: Some(sheet_part.clone()),
                        ..SourceLocator::default()
                    }),
                ));
            } else {
                return Err(malformed(
                    Some(sheet_part),
                    "referenced drawing relationship is missing",
                ));
            }
        }
        let mut comment_cells = BTreeSet::new();
        if extras.annotations.iter().any(|annotation| !comment_cells.insert(annotation.cell)) {
            return Err(malformed(Some(sheet_part), "worksheet contains duplicate cell comments"));
        }
        output.insert(sheet_name.clone(), extras);
        table_ranges.insert(sheet_name.clone(), sheet_table_ranges);
    }
    Ok(ExtractedSheetExtras { sheets: output, table_ranges, assets, diagnostics })
}

#[cfg(test)]
pub(super) fn parse_binary_comments_for_test(
    data: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<crate::workbook::model::Annotation>, ConversionError> {
    parse_binary_comments(data, part, options, context)
}

#[cfg(test)]
pub(super) fn safe_hyperlink_target_for_test(
    base: &str,
    location: Option<&str>,
    options: &ConversionOptions,
) -> Result<String, ConversionError> {
    hyperlinks::safe_hyperlink_target_for_test(base, location, options)
}

#[cfg(test)]
type CellMetadataForTest = (
    BTreeMap<crate::workbook::model::CellCoordinate, Vec<into_markdown_core::InlineMark>>,
    Vec<(u32, u32)>,
    Vec<(u32, u32)>,
);

#[cfg(test)]
pub(super) fn parse_sheet_cell_metadata_for_test(
    xml: &[u8],
    part: &str,
    styles: &[Vec<into_markdown_core::InlineMark>],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<CellMetadataForTest, ConversionError> {
    parse_sheet_cell_metadata(xml, part, styles, options, context)
}
