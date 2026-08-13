use super::allocation::try_clone_string;
use super::error::{limit, malformed};
use super::mce::McSelection;
use super::model::ContentTypes;
use super::relationships::{ascii_case_cmp, dangerous_content_type, validate_part_name};
use super::xml::{XmlProfile, preflight_xml};
use super::xml_base::{local, required_attr};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::Event;
use quick_xml::reader::NsReader;

impl ContentTypes {
    pub(super) fn content_type(&self, part: &str) -> Option<&str> {
        self.overrides
            .binary_search_by(|(name, _)| name.as_str().cmp(part))
            .ok()
            .map(|index| self.overrides[index].1.as_str())
            .or_else(|| {
                // OPC defines the extension as the substring after the final dot. Unlike
                // `Path::extension`, this includes the root relationship part `_rels/.rels`.
                part.rsplit_once('.').filter(|(_, extension)| !extension.is_empty()).and_then(
                    |(_, extension)| {
                        self.defaults
                            .binary_search_by(|(key, _)| ascii_case_cmp(key, extension))
                            .ok()
                            .map(|index| self.defaults[index].1.as_str())
                    },
                )
            })
    }

    pub(super) fn dangerous(&self, part: &str) -> bool {
        self.content_type(part).is_some_and(dangerous_content_type)
    }
}

pub(super) fn parse_content_types(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ContentTypes, ConversionError> {
    preflight_xml(bytes, "[Content_Types].xml", XmlProfile::Types, options, context)?;
    let mut reader = NsReader::from_reader(bytes);
    let mut result = ContentTypes::default();
    let mut mc = McSelection::default();
    loop {
        context.checkpoint()?;
        let event = reader
            .read_event()
            .map_err(|error| malformed(Some("[Content_Types].xml"), error.to_string()))?;
        if mc.skip(&reader, &event, "[Content_Types].xml")? {
            continue;
        }
        match event {
            Event::Start(element) | Event::Empty(element) => match local(element.name().as_ref()) {
                "Override" => {
                    let part = required_attr(&element, "PartName", "[Content_Types].xml")?;
                    let content_type =
                        required_attr(&element, "ContentType", "[Content_Types].xml")?;
                    if !part.starts_with('/') {
                        return Err(malformed(Some("[Content_Types].xml"), "unsafe PartName"));
                    }
                    validate_part_name(&part[1..])?;
                    result.overrides.try_reserve(1).map_err(|error| {
                        limit(
                            "max_memory_bytes",
                            format!("cannot reserve content-type override: {error}"),
                        )
                    })?;
                    result
                        .overrides
                        .push((try_clone_string(&part[1..], "override part")?, content_type));
                }
                "Default" => {
                    let mut extension =
                        required_attr(&element, "Extension", "[Content_Types].xml")?;
                    extension.make_ascii_lowercase();
                    let content_type =
                        required_attr(&element, "ContentType", "[Content_Types].xml")?;
                    if extension.is_empty() || extension.contains(['/', '\\', '.']) {
                        return Err(malformed(Some("[Content_Types].xml"), "unsafe Extension"));
                    }
                    result.defaults.try_reserve(1).map_err(|error| {
                        limit(
                            "max_memory_bytes",
                            format!("cannot reserve content-type default: {error}"),
                        )
                    })?;
                    result.defaults.push((extension, content_type));
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    result.overrides.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if result.overrides.windows(2).any(|values| values[0].0 == values[1].0) {
        return Err(malformed(Some("[Content_Types].xml"), "duplicate Override"));
    }
    result.defaults.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if result.defaults.windows(2).any(|values| values[0].0 == values[1].0) {
        return Err(malformed(Some("[Content_Types].xml"), "duplicate Default"));
    }
    Ok(result)
}
