use into_markdown_core::ConversionError;
use std::collections::BTreeSet;
use unicode_normalization::UnicodeNormalization as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryKind {
    File,
    Directory,
}

#[derive(Default)]
pub(super) struct EntryPolicy {
    aliases: BTreeSet<String>,
    files: BTreeSet<String>,
    directories: BTreeSet<String>,
}

impl EntryPolicy {
    pub(super) fn accept(
        &mut self,
        raw_name: &[u8],
        decoded_name: &str,
        unix_mode: Option<u32>,
        directory: bool,
    ) -> Result<(String, EntryKind), ConversionError> {
        let raw = std::str::from_utf8(raw_name)
            .map_err(|_| malformed(decoded_name, "entry name is not UTF-8"))?;
        if raw != decoded_name {
            return Err(malformed(decoded_name, "raw and decoded entry names disagree"));
        }
        let kind = validate_type(decoded_name, unix_mode, directory)?;
        let name = canonical_name(decoded_name, kind)?;
        let alias = alias_key(&name)?;
        if !self.aliases.insert(alias) {
            return Err(malformed(&name, "duplicate or Unicode/case alias entry name"));
        }
        let mut prefix = String::new();
        prefix.try_reserve_exact(name.len()).map_err(|error| {
            memory_limit(format!("reserve canonical prefix for {name:?}: {error}"))
        })?;
        let mut components = name.split('/').peekable();
        while let Some(component) = components.next() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if components.peek().is_some() && self.files.contains(&alias_key(&prefix)?) {
                return Err(malformed(&name, "entry descends through another file"));
            }
        }
        let key = alias_key(&name)?;
        match kind {
            EntryKind::File => {
                if self.directories.contains(&key)
                    || self.directories.iter().any(|path| descends_from(path, &key))
                    || self.files.iter().any(|path| descends_from(path, &key))
                {
                    return Err(malformed(&name, "file conflicts with an archive path prefix"));
                }
                self.files.insert(key);
            }
            EntryKind::Directory => {
                if self.files.contains(&key) {
                    return Err(malformed(&name, "directory conflicts with an archive file"));
                }
                self.directories.insert(key);
            }
        }
        Ok((name, kind))
    }
}

fn validate_type(
    name: &str,
    unix_mode: Option<u32>,
    directory: bool,
) -> Result<EntryKind, ConversionError> {
    const TYPE_MASK: u32 = 0o170_000;
    const REGULAR: u32 = 0o100_000;
    const DIRECTORY: u32 = 0o040_000;
    let declared = unix_mode.map_or(0, |mode| mode & TYPE_MASK);
    if !matches!(declared, 0 | REGULAR | DIRECTORY) {
        return Err(malformed(name, "symbolic links and special files are forbidden"));
    }
    if directory {
        if declared == REGULAR {
            return Err(malformed(name, "directory marker conflicts with regular-file mode"));
        }
        Ok(EntryKind::Directory)
    } else {
        if declared == DIRECTORY {
            return Err(malformed(name, "regular entry conflicts with directory mode"));
        }
        Ok(EntryKind::File)
    }
}

fn canonical_name(name: &str, kind: EntryKind) -> Result<String, ConversionError> {
    let name = match kind {
        EntryKind::Directory => name.strip_suffix('/').unwrap_or(name),
        EntryKind::File => name,
    };
    if name.is_empty()
        || name.starts_with('/')
        || name.starts_with("//")
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(char::is_control)
    {
        return Err(malformed(name, "entry name is not a safe relative path"));
    }
    let mut canonical = String::new();
    canonical
        .try_reserve_exact(name.len())
        .map_err(|error| memory_limit(format!("reserve canonical name for {name:?}: {error}")))?;
    for component in name.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(malformed(name, "entry path contains an empty or dot component"));
        }
        if component.trim_end_matches([' ', '.']) != component
            || !portable_component(component)
            || is_windows_device(component)
        {
            return Err(malformed(name, "entry path contains a reserved platform component"));
        }
        if !canonical.is_empty() {
            canonical.push('/');
        }
        canonical.push_str(component);
    }
    Ok(canonical)
}

/// Admit only names whose cross-platform alias behavior is provable without
/// claiming Unicode full case folding. Compatibility spellings, combining
/// sequences, non-ASCII case mappings, punctuation, and format characters are
/// rejected. Normalized case-less letter/digit scripts (including CJK) remain
/// usable.
fn portable_component(component: &str) -> bool {
    if component.nfkc().ne(component.chars()) {
        return false;
    }
    component.chars().all(|character| {
        if character.is_ascii() {
            character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.' | ' ' | '(' | ')' | '[' | ']')
        } else {
            character.is_alphanumeric()
                && character.to_lowercase().eq(std::iter::once(character))
                && character.to_uppercase().eq(std::iter::once(character))
        }
    })
}

fn is_windows_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component).trim_end_matches([' ', '.']);
    let bytes = stem.as_bytes();
    ["con", "prn", "aux", "nul"].iter().any(|name| stem.eq_ignore_ascii_case(name))
        || bytes.len() == 4
            && ([b"com", b"lpt"].iter().any(|prefix| bytes[..3].eq_ignore_ascii_case(*prefix))
                && matches!(bytes[3], b'1'..=b'9'))
}

fn alias_key(name: &str) -> Result<String, ConversionError> {
    let mut alias = String::new();
    alias
        .try_reserve_exact(name.len())
        .map_err(|error| memory_limit(format!("reserve portable alias for {name:?}: {error}")))?;
    alias.extend(name.chars().map(|character| character.to_ascii_lowercase()));
    Ok(alias)
}

fn descends_from(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix).is_some_and(|suffix| suffix.starts_with('/'))
}

fn malformed(name: &str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed {
        part: Some(name.into()),
        detail: format!("ZIP member {name:?}: {}", detail.into()),
    }
}

fn memory_limit(detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: "max_memory_bytes", detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(policy: &mut EntryPolicy, name: &str) -> Result<(), ConversionError> {
        policy.accept(name.as_bytes(), name, Some(0o100_644), false).map(|_| ())
    }

    #[test]
    fn portable_policy_accepts_normalized_caseless_scripts_and_ascii() {
        let mut policy = EntryPolicy::default();
        accept(&mut policy, "目录/报告.txt").unwrap();
        accept(&mut policy, "かな/文書.md").unwrap();
        accept(&mut policy, "A File [1].TXT").unwrap();
    }

    #[test]
    fn aliases_and_unprovable_unicode_names_are_rejected() {
        let mut aliases = EntryPolicy::default();
        accept(&mut aliases, "A.txt").unwrap();
        assert!(accept(&mut aliases, "a.TXT").is_err());

        for name in [
            "/absolute.txt",
            "C:drive.txt",
            "dir\\escape.txt",
            "straße.txt",
            "οσ.txt",
            "ος.txt",
            "İ.txt",
            "ı.txt",
            "Ａ.txt",
            "Ⓐ.txt",
            "e\u{301}.txt",
        ] {
            assert!(accept(&mut EntryPolicy::default(), name).is_err(), "accepted {name:?}");
        }
    }
}
