#[derive(Debug, Clone, Default)]
struct Relationship {
    target: String,
    external: bool,
    kind: String,
}

fn canonical_part_name(name: &str) -> Result<String, ConversionError> {
    if name.is_empty() || name.contains('\\') || name.contains('\0') || name.starts_with('/') {
        return Err(malformed(None, "unsafe ZIP part name"));
    }
    if name.split('/').any(|part| part.is_empty() || matches!(part, "." | "..")) {
        return Err(malformed(Some(name), "unsafe ZIP part path"));
    }
    let path = Path::new(name);
    if path.components().any(|value| !matches!(value, Component::Normal(_))) {
        return Err(malformed(Some(name), "unsafe ZIP part path"));
    }
    Ok(name.to_owned())
}

fn resolve_target(owner: &str, target: &str) -> Result<String, ConversionError> {
    if target.is_empty() || target.contains('\\') || target.contains('\0') || target.contains(':') {
        return Err(malformed(Some(owner), "unsafe internal relationship target"));
    }
    let package_absolute = target.starts_with('/');
    let target = target.strip_prefix('/').unwrap_or(target);
    if target.is_empty() || target.starts_with('/') {
        return Err(malformed(Some(owner), "unsafe internal relationship target"));
    }
    let mut segments = if package_absolute {
        Vec::new()
    } else {
        owner
            .rsplit_once('/')
            .map_or(Vec::new(), |(dir, _)| dir.split('/').map(str::to_owned).collect())
    };
    for value in target.split('/') {
        match value {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(malformed(Some(owner), "relationship escapes package root"));
                }
            }
            other => segments.push(other.to_owned()),
        }
    }
    if segments.is_empty() {
        return Err(malformed(Some(owner), "relationship target is empty"));
    }
    Ok(segments.join("/"))
}

fn relationship_part(owner: &str) -> String {
    let (dir, file) = owner.rsplit_once('/').unwrap_or(("", owner));
    if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") }
}

fn relationship_owner(part: &str) -> Result<String, ConversionError> {
    if part == "_rels/.rels" {
        return Ok(String::new());
    }
    let (directory, filename) = part
        .rsplit_once('/')
        .ok_or_else(|| malformed(Some(part), "relationship part has no _rels directory"))?;
    let owner_directory = directory
        .strip_suffix("/_rels")
        .ok_or_else(|| malformed(Some(part), "relationship part is outside _rels"))?;
    let owner_filename = filename
        .strip_suffix(".rels")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed(Some(part), "invalid relationship part name"))?;
    canonical_part_name(&format!("{owner_directory}/{owner_filename}"))
}

fn relationship_type(suffix: &str) -> String {
    format!("{REL_TYPE_PREFIX}{suffix}")
}

fn canonical_relationship_kind(kind: &str) -> String {
    kind.strip_prefix(STRICT_REL_TYPE_PREFIX)
        .map_or_else(|| kind.to_owned(), relationship_type)
}

fn unique_internal_relationship<'a>(
    relationships: &'a BTreeMap<String, Relationship>,
    kind: &str,
    owner: &str,
) -> Result<Option<(&'a str, &'a Relationship)>, ConversionError> {
    let mut matches = relationships
        .iter()
        .filter(|(_, relationship)| relationship.kind == kind && !relationship.external);
    let first = matches.next().map(|(id, relationship)| (id.as_str(), relationship));
    if matches.next().is_some() {
        return Err(malformed(
            Some(&relationship_part(owner)),
            format!("multiple relationships of type {kind}"),
        ));
    }
    Ok(first)
}

pub(super) fn decode_text(event: &BytesText<'_>, part: &str) -> Result<String, ConversionError> {
    let value = event
        .decode()
        .map_err(|error| malformed(Some(part), format!("invalid text encoding: {error}")))?
        .into_owned();
    validate_xml_characters(&value, part)?;
    Ok(value)
}

pub(super) fn decode_cdata(event: &BytesCData<'_>, part: &str) -> Result<String, ConversionError> {
    let value = event
        .decode()
        .map_err(|error| malformed(Some(part), format!("invalid CDATA encoding: {error}")))?
        .into_owned();
    validate_xml_characters(&value, part)?;
    Ok(value)
}

pub(super) fn decode_reference(
    event: &BytesRef<'_>,
    part: &str,
) -> Result<String, ConversionError> {
    let reference = event
        .decode()
        .map_err(|error| malformed(Some(part), format!("invalid reference encoding: {error}")))?;
    decode_reference_name(&reference, part)
}

fn decode_reference_name(reference: &str, part: &str) -> Result<String, ConversionError> {
    let predefined = match reference {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "apos" => Some("'"),
        "quot" => Some("\""),
        _ => None,
    };
    if let Some(value) = predefined {
        return Ok(value.into());
    }
    let (digits, radix) = if let Some(value) = reference.strip_prefix("#x") {
        (value, 16)
    } else if let Some(value) = reference.strip_prefix('#') {
        (value, 10)
    } else {
        return Err(malformed(Some(part), format!("custom XML entity &{reference}; is forbidden")));
    };
    if digits.is_empty()
        || (radix == 10 && !digits.bytes().all(|value| value.is_ascii_digit()))
        || (radix == 16 && !digits.bytes().all(|value| value.is_ascii_hexdigit()))
    {
        return Err(malformed(Some(part), "invalid numeric character reference"));
    }
    let codepoint = u32::from_str_radix(digits, radix)
        .map_err(|_| malformed(Some(part), "numeric character reference is out of range"))?;
    let character = char::from_u32(codepoint)
        .filter(|value| is_xml_character(*value))
        .ok_or_else(|| malformed(Some(part), "numeric character reference is not legal XML"))?;
    Ok(character.to_string())
}

pub(super) fn decode_xml_attribute(raw: &[u8], part: &str) -> Result<String, ConversionError> {
    let raw = std::str::from_utf8(raw)
        .map_err(|error| malformed(Some(part), format!("attribute is not UTF-8: {error}")))?;
    let mut decoded = String::with_capacity(raw.len());
    let mut cursor = 0;
    while let Some(relative_start) = raw[cursor..].find('&') {
        let start = cursor + relative_start;
        let literal = &raw[cursor..start];
        validate_xml_characters(literal, part)?;
        decoded.push_str(literal);
        let reference_start = start + 1;
        let end = raw[reference_start..]
            .find(';')
            .map(|relative| reference_start + relative)
            .ok_or_else(|| malformed(Some(part), "unterminated XML attribute reference"))?;
        decoded.push_str(&decode_reference_name(&raw[reference_start..end], part)?);
        cursor = end + 1;
    }
    let remainder = &raw[cursor..];
    validate_xml_characters(remainder, part)?;
    decoded.push_str(remainder);
    Ok(decoded)
}

fn validate_xml_characters(value: &str, part: &str) -> Result<(), ConversionError> {
    if value.chars().all(is_xml_character) {
        Ok(())
    } else {
        Err(malformed(Some(part), "text contains a character forbidden by XML 1.0"))
    }
}

fn is_xml_character(value: char) -> bool {
    matches!(value, '\u{9}' | '\u{a}' | '\u{d}')
        || matches!(value as u32, 0x20..=0xd7ff | 0xe000..=0xfffd | 0x0001_0000..=0x0010_ffff)
}

fn attr(e: &BytesStart<'_>, key: &[u8], part: &str) -> Result<Option<String>, ConversionError> {
    for value in e.attributes() {
        let value = value
            .map_err(|error| malformed(Some(part), format!("invalid XML attribute: {error}")))?;
        if value.key.as_ref() == key {
            return decode_xml_attribute(value.value.as_ref(), part).map(Some);
        }
    }
    Ok(None)
}

fn attr_local(
    e: &BytesStart<'_>,
    key: &str,
    part: &str,
) -> Result<Option<String>, ConversionError> {
    for value in e.attributes() {
        let value = value
            .map_err(|error| malformed(Some(part), format!("invalid XML attribute: {error}")))?;
        if local(value.key.as_ref()) == key {
            return decode_xml_attribute(value.value.as_ref(), part).map(Some);
        }
    }
    Ok(None)
}

fn local(name: &[u8]) -> &str {
    std::str::from_utf8(name.rsplit(|b| *b == b':').next().unwrap_or(name)).unwrap_or("")
}

pub(super) fn supported_image(
    part: &str,
    declared_content_type: &str,
) -> Result<SupportedImage, ConversionError> {
    let extension = Path::new(part)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| malformed(Some(part), "image part has no safe extension"))?;
    match (declared_content_type, extension.as_str()) {
        ("image/png", "png") => Ok(SupportedImage::Png),
        ("image/jpeg", "jpg" | "jpeg") => Ok(SupportedImage::Jpeg),
        _ => Err(malformed(
            Some("[Content_Types].xml"),
            format!(
                "image target {part} has an unsupported or extension-mismatched content type {declared_content_type}"
            ),
        )),
    }
}

pub(super) fn validate_image_bytes(
    image: SupportedImage,
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    match image {
        SupportedImage::Png => validate_png(bytes, part, options, context),
        SupportedImage::Jpeg => {
            let dimensions = validate_jpeg(bytes, part)?;
            validate_jpeg_pixels(bytes, dimensions, part, options, context)
        }
    }
}
