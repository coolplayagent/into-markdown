#[derive(Debug, Clone, Default)]
struct ContentTypes {
    overrides: BTreeMap<String, String>,
    defaults: BTreeMap<String, String>,
}
impl ContentTypes {
    fn content_type(&self, part: &str) -> Option<&str> {
        self.overrides.get(&format!("/{part}")).map(String::as_str).or_else(|| {
            Path::new(part)
                .extension()
                .and_then(|value| value.to_str())
                .and_then(|extension| self.defaults.get(&extension.to_ascii_lowercase()))
                .map(String::as_str)
        })
    }

    fn is_macro_part(&self, part: &str) -> bool {
        self.content_type(part).is_some_and(is_macro_content_type)
    }

    fn macro_enabled_main(&self) -> bool {
        self.overrides.values().any(|content_type| {
            content_type
                .eq_ignore_ascii_case("application/vnd.ms-word.document.macroEnabled.main+xml")
        })
    }
}

fn parse_content_types(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ContentTypes, ConversionError> {
    preflight_xml(bytes, "[Content_Types].xml", XmlProfile::ContentTypes, options, context)?;
    let mut reader = NsReader::from_reader(bytes);
    let mut result = ContentTypes::default();
    loop {
        context.checkpoint()?;
        match reader.read_event().map_err(|error| {
            malformed(Some("[Content_Types].xml"), format!("invalid XML: {error}"))
        })? {
            Event::Empty(element) | Event::Start(element)
                if local(element.name().as_ref()) == "Override" =>
            {
                let part =
                    attr(&element, b"PartName", "[Content_Types].xml")?.ok_or_else(|| {
                        malformed(Some("[Content_Types].xml"), "Override lacks PartName")
                    })?;
                let content_type = attr(&element, b"ContentType", "[Content_Types].xml")?
                    .ok_or_else(|| {
                        malformed(Some("[Content_Types].xml"), "Override lacks ContentType")
                    })?;
                if !part.starts_with('/') || canonical_part_name(&part[1..])? != part[1..] {
                    return Err(malformed(Some("[Content_Types].xml"), "unsafe Override PartName"));
                }
                if result.overrides.insert(part, content_type).is_some() {
                    return Err(malformed(
                        Some("[Content_Types].xml"),
                        "duplicate Override PartName",
                    ));
                }
            }
            Event::Empty(element) | Event::Start(element)
                if local(element.name().as_ref()) == "Default" =>
            {
                let extension = attr(&element, b"Extension", "[Content_Types].xml")?
                    .ok_or_else(|| {
                        malformed(Some("[Content_Types].xml"), "Default lacks Extension")
                    })?
                    .to_ascii_lowercase();
                let content_type = attr(&element, b"ContentType", "[Content_Types].xml")?
                    .ok_or_else(|| {
                        malformed(Some("[Content_Types].xml"), "Default lacks ContentType")
                    })?;
                if extension.is_empty() || extension.contains(['/', '\\', '.']) {
                    return Err(malformed(Some("[Content_Types].xml"), "unsafe Default Extension"));
                }
                if result.defaults.insert(extension, content_type).is_some() {
                    return Err(malformed(
                        Some("[Content_Types].xml"),
                        "duplicate Default Extension",
                    ));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(result)
}
