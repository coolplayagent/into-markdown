use crate::docx::supported_image;
use crate::workbook::budget::checked_field_bytes;
use crate::workbook::error::malformed;
use crate::workbook::extras::drawing::{
    DrawingReference, DrawingReferenceKind, parse_drawing_references,
};
use crate::workbook::images::{ExtractedAssets, ImageValidation};
use crate::workbook::model::{ChartTitle, ImageAnchor, SheetExtras};
use crate::workbook::opc::content_types::{ContentTypeMap, require_content_type};
use crate::workbook::opc::package::{has_extension, read_entry};
use crate::workbook::opc::relationships::{
    Relationship, is_relationship_kind, parse_relationships, relationship_part,
};
use crate::workbook::schema::{CHART_CT, PACKAGE_REL_CT};
use into_markdown_core::{ConversionError, ConversionOptions, ErrorPolicy, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::reader::Reader as XmlReader;
use std::collections::BTreeSet;
use std::io::Cursor;

#[allow(clippy::too_many_arguments)]
fn append_chart_title(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    package_parts: &BTreeSet<String>,
    relationship: &Relationship,
    reference: &DrawingReference,
    drawing_part: &str,
    extras: &mut SheetExtras,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    require_package_part(package_parts, &relationship.target)?;
    if !has_extension(&relationship.target, "xml") {
        return Err(ConversionError::Unsupported {
            detail: format!("binary chart title is unsupported ({})", relationship.target),
        });
    }
    let index = zip
        .index_for_name(&relationship.target)
        .ok_or_else(|| malformed(Some(&relationship.target), "chart part is missing"))?;
    let xml = read_entry(zip, index, &relationship.target)?;
    if let Some(title) = parse_chart_title(&xml, &relationship.target, options, context)? {
        checked_field_bytes(
            options,
            "rendered chart title",
            &[7, u64::try_from(title.len()).unwrap_or(u64::MAX)],
        )?;
        for (label, value) in [
            ("chart anchor part", drawing_part),
            ("chart target part", relationship.target.as_str()),
            ("chart relationship id", reference.relationship_id.as_str()),
        ] {
            checked_field_bytes(options, label, &[u64::try_from(value.len()).unwrap_or(u64::MAX)])?;
        }
        extras.chart_titles.push(ChartTitle {
            cell: reference.start,
            end: reference.end,
            title,
            part: drawing_part.to_owned(),
            target: relationship.target.clone(),
            relationship_id: reference.relationship_id.clone(),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) fn extract_drawing_objects(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    package_parts: &BTreeSet<String>,
    content_types: &ContentTypeMap,
    drawing_relationship: &Relationship,
    extras: &mut SheetExtras,
    assets: &mut ExtractedAssets,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<u64, ConversionError> {
    let drawing_part = &drawing_relationship.target;
    let index = zip
        .index_for_name(drawing_part)
        .ok_or_else(|| malformed(Some(drawing_part), "drawing part is missing"))?;
    let drawing_xml = read_entry(zip, index, drawing_part)?;
    let references = parse_drawing_references(&drawing_xml, drawing_part, options, context)?;
    if references.is_empty() {
        return Ok(0);
    }
    let drawing_rels_path = relationship_part(drawing_part);
    require_package_part(package_parts, &drawing_rels_path)?;
    require_content_type(content_types, &drawing_rels_path, &[PACKAGE_REL_CT])?;
    let rels_index = zip
        .index_for_name(&drawing_rels_path)
        .ok_or_else(|| malformed(Some(&drawing_rels_path), "drawing relationships are missing"))?;
    let rels_xml = read_entry(zip, rels_index, &drawing_rels_path)?;
    let relationships =
        parse_relationships(&rels_xml, drawing_part, &drawing_rels_path, options, context)?;
    let mut omitted_images = 0_u64;
    for reference in references {
        context.checkpoint()?;
        let relationship = relationships.get(&reference.relationship_id).ok_or_else(|| {
            malformed(
                Some(drawing_part),
                format!("missing drawing relationship {}", reference.relationship_id),
            )
        })?;
        if relationship.external {
            return Err(ConversionError::Unsupported {
                detail: format!("external drawing object is forbidden ({drawing_rels_path})"),
            });
        }
        require_package_part(package_parts, &relationship.target)?;
        match reference.kind {
            DrawingReferenceKind::Chart => {
                if !is_relationship_kind(&relationship.kind, "chart") {
                    return Err(malformed(
                        Some(drawing_part),
                        "chart anchor relationship has the wrong type",
                    ));
                }
                require_content_type(content_types, &relationship.target, &[CHART_CT])?;
                append_chart_title(
                    zip,
                    package_parts,
                    relationship,
                    &reference,
                    drawing_part,
                    extras,
                    options,
                    context,
                )?;
            }
            DrawingReferenceKind::Image => {
                if !is_relationship_kind(&relationship.kind, "image") {
                    return Err(malformed(
                        Some(drawing_part),
                        "image anchor relationship has the wrong type",
                    ));
                }
                let declared = content_types.for_part(&relationship.target).ok_or_else(|| {
                    malformed(Some(&relationship.target), "image target has no content type")
                })?;
                let image = match supported_image(&relationship.target, declared) {
                    Ok(image) => image,
                    Err(_)
                        if options.error_policy == ErrorPolicy::BestEffort
                            && known_unsupported_image(&relationship.target, declared) =>
                    {
                        omitted_images = omitted_images.saturating_add(1);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let media_index = zip.index_for_name(&relationship.target).ok_or_else(|| {
                    malformed(Some(&relationship.target), "image target is missing")
                })?;
                let bytes = read_entry(zip, media_index, &relationship.target)?;
                for (label, value) in [
                    ("image anchor part", drawing_part.as_str()),
                    ("image target part", relationship.target.as_str()),
                    ("image relationship id", reference.relationship_id.as_str()),
                ] {
                    checked_field_bytes(
                        options,
                        label,
                        &[u64::try_from(value.len()).unwrap_or(u64::MAX)],
                    )?;
                }
                if let Some(alt) = &reference.alt {
                    checked_field_bytes(
                        options,
                        "image alternative text",
                        &[u64::try_from(alt.len()).unwrap_or(u64::MAX)],
                    )?;
                }
                assets.account_decode_working_set(image, &bytes, &relationship.target)?;
                let (asset, asset_index) =
                    assets.add(&relationship.target, image.media_type(), bytes, options)?;
                assets.validations.push(ImageValidation {
                    image,
                    asset_index,
                    part: relationship.target.clone(),
                });
                extras.images.push(ImageAnchor {
                    cell: reference.start,
                    end: reference.end,
                    asset,
                    alt: reference.alt.clone(),
                    part: drawing_part.clone(),
                    target: relationship.target.clone(),
                    relationship_id: reference.relationship_id.clone(),
                });
            }
        }
    }
    Ok(omitted_images)
}

fn known_unsupported_image(part: &str, declared: &str) -> bool {
    let extension =
        std::path::Path::new(part).extension().and_then(|value| value.to_str()).unwrap_or_default();
    matches!(
        (declared, extension.to_ascii_lowercase().as_str()),
        ("image/x-emf" | "image/emf", "emf") | ("image/x-wmf" | "image/wmf", "wmf")
    )
}

fn parse_chart_title(
    xml: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Option<String>, ConversionError> {
    let mut reader = XmlReader::from_reader(xml);
    let mut title_depth = 0_u16;
    let mut capture_text = false;
    let mut title = String::new();
    loop {
        context.checkpoint()?;
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                if event.local_name().as_ref() == b"title" {
                    title_depth = title_depth.saturating_add(1);
                } else if title_depth > 0 && event.local_name().as_ref() == b"t" {
                    capture_text = true;
                }
            }
            Ok(Event::Text(text)) if capture_text => {
                let value = text
                    .xml_content()
                    .map_err(|error| malformed(Some(part), format!("chart title: {error}")))?;
                checked_field_bytes(
                    options,
                    "chart title",
                    &[
                        u64::try_from(title.len()).unwrap_or(u64::MAX),
                        u64::from(!title.is_empty()),
                        u64::try_from(value.len()).unwrap_or(u64::MAX),
                    ],
                )?;
                if !title.is_empty() {
                    title.push(' ');
                }
                title.push_str(&value);
            }
            Ok(Event::End(event)) => {
                if event.local_name().as_ref() == b"t" {
                    capture_text = false;
                } else if event.local_name().as_ref() == b"title" && title_depth > 0 {
                    title_depth -= 1;
                    if title_depth == 0 && !title.is_empty() {
                        return Ok(Some(title));
                    }
                }
            }
            Ok(Event::DocType(_)) => return Err(malformed(Some(part), "DTD is forbidden")),
            Ok(Event::Eof) => break,
            Err(error) => return Err(malformed(Some(part), format!("invalid chart XML: {error}"))),
            _ => {}
        }
    }
    Ok(None)
}

pub(super) fn require_package_part(
    package_parts: &BTreeSet<String>,
    part: &str,
) -> Result<(), ConversionError> {
    if package_parts.contains(part) {
        Ok(())
    } else {
        Err(malformed(Some(part), "related package part is missing"))
    }
}
