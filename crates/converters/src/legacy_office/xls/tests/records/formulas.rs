use super::*;
use sha2::{Digest, Sha256};

fn formula_workbook(tokens: &[u8], cached: f64) -> Vec<u8> {
    let mut data = sheet(0x0010, b"");
    data.truncate(data.len() - 4);
    let mut formula = vec![0; 22];
    formula[6..14].copy_from_slice(&cached.to_le_bytes());
    formula[20..22].copy_from_slice(&u16::try_from(tokens.len()).unwrap().to_le_bytes());
    formula.extend_from_slice(tokens);
    push_biff_record(&mut data, FORMULA, &formula).unwrap();
    push_biff_record(&mut data, EOF, &[]).unwrap();
    workbook(&[(0, &data)])
}

#[test]
fn original_formula_tokens_are_authoritative_in_both_error_policies_and_repeated_reads() {
    for (tokens, prefix) in [
        (vec![0x24, 22, 0, 5, 0xc0, 0x1e, 0, 0, 0x0d], "=F23>0"),
        (vec![0x24, 28, 0, 26, 0xc0], "=AA29"),
        (vec![0x25, 0, 0, 1, 0, 0, 0xc0, 1, 0xc0], "=A1:B2"),
        (vec![0x20, 0, 0, 0, 0, 0, 0, 0], "[formula cached-only: array-constant]"),
        (vec![0x23, 1, 0, 0, 0], "[formula cached-only: defined-name]"),
    ] {
        let bytes = formula_workbook(&tokens, 42.0);
        let expected_hash = format!("[biff-sha256:{:x}]", Sha256::digest(&tokens));
        for error_policy in [ErrorPolicy::Strict, ErrorPolicy::BestEffort] {
            let options = ConversionOptions { error_policy, ..ConversionOptions::default() };
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let mut plan =
                preflight(&bytes, WORKBOOK, &mut budget(&options, &context), error_policy).unwrap();
            plan.hints = scan_workbook_inventory(
                &bytes,
                BIFF8,
                WORKBOOK,
                &mut budget(&options, &context),
                &context,
                error_policy,
            )
            .unwrap();
            let (wrapper, reservation) = reader_view::prepare_wrapper(
                &bytes,
                &plan,
                false,
                &std::collections::BTreeMap::new(),
                &context,
            )
            .unwrap()
            .unwrap();
            let first =
                crate::workbook::convert_legacy_xls(&wrapper, &plan.hints, &options, &context)
                    .unwrap();
            let second =
                crate::workbook::convert_legacy_xls(&wrapper, &plan.hints, &options, &context)
                    .unwrap();
            assert_eq!(first.document, second.document);
            let cell = cell_text(&table(&first).1[0].cells[0]);
            assert!(cell.starts_with(prefix), "{cell}");
            assert!(cell.contains(&expected_hash));
            assert!(cell.ends_with("[cached: 42]"));
            drop((plan, reservation));
            assert_eq!(context.reserved_memory_bytes(), 0);
        }
        let output = converted(&bytes);
        let degraded = prefix.starts_with('[');
        assert_eq!(
            output.diagnostics.iter().any(|item| item.code == "legacyOffice.xls.formulaCachedOnly"),
            degraded
        );
    }
}
