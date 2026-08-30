use super::*;
use into_markdown_core::{ConversionOptions, ExecutionContext, ExecutionOptions};

fn expression(tokens: &[u8]) -> String {
    let mut expression = Expression::default();
    parse(tokens, true, &References::default(), &mut expression).unwrap();
    expression.render().unwrap()
}

fn integer(value: u16) -> Vec<u8> {
    let mut output = vec![0x1e];
    output.extend_from_slice(&value.to_le_bytes());
    output
}

fn reference(row: u16, column: u16) -> Vec<u8> {
    let mut output = vec![0x24];
    output.extend_from_slice(&row.to_le_bytes());
    output.extend_from_slice(&column.to_le_bytes());
    output
}

#[test]
fn comparison_column_and_area_tokens_preserve_their_original_meaning() {
    for (token, operator) in [(0x0c, ">="), (0x0d, ">"), (0x09, "<"), (0x0a, "<=")] {
        assert_eq!(
            expression(&[reference(22, 0xc005), integer(0), vec![token]].concat()),
            format!("F23{operator}0")
        );
    }
    for (column, name) in [(25, "Z"), (26, "AA"), (27, "AB"), (28, "AC"), (29, "AD"), (255, "IV")] {
        for (flags, expected) in [
            (0, format!("${name}$29")),
            (0x4000, format!("{name}$29")),
            (0x8000, format!("${name}29")),
            (0xc000, format!("{name}29")),
        ] {
            assert_eq!(expression(&reference(28, column | flags)), expected);
        }
    }
    for (flags, expected) in
        [(0xc0, "A1:B2"), (0x00, "$A$1:$B$2"), (0x40, "A$1:B$2"), (0x80, "$A1:$B2")]
    {
        assert_eq!(expression(&[0x25, 0, 0, 1, 0, 0, flags, 1, flags]), expected);
    }
}

#[test]
fn precedence_and_associativity_are_preserved_without_recursive_rendering() {
    let operands = [integer(1), integer(2), integer(3)].concat();
    for (operators, expected) in [
        ([0x04, 0x04], "1-(2-3)"),
        ([0x06, 0x06], "1/(2/3)"),
        ([0x03, 0x05], "1*(2+3)"),
        ([0x0d, 0x0b], "1=(2>3)"),
        ([0x07, 0x07], "1^(2^3)"),
    ] {
        assert_eq!(expression(&[operands.clone(), operators.to_vec()].concat()), expected);
    }
    assert_eq!(
        expression(&[integer(1), integer(2), vec![0x04], integer(3), vec![0x04]].concat()),
        "1-2-3"
    );
    assert_eq!(expression(&[integer(1), integer(2), vec![0x03, 0x13]].concat()), "-(1+2)");
    let mut long = integer(0);
    for _ in 0..8_000 {
        long.extend_from_slice(&[0x1e, 1, 0, 0x03]);
    }
    assert_eq!(expression(&long), format!("0{}", "+1".repeat(8_000)));
}

#[test]
fn common_functions_and_unicode_literals_remain_decoded() {
    let area = vec![0x25, 0, 0, 1, 0, 0, 0xc0, 1, 0xc0];
    assert_eq!(
        expression(&[area, integer(1), integer(1), vec![0x22, 3, 29, 0]].concat()),
        "INDEX(A1:B2,1,1)"
    );
    assert_eq!(expression(&[integer(1), integer(2), vec![0x22, 2, 4, 0]].concat()), "SUM(1,2)");
    assert_eq!(expression(&[0x17, 2, 1, 0x22, 0, 0x2d, 0x4e]), "\"\"\"中\"");
}

#[test]
fn local_3d_requires_complete_supbook_identity_and_uses_the_same_ref_flags() {
    let mut references = References::default();
    references.add_sheet("Data's");
    references.record(0x01ae, &[1, 0, 1, 4]);
    references.record(0x01ae, &[1, 0, 1, 0, b'x']);
    references.record(0x0017, &[2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
    let mut value = Expression::default();
    parse(&[0x5a, 0, 0, 5, 0, 6, 0xc0], true, &references, &mut value).unwrap();
    assert_eq!(value.render().unwrap(), "'Data''s'!G6");
    for (index, reason) in [(1, "external-reference"), (2, "unknown-xti")] {
        assert_eq!(
            parse(&[0x5a, index, 0, 5, 0, 6, 0xc0], true, &references, &mut Expression::default()),
            Err(reason)
        );
    }
}

#[test]
fn unsupported_and_truncated_tokens_keep_exact_evidence_and_release_all_leases() {
    let options = ConversionOptions::default();
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    for tokens in [
        &[0x20][..],
        &[0x23, 1, 0, 0, 0],
        &[0x24, 0],
        &[0x01, 0, 0, 0, 0],
        &[0x03],
        &[0x16],
        &[0x1e, 1, 0, 0x16, 0x03],
    ] {
        let mut budget = LegacyBudget::new(100, &options, &context).unwrap();
        let mut retained = context.reserve_memory(0).unwrap();
        let result =
            decode(tokens, BIFF8, &References::default(), &mut budget, &mut retained).unwrap();
        let LegacyFormulaValue::CachedOnly { tokens: actual, .. } = result else { panic!() };
        assert_eq!(actual, tokens);
        drop(retained);
        assert_eq!(context.reserved_memory_bytes(), 0);
    }
}

#[test]
fn legacy_ref_flags_and_resource_failures_remain_bounded() {
    for (bytes, expected) in
        [(vec![0x24, 28, 0xc0, 26], "AA29"), (vec![0x25, 0, 0, 1, 0xc0, 0, 1], "$A$1:B2")]
    {
        let mut value = Expression::default();
        parse(&bytes, false, &References::default(), &mut value).unwrap();
        assert_eq!(value.render().unwrap(), expected);
    }
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = 128;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut retained = context.reserve_memory(0).unwrap();
    let mut budget = LegacyBudget::new(100, &options, &context).unwrap();
    assert!(matches!(
        decode(&integer(1), BIFF8, &References::default(), &mut budget, &mut retained),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    drop(retained);
    assert_eq!(context.reserved_memory_bytes(), 0);
}
