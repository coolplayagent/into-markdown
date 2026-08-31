//! Context-aware inline serialization.

use super::{
    BTreeSet, ConversionError, Inline, InlineContext, InlineMark, escape_destination,
    escape_html_attribute, escape_html_code, escape_html_text, escape_text, footnote_label,
    normalize_lf, render_code_span, render_error, single_line, validate_link_target,
};

fn text_parts(inline: &Inline) -> Option<(&str, &[InlineMark])> {
    match inline {
        Inline::Text { value, marks }
        | Inline::SourceText { value, marks, .. }
        | Inline::OcrText { value, marks, .. } => Some((value, marks)),
        _ => None,
    }
}

fn same_marks(left: &[InlineMark], right: &[InlineMark]) -> bool {
    left.len() == right.len() && left.iter().all(|mark| right.contains(mark))
}

pub(super) fn render_inlines(
    inlines: &[Inline],
    context: InlineContext,
) -> Result<String, ConversionError> {
    let mut output = String::new();
    let mut index = 0;
    while let Some(inline) = inlines.get(index) {
        if let Some((value, marks)) = text_parts(inline) {
            let mut value = value.to_owned();
            index += 1;
            while let Some((next, next_marks)) = inlines.get(index).and_then(text_parts) {
                if !same_marks(marks, next_marks) {
                    break;
                }
                value.push_str(next);
                index += 1;
            }
            let next =
                inlines.get(index).and_then(text_parts).and_then(|(value, _)| value.chars().next());
            output.push_str(&render_marked_text(
                &value,
                marks,
                context,
                output.chars().next_back(),
                next,
            ));
            continue;
        }
        match inline {
            Inline::Code(value) => output.push_str(&render_code_span(value, context)),
            Inline::Link { target, content } => {
                validate_link_target(target)?;
                output.push('[');
                output.push_str(&render_inlines(content, context)?);
                output.push_str("](<");
                output.push_str(&escape_destination(target, context));
                output.push_str(">)");
            }
            Inline::Formula(value) => {
                output.push('$');
                output.push_str(&render_code_span(value, context));
                output.push('$');
            }
            Inline::FootnoteReference(label) => {
                output.push_str("[^");
                output.push_str(&footnote_label(label));
                output.push(']');
            }
            Inline::LineBreak => output.push_str(match context {
                InlineContext::Normal => "  \n",
                InlineContext::TableCell => "<br>",
            }),
            _ => {
                return Err(render_error("document contains an unsupported future inline variant"));
            }
        }
        index += 1;
    }
    Ok(output)
}

// Native delimiters can border word characters freely. At punctuation edges we
// require a known separating boundary; uncertain Unicode categories use HTML.
fn separating(character: Option<char>) -> bool {
    character.is_none_or(|value| value.is_whitespace() || value.is_ascii_punctuation())
}

fn native_boundaries(value: &str, previous: Option<char>, next: Option<char>) -> bool {
    let first = value.chars().next();
    let last = value.chars().next_back();
    first.is_some_and(|value| value.is_alphanumeric() || separating(previous))
        && last.is_some_and(|value| value.is_alphanumeric() || separating(next))
        && !matches!(previous, Some('*' | '~'))
}

fn render_marked_text(
    value: &str,
    marks: &[InlineMark],
    context: InlineContext,
    previous: Option<char>,
    next: Option<char>,
) -> String {
    let value = single_line(value);
    let trimmed = value.trim();
    if trimmed.is_empty() || marks.is_empty() {
        return escape_text(&value, context);
    }
    let leading = &value[..value.len() - value.trim_start().len()];
    let trailing = &value[value.trim_end().len()..];
    let previous = leading.chars().next_back().or(previous);
    let next = trailing.chars().next().or(next);
    let html_marks = (marks.contains(&InlineMark::Strikethrough) && marks.len() > 1)
        || marks.iter().any(|mark| {
            matches!(mark, InlineMark::Underline | InlineMark::Superscript | InlineMark::Subscript)
        });
    let native = native_boundaries(trimmed, previous, next)
        && (!html_marks || (separating(previous) && separating(next)));
    let text = escape_text(if native { trimmed } else { &value }, context);
    let wrappers = [
        InlineMark::Subscript,
        InlineMark::Superscript,
        InlineMark::Underline,
        InlineMark::Italic,
        InlineMark::Bold,
        InlineMark::Strikethrough,
    ]
    .map(|mark| {
        if !marks.contains(&mark) {
            return ("", "");
        }
        match (mark, native) {
            (InlineMark::Bold, true) => ("**", "**"),
            (InlineMark::Italic, true) => ("*", "*"),
            (InlineMark::Strikethrough, true) => ("~~", "~~"),
            (InlineMark::Bold, false) => ("<strong>", "</strong>"),
            (InlineMark::Italic, false) => ("<em>", "</em>"),
            (InlineMark::Strikethrough, false) => ("<del>", "</del>"),
            (InlineMark::Underline, _) => ("<u>", "</u>"),
            (InlineMark::Superscript, _) => ("<sup>", "</sup>"),
            (InlineMark::Subscript, _) => ("<sub>", "</sub>"),
        }
    });
    let mut rendered = String::with_capacity(text.len() + 78 + leading.len() + trailing.len());
    if native {
        rendered.push_str(leading);
    }
    for (open, _) in wrappers.iter().rev() {
        rendered.push_str(open);
    }
    rendered.push_str(&text);
    for (_, close) in wrappers {
        rendered.push_str(close);
    }
    if native {
        rendered.push_str(trailing);
    }
    rendered
}

// Office paragraph indentation is text, while Markdown indentation can start code.
pub(super) fn protect_paragraph_indent(mut rendered: String) -> String {
    let leading = rendered.chars().take_while(|c| matches!(c, ' ' | '\t'));
    let (count, tab) =
        leading.fold((0_usize, false), |(count, tab), c| (count + 1, tab || c == '\t'));
    if !rendered.trim().is_empty() && (tab || count >= 4) {
        let entity = if rendered.starts_with('\t') { "&#9;" } else { "&#32;" };
        rendered.replace_range(..1, entity);
    }
    rendered
}

pub(super) fn bold_cell(rendered: &str) -> String {
    if rendered.trim().is_empty() {
        return rendered.to_owned();
    }
    // Already-marked cell content can end in an emphasis delimiter. HTML keeps
    // these nested runs independent while ordinary headers stay readable.
    if rendered.contains('*') || rendered.starts_with('<') || rendered.ends_with('>') {
        format!("<strong>{rendered}</strong>")
    } else {
        format!("**{}**", rendered.trim())
    }
}

pub(super) fn render_html_inlines(inlines: &[Inline]) -> Result<String, ConversionError> {
    let mut output = String::new();
    for inline in inlines {
        match inline {
            Inline::Text { value, marks }
            | Inline::SourceText { value, marks, .. }
            | Inline::OcrText { value, marks, .. } => {
                let mut rendered = escape_html_text(&normalize_lf(value)).replace('\n', "<br>");
                let marks = marks.iter().copied().collect::<BTreeSet<_>>();
                for mark in [
                    InlineMark::Subscript,
                    InlineMark::Superscript,
                    InlineMark::Underline,
                    InlineMark::Strikethrough,
                    InlineMark::Italic,
                    InlineMark::Bold,
                ] {
                    if marks.contains(&mark) {
                        let tag = match mark {
                            InlineMark::Bold => "strong",
                            InlineMark::Italic => "em",
                            InlineMark::Strikethrough => "del",
                            InlineMark::Underline => "u",
                            InlineMark::Superscript => "sup",
                            InlineMark::Subscript => "sub",
                        };
                        rendered = format!("<{tag}>{rendered}</{tag}>");
                    }
                }
                output.push_str(&rendered);
            }
            Inline::Code(value) | Inline::Formula(value) => {
                output.push_str("<code>");
                output.push_str(&escape_html_code(value, InlineContext::Normal));
                output.push_str("</code>");
            }
            Inline::Link { target, content } => {
                validate_link_target(target)?;
                output.push_str("<a href=\"");
                output.push_str(&escape_html_attribute(target));
                output.push_str("\">");
                output.push_str(&render_html_inlines(content)?);
                output.push_str("</a>");
            }
            Inline::FootnoteReference(label) => {
                output.push_str("[^");
                output.push_str(&escape_html_text(label));
                output.push(']');
            }
            Inline::LineBreak => output.push_str("<br>"),
            _ => {
                return Err(render_error("document contains an unsupported future inline variant"));
            }
        }
    }
    Ok(output)
}
