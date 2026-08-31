use caseless::Caseless as _;
use into_markdown_core::ConversionError;
use into_markdown_core::{ExecutionContext, ResourceReservation};
use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryKind {
    File,
    Directory,
}

struct PathEntry {
    original: String,
    kind: Option<EntryKind>,
}

pub(super) struct EntryPolicy<'a> {
    paths: BTreeMap<String, PathEntry>,
    memory: ResourceReservation,
    context: &'a ExecutionContext,
}

impl<'a> EntryPolicy<'a> {
    pub(super) fn new(context: &'a ExecutionContext) -> Result<Self, ConversionError> {
        Ok(Self { paths: BTreeMap::new(), memory: context.reserve_memory(0)?, context })
    }

    pub(super) fn accept(
        &mut self,
        decoded_name: &str,
        unix_mode: Option<u32>,
        directory: bool,
    ) -> Result<(String, EntryKind), ConversionError> {
        let kind = validate_type(decoded_name, unix_mode, directory)?;
        // Unicode normalization can expand input; acquire temporary capacity before processing it.
        let scratch_bytes = u64::try_from(decoded_name.len())
            .unwrap_or(u64::MAX)
            .checked_mul(128)
            .ok_or_else(|| memory_limit("ZIP name normalization plan overflow"))?;
        let _scratch = self.context.reserve_memory(scratch_bytes)?;
        let name = canonical_name(decoded_name, kind)?;
        let mut original = String::new();
        let mut key = String::new();
        let count = name.split('/').count();
        if count > usize::from(self.context.resource_limits().max_nesting_depth) {
            return Err(ConversionError::ResourceLimit {
                limit: "max_nesting_depth",
                detail: format!(
                    "ZIP member {name:?}: {count} path components exceed {}",
                    self.context.resource_limits().max_nesting_depth
                ),
            });
        }
        for (index, component) in name.split('/').enumerate() {
            self.context.checkpoint()?;
            if index != 0 {
                original.push('/');
                key.push('/');
            }
            original.push_str(component);
            key.push_str(&alias_key(component));
            let last = index + 1 == count;
            if let Some(existing) = self.paths.get_mut(&key) {
                if existing.original != original {
                    return Err(malformed(
                        &name,
                        format!("Unicode/case alias conflicts with {:?}", existing.original),
                    ));
                }
                if last {
                    if existing.kind.is_some() || kind == EntryKind::File {
                        return Err(malformed(
                            &name,
                            "duplicate entry or file/directory prefix conflict",
                        ));
                    }
                    existing.kind = Some(kind);
                } else if existing.kind == Some(EntryKind::File) {
                    return Err(malformed(&name, "entry descends through another file"));
                }
            } else {
                let bytes = key
                    .len()
                    .checked_add(original.len())
                    .and_then(|n| n.checked_add(256))
                    .ok_or_else(|| memory_limit("ZIP path index plan overflow"))?;
                self.memory.grow(u64::try_from(bytes).unwrap_or(u64::MAX))?;
                self.paths.insert(
                    key.clone(),
                    PathEntry { original: original.clone(), kind: last.then_some(kind) },
                );
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
            || !safe_component(component)
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

pub(crate) fn portable_identity(name: &str, directory: bool) -> Result<String, ConversionError> {
    canonical_name(name, if directory { EntryKind::Directory } else { EntryKind::File })
}

/// Logical names remain exact; compatibility aliases are checked separately.
fn safe_component(component: &str) -> bool {
    let normalized: String = component.nfkc().collect();
    !normalized.is_empty()
        && !matches!(normalized.as_str(), "." | "..")
        && normalized.trim_end_matches([' ', '.']) == normalized
        && !normalized.chars().any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ':' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        && !is_windows_device(&normalized)
}

fn is_windows_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component).trim_end_matches([' ', '.']);
    let bytes = stem.as_bytes();
    ["con", "prn", "aux", "nul", "conin$", "conout$"]
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
        || bytes.len() == 4
            && ([b"com", b"lpt"].iter().any(|prefix| bytes[..3].eq_ignore_ascii_case(*prefix))
                && matches!(bytes[3], b'1'..=b'9'))
}

fn alias_key(name: &str) -> String {
    name.chars().nfd().default_case_fold().nfkd().default_case_fold().nfkd().collect()
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
    use into_markdown_core::{ConversionOptions, ExecutionOptions};

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ConversionOptions::default().limits)
    }
    fn accept(policy: &mut EntryPolicy<'_>, name: &str) -> Result<(), ConversionError> {
        policy.accept(name, Some(0o100_644), false).map(|_| ())
    }

    #[test]
    fn unicode_names_are_preserved_and_aliases_are_rejected_in_both_orders() {
        for name in [
            "目录/报告（最终）.txt",
            "かな/文書.md",
            "café.txt",
            "e\u{301}.txt",
            "straße.txt",
            "Ａ.txt",
            "😀.txt",
        ] {
            let context = context();
            let mut policy = EntryPolicy::new(&context).unwrap();
            assert_eq!(policy.accept(name, None, false).unwrap().0, name);
            drop(policy);
            assert_eq!(context.reserved_memory_bytes(), 0);
        }
        for (a, b) in [
            ("A.txt", "a.TXT"),
            ("Ａ.txt", "a.txt"),
            ("é.txt", "e\u{301}.txt"),
            ("straße.txt", "STRASSE.TXT"),
            ("οσ.txt", "ος.txt"),
            ("报告（1）.txt", "报告(1).txt"),
            ("A/x.txt", "a/y.txt"),
            ("x", "x/y.txt"),
            ("x", "x/y/z.txt"),
        ] {
            for (first, second) in [(a, b), (b, a)] {
                let context = context();
                let mut policy = EntryPolicy::new(&context).unwrap();
                accept(&mut policy, first).unwrap();
                assert!(accept(&mut policy, second).is_err(), "accepted {first:?} and {second:?}");
            }
        }
    }

    #[test]
    fn dangerous_names_and_types_remain_rejected() {
        for name in [
            "/absolute.txt",
            "../a",
            "a/../b",
            "a//b",
            "C:drive.txt",
            "dir\\escape.txt",
            "a：b",
            "a／b",
            "．．/a",
            "a. ",
            "NUL.txt",
            "COM¹.txt",
            "ＣＯＮ",
            "conin$",
            "a\u{202e}txt",
        ] {
            let context = context();
            assert!(
                accept(&mut EntryPolicy::new(&context).unwrap(), name).is_err(),
                "accepted {name:?}"
            );
        }
        for mode in [0o120_777, 0o020_644, 0o010_644] {
            let context = context();
            assert!(
                EntryPolicy::new(&context).unwrap().accept("safe.txt", Some(mode), false).is_err()
            );
        }
    }

    #[test]
    fn explicit_directory_can_follow_its_implicit_prefix_once() {
        let context = context();
        let mut policy = EntryPolicy::new(&context).unwrap();
        accept(&mut policy, "dir/a.txt").unwrap();
        policy.accept("dir/", None, true).unwrap();
        assert!(policy.accept("dir/", None, true).is_err());
    }
}
