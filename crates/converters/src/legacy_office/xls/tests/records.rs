use super::*;

mod reader;

fn sheet(kind: u16, text: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut bof = BIFF8.to_le_bytes().to_vec();
    bof.extend_from_slice(&kind.to_le_bytes());
    push_biff_record(&mut bytes, BOF, &bof).unwrap();
    if !text.is_empty() {
        let mut label = vec![0; 6];
        label.extend_from_slice(&u16::try_from(text.len()).unwrap().to_le_bytes());
        label.push(0);
        label.extend_from_slice(text);
        push_biff_record(&mut bytes, 0x0204, &label).unwrap();
    }
    push_biff_record(&mut bytes, EOF, &[]).unwrap();
    bytes
}

fn workbook(sheets: &[(u8, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, BOF, &[0, 6, 5, 0]).unwrap();
    push_biff_record(&mut bytes, 0x0042, &1200_u16.to_le_bytes()).unwrap();
    push_biff_record(&mut bytes, 0x00e0, &[0; 20]).unwrap();
    let mut pointers = Vec::new();
    for (index, (kind, _)) in sheets.iter().enumerate() {
        pointers.push(bytes.len() + 4);
        let name = b'A' + u8::try_from(index).unwrap();
        push_biff_record(&mut bytes, BOUND_SHEET, &[0, 0, 0, 0, 0, *kind, 1, 0, name]).unwrap();
    }
    push_biff_record(&mut bytes, EOF, &[]).unwrap();
    for ((_, data), pointer) in sheets.iter().zip(pointers) {
        let offset = u32::try_from(bytes.len()).unwrap();
        bytes[pointer..pointer + 4].copy_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(data);
    }
    bytes
}

fn converted(bytes: &[u8]) -> ConverterOutput {
    let layout = cfb_wrapper_layout(bytes.len()).unwrap();
    convert_fixture(&build_cfb_wrapper(bytes, false, false, BIFF8, layout).unwrap())
}

#[test]
fn chart_and_macro_sheets_preserve_worksheet_content_order_and_diagnostics() {
    for kind in [1, 2] {
        let before = sheet(0x0010, b"first");
        let optional = sheet(if kind == 1 { 0x0040 } else { 0x0020 }, b"inert");
        let after = sheet(0x0010, b"last");
        let bytes = workbook(&[(0, &before), (kind, &optional), (0, &after)]);
        let options = ConversionOptions::default();
        let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
        for policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context), policy).unwrap();
        }
        let output = converted(&bytes);
        assert_eq!(output.document.blocks.len(), 2);
        for (block, (expected_name, expected_value)) in
            output.document.blocks.iter().zip([("A", "first"), ("C", "last")])
        {
            let Block::Sheet { name, blocks } = &block.block else { panic!("not a sheet") };
            let Block::Table { rows, .. } = &blocks[0].block else { panic!("not a table") };
            assert_eq!(name, expected_name);
            assert_eq!(cell_text(&rows[0].cells[0]), expected_value);
        }
        let diagnostic = output
            .diagnostics
            .iter()
            .find(|item| item.code == "legacyOffice.xls.nonWorksheetSkipped")
            .unwrap();
        assert_eq!(diagnostic.locator.as_ref().unwrap().sheet.as_deref(), Some("B"));
        assert_eq!(output.document, converted(&bytes).document);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn optional_sheet_types_do_not_mask_mismatches_or_incomplete_substreams() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let ordinary = sheet(0x0010, b"keep");
    let chart = sheet(0x0020, b"");
    let mut truncated = chart.clone();
    truncated.truncate(truncated.len() - 4);
    let cases = [
        workbook(&[(0, &ordinary), (1, &chart)]),
        workbook(&[(0, &ordinary), (2, &truncated)]),
        workbook(&[(0, &ordinary), (2, &[])]),
    ];
    for bytes in cases {
        for policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
            assert!(matches!(
                preflight(&bytes, WORKBOOK, &mut budget(&options, &context), policy),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }
    let unknown = workbook(&[(0, &ordinary), (9, &chart)]);
    assert!(matches!(
        preflight(&unknown, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::BestEffort),
        Err(ConversionError::Unsupported { .. })
    ));
}

fn attachment(kind: u16) -> Vec<u8> {
    let size = match kind {
        SHARED_FORMULA => 10,
        0x0221 => 14,
        0x0236 => 16,
        _ => panic!(),
    };
    let mut body = vec![0; size];
    if kind != 0x0236 {
        body[size - 2] = 3;
        body.extend_from_slice(&[0x1e, 1, 0]);
    }
    body
}

#[test]
fn formula_attachments_preserve_continued_cached_strings() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    for kind in [SHARED_FORMULA, 0x0221, 0x0236] {
        let mut bytes = Vec::new();
        push_biff_record(&mut bytes, kind, &attachment(kind)).unwrap();
        push_biff_record(&mut bytes, STRING, &[5, 0, 0, b'c', b'a']).unwrap();
        push_biff_record(&mut bytes, CONTINUE, &[0, b'c', b'h', b'e']).unwrap();
        assert_eq!(
            decode_continued_formula_string(
                &bytes,
                0,
                WORKBOOK,
                &mut budget(&options, &context),
                100
            )
            .unwrap()
            .as_deref(),
            Some("cache")
        );
        let mut formula_sheet = sheet(0x0010, b"");
        formula_sheet.truncate(formula_sheet.len() - 4);
        let mut formula = vec![0; 22];
        formula[7] = 1; // Noncanonical reserved cache bytes require compatibility decoding.
        formula[12..14].fill(0xff);
        formula[20] = 3;
        formula.extend_from_slice(&[0x1e, 1, 0]);
        push_biff_record(&mut formula_sheet, FORMULA, &formula).unwrap();
        formula_sheet.extend_from_slice(&bytes);
        push_biff_record(&mut formula_sheet, EOF, &[]).unwrap();
        let output = converted(&workbook(&[(0, &formula_sheet)]));
        let (_, rows) = table(&output);
        assert!(cell_text(&rows[0].cells[0]).ends_with("[cached: cache]"));
    }
}

#[test]
fn formula_attachments_do_not_hide_truncation_or_arbitrary_records() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    for kind in [SHARED_FORMULA, 0x0221, 0x0236, 0x1234] {
        let mut bytes = Vec::new();
        push_biff_record(&mut bytes, kind, &[0; 9]).unwrap();
        push_biff_record(&mut bytes, STRING, &[1, 0, 0, b'x']).unwrap();
        assert!(
            decode_continued_formula_string(
                &bytes,
                0,
                WORKBOOK,
                &mut budget(&options, &context),
                100
            )
            .is_err()
        );
    }
    for kind in [SHARED_FORMULA, 0x0221] {
        let mut body = attachment(kind);
        body.pop();
        let mut bytes = Vec::new();
        push_biff_record(&mut bytes, kind, &body).unwrap();
        push_biff_record(&mut bytes, STRING, &[1, 0, 0, b'x']).unwrap();
        assert!(
            decode_continued_formula_string(
                &bytes,
                0,
                WORKBOOK,
                &mut budget(&options, &context),
                100
            )
            .is_err()
        );
    }
}

fn named_workbook(name: &[u8]) -> Vec<u8> {
    let ordinary = sheet(0x0010, b"retained");
    let mut bytes = workbook(&[(0, &ordinary)]);
    let mut cursor = 0;
    loop {
        let (kind, _, end) = biff_record(&bytes, cursor, WORKBOOK).unwrap();
        if kind == BOUND_SHEET {
            let mut record = vec![0; 6];
            record.push(u8::try_from(name.len()).unwrap());
            record.push(0);
            record.extend_from_slice(name);
            // The single sheet immediately follows this record and the globals EOF.
            let offset = u32::try_from(cursor + 4 + record.len() + 4).unwrap();
            record[..4].copy_from_slice(&offset.to_le_bytes());
            let mut replacement = Vec::new();
            push_biff_record(&mut replacement, BOUND_SHEET, &record).unwrap();
            bytes.splice(cursor..end, replacement);
            return bytes;
        }
        cursor = end;
    }
}

#[test]
fn long_sheet_names_are_complete_safe_and_best_effort_only() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    for length in [32, 55, 255] {
        let name = vec![b'N'; length];
        let bytes = named_workbook(&name);
        assert!(
            preflight(&bytes, WORKBOOK, &mut budget(&options, &context), ErrorPolicy::Strict)
                .is_err()
        );
        let output = converted(&bytes);
        let (actual_name, rows) = table(&output);
        assert_eq!(actual_name.as_bytes(), name);
        assert_eq!(cell_text(&rows[0].cells[0]), "retained");
        assert!(
            output
                .diagnostics
                .iter()
                .any(|item| item.code == "legacyOffice.xls.longSheetNameRecovered")
        );
    }
    for suffix in [b"/../x".as_slice(), b"\\x", b"\0x"] {
        let mut name = vec![b'N'; 32];
        name.extend_from_slice(suffix);
        let bytes = named_workbook(&name);
        assert!(
            scan_workbook_inventory(
                &bytes,
                BIFF8,
                WORKBOOK,
                &mut budget(&options, &context),
                &context,
                ErrorPolicy::BestEffort
            )
            .is_err()
        );
    }
    let mut truncated = named_workbook(&[b'N'; 32]);
    // Increase the declared count without adding name payload or moving the sheet.
    let mut cursor = 0;
    loop {
        let (kind, _, end) = biff_record(&truncated, cursor, WORKBOOK).unwrap();
        if kind == BOUND_SHEET {
            truncated[cursor + 10] = 33;
            break;
        }
        cursor = end;
    }
    assert!(
        scan_workbook_inventory(
            &truncated,
            BIFF8,
            WORKBOOK,
            &mut budget(&options, &context),
            &context,
            ErrorPolicy::BestEffort
        )
        .is_err()
    );
}
