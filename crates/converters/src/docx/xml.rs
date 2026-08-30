#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XmlProfile {
    ContentTypes,
    Relationships,
    Document,
    Header,
    Footer,
    Styles,
    Numbering,
    Comments,
    Footnotes,
    Endnotes,
    CoreProperties,
}

impl XmlProfile {
    fn root(self) -> (&'static [u8], &'static [u8]) {
        match self {
            Self::ContentTypes => (CONTENT_TYPES_NS, b"Types"),
            Self::Relationships => (PACKAGE_REL_NS, b"Relationships"),
            Self::Document => (WORD_NS, b"document"),
            Self::Header => (WORD_NS, b"hdr"),
            Self::Footer => (WORD_NS, b"ftr"),
            Self::Styles => (WORD_NS, b"styles"),
            Self::Numbering => (WORD_NS, b"numbering"),
            Self::Comments => (WORD_NS, b"comments"),
            Self::Footnotes => (WORD_NS, b"footnotes"),
            Self::Endnotes => (WORD_NS, b"endnotes"),
            Self::CoreProperties => (CORE_PROPERTIES_NS, b"coreProperties"),
        }
    }
}

fn interpreted_word_local(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
    part: &str,
) -> Result<Option<String>, ConversionError> {
    let (namespace, local_name) = reader.resolve_element(name);
    let namespace = match namespace {
        ResolveResult::Bound(value) => value,
        ResolveResult::Unbound => return Ok(None),
        ResolveResult::Unknown(prefix) => {
            return Err(malformed(
                Some(part),
                format!("undeclared XML namespace prefix {}", String::from_utf8_lossy(&prefix)),
            ));
        }
    };
    let local_name = local_name.as_ref();
    let interpreted = matches!(namespace.as_ref(), WORD_NS | STRICT_WORD_NS)
        || namespace.as_ref() == MC_NS
            && matches!(local_name, b"AlternateContent" | b"Choice" | b"Fallback")
        || namespace.as_ref() == MATH_NS && matches!(local_name, b"oMath" | b"r" | b"t")
        || namespace.as_ref() == DRAWING_NS && local_name == b"blip"
        || namespace.as_ref() == WORD_DRAWING_NS && local_name == b"docPr"
        || namespace.as_ref() == VML_NS && local_name == b"imagedata"
        || namespace.as_ref() == CHART_NS && local_name == b"chart"
        || namespace.as_ref() == DIAGRAM_NS && local_name == b"relIds"
        || namespace.as_ref() == OFFICE_VML_NS && local_name == b"OLEObject";
    interpreted
        .then(|| {
            std::str::from_utf8(local_name)
                .map(str::to_owned)
                .map_err(|_| malformed(Some(part), "XML local name is not UTF-8"))
        })
        .transpose()
}

fn reject_dangerous_xml(bytes: &[u8], part: &str) -> Result<(), ConversionError> {
    let lower = bytes.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    if lower.windows(9).any(|v| v == b"<!doctype") || lower.windows(8).any(|v| v == b"<!entity") {
        return Err(malformed(Some(part), "DTD and entity declarations are forbidden"));
    }
    Ok(())
}

fn enforce_field_limit(value: &str, options: &ConversionOptions) -> Result<(), ConversionError> {
    let size = u64::try_from(value.len()).unwrap_or(u64::MAX);
    if size > options.limits.max_field_bytes {
        Err(limit("max_field_bytes", format!("{size} > {}", options.limits.max_field_bytes)))
    } else {
        Ok(())
    }
}

fn xml_budget(bytes: &[u8], options: &ConversionOptions) -> Result<(), ConversionError> {
    let events = u64::try_from(bytes.len()).unwrap_or(u64::MAX).saturating_mul(XML_EVENT_FACTOR);
    let permitted = options.limits.max_decompressed_bytes.saturating_mul(XML_EVENT_FACTOR);
    if events > permitted {
        return Err(limit("max_decompressed_bytes", "XML event budget exceeded"));
    }
    let mut reader = Reader::from_reader(bytes);
    let mut depth = 0_u16;
    loop {
        match reader
            .read_event()
            .map_err(|error| malformed(None, format!("invalid package XML: {error}")))?
        {
            Event::Start(_) => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| limit("max_nesting_depth", "XML depth overflow"))?;
                if depth > options.limits.max_nesting_depth {
                    return Err(limit(
                        "max_nesting_depth",
                        format!("{depth} > {}", options.limits.max_nesting_depth),
                    ));
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::DocType(_) => return Err(malformed(None, "DOCTYPE is forbidden")),
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 {
        return Err(malformed(None, "truncated package XML structure"));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn preflight_xml(
    bytes: &[u8],
    part: &str,
    profile: XmlProfile,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    reject_dangerous_xml(bytes, part)?;
    xml_budget(bytes, options)?;
    let mut reader = NsReader::from_reader(bytes);
    let config = reader.config_mut();
    config.allow_dangling_amp = false;
    config.allow_unmatched_ends = false;
    config.check_end_names = true;
    config.check_comments = true;
    let mut stack = Vec::<(Vec<u8>, Vec<u8>)>::new();
    let mut root_seen = false;
    let mut body_seen = false;
    let mut alternate = Vec::<(usize, usize)>::new();
    loop {
        context.checkpoint()?;
        match reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?
        {
            Event::Start(element) => {
                let name = resolved_element(&reader, element.name(), part)?;
                if stack.is_empty() {
                    if root_seen {
                        return Err(malformed(Some(part), "XML contains multiple roots"));
                    }
                    let root = profile.root();
                    if name.0.as_slice() != root.0 || name.1.as_slice() != root.1 {
                        return Err(malformed(Some(part), "unexpected XML root or namespace"));
                    }
                    root_seen = true;
                }
                validate_xml_element(
                    profile,
                    &name,
                    &stack,
                    part,
                    options.error_policy == into_markdown_core::ErrorPolicy::Strict,
                )?;
                validate_xml_attributes(
                    &reader,
                    &element,
                    &name,
                    part,
                    options.error_policy == into_markdown_core::ErrorPolicy::Strict,
                )?;
                if profile == XmlProfile::Document
                    && name.0.as_slice() == WORD_NS
                    && name.1.as_slice() == b"body"
                {
                    if body_seen {
                        return Err(malformed(Some(part), "document contains multiple bodies"));
                    }
                    body_seen = true;
                }
                if name.0.as_slice() == MC_NS && name.1.as_slice() == b"AlternateContent" {
                    alternate.push((0, 0));
                } else if name.0.as_slice() == MC_NS && name.1.as_slice() == b"Choice" {
                    let Some((choices, _)) = alternate.last_mut() else {
                        return Err(malformed(Some(part), "mc:Choice is outside AlternateContent"));
                    };
                    *choices += 1;
                } else if name.0.as_slice() == MC_NS && name.1.as_slice() == b"Fallback" {
                    let Some((_, fallbacks)) = alternate.last_mut() else {
                        return Err(malformed(
                            Some(part),
                            "mc:Fallback is outside AlternateContent",
                        ));
                    };
                    *fallbacks += 1;
                }
                stack.push(name);
            }
            Event::Empty(element) => {
                let name = resolved_element(&reader, element.name(), part)?;
                if stack.is_empty() {
                    return Err(malformed(Some(part), "package XML root cannot be empty"));
                }
                validate_xml_element(
                    profile,
                    &name,
                    &stack,
                    part,
                    options.error_policy == into_markdown_core::ErrorPolicy::Strict,
                )?;
                validate_xml_attributes(
                    &reader,
                    &element,
                    &name,
                    part,
                    options.error_policy == into_markdown_core::ErrorPolicy::Strict,
                )?;
                if name.0.as_slice() == MC_NS && name.1.as_slice() == b"AlternateContent" {
                    return Err(malformed(
                        Some(part),
                        "empty AlternateContent has no selected branch",
                    ));
                }
                if name.0.as_slice() == MC_NS && name.1.as_slice() == b"Choice" {
                    let Some((choices, _)) = alternate.last_mut() else {
                        return Err(malformed(Some(part), "mc:Choice is outside AlternateContent"));
                    };
                    *choices += 1;
                }
                if name.0.as_slice() == MC_NS && name.1.as_slice() == b"Fallback" {
                    let Some((_, fallbacks)) = alternate.last_mut() else {
                        return Err(malformed(
                            Some(part),
                            "mc:Fallback is outside AlternateContent",
                        ));
                    };
                    *fallbacks += 1;
                }
            }
            Event::End(element) => {
                let actual = resolved_element(&reader, element.name(), part)?;
                let expected = stack
                    .pop()
                    .ok_or_else(|| malformed(Some(part), "XML end tag has no start tag"))?;
                if actual != expected {
                    return Err(malformed(Some(part), "XML end namespace differs from start"));
                }
                if actual.0.as_slice() == MC_NS && actual.1.as_slice() == b"AlternateContent" {
                    let (choices, fallbacks) = alternate
                        .pop()
                        .ok_or_else(|| malformed(Some(part), "invalid AlternateContent nesting"))?;
                    if choices == 0 || fallbacks != 1 {
                        return Err(malformed(
                            Some(part),
                            "AlternateContent requires Choice and exactly one Fallback",
                        ));
                    }
                }
            }
            Event::Text(text) => {
                let value = decode_text(&text, part)?;
                if stack.is_empty() && !value.chars().all(char::is_whitespace) {
                    return Err(malformed(Some(part), "character data outside XML root"));
                }
            }
            Event::CData(text) => {
                let value = decode_cdata(&text, part)?;
                if stack.is_empty() && !value.is_empty() {
                    return Err(malformed(Some(part), "CDATA outside XML root"));
                }
            }
            Event::GeneralRef(reference) => {
                let value = decode_reference(&reference, part)?;
                if stack.is_empty() && !value.chars().all(char::is_whitespace) {
                    return Err(malformed(Some(part), "character reference outside XML root"));
                }
            }
            Event::DocType(_) => {
                return Err(malformed(Some(part), "DOCTYPE is forbidden"));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || !stack.is_empty() || !alternate.is_empty() {
        return Err(malformed(Some(part), "XML root is missing or incomplete"));
    }
    if profile == XmlProfile::Document && !body_seen {
        return Err(malformed(Some(part), "Word document body is missing"));
    }
    Ok(())
}

fn resolved_element(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
    part: &str,
) -> Result<(Vec<u8>, Vec<u8>), ConversionError> {
    let (namespace, local) = reader.resolve_element(name);
    let namespace = match namespace {
        ResolveResult::Bound(value) if value.as_ref() == STRICT_WORD_NS => WORD_NS.to_vec(),
        ResolveResult::Bound(value) => value.as_ref().to_vec(),
        ResolveResult::Unbound => Vec::new(),
        ResolveResult::Unknown(prefix) => {
            return Err(malformed(
                Some(part),
                format!("undeclared XML namespace prefix {}", String::from_utf8_lossy(&prefix)),
            ));
        }
    };
    Ok((namespace, local.as_ref().to_vec()))
}

#[allow(clippy::match_same_arms, clippy::too_many_lines)]
fn validate_xml_element(
    profile: XmlProfile,
    name: &(Vec<u8>, Vec<u8>),
    ancestors: &[(Vec<u8>, Vec<u8>)],
    part: &str,
    strict_semantics: bool,
) -> Result<(), ConversionError> {
    let ns = name.0.as_slice();
    let local = name.1.as_slice();
    let depth = ancestors.len() + 1;
    let raw_parent = ancestors.last();
    let parent = semantic_parent(ancestors);
    let parent_is = |namespace: &[u8], value: &[u8]| xml_name_is(parent, namespace, value);
    let raw_parent_is = |namespace: &[u8], value: &[u8]| xml_name_is(raw_parent, namespace, value);
    let has_ancestor = |namespace: &[u8], value: &[u8]| {
        ancestors.iter().any(|ancestor| xml_name_is(Some(ancestor), namespace, value))
    };
    let expected_namespace = match local {
        b"Types" | b"Default" | b"Override" => Some(CONTENT_TYPES_NS),
        b"Relationships" | b"Relationship" => Some(PACKAGE_REL_NS),
        b"AlternateContent" | b"Choice" | b"Fallback" => Some(MC_NS),
        b"oMath" => Some(MATH_NS),
        b"r" | b"t" if ns == MATH_NS => Some(MATH_NS),
        b"blip" => Some(DRAWING_NS),
        b"docPr" => Some(WORD_DRAWING_NS),
        b"imagedata" => Some(VML_NS),
        b"document" | b"hdr" | b"ftr" | b"body" | b"styles" | b"style" | b"name" | b"basedOn"
        | b"outlineLvl" | b"numbering" | b"abstractNum" | b"lvl" | b"numFmt" | b"start"
        | b"lvlText" | b"num" | b"lvlOverride" | b"startOverride" | b"abstractNumId"
        | b"comments" | b"comment" | b"footnotes" | b"footnote" | b"endnotes" | b"endnote"
        | b"p" | b"pPr" | b"pStyle" | b"numPr" | b"numId" | b"ilvl" | b"r" | b"rPr" | b"b"
        | b"i" | b"strike" | b"dstrike" | b"u" | b"vertAlign" | b"tab" | b"br" | b"cr"
        | b"footnoteReference" | b"endnoteReference" | b"commentReference" | b"headerReference"
        | b"footerReference" | b"fldChar" | b"instrText" | b"hyperlink" | b"tbl" | b"tblPr"
        | b"tr" | b"trPr" | b"tc" | b"tcPr" | b"gridSpan" | b"tblHeader" | b"vMerge"
        | b"sectPr" | b"drawing" | b"pict" | b"sdt" | b"sdtPr" | b"sdtContent"
        | b"txbxContent" | b"customXml" | b"smartTag" | b"ins" | b"del" | b"moveFrom"
        | b"moveTo" | b"fldSimple" | b"altChunk" | b"t" => {
            Some(WORD_NS)
        }
        b"coreProperties" | b"keywords" | b"lastModifiedBy" | b"revision" | b"category"
        | b"contentStatus" | b"version" => Some(CORE_PROPERTIES_NS),
        b"title" | b"subject" | b"creator" | b"description" | b"identifier" | b"language" => {
            Some(DUBLIN_CORE_NS)
        }
        b"created" | b"modified" => Some(DUBLIN_CORE_TERMS_NS),
        _ => None,
    };
    if strict_semantics && expected_namespace.is_some_and(|expected| ns != expected) {
        return Err(malformed(
            Some(part),
            format!(
                "interpreted element {} has an unexpected namespace",
                String::from_utf8_lossy(local)
            ),
        ));
    }
    if strict_semantics
        && matches!(
            local,
            b"Types"
                | b"Relationships"
                | b"document"
                | b"hdr"
                | b"ftr"
                | b"styles"
                | b"numbering"
                | b"comments"
                | b"footnotes"
                | b"endnotes"
                | b"coreProperties"
        )
        && depth != 1
    {
        return Err(malformed(Some(part), "package part root appears at a nested level"));
    }
    if strict_semantics
        && local == b"body"
        && (profile != XmlProfile::Document || !parent_is(WORD_NS, b"document"))
    {
        return Err(malformed(Some(part), "w:body is only valid in the main document"));
    }
    if strict_semantics {
        match profile {
            XmlProfile::ContentTypes if depth > 1 && !parent_is(CONTENT_TYPES_NS, b"Types") => {
                return Err(malformed(
                    Some(part),
                    "content type declarations must be direct children",
                ));
            }
            XmlProfile::Relationships
                if depth > 1
                    && !(local == b"Relationship"
                        && parent_is(PACKAGE_REL_NS, b"Relationships")) =>
            {
                return Err(malformed(Some(part), "relationships must be direct children"));
            }
            XmlProfile::Styles => validate_styles_hierarchy(ns, local, parent, ancestors, part)?,
            XmlProfile::Numbering => validate_numbering_hierarchy(ns, local, parent, part)?,
            XmlProfile::Comments
                if matches!(local, b"comment" | b"footnote" | b"endnote")
                    && !(local == b"comment" && parent_is(WORD_NS, b"comments")) =>
            {
                return Err(malformed(
                    Some(part),
                    "invalid annotation definition for comments part",
                ));
            }
            XmlProfile::Footnotes
                if matches!(local, b"comment" | b"footnote" | b"endnote")
                    && !(local == b"footnote" && parent_is(WORD_NS, b"footnotes")) =>
            {
                return Err(malformed(
                    Some(part),
                    "invalid annotation definition for footnotes part",
                ));
            }
            XmlProfile::Endnotes
                if matches!(local, b"comment" | b"footnote" | b"endnote")
                    && !(local == b"endnote" && parent_is(WORD_NS, b"endnotes")) =>
            {
                return Err(malformed(
                    Some(part),
                    "invalid annotation definition for endnotes part",
                ));
            }
            XmlProfile::CoreProperties
                if is_core_property(ns, local)
                    && !(ns == CORE_PROPERTIES_NS && local == b"coreProperties")
                    && !raw_parent_is(CORE_PROPERTIES_NS, b"coreProperties") =>
            {
                return Err(malformed(
                    Some(part),
                    "core properties must be direct coreProperties children",
                ));
            }
            XmlProfile::CoreProperties
                if raw_parent.is_some_and(|name| is_core_property(&name.0, &name.1))
                    && !raw_parent_is(CORE_PROPERTIES_NS, b"coreProperties") =>
            {
                return Err(malformed(Some(part), "core property values must contain text only"));
            }
            _ => {}
        }
    }
    if strict_semantics && is_word_content_profile(profile) {
        if matches!(local, b"comment" | b"footnote" | b"endnote")
            && !matches!(
                profile,
                XmlProfile::Comments | XmlProfile::Footnotes | XmlProfile::Endnotes
            )
        {
            return Err(malformed(Some(part), "annotation definition is invalid for this part"));
        }
        validate_word_content_hierarchy(ns, local, parent, ancestors, part)?;
    } else if strict_semantics
        && is_word_content_semantic(ns, local)
        && !matches!(profile, XmlProfile::Styles | XmlProfile::Numbering)
    {
        return Err(malformed(Some(part), "Word content element is invalid for this part profile"));
    }
    if strict_semantics
        && matches!(local, b"Choice" | b"Fallback")
        && !raw_parent_is(MC_NS, b"AlternateContent")
    {
        return Err(malformed(Some(part), "MC branches must be direct AlternateContent children"));
    }
    if strict_semantics
        && ns == MC_NS
        && local == b"AlternateContent"
        && !has_ancestor(WORD_NS, b"document")
        && !matches!(profile, XmlProfile::Header | XmlProfile::Footer)
    {
        return Err(malformed(Some(part), "AlternateContent is outside Word content"));
    }
    Ok(())
}

fn xml_name_is(name: Option<&(Vec<u8>, Vec<u8>)>, namespace: &[u8], local: &[u8]) -> bool {
    name.is_some_and(|name| name.0.as_slice() == namespace && name.1.as_slice() == local)
}

fn semantic_parent(ancestors: &[(Vec<u8>, Vec<u8>)]) -> Option<&(Vec<u8>, Vec<u8>)> {
    ancestors.iter().rev().find(|name| {
        !(name.0.as_slice() == MC_NS
            && matches!(name.1.as_slice(), b"AlternateContent" | b"Choice" | b"Fallback")
            || name.0.as_slice() == WORD_NS
                && matches!(
                    name.1.as_slice(),
                    b"sdt"
                        | b"sdtContent"
                        | b"txbxContent"
                        | b"customXml"
                        | b"smartTag"
                        | b"ins"
                        | b"del"
                        | b"moveFrom"
                        | b"moveTo"
                        | b"fldSimple"
                ))
    })
}

fn is_word_content_profile(profile: XmlProfile) -> bool {
    matches!(
        profile,
        XmlProfile::Document
            | XmlProfile::Header
            | XmlProfile::Footer
            | XmlProfile::Comments
            | XmlProfile::Footnotes
            | XmlProfile::Endnotes
    )
}

fn is_core_property(namespace: &[u8], local: &[u8]) -> bool {
    matches!(
        (namespace, local),
        (
            CORE_PROPERTIES_NS,
            b"coreProperties"
                | b"keywords"
                | b"lastModifiedBy"
                | b"revision"
                | b"category"
                | b"contentStatus"
                | b"version"
        ) | (
            DUBLIN_CORE_NS,
            b"title" | b"subject" | b"creator" | b"description" | b"identifier" | b"language"
        ) | (DUBLIN_CORE_TERMS_NS, b"created" | b"modified")
    )
}

fn validate_styles_hierarchy(
    namespace: &[u8],
    local: &[u8],
    parent: Option<&(Vec<u8>, Vec<u8>)>,
    ancestors: &[(Vec<u8>, Vec<u8>)],
    part: &str,
) -> Result<(), ConversionError> {
    if namespace != WORD_NS {
        return Ok(());
    }
    let valid = match local {
        b"style" => xml_name_is(parent, WORD_NS, b"styles"),
        b"name" | b"basedOn" => xml_name_is(parent, WORD_NS, b"style"),
        b"pPr" => {
            xml_name_is(parent, WORD_NS, b"style") || xml_name_is(parent, WORD_NS, b"pPrDefault")
        }
        b"rPr" => {
            xml_name_is(parent, WORD_NS, b"style") || xml_name_is(parent, WORD_NS, b"rPrDefault")
        }
        b"outlineLvl" => {
            xml_name_is(parent, WORD_NS, b"pPr")
                && ancestors
                    .iter()
                    .rev()
                    .nth(1)
                    .is_some_and(|name| xml_name_is(Some(name), WORD_NS, b"style"))
        }
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(malformed(
            Some(part),
            format!(
                "invalid styles semantic element hierarchy for {} under {}",
                String::from_utf8_lossy(local),
                parent.map_or_else(
                    || "<root>".into(),
                    |value| String::from_utf8_lossy(&value.1).into_owned()
                )
            ),
        ))
    }
}

fn validate_numbering_hierarchy(
    namespace: &[u8],
    local: &[u8],
    parent: Option<&(Vec<u8>, Vec<u8>)>,
    part: &str,
) -> Result<(), ConversionError> {
    if namespace != WORD_NS {
        return Ok(());
    }
    let valid = match local {
        b"abstractNum" | b"num" => xml_name_is(parent, WORD_NS, b"numbering"),
        b"lvl" => xml_name_is(parent, WORD_NS, b"abstractNum"),
        b"numFmt" | b"start" | b"lvlText" | b"pPr" | b"rPr" => xml_name_is(parent, WORD_NS, b"lvl"),
        b"abstractNumId" | b"lvlOverride" => xml_name_is(parent, WORD_NS, b"num"),
        b"startOverride" => xml_name_is(parent, WORD_NS, b"lvlOverride"),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(malformed(
            Some(part),
            format!(
                "invalid numbering semantic element hierarchy for {} under {}",
                String::from_utf8_lossy(local),
                parent.map_or_else(
                    || "<root>".into(),
                    |value| String::from_utf8_lossy(&value.1).into_owned()
                )
            ),
        ))
    }
}

fn is_word_content_semantic(namespace: &[u8], local: &[u8]) -> bool {
    (namespace == WORD_NS
        && matches!(
            local,
            b"body"
                | b"p"
                | b"pPr"
                | b"pStyle"
                | b"numPr"
                | b"numId"
                | b"ilvl"
                | b"r"
                | b"rPr"
                | b"b"
                | b"i"
                | b"strike"
                | b"dstrike"
                | b"u"
                | b"vertAlign"
                | b"t"
                | b"tab"
                | b"br"
                | b"cr"
                | b"fldChar"
                | b"instrText"
                | b"hyperlink"
                | b"footnoteReference"
                | b"endnoteReference"
                | b"commentReference"
                | b"drawing"
                | b"pict"
                | b"tbl"
                | b"tblPr"
                | b"tr"
                | b"trPr"
                | b"tc"
                | b"tcPr"
                | b"gridSpan"
                | b"tblHeader"
                | b"vMerge"
                | b"sectPr"
                | b"headerReference"
                | b"footerReference"
                | b"altChunk"
        ))
        || (namespace == MATH_NS && matches!(local, b"oMath" | b"r" | b"t"))
        || (namespace == DRAWING_NS && local == b"blip")
        || (namespace == WORD_DRAWING_NS && local == b"docPr")
        || (namespace == VML_NS && local == b"imagedata")
}

#[allow(clippy::too_many_lines)]
fn validate_word_content_hierarchy(
    namespace: &[u8],
    local: &[u8],
    parent: Option<&(Vec<u8>, Vec<u8>)>,
    ancestors: &[(Vec<u8>, Vec<u8>)],
    part: &str,
) -> Result<(), ConversionError> {
    if !is_word_content_semantic(namespace, local) {
        return Ok(());
    }
    let parent_word_is = |value: &[u8]| xml_name_is(parent, WORD_NS, value);
    let has_ancestor =
        |ns: &[u8], value: &[u8]| ancestors.iter().any(|name| xml_name_is(Some(name), ns, value));
    let valid = match (namespace, local) {
        (WORD_NS, b"body") => parent_word_is(b"document"),
        (WORD_NS, b"p") => matches!(
            parent.map(|name| name.1.as_slice()),
            Some(b"body" | b"hdr" | b"ftr" | b"tc" | b"comment" | b"footnote" | b"endnote")
        ) || has_ancestor(WORD_NS, b"txbxContent"),
        (WORD_NS, b"pPr" | b"hyperlink") => parent_word_is(b"p"),
        (WORD_NS, b"pStyle" | b"numPr") => parent_word_is(b"pPr"),
        (WORD_NS, b"numId" | b"ilvl") => parent_word_is(b"numPr"),
        (WORD_NS, b"r") => parent_word_is(b"p") || parent_word_is(b"hyperlink"),
        (WORD_NS, b"b" | b"i" | b"strike" | b"dstrike" | b"u" | b"vertAlign") => {
            parent_word_is(b"rPr")
        }
        (
            WORD_NS,
            b"drawing" | b"pict" | b"t" | b"tab" | b"br" | b"cr" | b"fldChar" | b"instrText"
            | b"footnoteReference" | b"endnoteReference" | b"commentReference",
        ) => parent_word_is(b"r"),
        (WORD_NS, b"rPr") => parent_word_is(b"r") || parent_word_is(b"pPr"),
        (WORD_DRAWING_NS, b"docPr") | (DRAWING_NS, b"blip") => {
            has_ancestor(WORD_NS, b"drawing") && has_ancestor(WORD_NS, b"r")
        }
        (VML_NS, b"imagedata") => has_ancestor(WORD_NS, b"pict") && has_ancestor(WORD_NS, b"r"),
        (MATH_NS, b"oMath") => matches!(
            parent.map(|name| name.1.as_slice()),
            Some(b"p" | b"body" | b"hdr" | b"ftr" | b"tc")
        ),
        (MATH_NS, b"r") => has_ancestor(MATH_NS, b"oMath"),
        (MATH_NS, b"t") => xml_name_is(parent, MATH_NS, b"r") && has_ancestor(MATH_NS, b"oMath"),
        (WORD_NS, b"tbl") => {
            matches!(parent.map(|name| name.1.as_slice()), Some(b"body" | b"hdr" | b"ftr" | b"tc"))
        }
        (WORD_NS, b"tblPr" | b"tr") => parent_word_is(b"tbl"),
        (WORD_NS, b"trPr" | b"tc") => parent_word_is(b"tr"),
        (WORD_NS, b"tcPr") => parent_word_is(b"tc"),
        (WORD_NS, b"gridSpan" | b"vMerge") => parent_word_is(b"tcPr"),
        (WORD_NS, b"tblHeader") => parent_word_is(b"trPr"),
        (WORD_NS, b"sectPr") => parent_word_is(b"body") || parent_word_is(b"pPr"),
        (WORD_NS, b"headerReference" | b"footerReference") => parent_word_is(b"sectPr"),
        (WORD_NS, b"altChunk") => matches!(
            parent.map(|name| name.1.as_slice()),
            Some(b"body" | b"hdr" | b"ftr" | b"tc")
        ),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(malformed(
            Some(part),
            format!(
                "invalid Word semantic element hierarchy for {} under {}",
                String::from_utf8_lossy(local),
                parent.map_or_else(
                    || "<root>".into(),
                    |value| String::from_utf8_lossy(&value.1).into_owned()
                )
            ),
        ))
    }
}

fn validate_xml_attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    element_name: &(Vec<u8>, Vec<u8>),
    part: &str,
    strict_semantics: bool,
) -> Result<(), ConversionError> {
    for attribute in element.attributes() {
        let attribute = attribute
            .map_err(|error| malformed(Some(part), format!("invalid XML attribute: {error}")))?;
        decode_xml_attribute(attribute.value.as_ref(), part)?;
        let raw = attribute.key.as_ref();
        if raw == b"xmlns" || raw.starts_with(b"xmlns:") {
            continue;
        }
        let (resolved, local) = reader.resolve_attribute(attribute.key);
        let namespace = match resolved {
            ResolveResult::Bound(value) if value.as_ref() == STRICT_OFFICE_REL_NS => {
                OFFICE_REL_NS.to_vec()
            }
            ResolveResult::Bound(value) if value.as_ref() == STRICT_WORD_NS => WORD_NS.to_vec(),
            ResolveResult::Bound(value) => value.as_ref().to_vec(),
            ResolveResult::Unbound => Vec::new(),
            ResolveResult::Unknown(prefix) => {
                return Err(malformed(
                    Some(part),
                    format!("undeclared attribute prefix {}", String::from_utf8_lossy(&prefix)),
                ));
            }
        };
        let element_ns = element_name.0.as_slice();
        let element_local = element_name.1.as_slice();
        let expected = if matches!(element_ns, CONTENT_TYPES_NS | PACKAGE_REL_NS) {
            Some(&[][..])
        } else if matches!(
            (element_ns, element_local, local.as_ref()),
            (
                WORD_NS,
                b"hyperlink" | b"headerReference" | b"footerReference" | b"altChunk",
                b"id"
            )
                | (DRAWING_NS, b"blip", b"embed" | b"link")
                | (VML_NS, b"imagedata", b"id")
        ) {
            Some(OFFICE_REL_NS)
        } else if element_ns == WORD_NS
            && matches!(
                local.as_ref(),
                b"val"
                    | b"styleId"
                    | b"abstractNumId"
                    | b"numId"
                    | b"ilvl"
                    | b"id"
                    | b"fldCharType"
                    | b"anchor"
            )
        {
            Some(WORD_NS)
        } else if element_ns == WORD_DRAWING_NS
            && matches!(local.as_ref(), b"descr" | b"title" | b"id" | b"name")
        {
            Some(&[][..])
        } else {
            None
        };
        if strict_semantics && expected.is_some_and(|expected| namespace.as_slice() != expected) {
            return Err(malformed(
                Some(part),
                format!(
                    "interpreted attribute {} has an unexpected namespace",
                    String::from_utf8_lossy(local.as_ref())
                ),
            ));
        }
    }
    Ok(())
}
