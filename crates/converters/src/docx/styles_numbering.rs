#[derive(Debug, Clone)]
struct Numbering {
    kind: ListKind,
    start: u64,
    label: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SupportedImage {
    Png,
    Jpeg,
}

impl SupportedImage {
    pub(super) fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

#[derive(Default)]
struct ParseState {
    document: Document,
    assets: Vec<Asset>,
    diagnostics: Vec<Diagnostic>,
    next_node: usize,
    inline_count: usize,
    asset_bytes: u64,
    assets_by_part: BTreeMap<String, String>,
    last_list_key: Option<(String, u8)>,
    related_parts: Vec<(String, &'static str)>,
    comment_refs: BTreeSet<String>,
    footnote_refs: BTreeSet<String>,
    endnote_refs: BTreeSet<String>,
    nested_outputs: Vec<ConverterOutput>,
    next_alt_chunk: usize,
    annotation_blocks: Vec<(String, Vec<BlockNode>)>,
    annotation_filter: Option<BTreeSet<String>>,
}

impl ParseState {
    fn node(&mut self, block: Block, part: &str) -> Result<BlockNode, ConversionError> {
        self.next_node = self
            .next_node
            .checked_add(1)
            .ok_or_else(|| limit("max_document_nodes", "node count overflow"))?;
        if self.next_node > MAX_DOCUMENT_NODES {
            return Err(limit(
                "max_document_nodes",
                format!("{} > {MAX_DOCUMENT_NODES}", self.next_node),
            ));
        }
        Ok(BlockNode {
            id: NodeId(format!("docx-{}", self.next_node)),
            block,
            provenance: Provenance {
                kind: ProvenanceKind::NativeParser,
                provider: PROVIDER_ID.into(),
                locator: SourceLocator { part: Some(part.into()), ..SourceLocator::default() },
                confidence: Some(1.0),
            },
        })
    }

    fn add_inlines(&mut self, count: usize) -> Result<(), ConversionError> {
        self.inline_count = self
            .inline_count
            .checked_add(count)
            .ok_or_else(|| limit("max_document_inlines", "inline count overflow"))?;
        if self.inline_count > MAX_DOCUMENT_INLINES {
            return Err(limit(
                "max_document_inlines",
                format!("{} > {MAX_DOCUMENT_INLINES}", self.inline_count),
            ));
        }
        Ok(())
    }

    fn warning(&mut self, code: &str, message: impl Into<String>, part: &str) {
        let message = message.into();
        if let Some(diagnostic) = self.diagnostics.iter_mut().find(|diagnostic| {
            diagnostic.code == code
                && diagnostic.locator.as_ref().and_then(|locator| locator.part.as_deref())
                    == Some(part)
        }) {
            if diagnostic.severity == DiagnosticSeverity::Info {
                diagnostic.severity = DiagnosticSeverity::Warning;
                diagnostic.message = message;
            }
            return;
        }
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity: DiagnosticSeverity::Warning,
            message,
            locator: Some(SourceLocator { part: Some(part.into()), ..SourceLocator::default() }),
        });
    }

    fn info(&mut self, code: &str, message: impl Into<String>, part: &str) {
        if self.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == code
                && diagnostic.locator.as_ref().and_then(|locator| locator.part.as_deref())
                    == Some(part)
        }) {
            return;
        }
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity: DiagnosticSeverity::Info,
            message: message.into(),
            locator: Some(SourceLocator {
                part: Some(part.into()),
                ..SourceLocator::default()
            }),
        });
    }
}

#[allow(clippy::too_many_lines)]
fn convert_docx(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let mut package = Package::open(bytes, options, context)?;
    let xml_work = package
        .parts
        .iter()
        .filter(|(name, _)| {
            Path::new(name).extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("xml") || extension.eq_ignore_ascii_case("rels")
            })
        })
        .try_fold(0_u64, |total, (_, value)| {
            total.checked_add(u64::try_from(value.len()).unwrap_or(u64::MAX)).ok_or_else(|| {
                limit("max_memory_bytes", "XML parser working-set accounting overflow")
            })
        })?
        .checked_mul(3)
        .ok_or_else(|| limit("max_memory_bytes", "XML parser working-set accounting overflow"))?;
    // The package reservation owns the retained decompressed buffers. This second reservation
    // covers decoded strings, namespace stacks and IR containers while those buffers are live.
    let _parse_memory = context.reserve_memory(xml_work)?;

    let root_relationships = parse_relationships(
        package.parts.get("_rels/.rels").map(Vec::as_slice),
        "",
        options,
        context,
    )?;
    let (_, main_relationship) =
        unique_internal_relationship(&root_relationships, OFFICE_REL_TYPE, "")?.ok_or_else(
            || malformed(Some("_rels/.rels"), "officeDocument relationship is missing"),
        )?;
    let main_part = resolve_target("", &main_relationship.target)?;
    let main_content_type = package.content_types.content_type(&main_part).ok_or_else(|| {
        malformed(Some("[Content_Types].xml"), "Word main part has no content type")
    })?;
    if !matches!(
        main_content_type,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
            | "application/vnd.ms-word.document.macroEnabled.main+xml"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml"
            | "application/vnd.ms-word.template.macroEnabledTemplate.main+xml"
    ) {
        return Err(malformed(
            Some("[Content_Types].xml"),
            "officeDocument relationship does not target a DOCX/DOCM main part",
        ));
    }
    let relationships = parse_relationships(
        package.parts.get(&relationship_part(&main_part)).map(Vec::as_slice),
        &main_part,
        options,
        context,
    )?;
    let glossary_part = relationship_target(&relationships, "glossaryDocument", &main_part)?;
    if let Some(part) = glossary_part.as_deref() {
        require_content_type(
            &package,
            part,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.glossary+xml",
        )?;
        package.required(part)?;
    }
    let styles_part = relationship_target(&relationships, "styles", &main_part)?;
    let styles = if let Some(part) = styles_part.as_deref() {
        require_content_type(
            &package,
            part,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml",
        )?;
        parse_styles(Some(package.required(part)?), part, options, context)?
    } else {
        BTreeMap::new()
    };
    let numbering_part = relationship_target(&relationships, "numbering", &main_part)?;
    let numbering = if let Some(part) = numbering_part.as_deref() {
        require_content_type(
            &package,
            part,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml",
        )?;
        parse_numbering(Some(package.required(part)?), part, options, context)?
    } else {
        BTreeMap::new()
    };
    let mut state = ParseState::default();
    if let Some(part) = glossary_part.as_deref() {
        state.warning(
            "word.glossaryContentOmitted",
            "authenticated glossary document content is not represented in the Markdown output",
            part,
        );
    }
    if package.macro_present {
        state.warning(
            "docx.macrosIgnored",
            "macro project detected and intentionally not read or executed",
            "[Content_Types].xml",
        );
        state.document.metadata.properties.insert("docx.macrosPresent".into(), "true".into());
    }
    if let Some(core_part) = relationship_target_from_kind(
        &root_relationships,
        &relationship_type("metadata/core-properties"),
        "",
    )? {
        require_content_type(
            &package,
            &core_part,
            "application/vnd.openxmlformats-package.core-properties+xml",
        )?;
        parse_core_properties(
            package.required(&core_part)?,
            &core_part,
            &mut state,
            options,
            context,
        )?;
    }
    let main_bytes = package.take_required(&main_part)?;
    parse_word_part(
        &main_bytes,
        &main_part,
        XmlProfile::Document,
        &relationships,
        &styles,
        &numbering,
        &mut package,
        options,
        context,
        &mut state,
    )?;
    append_related_parts(&mut package, options, context, &mut state)?;

    let comment_references = state.comment_refs.clone();
    let comments = referenced_annotations(
        &mut package,
        &relationships,
        "comments",
        &main_part,
        &comment_references,
        XmlProfile::Comments,
        options,
        context,
        &mut state,
    )?;
    for (id, blocks) in comments {
        state.add_inlines(1)?;
        let title = state.node(
            Block::Heading {
                level: 6,
                content: vec![Inline::Text { value: format!("Comment {id}"), marks: Vec::new() }],
            },
            "comments",
        )?;
        state.document.blocks.push(title);
        state.document.blocks.extend(blocks);
    }
    let footnote_references = state.footnote_refs.clone();
    let mut notes = referenced_annotations(
        &mut package,
        &relationships,
        "footnotes",
        &main_part,
        &footnote_references,
        XmlProfile::Footnotes,
        options,
        context,
        &mut state,
    )?;
    let endnote_references = state.endnote_refs.clone();
    notes.extend(
        referenced_annotations(
            &mut package,
            &relationships,
            "endnotes",
            &main_part,
            &endnote_references,
            XmlProfile::Endnotes,
            options,
            context,
            &mut state,
        )?
        .into_iter()
        .map(|(id, content)| (format!("endnote-{id}"), content)),
    );
    for (id, blocks) in notes {
        let node = state.node(Block::Footnote { label: id, blocks }, "notes")?;
        state.document.blocks.push(node);
    }
    if state.document.blocks.is_empty()
        && let Some(part) = glossary_part.as_deref()
    {
        state.add_inlines(1)?;
        let node = state.node(
            Block::Paragraph(vec![Inline::Text {
                value: "[Word glossary content omitted]".into(),
                marks: Vec::new(),
            }]),
            part,
        )?;
        state.document.blocks.push(node);
    }
    state.document.validate().map_err(|error| ConversionError::Internal {
        detail: format!(
            "DOCX emitted invalid IR ({} at {}): {}",
            error.code.as_str(),
            error.path,
            error.detail
        ),
    })?;
    let evidence = if state.document.blocks.is_empty()
        && state.assets.is_empty()
        && state
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity == DiagnosticSeverity::Info)
    {
        SourceContentEvidence::Empty
    } else {
        SourceContentEvidence::Unknown
    };
    let mut nested_outputs = std::mem::take(&mut state.nested_outputs);
    let mut output = ConverterOutput::new(state.document, state.assets, state.diagnostics)
        .with_source_content_evidence(evidence);
    for nested in &mut nested_outputs {
        output.absorb_memory_lease(nested, context)?;
    }
    Ok(output)
}

fn relationship_target(
    relationships: &BTreeMap<String, Relationship>,
    suffix: &str,
    owner: &str,
) -> Result<Option<String>, ConversionError> {
    relationship_target_from_kind(relationships, &relationship_type(suffix), owner)
}

fn require_content_type(
    package: &Package,
    part: &str,
    expected: &str,
) -> Result<(), ConversionError> {
    if package.content_types.content_type(part) != Some(expected) {
        return Err(malformed(
            Some("[Content_Types].xml"),
            format!("part {part} has an unexpected content type"),
        ));
    }
    Ok(())
}

fn relationship_target_from_kind(
    relationships: &BTreeMap<String, Relationship>,
    kind: &str,
    owner: &str,
) -> Result<Option<String>, ConversionError> {
    unique_internal_relationship(relationships, kind, owner)?
        .map(|(_, relationship)| resolve_target(owner, &relationship.target))
        .transpose()
}

#[allow(clippy::too_many_arguments)]
fn referenced_annotations(
    package: &mut Package,
    relationships: &BTreeMap<String, Relationship>,
    relation_suffix: &str,
    owner: &str,
    references: &BTreeSet<String>,
    profile: XmlProfile,
    options: &ConversionOptions,
    context: &ExecutionContext,
    state: &mut ParseState,
) -> Result<Vec<(String, Vec<BlockNode>)>, ConversionError> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let part = relationship_target(relationships, relation_suffix, owner)?.ok_or_else(|| {
        malformed(
            Some(&relationship_part(owner)),
            format!("referenced {relation_suffix} relationship is missing"),
        )
    })?;
    let expected_content_type = match relation_suffix {
        "comments" => "application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml",
        "footnotes" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml"
        }
        "endnotes" => "application/vnd.openxmlformats-officedocument.wordprocessingml.endnotes+xml",
        _ => {
            return Err(ConversionError::Internal { detail: "unsupported annotation kind".into() });
        }
    };
    require_content_type(package, &part, expected_content_type)?;
    let annotation_relationships = parse_relationships(
        package.parts.get(&relationship_part(&part)).map(Vec::as_slice),
        &part,
        options,
        context,
    )?;
    let bytes = package.take_required(&part)?;
    state.annotation_filter = Some(references.clone());
    parse_word_part(
        &bytes,
        &part,
        profile,
        &annotation_relationships,
        &BTreeMap::new(),
        &BTreeMap::new(),
        package,
        options,
        context,
        state,
    )?;
    state.annotation_filter = None;
    let parsed = std::mem::take(&mut state.annotation_blocks);
    let mut definitions = BTreeMap::new();
    for (id, content) in parsed {
        if definitions.insert(id, content).is_some() {
            return Err(malformed(Some(&part), "duplicate annotation id"));
        }
    }
    references
        .iter()
        .map(|id| {
            definitions.remove(id).map(|content| (id.clone(), content)).ok_or_else(|| {
                malformed(Some(&part), format!("referenced annotation {id} is missing"))
            })
        })
        .collect()
}

fn parse_core_properties(
    bytes: &[u8],
    part: &str,
    state: &mut ParseState,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    preflight_xml(bytes, part, XmlProfile::CoreProperties, options, context)?;
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut current = None::<String>;
    let mut value = String::new();
    let mut stack = Vec::<(Vec<u8>, Vec<u8>)>::new();
    loop {
        context.checkpoint()?;
        match reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?
        {
            Event::Start(e) => {
                let name = resolved_element(&reader, e.name(), part)?;
                let property = if stack.len() == 1 {
                    match (name.0.as_slice(), name.1.as_slice()) {
                        (DUBLIN_CORE_NS, b"title") => Some("title"),
                        (DUBLIN_CORE_NS, b"creator") => Some("creator"),
                        (CORE_PROPERTIES_NS, b"lastModifiedBy") => Some("lastModifiedBy"),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(property) = property {
                    current = Some(property.into());
                    value.clear();
                } else if current.is_some() && stack.len() >= 2 {
                    current = None;
                    value.clear();
                }
                stack.push(name);
            }
            Event::Text(e) if current.is_some() && stack.len() == 2 => {
                append_bounded_text(&mut value, &decode_text(&e, part)?, options)?;
            }
            Event::CData(e) if current.is_some() && stack.len() == 2 => {
                append_bounded_text(&mut value, &decode_cdata(&e, part)?, options)?;
            }
            Event::GeneralRef(e) if current.is_some() && stack.len() == 2 => {
                append_bounded_text(&mut value, &decode_reference(&e, part)?, options)?;
            }
            Event::End(e) => {
                let actual = resolved_element(&reader, e.name(), part)?;
                if stack.len() == 2 {
                    let closes_current = current.as_deref().is_some_and(|property| {
                        matches!(
                            (property, actual.0.as_slice(), actual.1.as_slice()),
                            ("title", DUBLIN_CORE_NS, b"title")
                                | ("creator", DUBLIN_CORE_NS, b"creator")
                                | ("lastModifiedBy", CORE_PROPERTIES_NS, b"lastModifiedBy")
                        )
                    });
                    if closes_current {
                        match current.take().as_deref() {
                            Some("title") => {
                                state.document.metadata.title = Some(std::mem::take(&mut value));
                            }
                            Some("creator" | "lastModifiedBy") => {
                                state.document.metadata.authors.push(std::mem::take(&mut value));
                            }
                            _ => {}
                        }
                    }
                }
                if stack.pop().as_ref() != Some(&actual) {
                    return Err(malformed(Some(part), "XML end namespace differs from start"));
                }
            }
            Event::DocType(_) => {
                return Err(malformed(Some(part), "DOCTYPE is forbidden"));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

fn parse_relationships(
    bytes: Option<&[u8]>,
    owner: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<BTreeMap<String, Relationship>, ConversionError> {
    let Some(bytes) = bytes else {
        return Ok(BTreeMap::new());
    };
    let part = relationship_part(owner);
    preflight_xml(bytes, &part, XmlProfile::Relationships, options, context)?;
    let mut reader = NsReader::from_reader(bytes);
    let mut result = BTreeMap::new();
    let mut stack = Vec::<(Vec<u8>, Vec<u8>)>::new();
    loop {
        context.checkpoint()?;
        match reader
            .read_event()
            .map_err(|error| malformed(Some(&part), format!("invalid XML: {error}")))?
        {
            Event::Start(e) => {
                let name = resolved_element(&reader, e.name(), &part)?;
                if is_direct_relationship(&name, stack.last()) {
                    insert_relationship(&e, owner, &part, &mut result)?;
                }
                stack.push(name);
            }
            Event::Empty(e) => {
                let name = resolved_element(&reader, e.name(), &part)?;
                if is_direct_relationship(&name, stack.last()) {
                    insert_relationship(&e, owner, &part, &mut result)?;
                }
            }
            Event::End(e) => {
                let name = resolved_element(&reader, e.name(), &part)?;
                if stack.pop().as_ref() != Some(&name) {
                    return Err(malformed(Some(&part), "relationship XML end differs from start"));
                }
            }
            Event::DocType(_) => return Err(malformed(Some(&part), "DOCTYPE is forbidden")),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(result)
}

fn is_direct_relationship(
    name: &(Vec<u8>, Vec<u8>),
    parent: Option<&(Vec<u8>, Vec<u8>)>,
) -> bool {
    name.0.as_slice() == PACKAGE_REL_NS
        && name.1.as_slice() == b"Relationship"
        && parent.is_some_and(|parent| {
            parent.0.as_slice() == PACKAGE_REL_NS && parent.1.as_slice() == b"Relationships"
        })
}

fn insert_relationship(
    element: &BytesStart<'_>,
    owner: &str,
    part: &str,
    result: &mut BTreeMap<String, Relationship>,
) -> Result<(), ConversionError> {
    let id = attr(element, b"Id", part)?
        .ok_or_else(|| malformed(Some(part), "relationship lacks Id"))?;
    let target = attr(element, b"Target", part)?
        .ok_or_else(|| malformed(Some(part), "relationship lacks Target"))?;
    let kind = attr(element, b"Type", part)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed(Some(part), "relationship lacks Type"))?;
    let kind = canonical_relationship_kind(&kind);
    let target_mode = attr(element, b"TargetMode", part)?;
    let external = target_mode.as_deref().is_some_and(|value| value.eq_ignore_ascii_case("external"));
    if target_mode.is_some() && !external {
        return Err(malformed(Some(part), "unsupported relationship TargetMode"));
    }
    if !external {
        resolve_target(owner, &target)?;
    }
    if result.insert(id, Relationship { target, external, kind }).is_some() {
        return Err(malformed(Some(part), "duplicate relationship Id"));
    }
    Ok(())
}

fn parse_styles(
    bytes: Option<&[u8]>,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<BTreeMap<String, u8>, ConversionError> {
    let Some(bytes) = bytes else {
        return Ok(BTreeMap::new());
    };
    preflight_xml(bytes, part, XmlProfile::Styles, options, context)?;
    let mut reader = Reader::from_reader(bytes);
    let mut result = BTreeMap::new();
    let mut bases = BTreeMap::<String, String>::new();
    let mut id = None;
    let mut name = None;
    let mut level = None;
    let mut based_on = None;
    loop {
        context.checkpoint()?;
        match reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?
        {
            Event::Start(e) if local(e.name().as_ref()) == "style" => {
                id = attr_local(&e, "styleId", part)?;
                name = None;
                level = None;
                based_on = None;
            }
            Event::Empty(e) if local(e.name().as_ref()) == "name" => {
                name = attr_local(&e, "val", part)?;
            }
            Event::Empty(e) if local(e.name().as_ref()) == "outlineLvl" => {
                level = attr_local(&e, "val", part)?
                    .and_then(|v| v.parse::<u8>().ok())
                    .map(|v| v.saturating_add(1).clamp(1, 6));
            }
            Event::Empty(e) if local(e.name().as_ref()) == "basedOn" => {
                based_on = attr_local(&e, "val", part)?;
            }
            Event::End(e) if local(e.name().as_ref()) == "style" => {
                if let Some(style) = id.take() {
                    let inferred = name.as_deref().and_then(heading_name).or(level);
                    if let Some(value) = inferred {
                        result.insert(style, value);
                    } else if let Some(parent) = based_on.take() {
                        bases.insert(style, parent);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    for style in bases.keys().cloned().collect::<Vec<_>>() {
        let mut next = style.as_str();
        let mut seen = BTreeSet::new();
        while let Some(parent) = bases.get(next) {
            if !seen.insert(next.to_owned()) {
                return Err(malformed(Some(part), "style inheritance cycle"));
            }
            if let Some(level) = result.get(parent).copied() {
                result.insert(style.clone(), level);
                break;
            }
            next = parent;
        }
    }
    Ok(result)
}

fn heading_name(name: &str) -> Option<u8> {
    let lower = name.trim().to_ascii_lowercase();
    lower
        .strip_prefix("heading")
        .or_else(|| lower.strip_prefix("标题"))
        .and_then(|v| v.trim().parse::<u8>().ok())
        .map(|v| v.clamp(1, 6))
}

fn parse_numbering(
    bytes: Option<&[u8]>,
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<BTreeMap<(String, u8), Numbering>, ConversionError> {
    let Some(bytes) = bytes else {
        return Ok(BTreeMap::new());
    };
    preflight_xml(bytes, part, XmlProfile::Numbering, options, context)?;
    let mut reader = Reader::from_reader(bytes);
    let mut abstracts: BTreeMap<(String, u8), Numbering> = BTreeMap::new();
    let mut mapping = BTreeMap::new();
    let mut starts = BTreeMap::<(String, u8), u64>::new();
    let mut abstract_id = None::<String>;
    let mut num_id = None::<String>;
    let mut ilvl = 0_u8;
    let mut override_level = None::<u8>;
    let mut current = Numbering { kind: ListKind::Bullet, start: 1, label: None };
    loop {
        context.checkpoint()?;
        match reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?
        {
            Event::Start(e) if local(e.name().as_ref()) == "abstractNum" => {
                abstract_id = attr_local(&e, "abstractNumId", part)?;
            }
            Event::End(e) if local(e.name().as_ref()) == "abstractNum" => {
                abstract_id = None;
            }
            Event::Start(e) if local(e.name().as_ref()) == "lvl" => {
                ilvl = attr_local(&e, "ilvl", part)?.and_then(|v| v.parse().ok()).unwrap_or(0);
                current = Numbering { kind: ListKind::Bullet, start: 1, label: None };
            }
            Event::Empty(e) if local(e.name().as_ref()) == "numFmt" => {
                let format = attr_local(&e, "val", part)?.unwrap_or_default();
                current.kind =
                    if format == "bullet" { ListKind::Bullet } else { ListKind::Ordered };
            }
            Event::Empty(e) if local(e.name().as_ref()) == "start" => {
                current.start =
                    attr_local(&e, "val", part)?.and_then(|v| v.parse().ok()).unwrap_or(1);
            }
            Event::Empty(e) if local(e.name().as_ref()) == "lvlText" => {
                current.label = attr_local(&e, "val", part)?;
            }
            Event::End(e) if local(e.name().as_ref()) == "lvl" => {
                if let Some(id) = &abstract_id {
                    abstracts.insert((id.clone(), ilvl), current.clone());
                }
            }
            Event::Start(e) if local(e.name().as_ref()) == "num" => {
                num_id = attr_local(&e, "numId", part)?;
            }
            Event::End(e) if local(e.name().as_ref()) == "num" => {
                num_id = None;
                override_level = None;
            }
            Event::Start(e) if local(e.name().as_ref()) == "lvlOverride" => {
                override_level = attr_local(&e, "ilvl", part)?.and_then(|value| value.parse().ok());
            }
            Event::Empty(e) if local(e.name().as_ref()) == "startOverride" => {
                if let (Some(num), Some(level), Some(start)) = (
                    &num_id,
                    override_level,
                    attr_local(&e, "val", part)?.and_then(|value| value.parse::<u64>().ok()),
                ) {
                    starts.insert((num.clone(), level), start);
                }
            }
            Event::Empty(e) if local(e.name().as_ref()) == "abstractNumId" => {
                if let (Some(num), Some(abs)) = (&num_id, attr_local(&e, "val", part)?) {
                    mapping.insert(num.clone(), abs);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut result = BTreeMap::new();
    for (num, abs) in mapping {
        for ((abstract_key, level), value) in &abstracts {
            if abstract_key == &abs {
                result.insert((num.clone(), *level), value.clone());
            }
        }
    }
    for (key, start) in starts {
        if let Some(numbering) = result.get_mut(&key) {
            numbering.start = start;
        }
    }
    Ok(result)
}
