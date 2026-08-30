use base64::Engine as _;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AltChunkKind {
    Html,
    Xhtml,
    Mhtml,
    Rtf,
}

struct AltChunkScope<'a> {
    owner: &'a str,
    relationships: &'a BTreeMap<String, Relationship>,
    package: &'a Package,
    options: &'a ConversionOptions,
    context: &'a ExecutionContext,
}

fn convert_alt_chunk(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    scope: &AltChunkScope<'_>,
    state: &mut ParseState,
    table: Option<&mut TableBuild>,
) -> Result<(), ConversionError> {
    let AltChunkScope { owner, relationships, package, options, context } = scope;
    let Some(id) = relationship_attribute(reader, element, b"id", owner)? else {
        return push_alt_chunk_placeholder(
            state,
            table,
            owner,
            "word.unsupportedWrapperOmitted",
            "altChunk has no authoritative relationship id",
        );
    };
    let Some(relationship) = relationships.get(&id) else {
        return push_alt_chunk_placeholder(
            state,
            table,
            owner,
            "word.unsupportedWrapperOmitted",
            &format!("altChunk relationship {id} is missing"),
        );
    };
    if relationship.kind != relationship_type("aFChunk") {
        return Err(malformed(
            Some(owner),
            format!("altChunk relationship {id} has the wrong relationship type"),
        ));
    }
    if relationship.external {
        return push_alt_chunk_placeholder(
            state,
            table,
            owner,
            "office.relationshipOmitted",
            "external altChunk was not downloaded",
        );
    }
    let target = resolve_target(owner, &relationship.target)?;
    let Some(bytes) = package.parts.get(&target).map(Vec::as_slice) else {
        return push_alt_chunk_placeholder(
            state,
            table,
            owner,
            "word.unsupportedWrapperOmitted",
            &format!("altChunk target {target} is missing"),
        );
    };
    let Some(kind) = package.content_types.content_type(&target).and_then(alt_chunk_kind) else {
        return push_alt_chunk_placeholder(
            state,
            table,
            &target,
            "word.unsupportedWrapperOmitted",
            "altChunk media type is unsupported",
        );
    };
    let converted = convert_alt_chunk_payload(kind, bytes, &target, options, context)?;
    let output = match converted {
        Ok(output) => output,
        Err(error)
            if options.error_policy == into_markdown_core::ErrorPolicy::BestEffort
                && recoverable_alt_chunk_error(&error) =>
        {
            return push_alt_chunk_placeholder(
                state,
                table,
                &target,
                "word.unsupportedWrapperOmitted",
                &format!("altChunk content could not be converted: {error}"),
            );
        }
        Err(error) => return Err(error),
    };
    append_alt_chunk_output(state, output, &target, options, table)?;
    state.diagnostics.push(Diagnostic {
        code: "word.altChunkConverted".into(),
        severity: DiagnosticSeverity::Info,
        message: "local altChunk content was converted without external access".into(),
        locator: Some(SourceLocator {
            part: Some(target),
            ..SourceLocator::default()
        }),
    });
    Ok(())
}

fn convert_alt_chunk_payload(
    kind: AltChunkKind,
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Result<ConverterOutput, ConversionError>, ConversionError> {
    context.checkpoint()?;
    let converted = match kind {
        AltChunkKind::Html => {
            reject_active_embedded_markup(bytes, part)?;
            crate::html::convert_embedded_html_with_images(bytes, &[], options, context)
        }
        AltChunkKind::Xhtml => {
            reject_active_embedded_markup(bytes, part)?;
            xml_budget(bytes, options)?;
            crate::html::convert_embedded_html_with_images(bytes, &[], options, context)
        }
        AltChunkKind::Rtf => crate::rtf::convert_rtf_bytes(bytes, options, context),
        AltChunkKind::Mhtml => {
            let (html, xhtml) = match extract_mhtml_html(bytes, part, options, context) {
                Ok(value) => value,
                Err(error @ ConversionError::ResourceLimit { .. }) => return Err(error),
                Err(error) => return Ok(Err(error)),
            };
            reject_active_embedded_markup(&html, part)?;
            if xhtml {
                xml_budget(&html, options)?;
            }
            crate::html::convert_embedded_html_with_images(&html, &[], options, context)
        }
    };
    let output = match converted {
        Ok(output) => output,
        Err(error @ ConversionError::ResourceLimit { .. }) => return Err(error),
        Err(error) => return Ok(Err(error)),
    };
    if output.document.blocks.is_empty()
        && output.assets.is_empty()
        && output.source_content_evidence() == SourceContentEvidence::Unknown
    {
        return Ok(Err(ConversionError::EmptyContent));
    }
    Ok(Ok(output))
}

fn alt_chunk_kind(content_type: &str) -> Option<AltChunkKind> {
    let media_type = content_type.split(';').next()?.trim().to_ascii_lowercase();
    match media_type.as_str() {
        "text/html" => Some(AltChunkKind::Html),
        "application/xhtml+xml" => Some(AltChunkKind::Xhtml),
        "message/rfc822" | "multipart/related" => Some(AltChunkKind::Mhtml),
        "application/rtf" | "text/rtf" => Some(AltChunkKind::Rtf),
        _ => None,
    }
}

fn recoverable_alt_chunk_error(error: &ConversionError) -> bool {
    matches!(
        error,
        ConversionError::Malformed { .. }
            | ConversionError::Unsupported { .. }
            | ConversionError::EmptyContent
    )
}

fn relationship_attribute(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    expected_local: &[u8],
    part: &str,
) -> Result<Option<String>, ConversionError> {
    let mut found = None;
    for attribute in element.attributes() {
        let attribute = attribute
            .map_err(|error| malformed(Some(part), format!("invalid XML attribute: {error}")))?;
        let (namespace, local) = reader.resolve_attribute(attribute.key);
        let namespace = match namespace {
            ResolveResult::Bound(value) => value,
            ResolveResult::Unbound => continue,
            ResolveResult::Unknown(prefix) => {
                return Err(malformed(
                    Some(part),
                    format!("undeclared attribute prefix {}", String::from_utf8_lossy(&prefix)),
                ));
            }
        };
        if matches!(namespace.as_ref(), OFFICE_REL_NS | STRICT_OFFICE_REL_NS)
            && local.as_ref() == expected_local
        {
            if found.is_some() {
                return Err(malformed(Some(part), "ambiguous relationship attribute"));
            }
            found = Some(decode_xml_attribute(attribute.value.as_ref(), part)?);
        }
    }
    Ok(found)
}

fn append_alt_chunk_output(
    state: &mut ParseState,
    mut output: ConverterOutput,
    part: &str,
    options: &ConversionOptions,
    table: Option<&mut TableBuild>,
) -> Result<(), ConversionError> {
    state.next_alt_chunk = state
        .next_alt_chunk
        .checked_add(1)
        .ok_or_else(|| limit("max_document_nodes", "altChunk count overflow"))?;
    let prefix = format!("docx-altchunk-{}", state.next_alt_chunk);
    let mut asset_ids = BTreeMap::new();
    for asset in &mut output.assets {
        let size = u64::try_from(asset.bytes.len()).unwrap_or(u64::MAX);
        if size > options.limits.max_asset_bytes {
            return Err(limit(
                "max_asset_bytes",
                format!("altChunk asset {}: {size} > {}", asset.id.0, options.limits.max_asset_bytes),
            ));
        }
        state.asset_bytes = state
            .asset_bytes
            .checked_add(size)
            .ok_or_else(|| limit("max_total_asset_bytes", "altChunk asset bytes overflow"))?;
        if state.asset_bytes > options.limits.max_total_asset_bytes {
            return Err(limit(
                "max_total_asset_bytes",
                format!("{} > {}", state.asset_bytes, options.limits.max_total_asset_bytes),
            ));
        }
        let replacement = AssetId(format!("{prefix}-{}", asset.id.0));
        asset_ids.insert(asset.id.clone(), replacement.clone());
        asset.id = replacement;
    }
    for node in &mut output.document.blocks {
        remap_alt_chunk_node(node, part, &asset_ids, &mut state.next_node, &mut state.inline_count)?;
    }
    for diagnostic in &mut output.diagnostics {
        if matches!(
            diagnostic.code.as_str(),
            "html.sourceLocationUnavailable" | "html.mainContentFallback"
        ) {
            diagnostic.severity = DiagnosticSeverity::Info;
        }
        diagnostic.locator.get_or_insert_with(SourceLocator::default).part = Some(part.into());
    }
    if let Some(table) = table {
        table.cell_blocks.append(&mut output.document.blocks);
    } else {
        state.document.blocks.append(&mut output.document.blocks);
    }
    state.assets.append(&mut output.assets);
    state.diagnostics.append(&mut output.diagnostics);
    state.nested_outputs.push(output);
    Ok(())
}

fn remap_alt_chunk_node(
    node: &mut BlockNode,
    part: &str,
    asset_ids: &BTreeMap<AssetId, AssetId>,
    next_node: &mut usize,
    inline_count: &mut usize,
) -> Result<(), ConversionError> {
    *next_node = next_node
        .checked_add(1)
        .ok_or_else(|| limit("max_document_nodes", "node count overflow"))?;
    if *next_node > MAX_DOCUMENT_NODES {
        return Err(limit("max_document_nodes", format!("{} > {MAX_DOCUMENT_NODES}", *next_node)));
    }
    node.id = NodeId(format!("docx-{next_node}"));
    node.provenance.provider = PROVIDER_ID.into();
    node.provenance.locator.part = Some(part.into());
    match &mut node.block {
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
            remap_alt_chunk_inlines(inlines, part, inline_count)?;
        }
        Block::List { items, .. } => {
            for item in items {
                for child in &mut item.blocks {
                    remap_alt_chunk_node(child, part, asset_ids, next_node, inline_count)?;
                }
            }
        }
        Block::Table { rows, .. } => {
            for row in rows {
                for cell in &mut row.cells {
                    for child in &mut cell.blocks {
                        remap_alt_chunk_node(child, part, asset_ids, next_node, inline_count)?;
                    }
                }
            }
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => {
            for child in blocks {
                remap_alt_chunk_node(child, part, asset_ids, next_node, inline_count)?;
            }
        }
        Block::Image { asset, .. } => {
            if let Some(replacement) = asset_ids.get(asset) {
                *asset = replacement.clone();
            }
        }
        Block::TimedSegment { content, .. } => {
            remap_alt_chunk_inlines(content, part, inline_count)?;
        }
        _ => {}
    }
    Ok(())
}

fn remap_alt_chunk_inlines(
    inlines: &mut [Inline],
    part: &str,
    inline_count: &mut usize,
) -> Result<(), ConversionError> {
    *inline_count = inline_count
        .checked_add(inlines.len())
        .ok_or_else(|| limit("max_document_inlines", "inline count overflow"))?;
    if *inline_count > MAX_DOCUMENT_INLINES {
        return Err(limit(
            "max_document_inlines",
            format!("{} > {MAX_DOCUMENT_INLINES}", *inline_count),
        ));
    }
    for inline in inlines {
        match inline {
            Inline::SourceText { provenance, .. } | Inline::OcrText { provenance, .. } => {
                provenance.locator.part = Some(part.into());
            }
            Inline::Link { content, .. } => {
                remap_alt_chunk_inlines(content, part, inline_count)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn push_alt_chunk_placeholder(
    state: &mut ParseState,
    table: Option<&mut TableBuild>,
    part: &str,
    code: &str,
    message: &str,
) -> Result<(), ConversionError> {
    state.warning(code, message, part);
    state.add_inlines(1)?;
    let node = state.node(
        Block::Paragraph(vec![Inline::Text {
            value: "[Embedded Word content omitted]".into(),
            marks: Vec::new(),
        }]),
        part,
    )?;
    if let Some(table) = table {
        table.cell_blocks.push(node);
    } else {
        state.document.blocks.push(node);
    }
    Ok(())
}

fn reject_active_embedded_markup(bytes: &[u8], part: &str) -> Result<(), ConversionError> {
    if bytes.contains(&0) {
        return Err(malformed(Some(part), "embedded markup contains NUL"));
    }
    let lower = bytes.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    if lower.windows(8).any(|value| value == b"<!entity") {
        return Err(malformed(Some(part), "embedded DTD and entity declarations are forbidden"));
    }
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].windows(9).position(|value| value == b"<!doctype") {
        let start = cursor + relative;
        let end = lower[start..]
            .iter()
            .position(|value| *value == b'>')
            .map(|relative| start + relative)
            .ok_or_else(|| malformed(Some(part), "unterminated embedded DOCTYPE"))?;
        let declaration = &lower[start..=end];
        if declaration.contains(&b'[') || declaration.windows(6).any(|value| value == b"system") {
            return Err(malformed(
                Some(part),
                "embedded DOCTYPE has an active external or internal subset",
            ));
        }
        cursor = end + 1;
    }
    Ok(())
}

fn extract_mhtml_html(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(Vec<u8>, bool), ConversionError> {
    let (headers, body) = split_mime_headers(bytes, part, options)?;
    let content_type = headers
        .get("content-type")
        .ok_or_else(|| malformed(Some(part), "MHTML root has no Content-Type"))?;
    if !content_type.to_ascii_lowercase().starts_with("multipart/") {
        return Err(malformed(Some(part), "MHTML root is not multipart"));
    }
    let boundary = mime_parameter(content_type, "boundary")
        .filter(|value| !value.is_empty() && value.len() <= 200)
        .ok_or_else(|| malformed(Some(part), "MHTML root has no safe boundary"))?;
    if boundary.bytes().any(|value| !(0x21..=0x7e).contains(&value)) {
        return Err(malformed(Some(part), "MHTML boundary is not visible ASCII"));
    }
    let delimiter = format!("--{boundary}").into_bytes();
    let mut cursor = 0;
    let mut html = None;
    while let Some(offset) = find_mime_boundary(body, &delimiter, cursor) {
        context.checkpoint()?;
        let mut start = offset + delimiter.len();
        if body.get(start..start + 2) == Some(b"--") {
            break;
        }
        start = skip_mime_line_break(body, start);
        let next = find_mime_boundary(body, &delimiter, start).unwrap_or(body.len());
        let section = trim_mime_line_break(&body[start..next]);
        let (part_headers, payload) = split_mime_headers(section, part, options)?;
        let media_type = part_headers
            .get("content-type")
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or_default();
        if media_type.eq_ignore_ascii_case("text/html")
            || media_type.eq_ignore_ascii_case("application/xhtml+xml")
        {
            if html.is_some() {
                return Err(malformed(Some(part), "MHTML contains ambiguous HTML roots"));
            }
            let encoding = part_headers
                .get("content-transfer-encoding")
                .map(|value| value.trim().to_ascii_lowercase())
                .unwrap_or_default();
            let decoded = match encoding.as_str() {
                "quoted-printable" => decode_quoted_printable(payload, part, options)?,
                "base64" => decode_mime_base64(payload, part, options)?,
                "" | "7bit" | "8bit" | "binary" => bounded_mime_copy(payload, part, options)?,
                _ => {
                    return Err(malformed(
                        Some(part),
                        "MHTML HTML root uses an unsupported transfer encoding",
                    ));
                }
            };
            html = Some((decoded, media_type.eq_ignore_ascii_case("application/xhtml+xml")));
        }
        cursor = next;
        if next == body.len() {
            break;
        }
    }
    html.ok_or_else(|| malformed(Some(part), "MHTML contains no HTML root"))
}

fn split_mime_headers<'a>(
    bytes: &'a [u8],
    part: &str,
    options: &ConversionOptions,
) -> Result<(BTreeMap<String, String>, &'a [u8]), ConversionError> {
    let (offset, separator) = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|offset| (offset, 4))
        .or_else(|| {
            bytes
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|offset| (offset, 2))
        })
        .ok_or_else(|| malformed(Some(part), "MIME headers are incomplete"))?;
    if u64::try_from(offset).unwrap_or(u64::MAX) > options.limits.max_field_bytes {
        return Err(limit("max_field_bytes", "MIME headers exceed the field budget"));
    }
    let header_bytes = &bytes[..offset];
    if !header_bytes.is_ascii() {
        return Err(malformed(Some(part), "MIME structural headers are not ASCII"));
    }
    let normalized = String::from_utf8_lossy(header_bytes).replace("\r\n", "\n");
    let mut unfolded = Vec::<String>::new();
    for line in normalized.split('\n') {
        if line.starts_with([' ', '\t']) {
            let previous = unfolded
                .last_mut()
                .ok_or_else(|| malformed(Some(part), "MIME continuation has no field"))?;
            previous.push(' ');
            previous.push_str(line.trim());
        } else if !line.is_empty() {
            unfolded.push(line.to_owned());
        }
    }
    let mut headers = BTreeMap::new();
    for line in unfolded {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| malformed(Some(part), "MIME header has no colon"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty()
            || !name.bytes().all(|value| value.is_ascii_alphanumeric() || value == b'-')
        {
            return Err(malformed(Some(part), "MIME header name is invalid"));
        }
        if headers.insert(name, value.trim().to_owned()).is_some() {
            return Err(malformed(Some(part), "duplicate MIME structural header"));
        }
    }
    Ok((headers, &bytes[offset + separator..]))
}

fn mime_parameter(value: &str, expected: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|parameter| {
        let (name, raw) = parameter.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case(expected) {
            return None;
        }
        let raw = raw.trim();
        Some(
            raw.strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(raw)
                .to_owned(),
        )
    })
}

fn find_mime_boundary(body: &[u8], delimiter: &[u8], from: usize) -> Option<usize> {
    body.get(from..)?
        .windows(delimiter.len())
        .position(|window| window == delimiter)
        .map(|relative| from + relative)
        .and_then(|mut offset| loop {
            let before_ok = offset == 0 || body.get(offset.wrapping_sub(1)) == Some(&b'\n');
            let after = offset + delimiter.len();
            let after_ok = matches!(body.get(after), None | Some(b'\r' | b'\n' | b'-'));
            if before_ok && after_ok {
                return Some(offset);
            }
            offset = body
                .get(offset + 1..)?
                .windows(delimiter.len())
                .position(|window| window == delimiter)
                .map(|relative| offset + 1 + relative)?;
        })
}

fn skip_mime_line_break(bytes: &[u8], offset: usize) -> usize {
    if bytes.get(offset..offset + 2) == Some(b"\r\n") {
        offset + 2
    } else if bytes.get(offset) == Some(&b'\n') {
        offset + 1
    } else {
        offset
    }
}

fn trim_mime_line_break(mut bytes: &[u8]) -> &[u8] {
    if bytes.ends_with(b"\r\n") {
        bytes = &bytes[..bytes.len() - 2];
    } else if bytes.ends_with(b"\n") {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn bounded_mime_copy(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
) -> Result<Vec<u8>, ConversionError> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > options.limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            format!("decoded MHTML part {part} exceeds the decompression budget"),
        ));
    }
    Ok(bytes.to_vec())
}

fn decode_mime_base64(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
) -> Result<Vec<u8>, ConversionError> {
    let compact = bytes
        .iter()
        .copied()
        .filter(|value| !value.is_ascii_whitespace())
        .collect::<Vec<_>>();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|_| malformed(Some(part), "invalid MHTML base64 payload"))?;
    bounded_mime_copy(&decoded, part, options)
}

fn decode_quoted_printable(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
) -> Result<Vec<u8>, ConversionError> {
    let mut decoded = Vec::new();
    decoded.try_reserve(bytes.len()).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve MHTML decode buffer: {error}"))
    })?;
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] != b'=' {
            decoded.push(bytes[cursor]);
            cursor += 1;
            continue;
        }
        if bytes.get(cursor + 1..cursor + 3) == Some(b"\r\n") {
            cursor += 3;
            continue;
        }
        if bytes.get(cursor + 1) == Some(&b'\n') {
            cursor += 2;
            continue;
        }
        let hex = bytes
            .get(cursor + 1..cursor + 3)
            .ok_or_else(|| malformed(Some(part), "truncated quoted-printable escape"))?;
        let high = hex_value(hex[0])
            .ok_or_else(|| malformed(Some(part), "invalid quoted-printable escape"))?;
        let low = hex_value(hex[1])
            .ok_or_else(|| malformed(Some(part), "invalid quoted-printable escape"))?;
        decoded.push((high << 4) | low);
        cursor += 3;
    }
    bounded_mime_copy(&decoded, part, options)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}
