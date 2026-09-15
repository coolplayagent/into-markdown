use super::*;

pub(super) fn html_charset(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Option<String>, Vec<Diagnostic>), ConversionError> {
    let explicit = options
        .text
        .charset
        .as_deref()
        .or_else(|| input.metadata.media_type.as_deref().and_then(media_type_charset));
    if let Some(explicit) = explicit {
        let mut diagnostics = Vec::new();
        if let Some(meta) = prescan_meta_charset(&input.bytes, context)?
            && !meta.eq_ignore_ascii_case(explicit)
        {
            diagnostics.push(warning(
                "html.metaCharsetIgnored",
                format!("meta charset {meta} conflicts with explicit charset {explicit}"),
            ));
        }
        return Ok((Some(explicit.to_owned()), diagnostics));
    }
    Ok((prescan_meta_charset(&input.bytes, context)?, Vec::new()))
}

pub(super) fn media_type_charset(value: &str) -> Option<&str> {
    value
        .split(';')
        .skip(1)
        .find_map(|parameter| {
            let (name, value) = parameter.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches(['\'', '"']))
        })
        .filter(|value| !value.is_empty())
}
