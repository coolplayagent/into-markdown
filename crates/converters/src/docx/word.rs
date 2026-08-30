#[derive(Default)]
struct Paragraph {
    inlines: Vec<Inline>,
    style: Option<String>,
    num_id: Option<String>,
    level: u8,
    images: Vec<(String, Option<String>)>,
    field: String,
    pending_alt: Option<String>,
}

#[derive(Default)]
struct TableBuild {
    rows: Vec<TableRow>,
    vertical_merges: Vec<Vec<VerticalMerge>>,
    cells: Vec<Cell>,
    cell_merges: Vec<VerticalMerge>,
    cell_vertical_merge: VerticalMerge,
    cell_blocks: Vec<BlockNode>,
    cell_column_span: u32,
    row_header: bool,
    row_open: bool,
    cell_open: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum VerticalMerge {
    #[default]
    None,
    Restart,
    Continue,
    Invalid,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn parse_word_part(
    bytes: &[u8],
    part: &str,
    profile: XmlProfile,
    relationships: &BTreeMap<String, Relationship>,
    styles: &BTreeMap<String, u8>,
    numbering: &BTreeMap<(String, u8), Numbering>,
    package: &mut Package,
    options: &ConversionOptions,
    context: &ExecutionContext,
    state: &mut ParseState,
) -> Result<(), ConversionError> {
    preflight_xml(bytes, part, profile, options, context)?;
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut paragraph = None::<Paragraph>;
    let mut paragraph_nesting = 0_usize;
    let mut marks = Vec::<InlineMark>::new();
    let mut hyperlink = None::<(String, Vec<Inline>)>;
    let mut tables = Vec::<TableBuild>::new();
    let mut depth = 0_u16;
    let mut element_stack = Vec::<String>::new();
    let mut skipped_choice_depth = None::<u16>;
    let mut body_depth = None::<u16>;
    let mut field_active = false;
    let mut math_depth = 0_u16;
    let mut formula = String::new();
    loop {
        context.checkpoint()?;
        let event = reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?;
        match event {
            Event::Start(e) => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| limit("max_nesting_depth", "XML depth overflow"))?;
                if depth > options.limits.max_nesting_depth {
                    return Err(limit(
                        "max_nesting_depth",
                        format!("{depth} > {}", options.limits.max_nesting_depth),
                    ));
                }
                let name = interpreted_word_local(&reader, e.name(), part)?.unwrap_or_default();
                if skipped_choice_depth.is_some() {
                    element_stack.push(name);
                    continue;
                }
                if name == "Choice"
                    && element_stack.last().is_some_and(|parent| parent == "AlternateContent")
                {
                    skipped_choice_depth = Some(depth);
                    element_stack.push(name);
                    continue;
                }
                element_stack.push(name.clone());
                if name == "body" {
                    body_depth = Some(depth);
                }
                if name == "oMath" {
                    math_depth = depth;
                    formula.clear();
                }
                if name == "p" {
                    if profile == XmlProfile::Document && body_depth.is_none() {
                        return Err(malformed(
                            Some(part),
                            "paragraph is outside the document body",
                        ));
                    }
                    if paragraph.is_some() {
                        if options.error_policy == into_markdown_core::ErrorPolicy::Strict {
                            return Err(malformed(Some(part), "nested paragraphs are unsupported"));
                        }
                        let outer = paragraph.take().expect("checked above");
                        finish_paragraph(
                            outer,
                            part,
                            relationships,
                            styles,
                            numbering,
                            package,
                            options,
                            context,
                            state,
                            tables.last_mut(),
                        )?;
                        hyperlink = None;
                        field_active = false;
                        state.warning(
                            "word.unsupportedWrapperOmitted",
                            "nested paragraph wrapper was flattened in document order",
                            part,
                        );
                    }
                    if tables.last().is_some_and(|table| !table.cell_open) {
                        return Err(malformed(Some(part), "table paragraph is outside a cell"));
                    }
                    paragraph = Some(Paragraph::default());
                    paragraph_nesting = paragraph_nesting.saturating_add(1);
                } else if name == "instrText" && !field_active {
                    if options.error_policy == into_markdown_core::ErrorPolicy::Strict {
                        return Err(malformed(Some(part), "field instruction is outside a field"));
                    }
                    field_active = true;
                    if let Some(p) = &mut paragraph {
                        p.field.clear();
                    }
                    state.warning(
                        "word.unsupportedWrapperOmitted",
                        "field instruction without a begin marker was recovered",
                        part,
                    );
                } else if name == "tbl" {
                    if profile == XmlProfile::Document && body_depth.is_none() {
                        return Err(malformed(Some(part), "table is outside the document body"));
                    }
                    if !tables.is_empty() {
                        if options.error_policy == into_markdown_core::ErrorPolicy::Strict {
                            return Err(malformed(Some(part), "nested tables are unsupported"));
                        }
                        if !tables.last().is_some_and(|table| table.cell_open) {
                            return Err(malformed(
                                Some(part),
                                "nested table is outside a table cell",
                            ));
                        }
                        state.warning(
                            "word.tableNormalized",
                            "nested table was preserved inside its containing cell",
                            part,
                        );
                    }
                    tables.try_reserve(1).map_err(|error| {
                        limit("max_memory_bytes", format!("cannot reserve table stack: {error}"))
                    })?;
                    tables.push(TableBuild::default());
                } else if name == "tr" {
                    if let Some(t) = tables.last_mut() {
                        if t.row_open {
                            return Err(malformed(Some(part), "nested table rows are invalid"));
                        }
                        t.cells.clear();
                        t.cell_merges.clear();
                        t.row_header = false;
                        t.row_open = true;
                    } else {
                        return Err(malformed(Some(part), "table row is outside a table"));
                    }
                } else if name == "tc" {
                    if let Some(t) = tables.last_mut() {
                        if !t.row_open || t.cell_open {
                            return Err(malformed(Some(part), "invalid table cell hierarchy"));
                        }
                        t.cell_blocks.clear();
                        t.cell_column_span = 1;
                        t.cell_vertical_merge = VerticalMerge::None;
                        t.cell_open = true;
                    } else {
                        return Err(malformed(Some(part), "table cell is outside a table"));
                    }
                } else if name == "hyperlink" {
                    if let Some(id) = attr_local(&e, "id", part)? {
                        let relation = relationships.get(&id).ok_or_else(|| {
                            malformed(Some(part), format!("hyperlink relationship {id} is missing"))
                        })?;
                        if relation.kind != relationship_type("hyperlink") {
                            return Err(malformed(
                                Some(part),
                                "hyperlink uses a non-hyperlink relationship",
                            ));
                        }
                        let target = if relation.external {
                            relation.target.clone()
                        } else {
                            resolve_target(part, &relation.target)?
                        };
                        hyperlink = Some((target, Vec::new()));
                    } else if let Some(anchor) = attr_local(&e, "anchor", part)? {
                        hyperlink = Some((format!("#{anchor}"), Vec::new()));
                    }
                } else if name == "r" {
                    if paragraph.is_none() && math_depth == 0 {
                        return Err(malformed(Some(part), "run is outside a paragraph"));
                    }
                    marks.clear();
                } else if name == "docPr" {
                    if let Some(p) = &mut paragraph {
                        p.pending_alt = attr(&e, b"descr", part)?.or(attr(&e, b"title", part)?);
                    }
                } else if matches!(name.as_str(), "blip" | "imagedata") {
                    if let Some(p) = &mut paragraph
                        && let Some(id) =
                            attr_local(&e, "embed", part)?.or(attr_local(&e, "id", part)?)
                    {
                        p.images.push((id, p.pending_alt.take()));
                    }
                } else if name == "vMerge" {
                    if let Some(table) = tables.last_mut() {
                        table.cell_vertical_merge =
                            parse_vertical_merge(attr_local(&e, "val", part)?.as_deref());
                    }
                } else if name == "altChunk" {
                    convert_alt_chunk(
                        &reader,
                        &e,
                        &AltChunkScope {
                            owner: part,
                            relationships,
                            package,
                            options,
                            context,
                        },
                        state,
                        tables.last_mut(),
                    )?;
                } else if matches!(name.as_str(), "object" | "chart" | "relIds" | "OLEObject") {
                    recover_unsupported_word_object(&name, part, options, state)?;
                    push_word_object_placeholder(
                        &name,
                        part,
                        state,
                        &mut paragraph,
                        &mut hyperlink,
                        tables.last_mut(),
                    )?;
                } else if matches!(
                    name.as_str(),
                    "headerReference"
                        | "footerReference"
                        | "footnoteReference"
                        | "endnoteReference"
                        | "commentReference"
                        | "fldChar"
                ) {
                    if options.error_policy == into_markdown_core::ErrorPolicy::Strict {
                        return Err(malformed(
                            Some(part),
                            "reference and field marker elements must be empty",
                        ));
                    }
                    state.warning(
                        "word.unsupportedWrapperOmitted",
                        "non-empty reference or field marker was flattened",
                        part,
                    );
                }
            }
            Event::Empty(e) => {
                let name = interpreted_word_local(&reader, e.name(), part)?.unwrap_or_default();
                if skipped_choice_depth.is_some()
                    || (name == "Choice"
                        && element_stack.last().is_some_and(|parent| parent == "AlternateContent"))
                {
                    continue;
                }
                if let Some(p) = &mut paragraph {
                    match name.as_str() {
                        "pStyle" => p.style = attr_local(&e, "val", part)?,
                        "numId" => p.num_id = attr_local(&e, "val", part)?,
                        "ilvl" => {
                            p.level = attr_local(&e, "val", part)?
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0);
                        }
                        "b" => push_unique_mark(&mut marks, InlineMark::Bold),
                        "i" => push_unique_mark(&mut marks, InlineMark::Italic),
                        "strike" | "dstrike" => {
                            push_unique_mark(&mut marks, InlineMark::Strikethrough);
                        }
                        "u" => push_unique_mark(&mut marks, InlineMark::Underline),
                        "vertAlign" => match attr_local(&e, "val", part)?.as_deref() {
                            Some("superscript") => {
                                push_unique_mark(&mut marks, InlineMark::Superscript);
                            }
                            Some("subscript") => {
                                push_unique_mark(&mut marks, InlineMark::Subscript);
                            }
                            _ => {}
                        },
                        "tab" => push_inline(
                            p,
                            &mut hyperlink,
                            Inline::Text { value: "\t".into(), marks: marks.clone() },
                        ),
                        "br" | "cr" => push_inline(p, &mut hyperlink, Inline::LineBreak),
                        "footnoteReference" | "endnoteReference" => {
                            if let Some(id) = attr_local(&e, "id", part)? {
                                let label = if name == "footnoteReference" {
                                    state.footnote_refs.insert(id.clone());
                                    id.clone()
                                } else {
                                    state.endnote_refs.insert(id.clone());
                                    format!("endnote-{id}")
                                };
                                push_inline(p, &mut hyperlink, Inline::FootnoteReference(label));
                            }
                        }
                        "commentReference" => {
                            if let Some(id) = attr_local(&e, "id", part)? {
                                state.comment_refs.insert(id);
                            }
                        }
                        "docPr" => {
                            p.pending_alt = attr(&e, b"descr", part)?.or(attr(&e, b"title", part)?);
                        }
                        "blip" | "imagedata" => {
                            if let Some(id) =
                                attr_local(&e, "embed", part)?.or(attr_local(&e, "id", part)?)
                            {
                                p.images.push((id, p.pending_alt.take()));
                            }
                        }
                        "fldChar" => match attr_local(&e, "fldCharType", part)?.as_deref() {
                            Some("begin") => {
                                if field_active {
                                    return Err(malformed(
                                        Some(part),
                                        "nested fields are unsupported",
                                    ));
                                }
                                field_active = true;
                                p.field.clear();
                            }
                            Some("separate") => {
                                emit_field(p, &mut hyperlink);
                                field_active = false;
                            }
                            Some("end") => {
                                if field_active {
                                    emit_field(p, &mut hyperlink);
                                }
                                field_active = false;
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                if matches!(name.as_str(), "headerReference" | "footerReference") {
                    if profile != XmlProfile::Document {
                        return Err(malformed(
                            Some(part),
                            "section relationship outside main document",
                        ));
                    }
                    let id = attr_local(&e, "id", part)?.ok_or_else(|| {
                        malformed(Some(part), "header/footer reference lacks relationship id")
                    })?;
                    let relationship = relationships.get(&id).ok_or_else(|| {
                        malformed(Some(part), format!("section relationship {id} is missing"))
                    })?;
                    let suffix = if name == "headerReference" { "header" } else { "footer" };
                    if relationship.external || relationship.kind != relationship_type(suffix) {
                        return Err(malformed(
                            Some(part),
                            "section reference has the wrong relationship type",
                        ));
                    }
                    state.related_parts.push((
                        resolve_target(part, &relationship.target)?,
                        if suffix == "header" { "Header" } else { "Footer" },
                    ));
                }
                if name == "altChunk" {
                    convert_alt_chunk(
                        &reader,
                        &e,
                        &AltChunkScope {
                            owner: part,
                            relationships,
                            package,
                            options,
                            context,
                        },
                        state,
                        tables.last_mut(),
                    )?;
                } else if matches!(name.as_str(), "object" | "chart" | "relIds" | "OLEObject") {
                    recover_unsupported_word_object(&name, part, options, state)?;
                    push_word_object_placeholder(
                        &name,
                        part,
                        state,
                        &mut paragraph,
                        &mut hyperlink,
                        tables.last_mut(),
                    )?;
                }
                if let Some(t) = tables.last_mut() {
                    if name == "gridSpan" {
                        t.cell_column_span = attr_local(&e, "val", part)?
                            .and_then(|value| value.parse::<u32>().ok())
                            .filter(|value| *value > 0)
                            .ok_or_else(|| {
                                malformed(Some(part), "table gridSpan must be positive")
                            })?;
                    } else if name == "tblHeader" {
                        t.row_header = true;
                    } else if name == "vMerge" {
                        t.cell_vertical_merge =
                            parse_vertical_merge(attr_local(&e, "val", part)?.as_deref());
                    }
                }
            }
            Event::Text(e) => {
                if skipped_choice_depth.is_some() {
                    continue;
                }
                append_word_text(
                    decode_text(&e, part)?,
                    element_stack.last().map(String::as_str),
                    math_depth,
                    field_active,
                    &mut formula,
                    &mut paragraph,
                    &mut hyperlink,
                    &marks,
                    options,
                )?;
            }
            Event::CData(e) => {
                if skipped_choice_depth.is_some() {
                    continue;
                }
                append_word_text(
                    decode_cdata(&e, part)?,
                    element_stack.last().map(String::as_str),
                    math_depth,
                    field_active,
                    &mut formula,
                    &mut paragraph,
                    &mut hyperlink,
                    &marks,
                    options,
                )?;
            }
            Event::GeneralRef(e) => {
                if skipped_choice_depth.is_some() {
                    continue;
                }
                append_word_text(
                    decode_reference(&e, part)?,
                    element_stack.last().map(String::as_str),
                    math_depth,
                    field_active,
                    &mut formula,
                    &mut paragraph,
                    &mut hyperlink,
                    &marks,
                    options,
                )?;
            }
            Event::End(e) => {
                let name = interpreted_word_local(&reader, e.name(), part)?.unwrap_or_default();
                if let Some(skip_depth) = skipped_choice_depth {
                    element_stack.pop();
                    if depth == skip_depth {
                        skipped_choice_depth = None;
                    }
                    depth = depth.saturating_sub(1);
                    continue;
                }
                if name == "oMath" {
                    if let Some(p) = &mut paragraph
                        && !formula.is_empty()
                    {
                        push_inline(
                            p,
                            &mut hyperlink,
                            Inline::Formula(std::mem::take(&mut formula)),
                        );
                    } else if !formula.is_empty() {
                        let node =
                            state.node(Block::Formula(std::mem::take(&mut formula)), part)?;
                        if let Some(table) = tables.last_mut() {
                            table.cell_blocks.push(node);
                        } else {
                            state.document.blocks.push(node);
                        }
                    }
                    math_depth = 0;
                }
                if name == "hyperlink" {
                    if let (Some(p), Some((target, content))) = (&mut paragraph, hyperlink.take())
                        && !content.is_empty()
                    {
                        p.inlines.push(Inline::Link { target, content });
                    }
                } else if name == "p" {
                    if field_active {
                        if options.error_policy == into_markdown_core::ErrorPolicy::Strict {
                            return Err(malformed(
                                Some(part),
                                "field instruction crosses a paragraph",
                            ));
                        }
                        if let Some(p) = &mut paragraph {
                            emit_field(p, &mut hyperlink);
                        }
                        field_active = false;
                        state.warning(
                            "word.unsupportedWrapperOmitted",
                            "unterminated field instruction was closed at the paragraph boundary",
                            part,
                        );
                    }
                    if let Some(p) = paragraph.take() {
                        finish_paragraph(
                            p,
                            part,
                            relationships,
                            styles,
                            numbering,
                            package,
                            options,
                            context,
                            state,
                            tables.last_mut(),
                        )?;
                    }
                    if paragraph_nesting > 1 {
                        paragraph_nesting -= 1;
                        paragraph = Some(Paragraph::default());
                    } else {
                        paragraph_nesting = 0;
                    }
                } else if name == "tc" {
                    if let Some(t) = tables.last_mut() {
                        if !t.cell_open {
                            return Err(malformed(Some(part), "table cell closes without opening"));
                        }
                        t.cells.push(Cell {
                            row_span: 1,
                            column_span: t.cell_column_span,
                            header: t.row_header,
                            blocks: std::mem::take(&mut t.cell_blocks),
                        });
                        t.cell_merges.push(t.cell_vertical_merge);
                        t.cell_open = false;
                    }
                } else if name == "tr" {
                    if let Some(t) = tables.last_mut() {
                        if !t.row_open || t.cell_open {
                            return Err(malformed(
                                Some(part),
                                "table row closes with an open cell",
                            ));
                        }
                        t.rows.push(TableRow { cells: std::mem::take(&mut t.cells) });
                        t.vertical_merges.push(std::mem::take(&mut t.cell_merges));
                        t.row_open = false;
                    }
                } else if name == "tbl" {
                    if let Some(mut t) = tables.pop() {
                        if t.row_open || t.cell_open {
                            return Err(malformed(Some(part), "table closes with incomplete rows"));
                        }
                        if validate_table_limits(&t.rows, part, options)? {
                            normalize_ragged_table(&mut t.rows, options)?;
                            state.warning(
                                "word.tableNormalized",
                                "ragged table rows were padded to a consistent logical width",
                                part,
                            );
                        }
                        normalize_vertical_merges(
                            &mut t.rows,
                            &t.vertical_merges,
                            part,
                            options,
                            state,
                        )?;
                        let node = state.node(
                            Block::Table { rows: t.rows, alignments: Vec::<TableAlignment>::new() },
                            part,
                        )?;
                        if let Some(parent) = tables.last_mut() {
                            parent.cell_blocks.push(node);
                        } else {
                            state.document.blocks.push(node);
                        }
                    }
                } else if name == "body" {
                    body_depth = None;
                }
                element_stack.pop();
                depth = depth.saturating_sub(1);
            }
            Event::DocType(_) => return Err(malformed(Some(part), "DOCTYPE is forbidden")),
            Event::Eof => break,
            _ => {}
        }
    }
    if paragraph.is_some()
        || !tables.is_empty()
        || depth != 0
        || !element_stack.is_empty()
        || skipped_choice_depth.is_some()
    {
        return Err(malformed(Some(part), "truncated WordprocessingML structure"));
    }
    Ok(())
}
