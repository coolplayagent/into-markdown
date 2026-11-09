//! Literal text escaping and block-boundary protection.

use super::InlineContext;

pub(super) fn escape_text(value: &str, context: InlineContext) -> String {
    escape_run(value, context, None, None)
}

pub(super) fn escape_run(
    value: &str,
    context: InlineContext,
    previous: Option<char>,
    next: Option<char>,
) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    let mut previous = previous;
    while let Some(character) = characters.next() {
        let following = characters.peek().copied().or(next);
        match character {
            '&' => output.push_str("&amp;"),
            '\n' if context.table_cell || context.heading => output.push_str("<br>"),
            // A text newline carries soft-break semantics, including source whitespace.
            ' ' if following == Some('\n') && previous == Some(' ') => {
                output.push_str("&#32;");
            }
            '_' if previous.is_some_and(char::is_alphanumeric)
                && following.is_some_and(char::is_alphanumeric) =>
            {
                output.push('_');
            }
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '~' => {
                output.push('\\');
                output.push(character);
            }
            '|' if context.table_cell => output.push_str("\\|"),
            _ => output.push(character),
        }
        previous = Some(character);
    }
    output
}

/// Protect complete serialized lines so fragmented runs share block context.
pub(super) fn protect_paragraph(rendered: String, context: InlineContext) -> String {
    if context.table_cell {
        return rendered;
    }
    let mut output = String::with_capacity(rendered.len());
    for (index, line) in rendered.split('\n').enumerate() {
        if index > 0 {
            output.push('\n');
        }
        let trimmed = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - trimmed.len();
        if !trimmed.is_empty() && (line[..indent].contains('\t') || indent >= 4) {
            output.push_str(if line.starts_with('\t') { "&#9;" } else { "&#32;" });
            output.push_str(&line[1..]);
        } else if let Some(position) = block_marker(trimmed) {
            output.push_str(&line[..indent + position]);
            output.push('\\');
            output.push_str(&line[indent + position..]);
        } else {
            output.push_str(line);
        }
    }
    output
}

fn block_marker(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let first = *bytes.first()?;
    let whitespace_or_end = |index| bytes.get(index).is_none_or(u8::is_ascii_whitespace);
    match first {
        b'>' => return Some(0),
        b'#' => {
            let count = bytes.iter().take_while(|byte| **byte == b'#').count();
            if count <= 6 && whitespace_or_end(count) {
                return Some(0);
            }
        }
        b'+' | b'-' if whitespace_or_end(1) => return Some(0),
        b'0'..=b'9' => {
            let count = bytes.iter().take_while(|byte| byte.is_ascii_digit()).count();
            if count <= 9
                && matches!(bytes.get(count), Some(b'.' | b')'))
                && whitespace_or_end(count + 1)
            {
                return Some(count);
            }
        }
        _ => {}
    }
    let trimmed = line.trim_end();
    if (first == b'=' && trimmed.bytes().all(|byte| byte == b'='))
        || (first == b'-' && trimmed.bytes().all(|byte| byte == b'-' || byte.is_ascii_whitespace()))
    {
        return Some(0);
    }
    // A delimiter row can turn the preceding literal line into a GFM table.
    if line.contains('|')
        && line.contains('-')
        && bytes.iter().all(|byte| matches!(byte, b'|' | b'-' | b':') || byte.is_ascii_whitespace())
    {
        return line.find('-');
    }
    None
}

pub(super) fn protect_heading(mut rendered: String) -> String {
    let end = rendered.trim_end().len();
    let hashes = rendered[..end].trim_end_matches('#').len();
    if hashes < end && (hashes == 0 || rendered[..hashes].ends_with([' ', '\t'])) {
        rendered.insert(hashes, '\\');
    }
    rendered
}
