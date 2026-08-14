use crate::odf::model::malformed;
use into_markdown_core::ConversionError;
use std::path::{Component, Path};

pub(super) fn canonical_part_name(name: &str, directory: bool) -> Result<String, ConversionError> {
    if name.is_empty() || name.contains(['\\', '\0']) || name.starts_with('/') || name.contains(':')
    {
        return Err(malformed(None, "unsafe ODF ZIP part name"));
    }
    let stripped = if directory { name.strip_suffix('/').unwrap_or(name) } else { name };
    if stripped.is_empty()
        || stripped.split('/').any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(malformed(Some(name), "unsafe ODF ZIP part path"));
    }
    if Path::new(stripped).components().any(|value| !matches!(value, Component::Normal(_))) {
        return Err(malformed(Some(name), "unsafe ODF ZIP part path"));
    }
    Ok(if directory { format!("{stripped}/") } else { stripped.to_owned() })
}
