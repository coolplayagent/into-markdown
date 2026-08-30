#[allow(clippy::too_many_arguments)]
fn append_word_text(
    value: String,
    current: Option<&str>,
    math_depth: u16,
    field_active: bool,
    formula: &mut String,
    paragraph: &mut Option<Paragraph>,
    hyperlink: &mut Option<(String, Vec<Inline>)>,
    marks: &[InlineMark],
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if math_depth != 0 && current == Some("t") {
        append_bounded_text(formula, &value, options)?;
    } else if field_active && current == Some("instrText") {
        if let Some(paragraph) = paragraph {
            append_bounded_text(&mut paragraph.field, &value, options)?;
        }
    } else if current == Some("t")
        && let Some(paragraph) = paragraph
    {
        let inlines = hyperlink.as_mut().map_or(&mut paragraph.inlines, |(_, content)| content);
        append_text_inline(inlines, value, marks, options)?;
    }
    Ok(())
}

fn append_annotation_text(
    inlines: &mut Vec<Inline>,
    value: String,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    append_text_inline(inlines, value, &[], options)
}

fn append_text_inline(
    inlines: &mut Vec<Inline>,
    value: String,
    marks: &[InlineMark],
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if value.is_empty() {
        return Ok(());
    }
    let mut unique_marks = Vec::new();
    unique_marks.try_reserve_exact(marks.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve inline marks: {error}"))
    })?;
    for mark in marks {
        if !unique_marks.contains(mark) {
            unique_marks.push(*mark);
        }
    }
    if let Some(Inline::Text { value: previous, marks: previous_marks }) = inlines.last_mut()
        && *previous_marks == unique_marks
    {
        append_bounded_text(previous, &value, options)?;
    } else {
        enforce_field_limit(&value, options)?;
        inlines.push(Inline::Text { value, marks: unique_marks });
    }
    Ok(())
}

fn push_unique_mark(marks: &mut Vec<InlineMark>, mark: InlineMark) {
    if !marks.contains(&mark) {
        marks.push(mark);
    }
}

fn recover_unsupported_word_object(
    name: &str,
    part: &str,
    options: &ConversionOptions,
    state: &mut ParseState,
) -> Result<(), ConversionError> {
    if options.error_policy == into_markdown_core::ErrorPolicy::Strict {
        return Err(malformed(
            Some(part),
            format!("unsupported Word object {name} requires best-effort recovery"),
        ));
    }
    state.warning(
        "word.unsupportedWrapperOmitted",
        format!(
            "unsupported Word object {name} was omitted while preserving visible fallback content"
        ),
        part,
    );
    Ok(())
}

fn push_word_object_placeholder(
    name: &str,
    part: &str,
    state: &mut ParseState,
    paragraph: &mut Option<Paragraph>,
    hyperlink: &mut Option<(String, Vec<Inline>)>,
    table: Option<&mut TableBuild>,
) -> Result<(), ConversionError> {
    let label = match name {
        "chart" => "[Embedded Word chart omitted]",
        "object" | "OLEObject" => "[Embedded Word object omitted]",
        "relIds" => "[Embedded Word diagram omitted]",
        _ => "[Embedded Word content omitted]",
    };
    let inline = Inline::Text { value: label.into(), marks: Vec::new() };
    if let Some(paragraph) = paragraph {
        push_inline(paragraph, hyperlink, inline);
        return Ok(());
    }
    state.add_inlines(1)?;
    let node = state.node(Block::Paragraph(vec![inline]), part)?;
    if let Some(table) = table {
        table.cell_blocks.push(node);
    } else {
        state.document.blocks.push(node);
    }
    Ok(())
}

fn append_bounded_text(
    target: &mut String,
    value: &str,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    let combined = target
        .len()
        .checked_add(value.len())
        .ok_or_else(|| limit("max_field_bytes", "decoded XML text length overflow"))?;
    if u64::try_from(combined).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
        return Err(limit(
            "max_field_bytes",
            format!("{combined} > {}", options.limits.max_field_bytes),
        ));
    }
    target.push_str(value);
    Ok(())
}

fn push_inline(
    paragraph: &mut Paragraph,
    hyperlink: &mut Option<(String, Vec<Inline>)>,
    value: Inline,
) {
    if let Some((_, content)) = hyperlink {
        content.push(value);
    } else {
        paragraph.inlines.push(value);
    }
}

fn emit_field(paragraph: &mut Paragraph, hyperlink: &mut Option<(String, Vec<Inline>)>) {
    let field = paragraph.field.trim();
    if let Some(rest) = field.strip_prefix("HYPERLINK") {
        let target = rest.trim().trim_matches('"');
        if !target.is_empty() {
            push_inline(
                paragraph,
                hyperlink,
                Inline::Link {
                    target: target.into(),
                    content: vec![Inline::Text { value: target.into(), marks: Vec::new() }],
                },
            );
        }
    } else if !field.is_empty() {
        push_inline(paragraph, hyperlink, Inline::Code(field.into()));
    }
    paragraph.field.clear();
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn finish_paragraph(
    mut p: Paragraph,
    part: &str,
    relationships: &BTreeMap<String, Relationship>,
    styles: &BTreeMap<String, u8>,
    numbering: &BTreeMap<(String, u8), Numbering>,
    package: &mut Package,
    options: &ConversionOptions,
    context: &ExecutionContext,
    state: &mut ParseState,
    mut table: Option<&mut TableBuild>,
) -> Result<(), ConversionError> {
    normalize_word_inlines(&mut p.inlines, part, options, state)?;
    for (id, alt) in p.images {
        let rel = relationships
            .get(&id)
            .ok_or_else(|| malformed(Some(part), format!("image relationship {id} is missing")))?;
        if rel.kind == relationship_type("image")
            && rel.external
            && options.error_policy == into_markdown_core::ErrorPolicy::BestEffort
        {
            push_word_media_placeholder(
                state,
                table.as_deref_mut(),
                part,
                alt.as_deref(),
                "external image relationship was removed without downloading it",
                "office.relationshipOmitted",
            )?;
            continue;
        }
        if rel.kind != relationship_type("image") || rel.external {
            return Err(malformed(
                Some(part),
                "image reference has the wrong relationship type or target mode",
            ));
        }
        let target = resolve_target(part, &rel.target)?;
        let asset_id = if let Some(id) = state.assets_by_part.get(&target) {
            id.clone()
        } else {
            let declared_type = package.content_types.content_type(&target).ok_or_else(|| {
                malformed(
                    Some("[Content_Types].xml"),
                    format!("image target {target} has no content type"),
                )
            })?;
            let image = match supported_image(&target, declared_type) {
                Ok(image) => image,
                Err(error)
                    if options.error_policy == into_markdown_core::ErrorPolicy::BestEffort
                        && recoverable_office_media_error(&error) =>
                {
                    push_word_media_placeholder(
                        state,
                        table.as_deref_mut(),
                        part,
                        alt.as_deref(),
                        &format!("unsupported media {target} was omitted: {error}"),
                        "word.mediaPlaceholder",
                    )?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let bytes = package
                .parts
                .remove(&target)
                .ok_or_else(|| malformed(Some(&target), "related image part is missing"))?;
            if let Err(error) = validate_image_bytes(image, &bytes, &target, options, context) {
                if options.error_policy == into_markdown_core::ErrorPolicy::BestEffort
                    && recoverable_office_media_error(&error)
                {
                    push_word_media_placeholder(
                        state,
                        table.as_deref_mut(),
                        part,
                        alt.as_deref(),
                        &format!("invalid media {target} was omitted: {error}"),
                        "word.mediaPlaceholder",
                    )?;
                    continue;
                }
                return Err(error);
            }
            let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if size > options.limits.max_asset_bytes {
                return Err(limit(
                    "max_asset_bytes",
                    format!("asset {target}: {size} > {}", options.limits.max_asset_bytes),
                ));
            }
            state.asset_bytes = state
                .asset_bytes
                .checked_add(size)
                .ok_or_else(|| limit("max_total_asset_bytes", "DOCX asset byte count overflow"))?;
            if state.asset_bytes > options.limits.max_total_asset_bytes {
                return Err(limit(
                    "max_total_asset_bytes",
                    format!("{} > {}", state.asset_bytes, options.limits.max_total_asset_bytes),
                ));
            }
            let id = format!("docx-asset-{}", state.assets.len() + 1);
            state.assets.push(Asset {
                id: AssetId(id.clone()),
                filename: Path::new(&target)
                    .file_name()
                    .and_then(|v| v.to_str())
                    .map(str::to_owned),
                media_type: image.media_type().into(),
                bytes,
                external_uri: None,
            });
            state.assets_by_part.insert(target, id.clone());
            id
        };
        let node = state.node(Block::Image { asset: AssetId(asset_id), alt }, part)?;
        if let Some(table) = table.as_deref_mut() {
            table.cell_blocks.push(node);
        } else {
            state.document.blocks.push(node);
        }
    }
    state.add_inlines(p.inlines.len())?;
    let block = if let Some(level) = p
        .style
        .as_deref()
        .and_then(|style| styles.get(style))
        .copied()
        .or_else(|| p.style.as_deref().and_then(heading_name))
    {
        Block::Heading { level, content: p.inlines }
    } else {
        Block::Paragraph(p.inlines)
    };
    let node = state.node(block, part)?;
    if let Some(table) = table {
        table.cell_blocks.push(node);
    } else if let Some(num_id) = p.num_id {
        let list_key = (num_id.clone(), p.level);
        let descriptor = numbering.get(&(num_id, p.level)).cloned().unwrap_or(Numbering {
            kind: ListKind::Bullet,
            start: 1,
            label: None,
        });
        if let Some(last) = state.document.blocks.last_mut()
            && let Block::List { kind, start: _, items } = &mut last.block
            && *kind == descriptor.kind
            && state.last_list_key.as_ref() == Some(&list_key)
        {
            items.push(ListItem {
                checked: None,
                marker_label: descriptor.label,
                blocks: vec![node],
            });
        } else {
            let list = state.node(
                Block::List {
                    kind: descriptor.kind,
                    start: descriptor.start,
                    items: vec![ListItem {
                        checked: None,
                        marker_label: descriptor.label,
                        blocks: vec![node],
                    }],
                },
                part,
            )?;
            state.document.blocks.push(list);
        }
        state.last_list_key = Some(list_key);
    } else {
        state.last_list_key = None;
        if !matches!(&node.block, Block::Paragraph(inlines) if inlines.is_empty()) {
            state.document.blocks.push(node);
        }
    }
    context.checkpoint()
}

fn normalize_word_inlines(
    inlines: &mut Vec<Inline>,
    part: &str,
    options: &ConversionOptions,
    state: &mut ParseState,
) -> Result<(), ConversionError> {
    let mut normalized = Vec::new();
    normalized.try_reserve_exact(inlines.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve normalized Word inlines: {error}"))
    })?;
    for mut inline in std::mem::take(inlines) {
        match &mut inline {
            Inline::Text { marks, .. } => {
                let mut index = 0;
                while index < marks.len() {
                    if marks[..index].contains(&marks[index]) {
                        marks.remove(index);
                    } else {
                        index += 1;
                    }
                }
            }
            Inline::Link { target, content } => {
                normalize_word_inlines(content, part, options, state)?;
                if !safe_link_target(target) {
                    if options.error_policy == into_markdown_core::ErrorPolicy::Strict {
                        return Err(malformed(Some(part), "hyperlink uses a disallowed target"));
                    }
                    state.warning(
                        "office.relationshipOmitted",
                        "unsafe hyperlink target was removed while preserving its text",
                        part,
                    );
                    normalized.append(content);
                    continue;
                }
            }
            _ => {}
        }
        normalized.push(inline);
    }
    *inlines = normalized;
    Ok(())
}

fn safe_link_target(value: &str) -> bool {
    if value.chars().any(char::is_control) || contains_html_character_reference(value) {
        return false;
    }
    let Some(colon) = value.find(':') else {
        return true;
    };
    let scheme = &value[..colon];
    if scheme.is_empty()
        || !scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic() || index > 0 && matches!(byte, b'+' | b'-' | b'.')
        })
    {
        return true;
    }
    if matches!(scheme.to_ascii_lowercase().as_str(), "javascript" | "vbscript" | "data" | "file") {
        return false;
    }
    !value[colon + 1..]
        .strip_prefix("//")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|authority| authority.contains('@'))
}

fn contains_html_character_reference(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.iter().enumerate().any(|(index, byte)| {
        if *byte != b'&' {
            return false;
        }
        let tail = &bytes[index + 1..];
        tail.iter().position(|candidate| *candidate == b';').is_some_and(|end| {
            let entity = &tail[..end];
            !entity.is_empty()
                && entity.len() <= 32
                && (entity[0] == b'#' || entity.iter().all(u8::is_ascii_alphanumeric))
        })
    })
}
