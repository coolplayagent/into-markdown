//! Strict, offline nbformat 4 conversion into the common document IR.

use crate::markdown;
use base64::Engine as _;
use image::{
    AnimationDecoder as _, ImageDecoder as _, ImageFormat, ImageReader, Limits as ImageLimits,
    codecs::gif::GifDecoder,
};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, ConversionError, ConversionOptions, Converter,
    ConverterOutput, Diagnostic, DiagnosticSeverity, Document, DocumentMetadata, ExecutionContext,
    FormatCandidate, Inline, InputFormat, IrErrorCode, MAX_DOCUMENT_NODES, NodeId, ProbeOutcome,
    Provenance, ProvenanceKind, ResolvedInput, Services, SourceLocator,
};
use pulldown_cmark::{Event, Parser, Tag};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Cursor;

const FORMATS: &[InputFormat] = &[InputFormat::Ipynb];
const PROVIDER_ID: &str = "builtin.converter.jupyter-notebook";
const HTML_DIAGNOSTIC: &str = "notebook.htmlPreservedAsCode";
const CONTROL_DIAGNOSTIC: &str = "notebook.controlCharactersSanitized";
const ATTACHMENT_PLACEHOLDER_PREFIX: &str = "https://attachment.invalid/";

/// A deterministic nbformat 4 converter. It never evaluates notebook content.
#[derive(Debug, Default)]
pub struct NotebookConverter;

impl Converter for NotebookConverter {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> i32 {
        250
    }

    fn supported_formats(&self) -> &'static [InputFormat] {
        FORMATS
    }

    fn probe<'a>(
        &'a self,
        _: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            Ok(if candidate.format == InputFormat::Ipynb {
                ProbeOutcome::Match { confidence: 1.0 }
            } else {
                ProbeOutcome::NotApplicable
            })
        })
    }

    fn convert<'a>(
        &'a self,
        input: &'a ResolvedInput,
        _: &'a FormatCandidate,
        options: &'a ConversionOptions,
        _: &'a Services,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ConverterOutput, ConversionError>> {
        Box::pin(async move { convert_notebook(input, options, context) })
    }
}

fn malformed(part: impl Into<String>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: Some(part.into()), detail: detail.into() }
}

fn convert_notebook(
    input: &ResolvedInput,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ConverterOutput, ConversionError> {
    context.checkpoint()?;
    let json_memory = input
        .bytes
        .len()
        .checked_mul(64)
        .and_then(|value| value.checked_add(64 * 1024))
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "Notebook JSON allocation estimate overflowed".into(),
        })?;
    // This conservative reservation remains alive with the DOM, decoded assets, nested Markdown
    // IR and final document. It deliberately prices the worst case of many tiny JSON nodes much
    // higher than source bytes, and makes low-memory failure occur before serde allocates.
    let _json_memory = context.reserve_memory(u64::try_from(json_memory).map_err(|_| {
        ConversionError::ResourceLimit {
            limit: "max_memory_bytes",
            detail: "Notebook JSON allocation estimate cannot be represented as u64".into(),
        }
    })?)?;
    let json_bytes = input.bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&input.bytes);
    let root = parse_strict_json(json_bytes, options)?;
    let object = root.as_object().ok_or_else(|| malformed("notebook", "root must be an object"))?;
    let version = required_u64(object, "nbformat", "notebook")?;
    if version != 4 {
        return Err(ConversionError::Unsupported {
            detail: format!("Jupyter notebook nbformat {version} is not supported; expected 4"),
        });
    }
    let minor = required_u64(object, "nbformat_minor", "notebook")?;
    let cells = object
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("notebook.cells", "cells must be an array"))?;
    if cells.len() > MAX_DOCUMENT_NODES / 2 {
        return Err(ConversionError::ResourceLimit {
            limit: "notebook_cells",
            detail: format!("{} cells exceed the safe structural budget", cells.len()),
        });
    }
    let notebook_metadata = required_object(object, "metadata", "notebook.metadata")?;
    let mut builder = NotebookBuilder::new(options, context, version, minor, notebook_metadata)?;
    let mut cell_ids = BTreeSet::new();
    for (index, cell) in cells.iter().enumerate() {
        if index.is_multiple_of(32) {
            context.checkpoint()?;
        }
        builder.cell(index, cell, minor, &mut cell_ids)?;
    }
    builder.finish()
}

struct StrictJsonState<'a> {
    options: &'a ConversionOptions,
    nodes: usize,
    pending_error: Option<ConversionError>,
}

impl StrictJsonState<'_> {
    fn value<E: serde::de::Error>(&mut self, depth: usize) -> Result<(), E> {
        if depth > usize::from(self.options.limits.max_nesting_depth) {
            self.pending_error = Some(ConversionError::ResourceLimit {
                limit: "json_nesting_depth",
                detail: format!(
                    "Notebook JSON exceeds {} levels",
                    self.options.limits.max_nesting_depth
                ),
            });
            return Err(E::custom("Notebook JSON nesting limit exceeded"));
        }
        self.nodes = self.nodes.checked_add(1).ok_or_else(|| {
            self.pending_error = Some(ConversionError::ResourceLimit {
                limit: "json_nodes",
                detail: "Notebook JSON node count overflowed".into(),
            });
            E::custom("Notebook JSON node count overflowed")
        })?;
        if self.nodes > MAX_DOCUMENT_NODES {
            self.pending_error = Some(ConversionError::ResourceLimit {
                limit: "json_nodes",
                detail: format!("Notebook JSON exceeds {MAX_DOCUMENT_NODES} values"),
            });
            return Err(E::custom("Notebook JSON node limit exceeded"));
        }
        Ok(())
    }

    fn string<E: serde::de::Error>(&mut self, value: &str) -> Result<(), E> {
        if u64::try_from(value.len()).unwrap_or(u64::MAX) > self.options.limits.max_field_bytes {
            self.pending_error = Some(ConversionError::ResourceLimit {
                limit: "max_field_bytes",
                detail: format!(
                    "Notebook JSON string exceeds {} bytes",
                    self.options.limits.max_field_bytes
                ),
            });
            return Err(E::custom("Notebook JSON string limit exceeded"));
        }
        Ok(())
    }
}

struct StrictValueSeed<'a, 'options> {
    state: &'a mut StrictJsonState<'options>,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for StrictValueSeed<'_, '_> {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        self.state.value::<D::Error>(self.depth)?;
        deserializer.deserialize_any(StrictValueVisitor { state: self.state, depth: self.depth })
    }
}

struct StrictValueVisitor<'a, 'options> {
    state: &'a mut StrictJsonState<'options>,
    depth: usize,
}

impl<'de> Visitor<'de> for StrictValueVisitor<'_, '_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a strict JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        self.state.string::<E>(value)?;
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Value, E> {
        self.state.string::<E>(&value)?;
        Ok(Value::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        if let Some(size) = sequence.size_hint() {
            values.try_reserve(size).map_err(A::Error::custom)?;
        }
        while let Some(value) = sequence
            .next_element_seed(StrictValueSeed { state: self.state, depth: self.depth + 1 })?
        {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        let mut keys = BTreeSet::new();
        while let Some(key) = object.next_key::<String>()? {
            self.state.string::<A::Error>(&key)?;
            if !keys.insert(key.clone()) {
                self.state.pending_error =
                    Some(malformed("notebook", format!("duplicate JSON object key {key:?}")));
                return Err(A::Error::custom("duplicate JSON object key"));
            }
            let value = object
                .next_value_seed(StrictValueSeed { state: self.state, depth: self.depth + 1 })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

fn parse_strict_json(bytes: &[u8], options: &ConversionOptions) -> Result<Value, ConversionError> {
    let mut state = StrictJsonState { options, nodes: 0, pending_error: None };
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let result = StrictValueSeed { state: &mut state, depth: 0 }.deserialize(&mut deserializer);
    match result.and_then(|value| deserializer.end().map(|()| value)) {
        Ok(value) => Ok(value),
        Err(error) => Err(state
            .pending_error
            .unwrap_or_else(|| malformed("notebook", format!("invalid nbformat JSON: {error}")))),
    }
}

struct NotebookBuilder<'a> {
    options: &'a ConversionOptions,
    context: &'a ExecutionContext,
    document: Document,
    diagnostics: Vec<Diagnostic>,
    assets: Vec<Asset>,
    asset_ids: BTreeSet<String>,
    total_asset_bytes: u64,
    sequence: u64,
    language: Option<String>,
    node_count: usize,
    inline_count: usize,
}

impl<'a> NotebookBuilder<'a> {
    fn new(
        options: &'a ConversionOptions,
        context: &'a ExecutionContext,
        version: u64,
        minor: u64,
        metadata: &Map<String, Value>,
    ) -> Result<Self, ConversionError> {
        let mut properties = BTreeMap::new();
        properties.insert("jupyter.nbformat".into(), version.to_string());
        properties.insert("jupyter.nbformatMinor".into(), minor.to_string());
        properties.insert(
            "jupyter.metadata".into(),
            canonical_json(&Value::Object(metadata.clone()), options.limits.max_field_bytes)?,
        );
        let title = metadata
            .get("title")
            .and_then(Value::as_str)
            .map(|value| {
                sanitize_text_bounded(
                    value,
                    options.limits.max_field_bytes,
                    "notebook.metadata.title",
                )
            })
            .transpose()?
            .map(|(value, _)| value);
        let language = metadata
            .get("language_info")
            .and_then(Value::as_object)
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .map(sanitize_language);
        let document = Document {
            metadata: DocumentMetadata { title, authors: Vec::new(), properties },
            ..Document::default()
        };
        Ok(Self {
            options,
            context,
            document,
            diagnostics: Vec::new(),
            assets: Vec::new(),
            asset_ids: BTreeSet::new(),
            total_asset_bytes: 0,
            sequence: 0,
            language,
            node_count: 0,
            inline_count: 0,
        })
    }

    fn cell(
        &mut self,
        index: usize,
        value: &Value,
        minor: u64,
        cell_ids: &mut BTreeSet<String>,
    ) -> Result<(), ConversionError> {
        let part = format!("cells/{index}");
        let cell = value.as_object().ok_or_else(|| malformed(&part, "cell must be an object"))?;
        if minor >= 5 {
            let id = required_string(cell, "id", &part)?;
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err(malformed(
                    format!("{part}/id"),
                    "cell ID must be 1-64 ASCII letters, digits, '-' or '_'",
                ));
            }
            if !cell_ids.insert(id.into()) {
                return Err(malformed(format!("{part}/id"), format!("duplicate cell ID {id:?}")));
            }
        } else if let Some(id) = cell.get("id") {
            let id = id
                .as_str()
                .ok_or_else(|| malformed(format!("{part}/id"), "cell ID must be a string"))?;
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err(malformed(
                    format!("{part}/id"),
                    "cell ID must be 1-64 ASCII letters, digits, '-' or '_'",
                ));
            }
            if !cell_ids.insert(id.into()) {
                return Err(malformed(format!("{part}/id"), format!("duplicate cell ID {id:?}")));
            }
        }
        if let Some(id) = cell.get("id").and_then(Value::as_str) {
            self.insert_property(format!("jupyter.cell.{index}.id"), id.into())?;
        }
        let cell_type = required_string(cell, "cell_type", &part)?;
        let metadata = required_object(cell, "metadata", &format!("{part}.metadata"))?;
        self.insert_property(
            format!("jupyter.cell.{index}.metadata"),
            canonical_json(&Value::Object(metadata.clone()), self.options.limits.max_field_bytes)?,
        )?;
        let source = source_text(
            cell.get("source"),
            &format!("{part}.source"),
            self.options.limits.max_field_bytes,
        )?;
        match cell_type {
            "markdown" => self.markdown_cell(index, &source, cell),
            "code" => self.code_cell(index, &source, cell),
            "raw" => self.raw_cell(index, &source, cell),
            other => Err(malformed(part, format!("unsupported cell_type {other:?}"))),
        }
    }

    fn raw_cell(
        &mut self,
        index: usize,
        source: &str,
        cell: &Map<String, Value>,
    ) -> Result<(), ConversionError> {
        self.safe_code(Some("raw"), source, &format!("cells/{index}"))?;
        if let Some(attachments) = cell.get("attachments") {
            let attachments = attachments.as_object().ok_or_else(|| {
                malformed(format!("cells/{index}/attachments"), "attachments must be an object")
            })?;
            for (name, bundle) in attachments {
                validate_attachment_name(name, &format!("cells/{index}"))?;
                let bundle = bundle.as_object().ok_or_else(|| {
                    malformed(
                        format!("cells/{index}/attachments/{name}"),
                        "MIME bundle must be an object",
                    )
                })?;
                let Some((media_type, value)) = select_image(bundle) else {
                    return Err(malformed(
                        format!("cells/{index}/attachments/{name}"),
                        "raw attachment has no supported safe raster image representation",
                    ));
                };
                let bytes = decode_image(
                    value,
                    media_type,
                    self.options,
                    self.context,
                    &format!("cells/{index}/attachments/{name}"),
                )?;
                let id = self.add_asset(index, None, Some(name), media_type, bytes)?;
                self.push(
                    Block::Image { asset: id, alt: Some(name.clone()) },
                    &format!("cells/{index}/attachments/{name}"),
                )?;
            }
        }
        Ok(())
    }

    fn markdown_cell(
        &mut self,
        index: usize,
        source: &str,
        cell: &Map<String, Value>,
    ) -> Result<(), ConversionError> {
        let part = format!("cells/{index}");
        let attachments = match cell.get("attachments") {
            None => Map::new(),
            Some(Value::Object(value)) => value.clone(),
            Some(_) => {
                return Err(malformed(
                    format!("{part}.attachments"),
                    "attachments must be an object",
                ));
            }
        };
        let (rewritten, referenced) =
            rewrite_attachment_targets(source, index, &attachments, &part)?;
        let mut replacements = BTreeMap::new();
        for (name, bundle) in attachments {
            validate_attachment_name(&name, &part)?;
            let bundle = bundle.as_object().ok_or_else(|| {
                malformed(format!("{part}.attachments.{name}"), "MIME bundle must be an object")
            })?;
            if !referenced.contains(&name) {
                self.warning(
                    "notebook.unreferencedAttachmentIgnored",
                    format!("unreferenced attachment {name:?} was not decoded or emitted"),
                    &part,
                )?;
                continue;
            }
            let placeholder = format!("{ATTACHMENT_PLACEHOLDER_PREFIX}{index}/{}", hex_name(&name));
            let Some((media_type, data)) = select_image(bundle) else {
                return Err(malformed(
                    format!("{part}.attachments.{name}"),
                    "attachment has no supported safe raster image representation",
                ));
            };
            let bytes = decode_image(
                data,
                media_type,
                self.options,
                self.context,
                &format!("{part}.attachments.{name}"),
            )?;
            let id = self.add_asset(index, None, Some(&name), media_type, bytes)?;
            replacements.insert(placeholder, id);
        }
        let nested_input = ResolvedInput {
            bytes: rewritten.into_bytes().into(),
            metadata: input_metadata_for_nested(),
        };
        let mut converted = markdown::convert_markdown(&nested_input, self.options, self.context)?;
        self.merge_diagnostics(converted.diagnostics)?;
        for node in &mut converted.document.blocks {
            prefix_and_remap(node, &format!("cell-{index}"), &replacements);
        }
        // Only attachment-backed assets survive. Remote images remain the Markdown converter's
        // safe external-only assets and are never fetched.
        let mut bound_placeholders = BTreeSet::new();
        for mut asset in converted.assets {
            if let Some(uri) = asset.external_uri.as_ref() {
                if let Some(replacement) = replacements.get(uri) {
                    bound_placeholders.insert(uri.clone());
                    remap_asset_references(&mut converted.document.blocks, &asset.id, replacement);
                    continue;
                }
                if uri.starts_with(ATTACHMENT_PLACEHOLDER_PREFIX) {
                    return Err(malformed(
                        format!("{part}/attachments"),
                        "an internal attachment placeholder was not bound to an attachment",
                    ));
                }
            }
            {
                let old = asset.id.clone();
                asset.id.0 = format!("cell-{index}-{}", asset.id.0);
                remap_asset_references(&mut converted.document.blocks, &old, &asset.id);
                self.push_asset(asset)?;
            }
        }
        if blocks_contain_attachment_placeholder(&converted.document.blocks) {
            return Err(malformed(
                format!("{part}/attachments"),
                "inline attachment images cannot be represented by the block-only image IR",
            ));
        }
        for (placeholder, replacement) in &replacements {
            if !bound_placeholders.contains(placeholder)
                || !blocks_reference_asset(&converted.document.blocks, replacement)
            {
                return Err(malformed(
                    format!("{part}/attachments"),
                    "attachment image did not lower to a referenced internal asset",
                ));
            }
        }
        self.merge_fragment(converted.document.blocks)?;
        Ok(())
    }

    fn code_cell(
        &mut self,
        index: usize,
        source: &str,
        cell: &Map<String, Value>,
    ) -> Result<(), ConversionError> {
        let part = format!("cells/{index}");
        let count = match cell.get("execution_count") {
            Some(Value::Null) | None => None,
            Some(Value::Number(value)) => value.as_u64(),
            Some(_) => {
                return Err(malformed(
                    format!("{part}.execution_count"),
                    "must be null or a non-negative integer",
                ));
            }
        };
        if cell.get("execution_count").is_some_and(|v| !v.is_null()) && count.is_none() {
            return Err(malformed(
                format!("{part}.execution_count"),
                "must be a non-negative integer",
            ));
        }
        let language = self.language.clone();
        let label =
            count.map_or_else(|| "Code cell [ ]".into(), |value| format!("Code cell [{value}]"));
        self.push(Block::Heading { level: 3, content: text_inline(label) }, &part)?;
        self.safe_code(language.as_deref(), source, &part)?;
        let outputs = cell
            .get("outputs")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed(format!("{part}.outputs"), "outputs must be an array"))?;
        for (output_index, output) in outputs.iter().enumerate() {
            self.output(index, output_index, output)?;
        }
        Ok(())
    }

    fn output(&mut self, cell: usize, index: usize, value: &Value) -> Result<(), ConversionError> {
        let part = format!("cells/{cell}/outputs/{index}");
        let output =
            value.as_object().ok_or_else(|| malformed(&part, "output must be an object"))?;
        match required_string(output, "output_type", &part)? {
            "stream" => {
                let name = required_string(output, "name", &part)?;
                if !matches!(name, "stdout" | "stderr") {
                    return Err(malformed(
                        format!("{part}.name"),
                        "stream name must be stdout or stderr",
                    ));
                }
                let text = source_text(
                    output.get("text"),
                    &format!("{part}.text"),
                    self.options.limits.max_field_bytes,
                )?;
                self.safe_code(Some(name), &text, &part)
            }
            "error" => {
                let name = required_string(output, "ename", &part)?;
                let value = required_string(output, "evalue", &part)?;
                let traceback = source_text(
                    output.get("traceback"),
                    &format!("{part}.traceback"),
                    self.options.limits.max_field_bytes,
                )?;
                self.safe_code_segments(Some("text"), &[name, ": ", value, "\n", &traceback], &part)
            }
            "display_data" | "execute_result" | "update_display_data" => {
                let metadata = required_object(output, "metadata", &format!("{part}.metadata"))?;
                self.insert_property(
                    format!("jupyter.cell.{cell}.output.{index}.metadata"),
                    canonical_json(
                        &Value::Object(metadata.clone()),
                        self.options.limits.max_field_bytes,
                    )?,
                )?;
                if output.get("output_type").and_then(Value::as_str) == Some("execute_result") {
                    match output.get("execution_count") {
                        Some(Value::Null) | None => {}
                        Some(Value::Number(n)) if n.as_u64().is_some() => {
                            self.insert_property(
                                format!("jupyter.cell.{cell}.output.{index}.executionCount"),
                                n.to_string(),
                            )?;
                        }
                        _ => {
                            return Err(malformed(
                                format!("{part}.execution_count"),
                                "must be null or a non-negative integer",
                            ));
                        }
                    }
                }
                if let Some(transient) = output.get("transient") {
                    let transient = transient.as_object().ok_or_else(|| {
                        malformed(format!("{part}/transient"), "transient must be an object")
                    })?;
                    if let Some(display_id) = transient.get("display_id") {
                        let display_id = display_id.as_str().ok_or_else(|| {
                            malformed(
                                format!("{part}/transient/display_id"),
                                "display_id must be a string",
                            )
                        })?;
                        self.insert_property(
                            format!("jupyter.cell.{cell}.output.{index}.displayId"),
                            display_id.into(),
                        )?;
                    }
                }
                if output.get("output_type").and_then(Value::as_str) == Some("update_display_data")
                {
                    self.warning(
                        "notebook.updateDisplayPreserved",
                        "update_display_data was preserved at its source position",
                        &part,
                    )?;
                }
                let data = required_object(output, "data", &format!("{part}.data"))?;
                self.mime_bundle(cell, index, data, &part)
            }
            other => Err(malformed(part, format!("unsupported output_type {other:?}"))),
        }
    }

    fn mime_bundle(
        &mut self,
        cell: usize,
        output: usize,
        data: &Map<String, Value>,
        part: &str,
    ) -> Result<(), ConversionError> {
        if let Some((media_type, value)) = select_image(data) {
            let bytes = decode_image(value, media_type, self.options, self.context, part)?;
            let id = self.add_asset(cell, Some(output), None, media_type, bytes)?;
            return self
                .push(Block::Image { asset: id, alt: Some(format!("output {output}")) }, part);
        }
        if let Some(value) = data.get("text/markdown") {
            let text = source_text(
                Some(value),
                &format!("{part}.data.text/markdown"),
                self.options.limits.max_field_bytes,
            )?;
            let nested = ResolvedInput {
                bytes: text.into_bytes().into(),
                metadata: input_metadata_for_nested(),
            };
            let mut converted = markdown::convert_markdown(&nested, self.options, self.context)?;
            self.merge_diagnostics(converted.diagnostics)?;
            for node in &mut converted.document.blocks {
                prefix_and_remap(node, &format!("cell-{cell}-output-{output}"), &BTreeMap::new());
            }
            for mut asset in converted.assets {
                let old = asset.id.clone();
                asset.id.0 = format!("cell-{cell}-output-{output}-{}", asset.id.0);
                remap_asset_references(&mut converted.document.blocks, &old, &asset.id);
                self.push_asset(asset)?;
            }
            self.merge_fragment(converted.document.blocks)?;
            return Ok(());
        }
        if let Some(value) = data.get("text/plain") {
            let text = source_text(
                Some(value),
                &format!("{part}.data.text/plain"),
                self.options.limits.max_field_bytes,
            )?;
            return self.safe_code(Some("text"), &text, part);
        }
        if let Some(value) = data.get("text/html") {
            let html = source_text(
                Some(value),
                &format!("{part}.data.text/html"),
                self.options.limits.max_field_bytes,
            )?;
            self.warning(
                HTML_DIAGNOSTIC,
                "HTML output was preserved as non-executable fenced code",
                part,
            )?;
            return self.safe_code(Some("html"), &html, part);
        }
        self.warning(
            "notebook.unsupportedMimeBundle",
            "MIME bundle had no supported representation",
            part,
        )?;
        self.safe_code(Some("text"), "[unsupported notebook output]", part)
    }

    fn safe_code(
        &mut self,
        language: Option<&str>,
        value: &str,
        part: &str,
    ) -> Result<(), ConversionError> {
        self.safe_code_segments(language, &[value], part)
    }

    fn safe_code_segments(
        &mut self,
        language: Option<&str>,
        values: &[&str],
        part: &str,
    ) -> Result<(), ConversionError> {
        let mut sanitized = String::new();
        let mut changed = false;
        for value in values {
            changed |= append_sanitized_bounded(
                &mut sanitized,
                value,
                self.options.limits.max_field_bytes,
                part,
            )?;
        }
        if changed {
            self.warning(
                CONTROL_DIAGNOSTIC,
                "ANSI escapes or unsafe control characters were removed",
                part,
            )?;
        }
        self.push(Block::Code { language: language.map(str::to_owned), text: sanitized }, part)
    }

    fn add_asset(
        &mut self,
        cell: usize,
        output: Option<usize>,
        attachment: Option<&str>,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<AssetId, ConversionError> {
        let suffix = output.map_or_else(
            || format!("attachment-{}", hex_name(attachment.unwrap_or("asset"))),
            |value| format!("output-{value}"),
        );
        let id = AssetId(format!("notebook-cell-{cell}-{suffix}"));
        let extension = image_extension(media_type)
            .ok_or_else(|| malformed("notebook.assets", "unsupported image media type"))?;
        self.push_asset(Asset {
            id: id.clone(),
            filename: Some(format!("cell-{cell}-{suffix}.{extension}")),
            media_type: media_type.into(),
            bytes,
            external_uri: None,
        })?;
        Ok(id)
    }

    fn push_asset(&mut self, asset: Asset) -> Result<(), ConversionError> {
        if !self.asset_ids.insert(asset.id.0.clone()) {
            return Err(malformed(
                "notebook.assets",
                format!("duplicate generated asset ID {}", asset.id.0),
            ));
        }
        if self.assets.len() >= MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "notebook_assets",
                detail: format!("Notebook exceeds {MAX_DOCUMENT_NODES} assets"),
            });
        }
        let size =
            u64::try_from(asset.bytes.len()).map_err(|_| ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: "Notebook asset size cannot be represented as u64".into(),
            })?;
        self.total_asset_bytes = self.total_asset_bytes.checked_add(size).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: "Notebook total asset bytes overflowed".into(),
            }
        })?;
        if self.total_asset_bytes > self.options.limits.max_total_asset_bytes {
            return Err(ConversionError::ResourceLimit {
                limit: "max_total_asset_bytes",
                detail: format!(
                    "{} > {}",
                    self.total_asset_bytes, self.options.limits.max_total_asset_bytes
                ),
            });
        }
        self.assets.push(asset);
        Ok(())
    }

    fn push(&mut self, block: Block, part: &str) -> Result<(), ConversionError> {
        let (nodes, inlines) = count_block(&block)?;
        self.reserve_ir(nodes, inlines)?;
        if self.document.blocks.len() >= MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "notebook_nodes",
                detail: format!("Notebook exceeds {MAX_DOCUMENT_NODES} top-level IR nodes"),
            });
        }
        self.sequence =
            self.sequence.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "notebook_nodes",
                detail: "Notebook node sequence overflowed".into(),
            })?;
        self.document.blocks.push(BlockNode {
            id: NodeId(format!("notebook-node-{}", self.sequence)),
            block,
            provenance: provenance(part),
        });
        Ok(())
    }

    fn warning(
        &mut self,
        code: &str,
        message: impl Into<String>,
        part: &str,
    ) -> Result<(), ConversionError> {
        if self.diagnostics.len() >= MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "notebook_diagnostics",
                detail: format!("Notebook exceeds {MAX_DOCUMENT_NODES} diagnostics"),
            });
        }
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            locator: Some(SourceLocator { part: Some(part.into()), ..SourceLocator::default() }),
        });
        Ok(())
    }

    fn insert_property(&mut self, key: String, value: String) -> Result<(), ConversionError> {
        if self.document.metadata.properties.len() >= MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "notebook_metadata",
                detail: format!("Notebook exceeds {MAX_DOCUMENT_NODES} metadata properties"),
            });
        }
        enforce_field_bytes(value.len(), self.options.limits.max_field_bytes, &key)?;
        self.document.metadata.properties.insert(key, value);
        Ok(())
    }

    fn merge_diagnostics(
        &mut self,
        mut diagnostics: Vec<Diagnostic>,
    ) -> Result<(), ConversionError> {
        let total = self.diagnostics.len().checked_add(diagnostics.len()).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "notebook_diagnostics",
                detail: "Notebook diagnostic count overflowed".into(),
            }
        })?;
        if total > MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "notebook_diagnostics",
                detail: format!("Notebook exceeds {MAX_DOCUMENT_NODES} diagnostics"),
            });
        }
        self.diagnostics.append(&mut diagnostics);
        Ok(())
    }

    fn merge_fragment(&mut self, blocks: Vec<BlockNode>) -> Result<(), ConversionError> {
        let mut nodes = 0_usize;
        let mut inlines = 0_usize;
        for node in &blocks {
            let (next_nodes, next_inlines) = count_block(&node.block)?;
            nodes = nodes.checked_add(next_nodes).ok_or_else(ir_count_overflow)?;
            inlines = inlines.checked_add(next_inlines).ok_or_else(ir_count_overflow)?;
        }
        self.reserve_ir(nodes, inlines)?;
        self.document.blocks.extend(blocks);
        Ok(())
    }

    fn reserve_ir(&mut self, nodes: usize, inlines: usize) -> Result<(), ConversionError> {
        self.node_count = self.node_count.checked_add(nodes).ok_or_else(ir_count_overflow)?;
        self.inline_count = self.inline_count.checked_add(inlines).ok_or_else(ir_count_overflow)?;
        if self.node_count > MAX_DOCUMENT_NODES {
            return Err(ConversionError::ResourceLimit {
                limit: "notebook_nodes",
                detail: format!("Notebook exceeds {MAX_DOCUMENT_NODES} cumulative IR nodes"),
            });
        }
        if self.inline_count > into_markdown_core::MAX_DOCUMENT_INLINES {
            return Err(ConversionError::ResourceLimit {
                limit: "notebook_inlines",
                detail: format!(
                    "Notebook exceeds {} cumulative IR inlines",
                    into_markdown_core::MAX_DOCUMENT_INLINES
                ),
            });
        }
        Ok(())
    }

    fn finish(self) -> Result<ConverterOutput, ConversionError> {
        self.document.validate().map_err(|error| {
            if error.code == IrErrorCode::ResourceLimit {
                ConversionError::ResourceLimit {
                    limit: "notebook_ir",
                    detail: format!("{}: {}", error.path, error.detail),
                }
            } else {
                ConversionError::Internal {
                    detail: format!(
                        "Notebook converter produced invalid IR at {}: {}",
                        error.path, error.detail
                    ),
                }
            }
        })?;
        Ok(ConverterOutput {
            document: self.document,
            diagnostics: self.diagnostics,
            assets: self.assets,
        })
    }
}

fn ir_count_overflow() -> ConversionError {
    ConversionError::ResourceLimit {
        limit: "notebook_ir",
        detail: "Notebook cumulative IR count overflowed".into(),
    }
}

fn count_block(block: &Block) -> Result<(usize, usize), ConversionError> {
    let mut nodes = 1_usize;
    let mut inlines = match block {
        Block::Paragraph(values)
        | Block::Heading { content: values, .. }
        | Block::TimedSegment { content: values, .. } => count_inlines(values)?,
        _ => 0,
    };
    let nested: Vec<&BlockNode> = match block {
        Block::List { items, .. } => {
            nodes = nodes.checked_add(items.len()).ok_or_else(ir_count_overflow)?;
            items.iter().flat_map(|item| item.blocks.iter()).collect()
        }
        Block::Table { rows, .. } => {
            let cells = rows.iter().try_fold(0_usize, |total, row| {
                total.checked_add(row.cells.len()).ok_or_else(ir_count_overflow)
            })?;
            nodes = nodes
                .checked_add(rows.len())
                .and_then(|value| value.checked_add(cells))
                .ok_or_else(ir_count_overflow)?;
            rows.iter()
                .flat_map(|row| row.cells.iter())
                .flat_map(|cell| cell.blocks.iter())
                .collect()
        }
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => blocks.iter().collect(),
        _ => Vec::new(),
    };
    for child in nested {
        let (child_nodes, child_inlines) = count_block(&child.block)?;
        nodes = nodes.checked_add(child_nodes).ok_or_else(ir_count_overflow)?;
        inlines = inlines.checked_add(child_inlines).ok_or_else(ir_count_overflow)?;
    }
    Ok((nodes, inlines))
}

fn count_inlines(values: &[Inline]) -> Result<usize, ConversionError> {
    let mut count = values.len();
    for value in values {
        if let Inline::Link { content, .. } = value {
            count = count.checked_add(count_inlines(content)?).ok_or_else(ir_count_overflow)?;
        }
    }
    Ok(count)
}

fn rewrite_attachment_targets(
    source: &str,
    cell: usize,
    attachments: &Map<String, Value>,
    part: &str,
) -> Result<(String, BTreeSet<String>), ConversionError> {
    if source.contains(ATTACHMENT_PLACEHOLDER_PREFIX) {
        return Err(malformed(
            format!("{part}/attachments"),
            "Markdown source contains the converter's reserved attachment placeholder URI",
        ));
    }
    let mut edits = Vec::new();
    let mut referenced = BTreeSet::new();
    for (event, span) in Parser::new(source).into_offset_iter() {
        let (target, image) = match event {
            Event::Start(Tag::Image { dest_url, .. }) => (dest_url, true),
            Event::Start(Tag::Link { dest_url, .. }) => (dest_url, false),
            _ => continue,
        };
        let Some(name) = target.strip_prefix("attachment:") else { continue };
        if !image {
            return Err(malformed(
                format!("{part}/attachments"),
                format!(
                    "attachment link {name:?} cannot be represented by the current IR; use an image target"
                ),
            ));
        }
        validate_attachment_name(name, part)?;
        if !attachments.contains_key(name) {
            return Err(malformed(
                format!("{part}/attachments"),
                format!("Markdown references missing attachment {name:?}"),
            ));
        }
        let target_span = exact_inline_destination_span(source, span, target.as_ref()).ok_or_else(|| {
            malformed(
                format!("{part}/attachments"),
                format!(
                    "attachment reference {name:?} must have one unambiguous inline destination token"
                ),
            )
        })?;
        edits.push((
            target_span,
            format!("{ATTACHMENT_PLACEHOLDER_PREFIX}{cell}/{}", hex_name(name)),
        ));
        referenced.insert(name.to_owned());
    }
    edits.sort_by_key(|(span, _)| span.start);
    for pair in edits.windows(2) {
        if pair[0].0.end > pair[1].0.start {
            return Err(malformed(
                format!("{part}/attachments"),
                "overlapping attachment URI targets",
            ));
        }
    }
    let capacity = edits.iter().try_fold(source.len(), |total, (span, replacement)| {
        total
            .checked_sub(span.len())
            .and_then(|value| value.checked_add(replacement.len()))
            .ok_or_else(|| ConversionError::ResourceLimit {
                limit: "max_memory_bytes",
                detail: "rewritten attachment target size overflowed".into(),
            })
    })?;
    let mut rewritten = String::new();
    rewritten.try_reserve(capacity).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: "could not reserve rewritten Markdown attachment targets".into(),
    })?;
    let mut cursor = 0;
    for (span, replacement) in edits {
        rewritten.push_str(&source[cursor..span.start]);
        rewritten.push_str(&replacement);
        cursor = span.end;
    }
    rewritten.push_str(&source[cursor..]);
    Ok((rewritten, referenced))
}

fn exact_inline_destination_span(
    source: &str,
    image_span: std::ops::Range<usize>,
    expected: &str,
) -> Option<std::ops::Range<usize>> {
    let markup = source.get(image_span.clone())?;
    let bytes = markup.as_bytes();
    if !bytes.starts_with(b"![") {
        return None;
    }
    // Accept only the simple inline-label grammar we can locate without guessing. Escaped bytes
    // are skipped; nested labels and code spans fail closed instead of risking an alt-text edit.
    let mut close = 2_usize;
    loop {
        match *bytes.get(close)? {
            b'\\' => close = close.checked_add(2)?,
            b'[' | b'`' => return None,
            b']' => break,
            _ => close += 1,
        }
    }
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let mut start = close + 2;
    while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    let (token_start, token_end) = if bytes.get(start) == Some(&b'<') {
        let token_start = start + 1;
        let mut end = token_start;
        while end < bytes.len() && bytes[end] != b'>' {
            if bytes[end] == b'\\' {
                end = end.checked_add(2)?;
            } else {
                end += 1;
            }
        }
        (token_start, (end < bytes.len()).then_some(end)?)
    } else {
        let token_start = start;
        let mut end = start;
        let mut nested = 0_usize;
        while end < bytes.len() {
            match bytes[end] {
                b'\\' => end = end.checked_add(2)?,
                b'(' => {
                    nested = nested.checked_add(1)?;
                    end += 1;
                }
                b')' if nested > 0 => {
                    nested -= 1;
                    end += 1;
                }
                b')' | b' ' | b'\t' | b'\r' | b'\n' => break,
                _ => end += 1,
            }
        }
        (token_start, end)
    };
    (markup.get(token_start..token_end) == Some(expected)).then_some(
        image_span.start.checked_add(token_start)?..image_span.start.checked_add(token_end)?,
    )
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    part: &str,
) -> Result<&'a Map<String, Value>, ConversionError> {
    object
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(part, format!("{key} must be an object")))
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    part: &str,
) -> Result<&'a str, ConversionError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| malformed(part, format!("{key} must be a string")))
}

fn required_u64(
    object: &Map<String, Value>,
    key: &str,
    part: &str,
) -> Result<u64, ConversionError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed(part, format!("{key} must be a non-negative integer")))
}

fn source_text(
    value: Option<&Value>,
    part: &str,
    max_field_bytes: u64,
) -> Result<String, ConversionError> {
    match value {
        Some(Value::String(value)) => {
            enforce_field_bytes(value.len(), max_field_bytes, part)?;
            Ok(value.clone())
        }
        Some(Value::Array(lines)) => {
            let mut output = String::new();
            for line in lines {
                let line = line
                    .as_str()
                    .ok_or_else(|| malformed(part, "source arrays may contain only strings"))?;
                let next = output.len().checked_add(line.len()).ok_or_else(|| {
                    ConversionError::ResourceLimit {
                        limit: "max_field_bytes",
                        detail: format!("aggregate string-array field in {part} overflowed"),
                    }
                })?;
                enforce_field_bytes(next, max_field_bytes, part)?;
                output.try_reserve(line.len()).map_err(|_| ConversionError::ResourceLimit {
                    limit: "max_memory_bytes",
                    detail: format!("could not reserve aggregate string-array field in {part}"),
                })?;
                output.push_str(line);
            }
            Ok(output)
        }
        _ => Err(malformed(part, "must be a string or an array of strings")),
    }
}

fn enforce_field_bytes(length: usize, maximum: u64, part: &str) -> Result<(), ConversionError> {
    if u64::try_from(length).unwrap_or(u64::MAX) > maximum {
        return Err(ConversionError::ResourceLimit {
            limit: "max_field_bytes",
            detail: format!("aggregate field in {part} exceeds {maximum} decoded UTF-8 bytes"),
        });
    }
    Ok(())
}

fn canonical_json(value: &Value, max_field_bytes: u64) -> Result<String, ConversionError> {
    let result = serde_json::to_string(value).map_err(|error| ConversionError::Internal {
        detail: format!("metadata serialization failed: {error}"),
    })?;
    enforce_field_bytes(result.len(), max_field_bytes, "notebook metadata")?;
    Ok(result)
}

fn select_image(bundle: &Map<String, Value>) -> Option<(&'static str, &Value)> {
    ["image/png", "image/jpeg", "image/gif", "image/webp"]
        .into_iter()
        .find_map(|media_type| bundle.get(media_type).map(|value| (media_type, value)))
}

fn decode_image(
    value: &Value,
    media_type: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
    part: &str,
) -> Result<Vec<u8>, ConversionError> {
    let encoded = source_text(Some(value), part, options.limits.max_field_bytes)?;
    let payload = if encoded.starts_with("data:") {
        encoded.strip_prefix(&format!("data:{media_type};base64,")).ok_or_else(|| {
            malformed(part, "data URI media type or encoding does not match its MIME bundle key")
        })?
    } else {
        &encoded
    };
    let estimate =
        payload.len().checked_mul(3).and_then(|v| v.checked_div(4)).ok_or_else(|| {
            ConversionError::ResourceLimit {
                limit: "max_asset_bytes",
                detail: "base64 decoded-size estimate overflowed".into(),
            }
        })?;
    if u64::try_from(estimate).unwrap_or(u64::MAX) > options.limits.max_asset_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_asset_bytes",
            detail: format!("encoded image in {part} exceeds the configured asset budget"),
        });
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| malformed(part, "image payload is not canonical base64"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > options.limits.max_asset_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_asset_bytes",
            detail: format!("decoded image in {part} exceeds the configured asset budget"),
        });
    }
    let Some((width, height)) = valid_image_structure(media_type, &bytes) else {
        return Err(malformed(part, format!("decoded bytes do not match {media_type}")));
    };
    let rgba_pixel_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "image_pixels",
            detail: format!("image dimensions in {part} overflow the pixel budget"),
        })?;
    if rgba_pixel_bytes > options.limits.max_decompressed_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "image_pixels",
            detail: format!(
                "{width}x{height} image in {part} exceeds the decompressed-byte budget"
            ),
        });
    }
    validate_decodable_image(&bytes, media_type, (width, height), options, context, part)?;
    Ok(bytes)
}

fn validate_decodable_image(
    bytes: &[u8],
    media_type: &str,
    expected_dimensions: (u32, u32),
    options: &ConversionOptions,
    context: &ExecutionContext,
    part: &str,
) -> Result<(), ConversionError> {
    let format = match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => return Err(malformed(part, "unsupported image decoder format")),
    };
    // Reserve before constructing the third-party decoder. Eight bytes per pixel covers every
    // enabled decoder's widest output; three such buffers plus compressed input and fixed codec
    // state conservatively bound the decode working set. The notebook's live JSON reservation
    // separately remains held for the original encoded field and DOM.
    let maximum_pixel_bytes = u64::from(expected_dimensions.0)
        .checked_mul(u64::from(expected_dimensions.1))
        .and_then(|pixels| pixels.checked_mul(8))
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "image_decode_memory",
            detail: format!("image decode size in {part} overflowed"),
        })?;
    let compressed_bytes =
        u64::try_from(bytes.len()).map_err(|_| ConversionError::ResourceLimit {
            limit: "image_decode_memory",
            detail: format!("compressed image size in {part} cannot be represented"),
        })?;
    let working_set = maximum_pixel_bytes
        .checked_mul(3)
        .and_then(|value| compressed_bytes.checked_mul(2).and_then(|size| value.checked_add(size)))
        .and_then(|value| value.checked_add(256 * 1024))
        .ok_or_else(|| ConversionError::ResourceLimit {
            limit: "image_decode_memory",
            detail: format!("image decode working set in {part} overflowed"),
        })?;
    let _decode_memory = context.reserve_memory(working_set)?;
    context.checkpoint()?;

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = ImageLimits::default();
    limits.max_image_width = Some(expected_dimensions.0);
    limits.max_image_height = Some(expected_dimensions.1);
    limits.max_alloc = Some(working_set);
    if media_type == "image/gif" {
        let mut decoder = GifDecoder::new(Cursor::new(bytes))
            .map_err(|_| malformed(part, "image/gif decoder rejected the image header"))?;
        decoder
            .set_limits(limits)
            .map_err(|_| malformed(part, "image/gif decoder rejected the resource limits"))?;
        if decoder.dimensions() != expected_dimensions {
            return Err(malformed(
                part,
                "image/gif decoder dimensions disagree with the validated container",
            ));
        }
        let mut frames = 0_usize;
        for frame in decoder.into_frames() {
            context.checkpoint()?;
            frame.map_err(|_| malformed(part, "image/gif codec payload is not decodable"))?;
            frames = frames.checked_add(1).ok_or_else(|| ConversionError::ResourceLimit {
                limit: "image_frames",
                detail: format!("GIF frame count in {part} overflowed"),
            })?;
            if frames > MAX_DOCUMENT_NODES {
                return Err(ConversionError::ResourceLimit {
                    limit: "image_frames",
                    detail: format!("GIF in {part} exceeds {MAX_DOCUMENT_NODES} frames"),
                });
            }
        }
        if frames == 0 {
            return Err(malformed(part, "image/gif has no decodable frames"));
        }
        context.checkpoint()?;
        return Ok(());
    }
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|_| malformed(part, format!("{media_type} decoder rejected the image header")))?;
    if decoder.dimensions() != expected_dimensions {
        return Err(malformed(
            part,
            format!("{media_type} decoder dimensions disagree with the validated container"),
        ));
    }
    let decoded_bytes = decoder.total_bytes();
    if decoded_bytes > options.limits.max_decompressed_bytes {
        return Err(ConversionError::ResourceLimit {
            limit: "max_decompressed_bytes",
            detail: format!("decoded {media_type} pixels in {part} exceed the configured budget"),
        });
    }
    let decoded_length =
        usize::try_from(decoded_bytes).map_err(|_| ConversionError::ResourceLimit {
            limit: "max_decompressed_bytes",
            detail: format!("decoded {media_type} size in {part} cannot be represented"),
        })?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(decoded_length).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("could not reserve decoded {media_type} pixels in {part}"),
    })?;
    pixels.resize(decoded_length, 0);
    decoder
        .read_image(&mut pixels)
        .map_err(|_| malformed(part, format!("{media_type} codec payload is not decodable")))?;
    context.checkpoint()?;
    Ok(())
}

fn valid_image_structure(media_type: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    match media_type {
        "image/png" => valid_png(bytes),
        "image/jpeg" => valid_jpeg(bytes),
        "image/gif" => valid_gif(bytes),
        "image/webp" => valid_webp(bytes),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)] // Keeping the RIFF chunk state machine together is safer.
fn valid_webp(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 20
        || !bytes.starts_with(b"RIFF")
        || bytes.get(8..12) != Some(b"WEBP")
        || usize::try_from(little_u32(bytes, 4)?).ok()? != bytes.len() - 8
    {
        return None;
    }
    let mut offset = 12_usize;
    let mut extended_dimensions = None;
    let mut codec_dimensions = None;
    let mut extended_flags = None;
    let mut lossless_alpha = false;
    let mut codec_is_lossless = false;
    let mut saw_vp8x = false;
    let mut saw_iccp = false;
    let mut saw_alpha = false;
    let mut saw_exif = false;
    let mut saw_xmp = false;
    while offset < bytes.len() {
        let kind = bytes.get(offset..offset + 4)?;
        let length = usize::try_from(little_u32(bytes, offset + 4)?).ok()?;
        let start = offset.checked_add(8)?;
        let end = start.checked_add(length)?;
        let data = bytes.get(start..end)?;
        match kind {
            b"VP8X" => {
                if saw_vp8x
                    || offset != 12
                    || length != 10
                    || data[0] & 0xc3 != 0
                    || data[1..4] != [0, 0, 0]
                {
                    return None;
                }
                // Animated WebP requires validating nested ANMF bitstreams, which is deliberately
                // outside this bounded still-image parser.
                if data[0] & 0x02 != 0 {
                    return None;
                }
                saw_vp8x = true;
                extended_flags = Some(data[0]);
                extended_dimensions = Some((
                    1 + u32::from_le_bytes([data[4], data[5], data[6], 0]),
                    1 + u32::from_le_bytes([data[7], data[8], data[9], 0]),
                ));
            }
            b"VP8L" => {
                if codec_dimensions.is_some()
                    || saw_alpha
                    || length <= 5
                    || data[0] != 0x2f
                    || !saw_vp8x && offset != 12
                {
                    return None;
                }
                let bits = u32::from_le_bytes(data[1..5].try_into().ok()?);
                if bits >> 29 != 0 {
                    return None;
                }
                codec_is_lossless = true;
                lossless_alpha = bits & (1 << 28) != 0;
                codec_dimensions = Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1));
            }
            b"VP8 " => {
                if codec_dimensions.is_some()
                    || length <= 10
                    || data[0] & 1 != 0
                    || data.get(3..6) != Some(&[0x9d, 0x01, 0x2a])
                    || !saw_vp8x && offset != 12
                {
                    return None;
                }
                let width = u32::from(little_u16(data, 6)? & 0x3fff);
                let height = u32::from(little_u16(data, 8)? & 0x3fff);
                if width == 0 || height == 0 {
                    return None;
                }
                codec_dimensions = Some((width, height));
            }
            b"ICCP" => {
                if !saw_vp8x || saw_iccp || codec_dimensions.is_some() || data.is_empty() {
                    return None;
                }
                saw_iccp = true;
            }
            b"ALPH" => {
                if !saw_vp8x || saw_alpha || codec_dimensions.is_some() || data.is_empty() {
                    return None;
                }
                saw_alpha = true;
            }
            b"EXIF" => {
                if !saw_vp8x || saw_exif || codec_dimensions.is_none() || data.is_empty() {
                    return None;
                }
                saw_exif = true;
            }
            b"XMP " => {
                if !saw_vp8x || saw_xmp || codec_dimensions.is_none() || data.is_empty() {
                    return None;
                }
                saw_xmp = true;
            }
            _ => return None,
        }
        let padded_end = end.checked_add(length & 1)?;
        if length & 1 != 0 && *bytes.get(end)? != 0 {
            return None;
        }
        offset = padded_end;
    }
    let dimensions = codec_dimensions?;
    if extended_dimensions.is_some_and(|extended| extended != dimensions) {
        return None;
    }
    if let Some(flags) = extended_flags
        && ((flags & 0x20 != 0) != saw_iccp
            || (flags & 0x08 != 0) != saw_exif
            || (flags & 0x04 != 0) != saw_xmp
            || (flags & 0x10 != 0) != (saw_alpha || codec_is_lossless && lossless_alpha)
            || saw_alpha && codec_is_lossless)
    {
        return None;
    }
    (offset == bytes.len()).then_some(dimensions)
}

fn valid_gif(bytes: &[u8]) -> Option<(u32, u32)> {
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) || bytes.len() < 14 {
        return None;
    }
    let width = u32::from(little_u16(bytes, 6)?);
    let height = u32::from(little_u16(bytes, 8)?);
    if width == 0 || height == 0 {
        return None;
    }
    let packed = bytes[10];
    let mut offset = 13_usize;
    if packed & 0x80 != 0 {
        offset = offset.checked_add(3_usize.checked_mul(1 << (usize::from(packed & 7) + 1))?)?;
    }
    let mut saw_image = false;
    loop {
        match *bytes.get(offset)? {
            0x3b => return (saw_image && offset + 1 == bytes.len()).then_some((width, height)),
            0x21 => {
                offset = offset.checked_add(2)?;
                offset = skip_gif_sub_blocks(bytes, offset)?.0;
            }
            0x2c => {
                let descriptor = bytes.get(offset..offset + 10)?;
                let left = u32::from(little_u16(descriptor, 1)?);
                let top = u32::from(little_u16(descriptor, 3)?);
                let image_width = u32::from(little_u16(descriptor, 5)?);
                let image_height = u32::from(little_u16(descriptor, 7)?);
                if image_width == 0
                    || image_height == 0
                    || left.checked_add(image_width)? > width
                    || top.checked_add(image_height)? > height
                {
                    return None;
                }
                offset += 10;
                if descriptor[9] & 0x80 != 0 {
                    offset = offset.checked_add(
                        3_usize.checked_mul(1 << (usize::from(descriptor[9] & 7) + 1))?,
                    )?;
                }
                let code_size = *bytes.get(offset)?;
                if !(2..=8).contains(&code_size) {
                    return None;
                }
                let (next, compressed_bytes) = skip_gif_sub_blocks(bytes, offset + 1)?;
                if compressed_bytes == 0 {
                    return None;
                }
                offset = next;
                saw_image = true;
            }
            _ => return None,
        }
    }
}

fn skip_gif_sub_blocks(bytes: &[u8], mut offset: usize) -> Option<(usize, usize)> {
    let mut payload = 0_usize;
    loop {
        let length = usize::from(*bytes.get(offset)?);
        offset = offset.checked_add(1)?;
        if length == 0 {
            return Some((offset, payload));
        }
        payload = payload.checked_add(length)?;
        offset = offset.checked_add(length)?;
        bytes.get(offset - 1)?;
    }
}

fn valid_png(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut offset = 8_usize;
    let mut dimensions = None;
    let mut color_type = None;
    let mut saw_data = false;
    let mut left_data = false;
    let mut saw_palette = false;
    loop {
        let length = usize::try_from(big_u32(bytes, offset)?).ok()?;
        let kind = bytes.get(offset + 4..offset + 8)?;
        let data_start = offset.checked_add(8)?;
        let data_end = data_start.checked_add(length)?;
        let crc_end = data_end.checked_add(4)?;
        if png_crc(&bytes[offset + 4..data_end]) != big_u32(bytes, data_end)? {
            return None;
        }
        if kind.len() != 4
            || !kind.iter().all(u8::is_ascii_alphabetic)
            || kind[2].is_ascii_lowercase()
        {
            return None;
        }
        if dimensions.is_none() {
            if kind != b"IHDR" || length != 13 {
                return None;
            }
            let width = big_u32(bytes, data_start)?;
            let height = big_u32(bytes, data_start + 4)?;
            if width == 0 || height == 0 {
                return None;
            }
            let bit_depth = bytes[data_start + 8];
            let kind = bytes[data_start + 9];
            let valid_depth = match kind {
                0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
                2 | 4 | 6 => matches!(bit_depth, 8 | 16),
                3 => matches!(bit_depth, 1 | 2 | 4 | 8),
                _ => false,
            };
            if !valid_depth
                || bytes[data_start + 10] != 0
                || bytes[data_start + 11] != 0
                || !matches!(bytes[data_start + 12], 0 | 1)
            {
                return None;
            }
            dimensions = Some((width, height));
            color_type = Some(kind);
        } else if kind == b"IHDR" {
            return None;
        } else if kind == b"PLTE" {
            if saw_palette
                || saw_data
                || length == 0
                || length > 768
                || !length.is_multiple_of(3)
                || matches!(color_type, Some(0 | 4))
            {
                return None;
            }
            saw_palette = true;
        } else if kind == b"IDAT" {
            if length == 0 || left_data || color_type == Some(3) && !saw_palette {
                return None;
            }
            saw_data = true;
        } else if kind == b"IEND" {
            return (length == 0 && saw_data && crc_end == bytes.len()).then_some(dimensions?);
        } else {
            if kind[0].is_ascii_uppercase() {
                return None;
            }
            left_data |= saw_data;
        }
        offset = crc_end;
    }
}

fn png_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320_u32 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[allow(clippy::too_many_lines)] // Keeping the JPEG marker/scan state machine together is safer.
fn valid_jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return None;
    }
    let mut offset = 2;
    let mut dimensions = None;
    let mut components = [false; 256];
    let mut component_count = 0_usize;
    let mut saw_quantization = false;
    let mut saw_huffman = false;
    let mut saw_scan = false;
    let mut saw_restart_interval = false;
    while offset + 2 <= bytes.len() {
        if bytes.get(offset) != Some(&0xff) {
            return None;
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = bytes.get(offset).copied()?;
        offset += 1;
        if marker == 0xd9 {
            return (saw_scan && offset == bytes.len()).then_some(dimensions?);
        }
        if marker == 0xd8 || marker == 0x01 || matches!(marker, 0xd0..=0xd7) {
            return None;
        }
        let length = big_u16(bytes, offset).map(usize::from)?;
        if length < 2 || offset.checked_add(length).is_none_or(|end| end > bytes.len()) {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let count = usize::from(*bytes.get(offset + 7)?);
            let height = u32::from(big_u16(bytes, offset + 3)?);
            let width = u32::from(big_u16(bytes, offset + 5)?);
            if saw_scan
                || count == 0
                || length != 8_usize.checked_add(3_usize.checked_mul(count)?)?
                || *bytes.get(offset + 2)? == 0
                || width == 0
                || height == 0
                || dimensions.replace((width, height)).is_some()
            {
                return None;
            }
            component_count = count;
            for component in bytes.get(offset + 8..offset + 8 + 3 * count)?.chunks_exact(3) {
                let id = usize::from(component[0]);
                if components[id] {
                    return None;
                }
                components[id] = true;
            }
        } else if marker == 0xdb {
            if length < 67 || saw_scan {
                return None;
            }
            saw_quantization = true;
        } else if marker == 0xc4 {
            if length < 19 {
                return None;
            }
            saw_huffman = true;
        } else if marker == 0xdd {
            if length != 4 || saw_restart_interval {
                return None;
            }
            saw_restart_interval = true;
        }
        if marker == 0xda {
            let count = usize::from(*bytes.get(offset + 2)?);
            if dimensions.is_none()
                || !saw_quantization
                || !saw_huffman
                || count == 0
                || count > component_count
                || length != 6_usize.checked_add(2_usize.checked_mul(count)?)?
            {
                return None;
            }
            let mut scan_components = [false; 256];
            for component in bytes.get(offset + 3..offset + 3 + 2 * count)?.chunks_exact(2) {
                let id = usize::from(component[0]);
                if !components[id] || scan_components[id] {
                    return None;
                }
                scan_components[id] = true;
            }
            let mut scan = offset.checked_add(length)?;
            let mut entropy_bytes = 0_usize;
            while scan + 1 < bytes.len() {
                if bytes[scan] != 0xff {
                    entropy_bytes = entropy_bytes.checked_add(1)?;
                    scan += 1;
                    continue;
                }
                let next = bytes[scan + 1];
                if next == 0x00 {
                    entropy_bytes = entropy_bytes.checked_add(1)?;
                    scan += 2;
                    continue;
                }
                if matches!(next, 0xd0..=0xd7) {
                    scan += 2;
                    continue;
                }
                if entropy_bytes == 0 {
                    return None;
                }
                saw_scan = true;
                offset = scan;
                break;
            }
            if offset != scan {
                return None;
            }
            continue;
        }
        offset += length;
    }
    None
}

fn little_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

fn little_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn big_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

fn big_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn image_extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn validate_attachment_name(name: &str, part: &str) -> Result<(), ConversionError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(malformed(
            format!("{part}.attachments"),
            format!("unsafe attachment name {name:?}"),
        ));
    }
    Ok(())
}

fn hex_name(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len().saturating_mul(2));
    for byte in value.bytes() {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn sanitize_text_bounded(
    value: &str,
    maximum: u64,
    part: &str,
) -> Result<(String, bool), ConversionError> {
    let mut output = String::new();
    let changed = append_sanitized_bounded(&mut output, value, maximum, part)?;
    Ok((output, changed))
}

fn append_sanitized_bounded(
    output: &mut String,
    value: &str,
    maximum: u64,
    part: &str,
) -> Result<bool, ConversionError> {
    let (additional, changed) = sanitized_length(value)?;
    let final_length =
        output.len().checked_add(additional).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_field_bytes",
            detail: format!("sanitized field in {part} overflowed"),
        })?;
    enforce_field_bytes(final_length, maximum, part)?;
    output.try_reserve(additional).map_err(|_| ConversionError::ResourceLimit {
        limit: "max_memory_bytes",
        detail: format!("could not reserve sanitized field in {part}"),
    })?;
    append_sanitized(output, value);
    Ok(changed)
}

fn sanitized_length(value: &str) -> Result<(usize, bool), ConversionError> {
    let mut length = 0_usize;
    let mut changed = false;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            changed = true;
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        let bytes = if ch == '\n' || ch == '\r' || ch == '\t' || !ch.is_control() {
            ch.len_utf8()
        } else {
            changed = true;
            '\u{fffd}'.len_utf8()
        };
        length = length.checked_add(bytes).ok_or_else(|| ConversionError::ResourceLimit {
            limit: "max_field_bytes",
            detail: "sanitized UTF-8 length overflowed".into(),
        })?;
    }
    Ok((length, changed))
}

fn append_sanitized(output: &mut String, value: &str) {
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        output.push(if ch == '\n' || ch == '\r' || ch == '\t' || !ch.is_control() {
            ch
        } else {
            '\u{fffd}'
        });
    }
}

fn sanitize_language(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '_' | '.'))
        .take(64)
        .collect()
}

fn text_inline(value: String) -> Vec<Inline> {
    vec![Inline::Text { value, marks: Vec::new() }]
}

fn provenance(part: &str) -> Provenance {
    Provenance {
        kind: ProvenanceKind::NativeParser,
        provider: PROVIDER_ID.into(),
        locator: SourceLocator { part: Some(part.into()), ..SourceLocator::default() },
        confidence: Some(1.0),
    }
}

fn input_metadata_for_nested() -> into_markdown_core::SourceMetadata {
    into_markdown_core::SourceMetadata::default()
}

fn prefix_and_remap(node: &mut BlockNode, prefix: &str, replacements: &BTreeMap<String, AssetId>) {
    node.id.0 = format!("{prefix}-{}", node.id.0);
    node.provenance.provider = PROVIDER_ID.into();
    node.provenance.locator.part = Some(prefix.into());
    match &mut node.block {
        Block::Paragraph(content)
        | Block::Heading { content, .. }
        | Block::TimedSegment { content, .. } => prefix_footnotes(content, prefix),
        Block::Image { asset, .. } => {
            // The Markdown converter uses the generated HTTPS placeholder as external_uri;
            // remapping by ID happens after its asset inventory is inspected.
            let _ = replacements;
            let _ = asset;
        }
        Block::List { items, .. } => items
            .iter_mut()
            .flat_map(|item| &mut item.blocks)
            .for_each(|child| prefix_and_remap(child, prefix, replacements)),
        Block::Table { rows, .. } => rows
            .iter_mut()
            .flat_map(|row| &mut row.cells)
            .flat_map(|cell| &mut cell.blocks)
            .for_each(|child| prefix_and_remap(child, prefix, replacements)),
        Block::Footnote { label, blocks } => {
            *label = format!("{prefix}-{label}");
            for child in blocks {
                prefix_and_remap(child, prefix, replacements);
            }
        }
        Block::Page { blocks, .. } | Block::Slide { blocks, .. } | Block::Sheet { blocks, .. } => {
            for child in blocks {
                prefix_and_remap(child, prefix, replacements);
            }
        }
        _ => {}
    }
}

fn prefix_footnotes(inlines: &mut [Inline], prefix: &str) {
    for inline in inlines {
        match inline {
            Inline::FootnoteReference(label) => *label = format!("{prefix}-{label}"),
            Inline::Link { content, .. } => prefix_footnotes(content, prefix),
            _ => {}
        }
    }
}

fn remap_asset_references(nodes: &mut [BlockNode], old: &AssetId, new: &AssetId) {
    for node in nodes {
        match &mut node.block {
            Block::Image { asset, .. } if asset == old => asset.clone_from(new),
            Block::List { items, .. } => {
                for item in items {
                    remap_asset_references(&mut item.blocks, old, new);
                }
            }
            Block::Table { rows, .. } => rows
                .iter_mut()
                .flat_map(|row| &mut row.cells)
                .for_each(|cell| remap_asset_references(&mut cell.blocks, old, new)),
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => remap_asset_references(blocks, old, new),
            _ => {}
        }
    }
}

fn blocks_contain_attachment_placeholder(nodes: &[BlockNode]) -> bool {
    nodes.iter().any(|node| match &node.block {
        Block::Paragraph(inlines)
        | Block::Heading { content: inlines, .. }
        | Block::TimedSegment { content: inlines, .. } => {
            inlines_contain_attachment_placeholder(inlines)
        }
        Block::List { items, .. } => {
            items.iter().any(|item| blocks_contain_attachment_placeholder(&item.blocks))
        }
        Block::Table { rows, .. } => rows.iter().any(|row| {
            row.cells.iter().any(|cell| blocks_contain_attachment_placeholder(&cell.blocks))
        }),
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => blocks_contain_attachment_placeholder(blocks),
        _ => false,
    })
}

fn inlines_contain_attachment_placeholder(inlines: &[Inline]) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::Link { target, content } => {
            target.starts_with(ATTACHMENT_PLACEHOLDER_PREFIX)
                || inlines_contain_attachment_placeholder(content)
        }
        _ => false,
    })
}

fn blocks_reference_asset(nodes: &[BlockNode], expected: &AssetId) -> bool {
    nodes.iter().any(|node| match &node.block {
        Block::Image { asset, .. } => asset == expected,
        Block::List { items, .. } => {
            items.iter().any(|item| blocks_reference_asset(&item.blocks, expected))
        }
        Block::Table { rows, .. } => rows
            .iter()
            .any(|row| row.cells.iter().any(|cell| blocks_reference_asset(&cell.blocks, expected))),
        Block::Footnote { blocks, .. }
        | Block::Page { blocks, .. }
        | Block::Slide { blocks, .. }
        | Block::Sheet { blocks, .. } => blocks_reference_asset(blocks, expected),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ErrorCode, ExecutionOptions};
    use std::sync::Arc;

    fn input(bytes: &[u8]) -> ResolvedInput {
        ResolvedInput {
            bytes: Arc::from(bytes),
            metadata: into_markdown_core::SourceMetadata::default(),
        }
    }

    fn context(options: &ConversionOptions) -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), options.limits.clone())
    }

    #[test]
    fn fixture_preserves_cells_outputs_and_safe_representations() {
        let bytes = include_bytes!("../tests/fixtures/notebook/complete.ipynb");
        let options = ConversionOptions::default();
        let output = convert_notebook(&input(bytes), &options, &context(&options)).unwrap();
        let rendered =
            into_markdown_render_markdown::render(&output.document, &output.assets, &options)
                .unwrap();
        assert!(rendered.contains("# Notebook fixture"));
        assert!(rendered.contains(r"### Code cell \[7\]"));
        assert!(rendered.contains("```python\nprint(\"NEVER_EXECUTE\")"));
        assert!(rendered.contains("stdout"));
        assert!(rendered.contains("ValueError: bad"));
        assert!(rendered.contains("```html\n<script>NEVER_EXECUTE</script>"));
        assert!(rendered.find("stdout").unwrap() < rendered.find("ValueError: bad").unwrap());
        assert!(output.diagnostics.iter().any(|d| d.code == HTML_DIAGNOSTIC));
        assert_eq!(output.assets.len(), 2);
        assert_eq!(
            output.document.metadata.properties["jupyter.cell.1.output.3.executionCount"],
            "7"
        );
        assert_eq!(output.document.metadata.properties["jupyter.cell.1.output.4.displayId"], "x");
    }

    #[test]
    fn duplicate_keys_and_forged_images_fail_closed() {
        let options = ConversionOptions::default();
        let duplicate =
            br#"{"nbformat":4,"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[]}"#;
        assert_eq!(
            convert_notebook(&input(duplicate), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::Malformed
        );
        let forged = include_bytes!("../tests/fixtures/notebook/forged-image.ipynb");
        assert_eq!(
            convert_notebook(&input(forged), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::Malformed
        );
    }

    #[test]
    fn unsafe_attachment_names_and_asset_limits_fail_closed() {
        let options = ConversionOptions::default();
        let unsafe_name = include_bytes!("../tests/fixtures/notebook/unsafe-attachment.ipynb");
        assert_eq!(
            convert_notebook(&input(unsafe_name), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::Malformed
        );
        let mut limited = options.clone();
        limited.limits.max_asset_bytes = 1;
        let complete = include_bytes!("../tests/fixtures/notebook/complete.ipynb");
        assert_eq!(
            convert_notebook(&input(complete), &limited, &context(&limited)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn nesting_and_field_budgets_fail_closed() {
        let mut options = ConversionOptions::default();
        options.limits.max_nesting_depth = 4;
        let deep = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{"a":{"b":{"c":{"d":{}}}}},"cells":[]}"#;
        assert_eq!(
            convert_notebook(&input(deep), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );

        let mut options = ConversionOptions::default();
        options.limits.max_field_bytes = 3;
        let wide = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"cell_type":"raw","metadata":{},"source":"1234"}]}"#;
        assert_eq!(
            convert_notebook(&input(wide), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn nbformat_45_requires_valid_unique_cell_ids_and_raw_attachments_survive() {
        let options = ConversionOptions::default();
        for cells in [
            serde_json::json!([{"cell_type":"raw","metadata":{},"source":"x"}]),
            serde_json::json!([{"id":"bad id","cell_type":"raw","metadata":{},"source":"x"}]),
            serde_json::json!([
                {"id":"same","cell_type":"raw","metadata":{},"source":"x"},
                {"id":"same","cell_type":"raw","metadata":{},"source":"y"}
            ]),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":cells
            }))
            .unwrap();
            assert_eq!(
                convert_notebook(&input(&bytes), &options, &context(&options)).unwrap_err().code(),
                ErrorCode::Malformed
            );
        }

        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let bytes = serde_json::to_vec(&serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{
                "id":"raw","cell_type":"raw","metadata":{},"source":"raw",
                "attachments":{"raw.png":{"image/png":png}}
            }]
        }))
        .unwrap();
        let output = convert_notebook(&input(&bytes), &options, &context(&options)).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert!(matches!(output.document.blocks.last().unwrap().block, Block::Image { .. }));
    }

    #[test]
    fn attachment_binding_is_ast_exact_and_ignores_unreferenced_payloads() {
        let options = ConversionOptions::default();
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let bytes = serde_json::to_vec(&serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{
                "id":"markdown","cell_type":"markdown","metadata":{},
                "source":"ordinary attachment:a and `attachment:a`\n\n![x](attachment:ab)",
                "attachments":{"a":{"image/png":"not base64"},"ab":{"image/png":png}}
            }]
        }))
        .unwrap();
        let output = convert_notebook(&input(&bytes), &options, &context(&options)).unwrap();
        let rendered =
            into_markdown_render_markdown::render(&output.document, &output.assets, &options)
                .unwrap();
        assert!(rendered.contains("ordinary attachment:a"));
        assert!(rendered.contains("`attachment:a`"));
        assert_eq!(output.assets.len(), 1);
        assert!(
            output
                .diagnostics
                .iter()
                .any(|value| value.code == "notebook.unreferencedAttachmentIgnored")
        );

        let missing = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"m","cell_type":"markdown","metadata":{},"source":"![x](attachment:missing)","attachments":{}}]}"#;
        assert_eq!(
            convert_notebook(&input(missing), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::Malformed
        );

        let mut attachment_map = Map::new();
        attachment_map.insert("a".into(), serde_json::json!({"image/png":png}));
        let source = "before\n\n![attachment:a](attachment:a \"attachment:a\")\n\n![second](attachment:a)\n\nafter";
        let (rewritten, references) =
            rewrite_attachment_targets(source, 0, &attachment_map, "cells/0").unwrap();
        assert_eq!(references, BTreeSet::from(["a".into()]));
        assert_eq!(rewritten.matches(ATTACHMENT_PLACEHOLDER_PREFIX).count(), 2);
        assert!(rewritten.contains("![attachment:a](https://attachment.invalid/0/61"));
        assert!(rewritten.contains("\"attachment:a\")"));

        let bytes = serde_json::to_vec(&serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{
                "id":"exact","cell_type":"markdown","metadata":{},"source":source,
                "attachments":{"a":{"image/png":png}}
            }]
        }))
        .unwrap();
        let output = convert_notebook(&input(&bytes), &options, &context(&options)).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert!(
            !into_markdown_render_markdown::render(&output.document, &output.assets, &options)
                .unwrap()
                .contains(ATTACHMENT_PLACEHOLDER_PREFIX)
        );

        for source in [
            "prefix ![x](attachment:a)",
            "![x](attachment:a) suffix",
            "https://attachment.invalid/user-content",
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{
                    "id":"inline","cell_type":"markdown","metadata":{},"source":source,
                    "attachments":{"a":{"image/png":png}}
                }]
            }))
            .unwrap();
            assert_eq!(
                convert_notebook(&input(&bytes), &options, &context(&options)).unwrap_err().code(),
                ErrorCode::Malformed
            );
        }
    }

    #[test]
    fn combined_and_sanitized_fields_enforce_final_utf8_limit() {
        let make_error = |traceback: &str| {
            serde_json::to_vec(&serde_json::json!({
                "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{
                    "id":"error","cell_type":"code","metadata":{},"execution_count":null,
                    "source":"x","outputs":[{
                        "output_type":"error","ename":"Error","evalue":"value",
                        "traceback":traceback
                    }]
                }]
            }))
            .unwrap()
        };
        let mut options = ConversionOptions::default();
        options.limits.max_field_bytes = 20;
        let exact = make_error("1234567");
        convert_notebook(&input(&exact), &options, &context(&options)).unwrap();
        let oversized = make_error("12345678");
        assert_eq!(
            convert_notebook(&input(&oversized), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );

        let make_raw = |controls: usize| {
            serde_json::to_vec(&serde_json::json!({
                "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{
                    "id":"raw","cell_type":"raw","metadata":{},
                    "source":"\u{1}".repeat(controls)
                }]
            }))
            .unwrap()
        };
        convert_notebook(&input(&make_raw(6)), &options, &context(&options)).unwrap();
        assert_eq!(
            convert_notebook(&input(&make_raw(7)), &options, &context(&options))
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn aggregate_fields_and_live_memory_are_bounded() {
        let mut options = ConversionOptions::default();
        options.limits.max_field_bytes = 3;
        let bytes = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"r","cell_type":"raw","metadata":{},"source":["ab","cd"]}]}"#;
        assert_eq!(
            convert_notebook(&input(bytes), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );

        let bytes = serde_json::to_vec(&serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{"many":vec![0; 2_000]},"cells":[]
        }))
        .unwrap();
        let mut options = ConversionOptions::default();
        options.limits.max_memory_bytes = u64::try_from(bytes.len() * 64 + 64 * 1024 - 1).unwrap();
        assert_eq!(
            convert_notebook(&input(&bytes), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );

        let bytes = include_bytes!("../tests/fixtures/notebook/complete.ipynb");
        let mut options = ConversionOptions::default();
        options.limits.max_memory_bytes = u64::try_from(bytes.len() * 64 + 64 * 1024 - 1).unwrap();
        assert_eq!(
            convert_notebook(&input(bytes), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn cumulative_nested_ir_is_rejected_before_merge() {
        let source = "x\n\n".repeat(200);
        let cells = (0..501)
            .map(|index| {
                serde_json::json!({
                    "id":format!("c{index}"),"cell_type":"markdown","metadata":{},"source":source
                })
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":cells
        }))
        .unwrap();
        let options = ConversionOptions::default();
        assert_eq!(
            convert_notebook(&input(&bytes), &options, &context(&options)).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn truncated_images_and_pixel_bombs_fail_closed() {
        let options = ConversionOptions::default();
        let mut png = base64::engine::general_purpose::STANDARD.decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
        ).unwrap();
        png.truncate(png.len() - 4);
        assert!(valid_image_structure("image/png", &png).is_none());

        let mut bomb = base64::engine::general_purpose::STANDARD.decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
        ).unwrap();
        bomb[16..20].copy_from_slice(&100_000_u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&100_000_u32.to_be_bytes());
        let crc = png_crc(&bomb[12..29]);
        bomb[29..33].copy_from_slice(&crc.to_be_bytes());
        let encoded = base64::engine::general_purpose::STANDARD.encode(bomb);
        let mut limited = options.clone();
        limited.limits.max_decompressed_bytes = 1024;
        assert_eq!(
            decode_image(
                &Value::String(encoded),
                "image/png",
                &limited,
                &context(&limited),
                "bomb",
            )
            .unwrap_err()
            .code(),
            ErrorCode::ResourceLimit
        );

        assert!(valid_image_structure("image/gif", b"GIF89a\x01\0\x01\0\0\0\0").is_none());
        assert!(valid_image_structure("image/webp", b"RIFF\x0c\0\0\0WEBPVP8X").is_none());
        assert!(valid_image_structure("image/jpeg", b"\xff\xd8\xff\xd9").is_none());
    }

    #[test]
    fn real_images_decode_and_malformed_codecs_fail_closed() {
        let options = ConversionOptions::default();
        let fixtures = include_bytes!("../tests/fixtures/notebook/valid-images.ipynb");
        let output = convert_notebook(&input(fixtures), &options, &context(&options)).unwrap();
        assert_eq!(output.assets.len(), 4);
        assert_eq!(
            output.assets.iter().map(|asset| asset.media_type.as_str()).collect::<Vec<_>>(),
            ["image/png", "image/gif", "image/webp", "image/jpeg"]
        );

        let mut empty_png = b"\x89PNG\r\n\x1a\n".to_vec();
        append_png_chunk(&mut empty_png, *b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        append_png_chunk(&mut empty_png, *b"IDAT", &[]);
        append_png_chunk(&mut empty_png, *b"IEND", &[]);
        assert!(valid_image_structure("image/png", &empty_png).is_none());

        let png =
            &output.assets.iter().find(|asset| asset.media_type == "image/png").unwrap().bytes;
        let ihdr_end = 8 + 4 + 4 + 13 + 4;
        let mut duplicate_ihdr = png[..ihdr_end].to_vec();
        duplicate_ihdr.extend_from_slice(&png[8..ihdr_end]);
        duplicate_ihdr.extend_from_slice(&png[ihdr_end..]);
        assert!(valid_image_structure("image/png", &duplicate_ihdr).is_none());
        let mut corrupt_png = png.clone();
        let idat = corrupt_png.windows(4).position(|value| value == b"IDAT").unwrap();
        let idat_length = usize::try_from(big_u32(&corrupt_png, idat - 4).unwrap()).unwrap();
        corrupt_png[idat + 4] = 0;
        let crc_offset = idat + 4 + idat_length;
        let crc = png_crc(&corrupt_png[idat..crc_offset]);
        corrupt_png[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_be_bytes());
        assert_codec_rejected("image/png", &corrupt_png, &options);

        let empty_gif = b"GIF89a\x01\0\x01\0\0\0\0\x2c\0\0\0\0\x01\0\x01\0\0\x02\0\x3b";
        assert!(valid_image_structure("image/gif", empty_gif).is_none());
        let outside_canvas =
            b"GIF89a\x01\0\x01\0\0\0\0\x2c\x01\0\0\0\x01\0\x01\0\0\x02\x01\x44\0\x3b";
        assert!(valid_image_structure("image/gif", outside_canvas).is_none());
        let gif =
            &output.assets.iter().find(|asset| asset.media_type == "image/gif").unwrap().bytes;
        let mut corrupt_gif = gif.clone();
        let descriptor = corrupt_gif.iter().position(|byte| *byte == 0x2c).unwrap();
        let data_length = usize::from(corrupt_gif[descriptor + 11]);
        corrupt_gif[descriptor + 12..descriptor + 12 + data_length].fill(0xff);
        assert_codec_rejected("image/gif", &corrupt_gif, &options);
        let mut corrupt_second_frame = gif[..gif.len() - 1].to_vec();
        corrupt_second_frame.extend_from_slice(b"\x2c\0\0\0\0\x01\0\x01\0\0\x02\x02\xff\xff\0\x3b");
        assert_codec_rejected("image/gif", &corrupt_second_frame, &options);

        let mut vp8x_only = b"RIFF\x16\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
        vp8x_only.extend_from_slice(&[0; 10]);
        assert!(valid_image_structure("image/webp", &vp8x_only).is_none());
        let webp =
            &output.assets.iter().find(|asset| asset.media_type == "image/webp").unwrap().bytes;
        let mut mismatched_webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
        mismatched_webp.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        mismatched_webp.extend_from_slice(&webp[12..]);
        let riff_length = u32::try_from(mismatched_webp.len() - 8).unwrap().to_le_bytes();
        mismatched_webp[4..8].copy_from_slice(&riff_length);
        assert!(valid_image_structure("image/webp", &mismatched_webp).is_none());
        let mut corrupt_webp = webp.clone();
        corrupt_webp[30..].fill(0xff);
        assert_codec_rejected("image/webp", &corrupt_webp, &options);

        let jpeg =
            &output.assets.iter().find(|asset| asset.media_type == "image/jpeg").unwrap().bytes;
        let sos = jpeg.windows(2).position(|value| value == [0xff, 0xda]).unwrap();
        let scan = sos + 2 + usize::from(big_u16(jpeg, sos + 2).unwrap());
        let mut empty_jpeg = jpeg[..scan].to_vec();
        empty_jpeg.extend_from_slice(&[0xff, 0xd9]);
        assert!(valid_image_structure("image/jpeg", &empty_jpeg).is_none());
        let mut corrupt_jpeg = jpeg.clone();
        let dht = corrupt_jpeg.windows(2).position(|value| value == [0xff, 0xc4]).unwrap();
        corrupt_jpeg[dht + 5] = 0xff;
        assert_codec_rejected("image/jpeg", &corrupt_jpeg, &options);
        let sof_length = 2 + usize::from(big_u16(jpeg, 4).unwrap());
        let mut duplicate_sof = jpeg[..2 + sof_length].to_vec();
        duplicate_sof.extend_from_slice(&jpeg[2..2 + sof_length]);
        duplicate_sof.extend_from_slice(&jpeg[2 + sof_length..]);
        assert!(valid_image_structure("image/jpeg", &duplicate_sof).is_none());
    }

    fn append_png_chunk(output: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        output.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        output.extend_from_slice(&kind);
        output.extend_from_slice(data);
        let start = output.len() - data.len() - kind.len();
        output.extend_from_slice(&png_crc(&output[start..]).to_be_bytes());
    }

    fn assert_codec_rejected(media_type: &str, bytes: &[u8], options: &ConversionOptions) {
        assert!(
            valid_image_structure(media_type, bytes).is_some(),
            "corrupt fixture must reach the full codec decoder"
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert_eq!(
            decode_image(
                &Value::String(encoded),
                media_type,
                options,
                &context(options),
                "corrupt",
            )
            .unwrap_err()
            .code(),
            ErrorCode::Malformed
        );
    }

    #[test]
    fn repeated_display_updates_preserve_order_and_association() {
        let bytes = br#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"code","cell_type":"code","metadata":{},"execution_count":2,"source":"x","outputs":[{"output_type":"execute_result","execution_count":2,"metadata":{},"transient":{"display_id":"same"},"data":{"text/plain":"first"}},{"output_type":"update_display_data","metadata":{},"transient":{"display_id":"same"},"data":{"text/plain":"second"}},{"output_type":"update_display_data","metadata":{},"transient":{"display_id":"same"},"data":{"text/plain":"third"}}]}]}"#;
        let options = ConversionOptions::default();
        let output = convert_notebook(&input(bytes), &options, &context(&options)).unwrap();
        assert_eq!(
            output.document.metadata.properties["jupyter.cell.0.output.0.executionCount"],
            "2"
        );
        for index in 0..3 {
            assert_eq!(
                output.document.metadata.properties
                    [&format!("jupyter.cell.0.output.{index}.displayId")],
                "same"
            );
        }
        let rendered =
            into_markdown_render_markdown::render(&output.document, &output.assets, &options)
                .unwrap();
        assert!(rendered.find("first").unwrap() < rendered.find("second").unwrap());
        assert!(rendered.find("second").unwrap() < rendered.find("third").unwrap());
    }

    #[test]
    fn parse_render_is_stable() {
        let bytes = include_bytes!("../tests/fixtures/notebook/complete.ipynb");
        let options = ConversionOptions::default();
        let first = convert_notebook(&input(bytes), &options, &context(&options)).unwrap();
        let second = convert_notebook(&input(bytes), &options, &context(&options)).unwrap();
        let first_markdown =
            into_markdown_render_markdown::render(&first.document, &first.assets, &options)
                .unwrap();
        let second_markdown =
            into_markdown_render_markdown::render(&second.document, &second.assets, &options)
                .unwrap();
        assert_eq!(first_markdown, second_markdown);
        assert_eq!(first.document, second.document);
        assert!(first_markdown.contains("```html\n<script>NEVER_EXECUTE</script>\n```"));
    }
}
