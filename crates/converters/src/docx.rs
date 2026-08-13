//! Bounded, offline `WordprocessingML` (`.docx`/`.docm`) conversion.

use image::{ImageDecoder as _, Limits as ImageLimits, codecs::jpeg::JpegDecoder};
use into_markdown_core::{
    Asset, AssetId, Block, BlockNode, BoxFuture, Cell, ConversionError, ConversionOptions,
    Converter, ConverterOutput, Diagnostic, DiagnosticSeverity, Document, ExecutionContext,
    FormatCandidate, Inline, InlineMark, InputFormat, ListItem, ListKind, MAX_DOCUMENT_INLINES,
    MAX_DOCUMENT_NODES, MAX_TABLE_COLUMNS, NodeId, ProbeOutcome, Provenance, ProvenanceKind,
    ResolvedInput, Services, SourceLocator, TableAlignment, TableRow,
};
use quick_xml::events::{BytesCData, BytesRef, BytesStart, BytesText, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use quick_xml::reader::Reader;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Cursor, Read};
use std::path::{Component, Path};

const FORMATS: &[InputFormat] = &[InputFormat::Docx];
const PROVIDER_ID: &str = "builtin.converter.docx";
const XML_EVENT_FACTOR: u64 = 8;
const WORD_NS: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const OFFICE_REL_NS: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_REL_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";
const CONTENT_TYPES_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/content-types";
const MATH_NS: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/math";
const MC_NS: &[u8] = b"http://schemas.openxmlformats.org/markup-compatibility/2006";
const DRAWING_NS: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const WORD_DRAWING_NS: &[u8] =
    b"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
const VML_NS: &[u8] = b"urn:schemas-microsoft-com:vml";
const CORE_PROPERTIES_NS: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/metadata/core-properties";
const DUBLIN_CORE_NS: &[u8] = b"http://purl.org/dc/elements/1.1/";
const DUBLIN_CORE_TERMS_NS: &[u8] = b"http://purl.org/dc/terms/";
const OFFICE_REL_TYPE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
const REL_TYPE_PREFIX: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/";
const MAX_IMAGE_DIMENSION: u32 = 32_768;
const MAX_IMAGE_PIXELS: u64 = 100_000_000;

/// Strict, non-networking Word Open XML converter. Macro parts are never opened.
#[derive(Debug, Default)]
pub struct DocxConverter;

impl Converter for DocxConverter {
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
        input: &'a ResolvedInput,
        candidate: &'a FormatCandidate,
        context: &'a ExecutionContext,
    ) -> BoxFuture<'a, Result<ProbeOutcome, ConversionError>> {
        Box::pin(async move {
            context.checkpoint()?;
            if candidate.format != InputFormat::Docx {
                return Ok(ProbeOutcome::NotApplicable);
            }
            let zip = input.bytes.starts_with(b"PK\x03\x04")
                || input.bytes.starts_with(b"PK\x05\x06")
                || input.bytes.starts_with(b"PK\x07\x08");
            Ok(if candidate.explicit || candidate.detector_id == "builtin.detector.hints" || zip {
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
        Box::pin(async move { convert_docx(&input.bytes, options, context) })
    }
}

#[derive(Debug)]
struct Package {
    parts: BTreeMap<String, Vec<u8>>,
    content_types: ContentTypes,
    macro_present: bool,
    _memory: into_markdown_core::ResourceReservation,
}

impl Package {
    #[allow(clippy::too_many_lines)]
    fn open(
        bytes: &[u8],
        options: &ConversionOptions,
        context: &ExecutionContext,
    ) -> Result<Self, ConversionError> {
        if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
            // Password-protected OOXML is wrapped in an OLE Compound File containing
            // EncryptionInfo and EncryptedPackage streams. It is never passed to an OLE reader.
            return Err(ConversionError::Encrypted);
        }
        let input_size = u64::try_from(bytes.len())
            .map_err(|_| limit("max_input_bytes", "DOCX size overflow"))?;
        if input_size > options.limits.max_input_bytes {
            return Err(limit(
                "max_input_bytes",
                format!("{input_size} > {}", options.limits.max_input_bytes),
            ));
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|error| malformed(None, format!("invalid ZIP container: {error}")))?;
        let count = u32::try_from(archive.len()).unwrap_or(u32::MAX);
        if count > options.limits.max_archive_entries {
            return Err(limit(
                "max_archive_entries",
                format!("{count} > {}", options.limits.max_archive_entries),
            ));
        }
        let metadata_bytes = u64::try_from(archive.len())
            .unwrap_or(u64::MAX)
            .checked_mul(
                u64::try_from(std::mem::size_of::<(usize, String, u64, bool)>() + 64)
                    .unwrap_or(u64::MAX),
            )
            .ok_or_else(|| limit("max_memory_bytes", "ZIP metadata budget overflow"))?;
        let mut memory = context.reserve_memory(metadata_bytes)?;
        let mut names = BTreeSet::new();
        let mut metadata = Vec::with_capacity(archive.len());
        let mut total = 0_u64;
        for index in 0..archive.len() {
            context.checkpoint()?;
            let entry = archive.by_index_raw(index).map_err(|error| {
                malformed(None, format!("cannot inspect ZIP entry {index}: {error}"))
            })?;
            if entry.encrypted() {
                return Err(ConversionError::Encrypted);
            }
            if entry.is_dir() {
                continue;
            }
            let name = canonical_part_name(entry.name())?;
            let name_bytes = u64::try_from(name.len()).unwrap_or(u64::MAX).saturating_mul(2);
            memory.grow(name_bytes)?;
            if !names.insert(name.clone()) {
                return Err(malformed(Some(&name), "duplicate ZIP part name"));
            }
            total = total
                .checked_add(entry.size())
                .ok_or_else(|| limit("max_decompressed_bytes", "DOCX expanded size overflow"))?;
            if total > options.limits.max_decompressed_bytes {
                return Err(limit(
                    "max_decompressed_bytes",
                    format!("{total} > {}", options.limits.max_decompressed_bytes),
                ));
            }
            metadata.push((index, name, entry.size()));
        }

        let content_index = metadata
            .iter()
            .find(|(_, name, _)| name == "[Content_Types].xml")
            .map(|(index, _, _)| *index)
            .ok_or_else(|| {
                malformed(Some("[Content_Types].xml"), "required package part is missing")
            })?;
        let mut parts = BTreeMap::new();
        let content_types_bytes =
            read_zip_entry(&mut archive, content_index, "[Content_Types].xml", &mut memory)?;
        let content_types = parse_content_types(&content_types_bytes, options, context)?;
        parts.insert("[Content_Types].xml".into(), content_types_bytes);

        let mut excluded = metadata
            .iter()
            .filter(|(_, name, _)| content_types.is_macro_part(name))
            .map(|(_, name, _)| name.clone())
            .collect::<BTreeSet<_>>();

        // Walk reachable relationship parts from the package root before unrelated metadata.
        // A relationship can therefore classify an arbitrarily renamed VBA target (including a
        // misleading `.rels` suffix) before that target is ever opened or decompressed.
        let mut pending_relationships = metadata
            .iter()
            .filter(|(_, name, _)| {
                name != "[Content_Types].xml"
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("rels"))
            })
            .map(|(index, name, _)| (name.clone(), *index))
            .collect::<BTreeMap<_, _>>();
        let mut relationship_queue = VecDeque::from(["_rels/.rels".to_owned()]);
        while !pending_relationships.is_empty() {
            let name = relationship_queue.pop_front().or_else(|| {
                pending_relationships.keys().find(|name| !excluded.contains(*name)).cloned()
            });
            let Some(name) = name else {
                break;
            };
            let Some(index) = pending_relationships.remove(&name) else {
                continue;
            };
            if excluded.contains(&name) {
                continue;
            }
            let relationship_bytes = read_zip_entry(&mut archive, index, &name, &mut memory)?;
            let owner = relationship_owner(&name)?;
            let relationships =
                parse_relationships(Some(&relationship_bytes), &owner, options, context)?;
            for relationship in relationships.values().filter(|value| !value.external) {
                let target = resolve_target(&owner, &relationship.target)?;
                if is_macro_relationship_type(&relationship.kind) {
                    excluded.insert(target);
                } else {
                    let target_relationships = relationship_part(&target);
                    if pending_relationships.contains_key(&target_relationships) {
                        relationship_queue.push_back(target_relationships);
                    }
                }
            }
            parts.insert(name, relationship_bytes);
        }

        let macro_present = !excluded.is_empty() || content_types.macro_enabled_main();
        for (index, name, _declared) in metadata {
            context.checkpoint()?;
            if parts.contains_key(&name) || excluded.contains(&name) {
                continue;
            }
            let value = read_zip_entry(&mut archive, index, &name, &mut memory)?;
            parts.insert(name, value);
        }
        Ok(Self { parts, content_types, macro_present, _memory: memory })
    }

    fn required(&self, name: &str) -> Result<&[u8], ConversionError> {
        self.parts
            .get(name)
            .map(Vec::as_slice)
            .ok_or_else(|| malformed(Some(name), "required package part is missing"))
    }

    fn take_required(&mut self, name: &str) -> Result<Vec<u8>, ConversionError> {
        self.parts
            .remove(name)
            .ok_or_else(|| malformed(Some(name), "required package part is missing"))
    }
}

fn read_zip_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    index: usize,
    name: &str,
    memory: &mut into_markdown_core::ResourceReservation,
) -> Result<Vec<u8>, ConversionError> {
    let mut entry = archive
        .by_index(index)
        .map_err(|error| malformed(Some(name), format!("cannot open ZIP part: {error}")))?;
    if entry.encrypted() {
        return Err(ConversionError::Encrypted);
    }
    let declared = entry.size();
    memory.grow(declared)?;
    let cap = usize::try_from(declared)
        .map_err(|_| limit("max_decompressed_bytes", format!("part {name} is too large")))?;
    let mut value = Vec::new();
    value.try_reserve_exact(cap).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve part {name}: {error}"))
    })?;
    entry
        .by_ref()
        .take(declared.saturating_add(1))
        .read_to_end(&mut value)
        .map_err(|error| malformed(Some(name), format!("cannot decompress part: {error}")))?;
    if u64::try_from(value.len()).unwrap_or(u64::MAX) != declared {
        return Err(malformed(Some(name), "decompressed size differs from ZIP directory"));
    }
    Ok(value)
}

#[derive(Debug, Clone, Default)]
struct Relationship {
    target: String,
    external: bool,
    kind: String,
}

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

fn parse_content_types(
    bytes: &[u8],
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<ContentTypes, ConversionError> {
    preflight_xml(bytes, "[Content_Types].xml", XmlProfile::ContentTypes, options, context)?;
    let mut reader = Reader::from_reader(bytes);
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

fn is_macro_content_type(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("vbaproject")
        || value.contains("vbadata")
        || value.contains("activex")
        || value.contains("macroenabled.template")
}

fn is_macro_relationship_type(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.ends_with("/vbaproject") || value.ends_with("/vbadata") || value.contains("/activex")
}

#[derive(Debug, Clone)]
struct Numbering {
    kind: ListKind,
    start: u64,
    label: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupportedImage {
    Png,
    Jpeg,
}

impl SupportedImage {
    fn media_type(self) -> &'static str {
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
        self.diagnostics.push(Diagnostic {
            code: code.into(),
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            locator: Some(SourceLocator { part: Some(part.into()), ..SourceLocator::default() }),
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

    let comments = referenced_annotations(
        &package,
        &relationships,
        "comments",
        &main_part,
        &state.comment_refs,
        XmlProfile::Comments,
        options,
        context,
    )?;
    for (id, content) in comments {
        state.add_inlines(1)?;
        let title = state.node(
            Block::Heading {
                level: 6,
                content: vec![Inline::Text { value: format!("Comment {id}"), marks: Vec::new() }],
            },
            "comments",
        )?;
        state.document.blocks.push(title);
        state.add_inlines(content.len())?;
        let node = state.node(Block::Paragraph(content), "comments")?;
        state.document.blocks.push(node);
    }
    let mut notes = referenced_annotations(
        &package,
        &relationships,
        "footnotes",
        &main_part,
        &state.footnote_refs,
        XmlProfile::Footnotes,
        options,
        context,
    )?;
    notes.extend(referenced_annotations(
        &package,
        &relationships,
        "endnotes",
        &main_part,
        &state.endnote_refs,
        XmlProfile::Endnotes,
        options,
        context,
    )?);
    for (id, content) in notes {
        state.add_inlines(content.len())?;
        let paragraph = state.node(Block::Paragraph(content), "notes")?;
        let node = state.node(Block::Footnote { label: id, blocks: vec![paragraph] }, "notes")?;
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
    Ok(ConverterOutput {
        document: state.document,
        assets: state.assets,
        diagnostics: state.diagnostics,
    })
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
    package: &Package,
    relationships: &BTreeMap<String, Relationship>,
    relation_suffix: &str,
    owner: &str,
    references: &BTreeSet<String>,
    profile: XmlProfile,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<(String, Vec<Inline>)>, ConversionError> {
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
    let parsed =
        parse_annotations(Some(package.required(&part)?), &part, profile, options, context)?;
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
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut current = None::<String>;
    let mut value = String::new();
    loop {
        context.checkpoint()?;
        match reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?
        {
            Event::Start(e) => {
                let name = local(e.name().as_ref()).to_owned();
                if matches!(name.as_str(), "title" | "creator" | "lastModifiedBy") {
                    current = Some(name);
                    value.clear();
                }
            }
            Event::Text(e) if current.is_some() => {
                append_bounded_text(&mut value, &decode_text(&e, part)?, options)?;
            }
            Event::CData(e) if current.is_some() => {
                append_bounded_text(&mut value, &decode_cdata(&e, part)?, options)?;
            }
            Event::GeneralRef(e) if current.is_some() => {
                append_bounded_text(&mut value, &decode_reference(&e, part)?, options)?;
            }
            Event::End(e)
                if current.as_deref().is_some_and(|name| name == local(e.name().as_ref())) =>
            {
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
    let mut reader = Reader::from_reader(bytes);
    let mut result = BTreeMap::new();
    loop {
        context.checkpoint()?;
        match reader
            .read_event()
            .map_err(|error| malformed(Some(&part), format!("invalid XML: {error}")))?
        {
            Event::Empty(e) | Event::Start(e) if local(e.name().as_ref()) == "Relationship" => {
                let id = attr(&e, b"Id", &part)?
                    .ok_or_else(|| malformed(Some(&part), "relationship lacks Id"))?;
                let target = attr(&e, b"Target", &part)?
                    .ok_or_else(|| malformed(Some(&part), "relationship lacks Target"))?;
                let kind = attr(&e, b"Type", &part)?
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| malformed(Some(&part), "relationship lacks Type"))?;
                let target_mode = attr(&e, b"TargetMode", &part)?;
                let external =
                    target_mode.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("external"));
                if target_mode.is_some() && !external {
                    return Err(malformed(Some(&part), "unsupported relationship TargetMode"));
                }
                if !external {
                    resolve_target(owner, &target)?;
                }
                if result.insert(id, Relationship { target, external, kind }).is_some() {
                    return Err(malformed(Some(&part), "duplicate relationship Id"));
                }
            }
            Event::DocType(_) => return Err(malformed(Some(&part), "DOCTYPE is forbidden")),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(result)
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

fn parse_annotations(
    bytes: Option<&[u8]>,
    part: &str,
    profile: XmlProfile,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<Vec<(String, Vec<Inline>)>, ConversionError> {
    let Some(bytes) = bytes else {
        return Ok(Vec::new());
    };
    preflight_xml(bytes, part, profile, options, context)?;
    let mut reader = Reader::from_reader(bytes);
    let mut result = Vec::new();
    let mut current = None::<(String, Vec<Inline>)>;
    let mut stack = Vec::<String>::new();
    loop {
        context.checkpoint()?;
        match reader
            .read_event()
            .map_err(|error| malformed(Some(part), format!("invalid XML: {error}")))?
        {
            Event::Start(e)
                if matches!(local(e.name().as_ref()), "comment" | "footnote" | "endnote") =>
            {
                stack.push(local(e.name().as_ref()).to_owned());
                current = Some((
                    attr_local(&e, "id", part)?.unwrap_or_else(|| result.len().to_string()),
                    Vec::new(),
                ));
            }
            Event::Start(e) => stack.push(local(e.name().as_ref()).to_owned()),
            Event::Text(e) if current.is_some() && stack.last().is_some_and(|name| name == "t") => {
                append_annotation_text(
                    &mut current.as_mut().expect("guarded above").1,
                    decode_text(&e, part)?,
                    options,
                )?;
            }
            Event::CData(e)
                if current.is_some() && stack.last().is_some_and(|name| name == "t") =>
            {
                append_annotation_text(
                    &mut current.as_mut().expect("guarded above").1,
                    decode_cdata(&e, part)?,
                    options,
                )?;
            }
            Event::GeneralRef(e)
                if current.is_some() && stack.last().is_some_and(|name| name == "t") =>
            {
                append_annotation_text(
                    &mut current.as_mut().expect("guarded above").1,
                    decode_reference(&e, part)?,
                    options,
                )?;
            }
            Event::Empty(e) if local(e.name().as_ref()) == "tab" && current.is_some() => current
                .as_mut()
                .unwrap()
                .1
                .push(Inline::Text { value: "\t".into(), marks: Vec::new() }),
            Event::Empty(e)
                if matches!(local(e.name().as_ref()), "br" | "cr") && current.is_some() =>
            {
                current.as_mut().unwrap().1.push(Inline::LineBreak);
            }
            Event::End(e)
                if matches!(local(e.name().as_ref()), "comment" | "footnote" | "endnote") =>
            {
                if let Some(value) = current.take() {
                    result.push(value);
                }
                stack.pop();
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(result)
}

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
    cells: Vec<Cell>,
    cell_blocks: Vec<BlockNode>,
    cell_column_span: u32,
    row_header: bool,
    row_open: bool,
    cell_open: bool,
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
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut paragraph = None::<Paragraph>;
    let mut marks = Vec::<InlineMark>::new();
    let mut hyperlink = None::<(String, Vec<Inline>)>;
    let mut table = None::<TableBuild>;
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
                let name = local(e.name().as_ref()).to_owned();
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
                        return Err(malformed(Some(part), "nested paragraphs are unsupported"));
                    }
                    if table.as_ref().is_some_and(|table| !table.cell_open) {
                        return Err(malformed(Some(part), "table paragraph is outside a cell"));
                    }
                    paragraph = Some(Paragraph::default());
                } else if name == "instrText" && !field_active {
                    return Err(malformed(Some(part), "field instruction is outside a field"));
                } else if name == "tbl" {
                    if profile == XmlProfile::Document && body_depth.is_none() {
                        return Err(malformed(Some(part), "table is outside the document body"));
                    }
                    if table.is_some() {
                        return Err(malformed(Some(part), "nested tables are unsupported"));
                    }
                    table = Some(TableBuild::default());
                } else if name == "tr" {
                    if let Some(t) = &mut table {
                        if t.row_open {
                            return Err(malformed(Some(part), "nested table rows are invalid"));
                        }
                        t.cells.clear();
                        t.row_header = false;
                        t.row_open = true;
                    } else {
                        return Err(malformed(Some(part), "table row is outside a table"));
                    }
                } else if name == "tc" {
                    if let Some(t) = &mut table {
                        if !t.row_open || t.cell_open {
                            return Err(malformed(Some(part), "invalid table cell hierarchy"));
                        }
                        t.cell_blocks.clear();
                        t.cell_column_span = 1;
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
                } else if name == "vMerge" {
                    return Err(malformed(Some(part), "vertical table merges are unsupported"));
                } else if matches!(
                    name.as_str(),
                    "headerReference"
                        | "footerReference"
                        | "footnoteReference"
                        | "endnoteReference"
                        | "commentReference"
                        | "fldChar"
                ) {
                    return Err(malformed(
                        Some(part),
                        "reference and field marker elements must be empty",
                    ));
                }
            }
            Event::Empty(e) => {
                let name = local(e.name().as_ref()).to_owned();
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
                        "b" => marks.push(InlineMark::Bold),
                        "i" => marks.push(InlineMark::Italic),
                        "strike" | "dstrike" => marks.push(InlineMark::Strikethrough),
                        "u" => marks.push(InlineMark::Underline),
                        "vertAlign" => match attr_local(&e, "val", part)?.as_deref() {
                            Some("superscript") => marks.push(InlineMark::Superscript),
                            Some("subscript") => marks.push(InlineMark::Subscript),
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
                                if name == "footnoteReference" {
                                    state.footnote_refs.insert(id.clone());
                                } else {
                                    state.endnote_refs.insert(id.clone());
                                }
                                push_inline(p, &mut hyperlink, Inline::FootnoteReference(id));
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
                if let Some(t) = &mut table {
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
                        return Err(malformed(Some(part), "vertical table merges are unsupported"));
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
                let name = local(e.name().as_ref()).to_owned();
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
                        if let Some(table) = &mut table {
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
                        return Err(malformed(Some(part), "field instruction crosses a paragraph"));
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
                            table.as_mut(),
                        )?;
                    }
                } else if name == "tc" {
                    if let Some(t) = &mut table {
                        if !t.cell_open {
                            return Err(malformed(Some(part), "table cell closes without opening"));
                        }
                        t.cells.push(Cell {
                            row_span: 1,
                            column_span: t.cell_column_span,
                            header: t.row_header,
                            blocks: std::mem::take(&mut t.cell_blocks),
                        });
                        t.cell_open = false;
                    }
                } else if name == "tr" {
                    if let Some(t) = &mut table {
                        if !t.row_open || t.cell_open {
                            return Err(malformed(
                                Some(part),
                                "table row closes with an open cell",
                            ));
                        }
                        t.rows.push(TableRow { cells: std::mem::take(&mut t.cells) });
                        t.row_open = false;
                    }
                } else if name == "tbl" {
                    if let Some(t) = table.take() {
                        if t.row_open || t.cell_open {
                            return Err(malformed(Some(part), "table closes with incomplete rows"));
                        }
                        validate_table_limits(&t.rows, part, options)?;
                        let node = state.node(
                            Block::Table { rows: t.rows, alignments: Vec::<TableAlignment>::new() },
                            part,
                        )?;
                        state.document.blocks.push(node);
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
        || table.is_some()
        || depth != 0
        || !element_stack.is_empty()
        || skipped_choice_depth.is_some()
    {
        return Err(malformed(Some(part), "truncated WordprocessingML structure"));
    }
    Ok(())
}

fn validate_table_limits(
    rows: &[TableRow],
    part: &str,
    options: &ConversionOptions,
) -> Result<(), ConversionError> {
    if rows.is_empty() {
        return Err(malformed(Some(part), "table has no rows"));
    }
    let row_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    if row_count > options.limits.max_table_rows {
        return Err(limit(
            "max_table_rows",
            format!("{row_count} > {}", options.limits.max_table_rows),
        ));
    }
    let mut cells = 0_u64;
    let mut expected_width = None;
    for row in rows {
        if row.cells.is_empty() {
            return Err(malformed(Some(part), "table row has no cells"));
        }
        cells = cells
            .checked_add(u64::try_from(row.cells.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| limit("max_table_cells", "DOCX table cell count overflow"))?;
        let width = row.cells.iter().try_fold(0_u64, |total, cell| {
            total
                .checked_add(u64::from(cell.column_span))
                .ok_or_else(|| limit("max_table_columns", "DOCX table width overflow"))
        })?;
        if width > options.limits.max_table_columns || width > MAX_TABLE_COLUMNS as u64 {
            return Err(limit(
                "max_table_columns",
                format!(
                    "{width} > {}",
                    options.limits.max_table_columns.min(MAX_TABLE_COLUMNS as u64)
                ),
            ));
        }
        if expected_width.replace(width).is_some_and(|expected| expected != width) {
            return Err(malformed(Some(part), "table rows have inconsistent widths"));
        }
    }
    if cells > options.limits.max_table_cells {
        return Err(limit(
            "max_table_cells",
            format!("{cells} > {}", options.limits.max_table_cells),
        ));
    }
    Ok(())
}

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
    if let Some(Inline::Text { value: previous, marks: previous_marks }) = inlines.last_mut()
        && previous_marks == marks
    {
        append_bounded_text(previous, &value, options)?;
    } else {
        enforce_field_limit(&value, options)?;
        inlines.push(Inline::Text { value, marks: marks.to_vec() });
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
    p: Paragraph,
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
    for (id, alt) in p.images {
        let rel = relationships
            .get(&id)
            .ok_or_else(|| malformed(Some(part), format!("image relationship {id} is missing")))?;
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
            let image = supported_image(&target, declared_type)?;
            let bytes = package
                .parts
                .remove(&target)
                .ok_or_else(|| malformed(Some(&target), "related image part is missing"))?;
            validate_image_bytes(image, &bytes, &target, options, context)?;
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

fn append_related_parts(
    package: &mut Package,
    options: &ConversionOptions,
    context: &ExecutionContext,
    state: &mut ParseState,
) -> Result<(), ConversionError> {
    let mut seen = BTreeSet::new();
    for (part, label) in std::mem::take(&mut state.related_parts) {
        if !seen.insert(part.clone()) {
            continue;
        }
        let profile = if label == "Header" { XmlProfile::Header } else { XmlProfile::Footer };
        let expected_content_type = if label == "Header" {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"
        } else {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"
        };
        if package.content_types.content_type(&part) != Some(expected_content_type) {
            return Err(malformed(
                Some("[Content_Types].xml"),
                format!("{label} relationship targets a part with the wrong content type"),
            ));
        }
        state.add_inlines(1)?;
        let heading = state.node(
            Block::Heading {
                level: 6,
                content: vec![Inline::Text { value: label.into(), marks: Vec::new() }],
            },
            &part,
        )?;
        state.document.blocks.push(heading);
        let rels = parse_relationships(
            package.parts.get(&relationship_part(&part)).map(Vec::as_slice),
            &part,
            options,
            context,
        )?;
        let part_bytes = package.take_required(&part)?;
        parse_word_part(
            &part_bytes,
            &part,
            profile,
            &rels,
            &BTreeMap::new(),
            &BTreeMap::new(),
            package,
            options,
            context,
            state,
        )?;
    }
    Ok(())
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
                validate_xml_element(profile, &name, &stack, part)?;
                validate_xml_attributes(&reader, &element, &name, part)?;
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
                validate_xml_element(profile, &name, &stack, part)?;
                validate_xml_attributes(&reader, &element, &name, part)?;
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
        | b"sectPr" | b"drawing" | b"pict" | b"t" => Some(WORD_NS),
        b"coreProperties" | b"keywords" | b"lastModifiedBy" | b"revision" | b"category"
        | b"contentStatus" | b"version" => Some(CORE_PROPERTIES_NS),
        b"title" | b"subject" | b"creator" | b"description" | b"identifier" | b"language" => {
            Some(DUBLIN_CORE_NS)
        }
        b"created" | b"modified" => Some(DUBLIN_CORE_TERMS_NS),
        _ => None,
    };
    if expected_namespace.is_some_and(|expected| ns != expected) {
        return Err(malformed(
            Some(part),
            format!(
                "interpreted element {} has an unexpected namespace",
                String::from_utf8_lossy(local)
            ),
        ));
    }
    if matches!(
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
    ) && depth != 1
    {
        return Err(malformed(Some(part), "package part root appears at a nested level"));
    }
    if local == b"body" && (profile != XmlProfile::Document || !parent_is(WORD_NS, b"document")) {
        return Err(malformed(Some(part), "w:body is only valid in the main document"));
    }
    match profile {
        XmlProfile::ContentTypes if depth > 1 && !parent_is(CONTENT_TYPES_NS, b"Types") => {
            return Err(malformed(Some(part), "content type declarations must be direct children"));
        }
        XmlProfile::Relationships
            if depth > 1
                && !(local == b"Relationship" && parent_is(PACKAGE_REL_NS, b"Relationships")) =>
        {
            return Err(malformed(Some(part), "relationships must be direct children"));
        }
        XmlProfile::Styles => validate_styles_hierarchy(ns, local, parent, ancestors, part)?,
        XmlProfile::Numbering => validate_numbering_hierarchy(ns, local, parent, part)?,
        XmlProfile::Comments
            if matches!(local, b"comment" | b"footnote" | b"endnote")
                && !(local == b"comment" && parent_is(WORD_NS, b"comments")) =>
        {
            return Err(malformed(Some(part), "invalid annotation definition for comments part"));
        }
        XmlProfile::Footnotes
            if matches!(local, b"comment" | b"footnote" | b"endnote")
                && !(local == b"footnote" && parent_is(WORD_NS, b"footnotes")) =>
        {
            return Err(malformed(Some(part), "invalid annotation definition for footnotes part"));
        }
        XmlProfile::Endnotes
            if matches!(local, b"comment" | b"footnote" | b"endnote")
                && !(local == b"endnote" && parent_is(WORD_NS, b"endnotes")) =>
        {
            return Err(malformed(Some(part), "invalid annotation definition for endnotes part"));
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
    if is_word_content_profile(profile) {
        if matches!(local, b"comment" | b"footnote" | b"endnote")
            && !matches!(
                profile,
                XmlProfile::Comments | XmlProfile::Footnotes | XmlProfile::Endnotes
            )
        {
            return Err(malformed(Some(part), "annotation definition is invalid for this part"));
        }
        validate_word_content_hierarchy(ns, local, parent, ancestors, part)?;
    } else if is_word_content_semantic(ns, local)
        && !matches!(profile, XmlProfile::Styles | XmlProfile::Numbering)
    {
        return Err(malformed(Some(part), "Word content element is invalid for this part profile"));
    }
    if matches!(local, b"Choice" | b"Fallback") && !raw_parent_is(MC_NS, b"AlternateContent") {
        return Err(malformed(Some(part), "MC branches must be direct AlternateContent children"));
    }
    if ns == MC_NS
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
            && matches!(name.1.as_slice(), b"AlternateContent" | b"Choice" | b"Fallback"))
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
        b"name" | b"basedOn" | b"pPr" | b"rPr" => xml_name_is(parent, WORD_NS, b"style"),
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
        Err(malformed(Some(part), "invalid styles semantic element hierarchy"))
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
        Err(malformed(Some(part), "invalid numbering semantic element hierarchy"))
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
        ),
        (WORD_NS, b"pPr" | b"hyperlink") => parent_word_is(b"p"),
        (WORD_NS, b"pStyle" | b"numPr") => parent_word_is(b"pPr"),
        (WORD_NS, b"numId" | b"ilvl") => parent_word_is(b"numPr"),
        (WORD_NS, b"r") => parent_word_is(b"p") || parent_word_is(b"hyperlink"),
        (WORD_NS, b"b" | b"i" | b"strike" | b"dstrike" | b"u" | b"vertAlign") => {
            parent_word_is(b"rPr")
        }
        (
            WORD_NS,
            b"rPr" | b"drawing" | b"pict" | b"t" | b"tab" | b"br" | b"cr" | b"fldChar"
            | b"instrText" | b"footnoteReference" | b"endnoteReference" | b"commentReference",
        ) => parent_word_is(b"r"),
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
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(malformed(Some(part), "invalid Word semantic element hierarchy"))
    }
}

fn validate_xml_attributes(
    reader: &NsReader<&[u8]>,
    element: &BytesStart<'_>,
    element_name: &(Vec<u8>, Vec<u8>),
    part: &str,
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
            (WORD_NS, b"hyperlink" | b"headerReference" | b"footerReference", b"id")
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
        if expected.is_some_and(|expected| namespace.as_slice() != expected) {
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

fn canonical_part_name(name: &str) -> Result<String, ConversionError> {
    if name.is_empty() || name.contains('\\') || name.contains('\0') || name.starts_with('/') {
        return Err(malformed(None, "unsafe ZIP part name"));
    }
    if name.split('/').any(|part| part.is_empty() || matches!(part, "." | "..")) {
        return Err(malformed(Some(name), "unsafe ZIP part path"));
    }
    let path = Path::new(name);
    if path.components().any(|value| !matches!(value, Component::Normal(_))) {
        return Err(malformed(Some(name), "unsafe ZIP part path"));
    }
    Ok(name.to_owned())
}

fn resolve_target(owner: &str, target: &str) -> Result<String, ConversionError> {
    if target.is_empty()
        || target.contains('\\')
        || target.contains('\0')
        || target.starts_with('/')
        || target.contains(':')
    {
        return Err(malformed(Some(owner), "unsafe internal relationship target"));
    }
    let mut segments = owner
        .rsplit_once('/')
        .map_or(Vec::new(), |(dir, _)| dir.split('/').map(str::to_owned).collect());
    for value in target.split('/') {
        match value {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(malformed(Some(owner), "relationship escapes package root"));
                }
            }
            other => segments.push(other.to_owned()),
        }
    }
    if segments.is_empty() {
        return Err(malformed(Some(owner), "relationship target is empty"));
    }
    Ok(segments.join("/"))
}

fn relationship_part(owner: &str) -> String {
    let (dir, file) = owner.rsplit_once('/').unwrap_or(("", owner));
    if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") }
}

fn relationship_owner(part: &str) -> Result<String, ConversionError> {
    if part == "_rels/.rels" {
        return Ok(String::new());
    }
    let (directory, filename) = part
        .rsplit_once('/')
        .ok_or_else(|| malformed(Some(part), "relationship part has no _rels directory"))?;
    let owner_directory = directory
        .strip_suffix("/_rels")
        .ok_or_else(|| malformed(Some(part), "relationship part is outside _rels"))?;
    let owner_filename = filename
        .strip_suffix(".rels")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed(Some(part), "invalid relationship part name"))?;
    canonical_part_name(&format!("{owner_directory}/{owner_filename}"))
}

fn relationship_type(suffix: &str) -> String {
    format!("{REL_TYPE_PREFIX}{suffix}")
}

fn unique_internal_relationship<'a>(
    relationships: &'a BTreeMap<String, Relationship>,
    kind: &str,
    owner: &str,
) -> Result<Option<(&'a str, &'a Relationship)>, ConversionError> {
    let mut matches = relationships
        .iter()
        .filter(|(_, relationship)| relationship.kind == kind && !relationship.external);
    let first = matches.next().map(|(id, relationship)| (id.as_str(), relationship));
    if matches.next().is_some() {
        return Err(malformed(
            Some(&relationship_part(owner)),
            format!("multiple relationships of type {kind}"),
        ));
    }
    Ok(first)
}

fn decode_text(event: &BytesText<'_>, part: &str) -> Result<String, ConversionError> {
    let value = event
        .decode()
        .map_err(|error| malformed(Some(part), format!("invalid text encoding: {error}")))?
        .into_owned();
    validate_xml_characters(&value, part)?;
    Ok(value)
}

fn decode_cdata(event: &BytesCData<'_>, part: &str) -> Result<String, ConversionError> {
    let value = event
        .decode()
        .map_err(|error| malformed(Some(part), format!("invalid CDATA encoding: {error}")))?
        .into_owned();
    validate_xml_characters(&value, part)?;
    Ok(value)
}

fn decode_reference(event: &BytesRef<'_>, part: &str) -> Result<String, ConversionError> {
    let reference = event
        .decode()
        .map_err(|error| malformed(Some(part), format!("invalid reference encoding: {error}")))?;
    decode_reference_name(&reference, part)
}

fn decode_reference_name(reference: &str, part: &str) -> Result<String, ConversionError> {
    let predefined = match reference {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "apos" => Some("'"),
        "quot" => Some("\""),
        _ => None,
    };
    if let Some(value) = predefined {
        return Ok(value.into());
    }
    let (digits, radix) = if let Some(value) = reference.strip_prefix("#x") {
        (value, 16)
    } else if let Some(value) = reference.strip_prefix('#') {
        (value, 10)
    } else {
        return Err(malformed(Some(part), format!("custom XML entity &{reference}; is forbidden")));
    };
    if digits.is_empty()
        || (radix == 10 && !digits.bytes().all(|value| value.is_ascii_digit()))
        || (radix == 16 && !digits.bytes().all(|value| value.is_ascii_hexdigit()))
    {
        return Err(malformed(Some(part), "invalid numeric character reference"));
    }
    let codepoint = u32::from_str_radix(digits, radix)
        .map_err(|_| malformed(Some(part), "numeric character reference is out of range"))?;
    let character = char::from_u32(codepoint)
        .filter(|value| is_xml_character(*value))
        .ok_or_else(|| malformed(Some(part), "numeric character reference is not legal XML"))?;
    Ok(character.to_string())
}

fn decode_xml_attribute(raw: &[u8], part: &str) -> Result<String, ConversionError> {
    let raw = std::str::from_utf8(raw)
        .map_err(|error| malformed(Some(part), format!("attribute is not UTF-8: {error}")))?;
    let mut decoded = String::with_capacity(raw.len());
    let mut cursor = 0;
    while let Some(relative_start) = raw[cursor..].find('&') {
        let start = cursor + relative_start;
        let literal = &raw[cursor..start];
        validate_xml_characters(literal, part)?;
        decoded.push_str(literal);
        let reference_start = start + 1;
        let end = raw[reference_start..]
            .find(';')
            .map(|relative| reference_start + relative)
            .ok_or_else(|| malformed(Some(part), "unterminated XML attribute reference"))?;
        decoded.push_str(&decode_reference_name(&raw[reference_start..end], part)?);
        cursor = end + 1;
    }
    let remainder = &raw[cursor..];
    validate_xml_characters(remainder, part)?;
    decoded.push_str(remainder);
    Ok(decoded)
}

fn validate_xml_characters(value: &str, part: &str) -> Result<(), ConversionError> {
    if value.chars().all(is_xml_character) {
        Ok(())
    } else {
        Err(malformed(Some(part), "text contains a character forbidden by XML 1.0"))
    }
}

fn is_xml_character(value: char) -> bool {
    matches!(value, '\u{9}' | '\u{a}' | '\u{d}')
        || matches!(value as u32, 0x20..=0xd7ff | 0xe000..=0xfffd | 0x0001_0000..=0x0010_ffff)
}

fn attr(e: &BytesStart<'_>, key: &[u8], part: &str) -> Result<Option<String>, ConversionError> {
    for value in e.attributes() {
        let value = value
            .map_err(|error| malformed(Some(part), format!("invalid XML attribute: {error}")))?;
        if value.key.as_ref() == key {
            return decode_xml_attribute(value.value.as_ref(), part).map(Some);
        }
    }
    Ok(None)
}

fn attr_local(
    e: &BytesStart<'_>,
    key: &str,
    part: &str,
) -> Result<Option<String>, ConversionError> {
    for value in e.attributes() {
        let value = value
            .map_err(|error| malformed(Some(part), format!("invalid XML attribute: {error}")))?;
        if local(value.key.as_ref()) == key {
            return decode_xml_attribute(value.value.as_ref(), part).map(Some);
        }
    }
    Ok(None)
}

fn local(name: &[u8]) -> &str {
    std::str::from_utf8(name.rsplit(|b| *b == b':').next().unwrap_or(name)).unwrap_or("")
}

fn supported_image(
    part: &str,
    declared_content_type: &str,
) -> Result<SupportedImage, ConversionError> {
    let extension = Path::new(part)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| malformed(Some(part), "image part has no safe extension"))?;
    match (declared_content_type, extension.as_str()) {
        ("image/png", "png") => Ok(SupportedImage::Png),
        ("image/jpeg", "jpg" | "jpeg") => Ok(SupportedImage::Jpeg),
        _ => Err(malformed(
            Some("[Content_Types].xml"),
            format!(
                "image target {part} has an unsupported or extension-mismatched content type {declared_content_type}"
            ),
        )),
    }
}

fn validate_image_bytes(
    image: SupportedImage,
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    match image {
        SupportedImage::Png => validate_png(bytes, part, options, context),
        SupportedImage::Jpeg => {
            let dimensions = validate_jpeg(bytes, part)?;
            validate_jpeg_pixels(bytes, dimensions, part, options, context)
        }
    }
}

fn validate_image_dimensions(width: u32, height: u32, part: &str) -> Result<(), ConversionError> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| malformed(Some(part), "image dimensions overflow"))?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || pixels > MAX_IMAGE_PIXELS
    {
        return Err(malformed(Some(part), "image dimensions exceed the safe raster envelope"));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_png(
    bytes: &[u8],
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIGNATURE) {
        return Err(malformed(Some(part), "image/png target lacks the PNG signature"));
    }
    let mut cursor = SIGNATURE.len();
    let mut chunks = 0_u32;
    let mut saw_header = false;
    let mut saw_data = false;
    let mut data_ended = false;
    let mut saw_palette = false;
    let mut layout = None::<(u32, u32, u8, u8)>;
    let mut idat_bytes = 0_u64;
    loop {
        chunks = chunks
            .checked_add(1)
            .ok_or_else(|| malformed(Some(part), "PNG chunk count overflow"))?;
        if chunks > 100_000 {
            return Err(malformed(Some(part), "PNG has too many chunks"));
        }
        let header_end = cursor
            .checked_add(8)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed(Some(part), "truncated PNG chunk header"))?;
        let length = u32::from_be_bytes(
            bytes[cursor..cursor + 4]
                .try_into()
                .map_err(|_| malformed(Some(part), "truncated PNG length"))?,
        ) as usize;
        let kind = &bytes[cursor + 4..header_end];
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(malformed(Some(part), "PNG chunk type is invalid"));
        }
        let data_end = header_end
            .checked_add(length)
            .filter(|end| end.checked_add(4).is_some_and(|crc_end| crc_end <= bytes.len()))
            .ok_or_else(|| malformed(Some(part), "truncated or oversized PNG chunk"))?;
        let crc_end = data_end + 4;
        let expected_crc = u32::from_be_bytes(
            bytes[data_end..crc_end]
                .try_into()
                .map_err(|_| malformed(Some(part), "truncated PNG CRC"))?,
        );
        if png_crc32(&bytes[cursor + 4..data_end]) != expected_crc {
            return Err(malformed(Some(part), "PNG chunk CRC mismatch"));
        }
        match kind {
            b"IHDR" => {
                if saw_header || chunks != 1 || length != 13 {
                    return Err(malformed(
                        Some(part),
                        "PNG IHDR is missing, duplicated, or invalid",
                    ));
                }
                let data = &bytes[header_end..data_end];
                let width = u32::from_be_bytes(data[0..4].try_into().expect("fixed IHDR width"));
                let height = u32::from_be_bytes(data[4..8].try_into().expect("fixed IHDR height"));
                validate_image_dimensions(width, height, part)?;
                let bits_per_pixel = match (data[8], data[9]) {
                    (depth @ (1 | 2 | 4 | 8 | 16), 0) | (depth @ (1 | 2 | 4 | 8), 3) => Some(depth),
                    (depth @ (8 | 16), 2) => depth.checked_mul(3),
                    (depth @ (8 | 16), 4) => depth.checked_mul(2),
                    (depth @ (8 | 16), 6) => depth.checked_mul(4),
                    _ => None,
                };
                if bits_per_pixel.is_none() || data[10] != 0 || data[11] != 0 || data[12] != 0 {
                    return Err(malformed(Some(part), "PNG IHDR uses an unsupported encoding"));
                }
                layout = Some((width, height, bits_per_pixel.expect("checked above"), data[9]));
                saw_header = true;
            }
            b"IDAT" => {
                if !saw_header || length == 0 || data_ended {
                    return Err(malformed(
                        Some(part),
                        "PNG IDAT is empty, misplaced, or non-contiguous",
                    ));
                }
                saw_data = true;
                idat_bytes = idat_bytes
                    .checked_add(u64::try_from(length).unwrap_or(u64::MAX))
                    .ok_or_else(|| malformed(Some(part), "PNG IDAT length overflow"))?;
            }
            b"IEND" => {
                if length != 0 || !saw_header || !saw_data || crc_end != bytes.len() {
                    return Err(malformed(Some(part), "PNG IEND or trailing data is invalid"));
                }
                if layout.is_some_and(|(_, _, _, color_type)| color_type == 3) && !saw_palette {
                    return Err(malformed(Some(part), "indexed PNG is missing its palette"));
                }
                break;
            }
            b"PLTE" => {
                if !saw_header
                    || saw_palette
                    || saw_data
                    || length == 0
                    || !length.is_multiple_of(3)
                    || length > 768
                    || layout.is_some_and(|(_, _, _, color_type)| matches!(color_type, 0 | 4))
                    || layout.is_some_and(|(_, _, depth, color_type)| {
                        color_type == 3 && length / 3 > (1_usize << depth)
                    })
                {
                    return Err(malformed(Some(part), "PNG palette is invalid"));
                }
                saw_palette = true;
            }
            _ if kind[0].is_ascii_uppercase() => {
                return Err(malformed(Some(part), "PNG contains an unsupported critical chunk"));
            }
            _ => {}
        }
        if saw_data && kind != b"IDAT" {
            data_ended = true;
        }
        cursor = crc_end;
    }
    let (width, height, bits_per_pixel, _) =
        layout.ok_or_else(|| malformed(Some(part), "PNG is missing IHDR"))?;
    validate_png_data(bytes, part, width, height, bits_per_pixel, idat_bytes, options, context)
}

struct PngIdatReader<'a> {
    bytes: &'a [u8],
    chunk_cursor: usize,
    data_cursor: usize,
    data_end: usize,
    finished: bool,
}

impl<'a> PngIdatReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, chunk_cursor: 8, data_cursor: 0, data_end: 0, finished: false }
    }

    fn next_data(&mut self) -> std::io::Result<bool> {
        while !self.finished {
            let header_end = self.chunk_cursor.checked_add(8).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG chunk offset overflow")
            })?;
            if header_end > self.bytes.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated PNG chunk",
                ));
            }
            let length = usize::try_from(u32::from_be_bytes(
                self.bytes[self.chunk_cursor..self.chunk_cursor + 4].try_into().map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid PNG length")
                })?,
            ))
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG length overflow")
            })?;
            let data_end = header_end.checked_add(length).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG data offset overflow")
            })?;
            let crc_end = data_end.checked_add(4).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "PNG CRC offset overflow")
            })?;
            if crc_end > self.bytes.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated PNG data",
                ));
            }
            let kind = &self.bytes[self.chunk_cursor + 4..header_end];
            self.chunk_cursor = crc_end;
            if kind == b"IDAT" {
                self.data_cursor = header_end;
                self.data_end = data_end;
                return Ok(true);
            }
            if kind == b"IEND" {
                self.finished = true;
            }
        }
        Ok(false)
    }
}

impl Read for PngIdatReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        while written < output.len() {
            if self.data_cursor == self.data_end && !self.next_data()? {
                break;
            }
            let count = (output.len() - written).min(self.data_end - self.data_cursor);
            output[written..written + count]
                .copy_from_slice(&self.bytes[self.data_cursor..self.data_cursor + count]);
            self.data_cursor += count;
            written += count;
        }
        Ok(written)
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_png_data(
    bytes: &[u8],
    part: &str,
    width: u32,
    height: u32,
    bits_per_pixel: u8,
    idat_bytes: u64,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let row_bytes = u64::from(width)
        .checked_mul(u64::from(bits_per_pixel))
        .and_then(|bits| bits.checked_add(7))
        .map(|bits| bits / 8)
        .ok_or_else(|| malformed(Some(part), "PNG row size overflow"))?;
    let row_stride =
        row_bytes.checked_add(1).ok_or_else(|| malformed(Some(part), "PNG row stride overflow"))?;
    let expected = row_stride
        .checked_mul(u64::from(height))
        .ok_or_else(|| malformed(Some(part), "PNG decoded size overflow"))?;
    if expected > options.limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            format!("decoded image {part}: {expected} > {}", options.limits.max_decompressed_bytes),
        ));
    }
    let _work_memory = context.reserve_memory(64 * 1024)?;
    let mut decoder =
        flate2::read::ZlibDecoder::new_with_buf(PngIdatReader::new(bytes), vec![0; 8 * 1024]);
    let mut buffer = [0_u8; 8 * 1024];
    let mut decompressed_bytes = 0_u64;
    let mut row_position = 0_u64;
    loop {
        context.checkpoint()?;
        let count = decoder
            .read(&mut buffer)
            .map_err(|error| malformed(Some(part), format!("invalid PNG pixel stream: {error}")))?;
        if count == 0 {
            break;
        }
        decompressed_bytes = decompressed_bytes
            .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
            .ok_or_else(|| malformed(Some(part), "PNG decoded length overflow"))?;
        if decompressed_bytes > expected {
            return Err(malformed(Some(part), "PNG pixel stream exceeds declared dimensions"));
        }
        for byte in &buffer[..count] {
            if row_position == 0 && *byte > 4 {
                return Err(malformed(Some(part), "PNG scanline filter is invalid"));
            }
            row_position += 1;
            if row_position == row_stride {
                row_position = 0;
            }
        }
    }
    if decompressed_bytes != expected
        || row_position != 0
        || decoder.total_out() != expected
        || decoder.total_in() != idat_bytes
    {
        return Err(malformed(
            Some(part),
            "PNG pixel stream does not match IHDR dimensions or IDAT bounds",
        ));
    }
    Ok(())
}

fn png_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 { crc >> 1 } else { (crc >> 1) ^ 0xedb8_8320 };
        }
    }
    !crc
}

fn validate_jpeg(bytes: &[u8], part: &str) -> Result<(u32, u32), ConversionError> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(malformed(Some(part), "image/jpeg target lacks the JPEG SOI marker"));
    }
    let mut cursor = 2_usize;
    let mut quantization_tables = BTreeSet::<u8>::new();
    let mut huffman_tables = BTreeSet::<(u8, u8)>::new();
    let mut frame = None::<(u32, u32, BTreeMap<u8, u8>)>;
    while cursor < bytes.len() {
        if bytes[cursor] != 0xff {
            return Err(malformed(Some(part), "JPEG marker boundary is invalid"));
        }
        while cursor < bytes.len() && bytes[cursor] == 0xff {
            cursor += 1;
        }
        let marker =
            *bytes.get(cursor).ok_or_else(|| malformed(Some(part), "truncated JPEG marker"))?;
        cursor += 1;
        if marker == 0xd9 {
            return Err(malformed(Some(part), "JPEG has no scan data"));
        }
        if matches!(marker, 0x00 | 0x01 | 0xd0..=0xd8) {
            return Err(malformed(Some(part), "unexpected standalone JPEG marker"));
        }
        let length_end = cursor
            .checked_add(2)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG segment length"))?;
        let length = usize::from(u16::from_be_bytes(
            bytes[cursor..length_end]
                .try_into()
                .map_err(|_| malformed(Some(part), "truncated JPEG segment length"))?,
        ));
        if length < 2 {
            return Err(malformed(Some(part), "JPEG segment length is invalid"));
        }
        let segment_end = cursor
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG segment"))?;
        let data = &bytes[length_end..segment_end];
        match marker {
            0xdb => validate_jpeg_quantization(data, &mut quantization_tables, part)?,
            0xc4 => validate_jpeg_huffman(data, &mut huffman_tables, part)?,
            0xc0 => {
                if frame.is_some() {
                    return Err(malformed(Some(part), "JPEG has multiple baseline frames"));
                }
                if data.len() < 6 || data[0] != 8 {
                    return Err(malformed(Some(part), "JPEG baseline frame is invalid"));
                }
                let height = u32::from(u16::from_be_bytes([data[1], data[2]]));
                let width = u32::from(u16::from_be_bytes([data[3], data[4]]));
                let components = usize::from(data[5]);
                if !(1..=4).contains(&components) || data.len() != 6 + components * 3 {
                    return Err(malformed(Some(part), "JPEG component table is invalid"));
                }
                validate_image_dimensions(width, height, part)?;
                let mut component_tables = BTreeMap::new();
                for component in data[6..].chunks_exact(3) {
                    let sampling = component[1];
                    if sampling >> 4 == 0
                        || sampling >> 4 > 4
                        || sampling.is_multiple_of(16)
                        || sampling & 0x0f > 4
                        || component[2] > 3
                        || component_tables.insert(component[0], component[2]).is_some()
                    {
                        return Err(malformed(Some(part), "JPEG frame component is invalid"));
                    }
                }
                frame = Some((width, height, component_tables));
            }
            0xc1..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                return Err(malformed(Some(part), "only baseline JPEG frames are supported"));
            }
            0xda => {
                validate_jpeg_scan_header(
                    data,
                    frame.as_ref().map(|(_, _, components)| components),
                    &quantization_tables,
                    &huffman_tables,
                    part,
                )?;
                validate_jpeg_scan(&bytes[segment_end..], part)?;
                return frame
                    .map(|(width, height, _)| (width, height))
                    .ok_or_else(|| malformed(Some(part), "JPEG is missing its frame"));
            }
            0xdd if data.len() == 2 => {}
            0xe0..=0xef | 0xfe => {}
            _ => return Err(malformed(Some(part), "unsupported JPEG segment type")),
        }
        cursor = segment_end;
    }
    Err(malformed(Some(part), "JPEG is missing scan and EOI markers"))
}

fn validate_jpeg_pixels(
    bytes: &[u8],
    expected_dimensions: (u32, u32),
    part: &str,
    options: &ConversionOptions,
    context: &ExecutionContext,
) -> Result<(), ConversionError> {
    let maximum_pixel_bytes = u64::from(expected_dimensions.0)
        .checked_mul(u64::from(expected_dimensions.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| limit("image_decode_memory", "JPEG decoded size overflow"))?;
    if maximum_pixel_bytes > options.limits.max_decompressed_bytes {
        return Err(limit(
            "max_decompressed_bytes",
            format!(
                "decoded image {part}: {maximum_pixel_bytes} > {}",
                options.limits.max_decompressed_bytes
            ),
        ));
    }
    let compressed_bytes = u64::try_from(bytes.len())
        .map_err(|_| limit("image_decode_memory", "JPEG compressed size overflow"))?;
    // Reserve before constructing the decoder. This covers the retained package buffer, the
    // decoder's private input copy, output pixels, component planes/upsampling scratch, and a
    // fixed codec-state allowance. Decoder allocation limits are defense in depth; this explicit
    // request reservation is the authoritative bound.
    let working_set = maximum_pixel_bytes
        .checked_mul(6)
        .and_then(|value| compressed_bytes.checked_mul(2).and_then(|size| value.checked_add(size)))
        .and_then(|value| value.checked_add(256 * 1024))
        .ok_or_else(|| limit("image_decode_memory", "JPEG decode working set overflow"))?;
    let _decode_memory = context.reserve_memory(working_set)?;
    context.checkpoint()?;

    let mut decoder = JpegDecoder::new(Cursor::new(bytes))
        .map_err(|_| malformed(Some(part), "image/jpeg decoder rejected the image header"))?;
    let mut limits = ImageLimits::default();
    limits.max_image_width = Some(expected_dimensions.0);
    limits.max_image_height = Some(expected_dimensions.1);
    limits.max_alloc = Some(working_set);
    decoder
        .set_limits(limits)
        .map_err(|_| malformed(Some(part), "image/jpeg decoder rejected the resource limits"))?;
    if decoder.dimensions() != expected_dimensions {
        return Err(malformed(
            Some(part),
            "image/jpeg decoder dimensions disagree with the validated frame",
        ));
    }
    let decoded_bytes = decoder.total_bytes();
    if decoded_bytes > maximum_pixel_bytes || decoded_bytes > options.limits.max_decompressed_bytes
    {
        return Err(limit(
            "max_decompressed_bytes",
            format!("decoded image/jpeg pixels in {part} exceed the configured budget"),
        ));
    }
    let decoded_length = usize::try_from(decoded_bytes)
        .map_err(|_| limit("max_decompressed_bytes", "JPEG decoded size cannot be represented"))?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(decoded_length).map_err(|error| {
        limit("max_memory_bytes", format!("cannot reserve JPEG pixels: {error}"))
    })?;
    pixels.resize(decoded_length, 0);
    decoder
        .read_image(&mut pixels)
        .map_err(|_| malformed(Some(part), "image/jpeg entropy stream is not decodable"))?;
    context.checkpoint()?;
    Ok(())
}

fn validate_jpeg_quantization(
    data: &[u8],
    tables: &mut BTreeSet<u8>,
    part: &str,
) -> Result<(), ConversionError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let selector = data[cursor];
        cursor += 1;
        let precision = selector >> 4;
        let table = selector & 0x0f;
        if precision > 1 || table > 3 || !tables.insert(table) {
            return Err(malformed(Some(part), "JPEG quantization table selector is invalid"));
        }
        let table_bytes = if precision == 0 { 64 } else { 128 };
        let table_end = cursor
            .checked_add(table_bytes)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG quantization table"))?;
        let values_valid = if precision == 0 {
            data[cursor..table_end].iter().all(|value| *value != 0)
        } else {
            data[cursor..table_end].chunks_exact(2).all(|value| value != [0, 0])
        };
        if !values_valid {
            return Err(malformed(Some(part), "JPEG quantization value is zero"));
        }
        cursor = table_end;
    }
    if cursor == 0 {
        return Err(malformed(Some(part), "empty JPEG quantization segment"));
    }
    Ok(())
}

fn validate_jpeg_huffman(
    data: &[u8],
    tables: &mut BTreeSet<(u8, u8)>,
    part: &str,
) -> Result<(), ConversionError> {
    let mut cursor = 0;
    while cursor < data.len() {
        let header_end = cursor
            .checked_add(17)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG Huffman table"))?;
        let selector = data[cursor];
        let class = selector >> 4;
        let table = selector & 0x0f;
        if class > 1 || table > 3 || !tables.insert((class, table)) {
            return Err(malformed(Some(part), "JPEG Huffman table selector is invalid"));
        }
        let counts = &data[cursor + 1..header_end];
        let symbols = counts
            .iter()
            .try_fold(0_usize, |count, value| count.checked_add(usize::from(*value)))
            .ok_or_else(|| malformed(Some(part), "JPEG Huffman symbol count overflow"))?;
        if symbols == 0 || symbols > 256 {
            return Err(malformed(Some(part), "JPEG Huffman symbol count is invalid"));
        }
        let mut code_space = 1_i32;
        for count in counts {
            code_space = code_space
                .checked_mul(2)
                .and_then(|space| space.checked_sub(i32::from(*count)))
                .ok_or_else(|| malformed(Some(part), "JPEG Huffman code space overflow"))?;
            if code_space < 0 {
                return Err(malformed(Some(part), "JPEG Huffman table is oversubscribed"));
            }
        }
        let symbols_end = header_end
            .checked_add(symbols)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| malformed(Some(part), "truncated JPEG Huffman symbols"))?;
        let symbols_valid = if class == 0 {
            data[header_end..symbols_end].iter().all(|symbol| *symbol <= 11)
        } else {
            data[header_end..symbols_end].iter().all(|symbol| {
                let run = symbol >> 4;
                let size = symbol & 0x0f;
                size <= 10 && (size != 0 || matches!(run, 0 | 15))
            })
        };
        if !symbols_valid {
            return Err(malformed(Some(part), "JPEG Huffman symbol is invalid"));
        }
        cursor = symbols_end;
    }
    if cursor == 0 {
        return Err(malformed(Some(part), "empty JPEG Huffman segment"));
    }
    Ok(())
}

fn validate_jpeg_scan_header(
    data: &[u8],
    frame: Option<&BTreeMap<u8, u8>>,
    quantization_tables: &BTreeSet<u8>,
    huffman_tables: &BTreeSet<(u8, u8)>,
    part: &str,
) -> Result<(), ConversionError> {
    let frame = frame.ok_or_else(|| malformed(Some(part), "JPEG scan precedes its frame"))?;
    let components = data.first().copied().map_or(0, usize::from);
    if components != frame.len() || data.len() != 4 + components * 2 {
        return Err(malformed(Some(part), "JPEG scan component table is invalid"));
    }
    let mut seen = BTreeSet::new();
    for component in data[1..=components * 2].chunks_exact(2) {
        let id = component[0];
        let dc = component[1] >> 4;
        let ac = component[1] & 0x0f;
        if !frame.contains_key(&id)
            || !seen.insert(id)
            || !huffman_tables.contains(&(0, dc))
            || !huffman_tables.contains(&(1, ac))
        {
            return Err(malformed(Some(part), "JPEG scan references an undefined table"));
        }
    }
    if frame.values().any(|table| !quantization_tables.contains(table))
        || data[data.len() - 3..] != [0, 63, 0]
    {
        return Err(malformed(Some(part), "JPEG baseline scan parameters are invalid"));
    }
    Ok(())
}

fn validate_jpeg_scan(bytes: &[u8], part: &str) -> Result<(), ConversionError> {
    let mut cursor = 0;
    let mut entropy_bytes = 0_usize;
    while cursor < bytes.len() {
        if bytes[cursor] != 0xff {
            entropy_bytes += 1;
            cursor += 1;
            continue;
        }
        let marker = *bytes
            .get(cursor + 1)
            .ok_or_else(|| malformed(Some(part), "truncated JPEG entropy marker"))?;
        match marker {
            0x00 => {
                entropy_bytes += 1;
                cursor += 2;
            }
            0xd0..=0xd7 => cursor += 2,
            0xd9 if entropy_bytes != 0 && cursor + 2 == bytes.len() => return Ok(()),
            _ => return Err(malformed(Some(part), "unsupported JPEG marker inside scan data")),
        }
    }
    Err(malformed(Some(part), "JPEG scan is missing EOI"))
}

fn malformed(part: Option<&str>, detail: impl Into<String>) -> ConversionError {
    ConversionError::Malformed { part: part.map(str::to_owned), detail: detail.into() }
}
fn limit(name: &'static str, detail: impl Into<String>) -> ConversionError {
    ConversionError::ResourceLimit { limit: name, detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use into_markdown_core::{ConversionOptions, ExecutionOptions, ResourceLimits};
    use into_markdown_render_markdown::render;
    use std::fmt::Write as _;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const WORD: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const OFFICE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PACKAGE_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const MATH: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";
    const MC: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    const DRAWING: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    const WORD_DRAWING: &str =
        "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
    const DOC_CONTENT_TYPE: &str =
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";

    fn context() -> ExecutionContext {
        ExecutionContext::new(ExecutionOptions::default(), ResourceLimits::default())
    }

    fn limited_context(max_memory_bytes: u64) -> ExecutionContext {
        let limits = ResourceLimits { max_memory_bytes, ..ResourceLimits::default() };
        ExecutionContext::new(ExecutionOptions::default(), limits)
    }

    fn package(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut output);
            for (name, bytes) in parts {
                zip.start_file(
                    name,
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        output.into_inner()
    }

    fn append_png_chunk(output: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        output.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        output.extend_from_slice(&kind);
        output.extend_from_slice(data);
        let mut checked = Vec::with_capacity(kind.len() + data.len());
        checked.extend_from_slice(&kind);
        checked.extend_from_slice(data);
        output.extend_from_slice(&png_crc32(&checked).to_be_bytes());
    }

    fn valid_png(padding: usize) -> Vec<u8> {
        let mut output = b"\x89PNG\r\n\x1a\n".to_vec();
        append_png_chunk(&mut output, *b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
        if padding != 0 {
            let mut text = b"Comment\0".to_vec();
            text.resize(text.len() + padding, b'x');
            append_png_chunk(&mut output, *b"tEXt", &text);
        }
        append_png_chunk(&mut output, *b"IDAT", &[0x78, 0x9c, 0x63, 0x60, 0, 0, 0, 2, 0, 1]);
        append_png_chunk(&mut output, *b"IEND", &[]);
        output
    }

    fn append_jpeg_segment(output: &mut Vec<u8>, marker: u8, data: &[u8]) {
        output.extend_from_slice(&[0xff, marker]);
        output.extend_from_slice(&u16::try_from(data.len() + 2).unwrap().to_be_bytes());
        output.extend_from_slice(data);
    }

    fn valid_jpeg() -> Vec<u8> {
        let mut output = vec![0xff, 0xd8];
        let mut quantization = vec![0];
        quantization.extend_from_slice(&[1; 64]);
        append_jpeg_segment(&mut output, 0xdb, &quantization);
        append_jpeg_segment(&mut output, 0xc0, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]);
        let mut huffman = vec![0, 1];
        huffman.extend_from_slice(&[0; 15]);
        huffman.push(0);
        huffman.extend_from_slice(&[0x10, 1]);
        huffman.extend_from_slice(&[0; 15]);
        huffman.push(0);
        append_jpeg_segment(&mut output, 0xc4, &huffman);
        append_jpeg_segment(&mut output, 0xda, &[1, 1, 0, 0, 63, 0]);
        output.extend_from_slice(&[0x3f, 0xff, 0xd9]);
        output
    }

    fn base(document: &[u8], extra: &[(&str, &[u8])]) -> Vec<u8> {
        base_with_type(document, extra, DOC_CONTENT_TYPE)
    }

    fn base_with_type(document: &[u8], extra: &[(&str, &[u8])], content_type: &str) -> Vec<u8> {
        let mut overrides =
            format!(r#"<Override PartName="/word/document.xml" ContentType="{content_type}"/>"#);
        for (name, _) in extra {
            let content_type = match *name {
                "word/styles.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml",
                ),
                "word/numbering.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml",
                ),
                "word/comments.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml",
                ),
                "word/footnotes.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml",
                ),
                "word/endnotes.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.endnotes+xml",
                ),
                "word/header1.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml",
                ),
                "word/footer1.xml" => Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml",
                ),
                "docProps/core.xml" => {
                    Some("application/vnd.openxmlformats-package.core-properties+xml")
                }
                _ => None,
            };
            if let Some(content_type) = content_type {
                write!(
                    &mut overrides,
                    r#"<Override PartName="/{name}" ContentType="{content_type}"/>"#
                )
                .unwrap();
            }
        }
        let types = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="png" ContentType="image/png"/>{overrides}</Types>"#
        );
        let core_relationship = if extra.iter().any(|(name, _)| *name == "docProps/core.xml") {
            format!(
                r#"<Relationship Id="rCore" Type="{REL_TYPE_PREFIX}metadata/core-properties" Target="docProps/core.xml"/>"#
            )
        } else {
            String::new()
        };
        let root_relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rDocument" Type="{OFFICE_REL_TYPE}" Target="word/document.xml"/>{core_relationship}</Relationships>"#
        );
        let mut parts = vec![
            ("[Content_Types].xml".to_owned(), types.into_bytes()),
            ("_rels/.rels".to_owned(), root_relationships.into_bytes()),
            ("word/document.xml".to_owned(), document.to_vec()),
        ];
        parts.extend(extra.iter().map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec())));
        package(&parts)
    }

    fn image_package(part: &str, declared_content_type: &str, bytes: &[u8]) -> Vec<u8> {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:a="{DRAWING}"><w:body><w:p><w:r><w:drawing><a:blip r:embed="rImage"/></w:drawing></w:r></w:p></w:body></w:document>"#
        );
        let types = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="{DOC_CONTENT_TYPE}"/><Override PartName="/{part}" ContentType="{declared_content_type}"/></Types>"#
        );
        let root = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rDocument" Type="{OFFICE_REL_TYPE}" Target="word/document.xml"/></Relationships>"#
        );
        let target = part.strip_prefix("word/").expect("test image belongs to word part");
        let relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rImage" Type="{REL_TYPE_PREFIX}image" Target="{target}"/></Relationships>"#
        );
        package(&[
            ("[Content_Types].xml".into(), types.into_bytes()),
            ("_rels/.rels".into(), root.into_bytes()),
            ("word/document.xml".into(), document.into_bytes()),
            ("word/_rels/document.xml.rels".into(), relationships.into_bytes()),
            (part.into(), bytes.to_vec()),
        ])
    }

    #[test]
    fn converts_styles_lists_links_images_footnotes_headers_comments_fields_and_formula() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:m="{MATH}" xmlns:a="{DRAWING}" xmlns:wp="{WORD_DRAWING}"><w:body><w:p><w:pPr><w:pStyle w:val="CustomHeading"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>Title</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:hyperlink r:id="rLink"><w:r><w:t>site</w:t></w:r></w:hyperlink><w:r><w:footnoteReference w:id="2"/></w:r><w:r><w:commentReference w:id="0"/></w:r><w:r><w:fldChar w:fldCharType="begin"/><w:instrText> PAGE </w:instrText><w:fldChar w:fldCharType="end"/></w:r><m:oMath><m:r><m:t>x+y</m:t></m:r></m:oMath><w:r><w:drawing><wp:docPr id="1" name="picture" descr="alt"/><a:blip r:embed="rImg"/></w:drawing></w:r></w:p><w:tbl><w:tr><w:trPr><w:tblHeader/></w:trPr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:sectPr><w:headerReference r:id="rHeader"/></w:sectPr></w:body></w:document>"#
        );
        let rels = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rLink" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://example.com" TargetMode="External"/><Relationship Id="rImg" Type="{REL_TYPE_PREFIX}image" Target="media/a.png"/><Relationship Id="rStyles" Type="{REL_TYPE_PREFIX}styles" Target="styles.xml"/><Relationship Id="rNumbering" Type="{REL_TYPE_PREFIX}numbering" Target="numbering.xml"/><Relationship Id="rFootnotes" Type="{REL_TYPE_PREFIX}footnotes" Target="footnotes.xml"/><Relationship Id="rComments" Type="{REL_TYPE_PREFIX}comments" Target="comments.xml"/><Relationship Id="rHeader" Type="{REL_TYPE_PREFIX}header" Target="header1.xml"/></Relationships>"#
        );
        let styles = format!(
            r#"<w:styles xmlns:w="{WORD}"><w:style w:styleId="Heading1"><w:name w:val="heading 1"/></w:style><w:style w:styleId="CustomHeading"><w:basedOn w:val="Heading1"/></w:style></w:styles>"#
        );
        let numbering = format!(
            r#"<w:numbering xmlns:w="{WORD}"><w:abstractNum w:abstractNumId="7"><w:lvl w:ilvl="0"><w:start w:val="3"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="7"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="5"/></w:lvlOverride></w:num></w:numbering>"#
        );
        let footnotes = format!(
            r#"<w:footnotes xmlns:w="{WORD}"><w:footnote w:id="2"><w:p><w:r><w:t>note</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        let comments = format!(
            r#"<w:comments xmlns:w="{WORD}"><w:comment w:id="0"><w:p><w:r><w:t>review</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let header =
            format!(r#"<w:hdr xmlns:w="{WORD}"><w:p><w:r><w:t>head</w:t></w:r></w:p></w:hdr>"#);
        let core = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Fixture title</dc:title><dc:creator>Fixture author</dc:creator></cp:coreProperties>"#;
        let image = valid_png(0);
        let bytes = base(
            document.as_bytes(),
            &[
                ("word/_rels/document.xml.rels", rels.as_bytes()),
                ("word/styles.xml", styles.as_bytes()),
                ("word/numbering.xml", numbering.as_bytes()),
                ("word/footnotes.xml", footnotes.as_bytes()),
                ("word/comments.xml", comments.as_bytes()),
                ("word/header1.xml", header.as_bytes()),
                ("word/media/a.png", image.as_slice()),
                ("docProps/core.xml", core.as_bytes()),
            ],
        );
        let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(markdown.contains("# <strong>Title</strong>"), "{markdown}");
        assert!(markdown.contains("[site](<https://example.com>)"));
        assert!(markdown.contains("5."));
        assert!(markdown.contains("[^fn-32]"));
        assert!(markdown.contains("$`x+y`$"));
        assert!(markdown.contains("cell"));
        assert!(markdown.contains("Header") && markdown.contains("head"));
        assert!(markdown.contains("Comment 0") && markdown.contains("review"));
        assert!(markdown.contains("note"));
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.document.metadata.title.as_deref(), Some("Fixture title"));
    }

    #[test]
    fn predefined_and_numeric_references_reassemble_across_all_text_and_attribute_consumers() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:m="{MATH}"><w:body><w:p><w:r><w:t>A&amp;<![CDATA[<B&]]>&#38;&#x4E2D;&apos;&quot;&gt;</w:t></w:r><w:hyperlink r:id="r1"><w:r><w:t>go&amp;&#x4E2D;</w:t></w:r></w:hyperlink><w:r><w:fldChar w:fldCharType="begin"/><w:instrText>HYPERLINK &quot;https://field.example/?x=1&amp;y=2&quot;</w:instrText><w:fldChar w:fldCharType="end"/></w:r><m:oMath><m:r><m:t>x&amp;<![CDATA[<y]]>&#x4E2D;</m:t></m:r></m:oMath><w:r><w:commentReference w:id="0"/></w:r><w:r><w:footnoteReference w:id="2"/></w:r></w:p></w:body></w:document>"#
        );
        let relationships = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="r&#49;" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://example.com/?a=1&amp;b=&#50;" TargetMode="External"/><Relationship Id="rComments" Type="{REL_TYPE_PREFIX}comments" Target="comments.xml"/><Relationship Id="rFootnotes" Type="{REL_TYPE_PREFIX}footnotes" Target="footnotes.xml"/></Relationships>"#
        );
        let comments = format!(
            r#"<w:comments xmlns:w="{WORD}"><w:comment w:id="0"><w:p><w:r><w:t>comment&amp;<![CDATA[<piece>]]>&#x4E2D;</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let footnotes = format!(
            r#"<w:footnotes xmlns:w="{WORD}"><w:footnote w:id="2"><w:p><w:r><w:t>foot&amp;<![CDATA[<piece>]]>&#20013;</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        let core = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Core&amp;<![CDATA[<Title>]]>&#x4E2D;</dc:title><dc:creator>A&amp;B</dc:creator></cp:coreProperties>"#;
        let bytes = base(
            document.as_bytes(),
            &[
                ("word/_rels/document.xml.rels", relationships.as_bytes()),
                ("word/comments.xml", comments.as_bytes()),
                ("word/footnotes.xml", footnotes.as_bytes()),
                ("docProps/core.xml", core.as_bytes()),
            ],
        );
        let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();
        assert_eq!(output.document.metadata.title.as_deref(), Some("Core&<Title>中"));
        assert_eq!(output.document.metadata.authors, ["A&B"]);

        let main = output.document.blocks.iter().find_map(|node| match &node.block {
            Block::Paragraph(inlines) => Some(inlines),
            _ => None,
        });
        let main = main.expect("main paragraph");
        assert!(main.iter().any(|inline| matches!(
            inline,
            Inline::Text { value, .. } if value == "A&<B&&中'\">"
        )));
        assert!(main.iter().any(|inline| matches!(
            inline,
            Inline::Link { target, content }
                if target == "https://example.com/?a=1&b=2"
                    && matches!(content.as_slice(), [Inline::Text { value, .. }] if value == "go&中")
        )));
        assert!(main.iter().any(|inline| matches!(
            inline,
            Inline::Link { target, .. } if target == "https://field.example/?x=1&y=2"
        )));
        assert!(
            main.iter().any(|inline| matches!(inline, Inline::Formula(value) if value == "x&<y中"))
        );

        assert!(output.document.blocks.iter().any(|node| matches!(
            &node.block,
            Block::Paragraph(inlines)
                if inlines.iter().any(|inline| matches!(inline, Inline::Text { value, .. } if value == "comment&<piece>中"))
        )));
        assert!(output.document.blocks.iter().any(|node| matches!(
            &node.block,
            Block::Footnote { blocks, .. }
                if blocks.iter().any(|block| matches!(
                    &block.block,
                    Block::Paragraph(inlines)
                        if inlines.iter().any(|inline| matches!(inline, Inline::Text { value, .. } if value == "foot&<piece>中"))
                ))
        )));
    }

    #[test]
    fn custom_dtd_and_illegal_character_references_remain_fail_closed() {
        for reference in ["&custom;", "&#0;", "&#x1;", "&#xD800;", "&#x110000;", "&#X41;"] {
            let document = format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>{reference}</w:t></w:r></w:p></w:body></w:document>"#
            );
            assert!(matches!(
                convert_docx(
                    &base(document.as_bytes(), &[]),
                    &ConversionOptions::default(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let document = format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#);
        let custom_attribute = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="bad" Type="{REL_TYPE_PREFIX}hyperlink" Target="https://example.com/?q=&custom;" TargetMode="External"/></Relationships>"#
        );
        assert!(matches!(
            convert_docx(
                &base(
                    document.as_bytes(),
                    &[("word/_rels/document.xml.rels", custom_attribute.as_bytes())],
                ),
                &ConversionOptions::default(),
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));

        let dtd = format!(
            r#"<!DOCTYPE w:document [<!ENTITY custom "expanded">]><w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>&custom;</w:t></w:r></w:p></w:body></w:document>"#
        );
        assert!(matches!(
            convert_docx(&base(dtd.as_bytes(), &[]), &ConversionOptions::default(), &context(),),
            Err(ConversionError::Malformed { .. })
        ));
    }

    #[test]
    fn roots_namespaces_hierarchy_and_text_context_fail_closed() {
        for invalid in [
            format!(r#"<w:hdr xmlns:w="{WORD}"/>"#),
            r#"<w:document xmlns:w="w"><w:body/></w:document>"#.to_owned(),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:p><w:r><w:t>outside</w:t></w:r></w:p><w:body/></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}" xmlns:e="urn:evil"><w:body><w:p><w:r><e:t>spoofed</e:t></w:r></w:p></w:body></w:document>"#
            ),
        ] {
            assert!(matches!(
                convert_docx(
                    &base(invalid.as_bytes(), &[]),
                    &ConversionOptions::default(),
                    &context()
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:mc="{MC}" xmlns:x="urn:fixture-extension"><w:body>
              <w:p><x:payload>must-not-leak</x:payload><w:r><w:t>kept</w:t></w:r></w:p>
              <mc:AlternateContent><mc:Choice Requires="x"><w:p><w:r><w:t>choice-must-not-leak</w:t></w:r></w:p></mc:Choice><mc:Fallback><w:p><w:r><w:t>fallback-kept</w:t></w:r></w:p></mc:Fallback></mc:AlternateContent>
            </w:body></w:document>"#
        );
        let output = convert_docx(
            &base(document.as_bytes(), &[]),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(markdown.contains("kept") && markdown.contains("fallback\\-kept"), "{markdown}");
        assert!(!markdown.contains("must-not-leak") && !markdown.contains("choice-must-not-leak"));
    }

    #[test]
    fn core_properties_require_authoritative_namespace_and_direct_children() {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>body</w:t></w:r></w:p></w:body></w:document>"#
        );
        let core_cases = [
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:e="urn:evil"><e:lastModifiedBy>spoof</e:lastModifiedBy></cp:coreProperties>"#.to_owned(),
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"><cp:keywords><cp:lastModifiedBy>nested</cp:lastModifiedBy></cp:keywords></cp:coreProperties>"#.to_owned(),
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title><dc:creator>nested</dc:creator></dc:title></cp:coreProperties>"#.to_owned(),
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"><cp:modified>wrong namespace</cp:modified></cp:coreProperties>"#.to_owned(),
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:e="urn:evil"><dc:title><e:payload>nested text</e:payload></dc:title></cp:coreProperties>"#.to_owned(),
        ];
        for core in core_cases {
            assert!(matches!(
                convert_docx(
                    &base(document.as_bytes(), &[("docProps/core.xml", core.as_bytes())],),
                    &ConversionOptions::default(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let valid = r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/"><dc:title>title</dc:title><dc:creator>creator</dc:creator><cp:lastModifiedBy>editor</cp:lastModifiedBy><dcterms:modified>2026-08-13T00:00:00Z</dcterms:modified></cp:coreProperties>"#;
        let output = convert_docx(
            &base(document.as_bytes(), &[("docProps/core.xml", valid.as_bytes())]),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(output.document.metadata.title.as_deref(), Some("title"));
        assert_eq!(output.document.metadata.authors, ["creator", "editor"]);
    }

    #[test]
    fn style_numbering_and_word_semantics_reject_relocation_and_spoofing() {
        let document = format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#);
        let styles_relation = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="styles" Type="{REL_TYPE_PREFIX}styles" Target="styles.xml"/></Relationships>"#
        );
        let invalid_styles = [
            format!(
                r#"<w:styles xmlns:w="{WORD}"><w:style w:styleId="x"><w:pPr><w:name w:val="heading 1"/></w:pPr></w:style></w:styles>"#
            ),
            format!(
                r#"<w:styles xmlns:w="{WORD}" xmlns:e="urn:evil"><w:style w:styleId="x"><e:basedOn w:val="Heading1"/></w:style></w:styles>"#
            ),
            format!(
                r#"<w:styles xmlns:w="{WORD}"><w:style w:styleId="x"><w:outlineLvl w:val="0"/></w:style></w:styles>"#
            ),
            format!(
                r#"<w:styles xmlns:w="{WORD}"><w:pPr><w:style w:styleId="x"/></w:pPr></w:styles>"#
            ),
        ];
        for styles in invalid_styles {
            assert!(matches!(
                convert_docx(
                    &base(
                        document.as_bytes(),
                        &[
                            ("word/_rels/document.xml.rels", styles_relation.as_bytes()),
                            ("word/styles.xml", styles.as_bytes()),
                        ],
                    ),
                    &ConversionOptions::default(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let numbering_relation = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="numbering" Type="{REL_TYPE_PREFIX}numbering" Target="numbering.xml"/></Relationships>"#
        );
        let invalid_numbering = [
            format!(
                r#"<w:numbering xmlns:w="{WORD}"><w:abstractNum w:abstractNumId="1"><w:numFmt w:val="decimal"/></w:abstractNum></w:numbering>"#
            ),
            format!(
                r#"<w:numbering xmlns:w="{WORD}"><w:num w:numId="1"><w:lvl w:ilvl="0"><w:start w:val="9"/></w:lvl></w:num></w:numbering>"#
            ),
            format!(
                r#"<w:numbering xmlns:w="{WORD}"><w:num w:numId="1"><w:startOverride w:val="9"/></w:num></w:numbering>"#
            ),
            format!(
                r#"<w:numbering xmlns:w="{WORD}" xmlns:e="urn:evil"><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><e:lvlText w:val="%1"/></w:lvl></w:abstractNum></w:numbering>"#
            ),
        ];
        for numbering in invalid_numbering {
            assert!(matches!(
                convert_docx(
                    &base(
                        document.as_bytes(),
                        &[
                            ("word/_rels/document.xml.rels", numbering_relation.as_bytes()),
                            ("word/numbering.xml", numbering.as_bytes()),
                        ],
                    ),
                    &ConversionOptions::default(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }

        let invalid_documents = [
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:pStyle w:val="Heading1"/><w:r><w:t>x</w:t></w:r></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:b/><w:t>not bold</w:t></w:r></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:tab/><w:r><w:t>x</w:t></w:r></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:numId w:val="1"/><w:t>x</w:t></w:r></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}" xmlns:m="{MATH}"><w:body><w:p><m:t>spoof</m:t></w:p></w:body></w:document>"#
            ),
            format!(
                r#"<w:document xmlns:w="{WORD}"><w:body><w:comment w:id="7"><w:p><w:r><w:t>relocated annotation</w:t></w:r></w:p></w:comment></w:body></w:document>"#
            ),
        ];
        for invalid in invalid_documents {
            assert!(matches!(
                convert_docx(
                    &base(invalid.as_bytes(), &[]),
                    &ConversionOptions::default(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn images_require_content_type_extension_and_valid_bounded_structure() {
        let png = valid_png(0);
        let output = convert_docx(
            &image_package("word/media/image.png", "image/png", &png),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].media_type, "image/png");
        assert_eq!(output.assets[0].bytes, png);
        let jpeg = valid_jpeg();
        let output = convert_docx(
            &image_package("word/media/image.jpg", "image/jpeg", &jpeg),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].media_type, "image/jpeg");
        assert_eq!(output.assets[0].bytes, jpeg);

        let mut corrupt_crc = valid_png(0);
        let index = corrupt_crc.len() - 5;
        corrupt_crc[index] ^= 1;
        let corrupt_idat = {
            let mut value = valid_png(0);
            value[41] ^= 1;
            let crc = png_crc32(&value[37..51]);
            value[51..55].copy_from_slice(&crc.to_be_bytes());
            value
        };
        let oversized_header = {
            let mut value = valid_png(0);
            value[16..20].copy_from_slice(&(MAX_IMAGE_DIMENSION + 1).to_be_bytes());
            let crc = png_crc32(&value[12..29]);
            value[29..33].copy_from_slice(&crc.to_be_bytes());
            value
        };
        let mismatch = valid_png(0);
        let mut truncated_jpeg = valid_jpeg();
        truncated_jpeg.pop();
        let mut corrupt_jpeg_codestream = valid_jpeg();
        let entropy = corrupt_jpeg_codestream.len() - 3;
        corrupt_jpeg_codestream[entropy] = 0x7f;
        assert_eq!(
            validate_jpeg(&corrupt_jpeg_codestream, "word/media/codestream.jpg").unwrap(),
            (1, 1),
            "the adversarial fixture must remain marker/table/frame/scan valid"
        );
        match convert_docx(
            &image_package("word/media/codestream.jpg", "image/jpeg", &corrupt_jpeg_codestream),
            &ConversionOptions::default(),
            &context(),
        ) {
            Err(ConversionError::Malformed { detail, .. }) => {
                assert!(detail.contains("entropy stream"), "unexpected error: {detail}");
            }
            other => panic!("expected corrupt JPEG codestream rejection, got {other:?}"),
        }
        let adversarial = [
            ("word/media/fake.png", "image/png", b"PNG".as_slice()),
            ("word/media/truncated.png", "image/png", &valid_png(0)[..20]),
            ("word/media/corrupt.png", "image/png", corrupt_crc.as_slice()),
            ("word/media/broken-stream.png", "image/png", corrupt_idat.as_slice()),
            ("word/media/huge.png", "image/png", oversized_header.as_slice()),
            ("word/media/mismatch.png", "image/jpeg", mismatch.as_slice()),
            ("word/media/fake.jpg", "image/jpeg", mismatch.as_slice()),
            ("word/media/truncated.jpg", "image/jpeg", truncated_jpeg.as_slice()),
            ("word/media/ole.png", "image/png", b"\xd0\xcf\x11\xe0OLE"),
            ("word/media/program.png", "image/png", b"MZ"),
            ("word/media/vector.png", "image/png", b"<svg><script/></svg>"),
            ("word/media/opaque.bin", "application/octet-stream", b"opaque"),
            ("word/media/vector.svg", "image/svg+xml", b"<svg><script/></svg>"),
        ];
        for (part, content_type, bytes) in adversarial {
            assert!(matches!(
                convert_docx(
                    &image_package(part, content_type, bytes),
                    &ConversionOptions::default(),
                    &context(),
                ),
                Err(ConversionError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn relationships_are_type_checked_and_unreferenced_parts_cannot_inject() {
        let safe = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#
        );
        let header = format!(
            r#"<w:hdr xmlns:w="{WORD}"><w:p><w:r><w:t>injected-header</w:t></w:r></w:p></w:hdr>"#
        );
        let comments = format!(
            r#"<w:comments xmlns:w="{WORD}"><w:comment w:id="1"><w:p><w:r><w:t>injected-comment</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let output = convert_docx(
            &base(
                safe.as_bytes(),
                &[
                    ("word/header1.xml", header.as_bytes()),
                    ("word/comments.xml", comments.as_bytes()),
                ],
            ),
            &ConversionOptions::default(),
            &context(),
        )
        .unwrap();
        let markdown =
            render(&output.document, &output.assets, &ConversionOptions::default()).unwrap();
        assert!(markdown.contains("safe"));
        assert!(!markdown.contains("injected-header") && !markdown.contains("injected-comment"));

        let image_document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:a="{DRAWING}"><w:body><w:p><w:r><w:drawing><a:blip r:embed="rWrong"/></w:drawing></w:r></w:p></w:body></w:document>"#
        );
        let wrong_rels = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rWrong" Type="{REL_TYPE_PREFIX}comments" Target="media/a.png"/></Relationships>"#
        );
        let image = valid_png(0);
        assert!(matches!(
            convert_docx(
                &base(
                    image_document.as_bytes(),
                    &[
                        ("word/_rels/document.xml.rels", wrong_rels.as_bytes()),
                        ("word/media/a.png", image.as_slice()),
                    ],
                ),
                &ConversionOptions::default(),
                &context(),
            ),
            Err(ConversionError::Malformed { .. })
        ));
    }

    fn renamed_macro_package(by_content_type: bool) -> Vec<u8> {
        let document = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>safe</w:t></w:r></w:p></w:body></w:document>"#
        );
        let macro_part =
            if by_content_type { "word/media/renamed.dat" } else { "word/media/renamed.rels" };
        let macro_override = if by_content_type {
            r#"<Override PartName="/word/media/renamed.dat" ContentType="application/vnd.ms-office.vbaProject"/>"#
        } else {
            ""
        };
        let types = format!(
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="dat" ContentType="application/octet-stream"/><Override PartName="/word/document.xml" ContentType="{DOC_CONTENT_TYPE}"/>{macro_override}</Types>"#
        );
        let root = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rDocument" Type="{OFFICE_REL_TYPE}" Target="word/document.xml"/></Relationships>"#
        );
        let macro_relation = if by_content_type {
            String::new()
        } else {
            format!(
                r#"<Relationship Id="rMacro" Type="{REL_TYPE_PREFIX}vbaProject" Target="media/renamed.rels"/>"#
            )
        };
        let rels =
            format!(r#"<Relationships xmlns="{PACKAGE_REL}">{macro_relation}</Relationships>"#);
        let marker = b"UNIQUE_CORRUPTED_VBA_PAYLOAD".to_vec();
        let mut bytes = package(&[
            ("[Content_Types].xml".into(), types.into_bytes()),
            ("_rels/.rels".into(), root.into_bytes()),
            ("word/document.xml".into(), document.into_bytes()),
            (macro_part.into(), marker.clone()),
            ("word/_rels/document.xml.rels".into(), rels.into_bytes()),
        ]);
        let offset = bytes.windows(marker.len()).position(|value| value == marker).unwrap();
        bytes[offset] ^= 0x40;
        bytes
    }

    #[test]
    fn content_types_and_relationship_types_exclude_renamed_macros_before_decompression() {
        for bytes in [renamed_macro_package(true), renamed_macro_package(false)] {
            let output = convert_docx(&bytes, &ConversionOptions::default(), &context()).unwrap();
            assert!(output.diagnostics.iter().any(|d| d.code == "docx.macrosIgnored"));
            assert_eq!(
                output.document.metadata.properties.get("docx.macrosPresent").map(String::as_str),
                Some("true")
            );
        }
    }

    #[test]
    fn peak_memory_boundary_is_stable_and_assets_transfer_without_copying() {
        let text = "x".repeat(32 * 1024);
        let document = format!(
            r#"<w:document xmlns:w="{WORD}" xmlns:r="{OFFICE_REL}" xmlns:a="{DRAWING}"><w:body><w:p><w:r><w:t>{text}</w:t></w:r><w:r><w:drawing><a:blip r:embed="rImage"/><a:blip r:embed="rImage"/></w:drawing></w:r></w:p></w:body></w:document>"#
        );
        let rels = format!(
            r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="rImage" Type="{REL_TYPE_PREFIX}image" Target="media/large.png"/></Relationships>"#
        );
        let image = valid_png(64 * 1024);
        let bytes = base(
            document.as_bytes(),
            &[
                ("word/_rels/document.xml.rels", rels.as_bytes()),
                ("word/media/large.png", image.as_slice()),
            ],
        );
        let succeeds =
            |limit| convert_docx(&bytes, &ConversionOptions::default(), &limited_context(limit));
        let mut low = 0_u64;
        let mut high = 4 * 1024 * 1024_u64;
        assert!(succeeds(high).is_ok());
        while low + 1 < high {
            let middle = low + (high - low) / 2;
            if succeeds(middle).is_ok() {
                high = middle;
            } else {
                low = middle;
            }
        }
        assert!(matches!(
            succeeds(high - 1),
            Err(ConversionError::ResourceLimit { limit: "max_memory_bytes", .. })
        ));
        let output = succeeds(high).unwrap();
        assert_eq!(output.assets.len(), 1);
        assert_eq!(output.assets[0].bytes.len(), image.len());
    }

    #[test]
    fn nested_tables_and_vertical_merges_have_stable_errors() {
        let nested = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:tbl><w:tr><w:tc><w:tbl><w:tr><w:tc><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:tc></w:tr></w:tbl></w:body></w:document>"#
        );
        let merged = format!(
            r#"<w:document xmlns:w="{WORD}"><w:body><w:tbl><w:tr><w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#
        );
        for (document, expected) in [
            (nested, "nested tables are unsupported"),
            (merged, "vertical table merges are unsupported"),
        ] {
            match convert_docx(
                &base(document.as_bytes(), &[]),
                &ConversionOptions::default(),
                &context(),
            ) {
                Err(ConversionError::Malformed { detail, .. }) => {
                    assert!(detail.contains(expected));
                }
                other => panic!("expected stable table diagnostic, got {other:?}"),
            }
        }
    }

    #[test]
    fn corruption_dtd_traversal_and_budgets_fail_closed() {
        assert!(matches!(
            convert_docx(b"PK bad", &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
        let dtd = base(
            format!(r#"<!DOCTYPE x [<!ENTITY a "x">]><w:document xmlns:w="{WORD}"><w:body/></w:document>"#).as_bytes(),
            &[],
        );
        assert!(matches!(
            convert_docx(&dtd, &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
        let traversal = package(&[
            ("[Content_Types].xml".into(), format!(r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="{DOC_CONTENT_TYPE}"/></Types>"#).into_bytes()),
            ("../word/document.xml".into(), b"x".to_vec()),
        ]);
        assert!(matches!(
            convert_docx(&traversal, &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
        let escaping_relationship = base(
            format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#).as_bytes(),
            &[(
                "word/_rels/document.xml.rels",
                format!(r#"<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="bad" Type="{REL_TYPE_PREFIX}image" Target="../../secret"/></Relationships>"#).as_bytes(),
            )],
        );
        assert!(matches!(
            convert_docx(&escaping_relationship, &ConversionOptions::default(), &context()),
            Err(ConversionError::Malformed { .. })
        ));
        let valid_document = format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#);
        let valid = base(valid_document.as_bytes(), &[]);
        let mut options = ConversionOptions::default();
        options.limits.max_archive_entries = 2;
        assert!(matches!(
            convert_docx(&valid, &options, &context()),
            Err(ConversionError::ResourceLimit { limit: "max_archive_entries", .. })
        ));
        let deeply_nested = base(
            format!(r#"<w:document xmlns:w="{WORD}"><w:body><w:p><w:r><w:t>x</w:t></w:r></w:p></w:body></w:document>"#).as_bytes(),
            &[],
        );
        let mut depth_options = ConversionOptions::default();
        depth_options.limits.max_nesting_depth = 2;
        assert!(matches!(
            convert_docx(&deeply_nested, &depth_options, &context()),
            Err(ConversionError::ResourceLimit { limit: "max_nesting_depth", .. })
        ));
    }

    #[test]
    fn encrypted_ooxml_wrapper_has_stable_error() {
        let mut ole = vec![0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
        ole.extend_from_slice(b"EncryptionInfo\0EncryptedPackage");
        assert!(matches!(
            convert_docx(&ole, &ConversionOptions::default(), &context()),
            Err(ConversionError::Encrypted)
        ));

        let document = format!(r#"<w:document xmlns:w="{WORD}"><w:body/></w:document>"#);
        let mut encrypted_zip = base(document.as_bytes(), &[]);
        let local = encrypted_zip.windows(4).position(|value| value == b"PK\x03\x04").unwrap();
        encrypted_zip[local + 6] |= 1;
        let central = encrypted_zip.windows(4).position(|value| value == b"PK\x01\x02").unwrap();
        encrypted_zip[central + 8] |= 1;
        assert!(matches!(
            convert_docx(&encrypted_zip, &ConversionOptions::default(), &context()),
            Err(ConversionError::Encrypted)
        ));
    }
}
