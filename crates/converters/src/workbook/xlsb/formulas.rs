use crate::workbook::error::{limit, malformed};
use crate::workbook::model::{BinaryFormulaContext, BinaryHyperlink};
use crate::workbook::schema::{MAX_EXCEL_COLUMNS, MAX_EXCEL_ROWS};
use crate::workbook::xlsb::records::{le_u32, xlsb_wide_string};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};

#[allow(clippy::too_many_lines, reason = "BIFF12 token widths are audited in one state machine")]
pub(super) fn validate_xlsb_formula_tokens(
    mut tokens: &[u8],
    formula_context: BinaryFormulaContext,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
    depth: u16,
) -> Result<(), ConversionError> {
    if depth > options.limits.max_nesting_depth {
        return Err(limit("max_nesting_depth", "nested XLSB formula is too deep"));
    }
    if tokens.is_empty() {
        return Ok(());
    }
    let mut stack = 0_usize;
    let mut stack_trusted = true;
    while let Some((&token, rest)) = tokens.split_first() {
        context.checkpoint()?;
        tokens = rest;
        match token {
            0x3a | 0x5a | 0x7a | 0x3c | 0x5c | 0x7c => {
                let body = take_formula_bytes(&mut tokens, 8, part)?;
                if usize::from(u16::from_le_bytes([body[0], body[1]]))
                    >= formula_context.external_sheets
                {
                    return Err(malformed(Some(part), "formula external-sheet index is invalid"));
                }
                stack = stack.saturating_add(1);
            }
            0x3b | 0x5b | 0x7b | 0x3d | 0x5d | 0x7d => {
                let body = take_formula_bytes(&mut tokens, 14, part)?;
                if usize::from(u16::from_le_bytes([body[0], body[1]]))
                    >= formula_context.external_sheets
                {
                    return Err(malformed(Some(part), "formula external-sheet index is invalid"));
                }
                stack = stack.saturating_add(1);
            }
            0x01 | 0x20 | 0x40 | 0x60 => {
                return Err(ConversionError::Unsupported {
                    detail: format!(
                        "unexpanded XLSB array/shared formula token 0x{token:02x} is unsupported ({part})"
                    ),
                });
            }
            0x03..=0x11 => {
                if stack_trusted && stack < 2 {
                    return Err(malformed(Some(part), "formula operand stack underflow"));
                }
                stack = stack.saturating_sub(1);
            }
            0x12..=0x15 => {
                if stack_trusted && stack < 1 {
                    return Err(malformed(Some(part), "formula operand stack underflow"));
                }
            }
            0x16 => stack = stack.saturating_add(1),
            0x17 => {
                let length = take_formula_bytes(&mut tokens, 2, part)?;
                let units = usize::from(u16::from_le_bytes([length[0], length[1]]));
                let _ = take_formula_bytes(
                    &mut tokens,
                    units
                        .checked_mul(2)
                        .ok_or_else(|| malformed(Some(part), "formula string size overflow"))?,
                    part,
                )?;
                stack = stack.saturating_add(1);
            }
            0x18 => {
                let extended = take_formula_bytes(&mut tokens, 1, part)?[0];
                let width = match extended {
                    0x19 => 12,
                    0x1d => 4,
                    _ => return Err(malformed(Some(part), "invalid extended formula token")),
                };
                let _ = take_formula_bytes(&mut tokens, width, part)?;
                stack = stack.saturating_add(1);
            }
            0x19 => {
                let extended = take_formula_bytes(&mut tokens, 1, part)?[0];
                let width = match extended {
                    0x01 | 0x02 | 0x08 | 0x20 | 0x21 | 0x40 | 0x41 | 0x80 | 0x10 => 2,
                    0x04 => 10,
                    _ => return Err(malformed(Some(part), "invalid attribute formula token")),
                };
                let _ = take_formula_bytes(&mut tokens, width, part)?;
                if extended == 0x10 && stack_trusted && stack < 1 {
                    return Err(malformed(Some(part), "formula operand stack underflow"));
                }
            }
            0x1c | 0x1d => {
                let _ = take_formula_bytes(&mut tokens, 1, part)?;
                stack = stack.saturating_add(1);
            }
            0x1e => {
                let _ = take_formula_bytes(&mut tokens, 2, part)?;
                stack = stack.saturating_add(1);
            }
            0x1f => {
                let _ = take_formula_bytes(&mut tokens, 8, part)?;
                stack = stack.saturating_add(1);
            }
            0x22 | 0x42 | 0x62 => {
                let body = take_formula_bytes(&mut tokens, 3, part)?;
                let arguments = usize::from(body[0]);
                let function = usize::from(u16::from_le_bytes([body[1], body[2]]));
                if function >= 485 || stack_trusted && stack < arguments {
                    return Err(malformed(Some(part), "invalid variable formula function"));
                }
                stack = stack.saturating_sub(arguments).saturating_add(1);
            }
            0x21 | 0x41 | 0x61 => {
                let body = take_formula_bytes(&mut tokens, 2, part)?;
                let function = usize::from(u16::from_le_bytes([body[0], body[1]]));
                if function >= 485 {
                    return Err(malformed(Some(part), "invalid fixed formula function"));
                }
                // Calamine's pinned function table determines the fixed arity.
                // Width and table index are authenticated here; any malformed
                // operand stack is converted by the panic boundary without an
                // attacker-controlled allocation (formula capacity is modeled).
                stack_trusted = false;
                stack = stack.saturating_add(1);
            }
            0x23 | 0x43 | 0x63 => {
                let body = take_formula_bytes(&mut tokens, 4, part)?;
                let index = usize::try_from(le_u32(body))
                    .map_err(|_| malformed(Some(part), "formula name index overflow"))?;
                if index == 0 || index > formula_context.defined_names {
                    return Err(malformed(Some(part), "formula name index is invalid"));
                }
                stack = stack.saturating_add(1);
            }
            0x39 | 0x59 | 0x79 => {
                return Err(ConversionError::Unsupported {
                    detail: format!(
                        "external-name formula token 0x{token:02x} is forbidden ({part})"
                    ),
                });
            }
            0x24 | 0x44 | 0x64 | 0x2a | 0x4a | 0x6a => {
                let _ = take_formula_bytes(&mut tokens, 6, part)?;
                stack = stack.saturating_add(1);
            }
            0x25 | 0x45 | 0x65 | 0x2b | 0x4b | 0x6b => {
                let _ = take_formula_bytes(&mut tokens, 12, part)?;
                stack = stack.saturating_add(1);
            }
            0x29 | 0x49 | 0x69 => {
                let length = take_formula_bytes(&mut tokens, 2, part)?;
                let nested_length = usize::from(u16::from_le_bytes([length[0], length[1]]));
                let nested = take_formula_bytes(&mut tokens, nested_length, part)?;
                validate_xlsb_formula_tokens(
                    nested,
                    formula_context,
                    part,
                    options,
                    context,
                    depth.saturating_add(1),
                )?;
                stack = stack.saturating_add(1);
            }
            _ => return Err(malformed(Some(part), format!("invalid formula token 0x{token:02x}"))),
        }
        if u64::try_from(stack).unwrap_or(u64::MAX) > options.limits.max_table_cells {
            return Err(limit("max_table_cells", "XLSB formula stack is too large"));
        }
    }
    if stack_trusted && stack != 1 {
        return Err(malformed(Some(part), "invalid final XLSB formula stack"));
    }
    Ok(())
}

fn take_formula_bytes<'a>(
    tokens: &mut &'a [u8],
    count: usize,
    part: &str,
) -> Result<&'a [u8], ConversionError> {
    let (head, tail) = tokens
        .split_at_checked(count)
        .ok_or_else(|| malformed(Some(part), "truncated XLSB formula token"))?;
    *tokens = tail;
    Ok(head)
}

pub(super) fn parse_binary_hyperlink(
    payload: &[u8],
    part: &str,
) -> Result<BinaryHyperlink, ConversionError> {
    if payload.len() < 32 {
        return Err(malformed(Some(part), "invalid BrtHLink length"));
    }
    let start = (le_u32(&payload[0..4]), le_u32(&payload[8..12]));
    let end = (le_u32(&payload[4..8]), le_u32(&payload[12..16]));
    if start.0 > end.0 || start.1 > end.1 || end.0 >= MAX_EXCEL_ROWS || end.1 >= MAX_EXCEL_COLUMNS {
        return Err(malformed(Some(part), "invalid XLSB hyperlink range"));
    }
    let (relationship_id, offset) = xlsb_wide_string(payload, 16, false, part)?;
    let (location, offset) = xlsb_wide_string(payload, offset, false, part)?;
    let (tooltip, offset) = xlsb_wide_string(payload, offset, false, part)?;
    let (display, offset) = xlsb_wide_string(payload, offset, false, part)?;
    if offset != payload.len() {
        return Err(malformed(Some(part), "trailing BrtHLink bytes"));
    }
    Ok(BinaryHyperlink {
        start,
        end,
        relationship_id: (!relationship_id.is_empty()).then_some(relationship_id),
        location,
        tooltip,
        display,
    })
}
