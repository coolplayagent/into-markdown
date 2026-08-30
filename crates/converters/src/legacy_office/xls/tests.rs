use super::inventory::{inventory_memory_plan, scan_workbook_inventory};
use super::preflight::*;
use super::wrapper::*;
use super::*;
use crate::msg::ole::CompoundFile;
use into_markdown_core::{Cell, ExecutionOptions, Inline, ResourceLimits};

fn budget<'a>(options: &'a ConversionOptions, context: &'a ExecutionContext) -> LegacyBudget<'a> {
    LegacyBudget::new(64, options, context).unwrap()
}

fn raw_biff4_with_label(text: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, BOF4, &[0, 0, 0x10, 0, 0, 0]).unwrap();

    let mut dimensions = Vec::new();
    dimensions.extend_from_slice(&0u16.to_le_bytes());
    dimensions.extend_from_slice(&1u16.to_le_bytes());
    dimensions.extend_from_slice(&0u16.to_le_bytes());
    dimensions.extend_from_slice(&1u16.to_le_bytes());
    dimensions.extend_from_slice(&0u16.to_le_bytes());
    push_biff_record(&mut bytes, DIMENSIONS, &dimensions).unwrap();

    let mut label = vec![0; 6];
    label.extend_from_slice(&u16::try_from(text.len()).unwrap().to_le_bytes());
    label.extend_from_slice(text);
    push_biff_record(&mut bytes, 0x0204, &label).unwrap();
    push_biff_record(&mut bytes, EOF, &[]).unwrap();
    bytes
}

fn convert_fixture(bytes: &[u8]) -> ConverterOutput {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut compound_budget = LegacyBudget::new(bytes.len(), &options, &context).unwrap();
    let compound = CompoundFile::open(bytes, &mut compound_budget).unwrap();
    convert(bytes, compound.root(), &mut compound_budget, &options, &context, false).unwrap()
}

fn table(output: &ConverterOutput) -> (&str, &[into_markdown_core::TableRow]) {
    let Block::Sheet { name, blocks } = &output.document.blocks[0].block else {
        panic!("fixture did not emit a worksheet")
    };
    let Block::Table { rows, .. } = &blocks[0].block else {
        panic!("fixture worksheet did not emit a table")
    };
    (name, rows)
}

fn cell_text(cell: &Cell) -> String {
    let Some(block) = cell.blocks.first() else { return String::new() };
    let Block::Paragraph(inlines) = &block.block else {
        panic!("fixture cell did not emit a paragraph")
    };
    inlines
        .iter()
        .map(|inline| match inline {
            Inline::Text { value, .. } | Inline::Code(value) => value.as_str(),
            _ => panic!("fixture cell emitted an unexpected inline"),
        })
        .collect()
}

fn workbook_with_merge() -> Vec<u8> {
    const FIXTURE: &[u8] = include_bytes!("../../../../../tools/macos-release/fixtures/normal.xls");
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut compound_budget = LegacyBudget::new(FIXTURE.len(), &options, &context).unwrap();
    let compound = CompoundFile::open(FIXTURE, &mut compound_budget).unwrap();
    let mut workbook = compound.root().stream(WORKBOOK).unwrap().to_vec();
    let mut cursor = 0usize;
    let mut final_eof = None;
    while let Some(header) = workbook.get(cursor..cursor.saturating_add(4)) {
        let kind = u16::from_le_bytes([header[0], header[1]]);
        let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let Some(end) = cursor.checked_add(4).and_then(|value| value.checked_add(length)) else {
            break;
        };
        if end > workbook.len() {
            break;
        }
        if kind == EOF {
            final_eof = Some(cursor);
        }
        cursor = end;
    }
    let mut merged = Vec::new();
    push_biff_record(&mut merged, 0x00e5, &[1, 0, 0, 0, 0, 0, 0, 0, 1, 0]).unwrap();
    workbook.splice(final_eof.unwrap()..final_eof.unwrap(), merged);
    let layout = cfb_wrapper_layout(workbook.len()).unwrap();
    build_cfb_wrapper(&workbook, false, false, BIFF8, layout).unwrap()
}

#[test]
fn rejects_pre_biff8_and_filepass() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut old = vec![0x09, 0x08, 4, 0, 0x00, 0x05, 0x05, 0, 0x0a, 0, 0, 0];
    assert!(matches!(
        preflight(&old, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict),
        Err(ConversionError::Unsupported { .. })
    ));
    let recovered =
        preflight(&old, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
            .unwrap();
    assert_eq!(recovered.biff_version, BIFF5);
    old[4..6].copy_from_slice(&BIFF8.to_le_bytes());
    old.extend_from_slice(&[0x2f, 0, 0, 0]);
    assert!(matches!(
        preflight(&old, WORKBOOK, &mut budget(&options, &context), options.error_policy),
        Err(ConversionError::Encrypted)
    ));
}

#[test]
fn dimensions_use_table_resource_limits() {
    let limits = ResourceLimits { max_table_rows: 10, ..ResourceLimits::default() };
    let options = ConversionOptions { limits, ..ConversionOptions::default() };
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut bytes = vec![0x09, 0x08, 4, 0, 0, 6, 0, 0];
    bytes.extend_from_slice(&[0x00, 0x02, 12, 0]);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&100u32.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    assert!(matches!(
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), options.error_policy),
        Err(ConversionError::ResourceLimit { limit: "max_table_rows", .. })
    ));
}

#[test]
fn best_effort_accepts_only_zero_tail_after_complete_substream() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut bytes = vec![0x09, 0x08, 4, 0, 0, 6, 5, 0, 0x0a, 0, 0, 0, 0];
    let recovered =
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
            .unwrap();
    assert!(recovered.has(PreflightFlag::TailPadding));

    assert!(matches!(
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict,),
        Err(ConversionError::Malformed { .. })
    ));
    *bytes.last_mut().unwrap() = 1;
    assert!(matches!(
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort,),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn noncanonical_dimensions_are_omitted_only_in_best_effort() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, BOF, &[0, 6, 5, 0]).unwrap();
    let mut dimensions = Vec::new();
    dimensions.extend_from_slice(&0u32.to_le_bytes());
    dimensions.extend_from_slice(&1u32.to_le_bytes());
    dimensions.extend_from_slice(&0u16.to_le_bytes());
    dimensions.extend_from_slice(&1u16.to_le_bytes());
    dimensions.extend_from_slice(&[0; 4]);
    push_biff_record(&mut bytes, DIMENSIONS, &dimensions).unwrap();
    push_biff_record(&mut bytes, EOF, &[]).unwrap();

    let recovered =
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
            .unwrap();
    assert!(recovered.has(PreflightFlag::DimensionMetadata));
    assert!(matches!(
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict,),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn noncanonical_formula_string_cache_reserved_bytes_are_normalized_only_in_best_effort() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, BOF, &[0, 6, 5, 0]).unwrap();
    let formula_offset = bytes.len();
    let mut formula = vec![0; 22];
    formula[7] = 0x5a;
    formula[12..14].fill(0xff);
    push_biff_record(&mut bytes, FORMULA, &formula).unwrap();
    push_biff_record(&mut bytes, EOF, &[]).unwrap();

    let recovered =
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
            .unwrap();
    assert!(recovered.has(PreflightFlag::FormulaCacheMetadata));
    assert!(matches!(
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict),
        Err(ConversionError::Malformed { .. })
    ));

    let layout = cfb_wrapper_layout(bytes.len()).unwrap();
    let wrapper = build_cfb_wrapper(&bytes, false, true, BIFF8, layout).unwrap();
    let mut wrapper_budget = LegacyBudget::new(wrapper.len(), &options, &context).unwrap();
    let compound = CompoundFile::open(&wrapper, &mut wrapper_budget).unwrap();
    let workbook = compound.root().stream(WORKBOOK).unwrap();
    assert_eq!(&workbook[formula_offset + 11..formula_offset + 16], &[0; 5]);
}

#[test]
fn compatibility_wrapper_is_a_bounded_readable_cfb() {
    let mut workbook = raw_biff4_with_label(b"ok");
    let dimensions_offset =
        workbook.windows(4).position(|window| window == [0x00, 0x02, 0x0a, 0x00]).unwrap();
    workbook[dimensions_offset + 2..dimensions_offset + 4].copy_from_slice(&12_u16.to_le_bytes());
    workbook.splice(dimensions_offset + 14..dimensions_offset + 14, [0, 0]);
    let layout = cfb_wrapper_layout(workbook.len()).unwrap();
    let wrapper = build_cfb_wrapper(&workbook, true, false, BIFF4, layout).unwrap();
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut wrapper_budget = LegacyBudget::new(wrapper.len(), &options, &context).unwrap();
    let compound = CompoundFile::open(&wrapper, &mut wrapper_budget).unwrap();
    let recovered = compound.root().stream(WORKBOOK).unwrap();

    assert_eq!(&recovered[dimensions_offset..dimensions_offset + 2], &[0xff, 0xff]);
    assert_eq!(&recovered[..dimensions_offset], &workbook[..dimensions_offset]);
}

#[test]
fn raw_biff4_and_inventory_leases_honor_exact_capacity_and_release() {
    let bytes = raw_biff4_with_label(b"lease");
    let plan = raw_biff4_plan(&bytes).unwrap();
    let normalized_required = u64::try_from(plan.capacity).unwrap();
    let normalized_options = ConversionOptions {
        limits: ResourceLimits {
            max_memory_bytes: normalized_required,
            ..ResourceLimits::default()
        },
        ..ConversionOptions::default()
    };
    let normalized_context =
        ExecutionContext::new(ExecutionOptions::default(), normalized_options.limits.clone());
    let normalized_memory = normalized_context.reserve_memory(normalized_required).unwrap();
    let normalized = normalize_raw_biff4(&bytes, plan).unwrap();
    assert_eq!(normalized.capacity(), plan.capacity);
    assert_eq!(normalized_context.reserved_memory_bytes(), normalized_required);
    drop(normalized);
    drop(normalized_memory);
    assert_eq!(normalized_context.reserved_memory_bytes(), 0);

    let below_options = ConversionOptions {
        limits: ResourceLimits {
            max_memory_bytes: normalized_required - 1,
            ..ResourceLimits::default()
        },
        ..ConversionOptions::default()
    };
    let below = ExecutionContext::new(ExecutionOptions::default(), below_options.limits.clone());
    assert!(below.reserve_memory(normalized_required).is_err());
    assert_eq!(below.reserved_memory_bytes(), 0);

    let normalized = normalize_raw_biff4(&bytes, plan).unwrap();
    let inventory_required = inventory_memory_plan(normalized.len()).unwrap();
    let inventory_options = ConversionOptions {
        limits: ResourceLimits {
            max_memory_bytes: inventory_required,
            ..ResourceLimits::default()
        },
        ..ConversionOptions::default()
    };
    let inventory_context =
        ExecutionContext::new(ExecutionOptions::default(), inventory_options.limits.clone());
    let mut inventory_budget =
        LegacyBudget::new(normalized.len(), &inventory_options, &inventory_context).unwrap();
    let hints = scan_workbook_inventory(
        &normalized,
        BIFF4,
        WORKBOOK,
        &mut inventory_budget,
        &inventory_context,
        ErrorPolicy::BestEffort,
    )
    .unwrap();
    assert_eq!(inventory_context.reserved_memory_bytes(), inventory_required);
    drop(hints);
    assert_eq!(inventory_context.reserved_memory_bytes(), 0);

    let inventory_below_options = ConversionOptions {
        limits: ResourceLimits {
            max_memory_bytes: inventory_required - 1,
            ..ResourceLimits::default()
        },
        ..ConversionOptions::default()
    };
    let inventory_below =
        ExecutionContext::new(ExecutionOptions::default(), inventory_below_options.limits.clone());
    let mut inventory_below_budget =
        LegacyBudget::new(normalized.len(), &inventory_below_options, &inventory_below).unwrap();
    assert!(matches!(
        scan_workbook_inventory(
            &normalized,
            BIFF4,
            WORKBOOK,
            &mut inventory_below_budget,
            &inventory_below,
            ErrorPolicy::BestEffort,
        ),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(inventory_below.reserved_memory_bytes(), 0);
}

#[test]
fn biff4_formats_use_ordinal_keys_and_only_malformed_optional_records_recover() {
    let mut bytes = raw_biff4_with_label(b"formatted");
    let mut globals = Vec::new();
    push_biff_record(&mut globals, 0x041e, b"\0\0\x05$0.00").unwrap();
    push_biff_record(&mut globals, 0x00e0, &[0, 0, 0, 0]).unwrap();
    bytes.splice(10..10, globals);
    let normalized = normalize_raw_biff4(&bytes, raw_biff4_plan(&bytes).unwrap()).unwrap();
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut scan_budget = LegacyBudget::new(normalized.len(), &options, &context).unwrap();
    let hints = scan_workbook_inventory(
        &normalized,
        BIFF4,
        WORKBOOK,
        &mut scan_budget,
        &context,
        ErrorPolicy::BestEffort,
    )
    .unwrap();
    assert_eq!(hints.recovered_format_records, 0);
    assert_eq!(hints.cell_formats.len(), 1);
    assert_eq!(hints.cell_formats[0].format_index, 0);
    assert_eq!(hints.format_codes.get(&0).map(String::as_str), Some("$0.00"));

    let mut malformed_bytes = raw_biff4_with_label(b"malformed");
    let mut malformed_format = Vec::new();
    push_biff_record(&mut malformed_format, 0x041e, b"\0\0\x05x").unwrap();
    malformed_bytes.splice(10..10, malformed_format);
    let malformed_normalized =
        normalize_raw_biff4(&malformed_bytes, raw_biff4_plan(&malformed_bytes).unwrap()).unwrap();
    let mut best_effort_budget =
        LegacyBudget::new(malformed_normalized.len(), &options, &context).unwrap();
    let recovered = scan_workbook_inventory(
        &malformed_normalized,
        BIFF4,
        WORKBOOK,
        &mut best_effort_budget,
        &context,
        ErrorPolicy::BestEffort,
    )
    .unwrap();
    assert_eq!(recovered.recovered_format_records, 1);
    drop(recovered);

    let mut strict_budget =
        LegacyBudget::new(malformed_normalized.len(), &options, &context).unwrap();
    assert!(
        scan_workbook_inventory(
            &malformed_normalized,
            BIFF4,
            WORKBOOK,
            &mut strict_budget,
            &context,
            ErrorPolicy::Strict,
        )
        .is_err()
    );
}

#[test]
fn long_format_mulblank_inventory_is_interned_and_obeys_its_exact_lease() {
    let mut bytes = raw_biff4_with_label(b"anchor");
    let mut globals = Vec::new();
    let format = format!("${}", "0".repeat(254));
    let mut format_body = vec![0, 0, u8::try_from(format.len()).unwrap()];
    format_body.extend_from_slice(format.as_bytes());
    push_biff_record(&mut globals, 0x041e, &format_body).unwrap();
    push_biff_record(&mut globals, 0x00e0, &[0, 0, 0, 0]).unwrap();
    bytes.splice(10..10, globals);

    let mut mul_blank = Vec::new();
    mul_blank.extend_from_slice(&0_u16.to_le_bytes());
    mul_blank.extend_from_slice(&1_u16.to_le_bytes());
    for _ in 1..=255 {
        mul_blank.extend_from_slice(&0_u16.to_le_bytes());
    }
    mul_blank.extend_from_slice(&255_u16.to_le_bytes());
    let eof = bytes.len() - 4;
    let mut record = Vec::new();
    push_biff_record(&mut record, 0x00be, &mul_blank).unwrap();
    bytes.splice(eof..eof, record);

    let normalized = normalize_raw_biff4(&bytes, raw_biff4_plan(&bytes).unwrap()).unwrap();
    let required = inventory_memory_plan(normalized.len()).unwrap();
    let options = ConversionOptions {
        limits: ResourceLimits { max_memory_bytes: required, ..ResourceLimits::default() },
        ..ConversionOptions::default()
    };
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut scan_budget = LegacyBudget::new(normalized.len(), &options, &context).unwrap();
    let hints = scan_workbook_inventory(
        &normalized,
        BIFF4,
        WORKBOOK,
        &mut scan_budget,
        &context,
        ErrorPolicy::BestEffort,
    )
    .unwrap();
    assert_eq!(hints.cell_formats.len(), 256);
    assert_eq!(hints.format_codes.len(), 1);
    assert_eq!(hints.format_codes.get(&0), Some(&format));
    drop(hints);
    assert_eq!(context.reserved_memory_bytes(), 0);

    let below_options = ConversionOptions {
        limits: ResourceLimits { max_memory_bytes: required - 1, ..ResourceLimits::default() },
        ..ConversionOptions::default()
    };
    let below = ExecutionContext::new(ExecutionOptions::default(), below_options.limits.clone());
    let mut below_budget = LegacyBudget::new(normalized.len(), &below_options, &below).unwrap();
    assert!(matches!(
        scan_workbook_inventory(
            &normalized,
            BIFF4,
            WORKBOOK,
            &mut below_budget,
            &below,
            ErrorPolicy::BestEffort,
        ),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    assert_eq!(below.reserved_memory_bytes(), 0);
}

#[test]
fn large_compatibility_cfb_uses_precise_open_lease_boundary() {
    let workbook = vec![0x5a; 16 * 1024 * 1024];
    let layout = cfb_wrapper_layout(workbook.len()).unwrap();
    let wrapper = build_cfb_wrapper(&workbook, false, false, BIFF8, layout).unwrap();

    let options = ConversionOptions::default();
    let measuring = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut measuring_budget = LegacyBudget::new(wrapper.len(), &options, &measuring).unwrap();
    let compound = crate::msg::ole::CompoundFile::open_with_compatibility(
        &wrapper,
        &mut measuring_budget,
        crate::msg::ole::CompoundCompatibility::LegacyOfficeBestEffort,
    )
    .unwrap();
    let required = measuring.reserved_memory_bytes();
    assert!(required > workbook.len() as u64);
    assert!(required < wrapper.len() as u64 * 2);
    drop(compound);
    assert_eq!(measuring.reserved_memory_bytes(), 0);

    let exact_options = ConversionOptions {
        limits: ResourceLimits { max_memory_bytes: required, ..options.limits.clone() },
        ..options.clone()
    };
    let exact = ExecutionContext::new(ExecutionOptions::default(), exact_options.limits.clone());
    let mut exact_budget = LegacyBudget::new(wrapper.len(), &exact_options, &exact).unwrap();
    let compound = crate::msg::ole::CompoundFile::open_with_compatibility(
        &wrapper,
        &mut exact_budget,
        crate::msg::ole::CompoundCompatibility::LegacyOfficeBestEffort,
    )
    .unwrap();
    drop(compound);
    assert_eq!(exact.reserved_memory_bytes(), 0);

    let below_options = ConversionOptions {
        limits: ResourceLimits { max_memory_bytes: required - 1, ..options.limits.clone() },
        ..options
    };
    let below = ExecutionContext::new(ExecutionOptions::default(), below_options.limits.clone());
    let mut below_budget = LegacyBudget::new(wrapper.len(), &below_options, &below).unwrap();
    let error = crate::msg::ole::CompoundFile::open_with_compatibility(
        &wrapper,
        &mut below_budget,
        crate::msg::ole::CompoundCompatibility::LegacyOfficeBestEffort,
    )
    .unwrap_err();
    assert!(matches!(error, ConversionError::ResourceLimit { .. }));
    assert_eq!(below.reserved_memory_bytes(), 0);
}

#[test]
fn dense_noncanonical_dimensions_use_one_streaming_recovery_flag() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, BOF, &[0, 6, 5, 0]).unwrap();
    let mut dimensions = Vec::new();
    dimensions.extend_from_slice(&0_u32.to_le_bytes());
    dimensions.extend_from_slice(&1_u32.to_le_bytes());
    dimensions.extend_from_slice(&0_u16.to_le_bytes());
    dimensions.extend_from_slice(&1_u16.to_le_bytes());
    dimensions.extend_from_slice(&[0; 4]);
    for _ in 0..4_096 {
        push_biff_record(&mut bytes, DIMENSIONS, &dimensions).unwrap();
    }
    push_biff_record(&mut bytes, EOF, &[]).unwrap();

    let recovered =
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
            .unwrap();
    assert!(recovered.has(PreflightFlag::DimensionMetadata));
    assert_eq!(recovered.logical_end, bytes.len());
    assert_eq!(context.reserved_memory_bytes(), 0);
}

#[test]
fn best_effort_ignores_only_truncated_view_metadata_after_complete_substream() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, BOF, &[0, 6, 5, 0]).unwrap();
    push_biff_record(&mut bytes, EOF, &[]).unwrap();
    let complete_end = bytes.len();
    bytes.extend_from_slice(&WINDOW1.to_le_bytes());
    bytes.extend_from_slice(&10_u16.to_le_bytes());
    bytes.extend_from_slice(&[1, 2]);

    let recovered =
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
            .unwrap();
    assert!(recovered.has(PreflightFlag::OptionalTailRecord));
    assert_eq!(recovered.logical_end, complete_end);
    assert!(
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict).is_err()
    );

    bytes[complete_end..complete_end + 2].copy_from_slice(&DIMENSIONS.to_le_bytes());
    assert!(
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort)
            .is_err()
    );

    let mut incomplete = Vec::new();
    push_biff_record(&mut incomplete, BOF, &[0, 6, 5, 0]).unwrap();
    incomplete.extend_from_slice(&WINDOW1.to_le_bytes());
    incomplete.extend_from_slice(&10_u16.to_le_bytes());
    incomplete.extend_from_slice(&[1, 2]);
    assert!(
        preflight(&incomplete, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort,)
            .is_err()
    );
}

#[test]
fn optional_tail_recovery_requires_every_bound_sheet_substream() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, BOF, &[0, 6, 5, 0]).unwrap();
    let mut bound_sheet = 128_u32.to_le_bytes().to_vec();
    bound_sheet.extend_from_slice(&[0, 0, 1, 0, b'A']);
    push_biff_record(&mut bytes, BOUND_SHEET, &bound_sheet).unwrap();
    push_biff_record(&mut bytes, EOF, &[]).unwrap();
    bytes.extend_from_slice(&WINDOW1.to_le_bytes());
    bytes.extend_from_slice(&10_u16.to_le_bytes());
    bytes.extend_from_slice(&[1, 2]);

    assert!(matches!(
        preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort),
        Err(ConversionError::Malformed { .. })
    ));
}

#[test]
fn xls_content_cell_order_display_values_and_merges_are_stable() {
    const FIXTURE: &[u8] = include_bytes!("../../../../../tools/macos-release/fixtures/normal.xls");
    let output = convert_fixture(FIXTURE);
    let (name, rows) = table(&output);
    assert_eq!(name, "Corpus");
    let values = rows
        .iter()
        .map(|row| row.cells.iter().map(cell_text).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        [
            [
                "Corpus",
                "=TRUE [biff-sha256:9db374756abdee7a36154901f76e8ea2cbcb7af47fde1b2be040e6071ca319c4] [cached: true]",
                "42.5",
            ],
            [
                "2024-01-01 00:00:00",
                "=SUM(1,2) [biff-sha256:81908119f54f4d549711dc093838eceefabd13fa8db5cb46a94ab9b2f7f2f8aa] [cached: 3]",
                "=cmd",
            ],
        ]
    );
    for (row_index, row) in rows.iter().enumerate() {
        for (column_index, cell) in row.cells.iter().enumerate() {
            let locator = &cell.blocks[0].provenance.locator;
            assert_eq!(locator.sheet.as_deref(), Some("Corpus"));
            let reference = locator.cell.as_ref().unwrap();
            assert_eq!(reference.row, u32::try_from(row_index).unwrap());
            assert_eq!(reference.column, u32::try_from(column_index).unwrap());
        }
    }

    let merged = convert_fixture(&workbook_with_merge());
    let (_, rows) = table(&merged);
    assert_eq!(rows[0].cells.len(), 2);
    assert_eq!(rows[0].cells[0].row_span, 1);
    assert_eq!(rows[0].cells[0].column_span, 2);
    assert_eq!(cell_text(&rows[0].cells[0]), "Corpus");
    assert_eq!(cell_text(&rows[0].cells[1]), "42.5");
}

#[test]
fn tall_minimal_xls_pages_before_the_document_node_limit_without_loss() {
    const ROWS: u16 = 40_000;
    let mut bytes = raw_biff4_with_label(b"anchor");
    let dimensions =
        bytes.windows(4).position(|window| window == [0x00, 0x02, 0x0a, 0x00]).unwrap();
    bytes[dimensions + 6..dimensions + 8].copy_from_slice(&ROWS.to_le_bytes());
    let mut tail_label = vec![0; 6];
    tail_label[..2].copy_from_slice(&(ROWS - 1).to_le_bytes());
    tail_label.extend_from_slice(&4_u16.to_le_bytes());
    tail_label.extend_from_slice(b"tail");
    let mut tail_record = Vec::new();
    push_biff_record(&mut tail_record, 0x0204, &tail_label).unwrap();
    let eof = bytes.len() - 4;
    bytes.splice(eof..eof, tail_record);

    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut conversion_budget = LegacyBudget::new(bytes.len(), &options, &context).unwrap();
    let output = convert_raw(&bytes, &mut conversion_budget, &options, &context).unwrap();

    let Block::Sheet { blocks, .. } = &output.document.blocks[0].block else {
        panic!("fixture did not emit a worksheet")
    };
    let mut next_row = 0_u32;
    for (page_index, block) in blocks.iter().enumerate() {
        let Block::Code { language, text } = &block.block else {
            panic!("large fixture did not use bounded page blocks")
        };
        assert_eq!(language.as_deref(), Some("tsv"));
        assert_eq!(block.id.0, format!("workbook-page-0-{page_index}"));
        assert_eq!(block.provenance.locator.sheet.as_deref(), Some("Sheet 1"));
        let rows = u32::try_from(text.lines().count()).unwrap();
        assert!((1..=2_048).contains(&rows));
        assert!(text.lines().all(|row| !row.contains('\t')));
        next_row += rows;
    }
    assert_eq!(next_row, u32::from(ROWS));
    assert_eq!(blocks.len(), usize::from(ROWS).div_ceil(2_048));
    let Block::Code { text, .. } = &blocks[0].block else { unreachable!() };
    assert_eq!(text.lines().next(), Some("anchor"));
    let Block::Code { text, .. } = &blocks.last().unwrap().block else { unreachable!() };
    assert_eq!(text.lines().last(), Some("tail"));
    assert_eq!(
        output.document.metadata.properties.get("spreadsheet.sheet.0.bounds").map(String::as_str),
        Some("A1:A40000")
    );
    assert_eq!(
        output.diagnostics.iter().filter(|item| item.code == "spreadsheet.largeTablePaged").count(),
        1
    );
}

#[test]
fn biff4_formula_framing_preserves_value_options_and_tokens() {
    let mut body = (0u8..16).collect::<Vec<_>>();
    body.extend_from_slice(&3u16.to_le_bytes());
    body.extend_from_slice(&[0x1e, 1, 0]);
    let mut record = Vec::new();
    push_normalized_biff4_formula(&mut record, &body).unwrap();

    assert_eq!(&record[..2], &0x0006_u16.to_le_bytes());
    let expected_record_size = u16::try_from(body.len())
        .expect("fixture body fits BIFF record size")
        .checked_add(4)
        .expect("normalized formula record size fits u16");
    assert_eq!(u16::from_le_bytes([record[2], record[3]]), expected_record_size);
    assert_eq!(&record[4..20], &body[..16]);
    assert_eq!(&record[20..24], &[0; 4]);
    assert_eq!(&record[24..], &body[16..]);
}

#[test]
fn continued_formula_strings_are_bounded_and_decoded_without_evaluation() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, STRING, &[5, 0, 0, b'a', b'b']).unwrap();
    push_biff_record(&mut bytes, CONTINUE, &[0, b'c', b'd', b'e']).unwrap();
    let mut conversion_budget = budget(&options, &context);
    assert_eq!(
        decode_continued_formula_string(
            &bytes,
            0,
            WORKBOOK,
            &mut conversion_budget,
            options.limits.max_field_bytes,
        )
        .unwrap()
        .as_deref(),
        Some("abcde")
    );

    let mut limited_budget = budget(&options, &context);
    assert!(matches!(
        decode_continued_formula_string(&bytes, 0, WORKBOOK, &mut limited_budget, 4,),
        Err(ConversionError::ResourceLimit { limit: "max_field_bytes", .. })
    ));
}

#[test]
fn raw_biff4_is_normalized_but_strict_mode_rejects_it() {
    let bytes = raw_biff4_with_label(b"ok");
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut best_effort_budget = LegacyBudget::new(bytes.len(), &options, &context).unwrap();
    let output = convert_raw(&bytes, &mut best_effort_budget, &options, &context).unwrap();

    assert!(format!("{:?}", output.document.blocks).contains("ok"));
    assert!(output.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "legacyOffice.xls.rawBiffRecovered"
            && diagnostic.severity == DiagnosticSeverity::Info
    }));

    let strict_options =
        ConversionOptions { error_policy: ErrorPolicy::Strict, ..ConversionOptions::default() };
    let strict_context =
        ExecutionContext::new(ExecutionOptions::default(), strict_options.limits.clone());
    let mut strict_budget =
        LegacyBudget::new(bytes.len(), &strict_options, &strict_context).unwrap();
    assert!(matches!(
        convert_raw(&bytes, &mut strict_budget, &strict_options, &strict_context),
        Err(ConversionError::Unsupported { .. })
    ));
}

#[test]
fn raw_biff4_applies_recovery_state_and_emits_security_diagnostics() {
    let mut bytes = raw_biff4_with_label(b"ok");
    let dimensions =
        bytes.windows(4).position(|window| window == [0x00, 0x02, 0x0a, 0x00]).unwrap();
    bytes[dimensions + 2..dimensions + 4].copy_from_slice(&12_u16.to_le_bytes());
    bytes.splice(dimensions + 14..dimensions + 14, [0, 0]);

    let eof = bytes.windows(4).rposition(|window| window == [0x0a, 0x00, 0x00, 0x00]).unwrap();
    let mut inert_metadata = Vec::new();
    push_biff_record(&mut inert_metadata, SUP_BOOK, &[]).unwrap();
    push_biff_record(&mut inert_metadata, OBJ, &[]).unwrap();
    bytes.splice(eof..eof, inert_metadata);
    bytes.extend_from_slice(&WINDOW1.to_le_bytes());
    bytes.extend_from_slice(&10_u16.to_le_bytes());
    bytes.extend_from_slice(&[1, 2]);

    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut conversion_budget = LegacyBudget::new(bytes.len(), &options, &context).unwrap();
    let output = convert_raw(&bytes, &mut conversion_budget, &options, &context).unwrap();
    for code in [
        "legacyOffice.xls.dimensionMetadataRecovered",
        "legacyOffice.xls.externalBindingsSkipped",
        "legacyOffice.xls.embeddedObjectsSkipped",
        "legacyOffice.xls.optionalTailRecordIgnored",
    ] {
        assert!(output.diagnostics.iter().any(|item| item.code == code), "missing {code}");
    }
    assert_eq!(
        output
            .diagnostics
            .iter()
            .filter(|item| item.code == "legacyOffice.xls.embeddedObjectsSkipped")
            .count(),
        1
    );
}
