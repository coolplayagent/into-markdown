//! Filesystem-path to readable URI conversion, independent of the host platform.

use super::{CliError, ExitClass};
use std::fmt::Write as _;
use std::path::Path;

pub(super) fn asset_uri_prefix_for_stdout(
    directory: &Path,
    working_directory: &Path,
) -> Result<String, CliError> {
    asset_uri_prefix_for_stdout_with_flavor(
        directory.as_os_str().as_encoded_bytes(),
        working_directory.as_os_str().as_encoded_bytes(),
        native_path_flavor(),
    )
}

pub(super) fn asset_uri_prefix_for_stdout_with_flavor(
    directory: &[u8],
    working_directory: &[u8],
    flavor: PathFlavor,
) -> Result<String, CliError> {
    let base = lexical_absolute(working_directory, None, flavor)?;
    let target = lexical_absolute(directory, Some(&base), flavor)?;
    relative_uri_path(&base, &target)
}

pub(super) fn asset_uri_prefix_for_file(
    output: &Path,
    directory: &Path,
    working_directory: &Path,
) -> Result<String, CliError> {
    asset_uri_prefix_for_file_with_flavor(
        output.as_os_str().as_encoded_bytes(),
        directory.as_os_str().as_encoded_bytes(),
        working_directory.as_os_str().as_encoded_bytes(),
        native_path_flavor(),
    )
}

pub(super) fn asset_uri_prefix_for_file_with_flavor(
    output: &[u8],
    directory: &[u8],
    working_directory: &[u8],
    flavor: PathFlavor,
) -> Result<String, CliError> {
    let cwd = lexical_absolute(working_directory, None, flavor)?;
    let mut base = lexical_absolute(output, Some(&cwd), flavor)?;
    if base.components.pop().is_none() {
        return Err(asset_path_unsupported("output path has no filename"));
    }
    let target = lexical_absolute(directory, Some(&cwd), flavor)?;
    relative_uri_path(&base, &target)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PathFlavor {
    Posix,
    Windows,
}

#[cfg(not(windows))]
const fn native_path_flavor() -> PathFlavor {
    PathFlavor::Posix
}

#[cfg(windows)]
const fn native_path_flavor() -> PathFlavor {
    PathFlavor::Windows
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LexicalRoot {
    Posix,
    WindowsDrive(u8),
    Unc { server: Vec<u8>, share: Vec<u8> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LexicalAbsolutePath {
    root: LexicalRoot,
    components: Vec<Vec<u8>>,
}

type ParsedAbsoluteRoot<'a> = Option<(LexicalRoot, &'a [u8], PathFlavor)>;

fn lexical_absolute(
    path: &[u8],
    relative_base: Option<&LexicalAbsolutePath>,
    flavor: PathFlavor,
) -> Result<LexicalAbsolutePath, CliError> {
    if let Some((root, rest, parsed_flavor)) = absolute_root(path, flavor)? {
        let mut absolute = LexicalAbsolutePath { root, components: Vec::new() };
        normalize_components(&mut absolute.components, rest, parsed_flavor);
        return Ok(absolute);
    }
    if flavor == PathFlavor::Windows
        && path.len() >= 2
        && path[0].is_ascii_alphabetic()
        && path[1] == b':'
    {
        return Err(asset_path_unsupported(
            "drive-relative paths cannot be represented safely in Markdown",
        ));
    }
    if flavor == PathFlavor::Windows && path.first().is_some_and(|byte| is_separator(*byte, flavor))
    {
        return Err(asset_path_unsupported(
            "root-relative Windows paths cannot be represented without a drive",
        ));
    }
    let Some(base) = relative_base else {
        return Err(asset_path_unsupported("path base is not absolute"));
    };
    let mut absolute = base.clone();
    normalize_components(&mut absolute.components, path, flavor);
    Ok(absolute)
}

fn absolute_root(path: &[u8], flavor: PathFlavor) -> Result<ParsedAbsoluteRoot<'_>, CliError> {
    if flavor == PathFlavor::Posix {
        let mut cursor = 0;
        while cursor < path.len() && path[cursor] == b'/' {
            cursor += 1;
        }
        return Ok((cursor > 0).then_some((LexicalRoot::Posix, &path[cursor..], flavor)));
    }
    if path.len() >= 2 && is_separator(path[0], flavor) && is_separator(path[1], flavor) {
        let mut namespace_end = 2;
        while namespace_end < path.len() && !is_separator(path[namespace_end], flavor) {
            namespace_end += 1;
        }
        let namespace = &path[2..namespace_end];
        if namespace == b"?" {
            if namespace_end == path.len() {
                return Err(asset_path_unsupported("Windows verbatim path has no absolute root"));
            }
            let mut cursor = namespace_end;
            while cursor < path.len() && is_separator(path[cursor], flavor) {
                cursor += 1;
            }
            let rest = &path[cursor..];
            if rest.len() >= 3
                && rest[0].is_ascii_alphabetic()
                && rest[1] == b':'
                && is_separator(rest[2], flavor)
            {
                return Ok(Some((
                    LexicalRoot::WindowsDrive(rest[0].to_ascii_uppercase()),
                    &rest[3..],
                    flavor,
                )));
            }
            let mut marker_end = cursor;
            while marker_end < path.len() && !is_separator(path[marker_end], flavor) {
                marker_end += 1;
            }
            if path[cursor..marker_end].eq_ignore_ascii_case(b"UNC") {
                return unc_root(path, marker_end, flavor);
            }
            return Err(asset_path_unsupported(
                "Windows device paths cannot be represented safely in Markdown",
            ));
        }
        if namespace == b"." {
            return Err(asset_path_unsupported(
                "Windows device paths cannot be represented safely in Markdown",
            ));
        }
        return unc_root(path, 2, flavor);
    }
    if path.len() >= 3
        && path[0].is_ascii_alphabetic()
        && path[1] == b':'
        && is_separator(path[2], flavor)
    {
        return Ok(Some((
            LexicalRoot::WindowsDrive(path[0].to_ascii_uppercase()),
            &path[3..],
            flavor,
        )));
    }
    Ok(None)
}

fn unc_root(
    path: &[u8],
    mut cursor: usize,
    flavor: PathFlavor,
) -> Result<ParsedAbsoluteRoot<'_>, CliError> {
    while cursor < path.len() && is_separator(path[cursor], flavor) {
        cursor += 1;
    }
    let server_start = cursor;
    while cursor < path.len() && !is_separator(path[cursor], flavor) {
        cursor += 1;
    }
    let server = &path[server_start..cursor];
    if server.is_empty() {
        return Err(asset_path_unsupported("UNC path is missing its server"));
    }
    while cursor < path.len() && is_separator(path[cursor], flavor) {
        cursor += 1;
    }
    let share_start = cursor;
    while cursor < path.len() && !is_separator(path[cursor], flavor) {
        cursor += 1;
    }
    let share = &path[share_start..cursor];
    if share.is_empty() {
        return Err(asset_path_unsupported("UNC path is missing its share"));
    }
    if matches!(server, b"." | b".." | b"?") || matches!(share, b"." | b"..") {
        return Err(asset_path_unsupported("UNC path has an invalid server or share"));
    }
    while cursor < path.len() && is_separator(path[cursor], flavor) {
        cursor += 1;
    }
    Ok(Some((
        LexicalRoot::Unc { server: server.to_vec(), share: share.to_vec() },
        &path[cursor..],
        flavor,
    )))
}

fn split_components(path: &[u8], flavor: PathFlavor) -> impl Iterator<Item = &[u8]> {
    path.split(move |byte| is_separator(*byte, flavor)).filter(|component| !component.is_empty())
}

fn normalize_components(output: &mut Vec<Vec<u8>>, path: &[u8], flavor: PathFlavor) {
    for component in split_components(path, flavor) {
        match component {
            b"." => {}
            b".." => {
                output.pop();
            }
            _ => output.push(component.to_vec()),
        }
    }
}

fn is_separator(byte: u8, flavor: PathFlavor) -> bool {
    byte == b'/' || flavor == PathFlavor::Windows && byte == b'\\'
}

fn relative_uri_path(
    base: &LexicalAbsolutePath,
    target: &LexicalAbsolutePath,
) -> Result<String, CliError> {
    if !same_root(&base.root, &target.root) {
        return Err(asset_path_unsupported(
            "asset directory and Markdown output use different filesystem roots",
        ));
    }
    let windows = !matches!(base.root, LexicalRoot::Posix);
    let common = base
        .components
        .iter()
        .zip(&target.components)
        .take_while(|(left, right)| component_eq(left, right, windows))
        .count();
    let mut parts = vec![b"..".to_vec(); base.components.len() - common];
    parts.extend(target.components[common..].iter().cloned());
    if parts.is_empty() {
        return Ok(".".into());
    }
    Ok(parts.iter().map(|part| encode_uri_segment(part)).collect::<Vec<_>>().join("/"))
}

fn same_root(left: &LexicalRoot, right: &LexicalRoot) -> bool {
    match (left, right) {
        (LexicalRoot::Posix, LexicalRoot::Posix) => true,
        (LexicalRoot::WindowsDrive(left), LexicalRoot::WindowsDrive(right)) => {
            left.eq_ignore_ascii_case(right)
        }
        (
            LexicalRoot::Unc { server: left_server, share: left_share },
            LexicalRoot::Unc { server: right_server, share: right_share },
        ) => {
            component_eq(left_server, right_server, true)
                && component_eq(left_share, right_share, true)
        }
        _ => false,
    }
}

fn component_eq(left: &[u8], right: &[u8], case_insensitive: bool) -> bool {
    if case_insensitive { left.eq_ignore_ascii_case(right) } else { left == right }
}

fn encode_uri_segment(segment: &[u8]) -> String {
    let mut encoded = String::with_capacity(segment.len());
    let mut remaining = segment;
    while !remaining.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(remaining) {
            Ok(valid) => (valid, 0),
            Err(error) => (
                std::str::from_utf8(&remaining[..error.valid_up_to()]).expect("valid UTF-8 prefix"),
                error.error_len().unwrap_or(remaining.len() - error.valid_up_to()),
            ),
        };
        for character in valid.chars() {
            if (!character.is_ascii() && !character.is_whitespace() && !character.is_control())
                || character.is_ascii_alphanumeric()
                || matches!(character, '-' | '.' | '_' | '~' | '(' | ')')
            {
                encoded.push(character);
            } else {
                for byte in character.encode_utf8(&mut [0; 4]).bytes() {
                    write!(encoded, "%{byte:02X}").expect("String write");
                }
            }
        }
        let consumed = valid.len();
        for byte in &remaining[consumed..consumed + invalid] {
            write!(encoded, "%{byte:02X}").expect("String write");
        }
        remaining = &remaining[consumed + invalid..];
    }
    encoded
}

pub(super) fn asset_path_unsupported(message: impl Into<String>) -> CliError {
    CliError::new(ExitClass::Usage, "assetPathUnsupported", message)
}
