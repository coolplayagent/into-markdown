use super::*;
use crate::legacy_office::xls::reader_view::{format_id_remap, patch_reader_view};

#[test]
fn nested_chart_cannot_replace_cells_or_terminate_the_worksheet() {
    let mut ordinary = sheet(0x0010, b"source");
    ordinary.truncate(ordinary.len() - 4);
    ordinary.extend_from_slice(&sheet(0x0020, b"wrong"));
    let mut tail = sheet(0x0010, b"after");
    tail[12..14].copy_from_slice(&1_u16.to_le_bytes());
    ordinary.extend_from_slice(&tail[8..]);
    let bytes = workbook(&[(0, &ordinary)]);
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    for policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
        let result = preflight(&bytes, WORKBOOK, &mut budget(&options, &context), policy).unwrap();
        assert!(result.has(PreflightFlag::NestedCharts));
    }
    let mut view = bytes.clone();
    patch_reader_view(&mut view, &bytes, &std::collections::BTreeMap::new()).unwrap();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let (kind, body, end) = biff_record(&bytes, cursor, WORKBOOK).unwrap();
        let (patched_kind, patched_body, patched_end) =
            biff_record(&view, cursor, WORKBOOK).unwrap();
        assert_eq!(end, patched_end);
        assert_eq!(body, patched_body);
        if matches!(kind, BOUND_SHEET) {
            assert_eq!(kind, patched_kind);
        }
        cursor = end;
    }
    let output = converted(&bytes);
    let (_, rows) = table(&output);
    assert_eq!(rows.len(), 2);
    assert_eq!(cell_text(&rows[0].cells[0]), "source");
    assert_eq!(cell_text(&rows[1].cells[0]), "after");
    assert!(
        output.diagnostics.iter().any(|item| item.code == "legacyOffice.xls.chartCachesSkipped")
    );
    assert_eq!(output.document, converted(&bytes).document);
}

fn formatted_workbook(epoch: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_biff_record(&mut bytes, BOF, &[0, 6, 5, 0]).unwrap();
    push_biff_record(&mut bytes, 0x0042, &1200_u16.to_le_bytes()).unwrap();
    push_biff_record(&mut bytes, 0x0022, &epoch.to_le_bytes()).unwrap();
    for (index, code) in [(50_u16, "dd\"-\"mmm\"-\"yyyy"), (51, "hh\":\"mm AM/PM"), (164, "0.00%")]
    {
        let mut format = index.to_le_bytes().to_vec();
        format.extend_from_slice(&u16::try_from(code.len()).unwrap().to_le_bytes());
        format.push(0);
        format.extend_from_slice(code.as_bytes());
        push_biff_record(&mut bytes, 0x041e, &format).unwrap();
        let mut xf = vec![0; 20];
        xf[2..4].copy_from_slice(&index.to_le_bytes());
        push_biff_record(&mut bytes, 0x00e0, &xf).unwrap();
    }
    let pointer = bytes.len() + 4;
    push_biff_record(&mut bytes, BOUND_SHEET, &[0, 0, 0, 0, 0, 0, 1, 0, b'A']).unwrap();
    push_biff_record(&mut bytes, EOF, &[]).unwrap();
    let offset = u32::try_from(bytes.len()).unwrap();
    bytes[pointer..pointer + 4].copy_from_slice(&offset.to_le_bytes());
    push_biff_record(&mut bytes, BOF, &[0, 6, 0x10, 0]).unwrap();
    for (column, value) in [(0_u16, 1_f64), (1, 0.5), (2, 0.25)] {
        let mut number = vec![0; 2];
        number.extend_from_slice(&column.to_le_bytes());
        number.extend_from_slice(&column.to_le_bytes()); // XF index.
        number.extend_from_slice(&value.to_le_bytes());
        push_biff_record(&mut bytes, 0x0203, &number).unwrap();
    }
    let mut formula = vec![0; 22];
    formula[0] = 1;
    formula[6..14].copy_from_slice(&1_f64.to_le_bytes());
    formula[20] = 3;
    formula.extend_from_slice(&[0x1e, 1, 0]);
    push_biff_record(&mut bytes, FORMULA, &formula).unwrap();
    push_biff_record(&mut bytes, EOF, &[]).unwrap();
    bytes
}

#[test]
fn format_identifier_recovery_preserves_dates_epoch_and_formula_caches() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    for (epoch, date, time) in [
        (0, "1900-01-01 00:00:00", "1899-12-31 12:00:00"),
        (1, "1904-01-02 00:00:00", "1904-01-01 12:00:00"),
    ] {
        let bytes = formatted_workbook(epoch);
        let hints = scan_workbook_inventory(
            &bytes,
            BIFF8,
            WORKBOOK,
            &mut budget(&options, &context),
            &context,
            ErrorPolicy::BestEffort,
        )
        .unwrap();
        assert!(format_id_remap(&bytes, &hints, ErrorPolicy::Strict).is_err());
        let remap = format_id_remap(&bytes, &hints, ErrorPolicy::BestEffort).unwrap();
        assert_eq!(remap.get(&50), Some(&165)); // Existing 164 must not be overwritten.
        assert_eq!(remap.get(&51), Some(&166));
        let output = converted(&bytes);
        let (_, rows) = table(&output);
        assert_eq!(cell_text(&rows[0].cells[0]), date);
        assert_eq!(cell_text(&rows[0].cells[1]), time);
        assert_eq!(cell_text(&rows[0].cells[2]), "25.00%");
        assert!(cell_text(&rows[1].cells[0]).ends_with(&format!("[cached: {date}]")));
        assert!(
            output
                .diagnostics
                .iter()
                .any(|item| item.code == "legacyOffice.xls.formatIndexRecovered")
        );
    }
}

#[test]
fn format_remapping_never_reuses_referenced_slots_or_exhausted_space() {
    let mut hints = crate::workbook::LegacyXlsHints::default();
    hints.format_codes.insert(50, "yyyy".into());
    let mut bytes = Vec::new();
    for index in 164_u16..=382 {
        let mut xf = vec![0; 4];
        xf[2..4].copy_from_slice(&index.to_le_bytes());
        push_biff_record(&mut bytes, 0x00e0, &xf).unwrap();
    }
    push_biff_record(&mut bytes, EOF, &[]).unwrap();
    assert!(matches!(
        format_id_remap(&bytes, &hints, ErrorPolicy::BestEffort),
        Err(ConversionError::Unsupported { .. })
    ));
}
