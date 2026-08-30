use super::*;
use into_markdown_core::{ConversionOptions, ExecutionContext, ExecutionOptions};

fn expression(tokens: &[u8]) -> String {
    let mut expression = Expression::default();
    parse(tokens, true, &References::default(), 0, &mut expression).unwrap();
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
    parse(&[0x5a, 0, 0, 5, 0, 6, 0xc0], true, &references, 0, &mut value).unwrap();
    assert_eq!(value.render().unwrap(), "'Data''s'!G6");
    for (index, reason) in [(1, "external-reference"), (2, "unknown-xti")] {
        assert_eq!(
            parse(
                &[0x5a, index, 0, 5, 0, 6, 0xc0],
                true,
                &references,
                0,
                &mut Expression::default()
            ),
            Err(reason)
        );
    }
}

#[test]
fn local_supbook_undefined_count_does_not_override_authenticated_sheet_indices() {
    for count in [0, 1, u16::MAX] {
        let mut references = References::default();
        references.add_sheet("Local");
        let [low, high] = count.to_le_bytes();
        references.record(0x01ae, &[low, high, 1, 4]);
        references.record(0x0017, &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0]);
        let mut value = Expression::default();
        parse(&[0x5a, 0, 0, 0, 0, 0, 0xc0], true, &references, 0, &mut value).unwrap();
        assert_eq!(value.render().unwrap(), "'Local'!A1");
        assert_eq!(references.sheet_prefix(1), Err("invalid-local-reference"));
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
            decode(tokens, BIFF8, &References::default(), 0, &mut budget, &mut retained).unwrap();
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
        parse(&bytes, false, &References::default(), 0, &mut value).unwrap();
        assert_eq!(value.render().unwrap(), expected);
    }
    let mut options = ConversionOptions::default();
    options.limits.max_memory_bytes = 128;
    let context = ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
    let mut retained = context.reserve_memory(0).unwrap();
    let mut budget = LegacyBudget::new(100, &options, &context).unwrap();
    assert!(matches!(
        decode(&integer(1), BIFF8, &References::default(), 0, &mut budget, &mut retained),
        Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
    ));
    drop(retained);
    assert_eq!(context.reserved_memory_bytes(), 0);
}

fn label(name: &str, scope: u16, flags: u16) -> Vec<u8> {
    let units = name.encode_utf16().collect::<Vec<_>>();
    let mut body = vec![0; 14];
    body[0..2].copy_from_slice(&flags.to_le_bytes());
    body[3] = u8::try_from(units.len()).unwrap();
    body[4..6].copy_from_slice(&3_u16.to_le_bytes());
    body[8..10].copy_from_slice(&scope.to_le_bytes());
    body.push(1);
    for unit in units {
        body.extend_from_slice(&unit.to_le_bytes());
    }
    body.extend_from_slice(&[0x1e, 1, 0]);
    body
}

#[test]
fn ordinary_names_preserve_identifiers_and_scope_without_expanding_definitions() {
    let mut references = References::default();
    references.add_sheet("First");
    references.add_sheet("Other's");
    for name in ["A", "B", "threeByThree"] {
        references.record(0x0018, &label(name, 0, 0));
    }
    let mut value = Expression::default();
    parse(&[0x43, 1, 0, 0, 0, 0x43, 2, 0, 0, 0, 0x03], true, &references, 0, &mut value).unwrap();
    assert_eq!(value.render().unwrap(), "A+B");
    let mut value = Expression::default();
    parse(
        &[0x23, 3, 0, 0, 0, 0x44, 4, 0, 4, 0xc0, 0x44, 4, 0, 5, 0xc0, 0x42, 3, 29, 0],
        true,
        &references,
        0,
        &mut value,
    )
    .unwrap();
    assert_eq!(value.render().unwrap(), "INDEX(threeByThree,E5,F5)");
    references.record(0x0018, &label("MyTestPackName", 2, 0));
    assert_eq!(references.defined_name(4, 0).unwrap(), "'Other''s'!MyTestPackName");
    assert_eq!(references.defined_name(4, 1).unwrap(), "MyTestPackName");
    references.record(0x0018, &label("A", 1, 0));
    assert_eq!(references.defined_name(1, 0), Err("shadowed-global-name"));
    assert_eq!(references.defined_name(5, 0).unwrap(), "A");
    assert_eq!(references.defined_name(1, 1).unwrap(), "A");
    references.record(0x0018, &label("On2", 0, 0));
    assert_eq!(references.defined_name(6, 0).unwrap(), "On2");
}

#[test]
fn unsupported_or_ambiguous_name_records_do_not_shift_ordinals_or_create_formula_text() {
    let mut references = References::default();
    references.add_sheet("First");
    for (name, flags, scope) in [
        ("Macro", 0x0a, 0),
        ("Print_Area", 0x20, 0),
        ("Valid", 0, 0),
        ("A+B", 0, 0),
        ("R1C1", 0, 0),
        ("Far", 0, 2),
    ] {
        references.record(0x0018, &label(name, scope, flags));
    }
    assert_eq!(references.defined_name(1, 0), Err("macro-defined-name"));
    assert_eq!(references.defined_name(2, 0), Err("builtin-defined-name"));
    assert_eq!(references.defined_name(3, 0).unwrap(), "Valid");
    for index in [4, 5] {
        assert_eq!(references.defined_name(index, 0), Err("unsupported-defined-name-identifier"));
    }
    assert_eq!(references.defined_name(6, 0), Err("invalid-defined-name-scope"));
    references.record(0x0018, &label("VALID", 0, 0));
    assert_eq!(references.defined_name(3, 0), Err("ambiguous-defined-name"));
    assert_eq!(references.defined_name(7, 0), Err("ambiguous-defined-name"));
    assert_eq!(references.defined_name(0, 0), Err("unknown-defined-name"));
    assert_eq!(references.defined_name(u32::MAX, 0), Err("unknown-defined-name"));
    references.record(0x0018, &[0; 4]);
    assert_eq!(references.defined_name(3, 0), Err("invalid-defined-name-metadata"));
}
