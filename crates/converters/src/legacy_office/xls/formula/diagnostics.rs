use crate::workbook::{LegacyFormulaValue, LegacyXlsHints};
use into_markdown_core::{ConverterOutput, Diagnostic, DiagnosticSeverity};
use std::collections::BTreeMap;

pub(in crate::legacy_office::xls) fn append_diagnostics(
    output: &mut ConverterOutput,
    hints: &LegacyXlsHints,
    part: &str,
) {
    // One diagnostic per reason, with a first-cell locator. Every affected cell also
    // carries its reason and exact tokens, so large shared-formula sheets stay bounded.
    let mut reasons = BTreeMap::new();
    for formula in &hints.formula_expressions {
        if let LegacyFormulaValue::CachedOnly { reason, .. } = &formula.value {
            let entry = reasons.entry(*reason).or_insert((0_u64, formula));
            entry.0 += 1;
        }
    }
    for (reason, (count, first)) in reasons {
        let mut locator = crate::legacy_office::builder::locator(part);
        locator.sheet = output
            .document
            .metadata
            .properties
            .get(&format!("spreadsheet.sheet.{}.name", first.sheet_index))
            .cloned();
        locator.cell = Some(into_markdown_core::CellRef { row: first.row, column: first.column });
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.xls.formulaCachedOnly".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!("{count} formula(s) retained as cached-only ({reason}); original BIFF tokens and SHA-256 are preserved in each cell, without formula evaluation or parser-text fallback"),
            locator: Some(locator),
        });
    }
}
