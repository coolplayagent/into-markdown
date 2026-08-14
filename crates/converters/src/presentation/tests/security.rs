use super::super::PresentationConverter;
use super::super::budget::{ASSET_INDEX_ENTRY_CHARGE, MAX_XML_WIDTH};
use super::super::convert_presentation;
use super::super::error::malformed;
use super::super::geometry::sort_shapes_for_reading;
use super::super::model::{Package, ParseState, Shape};
use super::super::opc_package::part_allocation_charge;
use super::super::schema::{P_NS, TYPES_NS};
use super::super::tables::table_block;
use super::super::test_observer::PART_MATERIALIZATIONS;
use super::super::xml::{XmlProfile, preflight_xml};
use super::support::{
    convert, corrupt_central_entry_name_utf8, corrupt_stored_entry, deflated_zip, fixture,
    force_zip64_end, large_valid_png, mark_zip_encrypted, mark_zip_entry_symlink, picture_fixture,
    retained_lease_fixture, rewrite_part, zip, zip_with_directories,
};
use crate::docx::{supported_image, validate_image_bytes};
use into_markdown_core::{
    Block, ConversionError, ConversionOptions, Converter, ConverterOutput, ExecutionContext,
    ExecutionOptions, FormatCandidate, InputFormat, MAX_DOCUMENT_INLINES, MAX_DOCUMENT_NODES,
    ResolvedInput, SourceMetadata, estimate_retained_output,
};
use std::io::{Cursor, Read};
use std::sync::Arc;

#[test]
fn zip_paths_duplicates_symlinks_encryption_and_archive_limits_fail_closed() {
    let types = format!(r#"<Types xmlns="{}"/>"#, String::from_utf8_lossy(TYPES_NS));
    for name in
        ["../evil", "a/../evil", "/absolute", "C:drive", "a\\b", "a//b", "a?query", "a#fragment"]
    {
        assert!(matches!(
            convert(&zip(&[(name, Vec::new()), ("[Content_Types].xml", types.clone().into())])),
            Err(ConversionError::Malformed { .. })
        ));
    }
    assert!(matches!(
        convert(&zip(&[
            ("regular-mode-directory/", Vec::new()),
            ("[Content_Types].xml", types.clone().into()),
        ])),
        Err(ConversionError::Malformed { .. })
    ));
    let mut duplicate = zip(&[("duplicate1.xml", Vec::new()), ("duplicate2.xml", Vec::new())]);
    for index in 0..=duplicate.len().saturating_sub("duplicate2.xml".len()) {
        if &duplicate[index..index + "duplicate2.xml".len()] == b"duplicate2.xml" {
            duplicate[index..index + "duplicate2.xml".len()].copy_from_slice(b"duplicate1.xml");
        }
    }
    assert!(matches!(convert(&duplicate), Err(ConversionError::Malformed { .. })));

    let normal = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    assert!(convert(&force_zip64_end(normal.clone())).is_ok());
    let mut archive = zip::ZipArchive::new(Cursor::new(normal.as_slice())).unwrap();
    let mut normal_parts = Vec::<(String, Vec<u8>)>::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let name = file.name().to_owned();
        let mut value = Vec::new();
        file.read_to_end(&mut value).unwrap();
        normal_parts.push((name, value));
    }
    let normal_refs =
        normal_parts.iter().map(|(name, value)| (name.as_str(), value.clone())).collect::<Vec<_>>();
    assert!(convert(&zip_with_directories(&normal_refs, &["ppt/media/"])).is_ok());
    let mut duplicate_directories = zip_with_directories(
        &[("[Content_Types].xml", types.clone().into_bytes())],
        &["ppta/", "pptb/"],
    );
    for index in 0..=duplicate_directories.len().saturating_sub("pptb/".len()) {
        if &duplicate_directories[index..index + "pptb/".len()] == b"pptb/" {
            duplicate_directories[index..index + "pptb/".len()].copy_from_slice(b"ppta/");
        }
    }
    assert!(matches!(convert(&duplicate_directories), Err(ConversionError::Malformed { .. })));
    assert!(matches!(
        convert(&mark_zip_entry_symlink(normal.clone(), "ppt/slides/slide1.xml")),
        Err(ConversionError::Malformed { .. })
    ));
    assert!(matches!(
        convert(&mark_zip_encrypted(normal.clone())),
        Err(ConversionError::Encrypted)
    ));
    assert!(matches!(
        convert(&corrupt_stored_entry(normal.clone(), "ppt/slides/slide1.xml")),
        Err(ConversionError::Malformed { .. })
    ));
    assert!(matches!(
        convert(&corrupt_central_entry_name_utf8(normal.clone(), "ppt/slides/slide1.xml")),
        Err(ConversionError::Malformed { .. })
    ));

    let mut options = ConversionOptions::default();
    options.limits.max_archive_entries = 1;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        convert_presentation(&normal, &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
    ));
    let mut options = ConversionOptions::default();
    options.limits.max_decompressed_bytes = 16;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        convert_presentation(&normal, &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_decompressed_bytes", .. })
    ));
    let bomb = deflated_zip(&[
        ("[Content_Types].xml", types.into_bytes()),
        ("unreferenced/bomb.bin", vec![0_u8; 2 * 1024 * 1024]),
    ]);
    assert!(matches!(
        convert(&bomb),
        Err(ConversionError::ResourceLimit { limit: "archive_compression_ratio", .. })
    ));
}

#[test]
fn low_memory_refuses_before_part_allocation() {
    let bytes = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = 64;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    PART_MATERIALIZATIONS.with(|count| count.set(0));
    let input =
        ResolvedInput { bytes: Arc::from(bytes.clone()), metadata: SourceMetadata::default() };
    let candidate = FormatCandidate::explicit(InputFormat::Pptx);
    let plan =
        PresentationConverter.planned_output_bytes(&input, &candidate, &options, &context).unwrap();
    assert!(plan > context.available_memory_bytes());
    let converter_invoked = std::cell::Cell::new(false);
    let gated = context.reserve_memory(plan).and_then(|_memory| {
        converter_invoked.set(true);
        convert_presentation(&bytes, &options, &context)
    });
    assert!(matches!(gated, Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })));
    assert!(!converter_invoked.get());
    assert_eq!(PART_MATERIALIZATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(context.reserved_memory_bytes(), 0);
    assert!(matches!(
        convert_presentation(&bytes, &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(PART_MATERIALIZATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(context.reserved_memory_bytes(), 0);
    let mut positive = ConversionOptions::default();
    positive.limits.max_memory_bytes = 2 * 1024 * 1024;
    let context = ExecutionContext::new(ExecutionOptions::default(), positive.limits.clone());
    assert!(convert_presentation(&bytes, &positive, &context).is_ok());
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
#[allow(clippy::too_many_lines)]
fn retained_output_lease_is_exact_and_composite_peak_boundary_releases() {
    let bytes = retained_lease_fixture();
    let run = |memory_limit| {
        let mut options = ConversionOptions::default();
        options.limits.max_memory_bytes = memory_limit;
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        let result = convert_presentation(&bytes, &options, &context);
        (result, context)
    };

    let maximum = 64 * 1024 * 1024;
    let (result, context) = run(maximum);
    let output = result.expect("composite fixture must fit its conservative peak envelope");
    assert_eq!(output.document.blocks.len(), 2);
    assert_eq!(output.assets.len(), 1);
    assert!(
        output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "presentation.hiddenSlideSkipped")
    );
    assert!(
        output
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "presentation.dangerousPartsIgnored")
    );
    let required =
        estimate_retained_output(&output.document, &output.assets, &output.diagnostics).unwrap();
    assert_eq!(output.leased_memory_for(&context), required);
    assert_eq!(context.reserved_memory_bytes(), required);
    let unrelated =
        ExecutionContext::new(ExecutionOptions::default(), ConversionOptions::default().limits);
    assert_eq!(output.leased_memory_for(&unrelated), 0);

    let cloned_document = output.document.clone();
    let cloned_assets = output.assets.clone();
    let cloned_diagnostics = output.diagnostics.clone();
    let clone_required =
        estimate_retained_output(&cloned_document, &cloned_assets, &cloned_diagnostics).unwrap();
    let exact_context = ExecutionContext::new(
        ExecutionOptions::default(),
        into_markdown_core::ResourceLimits {
            max_memory_bytes: clone_required,
            ..into_markdown_core::ResourceLimits::default()
        },
    );
    let exact_retained = ConverterOutput::new(
        cloned_document.clone(),
        cloned_assets.clone(),
        cloned_diagnostics.clone(),
    )
    .account_retained(&exact_context)
    .unwrap();
    assert_eq!(exact_retained.leased_memory_for(&exact_context), clone_required);
    assert_eq!(exact_context.reserved_memory_bytes(), clone_required);
    let before_handoff = exact_context.reserved_memory_bytes();
    let exact_result = exact_retained
        .into_conversion_result(String::new(), Vec::new(), [None, None, None])
        .unwrap();
    assert_eq!(exact_context.reserved_memory_bytes(), before_handoff);
    drop(exact_result);
    assert_eq!(exact_context.reserved_memory_bytes(), 0);

    let low_context = ExecutionContext::new(
        ExecutionOptions::default(),
        into_markdown_core::ResourceLimits {
            max_memory_bytes: clone_required - 1,
            ..into_markdown_core::ResourceLimits::default()
        },
    );
    assert!(matches!(
        ConverterOutput::new(cloned_document, cloned_assets, cloned_diagnostics)
            .account_retained(&low_context),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(low_context.reserved_memory_bytes(), 0);

    drop(output);
    assert_eq!(context.reserved_memory_bytes(), 0);

    let mut low = 0_u64;
    let mut high = maximum;
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        let (result, context) = run(middle);
        let succeeded = result.is_ok();
        drop(result);
        assert_eq!(context.reserved_memory_bytes(), 0);
        if succeeded {
            high = middle;
        } else {
            low = middle;
        }
    }
    let (below, below_context) = run(high - 1);
    assert!(matches!(below, Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })));
    assert_eq!(below_context.reserved_memory_bytes(), 0);
    let (exact_peak, exact_context) = run(high);
    let exact_peak = exact_peak.unwrap();
    let exact_retained =
        estimate_retained_output(&exact_peak.document, &exact_peak.assets, &exact_peak.diagnostics)
            .unwrap();
    assert_eq!(exact_peak.leased_memory_for(&exact_context), exact_retained);
    assert_eq!(exact_context.reserved_memory_bytes(), exact_retained);
    drop(exact_peak);
    assert_eq!(exact_context.reserved_memory_bytes(), 0);
}

#[test]
#[allow(clippy::too_many_lines)]
fn large_image_storage_unique_duplicate_exact_boundary_and_drop() {
    const LARGE: usize = 12 * 1024 * 1024;
    const PNG_CODEC_WORKING_SET: u64 = 64 * 1024;
    let image = large_valid_png(LARGE);
    assert!(image.len() > 11 * 1024 * 1024);
    let bytes = picture_fixture(&[
        ("first", "large-a.png", image.clone()),
        ("second", "large-b.png", image),
    ]);
    let options = ConversionOptions::default();

    let default_context =
        ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let output = convert_presentation(&bytes, &options, &default_context).unwrap();
    assert_eq!(output.assets.len(), 1);
    assert!(output.assets[0].bytes.len() > 11 * 1024 * 1024);
    drop(output);
    assert_eq!(default_context.reserved_memory_bytes(), 0);

    let probe_context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut probe = Package::open(&bytes, &options, &probe_context).unwrap();
    let open_bytes = probe.memory_bytes;
    {
        let loaded = probe.load("ppt/media/large-a.png", &options, &probe_context).unwrap();
        validate_image_bytes(
            supported_image("ppt/media/large-a.png", "image/png").unwrap(),
            loaded,
            "ppt/media/large-a.png",
            &options,
            &probe_context,
        )
        .unwrap();
    }
    let first_capacity =
        u64::try_from(probe.parts["ppt/media/large-a.png"].bytes.capacity()).unwrap();
    let first_expanded =
        probe.entries.iter().find(|(name, _)| name == "ppt/media/large-a.png").unwrap().1.expanded;
    let first_charge = probe.parts["ppt/media/large-a.png"].charge;
    drop(probe);
    assert_eq!(probe_context.reserved_memory_bytes(), 0);

    let unique_peak = open_bytes
        .checked_add(first_charge)
        .and_then(|value| value.checked_add(PNG_CODEC_WORKING_SET))
        .unwrap();
    for (limit_bytes, succeeds) in [(unique_peak - 1, false), (unique_peak, true)] {
        let limits = into_markdown_core::ResourceLimits {
            max_memory_bytes: limit_bytes,
            ..options.limits.clone()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        let mut configured = options.clone();
        configured.limits = limits;
        let result = (|| {
            let mut package = Package::open(&bytes, &configured, &context)?;
            {
                let loaded = package.load("ppt/media/large-a.png", &configured, &context)?;
                validate_image_bytes(
                    supported_image("ppt/media/large-a.png", "image/png")?,
                    loaded,
                    "ppt/media/large-a.png",
                    &configured,
                    &context,
                )?;
            }
            let loaded = package
                .take_loaded("ppt/media/large-a.png")
                .ok_or_else(|| malformed(None, "large image missing"))?;
            let capacity = u64::try_from(loaded.bytes.capacity()).unwrap_or(u64::MAX);
            package.shrink_memory(loaded.charge - capacity)?;
            package.grow_memory(ASSET_INDEX_ENTRY_CHARGE * 2)?;
            drop(loaded);
            Ok::<(), ConversionError>(())
        })();
        assert_eq!(result.is_ok(), succeeds);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }

    let second_charge = part_allocation_charge("ppt/media/large-b.png", first_expanded).unwrap();
    let duplicate_peak = open_bytes
        .checked_add(first_capacity)
        .and_then(|value| value.checked_add(ASSET_INDEX_ENTRY_CHARGE * 2))
        .and_then(|value| value.checked_add(second_charge))
        .and_then(|value| value.checked_add(PNG_CODEC_WORKING_SET))
        .unwrap();
    for (limit_bytes, succeeds) in [(duplicate_peak - 1, false), (duplicate_peak, true)] {
        let limits = into_markdown_core::ResourceLimits {
            max_memory_bytes: limit_bytes,
            ..options.limits.clone()
        };
        let context = ExecutionContext::new(ExecutionOptions::default(), limits.clone());
        let mut configured = options.clone();
        configured.limits = limits;
        let result = (|| {
            let mut package = Package::open(&bytes, &configured, &context)?;
            let first = {
                let loaded = package.load("ppt/media/large-a.png", &configured, &context)?;
                validate_image_bytes(
                    supported_image("ppt/media/large-a.png", "image/png")?,
                    loaded,
                    "ppt/media/large-a.png",
                    &configured,
                    &context,
                )?;
                package
                    .take_loaded("ppt/media/large-a.png")
                    .ok_or_else(|| malformed(None, "large image missing"))?
            };
            let retained = u64::try_from(first.bytes.capacity()).unwrap_or(u64::MAX);
            package.shrink_memory(first.charge - retained)?;
            package.grow_memory(ASSET_INDEX_ENTRY_CHARGE * 2)?;
            {
                let loaded = package.load("ppt/media/large-b.png", &configured, &context)?;
                validate_image_bytes(
                    supported_image("ppt/media/large-b.png", "image/png")?,
                    loaded,
                    "ppt/media/large-b.png",
                    &configured,
                    &context,
                )?;
            }
            let duplicate = package
                .take_loaded("ppt/media/large-b.png")
                .ok_or_else(|| malformed(None, "duplicate image missing"))?;
            let duplicate_charge = duplicate.charge;
            drop(duplicate);
            package.shrink_memory(duplicate_charge)?;
            drop(first);
            Ok::<(), ConversionError>(())
        })();
        assert_eq!(result.is_ok(), succeeds);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn xml_ir_and_geometry_work_limits_are_stable() {
    let original = fixture(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
        &[],
    );
    let nested = format!(
        r#"<p:sld xmlns:p="{p}"><p:cSld><p:spTree><p:grpSp><p:grpSp><p:grpSp><p:grpSp></p:grpSp></p:grpSp></p:grpSp></p:grpSp></p:spTree></p:cSld></p:sld>"#,
        p = String::from_utf8_lossy(P_NS)
    );
    let nested = rewrite_part(&original, "ppt/slides/slide1.xml", nested.as_bytes());
    let mut options = ConversionOptions::default();
    options.limits.max_nesting_depth = 5;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        convert_presentation(&nested, &options, &context),
        Err(ConversionError::ResourceLimit { limit: "max_nesting_depth", .. })
    ));
    let mut wide =
        format!(r#"<p:sld xmlns:p="{}"><p:cSld><p:spTree>"#, String::from_utf8_lossy(P_NS));
    wide.push_str(&"<p:sp/>".repeat(MAX_XML_WIDTH + 1));
    wide.push_str("</p:spTree></p:cSld></p:sld>");
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    assert!(matches!(
        preflight_xml(
            wide.as_bytes(),
            "ppt/slides/wide.xml",
            XmlProfile::Slide,
            &options,
            &context
        ),
        Err(ConversionError::ResourceLimit { limit: "xml_width", .. })
    ));

    let mut state = ParseState { nodes: MAX_DOCUMENT_NODES, ..ParseState::default() };
    assert!(matches!(
        state.node(Block::Paragraph(Vec::new()), "part", 1, None, None, None),
        Err(ConversionError::ResourceLimit { limit: "max_document_nodes", .. })
    ));
    let mut state = ParseState { inlines: MAX_DOCUMENT_INLINES, ..ParseState::default() };
    assert!(matches!(
        state.add_inlines(1),
        Err(ConversionError::ResourceLimit { limit: "max_document_inlines", .. })
    ));

    let mut shapes = Vec::new();
    shapes.resize_with(3_164, Shape::default);
    for (index, shape) in shapes.iter_mut().enumerate() {
        shape.z_order = index;
        shape.geometry.cx = 914_400;
        shape.geometry.cy = 914_400;
    }
    let context =
        ExecutionContext::new(ExecutionOptions::default(), ConversionOptions::default().limits);
    assert!(matches!(
        sort_shapes_for_reading(&mut shapes, &context),
        Err(ConversionError::ResourceLimit { limit: "geometry_comparisons", .. })
    ));

    for (limit_name, limits) in [
        (
            "max_table_rows",
            into_markdown_core::ResourceLimits {
                max_table_rows: 0,
                ..into_markdown_core::ResourceLimits::default()
            },
        ),
        (
            "max_table_columns",
            into_markdown_core::ResourceLimits {
                max_table_columns: 0,
                ..into_markdown_core::ResourceLimits::default()
            },
        ),
        (
            "max_table_cells",
            into_markdown_core::ResourceLimits {
                max_table_cells: 0,
                ..into_markdown_core::ResourceLimits::default()
            },
        ),
    ] {
        let options = ConversionOptions { limits, ..ConversionOptions::default() };
        let mut state = ParseState::default();
        assert!(matches!(
            table_block(
                vec![vec![Vec::new()]],
                "table",
                1,
                None,
                1,
                &[],
                &options,
                &mut state
            ),
            Err(ConversionError::ResourceLimit { limit, .. }) if limit == limit_name
        ));
    }
}
