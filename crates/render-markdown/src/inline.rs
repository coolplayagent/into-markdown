//! Context-aware inline serialization.

use super::{
    BTreeSet, ConversionError, Inline, InlineContext, InlineMark, escape_destination,
    escape_html_attribute, escape_html_code, escape_html_text, footnote_label, normalize_lf,
    render_code_span, render_error, validate_link_target,
};

use super::escaping::{escape_run, protect_heading};

pub(super) fn text_parts(inline: &Inline) -> Option<(&str, &[InlineMark])> {
    match inline {
        Inline::Text { value, marks }
        | Inline::SourceText { value, marks, .. }
        | Inline::OcrText { value, marks, .. } => Some((value, marks)),
        _ => None,
    }
}

pub(super) fn same_marks(left: &[InlineMark], right: &[InlineMark]) -> bool {
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
                output.push_str(&render_inlines(content, context.label())?);
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
            Inline::LineBreak => {
                // Preserve edge/adjacent breaks that Markdown consumes as blank
                // lines or the end of a paragraph/link label.
                let html = context.table_cell
                    || context.heading
                    || context.link_label
                    || output.is_empty()
                    || inlines.get(index + 1).is_none_or(|next| matches!(next, Inline::LineBreak))
                    || index
                        .checked_sub(1)
                        .and_then(|i| inlines.get(i))
                        .is_some_and(|previous| matches!(previous, Inline::LineBreak));
                output.push_str(if html { "<br>" } else { "  \n" });
            }
            _ => {
                return Err(render_error("document contains an unsupported future inline variant"));
            }
        }
        index += 1;
    }
    Ok(if context.heading && !context.link_label { protect_heading(output) } else { output })
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
    let value = normalize_lf(value);
    // Close style delimiters at paragraph boundaries and reopen them afterwards.
    let mut output = String::new();
    let mut remaining = value.as_str();
    let mut previous = previous;
    while let Some((start, end)) = paragraph_boundary(remaining) {
        output.push_str(&render_marked_segment(
            &remaining[..start],
            marks,
            context,
            previous,
            None,
        ));
        let separator = &remaining[start..end];
        if context.table_cell || context.heading || context.link_label {
            output.push_str(&separator.replace('\n', "<br>"));
        } else {
            output.push_str(separator);
        }
        remaining = &remaining[end..];
        previous = None;
    }
    output.push_str(&render_marked_segment(remaining, marks, context, previous, next));
    output
}

fn paragraph_boundary(value: &str) -> Option<(usize, usize)> {
    let mut start = None;
    for (index, byte) in value.bytes().enumerate() {
        match byte {
            b'\n' => {
                if let Some(start) = start {
                    return Some((start, index + 1));
                }
                start = Some(index);
            }
            b' ' | b'\t' => {}
            _ => start = None,
        }
    }
    None
}

fn render_marked_segment(
    value: &str,
    marks: &[InlineMark],
    context: InlineContext,
    previous: Option<char>,
    next: Option<char>,
) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() || marks.is_empty() {
        return escape_run(value, context, previous, next);
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
    let text = escape_run(if native { trimmed } else { value }, context, previous, next);
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
        rendered.push_str(&escape_run(leading, context, previous, trimmed.chars().next()));
    }
    for (open, _) in wrappers.iter().rev() {
        rendered.push_str(open);
    }
    rendered.push_str(&text);
    for (_, close) in wrappers {
        rendered.push_str(close);
    }
    if native {
        rendered.push_str(&escape_run(trailing, context, trimmed.chars().next_back(), next));
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
                output.push_str(&escape_html_code(value, InlineContext::NORMAL));
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
