use crate::legacy_office::budget::{LegacyBudget, malformed};
use crate::legacy_office::builder::locator;
use into_markdown_core::{
    Block, BlockNode, ConversionError, ConverterOutput, Diagnostic, DiagnosticSeverity,
    ErrorPolicy, Inline,
};

pub(super) fn normalize(
    output: &mut ConverterOutput,
    budget: &LegacyBudget<'_>,
) -> Result<(), ConversionError> {
    if normalize_blocks(&mut output.document.blocks) {
        if budget.error_policy() == ErrorPolicy::Strict {
            return Err(malformed("WordDocument/fields", "hyperlink uses an unsafe target"));
        }
        output.diagnostics.push(Diagnostic {
            code: "legacyOffice.doc.unsafeHyperlinkOmitted".into(),
            severity: DiagnosticSeverity::Warning,
            message: "unsafe hyperlink targets were removed while retaining their display text"
                .into(),
            locator: Some(locator("WordDocument/fields")),
        });
    }
    Ok(())
}

fn normalize_blocks(nodes: &mut [BlockNode]) -> bool {
    let mut omitted = false;
    for node in nodes {
        omitted |= match &mut node.block {
            Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
                normalize_inlines(inlines)
            }
            Block::Table { rows, .. } => rows
                .iter_mut()
                .flat_map(|row| &mut row.cells)
                .fold(false, |changed, cell| normalize_blocks(&mut cell.blocks) | changed),
            Block::List { items, .. } => items
                .iter_mut()
                .fold(false, |changed, item| normalize_blocks(&mut item.blocks) | changed),
            Block::Footnote { blocks, .. } => normalize_blocks(blocks),
            _ => false,
        };
    }
    omitted
}

fn normalize_inlines(inlines: &mut Vec<Inline>) -> bool {
    let mut omitted = false;
    let mut normalized = Vec::with_capacity(inlines.len());
    for mut inline in std::mem::take(inlines) {
        if let Inline::Link { target, content } = &mut inline {
            omitted |= normalize_inlines(content);
            if !safe_target(target) {
                omitted = true;
                normalized.append(content);
                continue;
            }
        }
        normalized.push(inline);
    }
    *inlines = normalized;
    omitted
}

fn safe_target(value: &str) -> bool {
    if value.chars().any(char::is_control) || contains_entity(value) {
        return false;
    }
    let Some(colon) = value.find(':') else {
        return true;
    };
    let scheme = &value[..colon];
    if scheme.is_empty()
        || !scheme.bytes().enumerate().all(|(i, byte)| {
            byte.is_ascii_alphabetic() || i > 0 && matches!(byte, b'+' | b'-' | b'.')
        })
    {
        return true;
    }
    if matches!(scheme.to_ascii_lowercase().as_str(), "javascript" | "vbscript" | "data" | "file") {
        return false;
    }
    !value[colon + 1..]
        .strip_prefix("//")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|authority| authority.contains('@'))
}

fn contains_entity(value: &str) -> bool {
    value.split('&').skip(1).any(|tail| {
        let Some((body, _)) = tail.split_once(';') else {
            return false;
        };
        if body.is_empty() || body.len() > 32 {
            return false;
        }
        body.bytes().all(|byte| byte.is_ascii_alphanumeric())
            || body.strip_prefix('#').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
            || body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")).is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy_office::builder::OutputBuilder;

    #[test]
    fn unsafe_links_keep_display_text_and_safe_query_links() {
        for target in [
            "https://example.test/\u{1}",
            "javascript:alert(1)",
            "https://user@example.test",
            "java&#115;cript:x",
        ] {
            let mut inlines = vec![Inline::Link {
                target: target.into(),
                content: vec![OutputBuilder::text("shown")],
            }];
            assert!(normalize_inlines(&mut inlines));
            assert_eq!(inlines, vec![OutputBuilder::text("shown")]);
        }
        assert!(safe_target("https://example.test/?a=1&b=2"));
    }

    #[test]
    fn smart_quote_control_field_is_degraded_in_best_effort_and_rejected_in_strict() {
        use into_markdown_core::{ConversionOptions, ExecutionContext, ExecutionOptions};
        for policy in [ErrorPolicy::BestEffort, ErrorPolicy::Strict] {
            let options =
                ConversionOptions { error_policy: policy, ..ConversionOptions::default() };
            let context =
                ExecutionContext::new(ExecutionOptions::default(), options.limits.clone());
            let budget = LegacyBudget::new(128, &options, &context).unwrap();
            let mut builder = OutputBuilder::new("doc");
            let value = "\u{13} HYPERLINK \"https://example.test/path” \u{1}\u{14}shown\u{15}";
            builder.push(
                Block::Paragraph(super::super::field_inlines(value)),
                locator("WordDocument"),
            );
            let mut output = builder.finish();
            let result = normalize(&mut output, &budget);
            if policy == ErrorPolicy::Strict {
                assert!(matches!(result, Err(ConversionError::Malformed { .. })));
            } else {
                result.unwrap();
                assert_eq!(
                    output.document.blocks[0].block,
                    Block::Paragraph(vec![OutputBuilder::text("shown")])
                );
                assert_eq!(output.diagnostics[0].code, "legacyOffice.doc.unsafeHyperlinkOmitted");
            }
        }
    }
}
