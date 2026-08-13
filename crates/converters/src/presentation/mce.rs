use super::error::{limit, malformed};
use super::schema::{A_NS, C_NS, MC_NS, P_NS, R_NS};
use super::xml_base::required_attr;
use into_markdown_core::ConversionError;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::reader::NsReader;

pub(super) fn validate_mc_requires(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    part: &str,
) -> Result<(), ConversionError> {
    let value = required_attr(element, "Requires", part)?;
    let mut count = 0_usize;
    for prefix in value.split_ascii_whitespace() {
        count = count
            .checked_add(1)
            .ok_or_else(|| limit("max_field_bytes", "mc:Requires token count overflow"))?;
        let mut bytes = prefix.bytes();
        let valid_first =
            bytes.next().is_some_and(|value| value == b'_' || value.is_ascii_alphabetic());
        if !valid_first
            || !bytes.all(|value| {
                value == b'_' || value == b'-' || value == b'.' || value.is_ascii_alphanumeric()
            })
        {
            return Err(malformed(Some(part), "mc:Requires contains an invalid prefix"));
        }
        let mut qualified = Vec::new();
        qualified.try_reserve_exact(prefix.len().saturating_add(2)).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve mc:Requires prefix: {error}"))
        })?;
        qualified.extend_from_slice(prefix.as_bytes());
        qualified.extend_from_slice(b":x");
        if !matches!(reader.resolve_element(QName(&qualified)).0, ResolveResult::Bound(_)) {
            return Err(malformed(Some(part), "mc:Requires prefix is not declared"));
        }
    }
    if count == 0 {
        return Err(malformed(Some(part), "mc:Choice Requires cannot be empty"));
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct McSelection {
    pub(super) alternates: Vec<bool>,
    skipped_depth: usize,
}

impl McSelection {
    pub(super) fn skip(
        &mut self,
        reader: &NsReader<&[u8]>,
        event: &Event<'_>,
        part: &str,
    ) -> Result<bool, ConversionError> {
        if self.skipped_depth != 0 {
            match event {
                Event::Start(_) => {
                    self.skipped_depth = self.skipped_depth.checked_add(1).ok_or_else(|| {
                        limit("max_nesting_depth", "MCE skipped branch depth overflow")
                    })?;
                }
                Event::End(_) => self.skipped_depth -= 1,
                _ => {}
            }
            return Ok(true);
        }
        let name = match event {
            Event::Start(element) | Event::Empty(element) => reader.resolve_element(element.name()),
            Event::End(element) => reader.resolve_element(element.name()),
            _ => return Ok(false),
        };
        let ResolveResult::Bound(namespace) = name.0 else { return Ok(false) };
        if namespace.as_ref() != MC_NS {
            return Ok(false);
        }
        match (name.1.as_ref(), event) {
            (b"AlternateContent", Event::Start(_)) => {
                self.alternates.try_reserve(1).map_err(|error| {
                    limit(
                        "max_memory_bytes",
                        format!("cannot reserve MCE selection state: {error}"),
                    )
                })?;
                self.alternates.push(false);
            }
            (b"AlternateContent", Event::End(_)) => {
                self.alternates
                    .pop()
                    .ok_or_else(|| malformed(Some(part), "MCE selection stack underflow"))?;
            }
            (b"Choice", Event::Start(element)) => {
                let selected =
                    self.alternates.last().copied().ok_or_else(|| {
                        malformed(Some(part), "mc:Choice outside AlternateContent")
                    })?;
                if selected || !mc_choice_is_understood(reader, element, part)? {
                    self.skipped_depth = 1;
                } else if let Some(selected) = self.alternates.last_mut() {
                    *selected = true;
                }
            }
            (b"Choice", Event::Empty(element)) => {
                let selected =
                    self.alternates.last().copied().ok_or_else(|| {
                        malformed(Some(part), "mc:Choice outside AlternateContent")
                    })?;
                if !selected
                    && mc_choice_is_understood(reader, element, part)?
                    && let Some(selected) = self.alternates.last_mut()
                {
                    *selected = true;
                }
            }
            (b"Fallback", Event::Start(_)) => {
                let selected =
                    self.alternates.last().copied().ok_or_else(|| {
                        malformed(Some(part), "mc:Fallback outside AlternateContent")
                    })?;
                if selected {
                    self.skipped_depth = 1;
                } else if let Some(selected) = self.alternates.last_mut() {
                    *selected = true;
                }
            }
            (b"Fallback", Event::Empty(_)) => {
                let selected = self
                    .alternates
                    .last_mut()
                    .ok_or_else(|| malformed(Some(part), "mc:Fallback outside AlternateContent"))?;
                if !*selected {
                    *selected = true;
                }
            }
            _ => {}
        }
        Ok(true)
    }
}

pub(super) fn mc_choice_is_understood(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    part: &str,
) -> Result<bool, ConversionError> {
    let requires = required_attr(element, "Requires", part)?;
    for prefix in requires.split_ascii_whitespace() {
        let mut qualified = Vec::new();
        qualified.try_reserve_exact(prefix.len().saturating_add(2)).map_err(|error| {
            limit("max_memory_bytes", format!("cannot reserve MCE namespace probe: {error}"))
        })?;
        qualified.extend_from_slice(prefix.as_bytes());
        qualified.extend_from_slice(b":x");
        let ResolveResult::Bound(namespace) = reader.resolve_element(QName(&qualified)).0 else {
            return Err(malformed(Some(part), "mc:Requires prefix is not declared"));
        };
        if ![P_NS, A_NS, C_NS, R_NS].contains(&namespace.as_ref()) {
            return Ok(false);
        }
    }
    Ok(true)
}
