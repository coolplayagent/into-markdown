//! BIFF12 record scanners.

mod formulas;
pub(super) mod merges;
pub(super) mod records;
pub(super) mod sheet;
pub(super) mod tables;
pub(super) mod workbook;

#[cfg(test)]
pub(super) fn validate_xlsb_formula_tokens_for_test(
    tokens: &[u8],
    formula_context: crate::workbook::model::BinaryFormulaContext,
    part: &str,
    options: &into_markdown_core::ConversionOptions,
    context: &into_markdown_core::ExecutionContext,
    depth: u16,
) -> Result<(), into_markdown_core::ConversionError> {
    formulas::validate_xlsb_formula_tokens(tokens, formula_context, part, options, context, depth)
}
