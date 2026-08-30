use super::support::{NS, context_with, package};
use crate::odf::convert_odf;
use into_markdown_core::{
    ConversionError, ConversionOptions, ErrorPolicy, InputFormat, ResourceLimits,
};
use into_markdown_render_markdown::render;

fn convert_policy(
    bytes: &[u8],
    format: InputFormat,
    policy: ErrorPolicy,
    limits: ResourceLimits,
) -> Result<into_markdown_core::ConverterOutput, ConversionError> {
    convert_odf(
        bytes,
        format,
        &ConversionOptions {
            error_policy: policy,
            limits: limits.clone(),
            ..ConversionOptions::default()
        },
        &context_with(limits),
    )
}

#[test]
fn optional_scripts_and_revision_history_do_not_replace_static_body() {
    let content = format!(
        "<office:document-content {NS} xmlns:s='urn:oasis:names:tc:opendocument:xmlns:script:1.0'><office:scripts><office:script s:language='Basic'>DO_NOT_EXPORT</office:script></office:scripts><office:body><office:text><text:tracked-changes><text:changed-region><text:deletion><text:p>deleted history</text:p></text:deletion></text:changed-region></text:tracked-changes><text:p>before<text:change-start text:change-id='x'/>inserted<text:change-end text:change-id='x'/>after</text:p><draw:frame><office:event-listeners><s:event-listener s:language='Basic' xlink:href='vnd.sun.star.script:DO_NOT_EXPORT'/></office:event-listeners><draw:text-box><text:p>Static shape</text:p></draw:text-box></draw:frame></office:text></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let output = convert_policy(
        &bytes,
        InputFormat::Odt,
        ErrorPolicy::BestEffort,
        ResourceLimits::default(),
    )
    .unwrap();
    let md = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(md.contains("beforeinsertedafter") && md.contains("Static shape"));
    assert!(!md.contains("DO_NOT_EXPORT") && !md.contains("deleted history"));
    assert!(output.assets.is_empty());
    for code in ["odf.scriptsOmitted", "odf.trackedChanges"] {
        assert!(output.diagnostics.iter().any(|d| d.code == code));
    }
    assert!(
        convert_policy(&bytes, InputFormat::Odt, ErrorPolicy::Strict, ResourceLimits::default())
            .is_err()
    );
    let dtd = content
        .replace("DO_NOT_EXPORT</office:script>", "<!DOCTYPE bad>DO_NOT_EXPORT</office:script>");
    assert!(
        convert_policy(
            &package(InputFormat::Odt, &dtd, &[]),
            InputFormat::Odt,
            ErrorPolicy::BestEffort,
            ResourceLimits::default()
        )
        .is_err()
    );
}

#[test]
fn forms_animation_and_svg_keep_static_slide_with_auditable_omissions() {
    let content = format!(
        "<office:document-content {NS} xmlns:form='urn:oasis:names:tc:opendocument:xmlns:form:1.0' xmlns:anim='urn:oasis:names:tc:opendocument:xmlns:animation:1.0'><office:body><office:presentation><draw:page><office:forms><form:form form:name='search'/></office:forms><draw:frame><draw:text-box><text:p>Static slide</text:p></draw:text-box></draw:frame><draw:frame><draw:image xlink:href='Pictures/vector.svg'/></draw:frame><anim:par><anim:seq/></anim:par></draw:page></office:presentation></office:body></office:document-content>"
    );
    let bytes =
        package(InputFormat::Odp, &content, &[("Pictures/vector.svg", "image/svg+xml", b"<svg/>")]);
    let output = convert_policy(
        &bytes,
        InputFormat::Odp,
        ErrorPolicy::BestEffort,
        ResourceLimits::default(),
    )
    .unwrap();
    let md = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(md.contains("Static slide") && md.contains("Image omitted:"));
    assert!(output.assets.is_empty());
    for code in ["odf.formsOmitted", "odf.animationOmitted", "odf.imageOmitted"] {
        assert!(output.diagnostics.iter().any(|d| d.code == code));
    }
    assert!(
        convert_policy(&bytes, InputFormat::Odp, ErrorPolicy::Strict, ResourceLimits::default())
            .is_err()
    );
}

#[test]
fn million_empty_tail_rows_are_sparse_but_real_repeats_remain_limited() {
    let make = |tail: &str| {
        format!(
            "<office:document-content {NS}><office:body><office:spreadsheet><table:table table:name='S'><table:table-row><table:table-cell><text:p>actual value</text:p></table:table-cell></table:table-row><table:table-row table:number-rows-repeated='1048575'>{tail}</table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"
        )
    };
    let limits = ResourceLimits { max_table_rows: 1, ..ResourceLimits::default() };
    let bytes = package(
        InputFormat::Ods,
        &make("<table:table-cell table:number-columns-repeated='16384'/>"),
        &[],
    );
    for policy in [ErrorPolicy::BestEffort, ErrorPolicy::Strict] {
        let output = convert_policy(&bytes, InputFormat::Ods, policy, limits.clone()).unwrap();
        output.document.validate().unwrap();
        let md = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(md.contains("actual value") && md.len() < 100);
        assert!(output.diagnostics.iter().any(|d| d.code == "odf.emptyRowPadding"));
    }
    for cell in [
        "<table:table-cell office:value-type='float' office:value='1'/>",
        "<table:table-cell table:formula='of:=1' office:value-type='float' office:value='1'/>",
        "<table:table-cell table:number-rows-spanned='2'/>",
        "<table:covered-table-cell/>",
    ] {
        assert!(matches!(
            convert_policy(
                &package(InputFormat::Ods, &make(cell), &[]),
                InputFormat::Ods,
                ErrorPolicy::BestEffort,
                limits.clone()
            ),
            Err(ConversionError::ResourceLimit { limit: "max_table_rows", .. })
        ));
    }
    let interior = make("<table:table-cell/>").replace("</table:table>", "<table:table-row><table:table-cell><text:p>later</text:p></table:table-cell></table:table-row></table:table>");
    assert!(matches!(
        convert_policy(
            &package(InputFormat::Ods, &interior, &[]),
            InputFormat::Ods,
            ErrorPolicy::BestEffort,
            limits
        ),
        Err(ConversionError::ResourceLimit { limit: "max_table_rows", .. })
    ));
}

#[test]
fn producer_formula_is_namespace_bound_inert_and_retains_cached_value() {
    let content = format!(
        "<office:document-content {NS} xmlns:old='http://schemas.microsoft.com/office/excel/formula'><office:body><office:spreadsheet><table:table><table:table-row><table:table-cell office:value-type='float' office:value='190.944' table:formula='old:=C5*95.472'/></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Ods, &content, &[]);
    let output = convert_policy(
        &bytes,
        InputFormat::Ods,
        ErrorPolicy::BestEffort,
        ResourceLimits::default(),
    )
    .unwrap();
    let md = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(md.contains(r"190\.944") && !md.contains("openformula"), "{md}");
    let into_markdown_core::Block::Sheet { blocks, .. } = &output.document.blocks[0].block else {
        panic!()
    };
    let into_markdown_core::Block::Table { rows, .. } = &blocks[0].block else { panic!() };
    assert!(rows[0].cells[0].blocks.iter().any(|block| matches!(&block.block, into_markdown_core::Block::Code { language: None, text } if text == "old:=C5*95.472")));
    assert!(output.diagnostics.iter().any(|d| d.code == "odf.cachedProducerFormula"));
    assert!(
        convert_policy(&bytes, InputFormat::Ods, ErrorPolicy::Strict, ResourceLimits::default())
            .is_err()
    );
    let wrong =
        content.replace("http://schemas.microsoft.com/office/excel/formula", "urn:untrusted");
    assert!(
        convert_policy(
            &package(InputFormat::Ods, &wrong, &[]),
            InputFormat::Ods,
            ErrorPolicy::BestEffort,
            ResourceLimits::default()
        )
        .is_err()
    );
}

fn rewrite_archive(bytes: &[u8], omit: &[&str], compressed_mimetype: bool) -> Vec<u8> {
    use std::io::{Cursor, Read, Write};
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let mut names: Vec<_> = archive.file_names().map(str::to_owned).collect();
    if compressed_mimetype {
        names.sort();
    }
    for name in names {
        if omit.contains(&name.as_str()) {
            continue;
        }
        let mut value = Vec::new();
        archive.by_name(&name).unwrap().read_to_end(&mut value).unwrap();
        let options = if name == "mimetype" && !compressed_mimetype {
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
        } else {
            zip::write::SimpleFileOptions::default()
        };
        writer.start_file(name, options).unwrap();
        writer.write_all(&value).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn optional_part_roles_do_not_relax_consumed_part_requirements() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>actual body</text:p></office:text></office:body></office:document-content>"
    );
    let original = package(
        InputFormat::Odt,
        &content,
        &[
            ("CustomUI/", "application/vnd.sun.xml.ui.configuration", b""),
            ("CustomUI/keys.xml", "", b""),
            ("preview.png", "image/png", b""),
            ("meta.xml", "text/xml", b""),
        ],
    );
    let bytes =
        rewrite_archive(&original, &["CustomUI/keys.xml", "preview.png", "meta.xml"], false);
    let output = convert_policy(
        &bytes,
        InputFormat::Odt,
        ErrorPolicy::BestEffort,
        ResourceLimits::default(),
    )
    .unwrap();
    assert_eq!(
        output.diagnostics.iter().filter(|d| d.code == "odf.optionalPartMissing").count(),
        3
    );
    assert!(
        convert_policy(&bytes, InputFormat::Odt, ErrorPolicy::Strict, ResourceLimits::default())
            .is_err()
    );
    let missing_body = rewrite_archive(&bytes, &["content.xml"], false);
    assert!(
        convert_policy(
            &missing_body,
            InputFormat::Odt,
            ErrorPolicy::BestEffort,
            ResourceLimits::default()
        )
        .is_err()
    );
    let with_reference = content.replace(
        "</office:text>",
        "<draw:frame><draw:image xlink:href='preview.png'/></draw:frame></office:text>",
    );
    let missing_image = rewrite_archive(
        &package(InputFormat::Odt, &with_reference, &[("preview.png", "image/png", b"")]),
        &["preview.png"],
        false,
    );
    assert!(
        convert_policy(
            &missing_image,
            InputFormat::Odt,
            ErrorPolicy::BestEffort,
            ResourceLimits::default()
        )
        .is_err()
    );
}

#[test]
fn noncanonical_mimetype_packing_is_diagnosed_but_integrity_is_mandatory() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>real body</text:p></office:text></office:body></office:document-content>"
    );
    let original = package(InputFormat::Odt, &content, &[]);
    for reordered in [false, true] {
        let base = if reordered { rewrite_archive(&original, &[], true) } else { original.clone() };
        for signed in [false, true] {
            let (bytes, descriptor) =
                super::zip_compatibility::named_descriptor(base.clone(), signed, "mimetype");
            let output = convert_policy(
                &bytes,
                InputFormat::Odt,
                ErrorPolicy::BestEffort,
                ResourceLimits::default(),
            )
            .unwrap();
            assert!(output.diagnostics.iter().any(|d| d.code == "odf.noncanonicalMimetype"));
            assert!(
                convert_policy(
                    &bytes,
                    InputFormat::Odt,
                    ErrorPolicy::Strict,
                    ResourceLimits::default()
                )
                .is_err()
            );
            for field in [0, 4, 8] {
                let mut corrupt = bytes.clone();
                corrupt[descriptor + usize::from(signed) * 4 + field] ^= 1;
                assert!(
                    convert_policy(
                        &corrupt,
                        InputFormat::Odt,
                        ErrorPolicy::BestEffort,
                        ResourceLimits::default()
                    )
                    .is_err()
                );
            }
        }
    }
}

#[test]
fn cached_index_fields_and_image_bullets_retain_body_under_best_effort() {
    let content = format!(
        "<office:document-content {NS}><office:automatic-styles><text:list-style style:name='L'><text:list-level-style-image text:level='1' xlink:href='ignored.png'/></text:list-style></office:automatic-styles><office:body><office:text><text:table-of-content><text:table-of-content-source/><text:index-body><text:index-title><text:p>Contents</text:p></text:index-title><text:p>Entry one</text:p></text:index-body></text:table-of-content><text:p><text:user-field-get text:name='f' style:data-style-name='n'>cached value</text:user-field-get><text:bibliography-mark text:identifier='r'>[reference]</text:bibliography-mark></text:p><text:list text:style-name='L'><text:list-item><text:p>Bullet body</text:p></text:list-item></text:list></office:text></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let output = convert_policy(
        &bytes,
        InputFormat::Odt,
        ErrorPolicy::BestEffort,
        ResourceLimits::default(),
    )
    .unwrap();
    let md = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    for expected in ["Contents", "Entry one", "cached value", "reference", "Bullet body"] {
        assert!(md.contains(expected), "{md}");
    }
    for code in ["odf.cachedIndex", "odf.cachedField", "odf.imageListMarker"] {
        assert!(output.diagnostics.iter().any(|d| d.code == code));
    }
    assert!(
        convert_policy(&bytes, InputFormat::Odt, ErrorPolicy::Strict, ResourceLimits::default())
            .is_err()
    );
}

#[test]
fn vector_paths_keep_text_and_whitespace_separated_transforms_are_parsed() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:presentation><draw:page><draw:path svg:x='1cm' svg:y='2cm' svg:width='3cm' svg:height='4cm' svg:d='M0 0' svg:viewBox='0 0 10 10' draw:transform='rotate (0) translate (1cm 2cm)'><text:p>Path caption</text:p></draw:path></draw:page></office:presentation></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odp, &content, &[]);
    let output = convert_policy(
        &bytes,
        InputFormat::Odp,
        ErrorPolicy::BestEffort,
        ResourceLimits::default(),
    )
    .unwrap();
    let md = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(md.contains("Path caption"));
    let into_markdown_core::Block::Slide { blocks, .. } = &output.document.blocks[0].block else {
        panic!()
    };
    let bounds = blocks[0].provenance.locator.bounds.unwrap();
    assert!((bounds.x - 2.0 * 72.0 / 2.54).abs() < 0.01);
    assert!((bounds.y - 4.0 * 72.0 / 2.54).abs() < 0.01);
    assert!(
        convert_policy(&bytes, InputFormat::Odp, ErrorPolicy::Strict, ResourceLimits::default())
            .is_err()
    );
    let bad = content.replace("rotate (0)", "rotate (NaN)");
    assert!(
        convert_policy(
            &package(InputFormat::Odp, &bad, &[]),
            InputFormat::Odp,
            ErrorPolicy::BestEffort,
            ResourceLimits::default()
        )
        .is_err()
    );
}

#[test]
fn unsupported_images_still_require_present_crc_valid_members() {
    let content = format!(
        "<office:document-content {NS}><office:body><office:text><text:p>body</text:p><draw:frame><draw:image xlink:href='a.svg'/></draw:frame></office:text></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odt, &content, &[("a.svg", "image/svg+xml", b"<svg/>")]);
    let missing = rewrite_archive(&bytes, &["a.svg"], false);
    assert!(
        convert_policy(
            &missing,
            InputFormat::Odt,
            ErrorPolicy::BestEffort,
            ResourceLimits::default()
        )
        .is_err()
    );
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let offset = usize::try_from(archive.by_name("a.svg").unwrap().data_start()).unwrap();
    drop(archive);
    let mut corrupt = bytes;
    corrupt[offset] ^= 0x80;
    assert!(
        convert_policy(
            &corrupt,
            InputFormat::Odt,
            ErrorPolicy::BestEffort,
            ResourceLimits::default()
        )
        .is_err()
    );
}

#[test]
fn drawing_only_macro_has_source_shape_placeholder_not_heading_only_output() {
    let content = format!(
        "<office:document-content {NS} xmlns:s='urn:oasis:names:tc:opendocument:xmlns:script:1.0'><office:body><office:presentation><draw:page><draw:custom-shape><office:event-listeners><s:event-listener s:language='Basic'/></office:event-listeners><text:p/><draw:enhanced-geometry draw:type='smiley'/></draw:custom-shape></draw:page></office:presentation></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odp, &content, &[]);
    let output = convert_policy(
        &bytes,
        InputFormat::Odp,
        ErrorPolicy::BestEffort,
        ResourceLimits::default(),
    )
    .unwrap();
    let md = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(md.contains("Drawing omitted: smiley"));
    assert!(output.diagnostics.iter().any(|d| d.code == "odf.scriptsOmitted"));
    assert!(output.diagnostics.iter().any(|d| d.code == "odf.drawingPlaceholder"));
    assert!(output.assets.is_empty());
}

#[test]
fn unsupported_list_restart_keeps_items_and_diagnoses_numbering() {
    let content = format!(
        "<office:document-content {NS}><office:automatic-styles><text:list-style style:name='L'><text:list-level-style-number text:level='1' style:num-format='1'/></text:list-style></office:automatic-styles><office:body><office:text><text:list text:style-name='L'><text:list-item><text:p>first</text:p></text:list-item><text:list-item text:start-value='9'><text:p>second</text:p></text:list-item></text:list></office:text></office:body></office:document-content>"
    );
    let bytes = package(InputFormat::Odt, &content, &[]);
    let output = convert_policy(
        &bytes,
        InputFormat::Odt,
        ErrorPolicy::BestEffort,
        ResourceLimits::default(),
    )
    .unwrap();
    let md = render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
    assert!(md.contains("first") && md.contains("second"));
    assert!(output.diagnostics.iter().any(|d| d.code == "odf.listRestart"));
    assert!(
        convert_policy(&bytes, InputFormat::Odt, ErrorPolicy::Strict, ResourceLimits::default())
            .is_err()
    );
}
