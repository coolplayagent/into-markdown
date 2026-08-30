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
                let references = css_references(&path, &entry.bytes, archive)?;
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
) -> Result<BTreeSet<String>, ConversionError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ConversionError::Malformed {
        part: Some(path.into()),
        detail: "reachable CSS resource is not UTF-8".into(),
    })?;
    let without_comments = strip_comments(text);
    let lower = without_comments.to_ascii_lowercase();
    let base = BasePath::document(path)?;
    let mut values = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("url(") {
        let start = cursor + relative + 4;
        let end =
            without_comments[start..].find(')').ok_or_else(|| ConversionError::Malformed {
                part: Some(path.into()),
                detail: "reachable CSS contains an unterminated url()".into(),
            })? + start;
        values.push(trim_css_url(&without_comments[start..end])?);
        cursor = end + 1;
    }
    cursor = 0;
    while let Some(relative) = lower[cursor..].find("@import") {
        let start = cursor + relative + "@import".len();
        let tail = without_comments[start..].trim_start();
        if let Some(quote @ ('\'' | '"')) = tail.chars().next() {
            let value = &tail[quote.len_utf8()..];
            let end = value.find(quote).ok_or_else(|| ConversionError::Malformed {
                part: Some(path.into()),
                detail: "reachable CSS contains an unterminated @import".into(),
            })?;
            values.push(value[..end].trim());
        }
        cursor = start;
    }
    let mut output = BTreeSet::new();
    for value in values {
        if value.is_empty() || value.starts_with('#') || value.starts_with("data:") {
            continue;
        }
        let reference = base.resolve(value)?.require_existing(archive)?;
        if let Reference::Internal { path, .. } = reference {
            output.insert(path);
        }
    }
    Ok(output)
}

fn trim_css_url(value: &str) -> Result<&str, ConversionError> {
    let value = value.trim();
    if let Some(quote @ ('\'' | '"')) = value.chars().next() {
        return value
            .strip_prefix(quote)
            .and_then(|value| value.strip_suffix(quote))
            .map(str::trim)
            .ok_or_else(|| ConversionError::Malformed {
                part: None,
                detail: "reachable CSS contains mismatched url() quotes".into(),
            });
    }
    Ok(value)
}

fn strip_comments(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(start) = remaining.find("/*") {
        output.push_str(&remaining[..start]);
        let Some(end) = remaining[start + 2..].find("*/") else { return output };
        remaining = &remaining[start + 2 + end + 2..];
    }
    output.push_str(remaining);
    output
}

#[cfg(test)]
mod tests {
    use super::strip_comments;

    #[test]
    fn comments_do_not_create_resource_edges() {
        assert_eq!(strip_comments("a{/* url(orphan.woff) */color:red}"), "a{color:red}");
    }
}
