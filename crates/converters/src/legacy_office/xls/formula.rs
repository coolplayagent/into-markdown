//! Faithful decoding of a bounded BIFF formula subset from authenticated original tokens.
//! Unknown constructs retain cached data and token evidence; calamine text is never a fallback.

mod diagnostics;
mod expression;
mod functions;
mod names;
mod reader;
mod references;

use super::{BIFF8, LegacyBudget, limit};
use crate::msg::ole::CompoundBudget as _;
use crate::workbook::LegacyFormulaValue;
pub(super) use diagnostics::append_diagnostics;
use expression::Expression;
use into_markdown_core::{ConversionError, ResourceReservation};
use reader::{Result as TokenResult, Tokens};
pub(super) use references::References;

pub(super) fn decode(
    tokens: &[u8],
    version: u16,
    references: &References,
    sheet_index: usize,
    budget: &mut LegacyBudget<'_>,
    retained: &mut ResourceReservation,
) -> Result<LegacyFormulaValue, ConversionError> {
    // Includes nodes, edges, stacks, literals and the iterative rendering stack.
    let scratch_bytes = u64::try_from(tokens.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(192)
        .saturating_add(512)
        .saturating_add(references.expansion_bytes(tokens.len()));
    let _scratch = budget.cfb_memory(scratch_bytes)?;
    budget.work(u64::try_from(tokens.len()).unwrap_or(u64::MAX), "XLS formula tokens")?;
    let mut expression = Expression::default();
    let decoded = parse(tokens, version == BIFF8, references, sheet_index, &mut expression);
    let capacity =
        if decoded.is_ok() { expression.capacity().max(tokens.len()) } else { tokens.len() };
    if u64::try_from(capacity).unwrap_or(u64::MAX) > budget.max_field_bytes() {
        return Err(limit("max_field_bytes", "XLS formula exceeds field limit"));
    }
    retained.grow(u64::try_from(capacity).unwrap_or(u64::MAX))?;
    let result = decoded.and_then(|()| expression.render());
    budget.checkpoint()?;
    Ok(match result {
        Ok(value) => LegacyFormulaValue::Decoded(value),
        Err(reason) => LegacyFormulaValue::CachedOnly { reason, tokens: tokens.to_vec() },
    })
}

fn parse(
    bytes: &[u8],
    biff8: bool,
    references: &References,
    sheet_index: usize,
    expression: &mut Expression,
) -> TokenResult<()> {
    let mut input = Tokens::new(bytes);
    while input.remaining() != 0 {
        let token = input.byte()?;
        // Only class variants of operand tokens are equivalent. Do not mask unknown high bits.
        let kind = if (0x20..=0x7d).contains(&token) { (token & 0x1f) | 0x20 } else { token };
        match kind {
            0x03..=0x11 => expression.binary(kind)?,
            0x12..=0x15 => expression.unary(kind)?,
            0x16 => expression.atom(String::new()),
            0x17 => expression.atom(input.string(biff8)?),
            0x19 => attribute(&mut input, expression)?,
            0x1c => expression.atom(error(input.byte()?)?.into()),
            0x1d => expression.atom(
                match input.byte()? {
                    0 => "FALSE",
                    1 => "TRUE",
                    _ => return Err("invalid-boolean-token"),
                }
                .into(),
            ),
            0x1e => expression.atom(input.word()?.to_string()),
            0x1f => {
                let bytes = input.take(8)?.try_into().map_err(|_| "truncated-number")?;
                let value = f64::from_le_bytes(bytes);
                if !value.is_finite() {
                    return Err("nonfinite-number-token");
                }
                expression.atom(value.to_string());
            }
            0x21 | 0x22 => {
                let arguments = if kind == 0x22 { Some(input.byte()?) } else { None };
                let (name, count) = functions::function(input.word()?, arguments)?;
                expression.call(name, count)?;
            }
            0x24 => expression.atom(references::reference(&mut input, biff8)?),
            0x23 if biff8 => expression.atom(references.defined_name(input.dword()?, sheet_index)?),
            0x25 => expression.atom(references::area(&mut input, biff8)?),
            0x2a | 0x2b => {
                let address_bytes = if biff8 { 4 } else { 3 };
                input.take(if kind == 0x2b { address_bytes * 2 } else { address_bytes })?;
                expression.atom("#REF!".into());
            }
            0x3a | 0x3b if biff8 => {
                let prefix = references.sheet_prefix(input.word()?)?;
                let address = if kind == 0x3a {
                    references::reference(&mut input, biff8)?
                } else {
                    references::area(&mut input, biff8)?
                };
                expression.atom(format!("{prefix}{address}"));
            }
            0x01 => return Err("shared-or-array-formula"),
            0x02 => return Err("data-table-formula"),
            0x20 => return Err("array-constant"),
            0x23 => return Err("legacy-defined-name"),
            0x39 => return Err("external-defined-name"),
            0x3a..=0x3d => return Err("unsupported-3d-reference"),
            0x2c | 0x2d => return Err("relative-shared-reference"),
            _ => return Err("unsupported-token"),
        }
    }
    Ok(())
}

fn attribute(input: &mut Tokens<'_>, expression: &mut Expression) -> TokenResult<()> {
    let kind = input.byte()?;
    let data = input.word()?;
    match kind {
        // Optimization/control hints do not change the RPN expression tree.
        0x01 | 0x20 | 0x21 | 0x40 | 0x41 => Ok(()),
        0x02 | 0x08 if usize::from(data) <= input.remaining().saturating_add(1) => Ok(()),
        0x04 => {
            // CHOOSE jump-table hints are not followed; each branch remains in RPN order.
            input.take((usize::from(data) + 1) * 2)?;
            Ok(())
        }
        0x10 => expression.call("SUM", 1),
        _ => Err("unsupported-formula-attribute"),
    }
}

fn error(code: u8) -> TokenResult<&'static str> {
    match code {
        0x00 => Ok("#NULL!"),
        0x07 => Ok("#DIV/0!"),
        0x0f => Ok("#VALUE!"),
        0x17 => Ok("#REF!"),
        0x1d => Ok("#NAME?"),
        0x24 => Ok("#NUM!"),
        0x2a => Ok("#N/A"),
        _ => Err("unsupported-error-token"),
    }
}

#[cfg(test)]
mod tests;
