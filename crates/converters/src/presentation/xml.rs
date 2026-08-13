use super::allocation::try_clone_bytes;
use super::budget::{MAX_XML_EVENTS, MAX_XML_WIDTH};
use super::error::{limit, malformed};
use super::mce::{McSelection, mc_choice_is_understood, validate_mc_requires};
use super::schema::{A_NS, C_NS, MC_NS, P_NS, R_NS, REL_NS, TYPES_NS};
use super::xml_base::{attr, level_paragraph, resolved};
use crate::docx::{decode_cdata, decode_reference, decode_text, decode_xml_attribute};
use into_markdown_core::{ConversionError, ConversionOptions, ExecutionContext};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum XmlProfile {
    Types,
    Relationships,
    Presentation,
    Slide,
    Notes,
    Layout,
    Master,
    Theme,
    Chart,
}

impl XmlProfile {
    fn root(self) -> (&'static [u8], &'static [u8]) {
        match self {
            Self::Types => (TYPES_NS, b"Types"),
            Self::Relationships => (REL_NS, b"Relationships"),
            Self::Presentation => (P_NS, b"presentation"),
            Self::Slide => (P_NS, b"sld"),
            Self::Notes => (P_NS, b"notes"),
            Self::Layout => (P_NS, b"sldLayout"),
            Self::Master => (P_NS, b"sldMaster"),
            Self::Theme => (A_NS, b"theme"),
            Self::Chart => (C_NS, b"chartSpace"),
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn preflight_xml(
    bytes: &[u8],
    part: &str,
    profile: XmlProfile,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    if contains_ascii_case_insensitive(bytes, b"<!doctype")
        || contains_ascii_case_insensitive(bytes, b"<!entity")
    {
        return Err(malformed(Some(part), "DTD and entity declarations are forbidden"));
    }
    let mut reader = NsReader::from_reader(bytes);
    let config = reader.config_mut();
    config.allow_dangling_amp = false;
    config.allow_unmatched_ends = false;
    config.check_end_names = true;
    config.check_comments = true;
    let maximum_depth = usize::from(options.limits.max_nesting_depth);
    let working_set_bytes = u64::try_from(bytes.len())
        .unwrap_or(u64::MAX)
        .checked_mul(4)
        .and_then(|value| {
            value.checked_add(u64::try_from(maximum_depth).unwrap_or(u64::MAX).checked_mul(128)?)
        })
        .and_then(|value| value.checked_add(4096))
        .ok_or_else(|| limit("max_memory_bytes", "XML preflight working-set plan overflow"))?;
    let _working_set = context.reserve_memory(working_set_bytes)?;
    let mut stack = Vec::<(Vec<u8>, Vec<u8>)>::new();
    stack.try_reserve_exact(maximum_depth).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve XML stack for {part}: {error}"))
    })?;
    let mut root = false;
    let width_slots = maximum_depth
        .checked_add(1)
        .ok_or_else(|| limit("max_memory_bytes", "XML width slot count overflow"))?;
    let mut width = Vec::<usize>::new();
    width.try_reserve_exact(width_slots).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve XML width counters: {error}"))
    })?;
    width.resize(width_slots, 0);
    let mut alternate = Vec::<(usize, usize, bool, bool)>::new();
    alternate.try_reserve_exact(maximum_depth).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve MC stack for {part}: {error}"))
    })?;
    // Structural validation intentionally visits every MCE branch. This second selector tracks
    // only the interpreted branch so profile-level cardinality checks cannot be influenced by an
    // unsupported Choice while malformed XML in that Choice is still rejected above.
    let mut semantic_mc = McSelection::default();
    semantic_mc.alternates.try_reserve_exact(maximum_depth).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve semantic MC stack for {part}: {error}"))
    })?;
    let mut master_tx_styles_seen = false;
    let mut master_text_sections_seen = 0_u8;
    let mut events = 0_usize;
    loop {
        context.checkpoint()?;
        let event = reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?;
        if !matches!(event, Event::Eof) {
            events = events
                .checked_add(1)
                .ok_or_else(|| limit("xml_events", "XML event count overflow"))?;
            if events > MAX_XML_EVENTS {
                return Err(limit("xml_events", format!("XML part {part}")));
            }
        }
        let interpreted = !semantic_mc.skip(&reader, &event, part)?;
        match event {
            Event::Start(element) => {
                let name = resolved(&reader, element.name(), part)?;
                if stack.is_empty() {
                    if root || name.0.as_slice() != profile.root().0 || name.1 != profile.root().1 {
                        return Err(malformed(Some(part), "unexpected XML root or namespace"));
                    }
                    root = true;
                }
                let depth = stack.len().saturating_add(1);
                if depth > maximum_depth {
                    return Err(limit("max_nesting_depth", format!("XML part {part}")));
                }
                width[depth] = width[depth].saturating_add(1);
                if width[depth] > MAX_XML_WIDTH {
                    return Err(limit("xml_width", format!("XML part {part}")));
                }
                let parent = if name.0 == MC_NS {
                    stack.last()
                } else {
                    stack.iter().rev().find(|value| value.0 != MC_NS)
                };
                if name.0 != MC_NS
                    && stack
                        .last()
                        .is_some_and(|value| value.0 == MC_NS && value.1 == b"AlternateContent")
                {
                    return Err(malformed(
                        Some(part),
                        "AlternateContent payload must be inside Choice or Fallback",
                    ));
                }
                validate_interpreted_element(profile, &name, parent, part)?;
                validate_attributes(&reader, &element, &name, part, options)?;
                if interpreted {
                    validate_master_text_style_cardinality(
                        profile,
                        &name,
                        part,
                        &mut master_tx_styles_seen,
                        &mut master_text_sections_seen,
                    )?;
                }
                if name.0 == MC_NS && name.1 == b"AlternateContent" {
                    alternate.push((0, 0, false, false));
                } else if name.0 == MC_NS && name.1 == b"Choice" {
                    validate_mc_requires(&reader, &element, part)?;
                    let understood = mc_choice_is_understood(&reader, &element, part)?;
                    let branch = alternate.last_mut().ok_or_else(|| {
                        malformed(Some(part), "mc:Choice outside AlternateContent")
                    })?;
                    if branch.2 {
                        return Err(malformed(Some(part), "mc:Choice cannot follow mc:Fallback"));
                    }
                    branch.0 += 1;
                    branch.3 |= understood;
                } else if name.0 == MC_NS && name.1 == b"Fallback" {
                    if attr(&element, "Requires", part)?.is_some() {
                        return Err(malformed(Some(part), "mc:Fallback cannot declare Requires"));
                    }
                    let branch = alternate.last_mut().ok_or_else(|| {
                        malformed(Some(part), "mc:Fallback outside AlternateContent")
                    })?;
                    branch.1 += 1;
                    branch.2 = true;
                }
                stack.push(name);
            }
            Event::Empty(element) => {
                let name = resolved(&reader, element.name(), part)?;
                if stack.is_empty() {
                    if root || name.0.as_slice() != profile.root().0 || name.1 != profile.root().1 {
                        return Err(malformed(Some(part), "unexpected XML root or namespace"));
                    }
                    root = true;
                }
                let parent = if name.0 == MC_NS {
                    stack.last()
                } else {
                    stack.iter().rev().find(|value| value.0 != MC_NS)
                };
                if name.0 != MC_NS
                    && stack
                        .last()
                        .is_some_and(|value| value.0 == MC_NS && value.1 == b"AlternateContent")
                {
                    return Err(malformed(
                        Some(part),
                        "AlternateContent payload must be inside Choice or Fallback",
                    ));
                }
                validate_interpreted_element(profile, &name, parent, part)?;
                validate_attributes(&reader, &element, &name, part, options)?;
                if interpreted {
                    validate_master_text_style_cardinality(
                        profile,
                        &name,
                        part,
                        &mut master_tx_styles_seen,
                        &mut master_text_sections_seen,
                    )?;
                }
                if name.0 == MC_NS && name.1 == b"AlternateContent" {
                    return Err(malformed(Some(part), "empty AlternateContent is invalid"));
                } else if name.0 == MC_NS && name.1 == b"Choice" {
                    validate_mc_requires(&reader, &element, part)?;
                    let understood = mc_choice_is_understood(&reader, &element, part)?;
                    let branch = alternate.last_mut().ok_or_else(|| {
                        malformed(Some(part), "mc:Choice outside AlternateContent")
                    })?;
                    if branch.2 {
                        return Err(malformed(Some(part), "mc:Choice cannot follow mc:Fallback"));
                    }
                    branch.0 += 1;
                    branch.3 |= understood;
                } else if name.0 == MC_NS && name.1 == b"Fallback" {
                    if attr(&element, "Requires", part)?.is_some() {
                        return Err(malformed(Some(part), "mc:Fallback cannot declare Requires"));
                    }
                    let branch = alternate.last_mut().ok_or_else(|| {
                        malformed(Some(part), "mc:Fallback outside AlternateContent")
                    })?;
                    branch.1 += 1;
                    branch.2 = true;
                }
                let depth = stack.len().saturating_add(1);
                if depth > maximum_depth {
                    return Err(limit("max_nesting_depth", format!("XML part {part}")));
                }
                width[depth] = width[depth].saturating_add(1);
                if width[depth] > MAX_XML_WIDTH {
                    return Err(limit("xml_width", format!("XML part {part}")));
                }
            }
            Event::End(element) => {
                let actual = resolved(&reader, element.name(), part)?;
                if stack.pop().as_ref() != Some(&actual) {
                    return Err(malformed(Some(part), "XML end namespace differs from start"));
                }
                if actual.0 == MC_NS && actual.1 == b"AlternateContent" {
                    let (choices, fallbacks, _, understood) = alternate
                        .pop()
                        .ok_or_else(|| malformed(Some(part), "invalid AlternateContent nesting"))?;
                    if choices == 0 || fallbacks > 1 || (!understood && fallbacks != 1) {
                        return Err(malformed(
                            Some(part),
                            "AlternateContent requires Choice and a unique fallback when no choice is understood",
                        ));
                    }
                }
            }
            Event::Text(text) => {
                let value = decode_text(&text, part)?;
                if stack.is_empty() && !value.trim().is_empty() {
                    return Err(malformed(Some(part), "text is not allowed outside the XML root"));
                }
            }
            Event::CData(text) => {
                if stack.is_empty() {
                    return Err(malformed(Some(part), "CDATA is not allowed outside the XML root"));
                }
                decode_cdata(&text, part)?;
            }
            Event::GeneralRef(reference) => {
                if stack.is_empty() {
                    return Err(malformed(
                        Some(part),
                        "character references are not allowed outside the XML root",
                    ));
                }
                decode_reference(&reference, part)?;
            }
            Event::DocType(_) => return Err(malformed(Some(part), "DOCTYPE is forbidden")),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root || !stack.is_empty() || !alternate.is_empty() {
        return Err(malformed(Some(part), "XML root is missing or incomplete"));
    }
    Ok(())
}

fn validate_master_text_style_cardinality(
    profile: XmlProfile,
    name: &(Vec<u8>, Vec<u8>),
    part: &str,
    tx_styles_seen: &mut bool,
    sections_seen: &mut u8,
) -> Result<(), ConversionError> {
    if profile != XmlProfile::Master || name.0.as_slice() != P_NS {
        return Ok(());
    }
    if name.1 == b"txStyles" {
        if *tx_styles_seen {
            return Err(malformed(Some(part), "master has multiple txStyles elements"));
        }
        *tx_styles_seen = true;
        return Ok(());
    }
    let flag = match name.1.as_slice() {
        b"titleStyle" => 1,
        b"bodyStyle" => 2,
        b"otherStyle" => 4,
        _ => return Ok(()),
    };
    if *sections_seen & flag != 0 {
        return Err(malformed(Some(part), "master txStyles has a duplicate section"));
    }
    *sections_seen |= flag;
    Ok(())
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window.iter().zip(needle).all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

#[allow(clippy::too_many_lines, clippy::match_same_arms)]
fn validate_interpreted_element(
    profile: XmlProfile,
    name: &(Vec<u8>, Vec<u8>),
    parent: Option<&(Vec<u8>, Vec<u8>)>,
    part: &str,
) -> Result<(), ConversionError> {
    let ns = name.0.as_slice();
    let local = name.1.as_slice();
    let parent_is = |namespace: &[u8], local: &[u8]| {
        parent.is_some_and(|value| value.0 == namespace && value.1 == local)
    };
    if parent_is(A_NS, b"t") || parent_is(C_NS, b"v") {
        return Err(malformed(
            Some(part),
            "text-bearing DrawingML and chart elements cannot contain child elements",
        ));
    }
    let expected = match local {
        b"Types" | b"Override" | b"Default" => Some(TYPES_NS),
        b"Relationships" | b"Relationship" => Some(REL_NS),
        b"AlternateContent" | b"Choice" | b"Fallback" => Some(MC_NS),
        b"presentation" | b"sldIdLst" | b"sldId" | b"sld" | b"notes" | b"sldLayout"
        | b"sldMaster" | b"cSld" | b"spTree" | b"sp" | b"pic" | b"graphicFrame" | b"grpSp"
        | b"grpSpPr" | b"nvSpPr" | b"nvPicPr" | b"nvGraphicFramePr" | b"nvGrpSpPr" | b"cNvPr"
        | b"cNvSpPr" | b"cNvPicPr" | b"cNvGraphicFramePr" | b"cNvGrpSpPr" | b"nvPr" | b"ph"
        | b"spPr" | b"blipFill" => Some(P_NS),
        b"txStyles" | b"titleStyle" | b"bodyStyle" | b"otherStyle" => Some(P_NS),
        b"theme" | b"graphic" | b"graphicData" | b"tbl" | b"tr" | b"tc" | b"tcPr" | b"p"
        | b"pPr" | b"r" | b"rPr" | b"defRPr" | b"t" | b"br" | b"buChar" | b"buNone" | b"buBlip"
        | b"bodyPr" | b"lstStyle" | b"buAutoNum" | b"off" | b"ext" | b"chOff" | b"chExt"
        | b"blip" => Some(A_NS),
        b"chartSpace" | b"chart" | b"plotArea" | b"ser" | b"cat" | b"val" | b"tx" | b"strRef"
        | b"numRef" | b"strCache" | b"numCache" | b"pt" | b"v" => Some(C_NS),
        _ => None,
    };
    if expected.is_some_and(|expected| expected != ns) {
        return Err(malformed(
            Some(part),
            format!(
                "interpreted element {} has an unexpected namespace",
                String::from_utf8_lossy(local)
            ),
        ));
    }
    if local == b"xfrm" && !matches!(ns, P_NS | A_NS) {
        return Err(malformed(Some(part), "xfrm has an unexpected namespace"));
    }
    if local == b"txBody" && !matches!(ns, P_NS | A_NS) {
        return Err(malformed(Some(part), "txBody has an unexpected namespace"));
    }
    let valid = match (ns, local) {
        (TYPES_NS, b"Override" | b"Default") => parent_is(TYPES_NS, b"Types"),
        (REL_NS, b"Relationship") => parent_is(REL_NS, b"Relationships"),
        (P_NS, b"sldIdLst") => parent_is(P_NS, b"presentation"),
        (P_NS, b"sldId") => parent_is(P_NS, b"sldIdLst"),
        (P_NS, b"cSld") => match profile {
            XmlProfile::Slide => parent_is(P_NS, b"sld"),
            XmlProfile::Notes => parent_is(P_NS, b"notes"),
            XmlProfile::Layout => parent_is(P_NS, b"sldLayout"),
            XmlProfile::Master => parent_is(P_NS, b"sldMaster"),
            _ => false,
        },
        (P_NS, b"spTree") => parent_is(P_NS, b"cSld"),
        (P_NS, b"grpSp") => parent_is(P_NS, b"spTree") || parent_is(P_NS, b"grpSp"),
        (P_NS, b"grpSpPr") => parent_is(P_NS, b"grpSp"),
        (P_NS, b"sp" | b"pic" | b"graphicFrame") => {
            parent_is(P_NS, b"spTree") || parent_is(P_NS, b"grpSp")
        }
        (P_NS, b"nvSpPr") => parent_is(P_NS, b"sp"),
        (P_NS, b"nvPicPr") => parent_is(P_NS, b"pic"),
        (P_NS, b"nvGraphicFramePr") => parent_is(P_NS, b"graphicFrame"),
        (P_NS, b"nvGrpSpPr") => parent_is(P_NS, b"grpSp"),
        (P_NS, b"cNvPr") => matches!(
            parent,
            Some((namespace, local))
                if namespace == P_NS
                    && matches!(
                        local.as_slice(),
                        b"nvSpPr" | b"nvPicPr" | b"nvGraphicFramePr" | b"nvGrpSpPr"
                    )
        ),
        (P_NS, b"cNvSpPr") => parent_is(P_NS, b"nvSpPr"),
        (P_NS, b"cNvPicPr") => parent_is(P_NS, b"nvPicPr"),
        (P_NS, b"cNvGraphicFramePr") => parent_is(P_NS, b"nvGraphicFramePr"),
        (P_NS, b"cNvGrpSpPr") => parent_is(P_NS, b"nvGrpSpPr"),
        (P_NS, b"nvPr") => matches!(
            parent,
            Some((namespace, local))
                if namespace == P_NS
                    && matches!(
                        local.as_slice(),
                        b"nvSpPr" | b"nvPicPr" | b"nvGraphicFramePr" | b"nvGrpSpPr"
                    )
        ),
        (P_NS, b"ph") => parent_is(P_NS, b"nvPr"),
        (P_NS, b"spPr") => parent_is(P_NS, b"sp") || parent_is(P_NS, b"pic"),
        (P_NS, b"txBody") => parent_is(P_NS, b"sp"),
        (A_NS, b"txBody") => parent_is(A_NS, b"tc"),
        (P_NS, b"blipFill") => parent_is(P_NS, b"pic"),
        (P_NS, b"txStyles") => parent_is(P_NS, b"sldMaster"),
        (P_NS, b"titleStyle" | b"bodyStyle" | b"otherStyle") => parent_is(P_NS, b"txStyles"),
        (P_NS, b"xfrm") => parent_is(P_NS, b"graphicFrame"),
        (A_NS, b"xfrm") => parent_is(P_NS, b"spPr") || parent_is(P_NS, b"grpSpPr"),
        (A_NS, b"off" | b"ext") => parent_is(A_NS, b"xfrm") || parent_is(P_NS, b"xfrm"),
        (A_NS, b"chOff" | b"chExt") => parent_is(A_NS, b"xfrm"),
        (A_NS, b"bodyPr" | b"lstStyle") => parent_is(P_NS, b"txBody") || parent_is(A_NS, b"txBody"),
        (A_NS, b"p") => parent_is(P_NS, b"txBody") || parent_is(A_NS, b"txBody"),
        (A_NS, b"pPr") => parent_is(A_NS, b"p"),
        (A_NS, local) if level_paragraph(local).is_some() => matches!(
            parent,
            Some((namespace, local))
                if namespace == P_NS
                    && matches!(local.as_slice(), b"titleStyle" | b"bodyStyle" | b"otherStyle")
        ),
        (A_NS, b"r") => parent_is(A_NS, b"p"),
        (A_NS, b"rPr") => parent_is(A_NS, b"r"),
        (A_NS, b"defRPr") => {
            parent_is(A_NS, b"pPr")
                || parent.is_some_and(|(namespace, local)| {
                    namespace == A_NS && level_paragraph(local).is_some()
                })
        }
        (A_NS, b"t") => parent_is(A_NS, b"r") || parent_is(A_NS, b"fld"),
        (A_NS, b"br") => parent_is(A_NS, b"p"),
        (A_NS, b"buChar" | b"buAutoNum" | b"buNone" | b"buBlip") => {
            parent_is(A_NS, b"pPr")
                || parent.is_some_and(|(namespace, local)| {
                    namespace == A_NS && level_paragraph(local).is_some()
                })
        }
        (A_NS, b"graphic") => parent_is(P_NS, b"graphicFrame"),
        (A_NS, b"graphicData") => parent_is(A_NS, b"graphic"),
        (A_NS, b"blip") => parent_is(P_NS, b"blipFill") || parent_is(A_NS, b"blipFill"),
        (A_NS, b"tbl") => parent_is(A_NS, b"graphicData"),
        (A_NS, b"tr") => parent_is(A_NS, b"tbl"),
        (A_NS, b"tc") => parent_is(A_NS, b"tr"),
        (A_NS, b"tcPr") => parent_is(A_NS, b"tc"),
        (C_NS, b"chart") => parent_is(C_NS, b"chartSpace") || parent_is(A_NS, b"graphicData"),
        (C_NS, b"plotArea") => parent_is(C_NS, b"chart"),
        (C_NS, b"ser") => parent_is(C_NS, b"plotArea"),
        (C_NS, b"cat" | b"val" | b"tx") => parent_is(C_NS, b"ser"),
        (C_NS, b"strRef" | b"numRef") => {
            parent_is(C_NS, b"cat") || parent_is(C_NS, b"val") || parent_is(C_NS, b"tx")
        }
        (C_NS, b"strCache" | b"numCache") => {
            parent_is(C_NS, b"strRef") || parent_is(C_NS, b"numRef")
        }
        (C_NS, b"pt") => parent_is(C_NS, b"strCache") || parent_is(C_NS, b"numCache"),
        (C_NS, b"v") => parent_is(C_NS, b"pt") || parent_is(C_NS, b"tx"),
        (MC_NS, b"Choice" | b"Fallback") => parent_is(MC_NS, b"AlternateContent"),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(malformed(
            Some(part),
            format!(
                "invalid PresentationML hierarchy for {} under {}",
                String::from_utf8_lossy(local),
                parent.map_or_else(
                    || "<root>".into(),
                    |value| String::from_utf8_lossy(&value.1).into_owned()
                )
            ),
        ))
    }
}

fn validate_attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    element_name: &(Vec<u8>, Vec<u8>),
    part: &str,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let attribute_count = element.attributes().count();
    let mut seen = Vec::<(Vec<u8>, Vec<u8>)>::new();
    seen.try_reserve_exact(attribute_count).map_err(|error| {
        limit(
            "max_memory_bytes",
            format!("cannot reserve XML attribute identities for {part}: {error}"),
        )
    })?;
    for attribute in element.attributes() {
        let attribute = attribute
            .map_err(|error| malformed(Some(part), format!("invalid XML attribute: {error}")))?;
        let raw = attribute.key.as_ref();
        let decoded = decode_xml_attribute(attribute.value.as_ref(), part)?;
        if u64::try_from(decoded.len()).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
            return Err(limit("max_field_bytes", format!("XML attribute in {part}")));
        }
        if raw == b"xmlns" || raw.starts_with(b"xmlns:") {
            continue;
        }
        let (namespace, local) = reader.resolve_attribute(attribute.key);
        let namespace = match namespace {
            ResolveResult::Bound(value) => {
                std::str::from_utf8(value.as_ref())
                    .map_err(|_| malformed(Some(part), "attribute namespace is not UTF-8"))?;
                try_clone_bytes(value.as_ref(), "attribute namespace")?
            }
            ResolveResult::Unbound => Vec::new(),
            ResolveResult::Unknown(_) => {
                return Err(malformed(Some(part), "undeclared attribute namespace prefix"));
            }
        };
        std::str::from_utf8(local.as_ref())
            .map_err(|_| malformed(Some(part), "attribute local name is not UTF-8"))?;
        if matches!(element_name.0.as_slice(), TYPES_NS | REL_NS) && !namespace.is_empty() {
            return Err(malformed(
                Some(part),
                "OPC content-type and relationship attributes must be unqualified",
            ));
        }
        let invalid_relationship_namespace =
            match (element_name.0.as_slice(), element_name.1.as_slice()) {
                (P_NS, b"sldId") if local.as_ref() == b"id" => {
                    !namespace.is_empty() && namespace != R_NS
                }
                (A_NS, b"blip") if matches!(local.as_ref(), b"embed" | b"link") => {
                    namespace != R_NS
                }
                (C_NS, b"chart") if local.as_ref() == b"id" => namespace != R_NS,
                _ => false,
            };
        if invalid_relationship_namespace {
            return Err(malformed(
                Some(part),
                "relationship attribute has an unexpected namespace",
            ));
        }
        seen.push((namespace, try_clone_bytes(local.as_ref(), "attribute local name")?));
    }
    seen.sort_unstable();
    if seen.windows(2).any(|values| values[0] == values[1]) {
        return Err(malformed(Some(part), "duplicate XML attribute"));
    }
    Ok(())
}
