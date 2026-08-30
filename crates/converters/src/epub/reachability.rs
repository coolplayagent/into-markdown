//! Reachability of EPUB resources that have no lossless document-IR mapping.

use super::navigation::Navigation;
use super::package::Package;
use super::path::{BasePath, Reference};
use super::spine::SpineResult;
use crate::zip_converter::archive_api::SafeArchive;
use into_markdown_core::{ConversionError, ExecutionContext};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct OmittedResources {
    pub(super) css: usize,
    pub(super) fonts: usize,
    pub(super) media: usize,
}

pub(super) fn omitted_resources(
    package: &Package,
    navigation: Option<&Navigation>,
    spine: &SpineResult,
    archive: &mut SafeArchive<'_, '_>,
    context: &ExecutionContext,
) -> Result<OmittedResources, ConversionError> {
    let by_path = package
        .manifest
        .values()
        .map(|item| (item.path.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    let mut queue = spine
        .chapters
        .iter()
        .flat_map(|chapter| chapter.resource_paths.iter().cloned())
        .collect::<VecDeque<_>>();
    if let Some(navigation) = navigation {
        queue.extend(navigation.resource_paths.iter().cloned());
    }
    let mut reached = BTreeSet::new();
    let mut omitted = OmittedResources::default();
    while let Some(path) = queue.pop_front() {
        context.checkpoint()?;
        if !reached.insert(path.clone()) {
            continue;
        }
        let Some(item) = by_path.get(path.as_str()) else { continue };
        match item.media_type.as_str() {
            "text/css" => {
                omitted.css += 1;
                let entry = archive.read(&path)?;
                let references = css_references(&path, &entry.bytes, archive, context)?;
                drop(entry);
                queue.extend(references);
            }
            value if value.starts_with("font/") || value.contains("font") => omitted.fonts += 1,
            value if value.starts_with("audio/") || value.starts_with("video/") => {
                omitted.media += 1;
            }
            _ => {}
        }
    }
    Ok(omitted)
}

fn css_references(
    path: &str,
    bytes: &[u8],
    archive: &SafeArchive<'_, '_>,
    context: &ExecutionContext,
) -> Result<BTreeSet<String>, ConversionError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ConversionError::Malformed {
        part: Some(path.into()),
        detail: "reachable CSS resource is not UTF-8".into(),
    })?;
    let base = BasePath::document(path)?;
    let bytes = text.as_bytes();
    let mut output = BTreeSet::new();
    let mut cursor = 0;
    let mut next_checkpoint = 4 * 1024;
    while cursor < bytes.len() {
        if cursor >= next_checkpoint {
            context.checkpoint()?;
            next_checkpoint = next_checkpoint.saturating_add(4 * 1024);
        }
        if starts_comment(bytes, cursor) {
            cursor = skip_comment(bytes, cursor);
            continue;
        }
        if matches!(bytes[cursor], b'\'' | b'"') {
            cursor = quoted_value(text, cursor)?.1;
            continue;
        }
        if bytes[cursor] == b'@' {
            let name_start = cursor + 1;
            let name_end = identifier_end(bytes, name_start);
            if bytes[name_start..name_end].eq_ignore_ascii_case(b"import") {
                let start = skip_layout(bytes, name_end);
                if start < bytes.len() && matches!(bytes[start], b'\'' | b'"') {
                    let (value, end) = quoted_value(text, start)?;
                    insert_reference(value, &base, archive, &mut output)?;
                    cursor = end;
                    continue;
                }
                let token_end = identifier_end(bytes, start);
                if bytes[start..token_end].eq_ignore_ascii_case(b"url") {
                    let open = skip_layout(bytes, token_end);
                    if bytes.get(open) == Some(&b'(') {
                        let (value, end) = url_value(text, open)?;
                        insert_reference(value, &base, archive, &mut output)?;
                        cursor = end;
                        continue;
                    }
                }
            }
            cursor = name_end.max(cursor + 1);
            continue;
        }
        if identifier_byte(bytes[cursor]) {
            let end = identifier_end(bytes, cursor);
            if bytes[cursor..end].eq_ignore_ascii_case(b"url") {
                let open = skip_layout(bytes, end);
                if bytes.get(open) == Some(&b'(') {
                    let (value, end) = url_value(text, open)?;
                    insert_reference(value, &base, archive, &mut output)?;
                    cursor = end;
                    continue;
                }
            }
            cursor = end;
            continue;
        }
        cursor += 1;
    }
    Ok(output)
}

fn insert_reference(
    value: &str,
    base: &BasePath,
    archive: &SafeArchive<'_, '_>,
    output: &mut BTreeSet<String>,
) -> Result<(), ConversionError> {
    let value = value.trim();
    if value.is_empty()
        || value.starts_with('#')
        || value.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return Ok(());
    }
    let reference = base.resolve(value)?.require_existing(archive)?;
    if let Reference::Internal { path, .. } = reference {
        output.insert(path);
    }
    Ok(())
}

fn url_value(text: &str, open: usize) -> Result<(&str, usize), ConversionError> {
    let bytes = text.as_bytes();
    let start = skip_layout(bytes, open + 1);
    if start >= bytes.len() {
        return Err(malformed_css("unterminated url()"));
    }
    if matches!(bytes[start], b'\'' | b'"') {
        let (value, end) = quoted_value(text, start)?;
        let close = skip_layout(bytes, end);
        if bytes.get(close) != Some(&b')') {
            return Err(malformed_css("url() has trailing tokens or no closing parenthesis"));
        }
        return Ok((value, close + 1));
    }
    let mut cursor = start;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b')' => return Ok((&text[start..cursor], cursor + 1)),
            b'\\' => cursor = (cursor + 2).min(bytes.len()),
            b'\'' | b'"' => return Err(malformed_css("url() contains an unexpected quote")),
            _ => cursor += 1,
        }
    }
    Err(malformed_css("unterminated url()"))
}

fn quoted_value(text: &str, start: usize) -> Result<(&str, usize), ConversionError> {
    let bytes = text.as_bytes();
    let quote = bytes[start];
    let value_start = start + 1;
    let mut cursor = value_start;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = (cursor + 2).min(bytes.len()),
            value if value == quote => return Ok((&text[value_start..cursor], cursor + 1)),
            _ => cursor += 1,
        }
    }
    Err(malformed_css("unterminated string"))
}

fn skip_layout(bytes: &[u8], mut cursor: usize) -> usize {
    loop {
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if !starts_comment(bytes, cursor) {
            return cursor;
        }
        cursor = skip_comment(bytes, cursor);
    }
}

fn starts_comment(bytes: &[u8], cursor: usize) -> bool {
    bytes.get(cursor) == Some(&b'/') && bytes.get(cursor + 1) == Some(&b'*')
}

fn skip_comment(bytes: &[u8], mut cursor: usize) -> usize {
    cursor += 2;
    while cursor + 1 < bytes.len() {
        if bytes[cursor] == b'*' && bytes[cursor + 1] == b'/' {
            return cursor + 2;
        }
        cursor += 1;
    }
    bytes.len()
}

fn identifier_end(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(|byte| identifier_byte(*byte)) {
        cursor += 1;
    }
    cursor
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn malformed_css(detail: &str) -> ConversionError {
    ConversionError::Malformed { part: None, detail: format!("reachable CSS contains {detail}") }
}

#[cfg(test)]
mod tests {
    use super::{identifier_end, quoted_value, skip_comment, skip_layout, url_value};

    #[test]
    fn css_scanner_skips_comments_and_strings_without_normalizing_the_document() {
        let css = r#"/* url(orphan.woff) */a{content:"url('text.woff')";src:url(real.woff)}"#;
        assert_eq!(skip_comment(css.as_bytes(), 0), 22);
        let quote = css.find('"').unwrap();
        assert_eq!(quoted_value(css, quote).unwrap().0, "url('text.woff')");
        let url = css.rfind("url").unwrap();
        let open = skip_layout(css.as_bytes(), identifier_end(css.as_bytes(), url));
        assert_eq!(url_value(css, open).unwrap().0, "real.woff");
    }
}
