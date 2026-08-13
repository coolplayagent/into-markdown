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
        let alias = alias_key(&name);
        if !self.aliases.insert(alias) {
            return Err(malformed(&name, "duplicate or Unicode/case alias entry name"));
        }
        let components = name.split('/').collect::<Vec<_>>();
        let mut prefix = String::new();
        for (index, component) in components.iter().enumerate() {
            if index > 0 {
                prefix.push('/');
            }
            prefix.push_str(component);
            let final_component = index + 1 == components.len();
            if !final_component && self.files.contains(&alias_key(&prefix)) {
                return Err(malformed(&name, "entry descends through another file"));
            }
        }
        let key = alias_key(&name);
        match kind {
            EntryKind::File => {
                if self.directories.contains(&key)
                    || self.directories.iter().any(|path| path.starts_with(&(key.clone() + "/")))
                    || self.files.iter().any(|path| path.starts_with(&(key.clone() + "/")))
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
    let declared = unix_mode.map(|mode| mode & TYPE_MASK).unwrap_or(0);
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
    for component in name.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(malformed(name, "entry path contains an empty or dot component"));
        }
        if component.contains(':') || is_windows_device(component) {
            return Err(malformed(name, "entry path contains a reserved platform component"));
        }
        if !canonical.is_empty() {
            canonical.push('/');
        }
        canonical.push_str(component);
    }
    Ok(canonical)
}

fn is_windows_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component).trim_end_matches([' ', '.']);
    let folded = stem.to_ascii_lowercase();
    matches!(folded.as_str(), "con" | "prn" | "aux" | "nul")
        || folded.strip_prefix("com").or_else(|| folded.strip_prefix("lpt")).is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

fn alias_key(name: &str) -> String {
    name.nfkc().flat_map(char::to_lowercase).collect()
}

fn malformed(name: &str, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed {
        part: Some(name.into()),
        detail: format!("ZIP member {name:?}: {}", detail.into()),
    }
}
