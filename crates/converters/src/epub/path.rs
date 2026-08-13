//! Container-confined URI reference and `xml:base` resolution.

use crate::zip_converter::archive_api::{SafeArchive, portable_identity};
use into_markdown_core::ConversionError;
use url::Url;

const SYNTHETIC_ORIGIN: &str = "https://epub.invalid/";

#[derive(Clone, Debug)]
pub(super) struct BasePath {
    path: String,
    directory: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Reference {
    Internal { path: String, fragment: Option<String> },
    External(String),
}

impl BasePath {
    pub(super) fn document(path: &str) -> Result<Self, ConversionError> {
        Ok(Self { path: portable_identity(path, false)?, directory: false })
    }

    pub(super) fn apply(&self, value: &str) -> Result<Self, ConversionError> {
        let raw = split_reference(value)?;
        if raw.external {
            return Err(malformed("xml:base must remain inside the EPUB container"));
        }
        if raw.query.is_some() || raw.fragment.is_some() {
            return Err(malformed("xml:base must not contain a query or fragment"));
        }
        let directory = raw.path.ends_with('/');
        let path = resolve_path(self, raw.path, directory)?;
        Ok(Self { path, directory })
    }

    pub(super) fn resolve(&self, value: &str) -> Result<Reference, ConversionError> {
        let raw = split_reference(value)?;
        if raw.external {
            return Ok(Reference::External(value.to_owned()));
        }
        if raw.query.is_some() {
            return Err(malformed("container references must not contain a query"));
        }
        let path = if raw.path.is_empty() {
            if self.directory {
                return Err(malformed("a fragment-only reference resolved to a directory base"));
            }
            self.path.clone()
        } else {
            resolve_path(self, raw.path, false)?
        };
        let fragment = raw.fragment.map(validate_fragment).transpose()?;
        Ok(Reference::Internal { path, fragment })
    }
}

impl Reference {
    pub(super) fn require_existing(
        self,
        archive: &SafeArchive<'_, '_>,
    ) -> Result<Self, ConversionError> {
        if let Self::Internal { path, .. } = &self
            && !archive.contains(path)
        {
            return Err(ConversionError::Malformed {
                part: Some(path.clone()),
                detail: format!("EPUB reference points to missing package part {path:?}"),
            });
        }
        Ok(self)
    }

    pub(super) fn canonical_target(&self) -> String {
        match self {
            Self::External(value) => value.clone(),
            Self::Internal { path, fragment } => match fragment {
                Some(fragment) => format!("{path}#{fragment}"),
                None => path.clone(),
            },
        }
    }

    pub(super) fn synthetic_url(&self) -> Result<Option<String>, ConversionError> {
        let Self::Internal { path, fragment } = self else { return Ok(None) };
        let mut url = Url::parse(SYNTHETIC_ORIGIN).map_err(|error| ConversionError::Internal {
            detail: format!("invalid built-in EPUB synthetic origin: {error}"),
        })?;
        url.set_path(path);
        url.set_fragment(fragment.as_deref());
        Ok(Some(url.into()))
    }
}

pub(super) fn synthetic_document_url(path: &str) -> Result<String, ConversionError> {
    Reference::Internal { path: portable_identity(path, false)?, fragment: None }
        .synthetic_url()?
        .ok_or_else(|| ConversionError::Internal {
            detail: "EPUB internal path did not produce a synthetic URL".into(),
        })
}

struct RawReference<'a> {
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
    external: bool,
}

fn split_reference(value: &str) -> Result<RawReference<'_>, ConversionError> {
    if value.is_empty() || value.chars().any(char::is_control) || value.contains('\\') {
        return Err(malformed("EPUB URI reference is empty or contains forbidden characters"));
    }
    if value.starts_with("//") {
        return Ok(RawReference { path: value, query: None, fragment: None, external: true });
    }
    let (before_fragment, fragment) =
        value.split_once('#').map_or((value, None), |(path, fragment)| (path, Some(fragment)));
    let (path, query) = before_fragment
        .split_once('?')
        .map_or((before_fragment, None), |(path, query)| (path, Some(query)));
    let colon = path.find(':');
    let slash = path.find('/');
    let external = colon.is_some_and(|colon| slash.is_none_or(|slash| colon < slash));
    if external {
        let scheme = &path[..colon.unwrap_or_default()];
        if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "mailto") {
            return Err(malformed("EPUB URI reference uses an unsupported scheme"));
        }
    }
    Ok(RawReference { path, query, fragment, external })
}

fn resolve_path(
    base: &BasePath,
    reference: &str,
    directory: bool,
) -> Result<String, ConversionError> {
    if reference.starts_with('/') || reference.contains("//") {
        return Err(malformed("EPUB path must be a relative container URI"));
    }
    let mut components = if base.directory {
        base.path.split('/').map(str::to_owned).collect::<Vec<_>>()
    } else {
        base.path
            .rsplit_once('/')
            .map_or_else(Vec::new, |(parent, _)| parent.split('/').map(str::to_owned).collect())
    };
    for raw in reference.split('/') {
        if raw.is_empty() {
            if directory && reference.ends_with('/') {
                continue;
            }
            return Err(malformed("EPUB path contains an empty component"));
        }
        let component = percent_decode(raw)?;
        match component.as_str() {
            "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(malformed("EPUB path escapes the container root"));
                }
            }
            _ => components.push(component),
        }
    }
    if components.is_empty() {
        return Err(malformed("EPUB path resolves to the container root"));
    }
    portable_identity(&components.join("/"), directory)
}

fn percent_decode(value: &str) -> Result<String, ConversionError> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high =
                *bytes.get(index + 1).ok_or_else(|| malformed("truncated percent escape"))?;
            let low = *bytes.get(index + 2).ok_or_else(|| malformed("truncated percent escape"))?;
            output.push(
                hex(high)?
                    .checked_mul(16)
                    .and_then(|v| v.checked_add(hex(low).ok()?))
                    .ok_or_else(|| malformed("invalid percent escape"))?,
            );
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|_| malformed("percent-decoded EPUB path is not UTF-8"))
}

fn hex(value: u8) -> Result<u8, ConversionError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(malformed("invalid percent escape")),
    }
}

fn validate_fragment(value: &str) -> Result<String, ConversionError> {
    if value.is_empty() || value.chars().any(char::is_control) || value.contains('&') {
        return Err(malformed("EPUB fragment is empty or unsafe"));
    }
    Ok(value.to_owned())
}

fn malformed(detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: None, detail: detail.into() }
}
